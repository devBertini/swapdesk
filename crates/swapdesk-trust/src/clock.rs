//! Wall-clock time for persisted records. C3's `Clock` is monotonic microseconds and cannot be persisted, so the
//! trust crate adds this small trait (contract gap, reported): the lockout and the records of C5 hold Unix ms.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use swapdesk_platform::Clock;

pub trait WallClock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn unix_ms(&self) -> u64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn unix_ms(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A `WallClock` that follows a C3 `Clock` from a fixed Unix origin, so a test moves both with one `FakeClock`.
#[derive(Clone)]
pub struct ClockWall {
    clock: Arc<dyn Clock>,
    origin_ms: u64,
}

impl ClockWall {
    pub fn new(clock: Arc<dyn Clock>, origin_ms: u64) -> Self {
        Self { clock, origin_ms }
    }
}

impl WallClock for ClockWall {
    fn unix_ms(&self) -> u64 {
        self.origin_ms + self.clock.now().0 / 1000
    }
}
