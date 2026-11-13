//! Rate control from the receiver's report, the path's RTT and packet loss (PROTOCOL.md 5.3 `ReceiverReport`,
//! 4 "Stale video"). The controller is pure: it takes the numbers and the time, and says the new target. The
//! encoder hears it through [`BitrateSink`]; this crate has no codec dependency.
//!
//! How the target moves (F-VIEW-09):
//!
//! * **Ramp.** A session starts below what a local network carries. While no sign of congestion was ever seen, each
//!   calm report raises the target by [`RateConfig::ramp`], so a clean link is at the ceiling within seconds.
//! * **Cut.** Frame loss, packet loss, a growing RTT or a backlog lower it, each by its own factor, and end the ramp.
//! * **Settle.** The rate that met congestion is remembered: calm reports raise the target again only up to
//!   [`RateConfig::settle`] of it, and there it stays. Without this the target would climb back into the same
//!   congestion every few seconds, a sawtooth the picture shows as a quality that keeps changing.
//! * **Try.** After a long calm at the settled rate one small step above it is tried. If it meets congestion the
//!   target returns to the rate that held, and the next try waits twice as long; if it holds, it is the new settled
//!   rate.

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

/// Tuning of the controller. The numbers are GUESS values, tuned in stage 14 against the impairment layer and in
/// M2-3 against a modelled link.
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
    /// Smoothed packet loss fraction that counts as congestion.
    pub packet_loss_limit: f64,
    /// Weight of the newest report in the smoothed packet loss: one report with few packets is noise.
    pub loss_weight: f64,
    /// Multiplier for each calm report while no congestion was ever seen (the ramp of a clean link).
    pub ramp: f64,
    /// Multiplier for each calm report once `hold` has passed since the last decrease.
    pub increase: f64,
    pub hold: Duration,
    /// After congestion at some rate, calm reports raise the target only up to this share of that rate.
    pub settle: f64,
    /// The share of the rate that held to which a failed try returns.
    pub back_off: f64,
    /// Calm time at the settled rate before one step above it is tried. It doubles after a try that met congestion,
    /// up to `try_max`, and returns to this after one that held.
    pub try_after: Duration,
    pub try_max: Duration,
}

/// The start rate of a wired link: a local Ethernet carries it from the first frame.
pub const START_KBPS_ETHERNET: u32 = 40_000;
/// The start rate of a wireless link (and of a link of unknown kind).
pub const START_KBPS_WIFI: u32 = 20_000;

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            min_kbps: 2_000,
            max_kbps: 80_000,
            start_kbps: START_KBPS_WIFI,
            loss_decrease: 0.85,
            backlog_decrease: 0.6,
            rtt_decrease: 0.9,
            rtt_factor: 2.0,
            rtt_slack: Duration::from_millis(20),
            packet_loss_limit: 0.02,
            loss_weight: 0.3,
            ramp: 1.3,
            increase: 1.05,
            hold: Duration::from_secs(2),
            settle: 0.9,
            back_off: 0.95,
            try_after: Duration::from_secs(15),
            try_max: Duration::from_secs(120),
        }
    }
}

impl RateConfig {
    /// The defaults with the start rate of a link: `wired` for Ethernet, otherwise Wi-Fi's.
    pub fn for_link(wired: bool) -> Self {
        Self { start_kbps: if wired { START_KBPS_ETHERNET } else { START_KBPS_WIFI }, ..Self::default() }
    }
}

/// Why the target moved (for logs and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateReason {
    FrameLoss,
    PacketLoss,
    RttGrowth,
    Backlog,
    /// A calm report before any congestion: the fast rise of a clean link.
    Ramp,
    /// A calm report below the settled rate.
    Probe,
    /// One step above a rate that held for a long calm.
    Try,
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
    /// No congestion was seen yet.
    ramping: bool,
    /// Smoothed packet loss.
    loss: Option<f64>,
    /// The rate calm reports climb back to after congestion (`None` until the first).
    settled: Option<u32>,
    /// When the target reached the settled rate.
    settled_at: Option<Instant>,
    /// A step above the rate that held is being tried: that rate.
    trying_from: Option<u32>,
    /// How long the settled rate must stay calm before the next try.
    try_wait: Duration,
}

