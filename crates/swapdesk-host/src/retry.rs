//! Retrying a start that fails because nothing can be captured right now (`ErrorKind::Unavailable`).
//!
//! A Mac whose display sleeps lists no display to ScreenCaptureKit, so the capture and the audio of a session that
//! begins then fail with `Unavailable`. The failure passes when the display wakes, so the helper keeps asking: every
//! [`FAST_PAUSE`] for the first [`FAST_FOR`], then every [`SLOW_PAUSE`], until it works or the thread is told to stop.
//! The viewer meanwhile sees the session active with its normal waiting picture: nothing is reported as a fault.

use std::time::{Duration, Instant};

use swapdesk_platform::{ErrorKind, PlatformError};

/// Pause between attempts during the first [`FAST_FOR`].
pub(crate) const FAST_PAUSE: Duration = Duration::from_millis(500);
/// How long the fast pause lasts.
pub(crate) const FAST_FOR: Duration = Duration::from_secs(10);
/// Pause between attempts after that.
pub(crate) const SLOW_PAUSE: Duration = Duration::from_secs(2);

/// The pause after the `failed`th failed attempt (1 for the first).
pub(crate) fn pause_after(failed: u32) -> Duration {
    let fast_attempts = u32::try_from(FAST_FOR.as_millis() / FAST_PAUSE.as_millis()).unwrap_or(u32::MAX);
    if failed <= fast_attempts { FAST_PAUSE } else { SLOW_PAUSE }
}

/// Whether a platform failure is the kind that passes by itself.
pub(crate) fn is_unavailable(e: &PlatformError) -> bool {
    e.kind == ErrorKind::Unavailable
}

/// Runs `attempt` until it succeeds or fails with something `retryable` refuses to wait out. `waiting` is called on
/// every failed attempt that will be retried (with the number of failures so far and the error), `sleep` waits out the
/// pause and returns false when the wait was cut short because the caller must stop. `None`: stopped before it worked.
pub(crate) fn retry<T, E>(
    mut attempt: impl FnMut() -> Result<T, E>,
    retryable: impl Fn(&E) -> bool,
    mut waiting: impl FnMut(u32, &E),
    mut sleep: impl FnMut(Duration) -> bool,
) -> Option<Result<T, E>> {
    let mut failed = 0u32;
    loop {
        match attempt() {
            Ok(v) => return Some(Ok(v)),
            Err(e) if retryable(&e) => {
                failed = failed.saturating_add(1);
                waiting(failed, &e);
                if !sleep(pause_after(failed)) {
                    return None;
                }
            }
            Err(e) => return Some(Err(e)),
        }
    }
}

/// A sleep that ends early when `stop` says so (checked every 50 ms); false when it did.
pub(crate) fn sleep_unless(total: Duration, stop: impl Fn() -> bool) -> bool {
    let end = Instant::now() + total;
    loop {
        if stop() {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake source: no display for the first `down` calls, then it works.
    struct Source {
        down: u32,
        calls: u32,
    }

    impl Source {
        fn open(&mut self) -> Result<&'static str, PlatformError> {
            self.calls += 1;
            if self.calls <= self.down {
                Err(PlatformError {
                    kind: ErrorKind::Unavailable,
                    op: "SCShareableContent.displays",
                    os_code: None,
                    detail: "no display".into(),
                })
            } else {
                Ok("open")
            }
        }
    }

    #[test]
    fn it_asks_again_until_a_display_appears_then_returns_the_success() {
        let mut source = Source { down: 3, calls: 0 };
        let (mut seen, mut pauses) = (Vec::new(), Vec::new());
        let r = retry(
            || source.open(),
            is_unavailable,
            |n, _| seen.push(n),
            |d| {
                pauses.push(d);
                true
            },
        );
        assert_eq!(r.unwrap().unwrap(), "open");
        assert_eq!(source.calls, 4);
        assert_eq!(seen, vec![1, 2, 3]);
        assert_eq!(pauses, vec![FAST_PAUSE; 3]);
    }

    #[test]
    fn the_pause_is_half_a_second_for_ten_seconds_then_two() {
        let mut source = Source { down: 25, calls: 0 };
        let mut pauses = Vec::new();
        let r = retry(
            || source.open(),
            is_unavailable,
            |_, _| {},
            |d| {
                pauses.push(d);
                true
            },
        );
        assert!(r.unwrap().is_ok());
        assert_eq!(pauses.len(), 25);
        assert!(pauses[..20].iter().all(|d| *d == Duration::from_millis(500)), "{pauses:?}");
        assert!(pauses[20..].iter().all(|d| *d == Duration::from_secs(2)), "{pauses:?}");
        let fast: Duration = pauses[..20].iter().sum();
        assert_eq!(fast, Duration::from_secs(10));
    }

    #[test]
    fn another_failure_is_returned_at_once_and_a_stop_ends_the_wait() {
        let mut calls = 0;
        let r = retry(
            || -> Result<(), PlatformError> {
                calls += 1;
                Err(PlatformError { kind: ErrorKind::Os, op: "x", os_code: None, detail: String::new() })
            },
            is_unavailable,
            |_, _| panic!("not retried"),
            |_| true,
        );
        assert_eq!(r.unwrap().unwrap_err().kind, ErrorKind::Os);
        assert_eq!(calls, 1);

        let mut source = Source { down: u32::MAX, calls: 0 };
        let r = retry(|| source.open(), is_unavailable, |_, _| {}, |_| false);
        assert!(r.is_none());
    }

    #[test]
    fn the_real_sleep_stops_early_when_asked() {
        let started = Instant::now();
        assert!(!sleep_unless(Duration::from_secs(5), || true));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(sleep_unless(Duration::from_millis(60), || false));
    }
}
