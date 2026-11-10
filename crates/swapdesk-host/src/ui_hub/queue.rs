//! The one writer of a UI connection: three queues in front of a single writer thread.
//!
//! * control (replies, notifications): never dropped and never waits, so a runtime task can post one at any time.
//!   It is bounded all the same: a UI that has not read [`CONTROL_MESSAGES`] messages (or [`CONTROL_BYTES`] bytes) is
//!   not reading, so the connection is cut (the queue closes and the UI reconnects and asks again) instead of growing
//!   without end. A `Stats` notice of a session replaces the one still queued for it ("the newest wins"), so a busy
//!   session never fills the queue with numbers nobody will read;
//! * video: ordered and decodable, bytes bounded. A producer that must not hold anything else back uses
//!   [`UiQueue::try_video`] and, when there is no room, drops video up to the next keyframe and asks for one (the
//!   pump of a viewer session does); [`UiQueue::video`] waits for room instead;
//! * realtime media (audio datagrams, cursor positions): dropped when the queue is full and counted.
//!
//! * terminal bytes and ends: ordered like video (never dropped, the producer waits for room), a queue of their own so
//!   a stalled terminal never holds back the video;
//!
//! The writer sends control first, then realtime media (a few small messages at most, so audio is never behind a video
//! backlog), then terminal bytes, then video. A producer that waits for room is woken by the writer taking a message,
//! never by a timer.

use std::collections::VecDeque;
use std::io::Write;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use swapdesk_ipc::{FrameWriter, Message, Notify};
use tokio::sync::Notify as Wake;

use crate::outbox::weight;

/// Bytes of video waiting for the pipe before the next frame is refused (a few frames of 1080p: more is only delay).
const VIDEO_BYTES: usize = 8 * 1024 * 1024;
/// Bytes of terminal output waiting for the pipe before the producer waits.
const TERM_BYTES: usize = 1024 * 1024;
/// Realtime messages waiting before the next one is dropped.
const REALTIME_MESSAGES: usize = 128;
/// Control messages waiting before the connection is cut: replies and notices are small and few, so this is far
/// above any burst a UI that reads can leave behind.
const CONTROL_MESSAGES: usize = 1024;
/// Bytes of control messages waiting before the connection is cut.
const CONTROL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
struct Queues {
    control: VecDeque<(Message, usize)>,
    control_bytes: usize,
    video: VecDeque<(Message, usize)>,
    video_bytes: usize,
    term: VecDeque<(Message, usize)>,
    term_bytes: usize,
    realtime: VecDeque<Message>,
    closed: bool,
    /// The sender side is gone: the writer ends once everything queued is written.
    done: bool,
}

