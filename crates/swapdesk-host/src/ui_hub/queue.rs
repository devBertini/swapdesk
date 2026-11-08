//! The one writer of a UI connection: three queues in front of a single writer thread.
//!
//! * control (replies, notifications): never dropped and never waits, so a runtime task can post one at any time;
//!   they are small and bounded by what the UI asks and what the service reports;
//! * video: ordered and decodable, so it is never dropped either, but the producer waits for room (bytes bounded:
//!   the receiver upstream bounds what it holds, so a slow UI slows the host's stream instead of growing memory);
//! * realtime media (audio datagrams, cursor positions): dropped when the queue is full and counted.
//!
//! * terminal bytes and ends: ordered like video (never dropped, the producer waits for room), a queue of their own so
//!   a stalled terminal never holds back the video;
//!
//! The writer sends control first, then terminal bytes, then video, then realtime media.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_ipc::{FrameWriter, Message};

/// Bytes of video waiting for the pipe before the producer waits.
const VIDEO_BYTES: usize = 24 * 1024 * 1024;
/// Bytes of terminal output waiting for the pipe before the producer waits.
const TERM_BYTES: usize = 1024 * 1024;
/// Realtime messages waiting before the next one is dropped.
const REALTIME_MESSAGES: usize = 128;
/// How long an async producer sleeps between two looks at a full video queue.
const FULL_RETRY: Duration = Duration::from_millis(2);

#[derive(Default)]
struct Queues {
    control: VecDeque<Message>,
    video: VecDeque<(Message, usize)>,
    video_bytes: usize,
    term: VecDeque<(Message, usize)>,
    term_bytes: usize,
    realtime: VecDeque<Message>,
    closed: bool,
    /// The sender side is gone: the writer ends once everything queued is written.
    done: bool,
}

/// A cheap handle to the queues.
#[derive(Clone)]
pub(crate) struct UiQueue {
    inner: Arc<(Mutex<Queues>, Condvar)>,
    dropped: Arc<AtomicU64>,
}

fn lock(m: &Mutex<Queues>) -> MutexGuard<'_, Queues> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a media message is, for routing.
fn payload_len(msg: &Message) -> usize {
    match msg {
        Message::ViewerVideo { payload, .. } => payload.0.len() + 64,
        Message::TermOutput { bytes, .. } => bytes.0.len() + 64,
        _ => 64,
    }
}

impl UiQueue {
    pub fn new() -> Self {
        Self { inner: Arc::default(), dropped: Arc::default() }
    }

    /// Queues a control message; `false` when the connection is closed.
    pub fn control(&self, msg: Message) -> bool {
        let (m, cv) = &*self.inner;
        let mut q = lock(m);
        if q.closed {
            return false;
        }
        q.control.push_back(msg);
        cv.notify_one();
        true
    }

    /// Queues video without waiting; `Some(msg)` hands it back when there is no room (or the queue is closed: check
    /// [`is_closed`](Self::is_closed)); `None` is queued.
    pub fn try_video(&self, msg: Message) -> Option<Message> {
        let (m, cv) = &*self.inner;
        let mut q = lock(m);
        let len = payload_len(&msg);
        // One frame always fits an empty queue, whatever its size.
        if q.closed || (q.video_bytes > 0 && q.video_bytes + len > VIDEO_BYTES) {
            return Some(msg);
        }
        q.video_bytes += len;
        q.video.push_back((msg, len));
        cv.notify_one();
        None
    }

    /// Queues video, waiting (asynchronously) for room. `false` when the connection closed first.
    pub async fn video(&self, msg: Message) -> bool {
        let mut msg = msg;
        loop {
            match self.try_video(msg) {
                None => return true,
                Some(back) => {
                    if self.is_closed() {
                        return false;
                    }
                    msg = back;
                    tokio::time::sleep(FULL_RETRY).await;
                }
            }
        }
    }

    /// Queues terminal output (or a terminal's end) without waiting; `Some(msg)` hands it back when there is no room.
    pub fn try_term(&self, msg: Message) -> Option<Message> {
        let (m, cv) = &*self.inner;
        let mut q = lock(m);
        let len = payload_len(&msg);
        if q.closed || (q.term_bytes > 0 && q.term_bytes + len > TERM_BYTES) {
            return Some(msg);
        }
        q.term_bytes += len;
        q.term.push_back((msg, len));
        cv.notify_one();
        None
    }

