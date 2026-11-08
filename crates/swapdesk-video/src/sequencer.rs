//! The receiver's ordering rules (PROTOCOL.md 4 "Stale video"), pure and clocked by the caller.
//!
//! Frames arrive on separate streams and may finish out of order. A frame goes to the decoder only when it can be
//! decoded: a keyframe, or the successor of the frame delivered last. A frame that finishes ahead of a missing
//! predecessor waits `hold`; then the predecessor is given up, every P-frame is dropped until a keyframe, and a
//! `KeyframeRequest` goes out (at most one per `request_interval`, repeated at that rate until the keyframe lands).
//! So the sequence handed to the decoder is always decodable.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use swapdesk_proto::{KeyframeRequest, VideoHeader};

/// A frame that finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed<T> {
    pub header: VideoHeader,
    pub body: T,
}

#[derive(Debug, Clone, Copy)]
pub struct SequencerConfig {
    pub hold: Duration,
    pub request_interval: Duration,
    /// Bytes of payload the frames parked behind a missing predecessor may hold, across all displays
    /// (PROTOCOL.md 1: 32 MiB per connection); the oldest go first.
    pub max_parked_bytes: usize,
}

impl Default for SequencerConfig {
    fn default() -> Self {
        Self {
            hold: Duration::from_millis(100),
            request_interval: Duration::from_millis(200),
            max_parked_bytes: 32 * 1024 * 1024,
        }
    }
}

/// What the receiver must do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action<T> {
    /// Hand to the decoder.
    Deliver(Completed<T>),
    /// Ask the host for a keyframe.
    Request(KeyframeRequest),
    /// Stop the unfinished streams of this display that are not keyframes: their frames can no longer be used.
    StopUnfinished { display: u16 },
}

/// Counters, cumulative.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SequencerStats {
    /// Frames handed to the decoder.
    pub delivered: u64,
    /// Frame ids that never reached the decoder (skipped ids when the sequence resumed).
    pub lost: u64,
    /// Complete frames dropped because they could not be decoded (a gap, or older than the last delivered).
    pub dropped: u64,
    pub requests: u64,
    pub gaps: u64,
}

#[derive(Debug)]
struct DisplayState<T> {
    last: Option<u32>,
    need_key: bool,
    /// The id of a keyframe whose stream was accepted and has not finished: P-frames after it may wait for it.
    key_started: Option<u32>,
    pending: Vec<Completed<T>>,
    waiting_since: Option<Instant>,
    last_request: Option<Instant>,
}

impl<T> Default for DisplayState<T> {
    fn default() -> Self {
        Self {
            last: None,
            need_key: false,
            key_started: None,
            pending: Vec::new(),
            waiting_since: None,
            last_request: None,
        }
    }
}

/// Most frames parked while waiting for a predecessor or a keyframe.
const MAX_PENDING: usize = 32;

/// Most displays a host lists (PROTOCOL.md 5.3 `DisplayList`), and so the most this side tracks.
pub const MAX_DISPLAYS: usize = 16;

/// Position of `id` relative to `last`: positive is newer, with wrapping.
pub(crate) fn ahead(id: u32, last: u32) -> i32 {
    id.wrapping_sub(last) as i32
}

#[derive(Debug)]
pub struct Sequencer<T> {
    cfg: SequencerConfig,
    displays: HashMap<u16, DisplayState<T>>,
    /// The displays of the host's last `DisplayList`; `None` until one was heard.
    listed: Option<HashSet<u16>>,
    stats: SequencerStats,
}

impl<T> Sequencer<T> {
    pub fn new(cfg: SequencerConfig) -> Self {
        Self { cfg, displays: HashMap::new(), listed: None, stats: SequencerStats::default() }
    }

    pub fn stats(&self) -> SequencerStats {
        self.stats
    }

    /// Whether a stream whose header just arrived is worth reading. False for a frame older than the last delivered
    /// one, and for a P-frame while a keyframe is awaited and none is on its way before it: its stream is stopped
    /// before the payload is read.
    pub fn accepts(&self, header: &VideoHeader) -> bool {
        if !self.admits(header.display_id) {
            return false;
        }
        if header.keyframe || header.discontinuity {
            return match self.displays.get(&header.display_id).and_then(|d| d.last) {
                Some(last) if !header.discontinuity => ahead(header.frame_id, last) > 0,
                _ => true,
            };
        }
        // Before the first keyframe has finished, a P-frame may only be early: it waits in `pending`.
        match self.displays.get(&header.display_id) {
            None => true,
            Some(d) if d.need_key => d.key_started.is_some_and(|k| ahead(header.frame_id, k) > 0),
            Some(d) => d.last.is_none_or(|last| ahead(header.frame_id, last) > 0),
        }
    }

