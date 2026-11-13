//! The viewer side: reads frame streams, orders them (`sequencer`) and hands decodable frames to the decoder.
//!
//! The session crate owns `accept_uni` and routes each stream by its kind: a stream of kind `Video` goes to
//! [`VideoReceiver::handle_stream`] with the kind already read. [`VideoReceiver::serve`] is the standalone loop for
//! a connection that carries video alone.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::{Connection, RecvStream, VarInt};
use swapdesk_proto::{ErrorCode, KeyframeRequest, ReceiverReport, VIDEO_HEADER_LEN, VideoHeader, Wire};
use tokio::sync::{Notify, mpsc};

use crate::sequencer::{Action, Completed, Sequencer, SequencerConfig, SequencerStats, ahead};
use crate::util::{Either, lock, select2};

/// Stop code of a stream the viewer does not need any more (an old frame, a P-frame while a keyframe is awaited).
pub const STOP_STALE: u32 = 0x1002;
/// Stop code of a stream dropped to stay inside the per-connection buffer budget.
pub const STOP_BUDGET: u32 = 0x1003;

/// Where keyframe requests go: the control stream (`KeyframeRequest`, PROTOCOL.md 5.3).
pub trait FeedbackSink: Send + Sync {
    fn request_keyframe(&self, req: KeyframeRequest);
    /// A frame stream broke the format (a bad header, a payload of the wrong length): a protocol violation the
    /// session may treat as fatal.
    fn malformed_frame(&self) {}
}

impl<F: Fn(KeyframeRequest) + Send + Sync> FeedbackSink for F {
    fn request_keyframe(&self, req: KeyframeRequest) {
        self(req);
    }
}

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    /// How long a frame that finished early waits for its predecessor.
    pub hold: Duration,
    /// Keyframe requests go out at most this often (PROTOCOL.md 4: 200 ms).
    pub request_interval: Duration,
    /// A frame stream must be complete this long after its header arrived.
    pub frame_timeout: Duration,
    /// Bytes buffered per connection (PROTOCOL.md 1: 32 MiB): the unfinished frame streams, the frames parked
    /// behind a missing predecessor and the frames queued for the decoder together. Past it the oldest unfinished
    /// streams are stopped, and new ones are refused while the parked and queued frames alone fill it.
    pub max_buffered: usize,
    /// Frames queued for the decoder (their bytes count in `max_buffered` too).
    pub queue: usize,
    /// How often the ordering timers are checked.
    pub tick: Duration,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            hold: Duration::from_millis(100),
            request_interval: Duration::from_millis(200),
            frame_timeout: Duration::from_secs(2),
            max_buffered: 32 * 1024 * 1024,
            queue: 64,
            tick: Duration::from_millis(10),
        }
    }
}

