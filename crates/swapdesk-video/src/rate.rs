//! Rate control from the receiver's report, the path's RTT and packet loss (PROTOCOL.md 5.3 `ReceiverReport`,
//! 4 "Stale video"). The controller is pure: it takes the numbers and the time, and says the new target. The
//! encoder hears it through [`BitrateSink`]; this crate has no codec dependency.

use std::time::{Duration, Instant};

use swapdesk_proto::ReceiverReport;

/// Where the target bitrate goes: the encoder (or a test). Called from the transport's tasks, so it must not block.
pub trait BitrateSink: Send + Sync {
    fn set_target_kbps(&self, kbps: u32);
}

impl<F: Fn(u32) + Send + Sync> BitrateSink for F {
    fn set_target_kbps(&self, kbps: u32) {
        self(kbps);
    }
}

/// A sink that drops the signal.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoBitrateSink;

impl BitrateSink for NoBitrateSink {
    fn set_target_kbps(&self, _kbps: u32) {}
}

/// Tuning of the controller. The numbers are GUESS values, tuned in stage 14 against the impairment layer.
#[derive(Debug, Clone)]
pub struct RateConfig {
    pub min_kbps: u32,
    /// Ceiling: the viewer's `VideoSettings.max_kbps` lowers it, never raises it above this.
    pub max_kbps: u32,
    pub start_kbps: u32,
    /// Multiplier after frame loss or heavy packet loss.
    pub loss_decrease: f64,
    /// Multiplier after the sender had to reset a backlog (the link cannot carry the rate).
    pub backlog_decrease: f64,
    /// Multiplier when the RTT grows past `rtt_factor * base + rtt_slack` (a queue is building).
    pub rtt_decrease: f64,
    pub rtt_factor: f64,
    pub rtt_slack: Duration,
    /// Packet loss fraction over one report interval that counts as congestion.
    pub packet_loss_limit: f64,
    /// Multiplier for each calm report once `hold` has passed since the last decrease.
    pub increase: f64,
    pub hold: Duration,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            min_kbps: 2_000,
            max_kbps: 80_000,
            start_kbps: 20_000,
            loss_decrease: 0.85,
            backlog_decrease: 0.6,
            rtt_decrease: 0.9,
            rtt_factor: 2.0,
            rtt_slack: Duration::from_millis(20),
            packet_loss_limit: 0.02,
            increase: 1.05,
            hold: Duration::from_secs(2),
        }
    }
}

/// Why the target moved (for logs and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateReason {
    FrameLoss,
    PacketLoss,
    RttGrowth,
    Backlog,
    Probe,
    Cap,
}

#[derive(Debug)]
pub struct RateController {
    cfg: RateConfig,
    target: u32,
    cap: u32,
    base_rtt: Option<Duration>,
    last_decrease: Option<Instant>,
    prev_lost: Option<u32>,
}

fn scale(kbps: u32, by: f64) -> u32 {
    // `as` saturates for floats, and the inputs are bounded by `max_kbps`.
    (f64::from(kbps) * by).round() as u32
}

impl RateController {
    pub fn new(cfg: RateConfig) -> Self {
        let target = cfg.start_kbps.clamp(cfg.min_kbps, cfg.max_kbps);
        let cap = cfg.max_kbps;
        Self { cfg, target, cap, base_rtt: None, last_decrease: None, prev_lost: None }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target
    }

    /// The viewer's own ceiling (`VideoSettings.max_kbps`, 0 = no ceiling of its own). Returns the new target when
    /// the old one was above it.
    pub fn set_cap(&mut self, max_kbps: u32) -> Option<u32> {
        self.cap = if max_kbps == 0 { self.cfg.max_kbps } else { max_kbps.clamp(self.cfg.min_kbps, self.cfg.max_kbps) };
        (self.target > self.cap).then(|| {
            self.target = self.cap;
            self.target
        })
    }

    fn lower(&mut self, now: Instant, by: f64) -> u32 {
        self.target = scale(self.target, by).max(self.cfg.min_kbps);
        self.last_decrease = Some(now);
        self.target
    }

    /// The sender reset its backlog (PROTOCOL.md 4): the link cannot carry the current rate.
    pub fn on_backlog(&mut self, now: Instant) -> Option<u32> {
        let before = self.target;
        let new = self.lower(now, self.cfg.backlog_decrease);
        (new != before).then_some(new)
    }

