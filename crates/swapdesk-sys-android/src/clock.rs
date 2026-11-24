//! The phone's monotonic clock (C3 `Clock`): microseconds since the clock was made, from `std::time::Instant`
//! (`CLOCK_MONOTONIC` on Android). One process holds the whole app, so no other process has to agree with it.

use std::time::Instant;

use swapdesk_platform::{Clock, MonoMicros};

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
    fn it_never_goes_back() {
        let clock = MonotonicClock::new();
        let mut last = clock.now();
        for _ in 0..1000 {
            let now = clock.now();
            assert!(now >= last);
            last = now;
        }
    }
}