/// The bytes of a frame in the decoder's queue, counted until the frame is taken out of it and dropped.
struct Queued {
    counter: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for Queued {
    fn drop(&mut self) {
        self.counter.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for Queued {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Queued({} bytes)", self.bytes)
    }
}

/// A frame ready for the decoder.
pub struct ReceivedFrame {
    pub header: VideoHeader,
    /// In a buffer of exactly its length.
    pub payload: Vec<u8>,
    /// When the last byte arrived (the transport's own latency is this minus the host's capture time).
    pub received_at: Instant,
    /// Keeps the frame's bytes in the receiver's budget while it waits for the decoder.
    _queued: Queued,
}

/// The payload is the screen's pixels: `Debug` shows its size only (CONTRACTS.md C2.3).
impl std::fmt::Debug for ReceivedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceivedFrame")
            .field("header", &self.header)
            .field("payload", &swapdesk_proto::RedactedLen(self.payload.len()))
            .field("received_at", &self.received_at)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverStats {
    pub sequence: SequencerStats,
    /// Streams stopped at their header as stale.
    pub refused: u64,
    /// Streams the host reset before they finished.
    pub reset_by_host: u64,
    /// Streams that did not finish in time.
    pub timed_out: u64,
    pub malformed: u64,
    /// Streams stopped by the buffer budget.
    pub budget_stops: u64,
    /// Frames the decoder queue could not take.
    pub queue_full: u64,
    pub streams: u64,
    /// How many times the ordering timer woke (it must not wake while nothing waits).
    pub ticks: u64,
}

struct Open {
    display: u16,
    key: bool,
    cancel: Arc<Notify>,
    /// The stop code to use when `cancel` fires.
    reason: Arc<AtomicU32>,
    bytes: usize,
    frame_id: u32,
}

impl Open {
    fn cancel_with(&self, code: u32) {
        self.reason.store(code, Ordering::SeqCst);
        self.cancel.notify_one();
    }
}

#[derive(Default)]
struct Table {
    next: u64,
    open: BTreeMap<u64, Open>,
    buffered: usize,
}

struct Inner {
    cfg: ReceiverConfig,
    feedback: Arc<dyn FeedbackSink>,
    tx: mpsc::Sender<ReceivedFrame>,
    seq: Mutex<Sequencer<(Vec<u8>, Instant)>>,
    /// Held from the moment the sequencer decides what goes to the decoder until it was handed over, so two streams
    /// that finish together cannot deliver their frames out of the order the sequencer chose.
    order: Mutex<()>,
    table: Mutex<Table>,
    stats: Mutex<ReceiverStats>,
    /// Bytes of the frames queued for the decoder.
    queued: Arc<AtomicUsize>,
    /// Wakes the ordering timer when the sequencer has something new to wait for.
    wake: Arc<Notify>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The ordering timer sleeps without a deadline when nothing waits: tell it the receiver is gone.
        self.wake.notify_one();
    }
}

/// Cheap to clone: clones share the state.
#[derive(Clone)]
pub struct VideoReceiver {
    inner: Arc<Inner>,
}

enum ReadEnd {
    Done(Vec<u8>),
    Cancelled,
    Reset,
    Timeout,
    Malformed,
}

impl VideoReceiver {
    /// Needs a tokio runtime: it starts the ordering timer. Frames come out of the returned channel; dropping the
    /// channel ends the receiver.
    pub fn new(cfg: ReceiverConfig, feedback: Arc<dyn FeedbackSink>) -> (Self, mpsc::Receiver<ReceivedFrame>) {
        let (tx, rx) = mpsc::channel(cfg.queue.max(1));
        let seq = Sequencer::new(SequencerConfig {
            hold: cfg.hold,
            request_interval: cfg.request_interval,
            max_parked_bytes: cfg.max_buffered,
        });
        let inner = Arc::new(Inner {
            cfg,
            feedback,
            tx,
            seq: Mutex::new(seq),
            order: Mutex::new(()),
            table: Mutex::new(Table::default()),
            stats: Mutex::new(ReceiverStats::default()),
            queued: Arc::new(AtomicUsize::new(0)),
            wake: Arc::new(Notify::new()),
        });
        tokio::spawn(ticker(Arc::downgrade(&inner), Arc::clone(&inner.wake), inner.tx.clone()));
        (Self { inner }, rx)
    }

    pub fn stats(&self) -> ReceiverStats {
        let mut s = *lock(&self.inner.stats);
        s.sequence = lock(&self.inner.seq).stats();
        s
    }

    /// The once-a-second `ReceiverReport`. The frame counters are cumulative; the decoder's and presenter's own
    /// figures come from the caller.
    pub fn report(&self, decode_us_p50: u32, present_lag_us_p50: u32, audio_underruns: u32) -> ReceiverReport {
        let s = lock(&self.inner.seq).stats();
        ReceiverReport {
            frames_presented: s.delivered as u32,
            frames_lost: s.lost as u32,
            decode_us_p50,
            present_lag_us_p50,
            audio_underruns,
        }
    }

    /// The decoder failed on a frame it was given: wait for a keyframe and ask for one.
    pub fn decode_failed(&self, display: u16) {
        self.inner.step(|seq, now| seq.decode_failed(now, display));
        self.inner.wake.notify_one();
    }

    /// The displays of the host's last `DisplayList` (PROTOCOL.md 5.3, at most 16): frames of any other display id
    /// are refused. Until a list is given, the first 16 displays seen are accepted.
    pub fn set_displays(&self, ids: &[u16]) {
        lock(&self.inner.seq).set_displays(ids);
    }

