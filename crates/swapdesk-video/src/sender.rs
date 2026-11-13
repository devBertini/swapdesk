//! The host side: one unidirectional stream per encoded frame (PROTOCOL.md 4 and 7.1).
//!
//! `send` opens a stream, writes the kind, the 32-byte header and the payload, and finishes it, from a task of its
//! own, so a frame stuck behind loss never blocks the next. Staleness (PROTOCOL.md 4):
//!
//! * a keyframe (or a discontinuity) resets every unfinished stream of its display before it is sent;
//! * when the oldest unfinished P-frame is older than `backlog_limit`, every unfinished P-frame stream of the
//!   display is reset, a keyframe is wanted and the bitrate is cut; the frame at hand is dropped, since its
//!   reference is gone;
//! * until the keyframe is sent, P-frames are dropped instead of sent;
//! * pausing (a revoked `VIEW`, a locked desktop) resets every unfinished stream of every display.
//!
//! A keyframe is exempt from the age rule and from the backlog reset, so a slow link cannot reset it forever.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::{Connection, SendStream, VarInt};
use swapdesk_proto::{KeyframeRequest, MAX_VIDEO_PAYLOAD, ReceiverReport, StreamKind, VIDEO_HEADER_LEN, Wire, Writer};
use tokio::sync::Notify;

use crate::rate::{BitrateSink, RateConfig, RateController};
use crate::util::{Either, lock, select2};

/// Quinn send priority of a video frame stream (PROTOCOL.md 4: higher goes first).
pub const VIDEO_PRIORITY: i32 = 1;
/// Reset code of a stream whose frame went stale at the host (a newer keyframe, a backlog, a timeout).
pub const RESET_STALE: u32 = 0x1001;

/// A frame to send: the header says what it is; `payload_len` is set from the payload. The payload is shared by
/// reference count: QUIC keeps a handle to these same bytes until the viewer has them, and nothing is copied.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: swapdesk_proto::VideoHeader,
    pub payload: Bytes,
}

/// The payload is the screen's pixels (up to 8 MiB): `Debug` shows its size only (CONTRACTS.md C2.3).
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("header", &self.header)
            .field("payload", &swapdesk_proto::RedactedLen(self.payload.len()))
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct SenderConfig {
    /// Age of the oldest unfinished P-frame past which the backlog is reset (GUESS 100 ms).
    pub backlog_limit: Duration,
    /// How long `send` waits for the peer to allow another stream.
    pub open_timeout: Duration,
    /// A frame stream that is not fully acknowledged by now is reset.
    pub frame_timeout: Duration,
    /// Keyframes are honoured at most this often, so a flood of requests cannot make the encoder emit only keyframes.
    pub min_keyframe_interval: Duration,
    /// Most unfinished frame streams at once.
    pub max_in_flight: usize,
    pub rate: RateConfig,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            backlog_limit: Duration::from_millis(100),
            open_timeout: Duration::from_millis(250),
            frame_timeout: Duration::from_secs(2),
            min_keyframe_interval: Duration::from_millis(100),
            max_in_flight: 128,
            rate: RateConfig::default(),
        }
    }
}

/// What happened to a frame handed to `send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// A stream is open and writing.
    Sent,
    /// Video is paused (`HostState.paused`, or `VIEW` revoked): nothing was sent.
    Paused,
    /// The backlog was reset: this P-frame was dropped, a keyframe is wanted.
    DroppedBacklog,
    /// A keyframe is wanted and this is not one.
    DroppedAwaitingKeyframe,
    /// The peer allows no more streams for now, or the in-flight limit is reached.
    DroppedFull,
    /// The connection is gone.
    Closed,
    /// The payload is over 8 MiB.
    TooLarge,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SenderStats {
    pub sent: u64,
    pub bytes: u64,
    /// Streams the peer acknowledged completely.
    pub completed: u64,
    pub reset_for_keyframe: u64,
    pub reset_for_backlog: u64,
    pub reset_for_timeout: u64,
    /// Streams reset because the sender paused.
    pub reset_for_pause: u64,
    /// The peer stopped the stream (it considered the frame stale).
    pub stopped_by_peer: u64,
    pub dropped_paused: u64,
    pub dropped_other: u64,
    pub keyframe_requests: u64,
    pub keyframes_sent: u64,
}

