//! The liveness probe (PROTOCOL.md 5.3 `Ping`/`Pong`, F-FIND-01): a peer that says nothing is reported offline
//! within the limit, even when the transport has not noticed (a sleeping or crashed machine does not close its
//! connection). mDNS detects a clean shutdown in about a second; this covers the rest. Pure and clocked by the caller.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct LivenessConfig {
    /// A `Ping` goes out this often (once per second, PROTOCOL.md 5.3).
    pub interval: Duration,
    /// Nothing heard for this long: the peer is reported offline. Under F-FIND-01's 10 s with room for the tick.
    pub offline_after: Duration,
    /// Nothing heard for this long: the connection is given up on (the transport's own idle timeout is shorter on
    /// QUIC; this covers a transport that has none).
    pub dead_after: Duration,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            offline_after: Duration::from_secs(5),
            dead_after: Duration::from_secs(8),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Nothing,
    WentOffline,
    Dead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    pub send_ping: bool,
    pub verdict: Verdict,
}

#[derive(Debug)]
pub struct Liveness {
    cfg: LivenessConfig,
    last_heard: Instant,
    next_ping: Instant,
    offline: bool,
}

impl Liveness {
    pub fn new(cfg: LivenessConfig, now: Instant) -> Self {
        Self { cfg, last_heard: now, next_ping: now + cfg.interval, offline: false }
    }

    pub fn config(&self) -> &LivenessConfig {
        &self.cfg
    }

    /// Anything arrived from the peer. Returns true when that brings it back online.
    pub fn heard(&mut self, now: Instant) -> bool {
        self.last_heard = now;
        std::mem::replace(&mut self.offline, false)
    }

    pub fn is_offline(&self) -> bool {
        self.offline
    }

    pub fn silent_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_heard)
    }

    pub fn tick(&mut self, now: Instant) -> Tick {
        let send_ping = now >= self.next_ping;
        if send_ping {
            self.next_ping = now + self.cfg.interval;
        }
        let silent = self.silent_for(now);
        let verdict = if silent >= self.cfg.dead_after {
            Verdict::Dead
        } else if silent >= self.cfg.offline_after && !self.offline {
            self.offline = true;
            Verdict::WentOffline
        } else {
            Verdict::Nothing
        };
        Tick { send_ping, verdict }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_ping_goes_out_every_interval() {
        let t0 = Instant::now();
        let mut l = Liveness::new(LivenessConfig::default(), t0);
        let pings = (0..=3_100).step_by(100).filter(|n| l.tick(t0 + ms(*n)).send_ping).count();
        assert_eq!(pings, 3);
    }

    #[test]
    fn a_silent_peer_is_offline_at_the_limit_and_dead_later() {
        let t0 = Instant::now();
        let mut l = Liveness::new(LivenessConfig::default(), t0);
        assert_eq!(l.tick(t0 + ms(4_999)).verdict, Verdict::Nothing);
        assert_eq!(l.tick(t0 + ms(5_000)).verdict, Verdict::WentOffline);
        assert_eq!(l.tick(t0 + ms(6_000)).verdict, Verdict::Nothing, "reported once");
        assert!(l.is_offline());
        assert_eq!(l.tick(t0 + ms(8_000)).verdict, Verdict::Dead);
    }

    #[test]
    fn hearing_the_peer_brings_it_back() {
        let t0 = Instant::now();
        let mut l = Liveness::new(LivenessConfig::default(), t0);
        l.tick(t0 + ms(5_100));
        assert!(l.is_offline());
        assert!(l.heard(t0 + ms(5_200)));
        assert!(!l.heard(t0 + ms(5_300)), "already online");
        assert_eq!(l.tick(t0 + ms(9_000)).verdict, Verdict::Nothing, "the silence counts from the last message");
    }

    #[test]
    fn the_default_limit_leaves_room_inside_ten_seconds() {
        let c = LivenessConfig::default();
        assert!(c.offline_after + c.interval <= Duration::from_secs(10));
    }
}
