//! R-UI-4: typed filters and the manual address field act 150 ms after the last key.
//!
//! Pure: the shell passes its own millisecond clock, so a test needs no sleeping. The shell keeps a one-shot timer set
//! to [`Debouncer::deadline`] while a value waits, so an idle window has no timer.

/// How long after the last key the typed value is acted on.
pub const DEBOUNCE_MS: u64 = 150;

#[derive(Debug, Default, Clone)]
pub struct Debouncer {
    waiting: Option<(String, u64)>,
}

impl Debouncer {
    pub fn new() -> Debouncer {
        Debouncer::default()
    }

    /// A key was typed: the value so far, at `now_ms`. Restarts the wait.
    pub fn input(&mut self, text: &str, now_ms: u64) {
        self.waiting = Some((text.to_owned(), now_ms));
    }

    /// The value to act on, once 150 ms passed since the last key (returned once).
    pub fn poll(&mut self, now_ms: u64) -> Option<String> {
        match &self.waiting {
            Some((_, at)) if now_ms.saturating_sub(*at) >= DEBOUNCE_MS => self.waiting.take().map(|(t, _)| t),
            _ => None,
        }
    }

    /// When `poll` will first return the waiting value (for the shell's one-shot timer).
    pub fn deadline(&self) -> Option<u64> {
        self.waiting.as_ref().map(|(_, at)| at + DEBOUNCE_MS)
    }

    /// Enter pressed: act now, without waiting.
    pub fn flush(&mut self) -> Option<String> {
        self.waiting.take().map(|(t, _)| t)
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acts_150_ms_after_the_last_key_and_only_once() {
        let mut d = Debouncer::new();
        d.input("1", 1000);
        d.input("19", 1100);
        d.input("192", 1200);
        assert_eq!(d.poll(1349), None, "149 ms after the last key");
        assert_eq!(d.deadline(), Some(1350));
        assert_eq!(d.poll(1350).as_deref(), Some("192"));
        assert_eq!(d.poll(2000), None);
        assert!(!d.is_waiting());
    }

    #[test]
    fn enter_acts_at_once() {
        let mut d = Debouncer::new();
        d.input("10.0.0.2", 5);
        assert_eq!(d.flush().as_deref(), Some("10.0.0.2"));
        assert_eq!(d.poll(10_000), None);
    }
}