struct InFlight {
    display: u16,
    key: bool,
    queued: Instant,
    reset: Arc<Notify>,
}

struct State {
    next_seq: u64,
    in_flight: HashMap<u64, InFlight>,
    wanted: HashSet<u16>,
    last_key: Option<Instant>,
    stats: SenderStats,
    rate: RateController,
    prev_packets: (u64, u64),
}

struct Inner {
    conn: Connection,
    cfg: SenderConfig,
    sink: Arc<dyn BitrateSink>,
    paused: AtomicBool,
    state: Mutex<State>,
}

/// Sends the frames of one session's video. Cheap to clone: clones share the state.
#[derive(Clone)]
pub struct VideoSender {
    inner: Arc<Inner>,
}

/// Resets the unfinished streams of `display`. They leave the table at once: a second look at the backlog must not
/// count streams that are already being reset (their tasks remove themselves a moment later, which is then a no-op).
fn reset_all(st: &mut State, display: u16, only_p: bool) -> usize {
    let keys: Vec<u64> =
        st.in_flight.iter().filter(|(_, f)| f.display == display && !(only_p && f.key)).map(|(k, _)| *k).collect();
    for k in &keys {
        if let Some(f) = st.in_flight.remove(k) {
            f.reset.notify_one();
        }
    }
    keys.len()
}

impl VideoSender {
    pub fn new(conn: Connection, cfg: SenderConfig, sink: Arc<dyn BitrateSink>) -> Self {
        let rate = RateController::new(cfg.rate.clone());
        sink.set_target_kbps(rate.target_kbps());
        let state = State {
            next_seq: 0,
            in_flight: HashMap::new(),
            wanted: HashSet::new(),
            last_key: None,
            stats: SenderStats::default(),
            rate,
            prev_packets: (0, 0),
        };
        Self { inner: Arc::new(Inner { conn, cfg, sink, paused: AtomicBool::new(false), state: Mutex::new(state) }) }
    }

