//! Pairing mode (D12, PROTOCOL.md 3): both users opened "Parear" within the last 2 minutes.

use std::sync::{Arc, Mutex, MutexGuard};

use swapdesk_platform::Clock;
use tokio::sync::watch;

use crate::ids::DeviceId;

/// How long pairing mode lasts.
pub const PAIRING_MODE_MICROS: u64 = 2 * 60 * 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Open {
    until: u64,
    expect: Option<DeviceId>,
    replaces: Option<DeviceId>,
}

/// What the user picked when entering pairing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModeOptions {
    /// The user chose a device in "Parear": only that device's key may pair (its device ID).
    pub expect: Option<DeviceId>,
    /// The ceremony replaces this device's entry (a re-pairing after a changed key, logged "chave substituída").
    pub replaces: Option<DeviceId>,
}

/// A copy of what the mode allows right now.
pub type ModeSnapshot = ModeOptions;

/// The state of one machine's pairing mode. The service closes the pairing endpoint when it turns inactive.
pub struct PairingMode {
    clock: Arc<dyn Clock>,
    open: Mutex<Option<Open>>,
    /// Counts every `enter` and `close`, so the accept loop of the pairing endpoint sleeps until the mode changes
    /// instead of looking at it on a timer.
    changes: watch::Sender<u64>,
}

impl PairingMode {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock, open: Mutex::new(None), changes: watch::Sender::new(0) }
    }

    fn guard(&self) -> MutexGuard<'_, Option<Open>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Opens pairing mode for 2 minutes from now (entering again restarts the time).
    pub fn enter(&self, options: ModeOptions) {
        let until = self.clock.now().0.saturating_add(PAIRING_MODE_MICROS);
        *self.guard() = Some(Open { until, expect: options.expect, replaces: options.replaces });
        self.changes.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// A receiver that sees every later `enter` and `close` (the first `changed()` resolves for the first of them).
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The user closed the screen, or a pairing ended.
    pub fn close(&self) {
        *self.guard() = None;
        self.changes.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// The options while the mode is open; `None` once it ended or expired.
    pub fn current(&self) -> Option<ModeSnapshot> {
        let mut open = self.guard();
        match *open {
            Some(o) if self.clock.now().0 < o.until => Some(ModeOptions { expect: o.expect, replaces: o.replaces }),
            Some(_) => {
                *open = None;
                None
            }
            None => None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.current().is_some()
    }

    /// Microseconds of the mode left (0 when closed).
    pub fn remaining_micros(&self) -> u64 {
        match *self.guard() {
            Some(o) => o.until.saturating_sub(self.clock.now().0),
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use swapdesk_fakes::FakeClock;
    use swapdesk_platform::MonoMicros;

    #[test]
    fn it_lasts_two_minutes() {
        let clock = FakeClock::new(MonoMicros(0));
        let mode = PairingMode::new(Arc::new(clock.clone()));
        assert!(!mode.is_open());
        mode.enter(ModeOptions::default());
        clock.advance(Duration::from_secs(119));
        assert!(mode.is_open());
        clock.advance(Duration::from_secs(1));
        assert!(!mode.is_open());
        assert_eq!(mode.remaining_micros(), 0);
    }

    #[test]
    fn a_subscriber_sees_every_enter_and_close() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let clock = FakeClock::new(MonoMicros(0));
        let mode = PairingMode::new(Arc::new(clock));
        let mut changes = mode.subscribe();
        mode.enter(ModeOptions::default());
        mode.close();
        rt.block_on(async {
            // Both changes happened before the wait: it resolves at once, it does not wait for a third.
            tokio::time::timeout(Duration::from_secs(1), changes.changed()).await.unwrap().unwrap();
            assert!(tokio::time::timeout(Duration::from_millis(50), changes.changed()).await.is_err());
            mode.enter(ModeOptions::default());
            tokio::time::timeout(Duration::from_secs(1), changes.changed()).await.unwrap().unwrap();
        });
    }

    #[test]
    fn closing_ends_it_and_entering_again_restarts_the_time() {
        let clock = FakeClock::new(MonoMicros(0));
        let mode = PairingMode::new(Arc::new(clock.clone()));
        mode.enter(ModeOptions { expect: Some([1; 16]), replaces: None });
        assert_eq!(mode.current().unwrap().expect, Some([1; 16]));
        mode.close();
        assert!(!mode.is_open());
        mode.enter(ModeOptions::default());
        clock.advance(Duration::from_secs(100));
        mode.enter(ModeOptions::default());
        clock.advance(Duration::from_secs(100));
        assert!(mode.is_open());
    }
}