    /// Takes a unidirectional stream whose kind (`Video`) was already read.
    pub fn handle_stream(&self, recv: RecvStream) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { inner.read_stream(recv).await });
    }

    /// For a connection that carries video alone: accepts unidirectional streams until the connection ends and
    /// handles those of kind `Video`; any other kind is stopped.
    pub async fn serve(&self, conn: Connection) {
        while let Ok(mut recv) = conn.accept_uni().await {
            let me = self.clone();
            tokio::spawn(async move {
                let mut kind = [0u8; 1];
                match tokio::time::timeout(me.inner.cfg.frame_timeout, recv.read_exact(&mut kind)).await {
                    Ok(Ok(())) if kind[0] == swapdesk_proto::StreamKind::Video.tag() => me.handle_stream(recv),
                    _ => {
                        let _ = recv.stop(VarInt::from_u32(u32::from(ErrorCode::ProtocolViolation.tag())));
                    }
                }
            });
        }
    }
}

/// The ordering timer: it sleeps until the sequencer's next deadline (the end of a hold, the next repeat of a
/// keyframe request) and, with nothing waiting, until something changes or the receiver is gone.
async fn ticker(me: Weak<Inner>, wake: Arc<Notify>, tx: mpsc::Sender<ReceivedFrame>) {
    loop {
        let next = {
            let Some(inner) = me.upgrade() else { return };
            if inner.tx.is_closed() {
                return;
            }
            inner.count(|s| s.ticks += 1);
            inner.step(|seq, now| seq.tick(now));
            lock(&inner.seq).next_deadline(Instant::now())
        };
        let changed = select2(wake.notified(), tx.closed());
        match next {
            None => {
                changed.await;
            }
            Some(at) => {
                select2(tokio::time::sleep_until(tokio::time::Instant::from_std(at)), changed).await;
            }
        }
    }
}

/// Pieces of a stream taken per read (one lock of the connection each).
const PIECES_PER_READ: usize = 32;
/// The most a frame's buffer reserves ahead of its bytes. A frame up to this size is read into one buffer of exactly
/// its length; a larger one into buffers of this size, made as the bytes come and joined once at the end.
const RESERVE_AHEAD: usize = 256 * 1024;

/// A frame's payload while its stream is read: owned bytes only, so nothing of QUIC's is kept, in buffers that are
/// made at their final size and never grow.
struct Assembly {
    want: usize,
    have: usize,
    /// Full buffers of a frame larger than [`RESERVE_AHEAD`].
    full: Vec<Vec<u8>>,
    /// The buffer being filled; its capacity is the room it was made with.
    open: Vec<u8>,
    /// Bytes `open` was made for.
    room: usize,
}

impl Assembly {
    fn new(want: usize) -> Self {
        let room = want.min(RESERVE_AHEAD);
        Self { want, have: 0, full: Vec::new(), open: Vec::with_capacity(room), room }
    }

    fn len(&self) -> usize {
        self.have
    }

    /// Copies `piece` in; `false` when the payload would run past its declared length (nothing is copied then).
    fn push(&mut self, mut piece: &[u8]) -> bool {
        if piece.len() > self.want - self.have {
            return false;
        }
        while !piece.is_empty() {
            if self.open.len() == self.room {
                self.room = (self.want - self.have).min(RESERVE_AHEAD);
                self.full.push(std::mem::replace(&mut self.open, Vec::with_capacity(self.room)));
            }
            let n = piece.len().min(self.room - self.open.len());
            self.open.extend_from_slice(&piece[..n]);
            self.have += n;
            piece = &piece[n..];
        }
        true
    }

    /// The payload in one buffer of exactly its length, or `None` when bytes are missing.
    fn finish(self) -> Option<Vec<u8>> {
        if self.have != self.want {
            return None;
        }
        if self.full.is_empty() {
            return Some(self.open);
        }
        let mut out = Vec::with_capacity(self.want);
        for part in self.full.iter().chain(std::iter::once(&self.open)) {
            out.extend_from_slice(part);
        }
        Some(out)
    }
}

fn stop(recv: &mut RecvStream, code: u32) {
    let _ = recv.stop(VarInt::from_u32(code));
}

