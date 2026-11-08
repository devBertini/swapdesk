//! R-UI-10: the UI logs UI-thread work slower than 16 ms with its name and duration only, never content (F-SEC-01).
//!
//! A tiny sink: entries go to a bounded in-memory ring (tests, a debug build's "last slow work") and to an optional
//! hook the shell sets (`OutputDebugString`, a file). R-UI-8: a debug build also asserts that nothing on the UI thread
//! blocks for a long time.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The UI thread's threshold (R-UI-10).
pub const UI_THRESHOLD_MS: u64 = 16;
/// A UI-thread step slower than this in a debug build is treated as a blocking call (R-UI-8).
pub const BLOCKING_ASSERT_MS: u64 = 1000;
const RING: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowEntry {
    pub name: &'static str,
    pub millis: u64,
}

static LOG: Mutex<Vec<SlowEntry>> = Mutex::new(Vec::new());
static HOOK: Mutex<Option<fn(&SlowEntry)>> = Mutex::new(None);

/// Sends every future entry to `hook` as well (the shell's debug output).
pub fn set_hook(hook: fn(&SlowEntry)) {
    if let Ok(mut h) = HOOK.lock() {
        *h = Some(hook);
    }
}

fn record(entry: SlowEntry) {
    if let Ok(h) = HOOK.lock()
        && let Some(hook) = *h
    {
        hook(&entry);
    }
    if let Ok(mut log) = LOG.lock() {
        if log.len() == RING {
            log.remove(0);
        }
        log.push(entry);
    }
}

/// The slow entries recorded so far (oldest first).
pub fn entries() -> Vec<SlowEntry> {
    LOG.lock().map(|l| l.clone()).unwrap_or_default()
}

pub fn clear() {
    if let Ok(mut l) = LOG.lock() {
        l.clear();
    }
}

/// Times a scope: on drop, records it when it ran longer than `threshold`.
pub struct SlowGuard {
    name: &'static str,
    start: Instant,
    threshold: Duration,
}

impl SlowGuard {
    /// A guard with the UI thread's 16 ms threshold.
    pub fn ui(name: &'static str) -> SlowGuard {
        SlowGuard::with_threshold(name, Duration::from_millis(UI_THRESHOLD_MS))
    }

    pub fn with_threshold(name: &'static str, threshold: Duration) -> SlowGuard {
        SlowGuard { name, start: Instant::now(), threshold }
    }
}

impl Drop for SlowGuard {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        if elapsed >= self.threshold {
            record(SlowEntry { name: self.name, millis: elapsed.as_millis() as u64 });
        }
        debug_assert!(
            std::thread::panicking() || elapsed < Duration::from_millis(BLOCKING_ASSERT_MS),
            "UI thread blocked for {} ms in {}",
            elapsed.as_millis(),
            self.name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_work_over_the_threshold_is_logged_with_name_and_duration() {
        clear();
        {
            let _g = SlowGuard::with_threshold("fast_step", Duration::from_millis(50));
        }
        {
            let _g = SlowGuard::with_threshold("slow_step", Duration::from_millis(10));
            std::thread::sleep(Duration::from_millis(25));
        }
        let e = entries();
        assert!(e.iter().all(|x| x.name != "fast_step"));
        let slow = e.iter().find(|x| x.name == "slow_step").expect("slow step logged");
        assert!(slow.millis >= 20 && slow.millis < 500, "{slow:?}");
    }
}
