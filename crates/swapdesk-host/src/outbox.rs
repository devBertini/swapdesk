//! The one writer of a helper's pipe: a bounded queue in front of a single writer thread.
//!
//! Policy: media (`VideoFrame`, `AudioPacket`, `CursorPos`, and the cursor shape the helper sends as media) never
//! waits for a slow pipe: when the queue is full the message is dropped and counted. Every other message is control
//! and is never dropped: its sender waits for room. The queue is bounded twice, by messages and by bytes (a video
//! message can be 8 MiB, so a count alone is no bound), and an empty queue always takes one message of any size.
//!
//! Waiting is event driven. A thread waits on a condition variable; an async task waits on a [`Notify`] that the
//! writer fires every time it takes a message, so no task sleeps and looks again. While a control sender waits, media
//! is dropped (it is the media that fills the queue), so the waiter cannot starve behind a stream.

use std::collections::VecDeque;
use std::io::Write;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_ipc::{ClipData, FrameWriter, FsAnswer, FsCall, IpcError, Message};
use tokio::sync::Notify;

/// What a queue holds before a media message is dropped and a control sender waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Messages queued (at least 1).
    pub messages: usize,
    /// Bytes queued, counted by [`weight`] (at least 1).
    pub bytes: usize,
}

impl Limits {
    pub const fn new(messages: usize, bytes: usize) -> Self {
        Self { messages: if messages == 0 { 1 } else { messages }, bytes: if bytes == 0 { 1 } else { bytes } }
    }

    /// The same message count with another byte budget.
    #[must_use]
    pub const fn with_bytes(self, bytes: usize) -> Self {
        Self::new(self.messages, bytes)
    }
}

/// Default byte budget of a queue: four large keyframes, or about a second of a busy 1080p stream.
pub const DEFAULT_BYTES: usize = 4 * 1024 * 1024;

/// The bytes a message holds in a queue: its payload plus a fixed cost per message, so a flood of tiny messages
/// counts too.
pub fn weight(msg: &Message) -> usize {
    const BASE: usize = 64;
    BASE + match msg {
        Message::VideoFrame { payload, .. } | Message::ViewerVideo { payload, .. } => payload.0.len(),
        Message::AudioPacket(b) => b.0.len(),
        Message::ViewerAudio { datagram, .. } => datagram.0.len(),
        Message::CursorShape(s) | Message::ViewerCursorShape { shape: s, .. } => s.0.rgba.len(),
        Message::PtyInput { bytes, .. }
        | Message::PtyOutput { bytes, .. }
        | Message::TermOutput { bytes, .. }
        | Message::TermInput { bytes, .. } => bytes.0.len(),
        Message::UiReply { body, .. } => body.0.len(),
        Message::FsRequest { call: FsCall::PartAppend { data, .. }, .. } => data.0.len(),
        Message::FsReply { result: Ok(FsAnswer::Data(d)), .. } => d.0.len(),
        Message::ClipWrite { content: ClipData::Text(t), .. } => t.len(),
        Message::ClipWrite { content: ClipData::Png(p), .. } => p.len(),
        Message::Input { events } | Message::ViewerInput { events, .. } => events.0.len() * 24,
        _ => 0,
    }
}

#[derive(Default)]
struct State {
    queue: VecDeque<(Message, usize)>,
    bytes: usize,
    /// Live [`Outbox`] handles: the reader ends once the queue is empty and none is left.
    senders: usize,
}

struct Inner {
    state: Mutex<State>,
    /// The writer waits here for a message.
    filled: Condvar,
    /// Threads waiting for room wait here.
    room: Condvar,
    /// Tasks waiting for room wait here.
    room_async: Notify,
    /// Control senders waiting for room (thread or task).
    waiting: AtomicUsize,
    limits: Limits,
    dropped: AtomicU64,
    skipped: AtomicU64,
    /// The pipe is over: nothing more is queued and the writer discards what it takes.
    closed: AtomicBool,
    /// [`Outbox::finish`] was called: nothing more is queued, what is queued still goes out, and the reader ends once
    /// the queue is empty. Set and read with the state locked, so no message is queued behind the reader's end.
    finished: AtomicBool,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake_room(&self) {
        self.room.notify_all();
        self.room_async.notify_waiters();
    }
}

/// A cheap handle to the queue; every thread of the helper holds a clone.
pub struct Outbox {
    inner: Arc<Inner>,
}

/// The receiving end, for the writer thread.
pub struct OutboxReader {
    inner: Arc<Inner>,
}