impl Inner {
    fn count(&self, f: impl FnOnce(&mut ReceiverStats)) {
        f(&mut lock(&self.stats));
    }

    /// Asks the sequencer what to do and does it, with nothing else deciding or delivering in between.
    fn step(&self, f: impl FnOnce(&mut Sequencer<(Vec<u8>, Instant)>, Instant) -> Vec<Action<(Vec<u8>, Instant)>>) {
        let _order = lock(&self.order);
        let actions = f(&mut lock(&self.seq), Instant::now());
        self.apply(actions);
    }

    /// Does what the sequencer decided. Called with `order` held. Once a frame of a display could not be handed to the
    /// decoder, the frames after it in the same batch are dropped too: the decoder would be given a P-frame whose
    /// reference it never saw.
    fn apply(&self, mut actions: Vec<Action<(Vec<u8>, Instant)>>) {
        let mut broken: Vec<u16> = Vec::new();
        while !actions.is_empty() {
            let mut next = Vec::new();
            for action in actions {
                match action {
                    Action::Deliver(Completed { header, .. }) if broken.contains(&header.display_id) => {
                        self.count(|s| s.queue_full += 1);
                    }
                    Action::Deliver(Completed { header, body: (payload, received_at) }) => {
                        // Streams still open for frames older than this one are of no use any more (PROTOCOL.md 4).
                        for o in lock(&self.table)
                            .open
                            .values()
                            .filter(|o| o.display == header.display_id && ahead(o.frame_id, header.frame_id) < 0)
                        {
                            o.cancel_with(STOP_STALE);
                        }
                        // The decoder's queue is bounded in bytes as well as in frames; a frame bigger than the whole
                        // budget still gets in when the queue is empty.
                        let bytes = payload.len();
                        let before = self.queued.fetch_add(bytes, Ordering::SeqCst);
                        let queued = Queued { counter: Arc::clone(&self.queued), bytes };
                        let fits = before == 0 || before + bytes <= self.cfg.max_buffered;
                        if !fits
                            || self
                                .tx
                                .try_send(ReceivedFrame { header, payload, received_at, _queued: queued })
                                .is_err()
                        {
                            self.count(|s| s.queue_full += 1);
                            broken.push(header.display_id);
                            next.extend(lock(&self.seq).decode_failed(Instant::now(), header.display_id));
                        }
                    }
                    Action::Request(req) => self.feedback.request_keyframe(req),
                    Action::StopUnfinished { display } => {
                        for o in lock(&self.table).open.values().filter(|o| o.display == display && !o.key) {
                            o.cancel_with(STOP_STALE);
                        }
                    }
                }
            }
            actions = next;
        }
    }

