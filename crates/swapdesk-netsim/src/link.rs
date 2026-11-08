//! The impairment model as pure logic: what happens to one packet, given the time and the link's state.
//! Time is a `Duration` on any timeline the caller keeps (the socket uses the time since it was made), so the
//! statistics are tested over 100 000 packets without a clock or a socket.

use std::time::Duration;

/// SplitMix64: small, fast and good enough for loss rolls. Not for cryptography.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The impairments of one direction of a link. The default changes nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Impairment {
    /// Probability in [0, 1] that a packet is lost.
    pub loss: f64,
    /// Fixed one-way delay.
    pub delay: Duration,
    /// Extra random delay, uniform in [0, jitter]. Jitter alone never reorders packets: a packet is not delivered
    /// before the one sent ahead of it.
    pub jitter: Duration,
    /// Probability in [0, 1] that a packet is held back by `reorder_extra` so later packets overtake it.
    pub reorder: f64,
    pub reorder_extra: Duration,
    /// Link rate in bits per second; `None` is unlimited.
    pub bandwidth_bps: Option<u64>,
    /// With a bandwidth cap, a packet that would wait longer than this to be sent is dropped (a full queue).
    pub queue_limit: Duration,
}

impl Default for Impairment {
    fn default() -> Self {
        Self {
            loss: 0.0,
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            reorder: 0.0,
            reorder_extra: Duration::from_millis(20),
            bandwidth_bps: None,
            queue_limit: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    LinkDown,
    Loss,
    QueueFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Drop(DropReason),
    /// Deliver at this time on the caller's timeline.
    Deliver {
        at: Duration,
    },
}

/// One direction of an impaired link.
#[derive(Debug, Clone)]
pub struct Link {
    config: Impairment,
    rng: Rng,
    up: bool,
    /// When the serializer is free again (bandwidth cap).
    link_free: Duration,
    /// Delivery time of the last packet that was not held back.
    last_deliver: Duration,
}

impl Link {
    pub fn new(config: Impairment, seed: u64) -> Self {
        Self { config, rng: Rng::new(seed), up: true, link_free: Duration::ZERO, last_deliver: Duration::ZERO }
    }

    pub fn is_up(&self) -> bool {
        self.up
    }

    pub fn set_up(&mut self, up: bool) {
        self.up = up;
    }

    pub fn config(&self) -> &Impairment {
        &self.config
    }

    pub fn set_config(&mut self, config: Impairment) {
        self.config = config;
    }

    /// Decides what happens to a packet of `len` bytes sent at `now`. The generator is drawn from three times per
    /// packet whatever the outcome, so one seed gives one fate sequence for one packet sequence.
    pub fn decide(&mut self, now: Duration, len: usize) -> Verdict {
        let loss_roll = self.rng.next_f64();
        let jitter_roll = self.rng.next_f64();
        let reorder_roll = self.rng.next_f64();
        if !self.up {
            return Verdict::Drop(DropReason::LinkDown);
        }
        if loss_roll < self.config.loss {
            return Verdict::Drop(DropReason::Loss);
        }
        let mut depart = now;
        if let Some(bps) = self.config.bandwidth_bps.filter(|b| *b > 0) {
            let start = now.max(self.link_free);
            if start - now > self.config.queue_limit {
                return Verdict::Drop(DropReason::QueueFull);
            }
            let tx = Duration::from_secs_f64(len as f64 * 8.0 / bps as f64);
            self.link_free = start + tx;
            depart = self.link_free;
        }
        let mut at = depart + self.config.delay + self.config.jitter.mul_f64(jitter_roll);
        if reorder_roll < self.config.reorder {
            at += self.config.reorder_extra;
        } else {
            at = at.max(self.last_deliver);
            self.last_deliver = at;
        }
        Verdict::Deliver { at }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn fates(link: &mut Link, n: usize) -> Vec<Verdict> {
        (0..n).map(|i| link.decide(MS * i as u32, 1200)).collect()
    }

    #[test]
    fn one_percent_loss_over_100_000_packets_is_within_tolerance() {
        let mut link = Link::new(Impairment { loss: 0.01, ..Impairment::default() }, 0x5EED);
        let dropped = (0..100_000u32)
            .filter(|i| {
                matches!(link.decide(Duration::from_micros(u64::from(*i)), 1200), Verdict::Drop(DropReason::Loss))
            })
            .count();
        // Binomial(100 000, 0.01): mean 1000, sigma 31.5; the window is +-5 sigma.
        assert!((843..=1157).contains(&dropped), "dropped {dropped}");
    }

    #[test]
    fn seed_decides_the_fates() {
        let cfg = Impairment { loss: 0.3, jitter: MS * 5, reorder: 0.2, ..Impairment::default() };
        let a = fates(&mut Link::new(cfg.clone(), 7), 500);
        let b = fates(&mut Link::new(cfg.clone(), 7), 500);
        let c = fates(&mut Link::new(cfg, 8), 500);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn link_down_drops_everything_and_up_restores() {
        let mut link = Link::new(Impairment::default(), 1);
        assert!(matches!(link.decide(MS, 100), Verdict::Deliver { .. }));
        link.set_up(false);
        for v in fates(&mut link, 10_000) {
            assert_eq!(v, Verdict::Drop(DropReason::LinkDown));
        }
        link.set_up(true);
        assert!(fates(&mut link, 100).iter().all(|v| matches!(v, Verdict::Deliver { .. })));
    }

    #[test]
    fn delay_and_jitter_stay_in_bounds_and_keep_order() {
        let cfg = Impairment { delay: MS * 30, jitter: MS * 10, ..Impairment::default() };
        let mut link = Link::new(cfg, 3);
        let mut last = Duration::ZERO;
        for i in 0..5_000u32 {
            let now = Duration::from_micros(u64::from(i) * 100);
            let Verdict::Deliver { at } = link.decide(now, 100) else { panic!("dropped") };
            assert!(at >= now + MS * 30 && at <= now + MS * 40 + MS * 10, "{at:?}");
            assert!(at >= last);
            last = at;
        }
    }

    #[test]
    fn reorder_holds_packets_back_so_later_ones_overtake() {
        let cfg = Impairment { reorder: 0.1, reorder_extra: MS * 20, ..Impairment::default() };
        let mut link = Link::new(cfg, 11);
        let n = 20_000u32;
        let at: Vec<Duration> = (0..n)
            .map(|i| match link.decide(Duration::from_micros(u64::from(i) * 100), 100) {
                Verdict::Deliver { at } => at,
                other => panic!("{other:?}"),
            })
            .collect();
        let overtaken =
            (1..at.len()).filter(|&i| at[i] < at[..i].iter().rev().take(300).copied().max().unwrap()).count();
        // About 10% held back; each is overtaken by the packets sent in the next 20 ms (200 of them).
        assert!(overtaken > 5_000, "{overtaken}");
        let held = (0..at.len()).filter(|&i| at[i] == Duration::from_micros(i as u64 * 100) + MS * 20).count();
        assert!((1_700..=2_300).contains(&held), "{held}");
    }

    #[test]
    fn bandwidth_cap_spaces_packets_and_a_full_queue_drops() {
        // 1 Mbit/s, 1250-byte packets: 10 ms each.
        let cfg = Impairment { bandwidth_bps: Some(1_000_000), queue_limit: MS * 35, ..Impairment::default() };
        let mut link = Link::new(cfg, 5);
        let v: Vec<Verdict> = (0..8).map(|_| link.decide(Duration::ZERO, 1250)).collect();
        for (k, verdict) in v.iter().take(4).enumerate() {
            assert_eq!(*verdict, Verdict::Deliver { at: MS * 10 * (k as u32 + 1) });
        }
        // The fifth would wait 40 ms for the serializer, over the 35 ms limit.
        for verdict in &v[4..] {
            assert_eq!(*verdict, Verdict::Drop(DropReason::QueueFull));
        }
        // After the queue drains the link accepts again.
        assert_eq!(link.decide(MS * 100, 1250), Verdict::Deliver { at: MS * 110 });
    }

    #[test]
    fn splitmix_is_uniform_enough() {
        let mut r = Rng::new(42);
        let mean = (0..100_000).map(|_| r.next_f64()).sum::<f64>() / 100_000.0;
        assert!((mean - 0.5).abs() < 0.01, "{mean}");
    }
}
