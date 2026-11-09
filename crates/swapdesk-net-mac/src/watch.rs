//! Network change notices without a timer, for the Mac app (the counterpart of `swapdesk-net-win`'s watcher).
//!
//! On macOS, SystemConfiguration's dynamic store tells when the IPv4 state of an interface (`State:/Network/Interface/
//! <bsd name>/IPv4`, its addresses) or the primary IPv4 service (`State:/Network/Global/IPv4`) changes: a DHCP lease
//! that moved the address, Wi-Fi joined or left, a cable plugged. The store's callback runs on a thread of this module
//! that sleeps in its `CFRunLoop`; it only marks a flag and wakes the settle thread, which waits [`SETTLE`] after the
//! first notice of a burst so the owner is told once, after the network has settled.
//!
//! The settle half is OS-free: [`NetworkWatcher::manual`] has no OS source and [`NetworkWatcher::poke`] plays a notice,
//! so the owner's reaction is tested on any host.
//!
//! Sources read on 2026-11-09: `SCDynamicStoreSetNotificationKeys` ("an array of keys to be monitored", "an array of
//! regex(3) pattern strings used to match keys to be monitored"),
//! <https://developer.apple.com/documentation/systemconfiguration/scdynamicstoresetnotificationkeys(_:_:_:)>; the key
//! names as `scutil` lists them on the MacBook (`State:/Network/Interface/en0/IPv4`, `State:/Network/Global/IPv4`).

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_platform::{ErrorKind, PlatformError, Result};

/// How long the settle thread waits after the first notice of a burst before it calls back.
pub const SETTLE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Flags {
    /// A notice came and the callback has not run for it yet.
    pending: bool,
    stopped: bool,
}

#[derive(Default)]
pub(crate) struct Shared {
    flags: Mutex<Flags>,
    wake: Condvar,
}

fn lock(m: &Mutex<Flags>) -> MutexGuard<'_, Flags> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// What the OS callback does: mark and wake. Nothing else runs on the OS's thread.
    pub(crate) fn poke(&self) {
        lock(&self.flags).pending = true;
        self.wake.notify_one();
    }

    fn stop(&self) {
        lock(&self.flags).stopped = true;
        self.wake.notify_all();
    }

    /// Sleeps until a notice came (`true`) or the watcher was stopped (`false`).
    fn wait_for_notice(&self) -> bool {
        let guard = self
            .wake
            .wait_while(lock(&self.flags), |f| !f.pending && !f.stopped)
            .unwrap_or_else(PoisonError::into_inner);
        !guard.stopped
    }

    /// Lets the burst finish: waits `settle` (cut short by a stop), then takes the pending mark. `false` when stopped.
    fn settle(&self, settle: Duration) -> bool {
        let (mut guard, _) = self
            .wake
            .wait_timeout_while(lock(&self.flags), settle, |f| !f.stopped)
            .unwrap_or_else(PoisonError::into_inner);
        guard.pending = false;
        !guard.stopped
    }
}

fn thread_error(op: &'static str, e: &std::io::Error) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: e.raw_os_error().map(i64::from), detail: e.to_string() }
}

/// Calls the owner's callback when the network changed, once per burst, until dropped.
pub struct NetworkWatcher {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    #[cfg(target_os = "macos")]
    store: Option<crate::watch_mac::StoreThread>,
}

impl NetworkWatcher {
    /// Watches SystemConfiguration's dynamic store; `on_change` runs on a thread of this watcher.
    #[cfg(target_os = "macos")]
    pub fn start(on_change: impl Fn() + Send + 'static) -> Result<Self> {
        let mut w = Self::manual(SETTLE, on_change)?;
        w.store = Some(crate::watch_mac::StoreThread::start(Arc::clone(&w.shared))?);
        Ok(w)
    }

    /// The settle half alone: no OS source, [`poke`](Self::poke) plays a notice (tests, any host).
    pub fn manual(settle: Duration, on_change: impl Fn() + Send + 'static) -> Result<Self> {
        let shared = Arc::new(Shared::default());
        let thread = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("swapdesk-net-watch".into())
                .spawn(move || {
                    while shared.wait_for_notice() {
                        if !shared.settle(settle) {
                            return;
                        }
                        on_change();
                    }
                })
                .map_err(|e| thread_error("NetworkWatcher::start", &e))?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
            #[cfg(target_os = "macos")]
            store: None,
        })
    }

    /// Reports a change as if the OS had.
    pub fn poke(&self) {
        self.shared.poke();
    }
}

impl Drop for NetworkWatcher {
    fn drop(&mut self) {
        // The OS source first: after it nothing pokes `shared` any more.
        #[cfg(target_os = "macos")]
        drop(self.store.take());
        self.shared.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;

    #[test]
    fn a_burst_of_notices_is_one_call_and_a_later_notice_is_another() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let w = NetworkWatcher::manual(Duration::from_millis(100), move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        for _ in 0..20 {
            w.poke();
        }
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a burst is one call");
        w.poke();
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Nothing comes without a notice.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_secs(1), "the watcher stops at once");
    }

    /// The real source: the dynamic store session opens with its keys and run loop source, and the watcher stops at
    /// once. (A notice itself needs a real IPv4 change: posting one on a `State:/Network` key needs root, `scutil`
    /// answers "Permission denied" to a user.)
    #[cfg(target_os = "macos")]
    #[test]
    fn the_dynamic_store_watcher_starts_and_stops_at_once() {
        let w = NetworkWatcher::start(|| {}).expect("the dynamic store watcher starts");
        std::thread::sleep(Duration::from_millis(200));
        let t = Instant::now();
        drop(w);
        assert!(t.elapsed() < Duration::from_secs(1), "the run loop thread stops at once");
        // Stopping right after starting, before the run loop may have been entered, does not hang either.
        let t = Instant::now();
        drop(NetworkWatcher::start(|| {}).expect("starts again"));
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}