    /// Queues terminal output, waiting (asynchronously) for room: a slow UI slows the host's shell, it loses nothing.
    /// `false` when the connection closed first.
    pub async fn term(&self, msg: Message) -> bool {
        let mut msg = msg;
        loop {
            match self.try_term(msg) {
                None => return true,
                Some(back) => {
                    if self.is_closed() {
                        return false;
                    }
                    msg = back;
                    tokio::time::sleep(FULL_RETRY).await;
                }
            }
        }
    }

    /// Queues a realtime message; dropped (and counted) when the queue is full.
    pub fn realtime(&self, msg: Message) -> bool {
        let (m, cv) = &*self.inner;
        let mut q = lock(m);
        if q.closed {
            return false;
        }
        if q.realtime.len() >= REALTIME_MESSAGES {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        q.realtime.push_back(msg);
        cv.notify_one();
        true
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.inner.0).closed
    }

    /// Realtime messages dropped for lack of room.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The reading side ended or the pipe broke: later sends return `false` and what is queued is discarded.
    pub fn close(&self) {
        let (m, cv) = &*self.inner;
        let mut q = lock(m);
        q.closed = true;
        q.control.clear();
        q.video.clear();
        q.video_bytes = 0;
        q.term.clear();
        q.term_bytes = 0;
        q.realtime.clear();
        cv.notify_all();
    }

    /// Ends the writer after it has written what is queued (a `Bye` for instance).
    pub fn finish(&self) {
        let (m, cv) = &*self.inner;
        lock(m).done = true;
        cv.notify_all();
    }

    fn next(&self) -> Option<Message> {
        let (m, cv) = &*self.inner;
        let mut q = lock(m);
        loop {
            if q.closed {
                return None;
            }
            if let Some(msg) = q.control.pop_front() {
                return Some(msg);
            }
            if let Some((msg, len)) = q.term.pop_front() {
                q.term_bytes -= len;
                return Some(msg);
            }
            if let Some((msg, len)) = q.video.pop_front() {
                q.video_bytes -= len;
                return Some(msg);
            }
            if let Some(msg) = q.realtime.pop_front() {
                return Some(msg);
            }
            if q.done {
                return None;
            }
            q = cv.wait(q).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Starts the writer thread. It ends on [`finish`](Self::finish) (after the queues drain), on
    /// [`close`](Self::close), or at the first I/O error (which also closes the queue).
    pub fn spawn_writer<W: Write + Send + 'static>(
        &self,
        mut writer: FrameWriter<W>,
    ) -> std::io::Result<JoinHandle<()>> {
        let queue = self.clone();
        std::thread::Builder::new().name("ui-hub-writer".into()).spawn(move || {
            while let Some(msg) = queue.next() {
                match writer.write_message(&msg) {
                    Ok(()) => {}
                    Err(swapdesk_ipc::IpcError::Io(_)) => {
                        queue.close();
                        return;
                    }
                    // A message over its limit: skipped, the connection stays.
                    Err(_) => {}
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_ipc::Bytes;

    fn video(len: usize) -> Message {
        let header = swapdesk_proto::VideoHeader {
            keyframe: false,
            config: false,
            discontinuity: false,
            display_id: 0,
            frame_id: 0,
            capture_us: 0,
            encode_us: 0,
            width: 1,
            height: 1,
            codec: swapdesk_proto::Codec::H264,
            payload_len: len as u32,
        };
        Message::ViewerVideo { session_id: 1, header, payload: Bytes(vec![0; len]) }
    }

    #[test]
    fn control_goes_first_video_waits_for_room_and_realtime_is_dropped_when_full() {
        let q = UiQueue::new();
        assert!(q.try_video(video(VIDEO_BYTES - 1000)).is_none());
        assert!(q.try_video(video(2000)).is_some(), "no room: the producer waits");
        assert!(q.control(Message::Bye { code: swapdesk_proto::ErrorCode::NoError }));
        for _ in 0..REALTIME_MESSAGES {
            assert!(q.realtime(Message::ViewerAudio { session_id: 1, datagram: Bytes(vec![1]) }));
        }
        assert!(!q.realtime(Message::ViewerAudio { session_id: 1, datagram: Bytes(vec![1]) }));
        assert_eq!(q.dropped(), 1);
        assert!(matches!(q.next(), Some(Message::Bye { .. })), "control first");
        assert!(matches!(q.next(), Some(Message::ViewerVideo { .. })), "then video");
        assert!(q.try_video(video(2000)).is_none(), "room again");
        q.close();
        assert!(!q.control(Message::Shutdown));
        assert!(q.next().is_none());
    }
}