    async fn read_stream(self: Arc<Self>, mut recv: RecvStream) {
        self.count(|s| s.streams += 1);
        let mut head = [0u8; VIDEO_HEADER_LEN];
        match tokio::time::timeout(self.cfg.frame_timeout, recv.read_exact(&mut head)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                self.count(|s| s.reset_by_host += 1);
                return;
            }
            Err(_) => {
                self.count(|s| s.timed_out += 1);
                stop(&mut recv, STOP_STALE);
                return;
            }
        }
        let Ok(header) = VideoHeader::decode(&head) else {
            self.count(|s| s.malformed += 1);
            stop(&mut recv, u32::from(ErrorCode::ProtocolViolation.tag()));
            self.feedback.malformed_frame();
            return;
        };
        {
            let mut seq = lock(&self.seq);
            if !seq.accepts(&header) {
                drop(seq);
                self.count(|s| s.refused += 1);
                stop(&mut recv, STOP_STALE);
                return;
            }
            // What is parked and queued already fills the budget: nothing more is taken until it drains.
            if self.held_bytes(&seq) >= self.cfg.max_buffered {
                drop(seq);
                self.count(|s| s.budget_stops += 1);
                stop(&mut recv, STOP_BUDGET);
                return;
            }
            seq.started(&header);
        }
        let cancel = Arc::new(Notify::new());
        let reason = Arc::new(AtomicU32::new(STOP_STALE));
        let id = {
            let mut t = lock(&self.table);
            let id = t.next;
            t.next += 1;
            let open = Open {
                display: header.display_id,
                key: header.keyframe || header.discontinuity,
                cancel: Arc::clone(&cancel),
                reason: Arc::clone(&reason),
                bytes: 0,
                frame_id: header.frame_id,
            };
            t.open.insert(id, open);
            id
        };
        let end = self.read_payload(&mut recv, &header, id, &cancel).await;
        {
            let mut t = lock(&self.table);
            if let Some(o) = t.open.remove(&id) {
                t.buffered = t.buffered.saturating_sub(o.bytes);
            }
        }
        if !matches!(end, ReadEnd::Done(_)) {
            lock(&self.seq).aborted(&header);
        }
        match end {
            ReadEnd::Done(payload) => {
                self.step(|seq, now| seq.complete(now, Completed { header, body: (payload, now) }));
                // A frame may now wait for a predecessor: the timer needs its deadline.
                self.wake.notify_one();
            }
            ReadEnd::Cancelled => stop(&mut recv, reason.load(Ordering::SeqCst)),
            ReadEnd::Reset => self.count(|s| s.reset_by_host += 1),
            ReadEnd::Timeout => {
                self.count(|s| s.timed_out += 1);
                stop(&mut recv, STOP_STALE);
            }
            ReadEnd::Malformed => {
                self.count(|s| s.malformed += 1);
                stop(&mut recv, u32::from(ErrorCode::ProtocolViolation.tag()));
                self.feedback.malformed_frame();
            }
        }
    }

    /// Reads exactly `payload_len` bytes and then the end of the stream, several pieces per look at the connection.
    /// Each piece is copied into the frame's own buffer as it comes ([`Assembly`]), so what is held is what is
    /// counted: memory follows the bytes that arrived, never the declared length, and no buffer grows on the way.
    async fn read_payload(&self, recv: &mut RecvStream, header: &VideoHeader, id: u64, cancel: &Notify) -> ReadEnd {
        let want = header.payload_len as usize;
        let mut frame = Assembly::new(want);
        let mut batch: [Bytes; PIECES_PER_READ] = std::array::from_fn(|_| Bytes::new());
        let deadline = tokio::time::Instant::now() + self.cfg.frame_timeout;
        loop {
            let read =
                select2(cancel.notified(), tokio::time::timeout_at(deadline, recv.read_chunks(&mut batch))).await;
            let filled = match read {
                Either::Left(()) => return ReadEnd::Cancelled,
                Either::Right(Err(_)) => return ReadEnd::Timeout,
                Either::Right(Ok(Err(_))) => return ReadEnd::Reset,
                Either::Right(Ok(Ok(n))) => n,
            };
            let Some(filled) = filled else {
                return frame.finish().map_or(ReadEnd::Malformed, ReadEnd::Done);
            };
            let before = frame.len();
            for piece in &mut batch[..filled] {
                // The piece is let go here: it keeps the whole datagram it came in alive, which no budget counts.
                if !frame.push(&std::mem::take(piece)) {
                    // A payload that runs past its length.
                    return ReadEnd::Malformed;
                }
            }
            self.account(id, frame.len() - before);
        }
    }

    /// Bytes held outside the unfinished streams: parked behind a missing predecessor, or queued for the decoder.
    fn held_bytes(&self, seq: &Sequencer<(Vec<u8>, Instant)>) -> usize {
        seq.parked_bytes() + self.queued.load(Ordering::SeqCst)
    }

    /// Adds `n` buffered bytes and, past the budget, stops the oldest unfinished streams first.
    fn account(&self, id: u64, n: usize) {
        let held = {
            let seq = lock(&self.seq);
            self.held_bytes(&seq)
        };
        let mut t = lock(&self.table);
        t.buffered += n;
        if let Some(o) = t.open.get_mut(&id) {
            o.bytes += n;
        }
        let mut over = (t.buffered + held).saturating_sub(self.cfg.max_buffered);
        if over == 0 {
            return;
        }
        let mut stopped = 0u64;
        for o in t.open.values() {
            if over == 0 {
                break;
            }
            if o.reason.load(Ordering::SeqCst) != STOP_BUDGET {
                o.cancel_with(STOP_BUDGET);
                stopped += 1;
            }
            over = over.saturating_sub(o.bytes);
        }
        drop(t);
        self.count(|s| s.budget_stops += stopped);
    }
}