/// What the writer thread reports when it ends.
#[derive(Debug)]
pub struct WriterReport {
    /// The first I/O error that closed the pipe, if any.
    pub io_error: Option<IpcError>,
    /// Messages that could not be encoded (over their limit) and were skipped; the pipe stayed open.
    pub skipped: u64,
}

/// The result of [`Outbox::try_send_control`].
#[derive(Debug)]
pub enum TryControl {
    Sent,
    /// The queue is full: the message comes back.
    Full(Message),
    /// The pipe is closed: nothing can be sent any more.
    Closed,
}

/// Media messages are droppable; everything else is control.
pub fn is_media(msg: &Message) -> bool {
    matches!(msg, Message::VideoFrame { .. } | Message::AudioPacket(_) | Message::CursorPos(_))
}

impl Clone for Outbox {
    fn clone(&self) -> Self {
        self.inner.state().senders += 1;
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        let last = {
            let mut st = self.inner.state();
            st.senders -= 1;
            st.senders == 0
        };
        if last {
            self.inner.filled.notify_all();
        }
    }
}

/// A control sender waiting for room, counted while it waits (also when its task is dropped).
struct Waiting<'a>(&'a Inner);

impl<'a> Waiting<'a> {
    fn new(inner: &'a Inner) -> Self {
        inner.waiting.fetch_add(1, Ordering::SeqCst);
        Self(inner)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Outbox {
    /// A queue of `capacity` messages (at least 1) with the default byte budget, and the receiving end.
    pub fn new(capacity: usize) -> (Self, OutboxReader) {
        Self::with_limits(Limits::new(capacity, DEFAULT_BYTES))
    }

    pub fn with_limits(limits: Limits) -> (Self, OutboxReader) {
        let inner = Arc::new(Inner {
            state: Mutex::new(State { senders: 1, ..State::default() }),
            filled: Condvar::new(),
            room: Condvar::new(),
            room_async: Notify::new(),
            waiting: AtomicUsize::new(0),
            limits,
            dropped: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        (Self { inner: Arc::clone(&inner) }, OutboxReader { inner })
    }

    /// The limits this queue was made with.
    pub fn limits(&self) -> Limits {
        self.inner.limits
    }

    /// Whether `st` has room for `w` more bytes: an empty queue always has.
    fn fits(&self, st: &State, w: usize) -> bool {
        st.queue.is_empty() || (st.queue.len() < self.inner.limits.messages && st.bytes + w <= self.inner.limits.bytes)
    }

    fn push(&self, st: &mut State, msg: Message, w: usize) {
        st.bytes += w;
        st.queue.push_back((msg, w));
        self.inner.filled.notify_one();
    }

    /// Queues a media message without waiting and without counting a refusal as a drop: `Full` hands the message back
    /// (no room, or a control sender waits) for a caller that keeps it to try again, as the cursor shape does.
    pub fn try_send_media(&self, msg: Message) -> TryControl {
        let w = weight(&msg);
        let mut st = self.inner.state();
        if self.is_closed() {
            return TryControl::Closed;
        }
        if self.inner.waiting.load(Ordering::SeqCst) > 0 || !self.fits(&st, w) {
            return TryControl::Full(msg);
        }
        self.push(&mut st, msg, w);
        TryControl::Sent
    }

    /// Queues a media message without waiting. `false` when it was dropped (queue full or a control sender waiting,
    /// counted) or the pipe is closed.
    pub fn send_media(&self, msg: Message) -> bool {
        let w = weight(&msg);
        let mut st = self.inner.state();
        if self.is_closed() {
            return false;
        }
        if self.inner.waiting.load(Ordering::SeqCst) > 0 || !self.fits(&st, w) {
            drop(st);
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.push(&mut st, msg, w);
        true
    }

    /// Queues a control message, waiting for room (this thread blocks). `false` only when the pipe is closed.
    pub fn send_control(&self, msg: Message) -> bool {
        let w = weight(&msg);
        let mut waiting = None;
        let mut st = self.inner.state();
        loop {
            if self.is_closed() {
                return false;
            }
            if self.fits(&st, w) {
                self.push(&mut st, msg, w);
                return true;
            }
            if waiting.is_none() {
                waiting = Some(Waiting::new(&self.inner));
            }
            st = self.inner.room.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Queues a control message without waiting: `Full` hands the message back.
    pub fn try_send_control(&self, msg: Message) -> TryControl {
        let w = weight(&msg);
        let mut st = self.inner.state();
        if self.is_closed() {
            return TryControl::Closed;
        }
        if self.fits(&st, w) {
            self.push(&mut st, msg, w);
            TryControl::Sent
        } else {
            TryControl::Full(msg)
        }
    }

    /// Queues a control message, waiting for room without blocking the thread: the task sleeps until the writer takes a
    /// message, not on a timer. `false` only when the pipe is closed.
    pub async fn send_control_async(&self, msg: Message) -> bool {
        let mut msg = msg;
        let mut waiting: Option<Waiting<'_>> = None;
        loop {
            // Registered before the queue is looked at: a message taken in between wakes this task, never lost.
            let mut notified = pin!(self.inner.room_async.notified());
            notified.as_mut().enable();
            match self.try_send_control(msg) {
                TryControl::Sent => return true,
                TryControl::Closed => return false,
                TryControl::Full(back) => msg = back,
            }
            if waiting.is_none() {
                waiting = Some(Waiting::new(&self.inner));
            }
            notified.await;
        }
    }

    /// Routes by [`is_media`].
    pub fn send(&self, msg: Message) -> bool {
        if is_media(&msg) { self.send_media(msg) } else { self.send_control(msg) }
    }

    /// Marks the pipe closed (its reading side ended): later sends return `false`, the queue is discarded and every
    /// sender that waits for room is released.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        {
            let mut st = self.inner.state();
            st.queue.clear();
            st.bytes = 0;
        }
        self.inner.wake_room();
    }

    /// Ends the queue while the pipe still works (the side that consumed what the pipe's reader read is gone, or the
    /// owner stops): later sends return `false` and every sender that waits for room is released, as after
    /// [`close`](Self::close), but what is queued still goes out. The reader then ends, whatever handles are left, so
    /// the writer thread returns and lets go of the pipe's write half: the other end reads the end of its pipe.
    pub fn finish(&self) {
        {
            let _st = self.inner.state();
            self.inner.finished.store(true, Ordering::Release);
        }
        self.inner.filled.notify_all();
        self.inner.wake_room();
    }

    /// Media messages dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Messages and bytes queued now.
    pub fn queued(&self) -> (usize, usize) {
        let st = self.inner.state();
        (st.queue.len(), st.bytes)
    }

    /// True once the pipe is closed (the reading side ended or the writer met an I/O error) or the queue was finished:
    /// nothing more can be sent.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire) || self.inner.finished.load(Ordering::Acquire)
    }
}

impl OutboxReader {
    fn take(&self, st: &mut State) -> Option<Message> {
        let (msg, w) = st.queue.pop_front()?;
        st.bytes -= w;
        Some(msg)
    }

    /// Nothing more will be queued: every [`Outbox`] is gone, or the queue was finished.
    fn over(&self, st: &State) -> bool {
        st.senders == 0 || self.inner.finished.load(Ordering::Acquire)
    }

    /// The next message, waiting for one; `None` once the queue is empty and every [`Outbox`] is gone or the queue was
    /// finished.
    pub fn recv(&self) -> Option<Message> {
        let mut st = self.inner.state();
        loop {
            if let Some(m) = self.take(&mut st) {
                drop(st);
                self.inner.wake_room();
                return Some(m);
            }
            if self.over(&st) {
                return None;
            }
            st = self.inner.filled.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The next message if one is queued.
    pub fn try_recv(&self) -> Option<Message> {
        let m = self.take(&mut self.inner.state());
        if m.is_some() {
            self.inner.wake_room();
        }
        m
    }

    /// The next message, waiting at most `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Message> {
        let end = Instant::now() + timeout;
        let mut st = self.inner.state();
        loop {
            if let Some(m) = self.take(&mut st) {
                drop(st);
                self.inner.wake_room();
                return Some(m);
            }
            let left = end.checked_duration_since(Instant::now())?;
            if self.over(&st) {
                return None;
            }
            st = self.inner.filled.wait_timeout(st, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// Everything queued right now, in order.
    pub fn try_iter(&self) -> impl Iterator<Item = Message> + '_ {
        std::iter::from_fn(|| self.try_recv())
    }
}

/// Runs the writer thread: it writes every queued message in order and ends when every [`Outbox`] clone is gone or
/// the queue was finished ([`Outbox::finish`]), dropping the pipe's write half. After an I/O error it keeps draining
/// (so no sender blocks) and discards.
pub fn spawn_writer<W: Write + Send + 'static>(
    outbox: &Outbox,
    reader: OutboxReader,
    mut writer: FrameWriter<W>,
) -> std::io::Result<JoinHandle<WriterReport>> {
    let inner = Arc::clone(&outbox.inner);
    std::thread::Builder::new().name("helper-writer".into()).spawn(move || {
        let mut io_error = None;
        while let Some(msg) = reader.recv() {
            if inner.closed.load(Ordering::Acquire) {
                continue;
            }
            match writer.write_message(&msg) {
                Ok(()) => {}
                Err(IpcError::Io(e)) => {
                    inner.closed.store(true, Ordering::Release);
                    io_error = Some(IpcError::Io(e));
                    // The senders that wait for room must see the pipe closed, not wait for a writer that is gone.
                    let mut st = inner.state();
                    st.queue.clear();
                    st.bytes = 0;
                    drop(st);
                    inner.wake_room();
                }
                Err(_) => {
                    inner.skipped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        WriterReport { io_error, skipped: inner.skipped.load(Ordering::Relaxed) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use swapdesk_ipc::Bytes;
    use swapdesk_proto::VideoHeader;

    fn video_of(n: u32, len: usize) -> Message {
        let header = VideoHeader {
            keyframe: false,
            config: false,
            discontinuity: false,
            display_id: 0,
            frame_id: n,
            capture_us: 0,
            encode_us: 0,
            width: 1,
            height: 1,
            codec: swapdesk_proto::Codec::H264,
            payload_len: len as u32,
        };
        Message::VideoFrame { header, payload: vec![0; len].into() }
    }

    fn video(n: u32) -> Message {
        video_of(n, 1)
    }

    #[test]
    fn a_full_queue_drops_video_but_never_control() {
        let (outbox, rx) = Outbox::new(2);
        assert!(outbox.send_media(video(0)));
        assert!(outbox.send_media(video(1)));
        assert!(!outbox.send_media(video(2)));
        assert!(!outbox.send(video(3)));
        assert!(!outbox.send_media(Message::AudioPacket(Bytes(vec![1]))));
        assert_eq!(outbox.dropped(), 3);

        // A control message waits for room instead of being dropped.
        let (done_tx, done_rx) = mpsc::channel();
        let sender = outbox.clone();
        let t = std::thread::spawn(move || {
            let ok = sender.send_control(Message::StopAudio);
            done_tx.send(ok).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(150)).is_err());
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { .. }));
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        t.join().unwrap();

        // Queue now: video(1), StopAudio. Control is all there, only the dropped video is missing.
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { header, .. } if header.frame_id == 1));
        assert_eq!(rx.recv().unwrap(), Message::StopAudio);
        assert!(rx.try_recv().is_none());
        assert_eq!(outbox.dropped(), 3);
    }

    #[test]
    fn control_is_not_media() {
        assert!(!is_media(&Message::Shutdown));
        assert!(!is_media(&Message::SasRequest));
        assert!(is_media(&video(0)));
    }

    #[test]
    fn the_byte_budget_holds_whatever_the_message_count_and_an_empty_queue_takes_one_big_message() {
        // 100 messages allowed, 1 MiB of bytes: frames of 300 KiB fit three at a time.
        let (outbox, rx) = Outbox::with_limits(Limits::new(100, 1024 * 1024));
        let frame = 300 * 1024;
        assert!(outbox.send_media(video_of(0, frame)));
        assert!(outbox.send_media(video_of(1, frame)));
        assert!(outbox.send_media(video_of(2, frame)));
        assert!(!outbox.send_media(video_of(3, frame)), "a fourth would pass 1 MiB");
        let (messages, bytes) = outbox.queued();
        assert_eq!(messages, 3);
        assert!(bytes <= 1024 * 1024, "{bytes} bytes queued");
        // Small messages still fit in what is left.
        assert!(outbox.send_media(Message::CursorPos(swapdesk_proto::CursorPosition {
            visible: true,
            display: 0,
            seq: 0,
            x: 0,
            y: 0,
            shape_id: 0,
        })));
        // A message bigger than the whole budget is taken by an empty queue (it could never go otherwise).
        while rx.try_recv().is_some() {}
        assert!(outbox.send_media(video_of(9, 3 * 1024 * 1024)));
        assert!(!outbox.send_media(video_of(10, 1)), "and nothing else beside it");
        assert_eq!(outbox.queued().0, 1);
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { header, .. } if header.frame_id == 9));
        assert_eq!(outbox.queued(), (0, 0), "the bytes are given back");
    }

    #[test]
    fn a_task_waiting_for_room_wakes_when_the_writer_takes_a_message_not_on_a_timer() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (outbox, rx) = Outbox::new(1);
        assert!(outbox.send_media(video(0)));
        let sender = outbox.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (took_tx, took_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            rt.block_on(async move {
                started_tx.send(()).unwrap();
                let ok = sender.send_control_async(Message::StopAudio).await;
                took_tx.send(Instant::now()).unwrap();
                ok
            })
        });
        started_rx.recv().unwrap();
        // Long enough for a polling sender to have come round many times, short of any sleep the code could hide.
        std::thread::sleep(Duration::from_millis(300));
        assert!(took_rx.try_recv().is_err(), "the queue is full: the task waits");
        let freed = Instant::now();
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { .. }));
        let woke = took_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(woke.duration_since(freed) < Duration::from_millis(100), "woke {:?} after the room", woke - freed);
        assert!(task.join().unwrap());
        assert_eq!(rx.recv().unwrap(), Message::StopAudio);
    }

    #[test]
    fn media_is_dropped_while_control_waits_so_the_waiter_is_not_starved() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (outbox, rx) = Outbox::new(2);
        assert!(outbox.send_media(video(0)));
        assert!(outbox.send_media(video(1)));
        let sender = outbox.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let task = std::thread::spawn(move || {
            rt.block_on(async move {
                started_tx.send(()).unwrap();
                sender.send_control_async(Message::StopAudio).await
            })
        });
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Room appears, and a media message arrives at once: the waiter's turn comes first.
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { .. }));
        assert!(!outbox.send_media(video(2)), "a control sender is waiting: the media gives way");
        assert!(task.join().unwrap());
        assert!(matches!(rx.recv().unwrap(), Message::VideoFrame { .. }));
        assert_eq!(rx.recv().unwrap(), Message::StopAudio);
    }

    #[test]
    fn closing_releases_the_senders_that_wait_and_the_reader_ends_with_the_last_handle() {
        let (outbox, rx) = Outbox::new(1);
        assert!(outbox.send_media(video(0)));
        let blocked = outbox.clone();
        let t = std::thread::spawn(move || blocked.send_control(Message::StopAudio));
        std::thread::sleep(Duration::from_millis(100));
        outbox.close();
        assert!(!t.join().unwrap(), "released with the pipe closed");
        assert!(!outbox.send_control(Message::StopAudio));
        assert_eq!(outbox.queued(), (0, 0));
        drop(outbox);
        assert!(rx.recv().is_none(), "no handle left and nothing queued");
    }

    #[test]
    fn finishing_sends_what_is_queued_takes_nothing_more_and_ends_the_reader_with_handles_left() {
        let (outbox, rx) = Outbox::new(1);
        assert!(outbox.send_control(Message::Shutdown));
        let blocked = outbox.clone();
        let t = std::thread::spawn(move || blocked.send_control(Message::StopAudio));
        std::thread::sleep(Duration::from_millis(100));
        outbox.finish();
        assert!(!t.join().unwrap(), "a sender that waited for room is released");
        assert!(outbox.is_closed());
        assert!(!outbox.send_control(Message::StopAudio));
        assert!(!outbox.send_media(video(0)));
        assert!(matches!(outbox.try_send_control(Message::StopAudio), TryControl::Closed));
        assert!(matches!(outbox.try_send_media(video(1)), TryControl::Closed));
        assert_eq!(outbox.dropped(), 0, "a refusal after the end is not a dropped frame");
        // Unlike `close`, the queue is kept: the message goes out, then the reader ends although a handle is alive.
        assert_eq!(rx.recv(), Some(Message::Shutdown));
        assert!(rx.recv().is_none());
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_none());
        drop(outbox);
    }

    #[test]
    fn a_finished_queue_ends_its_writer_thread_which_lets_go_of_the_pipe() {
        let (pipe_tx, pipe_rx) = mpsc::channel::<Vec<u8>>();
        struct Pipe(mpsc::Sender<Vec<u8>>);
        impl Write for Pipe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.send(buf.to_vec()).map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (outbox, rx) = Outbox::new(4);
        let writer = spawn_writer(&outbox, rx, FrameWriter::new(Pipe(pipe_tx))).unwrap();
        assert!(outbox.send_control(Message::Shutdown));
        outbox.finish();
        let report = writer.join().unwrap();
        assert!(report.io_error.is_none());
        // The handle is still alive; the pipe's other end has the message and then its end.
        let mut bytes = 0;
        while let Ok(chunk) = pipe_rx.recv_timeout(Duration::from_secs(5)) {
            bytes += chunk.len();
        }
        assert!(bytes > 0, "the queued message was written before the end");
        assert!(matches!(pipe_rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)), "the write half was dropped");
        drop(outbox);
    }
}
