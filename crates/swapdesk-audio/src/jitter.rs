//! The adaptive jitter buffer: pure logic over packets and a caller-supplied time, no Opus and no clock.
//!
//! The caller pushes every datagram as it arrives (`push`) and pulls one answer per playout tick, every
//! [`FRAME_US`] (`pop`). The buffer decides what the tick plays:
//!
//! * [`Playout::Waiting`]: pre-buffering (the stream has not started, or it was resynchronised).
//! * [`Playout::Packet`]: the next frame in sequence.
//! * [`Playout::Lost`]: the next frame never came but a later one is queued: conceal it (Opus PLC or FEC, with the
//!   following packet when there is one).
//! * [`Playout::Starved`]: nothing is queued; the tick is concealed and the sequence number is held, so the late
//!   packet still plays when it arrives. Starvation is the buffer's underrun and is counted.
//!
//! Target depth. The delay a packet needs on top of the fastest path is the largest lateness seen recently. Lateness
//! of a packet is its arrival time minus `seq * 10 ms` minus the lowest such offset tracked over the stream (the
//! sender's and receiver's clocks may differ by any constant; a slow upward creep of the minimum follows their drift).
//! The peak lateness grows at once and decays by 0.5 % per packet (about 2 s), and the target is 1.1 times the peak,
//! rounded up to whole frames and held in [`JitterConfig::min_ms`], [`JitterConfig::max_ms`] (20 to 40 ms by
//! default, B3). The peak starts at the maximum, so a stream begins safe and the target glides down as the link proves
//! calm. Playout starts when the queue spans the target. When the queue stays more than one frame deeper than the
//! target for a second, one frame is dropped every half second until it is not. When the frames queued fall two or
//! more below the target (the playout clock is faster than the sender's), one frame is repeated (concealed, the packet
//! held for the next tick) at most every half second and counted as `stretched`, so a clock drift of 0.1 % never
//! reaches an empty queue (W1.2).

use std::collections::BTreeMap;

use swapdesk_proto::AudioHeader;

use crate::FRAME_US;

#[derive(Debug, Clone)]
pub struct JitterConfig {
    /// Lowest target depth, ms (B3: 20).
    pub min_ms: u32,
    /// Highest target depth, ms (B3: 40).
    pub max_ms: u32,
    /// Frames the queue may span before it is flushed and restarted (a link stall that delivers a burst).
    pub max_queue_frames: usize,
    /// Starved ticks in a row before the stream is resynchronised.
    pub max_hold_frames: u32,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self { min_ms: 20, max_ms: 40, max_queue_frames: 50, max_hold_frames: 10 }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct QueuedPacket {
    pub header: AudioHeader,
    pub payload: Vec<u8>,
}

/// The payload is the host's system audio: `Debug` shows its size only (CONTRACTS.md C2.3).
impl core::fmt::Debug for QueuedPacket {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("QueuedPacket")
            .field("header", &self.header)
            .field("payload", &swapdesk_proto::RedactedLen(self.payload.len()))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playout {
    Waiting,
    Packet(QueuedPacket),
    /// `following`: the Opus packet after the lost frame, when it is queued (for FEC).
    Lost {
        following: Option<Vec<u8>>,
    },
    Starved,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JitterStats {
    pub received: u64,
    /// Arrived after its frame was played or concealed.
    pub late: u64,
    pub duplicates: u64,
    /// Frames concealed because a later packet was queued.
    pub lost: u64,
    /// Ticks with an empty queue mid-stream.
    pub underruns: u64,
    /// Restarts after too many starved ticks.
    pub resyncs: u64,
    /// Frames dropped to bring the depth down to the target.
    pub trimmed: u64,
    /// Frames repeated (concealed) ahead of starvation to bring the depth back up (clock drift, W1.2).
    pub stretched: u64,
    /// Flushes because the queue spanned too many frames.
    pub overflows: u64,
    pub discontinuities: u64,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    queue: BTreeMap<i64, QueuedPacket>,
    next: Option<i64>,
    started: bool,
    have_ref: bool,
    ref_seq: u32,
    ref_ext: i64,
    have_base: bool,
    base_us: f64,
    peak_us: f64,
    hold_run: u32,
    excess_since: Option<u64>,
    last_trim: u64,
    last_stretch: u64,
    stats: JitterStats,
}

/// Least time between two stretched frames, microseconds.
const STRETCH_GAP_US: u64 = 500_000;

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        let peak_us = f64::from(cfg.max_ms) * 1000.0;
        Self {
            cfg,
            queue: BTreeMap::new(),
            next: None,
            started: false,
            have_ref: false,
            ref_seq: 0,
            ref_ext: 0,
            have_base: false,
            base_us: 0.0,
            peak_us,
            hold_run: 0,
            excess_since: None,
            last_trim: 0,
            last_stretch: 0,
            stats: JitterStats::default(),
        }
    }

