use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use swapdesk_platform::{Clock, MonoMicros};

/// Manual time. Clones share the same instant.
#[derive(Debug, Clone, Default)]
pub struct FakeClock(Arc<AtomicU64>);

impl FakeClock {
    pub fn new(start: MonoMicros) -> Self {
        Self(Arc::new(AtomicU64::new(start.0)))
    }

    pub fn advance(&self, by: Duration) {
        let micros = u64::try_from(by.as_micros()).unwrap_or(u64::MAX);
        self.0.fetch_add(micros, Ordering::SeqCst);
    }

    pub fn set(&self, to: MonoMicros) {
        self.0.store(to.0, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(self.0.load(Ordering::SeqCst))
    }
}

/// Real monotonic time since the clock was made, for the paced fakes in measurements.
#[derive(Debug, Clone, Copy)]
pub struct MonotonicClock(Instant);

impl MonotonicClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_and_read() {
        let clock = FakeClock::new(MonoMicros(1_000));
        let shared = clock.clone();
        assert_eq!(clock.now(), MonoMicros(1_000));
        clock.advance(Duration::from_millis(5));
        assert_eq!(shared.now(), MonoMicros(6_000));
        shared.set(MonoMicros(42));
        assert_eq!(clock.now(), MonoMicros(42));
    }

    #[test]
    fn monotonic_never_goes_back() {
        let clock = MonotonicClock::new();
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a);
    }
}