    /// Pauses or resumes sending (`HostState.paused`, a revoked `VIEW`). On resume a keyframe is wanted for every
    /// display that was running, since frames were skipped.
    ///
    /// Pausing resets every frame stream still in flight, so nothing that was queued before the pause (a revoked
    /// `VIEW`, a locked desktop) reaches the viewer afterwards.
    pub fn set_paused(&self, paused: bool) {
        let was = self.inner.paused.swap(paused, Ordering::SeqCst);
        if was && !paused {
            self.want_keyframe_everywhere();
        }
        if paused && !was {
            let mut st = lock(&self.inner.state);
            let displays: HashSet<u16> = st.in_flight.values().map(|f| f.display).collect();
            for d in displays {
                let n = reset_all(&mut st, d, false);
                st.stats.reset_for_pause += n as u64;
            }
        }
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::SeqCst)
    }

    fn want_keyframe_everywhere(&self) {
        let mut st = lock(&self.inner.state);
        let displays: HashSet<u16> = st.in_flight.values().map(|f| f.display).collect();
        st.wanted.extend(displays);
        st.wanted.insert(0);
    }

    /// The receiver asked for a keyframe (`KeyframeRequest` on the control stream).
    pub fn on_keyframe_request(&self, req: &KeyframeRequest) {
        let mut st = lock(&self.inner.state);
        st.stats.keyframe_requests += 1;
        st.wanted.insert(req.display);
    }

    /// The encoder asks before every frame: true when this display needs a keyframe now. Clears the want.
    pub fn take_keyframe_request(&self, display: u16) -> bool {
        let mut st = lock(&self.inner.state);
        if !st.wanted.contains(&display) {
            return false;
        }
        let due = st.last_key.is_none_or(|t| t.elapsed() >= self.inner.cfg.min_keyframe_interval);
        if due {
            st.wanted.remove(&display);
        }
        due
    }

    /// The receiver's once-a-second report: feeds rate control with its frame counters, the path's RTT and the
    /// packet loss since the last report.
    pub fn on_receiver_report(&self, report: &ReceiverReport) {
        let stats = self.inner.conn.stats();
        let rtt = stats.path.rtt;
        let mut st = lock(&self.inner.state);
        let (lost0, sent0) = st.prev_packets;
        let (lost, sent) = (stats.path.lost_packets, stats.path.sent_packets);
        st.prev_packets = (lost, sent);
        let d_sent = sent.saturating_sub(sent0);
        let loss = if d_sent == 0 { 0.0 } else { lost.saturating_sub(lost0) as f64 / d_sent as f64 };
        if let Some((kbps, _)) = st.rate.on_report(Instant::now(), report, rtt, loss) {
            drop(st);
            self.inner.sink.set_target_kbps(kbps);
        }
    }

    /// The viewer's own ceiling from `VideoSettings.max_kbps` (0 = none).
    pub fn set_bitrate_cap(&self, max_kbps: u32) {
        let new = lock(&self.inner.state).rate.set_cap(max_kbps);
        if let Some(kbps) = new {
            self.inner.sink.set_target_kbps(kbps);
        }
    }

    pub fn target_kbps(&self) -> u32 {
        lock(&self.inner.state).rate.target_kbps()
    }

    pub fn stats(&self) -> SenderStats {
        lock(&self.inner.state).stats
    }

    /// Frame streams not yet fully acknowledged.
    pub fn in_flight(&self) -> usize {
        lock(&self.inner.state).in_flight.len()
    }

    /// Opens a stream for `frame` and starts writing it. Returns once the stream is open: the write finishes on a
    /// task of its own.
    pub async fn send(&self, mut frame: Frame) -> SendOutcome {
        let Ok(len) = u32::try_from(frame.payload.len()) else { return SendOutcome::TooLarge };
        if len > MAX_VIDEO_PAYLOAD {
            return SendOutcome::TooLarge;
        }
        frame.header.payload_len = len;
        let h = frame.header;
        if self.is_paused() {
            lock(&self.inner.state).stats.dropped_paused += 1;
            return SendOutcome::Paused;
        }
        let key = h.keyframe || h.discontinuity;
        let backlog_cut = {
            let mut st = lock(&self.inner.state);
            let now = Instant::now();
            if key {
                let n = reset_all(&mut st, h.display_id, false);
                st.stats.reset_for_keyframe += n as u64;
                st.wanted.remove(&h.display_id);
                st.last_key = Some(now);
                st.stats.keyframes_sent += 1;
                None
            } else {
                let stale = st
                    .in_flight
                    .values()
                    .filter(|f| f.display == h.display_id && !f.key)
                    .any(|f| now.saturating_duration_since(f.queued) > self.inner.cfg.backlog_limit);
                if stale {
                    // A keyframe in flight is exempt: the reset is of the P-frames that depend on what was lost.
                    let n = reset_all(&mut st, h.display_id, true);
                    st.stats.reset_for_backlog += n as u64;
                    st.wanted.insert(h.display_id);
                    st.stats.dropped_other += 1;
                    Some(st.rate.on_backlog(now))
                } else if st.wanted.contains(&h.display_id) {
                    st.stats.dropped_other += 1;
                    return SendOutcome::DroppedAwaitingKeyframe;
                } else if st.in_flight.len() >= self.inner.cfg.max_in_flight {
                    st.stats.dropped_other += 1;
                    st.wanted.insert(h.display_id);
                    return SendOutcome::DroppedFull;
                } else {
                    None
                }
            }
        };
        if let Some(new_rate) = backlog_cut {
            if let Some(kbps) = new_rate {
                self.inner.sink.set_target_kbps(kbps);
            }
            return SendOutcome::DroppedBacklog;
        }

        let opened = tokio::time::timeout(self.inner.cfg.open_timeout, self.inner.conn.open_uni()).await;
        let mut stream = match opened {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => return SendOutcome::Closed,
            Err(_) => {
                let mut st = lock(&self.inner.state);
                st.stats.dropped_other += 1;
                st.wanted.insert(h.display_id);
                return SendOutcome::DroppedFull;
            }
        };
        let _ = stream.set_priority(VIDEO_PRIORITY);
        // The stream is its kind, the header and the payload: the first two in a small buffer of their own, the
        // payload as it came.
        let mut head = Writer::appending(Vec::with_capacity(1 + VIDEO_HEADER_LEN));
        if StreamKind::Video.put(&mut head, 0).is_err() || h.put(&mut head, 0).is_err() {
            return SendOutcome::TooLarge;
        }
        let head = Bytes::from(head.into_bytes());
        let wire_len = head.len() + frame.payload.len();
        let parts = [head, frame.payload];

        let reset = Arc::new(Notify::new());
        let seq = {
            let mut st = lock(&self.inner.state);
            // A pause that came while the stream was opening: `set_paused` has already looked at the table, so this
            // frame must not enter it.
            if self.is_paused() {
                st.stats.dropped_paused += 1;
                drop(st);
                let _ = stream.reset(VarInt::from_u32(RESET_STALE));
                return SendOutcome::Paused;
            }
            let seq = st.next_seq;
            st.next_seq += 1;
            st.in_flight.insert(
                seq,
                InFlight { display: h.display_id, key, queued: Instant::now(), reset: Arc::clone(&reset) },
            );
            st.stats.sent += 1;
            st.stats.bytes += wire_len as u64;
            seq
        };
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let end = push(&mut stream, parts, &reset, inner.cfg.frame_timeout).await;
            let mut st = lock(&inner.state);
            st.in_flight.remove(&seq);
            match end {
                PushEnd::Acked => st.stats.completed += 1,
                PushEnd::StoppedByPeer => st.stats.stopped_by_peer += 1,
                PushEnd::ResetByUs => {}
                PushEnd::TimedOut => {
                    st.stats.reset_for_timeout += 1;
                    st.wanted.insert(h.display_id);
                }
                PushEnd::Failed => {}
            }
        });
        SendOutcome::Sent
    }
}