    /// The stream of `header` was accepted and is being read: a keyframe in progress lets the frames after it wait.
    pub fn started(&mut self, header: &VideoHeader) {
        if (header.keyframe || header.discontinuity) && self.admits(header.display_id) {
            self.displays.entry(header.display_id).or_default().key_started = Some(header.frame_id);
        }
    }

    /// A keyframe stream ended without a frame (reset, stopped, too slow, malformed).
    pub fn aborted(&mut self, header: &VideoHeader) {
        if let Some(d) = self.displays.get_mut(&header.display_id)
            && d.key_started == Some(header.frame_id)
        {
            d.key_started = None;
            // What waited behind that keyframe has nothing left to wait for.
            if d.need_key {
                self.stats.dropped += d.pending.len() as u64;
                d.pending.clear();
            }
        }
    }

    fn deliver_run(&mut self, now: Instant, display: u16, actions: &mut Vec<Action<T>>) {
        let st = self.displays.entry(display).or_default();
        while let Some(last) = st.last {
            let Some(pos) = st.pending.iter().position(|c| c.header.frame_id == last.wrapping_add(1)) else { break };
            let next = st.pending.swap_remove(pos);
            st.last = Some(next.header.frame_id);
            self.stats.delivered += 1;
            actions.push(Action::Deliver(next));
        }
        // Frames still parked wait for a new predecessor: the hold starts over from this progress.
        st.waiting_since = if st.pending.is_empty() { None } else { Some(now) };
    }

