//! What the viewer tells the service about the stream (C2.4 `ViewerKeyframe`, `ViewerReport`), as pure scheduling.
//!
//! - **Keyframes.** A decoder failure asks at once (but never twice within [`MIN_GAP`]); a decoder that is still
//!   waiting for a keyframe asks again after [`RETRY`] (the host may have missed the first request). The service
//!   limits requests itself (at most every 200 ms); this keeps the viewer from sending more than it can use.
//! - **Report**, once per second ([`REPORT_EVERY`]): `frames_presented` and `frames_lost` are cumulative (the rate
//!   controller takes the difference, `swapdesk-video`), the decode time and the present lag are the medians of the
//!   last second, `audio_underruns` is cumulative. The present lag is the time from a frame's arrival in the
//!   viewer to the end of its `present` call: both clocks are this machine's (the host's `capture_us` is on another
//!   clock).

use std::time::{Duration, Instant};

use swapdesk_proto::{KeyframeRequest, ReceiverReport};

pub const MIN_GAP: Duration = Duration::from_millis(250);
pub const RETRY: Duration = Duration::from_secs(1);
pub const REPORT_EVERY: Duration = Duration::from_secs(1);

/// The median of `values` (the upper one for an even count); 0 when empty. Sorts in place.
pub fn p50(values: &mut [u32]) -> u32 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

pub struct Feedback {
    presented: u32,
    lost: u32,
    last_good: u32,
    decode_us: Vec<u32>,
    lag_us: Vec<u32>,
    last_request: Option<Instant>,
    waiting: bool,
    last_report: Instant,
}

impl Feedback {
    pub fn new(now: Instant) -> Self {
        Self {
            presented: 0,
            lost: 0,
            last_good: 0,
            decode_us: Vec::new(),
            lag_us: Vec::new(),
            last_request: None,
            waiting: false,
            last_report: now,
        }
    }

    /// A picture reached the screen.
    pub fn on_presented(&mut self, frame_id: u32, decode_us: u32, lag_us: u32) {
        self.presented = self.presented.wrapping_add(1);
        self.last_good = frame_id;
        self.waiting = false;
        self.decode_us.push(decode_us);
        self.lag_us.push(lag_us);
    }

    /// `n` frames never reached the screen (decoder failures, frames dropped for lack of time).
    pub fn on_lost(&mut self, n: u32) {
        self.lost = self.lost.wrapping_add(n);
    }

    pub fn presented(&self) -> u32 {
        self.presented
    }

    pub fn lost(&self) -> u32 {
        self.lost
    }

    /// The decoder cannot continue without a keyframe. `urgent`: it just failed (ask at once, subject to the minimum
    /// gap); otherwise it has been waiting and the request is repeated after the retry interval.
    pub fn keyframe(&mut self, now: Instant, display: u16, urgent: bool) -> Option<KeyframeRequest> {
        let gap = if urgent || !self.waiting { MIN_GAP } else { RETRY };
        self.waiting = true;
        if self.last_request.is_some_and(|t| now.duration_since(t) < gap) {
            return None;
        }
        self.last_request = Some(now);
        Some(KeyframeRequest { display, last_good_frame: self.last_good })
    }

    /// The once-a-second report, when one is due.
    pub fn report(&mut self, now: Instant, audio_underruns: u32) -> Option<ReceiverReport> {
        if now.duration_since(self.last_report) < REPORT_EVERY {
            return None;
        }
        self.last_report = now;
        Some(ReceiverReport {
            frames_presented: self.presented,
            frames_lost: self.lost,
            decode_us_p50: p50(&mut self.decode_us),
            present_lag_us_p50: p50(&mut self.lag_us),
            audio_underruns,
        })
        .inspect(|_| {
            self.decode_us.clear();
            self.lag_us.clear();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_median_of_nothing_is_zero_and_of_a_list_is_its_middle() {
        assert_eq!(p50(&mut []), 0);
        assert_eq!(p50(&mut [9, 1, 5]), 5);
        assert_eq!(p50(&mut [4, 1, 3, 2]), 3);
    }

    #[test]
    fn the_first_keyframe_request_goes_at_once_and_a_failure_cannot_repeat_within_the_gap() {
        let t0 = Instant::now();
        let mut f = Feedback::new(t0);
        f.on_presented(41, 1, 1);
        let r = f.keyframe(t0, 0, true).expect("first request");
        assert_eq!(r, KeyframeRequest { display: 0, last_good_frame: 41 });
        assert!(f.keyframe(t0 + ms(100), 0, true).is_none());
        assert!(f.keyframe(t0 + ms(260), 0, true).is_some());
    }

    #[test]
    fn a_decoder_that_keeps_waiting_asks_again_each_second_not_each_frame() {
        let t0 = Instant::now();
        let mut f = Feedback::new(t0);
        assert!(f.keyframe(t0, 1, false).is_some());
        for step in 1..=9 {
            assert!(f.keyframe(t0 + ms(step * 100), 1, false).is_none(), "at {} ms", step * 100);
        }
        assert!(f.keyframe(t0 + ms(1000), 1, false).is_some());
    }

    #[test]
    fn a_presented_picture_ends_the_wait() {
        let t0 = Instant::now();
        let mut f = Feedback::new(t0);
        f.keyframe(t0, 0, true);
        f.on_presented(1, 1, 1);
        // Waiting again is a new episode: the short gap applies, not the retry interval.
        assert!(f.keyframe(t0 + ms(300), 0, false).is_some());
    }

    #[test]
    fn the_report_comes_once_a_second_with_cumulative_counts_and_windowed_medians() {
        let t0 = Instant::now();
        let mut f = Feedback::new(t0);
        assert!(f.report(t0 + ms(500), 0).is_none());
        for (i, d) in [(1, 4000), (2, 2000), (3, 3000)] {
            f.on_presented(i, d, d * 2);
        }
        f.on_lost(2);
        let r = f.report(t0 + ms(1000), 7).expect("due");
        assert_eq!(
            r,
            ReceiverReport {
                frames_presented: 3,
                frames_lost: 2,
                decode_us_p50: 3000,
                present_lag_us_p50: 6000,
                audio_underruns: 7
            }
        );
        assert!(f.report(t0 + ms(1500), 7).is_none());
        f.on_presented(4, 1000, 1000);
        let r2 = f.report(t0 + ms(2000), 7).expect("due again");
        assert_eq!(
            (r2.frames_presented, r2.frames_lost, r2.decode_us_p50),
            (4, 2, 1000),
            "counts stay, medians restart"
        );
    }
}