enum PushEnd {
    Acked,
    StoppedByPeer,
    ResetByUs,
    TimedOut,
    Failed,
}

/// Writes the whole frame (its head, then its payload), finishes the stream and waits for the peer to acknowledge it,
/// unless told to reset. QUIC takes the two buffers as they are.
async fn push(stream: &mut SendStream, mut parts: [Bytes; 2], reset: &Notify, limit: Duration) -> PushEnd {
    let stopped = stream.stopped();
    // A frame without a payload is its head alone.
    let filled = if parts[1].is_empty() { 1 } else { 2 };
    let work = async {
        stream.write_all_chunks(&mut parts[..filled]).await.map_err(|_| ())?;
        stream.finish().map_err(|_| ())?;
        stopped.await.map_err(|_| ())
    };
    let outcome = select2(reset.notified(), tokio::time::timeout(limit, work)).await;
    match outcome {
        Either::Left(()) => {
            let _ = stream.reset(VarInt::from_u32(RESET_STALE));
            PushEnd::ResetByUs
        }
        Either::Right(Ok(Ok(None))) => PushEnd::Acked,
        Either::Right(Ok(Ok(Some(_)))) => PushEnd::StoppedByPeer,
        Either::Right(Ok(Err(()))) => PushEnd::Failed,
        Either::Right(Err(_)) => {
            let _ = stream.reset(VarInt::from_u32(RESET_STALE));
            PushEnd::TimedOut
        }
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;
    use swapdesk_proto::{Codec, VideoHeader};

    #[test]
    fn debug_of_a_frame_shows_its_size_and_never_its_bytes() {
        let header = VideoHeader {
            keyframe: true,
            config: false,
            discontinuity: false,
            display_id: 0,
            frame_id: 3,
            capture_us: 0,
            encode_us: 0,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            payload_len: 64,
        };
        let shown = format!("{:?}", Frame { header, payload: vec![233; 64].into() });
        // Byte 233 as a `Vec<u8>` prints it (`233`) or as `Bytes` does (`\xe9`).
        assert!(!shown.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "payload bytes leaked: {shown}");
        assert!(!shown.contains("xe9"), "payload bytes leaked: {shown}");
        assert!(format!("{:?}", Bytes::from(vec![233u8; 4])).contains("xe9"), "the check knows how `Bytes` prints");
        assert!(shown.contains("<redacted 64 bytes>"), "{shown}");
    }
}