    pub fn stats(&self) -> &JitterStats {
        &self.stats
    }

    /// The target depth now, in milliseconds (a whole number of frames).
    pub fn target_ms(&self) -> u32 {
        (self.target_us() / 1000) as u32
    }

    /// Frames the queue spans from the next one to play.
    pub fn depth_frames(&self) -> usize {
        match (self.next, self.queue.keys().next_back()) {
            (Some(n), Some(&last)) if last >= n => (last - n + 1) as usize,
            (None, Some(&last)) => (last - *self.queue.keys().next().unwrap_or(&last) + 1) as usize,
            _ => 0,
        }
    }

    fn target_us(&self) -> u64 {
        let lo = u64::from(self.cfg.min_ms) * 1000;
        let hi = u64::from(self.cfg.max_ms) * 1000;
        let want = ((self.peak_us * 1.1) as u64).clamp(lo, hi);
        (want.div_ceil(FRAME_US) * FRAME_US).min(hi)
    }

    fn restart(&mut self) {
        self.queue.clear();
        self.next = None;
        self.started = false;
        self.hold_run = 0;
        self.excess_since = None;
    }

    /// One datagram arrived at `now_us`.
    pub fn push(&mut self, now_us: u64, header: AudioHeader, payload: Vec<u8>) {
        self.stats.received += 1;
        if header.discontinuity {
            self.stats.discontinuities += 1;
            self.restart();
            self.have_base = false;
        }
        let ext = if self.have_ref {
            self.ref_ext + i64::from(header.seq.wrapping_sub(self.ref_seq) as i32)
        } else {
            self.have_ref = true;
            i64::from(header.seq)
        };
        self.ref_seq = header.seq;
        self.ref_ext = ext;

        let offset = now_us as f64 - ext as f64 * FRAME_US as f64;
        if self.have_base {
            self.base_us = (self.base_us + 1.0).min(offset);
        } else {
            self.have_base = true;
            self.base_us = offset;
        }
        self.peak_us = (offset - self.base_us).max(self.peak_us * 0.995);

        if self.next.is_some_and(|n| ext < n) {
            self.stats.late += 1;
            return;
        }
        if self.queue.contains_key(&ext) {
            self.stats.duplicates += 1;
            return;
        }
        if let (Some(&first), Some(&last)) = (self.queue.keys().next(), self.queue.keys().next_back())
            && (ext.max(last) - ext.min(first) + 1) as usize > self.cfg.max_queue_frames
        {
            self.stats.overflows += 1;
            self.restart();
        }
        self.queue.insert(ext, QueuedPacket { header, payload });
    }