    /// One `ReceiverReport` (once per second). `rtt` is the path's smoothed RTT, `packet_loss` the fraction of
    /// packets the path lost since the previous report. `frames_lost` is cumulative: the controller takes the
    /// difference. Returns the new target and why, when it changed.
    pub fn on_report(
        &mut self,
        now: Instant,
        report: &ReceiverReport,
        rtt: Duration,
        packet_loss: f64,
    ) -> Option<(u32, RateReason)> {
        let lost_now = report.frames_lost.wrapping_sub(self.prev_lost.unwrap_or(report.frames_lost));
        self.prev_lost = Some(report.frames_lost);
        self.base_rtt = Some(self.base_rtt.map_or(rtt, |b| b.min(rtt)));
        let base = self.base_rtt.unwrap_or(rtt);
        let before = self.target;
        let calm_for = self.last_decrease.map(|t| now.saturating_duration_since(t));
        let held = calm_for.is_some_and(|d| d < self.cfg.hold);

        let reason = if lost_now > 0 && !held {
            self.lower(now, self.cfg.loss_decrease);
            Some(RateReason::FrameLoss)
        } else if packet_loss > self.cfg.packet_loss_limit && !held {
            self.lower(now, self.cfg.loss_decrease);
            Some(RateReason::PacketLoss)
        } else if rtt > base.mul_f64(self.cfg.rtt_factor) + self.cfg.rtt_slack && !held {
            self.lower(now, self.cfg.rtt_decrease);
            Some(RateReason::RttGrowth)
        } else if lost_now == 0 && packet_loss <= self.cfg.packet_loss_limit && !held {
            self.target = scale(self.target, self.cfg.increase).max(self.target + 1).min(self.cap);
            Some(RateReason::Probe)
        } else {
            None
        };
        if self.target > self.cap {
            self.target = self.cap;
        }
        if self.target == before {
            None
        } else {
            reason.map(|r| (self.target, r)).or(Some((self.target, RateReason::Cap)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(lost: u32) -> ReceiverReport {
        ReceiverReport {
            frames_presented: 60,
            frames_lost: lost,
            decode_us_p50: 2_000,
            present_lag_us_p50: 1_000,
            audio_underruns: 0,
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn frame_loss_lowers_the_target_and_calm_raises_it_again() {
        let t0 = Instant::now();
        let mut c = RateController::new(RateConfig { start_kbps: 20_000, ..RateConfig::default() });
        assert_eq!(c.on_report(t0, &report(0), ms(2), 0.0).map(|r| r.1), Some(RateReason::Probe));
        let top = c.target_kbps();
        let (low, why) = c.on_report(t0 + Duration::from_secs(1), &report(3), ms(2), 0.0).unwrap();
        assert_eq!(why, RateReason::FrameLoss);
        assert!(low < top);
        // During the hold nothing moves, even with more loss reported.
        assert_eq!(c.on_report(t0 + Duration::from_millis(1_500), &report(5), ms(2), 0.0), None);
        assert_eq!(c.target_kbps(), low);
        // After the hold, calm reports raise it step by step.
        let mut now = t0 + Duration::from_secs(4);
        let mut last = low;
        for _ in 0..5 {
            let (k, why) = c.on_report(now, &report(5), ms(2), 0.0).unwrap();
            assert_eq!(why, RateReason::Probe);
            assert!(k > last);
            last = k;
            now += Duration::from_secs(1);
        }
    }

    #[test]
    fn packet_loss_and_rtt_growth_lower_it() {
        let t0 = Instant::now();
        let mut c = RateController::new(RateConfig::default());
        let start = c.target_kbps();
        let (k, why) = c.on_report(t0, &report(0), ms(2), 0.05).unwrap();
        assert_eq!((why, k < start), (RateReason::PacketLoss, true));
        let mut c = RateController::new(RateConfig::default());
        c.on_report(t0, &report(0), ms(2), 0.0);
        let before = c.target_kbps();
        let (k, why) = c.on_report(t0 + Duration::from_secs(3), &report(0), ms(80), 0.0).unwrap();
        assert_eq!((why, k < before), (RateReason::RttGrowth, true));
    }

    #[test]
    fn the_target_stays_between_the_floor_and_the_cap() {
        let t0 = Instant::now();
        let mut c = RateController::new(RateConfig {
            min_kbps: 2_000,
            max_kbps: 30_000,
            start_kbps: 29_000,
            ..RateConfig::default()
        });
        let mut now = t0;
        for _ in 0..20 {
            c.on_report(now, &report(0), ms(2), 0.0);
            now += Duration::from_secs(1);
        }
        assert_eq!(c.target_kbps(), 30_000);
        assert_eq!(c.set_cap(10_000), Some(10_000));
        assert_eq!(c.set_cap(0), None);
        for i in 0..40u32 {
            now += Duration::from_secs(3);
            c.on_report(now, &report(i + 1), ms(2), 0.0);
        }
        assert_eq!(c.target_kbps(), 2_000, "floor");
        assert_eq!(c.on_backlog(now), None, "already at the floor");
    }

    #[test]
    fn a_backlog_reset_cuts_hard() {
        let mut c = RateController::new(RateConfig { start_kbps: 40_000, ..RateConfig::default() });
        assert_eq!(c.on_backlog(Instant::now()), Some(24_000));
    }

    #[test]
    fn the_sink_takes_a_closure() {
        let seen = std::sync::Mutex::new(Vec::new());
        let sink = |k: u32| seen.lock().unwrap().push(k);
        BitrateSink::set_target_kbps(&sink, 123);
        assert_eq!(*seen.lock().unwrap(), vec![123]);
    }
}