fn scale(kbps: u32, by: f64) -> u32 {
    // `as` saturates for floats, and the inputs are bounded by `max_kbps`.
    (f64::from(kbps) * by).round() as u32
}

impl RateController {
    pub fn new(cfg: RateConfig) -> Self {
        let target = cfg.start_kbps.clamp(cfg.min_kbps, cfg.max_kbps);
        let cap = cfg.max_kbps;
        let try_wait = cfg.try_after;
        Self {
            cfg,
            target,
            cap,
            base_rtt: None,
            last_decrease: None,
            prev_lost: None,
            ramping: true,
            loss: None,
            settled: None,
            settled_at: None,
            trying_from: None,
            try_wait,
        }
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

    /// Congestion at the current target: the ramp ends, the target drops and the rate to settle at is remembered.
    fn lower(&mut self, now: Instant, by: f64) -> u32 {
        let met = self.target;
        // A second sign right after a cut is the same congestion still clearing (a queue that drains, frames already
        // in flight): the target drops again, but it says nothing new about the rate the link carries.
        let same_episode = self.last_decrease.is_some_and(|t| now.saturating_duration_since(t) < self.cfg.hold)
            && self.settled.is_some();
        self.ramping = false;
        self.last_decrease = Some(now);
        self.settled_at = None;
        if same_episode {
            self.trying_from = None;
            self.target = scale(met, by).max(self.cfg.min_kbps);
            return self.target;
        }
        match self.trying_from.take() {
            // The step above a rate that held met congestion: back to that rate, and wait longer before the next.
            Some(held) => {
                self.target = scale(held, self.cfg.back_off).max(self.cfg.min_kbps);
                self.settled = Some(held);
                self.try_wait = (self.try_wait * 2).min(self.cfg.try_max);
            }
            None => {
                self.target = scale(met, by).max(self.cfg.min_kbps);
                self.settled = Some(scale(met, self.cfg.settle).max(self.cfg.min_kbps));
                self.try_wait = self.cfg.try_after;
            }
        }
        self.target
    }

    /// A calm report: the ramp, the climb back to the settled rate, or a try above it.
    fn raise(&mut self, now: Instant) -> Option<RateReason> {
        let stepped = |t: u32, by: f64| scale(t, by).max(t + 1);
        if self.ramping {
            self.target = stepped(self.target, self.cfg.ramp).min(self.cap);
            return Some(RateReason::Ramp);
        }
        let Some(settled) = self.settled else {
            self.target = stepped(self.target, self.cfg.increase).min(self.cap);
            return Some(RateReason::Probe);
        };
        if self.target < settled {
            self.target = stepped(self.target, self.cfg.increase).min(settled).min(self.cap);
            return Some(RateReason::Probe);
        }
        // At the settled rate: it has to hold for a while before anything above it is tried.
        let since = *self.settled_at.get_or_insert(now);
        if now.saturating_duration_since(since) < self.try_wait || self.target >= self.cap {
            return None;
        }
        if self.trying_from.is_some() {
            // The last try held for a whole wait: it is the settled rate now, and tries are no longer spaced out.
            self.try_wait = self.cfg.try_after;
        }
        self.trying_from = Some(self.target);
        self.target = stepped(self.target, self.cfg.increase).min(self.cap);
        self.settled = Some(self.target);
        self.settled_at = Some(now);
        Some(RateReason::Try)
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
        let w = self.cfg.loss_weight.clamp(0.0, 1.0);
        let loss = self.loss.map_or(packet_loss, |old| w * packet_loss + (1.0 - w) * old);
        self.loss = Some(loss);
        let before = self.target;
        let calm_for = self.last_decrease.map(|t| now.saturating_duration_since(t));
        let held = calm_for.is_some_and(|d| d < self.cfg.hold);

        let reason = if held {
            None
        } else if lost_now > 0 {
            self.lower(now, self.cfg.loss_decrease);
            Some(RateReason::FrameLoss)
        } else if loss > self.cfg.packet_loss_limit {
            self.lower(now, self.cfg.loss_decrease);
            Some(RateReason::PacketLoss)
        } else if rtt > base.mul_f64(self.cfg.rtt_factor) + self.cfg.rtt_slack {
            self.lower(now, self.cfg.rtt_decrease);
            Some(RateReason::RttGrowth)
        } else {
            self.raise(now)
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
    use swapdesk_netsim::{DropReason, Impairment, Link, Verdict};

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

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn frame_loss_lowers_the_target_and_calm_raises_it_back_to_the_settled_rate() {
        let t0 = Instant::now();
        let mut c = RateController::new(RateConfig { start_kbps: 20_000, ..RateConfig::default() });
        assert_eq!(c.on_report(t0, &report(0), ms(2), 0.0).map(|r| r.1), Some(RateReason::Ramp));
        let top = c.target_kbps();
        let (low, why) = c.on_report(t0 + secs(1), &report(3), ms(2), 0.0).unwrap();
        assert_eq!(why, RateReason::FrameLoss);
        assert!(low < top);
        // During the hold nothing moves, even with more loss reported.
        assert_eq!(c.on_report(t0 + ms(1_500), &report(5), ms(2), 0.0), None);
        assert_eq!(c.target_kbps(), low);
        // After the hold, calm reports raise it step by step, up to the settled share of the rate that met the loss.
        let settled = scale(top, 0.9);
        let mut now = t0 + secs(4);
        let mut last = low;
        while c.target_kbps() < settled {
            let (k, why) = c.on_report(now, &report(5), ms(2), 0.0).unwrap();
            assert_eq!(why, RateReason::Probe);
            assert!(k > last && k <= settled, "{last} -> {k}, settled {settled}");
            last = k;
            now += secs(1);
        }
        // There it stays: no report raises it for the whole wait before a try.
        for _ in 0..14 {
            assert_eq!(c.on_report(now, &report(5), ms(2), 0.0), None);
            now += secs(1);
        }
        assert_eq!(c.target_kbps(), settled);
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
        let (k, why) = c.on_report(t0 + secs(3), &report(0), ms(80), 0.0).unwrap();
        assert_eq!((why, k < before), (RateReason::RttGrowth, true));
    }

    #[test]
    fn one_report_with_a_burst_of_packet_loss_among_calm_ones_is_noise() {
        let t0 = Instant::now();
        let mut c = RateController::new(RateConfig::default());
        let mut now = t0;
        // A link that loses 1% of its packets, with one second at 4%: the smoothed loss stays under the limit.
        for loss in [0.01, 0.01, 0.01, 0.04, 0.01, 0.01] {
            let moved = c.on_report(now, &report(0), ms(2), loss);
            assert!(matches!(moved, Some((_, RateReason::Ramp)) | None), "loss {loss}: {moved:?}");
            now += secs(1);
        }
        // Loss that stays above the limit is congestion within three reports.
        let mut reasons = Vec::new();
        for _ in 0..3 {
            reasons.push(c.on_report(now, &report(0), ms(2), 0.06).map(|r| r.1));
            now += secs(1);
        }
        assert!(reasons.contains(&Some(RateReason::PacketLoss)), "{reasons:?}");
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
            now += secs(1);
        }
        assert_eq!(c.target_kbps(), 30_000);
        assert_eq!(c.set_cap(10_000), Some(10_000));
        assert_eq!(c.set_cap(0), None);
        for i in 0..40u32 {
            now += secs(3);
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

    // ---------- F-VIEW-09: the ramp of a clean link, and settling under loss and jitter ----------

    #[test]
    fn a_clean_link_goes_from_20_to_80_mbps_in_under_8_s() {
        let t0 = Instant::now();
        let mut c = RateController::new(RateConfig::default());
        assert_eq!((c.target_kbps(), RateConfig::default().max_kbps), (20_000, 80_000));
        // One report a second, the first a second after the session started.
        let mut reached = None;
        for s in 1..=10u64 {
            let moved = c.on_report(t0 + secs(s), &report(0), ms(2), 0.0);
            if c.target_kbps() == 80_000 && reached.is_none() {
                reached = Some(s);
            }
            if reached.is_none() {
                assert_eq!(moved.map(|m| m.1), Some(RateReason::Ramp), "second {s}");
            }
        }
        let reached = reached.expect("the ceiling is reached");
        assert!(reached < 8, "20 to 80 Mbps took {reached} s");
        // And it stays there.
        for s in 11..=40u64 {
            assert_eq!(c.on_report(t0 + secs(s), &report(0), ms(2), 0.0), None);
        }
        assert_eq!(c.target_kbps(), 80_000);
    }

    #[test]
    fn a_wired_link_starts_higher_and_a_wireless_or_unknown_one_at_20_mbps() {
        assert_eq!(RateController::new(RateConfig::for_link(true)).target_kbps(), START_KBPS_ETHERNET);
        assert_eq!(RateController::new(RateConfig::for_link(false)).target_kbps(), START_KBPS_WIFI);
        assert_eq!(RateConfig::default().start_kbps, START_KBPS_WIFI);
        assert!(START_KBPS_ETHERNET > START_KBPS_WIFI && START_KBPS_ETHERNET < RateConfig::default().max_kbps);
        // From the wired start the ceiling is three reports away.
        let t0 = Instant::now();
        let mut c = RateController::new(RateConfig::for_link(true));
        for s in 1..=3u64 {
            c.on_report(t0 + secs(s), &report(0), ms(2), 0.0);
        }
        assert_eq!(c.target_kbps(), 80_000);
    }

    /// What one second of a 60 fps stream at `kbps` meets on a modelled link, as the signals the sender reads: the
    /// share of packets lost, the smoothed round trip, the frames that did not get through whole, and whether a
    /// frame waited past the backlog limit of the transport (100 ms).
    struct Second {
        loss: f64,
        frames_lost: u32,
        backlog: bool,
    }

    struct Model {
        link: Link,
        /// Smoothed round trip, as a QUIC stack keeps it (1/8 of each new sample).
        srtt_ms: f64,
    }

    impl Model {
        fn new(config: Impairment, seed: u64) -> Self {
            Self { link: Link::new(config, seed), srtt_ms: 0.0 }
        }

        fn second(&mut self, t0: Duration, kbps: u32) -> Second {
            const PACKET: usize = 1_200;
            let frame_bytes = (u64::from(kbps) * 1_000 / 8 / 60) as usize;
            let per_frame = frame_bytes.div_ceil(PACKET).max(1);
            let (mut sent, mut lost, mut frames_lost, mut backlog) = (0u32, 0u32, 0u32, false);
            for f in 0..60u32 {
                let now = t0 + Duration::from_nanos(1_000_000_000 / 60 * u64::from(f));
                let mut whole = true;
                for _ in 0..per_frame {
                    sent += 1;
                    match self.link.decide(now, PACKET) {
                        // A reliable transport sends a lost packet again: the frame arrives, a little later.
                        Verdict::Drop(DropReason::Loss) => lost += 1,
                        // The link's queue is full: the frame is gone and the sender sees a backlog.
                        Verdict::Drop(_) => {
                            lost += 1;
                            whole = false;
                            backlog = true;
                        }
                        Verdict::Deliver { at } => {
                            let one_way = at.saturating_sub(now);
                            backlog |= one_way > Duration::from_millis(100);
                            let sample = 2.0 * one_way.as_secs_f64() * 1_000.0;
                            self.srtt_ms =
                                if self.srtt_ms == 0.0 { sample } else { self.srtt_ms * 0.875 + sample * 0.125 };
                        }
                    }
                }
                frames_lost += u32::from(!whole);
            }
            Second { loss: f64::from(lost) / f64::from(sent), frames_lost, backlog }
        }
    }

    /// Runs the controller against the modelled link for `seconds` and returns the target after each second.
    fn run_on(config: Impairment, seed: u64, seconds: u64) -> Vec<u32> {
        let t0 = Instant::now();
        let mut model = Model::new(config, seed);
        let mut c = RateController::new(RateConfig::default());
        let mut lost_total = 0u32;
        let mut targets = Vec::new();
        for s in 0..seconds {
            let got = model.second(secs(s), c.target_kbps());
            let now = t0 + secs(s + 1);
            if got.backlog {
                c.on_backlog(now);
                // The sender resets what it had queued (PROTOCOL.md 4): the link starts the next second empty.
                model.link = Link::new(model.link.config().clone(), seed ^ (s + 1));
            }
            lost_total += got.frames_lost;
            c.on_report(now, &report(lost_total), Duration::from_secs_f64(model.srtt_ms / 1_000.0), got.loss);
            targets.push(c.target_kbps());
        }
        targets
    }

    #[test]
    fn on_a_modelled_clean_link_the_ceiling_is_reached_in_under_8_s_and_kept() {
        // A local network that carries far more than the ceiling: nothing is lost, nothing queues.
        let targets = run_on(Impairment { bandwidth_bps: Some(1_000_000_000), ..Impairment::default() }, 1, 30);
        let reached = targets.iter().position(|t| *t == 80_000).expect("the ceiling is reached") + 1;
        assert!(reached < 8, "20 to 80 Mbps took {reached} s: {targets:?}");
        assert!(targets[reached..].iter().all(|t| *t == 80_000), "it left the ceiling: {targets:?}");
    }

    /// 1% of the packets lost and up to 20 ms of jitter on a link that carries 30 Mbps: the target finds a rate the
    /// link carries and stays near it, instead of climbing into the congestion again and again.
    #[test]
    fn with_1_percent_loss_and_20_ms_of_jitter_the_target_settles_without_oscillating() {
        let capacity_kbps = 30_000u32;
        let link = Impairment {
            loss: 0.01,
            jitter: Duration::from_millis(20),
            bandwidth_bps: Some(u64::from(capacity_kbps) * 1_000),
            queue_limit: Duration::from_millis(300),
            ..Impairment::default()
        };
        for seed in [1u64, 2, 3, 4, 5] {
            let targets = run_on(link.clone(), seed, 180);
            // The first half minute is for finding the rate; the rest must be settled.
            let settled = &targets[30..];
            let mut sorted = settled.to_vec();
            sorted.sort_unstable();
            let median = sorted[sorted.len() / 2];
            let (low, high) = (sorted[0], sorted[sorted.len() - 1]);
            assert!(
                f64::from(median) >= 0.7 * f64::from(capacity_kbps) && median <= capacity_kbps,
                "seed {seed}: settled at {median} kbps on a {capacity_kbps} kbps link: {targets:?}"
            );
            assert!(
                f64::from(low) >= 0.8 * f64::from(median) && f64::from(high) <= 1.15 * f64::from(median),
                "seed {seed}: between {low} and {high} around {median}: {targets:?}"
            );
            // Cuts are rare: a try that fails now and then, not a sawtooth.
            let cuts = settled.windows(2).filter(|w| w[1] < w[0]).count();
            assert!(cuts <= 6, "seed {seed}: {cuts} cuts in {} s: {targets:?}", settled.len());
        }
    }

    #[test]
    fn a_try_above_the_settled_rate_that_fails_returns_to_it_and_the_next_waits_longer() {
        let t0 = Instant::now();
        let cfg = RateConfig::default();
        let mut c = RateController::new(RateConfig { start_kbps: 40_000, ..cfg.clone() });
        // Congestion at 40 Mbps: the settled rate is 36 Mbps.
        c.on_report(t0, &report(0), ms(2), 0.0);
        let met = c.target_kbps();
        c.on_report(t0 + secs(1), &report(1), ms(2), 0.0);
        let settled = scale(met, cfg.settle);
        let mut now = t0 + secs(4);
        while c.target_kbps() < settled {
            c.on_report(now, &report(1), ms(2), 0.0);
            now += secs(1);
        }
        // A whole wait of calm, then one step up.
        let mut tried = None;
        for _ in 0..40 {
            if let Some((k, RateReason::Try)) = c.on_report(now, &report(1), ms(2), 0.0) {
                tried = Some((k, now));
                break;
            }
            now += secs(1);
        }
        let (above, at) = tried.expect("a try happens");
        assert!(above > settled && above <= scale(settled, cfg.increase) + 1);
        // It meets a backlog: back to just under the rate that held, not to 60% of it.
        now = at + secs(1);
        let back = c.on_backlog(now).expect("the target drops");
        assert_eq!(back, scale(settled, cfg.back_off));
        // It climbs back to the settled rate and the next try comes only after twice the wait.
        let mut next_try = None;
        for _ in 0..80 {
            now += secs(1);
            if let Some((_, RateReason::Try)) = c.on_report(now, &report(1), ms(2), 0.0) {
                next_try = Some(now);
                break;
            }
        }
        let next_try = next_try.expect("a second try happens");
        let waited = next_try.duration_since(at);
        assert!(waited >= cfg.try_after * 2, "the second try came after {waited:?}");
        assert_eq!(c.target_kbps(), scale(settled, cfg.increase).max(settled + 1));
    }
}