    /// One playout tick at `now_us`.
    pub fn pop(&mut self, now_us: u64) -> Playout {
        if !self.started {
            let (Some(&first), Some(&last)) = (self.queue.keys().next(), self.queue.keys().next_back()) else {
                return Playout::Waiting;
            };
            if (last - first + 1) as u64 * FRAME_US < self.target_us() {
                return Playout::Waiting;
            }
            self.started = true;
            self.next = Some(first);
            self.hold_run = 0;
        }
        let mut n = self.next.unwrap_or(0);

        let target_frames = (self.target_us() / FRAME_US) as i64;
        let span = self.queue.keys().next_back().map_or(0, |&l| l - n + 1);
        if span > target_frames + 1 {
            match self.excess_since {
                None => self.excess_since = Some(now_us),
                Some(t0)
                    if now_us.saturating_sub(t0) >= 1_000_000 && now_us.saturating_sub(self.last_trim) >= 500_000 =>
                {
                    self.queue.remove(&n);
                    n += 1;
                    self.last_trim = now_us;
                    self.stats.trimmed += 1;
                }
                Some(_) => {}
            }
        } else {
            self.excess_since = None;
        }

        // The playout clock (the viewer's device) runs faster than the host's capture clock, or a burst of lateness
        // drained the queue: when the frames queued fall two or more below the target, repeat (conceal) one frame
        // instead of playing the next, at most twice a second. The sequence is held, so the packet plays on the next
        // tick and the depth is one frame higher. It is a stretch, not an underrun: the packet is still here.
        let low = (target_frames - 2).max(1);
        if span >= 1
            && span <= low
            && self.queue.contains_key(&n)
            && now_us.saturating_sub(self.last_stretch) >= STRETCH_GAP_US
        {
            self.last_stretch = now_us;
            self.stats.stretched += 1;
            self.next = Some(n);
            return Playout::Starved;
        }

        if let Some(p) = self.queue.remove(&n) {
            self.next = Some(n + 1);
            self.hold_run = 0;
            return Playout::Packet(p);
        }
        if !self.queue.is_empty() {
            let following = self.queue.get(&(n + 1)).map(|p| p.payload.clone()).filter(|p| !p.is_empty());
            self.next = Some(n + 1);
            self.stats.lost += 1;
            self.hold_run = 0;
            return Playout::Lost { following };
        }
        self.next = Some(n);
        self.stats.underruns += 1;
        self.hold_run += 1;
        if self.hold_run > self.cfg.max_hold_frames {
            self.stats.resyncs += 1;
            self.restart();
            return Playout::Waiting;
        }
        Playout::Starved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(seq: u32) -> AudioHeader {
        AudioHeader { discontinuity: false, muted: false, frame_ms: 10, seq, capture_us: u64::from(seq) * FRAME_US }
    }

    #[test]
    fn debug_of_a_queued_packet_shows_its_size_and_never_the_audio() {
        let shown = format!("{:?}", QueuedPacket { header: hdr(1), payload: vec![233; 40] });
        assert!(!shown.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "audio bytes leaked: {shown}");
        assert!(shown.contains("<redacted 40 bytes>"), "{shown}");
    }

    fn feed(b: &mut JitterBuffer, now: u64, seq: u32) {
        b.push(now, hdr(seq), vec![seq as u8 + 1]);
    }

    fn seq_of(p: &Playout) -> Option<u32> {
        match p {
            Playout::Packet(q) => Some(q.header.seq),
            _ => None,
        }
    }

    /// A clean stream starts after the target is queued and plays in order.
    #[test]
    fn plays_in_order_after_prebuffering() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        let mut played = vec![];
        for tick in 0..30u32 {
            let now = u64::from(tick) * FRAME_US;
            feed(&mut b, now, tick);
            if let Playout::Packet(p) = b.pop(now) {
                played.push(p.header.seq);
            }
        }
        // Initial target is the maximum (4 frames): the first pop that plays is the fourth tick.
        assert_eq!(played[0], 0);
        assert_eq!(played, (0..played.len() as u32).collect::<Vec<_>>());
        assert_eq!(b.stats().underruns, 0);
        assert!(played.len() >= 25, "{}", played.len());
    }

    #[test]
    fn reorder_is_put_back_in_sequence_and_duplicates_and_late_packets_are_counted() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        for (i, s) in [0u32, 2, 1, 3, 3, 4, 5].iter().enumerate() {
            feed(&mut b, i as u64 * 1000, *s);
        }
        assert_eq!(b.stats().duplicates, 1);
        let got: Vec<_> = (0..6).filter_map(|i| seq_of(&b.pop(10_000 + i * FRAME_US))).collect();
        assert_eq!(got, vec![0, 1, 2, 3, 4, 5]);
        feed(&mut b, 200_000, 2);
        assert_eq!(b.stats().late, 1);
    }