    /// Drops the frame of `st` that has waited longest (the lowest id past `last`).
    fn drop_oldest(st: &mut DisplayState<T>) -> bool {
        let Some(oldest) = st
            .pending
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| c.header.frame_id.wrapping_sub(st.last.unwrap_or(0)) as i32)
            .map(|(i, _)| i)
        else {
            return false;
        };
        st.pending.swap_remove(oldest);
        true
    }

    /// Payload bytes of the frames parked across all displays.
    pub fn parked_bytes(&self) -> usize {
        self.displays.values().flat_map(|d| d.pending.iter()).map(|c| c.header.payload_len as usize).sum()
    }

    /// Parks a frame that finished ahead of what it needs, keeping at most `MAX_PENDING` per display and
    /// `max_parked_bytes` in all (the oldest go first).
    fn park(&mut self, display: u16, frame: Completed<T>) {
        let Some(st) = self.displays.get_mut(&display) else { return };
        st.pending.push(frame);
        if st.pending.len() > MAX_PENDING && Self::drop_oldest(st) {
            self.stats.dropped += 1;
        }
        while self.parked_bytes() > self.cfg.max_parked_bytes {
            // The frame just parked is the newest of its display: its oldest go first, then the other displays'.
            let order: Vec<u16> =
                std::iter::once(display).chain(self.displays.keys().copied().filter(|d| *d != display)).collect();
            let dropped = order.into_iter().any(|d| self.displays.get_mut(&d).is_some_and(Self::drop_oldest));
            if !dropped {
                return;
            }
            self.stats.dropped += 1;
        }
    }

    /// Whether frames of `display` are tracked: the displays of the host's last `DisplayList`, or, before one was
    /// heard, the first `MAX_DISPLAYS` seen. Anything else is a peer sending ids it never listed.
    fn admits(&self, display: u16) -> bool {
        match &self.listed {
            Some(list) => list.contains(&display),
            None => self.displays.contains_key(&display) || self.displays.len() < MAX_DISPLAYS,
        }
    }

    /// A frame finished.
    pub fn complete(&mut self, now: Instant, frame: Completed<T>) -> Vec<Action<T>> {
        let mut actions = Vec::new();
        let display = frame.header.display_id;
        let id = frame.header.frame_id;
        let h = frame.header;
        if !self.admits(display) {
            self.stats.dropped += 1;
            return actions;
        }
        let st = self.displays.entry(display).or_default();
        if h.keyframe || h.discontinuity {
            if let Some(last) = st.last
                && !h.discontinuity
                && ahead(id, last) <= 0
            {
                self.stats.dropped += 1;
                return actions;
            }
            if let Some(last) = st.last {
                let skipped = i64::from(ahead(id, last)) - 1;
                if skipped > 0 {
                    self.stats.lost += skipped as u64;
                }
            }
            st.pending.retain(|c| ahead(c.header.frame_id, id) > 0);
            st.last = Some(id);
            st.need_key = false;
            st.waiting_since = None;
            if st.key_started == Some(id) {
                st.key_started = None;
            }
            self.stats.delivered += 1;
            actions.push(Action::Deliver(frame));
            self.deliver_run(now, display, &mut actions);
            return actions;
        }
        if st.need_key {
            // The frames after the recovery keyframe may finish before it does: they wait for it, but only when
            // that keyframe is on its way (otherwise nothing would ever release them).
            if st.key_started.is_some_and(|k| ahead(id, k) > 0) {
                self.park(display, frame);
            } else {
                self.stats.dropped += 1;
            }
            return actions;
        }
        let Some(last) = st.last else {
            // No keyframe yet: this frame may only have finished ahead of it. It waits like any early frame.
            st.waiting_since.get_or_insert(now);
            self.park(display, frame);
            return actions;
        };
        match ahead(id, last) {
            d if d <= 0 => self.stats.dropped += 1,
            1 => {
                st.last = Some(id);
                self.stats.delivered += 1;
                actions.push(Action::Deliver(frame));
                self.deliver_run(now, display, &mut actions);
            }
            _ => {
                st.waiting_since.get_or_insert(now);
                self.park(display, frame);
            }
        }
        actions
    }

    /// Gives up on what is missing: drop what waits, stop unfinished P-frames, ask for a keyframe.
    fn begin_gap(&mut self, now: Instant, display: u16, actions: &mut Vec<Action<T>>) {
        let st = self.displays.entry(display).or_default();
        st.need_key = true;
        st.waiting_since = None;
        let dropped = st.pending.len() as u64;
        // Frames that waited were never delivered: they count as lost when the sequence resumes (skipped ids).
        st.pending.clear();
        self.stats.dropped += dropped;
        self.stats.gaps += 1;
        actions.push(Action::StopUnfinished { display });
        self.request(now, display, actions);
    }

    fn request(&mut self, now: Instant, display: u16, actions: &mut Vec<Action<T>>) {
        let st = self.displays.entry(display).or_default();
        let due = match st.last_request {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= self.cfg.request_interval,
        };
        if due {
            st.last_request = Some(now);
            self.stats.requests += 1;
            actions.push(Action::Request(KeyframeRequest { display, last_good_frame: st.last.unwrap_or(0) }));
        }
    }

    /// The decoder's input could not take a delivered frame (its queue was full) or the decoder failed: the chain is
    /// broken, so wait for a keyframe.
    pub fn decode_failed(&mut self, now: Instant, display: u16) -> Vec<Action<T>> {
        let mut actions = Vec::new();
        if !self.admits(display) {
            return actions;
        }
        self.displays.entry(display).or_default();
        self.begin_gap(now, display, &mut actions);
        actions
    }

    /// Time passes: gives up a predecessor that did not come within `hold`, and repeats a keyframe request that
    /// no keyframe answered.
    pub fn tick(&mut self, now: Instant) -> Vec<Action<T>> {
        let mut actions = Vec::new();
        let displays: Vec<u16> = self.displays.keys().copied().collect();
        for display in displays {
            let Some(st) = self.displays.get(&display) else { continue };
            let overdue =
                !st.need_key && st.waiting_since.is_some_and(|t| now.saturating_duration_since(t) >= self.cfg.hold);
            if overdue {
                self.begin_gap(now, display, &mut actions);
            } else if st.need_key {
                self.request(now, display, &mut actions);
            }
        }
        actions
    }

    /// The displays of the host's last `DisplayList` (at most `MAX_DISPLAYS`): frames of any other display are refused
    /// and what is tracked for a display that left is forgotten.
    pub fn set_displays(&mut self, ids: &[u16]) {
        let list: HashSet<u16> = ids.iter().copied().take(MAX_DISPLAYS).collect();
        self.displays.retain(|d, _| list.contains(d));
        self.listed = Some(list);
    }

    /// When `tick` next has something to do: the end of a hold, or the next repeat of a keyframe request. `None`
    /// while nothing waits, so the caller can sleep until a frame or a decoder failure changes that.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.displays
            .values()
            .filter_map(|st| {
                if st.need_key {
                    Some(st.last_request.map_or(now, |t| t + self.cfg.request_interval))
                } else {
                    st.waiting_since.map(|t| t + self.cfg.hold)
                }
            })
            .min()
    }

    /// Frames of `display` still waiting for a predecessor (for tests).
    pub fn waiting(&self, display: u16) -> usize {
        self.displays.get(&display).map_or(0, |d| d.pending.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_proto::Codec;

    fn hdr(id: u32, key: bool) -> VideoHeader {
        VideoHeader {
            keyframe: key,
            config: key,
            discontinuity: false,
            display_id: 0,
            frame_id: id,
            capture_us: 0,
            encode_us: 0,
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            payload_len: 0,
        }
    }

    fn frame(id: u32, key: bool) -> Completed<()> {
        Completed { header: hdr(id, key), body: () }
    }

    fn ids(actions: &[Action<()>]) -> Vec<u32> {
        actions.iter().filter_map(|a| if let Action::Deliver(c) = a { Some(c.header.frame_id) } else { None }).collect()
    }

    fn requests(actions: &[Action<()>]) -> Vec<KeyframeRequest> {
        actions.iter().filter_map(|a| if let Action::Request(r) = a { Some(r.clone()) } else { None }).collect()
    }

    #[test]
    fn frames_in_order_are_delivered_at_once() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        assert_eq!(ids(&s.complete(t, frame(1, true))), vec![1]);
        assert_eq!(ids(&s.complete(t, frame(2, false))), vec![2]);
        assert_eq!(ids(&s.complete(t, frame(3, false))), vec![3]);
        assert_eq!(s.stats().delivered, 3);
        assert_eq!(s.stats().lost, 0);
    }

    #[test]
    fn a_frame_that_finishes_early_waits_for_its_predecessor() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        assert!(s.complete(t, frame(3, false)).is_empty());
        assert!(s.complete(t, frame(4, false)).is_empty());
        assert_eq!(s.waiting(0), 2);
        // The late one arrives: all three go in order.
        assert_eq!(ids(&s.complete(t + Duration::from_millis(30), frame(2, false))), vec![2, 3, 4]);
        assert_eq!(s.waiting(0), 0);
        assert!(s.tick(t + Duration::from_secs(5)).is_empty());
    }

    #[test]
    fn a_predecessor_that_never_comes_ends_in_a_keyframe_request() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        s.complete(t, frame(2, false));
        assert!(s.complete(t, frame(4, false)).is_empty());
        assert!(s.tick(t + Duration::from_millis(99)).is_empty());
        let acts = s.tick(t + Duration::from_millis(100));
        assert_eq!(requests(&acts), vec![KeyframeRequest { display: 0, last_good_frame: 2 }]);
        assert!(acts.contains(&Action::StopUnfinished { display: 0 }));
        assert!(!s.accepts(&hdr(5, false)), "P-frames are refused while a keyframe is awaited");
        assert!(s.complete(t + Duration::from_millis(110), frame(5, false)).is_empty());
        // The keyframe resumes the sequence; ids 3 to 5 never reached the decoder.
        assert_eq!(ids(&s.complete(t + Duration::from_millis(150), frame(6, true))), vec![6]);
        assert_eq!(s.stats().lost, 3);
        assert_eq!(ids(&s.complete(t + Duration::from_millis(160), frame(7, false))), vec![7]);
        assert!(s.accepts(&hdr(8, false)));
    }

    #[test]
    fn p_frames_that_finish_before_the_recovery_keyframe_wait_for_it() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        s.complete(t, frame(2, false));
        s.complete(t, frame(4, false));
        s.tick(t + Duration::from_millis(100));
        // The keyframe (id 6) is on its way: the P-frames after it are worth reading, those before are not.
        assert!(!s.accepts(&hdr(7, false)));
        s.started(&hdr(6, true));
        assert!(s.accepts(&hdr(7, false)));
        assert!(!s.accepts(&hdr(5, false)));
        assert!(s.complete(t + Duration::from_millis(110), frame(7, false)).is_empty());
        assert!(s.complete(t + Duration::from_millis(111), frame(8, false)).is_empty());
        assert_eq!(ids(&s.complete(t + Duration::from_millis(120), frame(6, true))), vec![6, 7, 8]);
        // A keyframe stream that dies stops the waiting.
        s.complete(t + Duration::from_millis(130), frame(10, false));
        s.tick(t + Duration::from_millis(240));
        s.started(&hdr(12, true));
        s.aborted(&hdr(12, true));
        assert!(!s.accepts(&hdr(13, false)));
    }

    #[test]
    fn what_waits_behind_a_keyframe_is_capped() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        s.decode_failed(t, 0);
        s.started(&hdr(100, true));
        for id in 101..=200 {
            s.complete(t, frame(id, false));
        }
        assert_eq!(s.waiting(0), 32);
        // The oldest were dropped, so the run from the keyframe is broken after the cap: it delivers the keyframe
        // and nothing more, instead of growing without limit.
        assert_eq!(ids(&s.complete(t, frame(100, true))), vec![100]);
    }

    #[test]
    fn the_request_repeats_every_interval_and_not_faster() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        s.complete(t, frame(3, false));
        let mut sent = requests(&s.tick(t + Duration::from_millis(100))).len();
        for ms in (101..=1_000).step_by(10) {
            sent += requests(&s.tick(t + Duration::from_millis(ms))).len();
        }
        // First at 100 ms, then every 200 ms: 100, 300, 500, 700, 900.
        assert_eq!(sent, 5);
    }

    #[test]
    fn stale_frames_are_refused_and_dropped() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(10, true));
        s.complete(t, frame(11, false));
        assert!(!s.accepts(&hdr(11, false)));
        assert!(!s.accepts(&hdr(9, false)));
        assert!(!s.accepts(&hdr(9, true)));
        assert!(s.complete(t, frame(9, false)).is_empty());
        assert!(s.complete(t, frame(9, true)).is_empty());
        assert_eq!(s.stats().dropped, 2);
        assert_eq!(s.stats().delivered, 2);
    }

    #[test]
    fn ids_wrap_around() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(u32::MAX - 1, true));
        assert_eq!(ids(&s.complete(t, frame(u32::MAX, false))), vec![u32::MAX]);
        assert_eq!(ids(&s.complete(t, frame(0, false))), vec![0]);
        assert!(s.complete(t, frame(2, false)).is_empty());
        assert_eq!(ids(&s.complete(t, frame(1, false))), vec![1, 2]);
        assert_eq!(s.stats().lost, 0);
    }

    #[test]
    fn a_discontinuity_restarts_the_sequence_from_any_id() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(500, true));
        let mut h = hdr(3, true);
        h.discontinuity = true;
        assert!(s.accepts(&h));
        assert_eq!(ids(&s.complete(t, Completed { header: h, body: () })), vec![3]);
        assert_eq!(ids(&s.complete(t, frame(4, false))), vec![4]);
    }

    #[test]
    fn a_p_frame_that_finishes_before_the_first_keyframe_waits_for_it() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        assert!(s.accepts(&hdr(2, false)), "it may only be early");
        assert!(s.complete(t, frame(2, false)).is_empty());
        assert!(s.complete(t, frame(3, false)).is_empty());
        assert_eq!(ids(&s.complete(t + Duration::from_millis(20), frame(1, true))), vec![1, 2, 3]);
        assert!(s.tick(t + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn without_any_keyframe_the_viewer_asks_for_one_after_the_hold() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        assert!(s.complete(t, frame(2, false)).is_empty());
        let acts = s.tick(t + Duration::from_millis(100));
        assert_eq!(requests(&acts), vec![KeyframeRequest { display: 0, last_good_frame: 0 }]);
        assert_eq!(ids(&s.complete(t + Duration::from_millis(150), frame(9, true))), vec![9]);
    }

    #[test]
    fn a_decoder_failure_asks_for_a_keyframe() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        s.complete(t, frame(2, false));
        let acts = s.decode_failed(t, 0);
        assert_eq!(requests(&acts), vec![KeyframeRequest { display: 0, last_good_frame: 2 }]);
        assert!(s.complete(t, frame(3, false)).is_empty());
        assert_eq!(ids(&s.complete(t, frame(4, true))), vec![4]);
    }

    #[test]
    fn displays_are_independent() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        let mut other = hdr(1, true);
        other.display_id = 1;
        s.complete(t, frame(1, true));
        s.complete(t, Completed { header: other, body: () });
        s.complete(t, frame(3, false));
        let acts = s.tick(t + Duration::from_millis(100));
        assert_eq!(requests(&acts).iter().map(|r| r.display).collect::<Vec<_>>(), vec![0]);
        let mut next = hdr(2, false);
        next.display_id = 1;
        assert!(s.accepts(&next));
    }
    // ---------- stage 37 (review stage14-session-video.md): findings 3, 17 and 20 ----------

    fn hdr_big(id: u32, key: bool, display: u16, len: u32) -> VideoHeader {
        let mut h = hdr(id, key);
        h.display_id = display;
        h.payload_len = len;
        h
    }

    const MIB: u32 = 1024 * 1024;

    /// Finding 17: `ahead(id, last) - 1` overflowed in a debug build for a discontinuity whose id is half the id space away.
    #[test]
    fn f17_a_discontinuity_half_the_id_space_away_does_not_overflow() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        let mut h = hdr(1u32.wrapping_add(0x8000_0000), true);
        h.discontinuity = true;
        assert_eq!(ids(&s.complete(t, Completed { header: h, body: () })), vec![h.frame_id]);
    }

    /// Finding 3: frames parked while a predecessor is missing count against a byte budget, not only 32 per display.
    #[test]
    fn f3_parked_frames_stay_inside_a_byte_budget() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, Completed { header: hdr_big(1, true, 0, 1000), body: () });
        // Frame 2 never comes; frames 3 to 40 of 8 MiB each finish and are parked.
        for id in 3..40 {
            s.complete(t, Completed { header: hdr_big(id, false, 0, 8 * MIB), body: () });
        }
        let parked = s.waiting(0) as u64 * u64::from(8 * MIB);
        assert!(parked <= 32 * 1024 * 1024, "{} frames of 8 MiB are parked ({} MiB)", s.waiting(0), parked >> 20);
    }

    /// Finding 3: a P-frame that finishes while a keyframe is awaited and none is on its way is not parked.
    #[test]
    fn f3_nothing_is_parked_for_a_keyframe_that_is_not_on_its_way() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.complete(t, frame(1, true));
        s.decode_failed(t, 0);
        // It was accepted before the failure and finishes now.
        assert!(s.complete(t, frame(5, false)).is_empty());
        assert_eq!(s.waiting(0), 0, "no keyframe was started, so nothing can ever release this frame");
        // What waited for a keyframe whose stream died is cleared with it.
        s.started(&hdr(10, true));
        assert!(s.complete(t, frame(11, false)).is_empty());
        assert_eq!(s.waiting(0), 1);
        s.aborted(&hdr(10, true));
        assert_eq!(s.waiting(0), 0, "the keyframe is gone: what waited for it goes too");
    }

    /// Finding 3: only the displays of the last `DisplayList` (at most 16) are tracked, whatever id a peer sends.
    #[test]
    fn f3_only_the_listed_displays_are_accepted_and_never_more_than_16() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        s.set_displays(&[0, 1]);
        assert!(s.accepts(&hdr_big(1, true, 1, 0)));
        assert!(!s.accepts(&hdr_big(1, true, 40_000, 0)), "a display the host never listed");
        assert!(!s.accepts(&hdr_big(1, false, 40_000, 0)));
        assert!(s.complete(t, Completed { header: hdr_big(1, true, 40_000, 0), body: () }).is_empty());
        // Without a list the first 16 displays seen are accepted and no more.
        let mut s = Sequencer::new(SequencerConfig::default());
        for d in 0..16u16 {
            assert_eq!(ids(&s.complete(t, Completed { header: hdr_big(1, true, d, 0), body: () })), vec![1]);
        }
        assert!(!s.accepts(&hdr_big(1, true, 16, 0)), "a 17th display");
        assert!(s.complete(t, Completed { header: hdr_big(1, true, 16, 0), body: () }).is_empty());
        assert_eq!(s.stats().delivered, 16);
    }

    /// Finding 20c: the sequencer says when it next needs a tick, so the receiver need not wake every 10 ms.
    #[test]
    fn f20c_the_next_deadline_is_known_only_when_something_waits() {
        let t = Instant::now();
        let mut s = Sequencer::new(SequencerConfig::default());
        assert_eq!(s.next_deadline(t), None);
        s.complete(t, frame(1, true));
        assert_eq!(s.next_deadline(t), None, "nothing waits");
        s.complete(t, frame(3, false));
        assert_eq!(
            s.next_deadline(t),
            Some(t + Duration::from_millis(100)),
            "the hold ends 100 ms after frame 3 parked"
        );
        s.tick(t + Duration::from_millis(100));
        assert_eq!(
            s.next_deadline(t + Duration::from_millis(100)),
            Some(t + Duration::from_millis(300)),
            "the request repeats every 200 ms"
        );
    }
}