#[cfg(test)]
mod assembly_tests {
    use super::*;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// `bytes` fed in pieces of `step`.
    fn fed(want: usize, bytes: &[u8], step: usize) -> Assembly {
        let mut frame = Assembly::new(want);
        for piece in bytes.chunks(step) {
            assert!(frame.push(piece));
        }
        frame
    }

    #[test]
    fn an_ordinary_frame_is_read_into_one_buffer_of_exactly_its_length() {
        let bytes = pattern(80_667);
        let frame = fed(bytes.len(), &bytes, 1163);
        // One buffer, made before the first byte and never moved.
        let at = frame.open.as_ptr();
        assert!(frame.full.is_empty());
        let out = frame.finish().expect("complete");
        assert_eq!((out.as_ptr(), out.capacity()), (at, bytes.len()));
        assert_eq!(out, bytes);
    }

    #[test]
    fn a_large_frame_is_read_in_buffers_that_follow_its_bytes_and_joined_at_its_exact_length() {
        for len in [RESERVE_AHEAD, RESERVE_AHEAD + 1, 3 * RESERVE_AHEAD, 3 * RESERVE_AHEAD + 4567] {
            let bytes = pattern(len);
            for step in [1163, RESERVE_AHEAD - 1, RESERVE_AHEAD, len] {
                let mut frame = Assembly::new(len);
                let mut reserved_most = 0;
                for piece in bytes.chunks(step) {
                    assert!(frame.push(piece));
                    // Never more than one buffer's room ahead of the bytes that arrived, whatever the length says.
                    let reserved = frame.full.iter().map(Vec::capacity).sum::<usize>() + frame.open.capacity();
                    reserved_most = reserved_most.max(reserved - frame.len());
                }
                assert!(reserved_most <= RESERVE_AHEAD, "{reserved_most} bytes reserved ahead");
                assert_eq!(frame.len(), len);
                let out = frame.finish().expect("complete");
                assert_eq!(out.capacity(), len, "a frame of {len} bytes in pieces of {step}");
                assert!(out == bytes, "a frame of {len} bytes in pieces of {step}");
            }
        }
    }

    #[test]
    fn a_payload_past_its_length_is_refused_and_a_short_one_is_not_a_frame() {
        let bytes = pattern(1000);
        let mut frame = fed(1000, &bytes, 300);
        assert!(!frame.push(&[1]), "one byte too many");
        assert_eq!(frame.len(), 1000, "the refused piece was not taken");
        assert_eq!(frame.finish(), Some(bytes.clone()));
        // A piece that would cross the end is refused whole.
        let mut frame = fed(1000, &bytes[..900], 300);
        assert!(!frame.push(&[0; 101]));
        assert_eq!(frame.len(), 900);
        assert_eq!(frame.finish(), None, "bytes are missing");
        // No payload at all.
        let mut empty = Assembly::new(0);
        assert!(empty.push(&[]));
        assert!(!empty.push(&[1]));
        assert_eq!(empty.finish(), Some(Vec::new()));
        // A header that declares 8 MiB reserves one buffer's room and no more until bytes come.
        let big = Assembly::new(8 * 1024 * 1024);
        assert_eq!(big.open.capacity(), RESERVE_AHEAD);
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;
    use swapdesk_proto::Codec;

    #[test]
    fn debug_of_a_received_frame_shows_its_size_and_never_its_bytes() {
        let header = VideoHeader {
            keyframe: false,
            config: false,
            discontinuity: false,
            display_id: 0,
            frame_id: 9,
            capture_us: 0,
            encode_us: 0,
            width: 1280,
            height: 720,
            codec: Codec::H264,
            payload_len: 48,
        };
        let counter = Arc::new(AtomicUsize::new(48));
        let frame = ReceivedFrame {
            header,
            payload: vec![233; 48],
            received_at: Instant::now(),
            _queued: Queued { counter, bytes: 48 },
        };
        let shown = format!("{frame:?}");
        assert!(!shown.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "payload bytes leaked: {shown}");
        assert!(shown.contains("<redacted 48 bytes>"), "{shown}");
    }
}