    #[test]
    fn a_gap_with_a_later_packet_is_lost_and_carries_the_following_payload() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        for s in [0u32, 1, 3, 4, 5] {
            feed(&mut b, 0, s);
        }
        assert_eq!(seq_of(&b.pop(0)), Some(0));
        assert_eq!(seq_of(&b.pop(0)), Some(1));
        assert_eq!(b.pop(0), Playout::Lost { following: Some(vec![4]) });
        assert_eq!(seq_of(&b.pop(0)), Some(3));
        assert_eq!(b.stats().lost, 1);
        assert_eq!(b.stats().underruns, 0);
    }

    #[test]
    fn empty_queue_holds_the_sequence_so_the_late_packet_still_plays() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        for s in 0..4u32 {
            feed(&mut b, 0, s);
        }
        for s in 0..4 {
            assert_eq!(seq_of(&b.pop(0)), Some(s));
        }
        assert_eq!(b.pop(0), Playout::Starved);
        assert_eq!(b.stats().underruns, 1);
        feed(&mut b, 5_000, 4);
        assert_eq!(seq_of(&b.pop(10_000)), Some(4));
    }

    #[test]
    fn long_starvation_resynchronises() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        for s in 0..4u32 {
            feed(&mut b, 0, s);
        }
        for _ in 0..4 {
            b.pop(0);
        }
        let mut last = Playout::Starved;
        for i in 0..11 {
            last = b.pop(i * FRAME_US);
        }
        assert_eq!(last, Playout::Waiting);
        assert_eq!(b.stats().resyncs, 1);
        // The stream comes back at a later sequence number and starts again after pre-buffering.
        for s in 100..104u32 {
            feed(&mut b, 2_000_000, s);
        }
        assert_eq!(seq_of(&b.pop(2_000_000)), Some(100));
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        let seqs = [u32::MAX - 2, u32::MAX - 1, u32::MAX, 0, 1, 2];
        for s in seqs {
            b.push(0, hdr(s), vec![1]);
        }
        let got: Vec<_> = (0..6).filter_map(|_| seq_of(&b.pop(0))).collect();
        assert_eq!(got, seqs);
    }

    #[test]
    fn a_burst_beyond_the_queue_limit_flushes() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        for s in 0..60u32 {
            feed(&mut b, 0, s);
        }
        assert_eq!(b.stats().overflows, 1);
        assert!(b.depth_frames() < 50);
    }

    #[test]
    fn discontinuity_restarts_prebuffering() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        for s in 0..4u32 {
            feed(&mut b, 0, s);
        }
        assert_eq!(seq_of(&b.pop(0)), Some(0));
        let mut h = hdr(10);
        h.discontinuity = true;
        b.push(0, h, vec![1]);
        assert_eq!(b.pop(0), Playout::Waiting);
        assert_eq!(b.stats().discontinuities, 1);
    }

    #[test]
    fn target_stays_in_bounds_and_follows_the_jitter() {
        let cfg = JitterConfig::default();
        let mut b = JitterBuffer::new(cfg);
        assert_eq!(b.target_ms(), 40);
        // A perfectly regular stream: the peak decays and the target reaches the floor.
        for s in 0..1000u32 {
            b.push(u64::from(s) * FRAME_US, hdr(s), vec![1]);
        }
        assert_eq!(b.target_ms(), 20);
        // One packet 35 ms late raises it at once.
        b.push(1000 * FRAME_US + 35_000, hdr(1000), vec![1]);
        assert_eq!(b.target_ms(), 40);
    }

    #[test]
    fn excess_depth_is_trimmed_one_frame_at_a_time() {
        let mut b = JitterBuffer::new(JitterConfig::default());
        // Ten frames arrive at once, then one per tick: the queue stays about ten frames deep.
        for s in 0..10u32 {
            b.push(0, hdr(s), vec![1]);
        }
        for k in 1..=1000u32 {
            let now = u64::from(k) * FRAME_US;
            b.push(now, hdr(9 + k), vec![1]);
            b.pop(now);
        }
        assert!(b.stats().trimmed >= 4, "{:?}", b.stats());
        assert!(b.depth_frames() <= (b.target_ms() / 10) as usize + 2, "{} {}", b.depth_frames(), b.target_ms());
        assert_eq!(b.stats().underruns, 0);
    }
}