struct Shared {
    queues: Mutex<Queues>,
    /// The writer thread waits here for a message.
    filled: Condvar,
    /// Producers of video and terminal bytes wait here for room.
    room: Wake,
    dropped: AtomicU64,
    /// Times the control queue passed its bound and the connection was cut.
    cut: AtomicU64,
    /// What the owner of the pipe does when the connection is cut: cancel its I/O, so the blocked reader and writer
    /// return, the serve loop ends and the pipe closes (the UI sees the end of the pipe and reconnects).
    on_cut: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// What runs when a connection is cut.
pub(crate) type OnCut = Arc<dyn Fn() + Send + Sync>;

/// A cheap handle to the queues.
#[derive(Clone)]
pub(crate) struct UiQueue {
    inner: Arc<Shared>,
}

fn lock(m: &Mutex<Queues>) -> MutexGuard<'_, Queues> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The session whose latest `Stats` replaces an older one, if `msg` is one.
fn stats_of(msg: &Message) -> Option<u64> {
    match msg {
        Message::UiNotify(Notify::Stats { session_id, .. }) => Some(*session_id),
        _ => None,
    }
}

impl UiQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Shared {
                queues: Mutex::default(),
                filled: Condvar::new(),
                room: Wake::new(),
                dropped: AtomicU64::new(0),
                cut: AtomicU64::new(0),
                on_cut: Mutex::new(None),
            }),
        }
    }

    /// Queues a control message; `false` when the connection is closed (or this message passed the bound and cut it).
    pub fn control(&self, msg: Message) -> bool {
        let mut q = lock(&self.inner.queues);
        if q.closed {
            return false;
        }
        let len = weight(&msg);
        // The newest `Stats` of a session replaces the queued one, in its place.
        if let Some(session) = stats_of(&msg)
            && let Some(slot) = q.control.iter().rposition(|(m, _)| stats_of(m) == Some(session))
        {
            let old = std::mem::replace(&mut q.control[slot], (msg, len)).1;
            q.control_bytes = q.control_bytes - old + len;
            return true;
        }
        if q.control.len() >= CONTROL_MESSAGES || q.control_bytes + len > CONTROL_BYTES {
            self.inner.cut.fetch_add(1, Ordering::Relaxed);
            Self::close_locked(&mut q);
            drop(q);
            self.inner.filled.notify_all();
            self.inner.room.notify_waiters();
            // Outside every lock: the owner cancels the pipe's I/O, which wakes threads that take them.
            let on_cut = self.inner.on_cut.lock().unwrap_or_else(PoisonError::into_inner).clone();
            if let Some(on_cut) = on_cut {
                on_cut();
            }
            return false;
        }
        q.control_bytes += len;
        q.control.push_back((msg, len));
        self.inner.filled.notify_one();
        true
    }

    /// Queues video without waiting; `Some(msg)` hands it back when there is no room (or the queue is closed: check
    /// [`is_closed`](Self::is_closed)); `None` is queued.
    pub fn try_video(&self, msg: Message) -> Option<Message> {
        let mut q = lock(&self.inner.queues);
        let len = weight(&msg);
        // One frame always fits an empty queue, whatever its size.
        if q.closed || (q.video_bytes > 0 && q.video_bytes + len > VIDEO_BYTES) {
            return Some(msg);
        }
        q.video_bytes += len;
        q.video.push_back((msg, len));
        self.inner.filled.notify_one();
        None
    }

    /// Queues video, waiting (asynchronously) for room. `false` when the connection closed first.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn video(&self, msg: Message) -> bool {
        self.wait_for_room(msg, Self::try_video).await
    }

    /// Queues terminal output (or a terminal's end) without waiting; `Some(msg)` hands it back when there is no room.
    pub fn try_term(&self, msg: Message) -> Option<Message> {
        let mut q = lock(&self.inner.queues);
        let len = weight(&msg);
        if q.closed || (q.term_bytes > 0 && q.term_bytes + len > TERM_BYTES) {
            return Some(msg);
        }
        q.term_bytes += len;
        q.term.push_back((msg, len));
        self.inner.filled.notify_one();
        None
    }

    /// Queues terminal output, waiting (asynchronously) for room: a slow UI slows the host's shell, it loses nothing.
    /// `false` when the connection closed first.
    pub async fn term(&self, msg: Message) -> bool {
        self.wait_for_room(msg, Self::try_term).await
    }

    /// Tries `attempt` until it takes the message, sleeping on the room signal in between (a message the writer takes
    /// or the queue closing), never on a timer. The signal is registered before each look, so none is lost.
    async fn wait_for_room(&self, msg: Message, attempt: fn(&Self, Message) -> Option<Message>) -> bool {
        let mut msg = msg;
        loop {
            let mut room = pin!(self.inner.room.notified());
            room.as_mut().enable();
            match attempt(self, msg) {
                None => return true,
                Some(back) => {
                    if self.is_closed() {
                        return false;
                    }
                    msg = back;
                }
            }
            room.await;
        }
    }

    /// Queues a realtime message; dropped (and counted) when the queue is full.
    pub fn realtime(&self, msg: Message) -> bool {
        let mut q = lock(&self.inner.queues);
        if q.closed {
            return false;
        }
        if q.realtime.len() >= REALTIME_MESSAGES {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        q.realtime.push_back(msg);
        self.inner.filled.notify_one();
        true
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.inner.queues).closed
    }

    /// Realtime messages dropped for lack of room.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Sets what runs when the control queue passes its bound and the connection is cut.
    pub fn set_on_cut(&self, on_cut: OnCut) {
        *self.inner.on_cut.lock().unwrap_or_else(PoisonError::into_inner) = Some(on_cut);
    }

    /// Times the control queue passed its bound and the connection was cut.
    pub fn cut(&self) -> u64 {
        self.inner.cut.load(Ordering::Relaxed)
    }

    fn close_locked(q: &mut Queues) {
        q.closed = true;
        q.control.clear();
        q.control_bytes = 0;
        q.video.clear();
        q.video_bytes = 0;
        q.term.clear();
        q.term_bytes = 0;
        q.realtime.clear();
    }

    /// The reading side ended or the pipe broke: later sends return `false` and what is queued is discarded.
    pub fn close(&self) {
        Self::close_locked(&mut lock(&self.inner.queues));
        self.inner.filled.notify_all();
        self.inner.room.notify_waiters();
    }

    /// Ends the writer after it has written what is queued (a `Bye` for instance).
    pub fn finish(&self) {
        lock(&self.inner.queues).done = true;
        self.inner.filled.notify_all();
    }

    fn next(&self) -> Option<Message> {
        let mut q = lock(&self.inner.queues);
        loop {
            if q.closed {
                return None;
            }
            if let Some((msg, len)) = q.control.pop_front() {
                q.control_bytes -= len;
                return Some(msg);
            }
            if let Some(msg) = q.realtime.pop_front() {
                return Some(msg);
            }
            if let Some((msg, len)) = q.term.pop_front() {
                q.term_bytes -= len;
                drop(q);
                self.inner.room.notify_waiters();
                return Some(msg);
            }
            if let Some((msg, len)) = q.video.pop_front() {
                q.video_bytes -= len;
                drop(q);
                self.inner.room.notify_waiters();
                return Some(msg);
            }
            if q.done {
                return None;
            }
            q = self.inner.filled.wait(q).unwrap_or_else(PoisonError::into_inner);
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
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
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
        Message::ViewerVideo { session_id: 1, header, payload: vec![0; len].into() }
    }

    fn stats(session_id: u64, fps: u16) -> Message {
        Message::UiNotify(Notify::Stats { session_id, latency_ms: 1, fps, kbps: 1 })
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
        for _ in 0..REALTIME_MESSAGES {
            assert!(matches!(q.next(), Some(Message::ViewerAudio { .. })), "then realtime, ahead of the video backlog");
        }
        assert!(matches!(q.next(), Some(Message::ViewerVideo { .. })), "then video");
        assert!(q.try_video(video(2000)).is_none(), "room again");
        q.close();
        assert!(!q.control(Message::Shutdown));
        assert!(q.next().is_none());
    }

    #[test]
    fn the_control_queue_has_a_bound_and_a_ui_that_does_not_read_is_cut_not_buffered() {
        let q = UiQueue::new();
        for _ in 0..CONTROL_MESSAGES {
            assert!(q.control(Message::SasRequest));
        }
        assert!(!q.is_closed());
        assert!(!q.control(Message::SasRequest), "one past the bound");
        assert!(q.is_closed(), "the connection is cut");
        assert_eq!(q.cut(), 1);
        assert!(q.next().is_none(), "and what was queued is dropped with it");
        assert!(!q.control(Message::SasRequest));
        // The bytes are bounded too: replies of a few MiB each pass 16 MiB long before the message count.
        let q = UiQueue::new();
        let body = Message::UiReply { req: 1, status: 0, body: Bytes(vec![0; 5 * 1024 * 1024]) };
        let (mut taken, mut cut) = (0, false);
        for _ in 0..6 {
            if q.control(body.clone()) {
                taken += 1;
            } else {
                cut = true;
                break;
            }
        }
        assert!(cut && taken == 3, "{taken} replies of 5 MiB were queued before the cut");
    }

    #[test]
    fn a_cut_runs_the_owners_hook_once_so_the_pipe_can_be_closed() {
        let q = UiQueue::new();
        let ran = Arc::new(AtomicU64::new(0));
        {
            let ran = Arc::clone(&ran);
            q.set_on_cut(Arc::new(move || {
                ran.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for _ in 0..CONTROL_MESSAGES {
            assert!(q.control(Message::SasRequest));
        }
        assert_eq!(ran.load(Ordering::SeqCst), 0, "no cut yet");
        assert!(!q.control(Message::SasRequest));
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the hook ran when the connection was cut");
        // Later sends on the closed queue do not run it again.
        assert!(!q.control(Message::SasRequest));
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_newest_stats_of_a_session_replaces_the_queued_one_so_a_busy_session_never_grows_the_queue() {
        let q = UiQueue::new();
        assert!(q.control(Message::SasRequest));
        for fps in 0..5000u16 {
            assert!(q.control(stats(7, fps)), "a stats notice is never what cuts the connection");
        }
        assert!(q.control(stats(8, 1)), "another session has its own");
        assert!(q.control(Message::Shutdown));
        assert_eq!(lock(&q.inner.queues).control.len(), 4, "SasRequest, stats 7, stats 8, Shutdown");
        assert!(matches!(q.next(), Some(Message::SasRequest)));
        assert!(matches!(q.next(), Some(Message::UiNotify(Notify::Stats { session_id: 7, fps: 4999, .. }))));
        assert!(matches!(q.next(), Some(Message::UiNotify(Notify::Stats { session_id: 8, .. }))));
        assert!(matches!(q.next(), Some(Message::Shutdown)));
        assert_eq!(lock(&q.inner.queues).control_bytes, 0, "the byte count is exact");
    }

    #[test]
    fn a_video_producer_wakes_when_the_writer_takes_a_frame_not_on_a_timer() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let q = UiQueue::new();
        assert!(q.try_video(video(VIDEO_BYTES - 1000)).is_none());
        let producer = q.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (took_tx, took_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            rt.block_on(async move {
                started_tx.send(()).unwrap();
                let ok = producer.video(video(2000)).await;
                took_tx.send(Instant::now()).unwrap();
                ok
            })
        });
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(took_rx.try_recv().is_err(), "no room: the producer waits");
        let freed = Instant::now();
        assert!(matches!(q.next(), Some(Message::ViewerVideo { .. })));
        let woke = took_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(woke.duration_since(freed) < Duration::from_millis(100), "woke {:?} after the room", woke - freed);
        assert!(task.join().unwrap());
    }

    #[test]
    fn closing_wakes_a_waiting_terminal_producer_with_false() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let q = UiQueue::new();
        let big = Message::TermOutput { term_id: 1, bytes: Bytes(vec![0; TERM_BYTES]) };
        assert!(q.try_term(big.clone()).is_none());
        let producer = q.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            rt.block_on(async move {
                started_tx.send(()).unwrap();
                producer.term(big).await
            })
        });
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        q.close();
        assert!(!task.join().unwrap());
    }
}
