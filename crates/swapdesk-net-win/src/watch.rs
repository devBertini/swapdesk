//! Network change notices without a timer: the OS says when an IP interface or an IPv4 address changes, and a thread
//! that sleeps until then turns a burst of notices into one call of the owner's callback.
//!
//! `NotifyIpInterfaceChange` and `NotifyUnicastIpAddressChange` (IPv4) register a callback that Windows runs on one of
//! its own threads for every change (an adapter appearing, going down, a new address). The callback only marks a flag
//! and wakes the thread of this module: a cable plugged or a Wi-Fi switch makes dozens of notices within a moment, and
//! the thread waits [`SETTLE`] after the first so the owner is told once, after the network has settled.
//!
//! What no notice covers is the network category (Public or Private, set by the user in Settings) and the firewall
//! state: the owner re-reads the interface list now and then for those, much more rarely than it did when this was
//! its only source.
//!
//! Source read on 2026-11-08: `NotifyIpInterfaceChange`
//! <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-notifyipinterfacechange>: the callbacks
//! are serialized; `CancelMibChangeNotify2` must not be called from the callback's own thread (it is called from
//! [`Drop`] here, never from the callback), and the registration ends with the process anyway.

use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_platform::{ErrorKind, PlatformError, Result};
use windows_sys::Win32::Foundation::{HANDLE, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE, MIB_UNICASTIPADDRESS_ROW,
    NotifyIpInterfaceChange, NotifyUnicastIpAddressChange,
};
use windows_sys::Win32::Networking::WinSock::AF_INET;

/// How long the thread waits after the first notice of a burst before it calls back.
pub const SETTLE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Flags {
    /// A notice came and the callback has not run for it yet.
    pending: bool,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    flags: Mutex<Flags>,
    wake: Condvar,
}

fn lock(m: &Mutex<Flags>) -> MutexGuard<'_, Flags> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    /// What the OS callbacks do: mark and wake. Nothing else runs on the OS's thread.
    fn poke(&self) {
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

/// Calls the owner's callback when the network changed, until dropped.
pub struct NetworkWatcher {
    shared: Arc<Shared>,
    /// `NotifyIpInterfaceChange` and `NotifyUnicastIpAddressChange`.
    registrations: [HANDLE; 2],
    /// The reference the OS callbacks hold, given back after both are cancelled.
    context: *const Shared,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: the raw pointer is an `Arc<Shared>` reference count leaked on purpose for the callbacks (`Shared` is `Sync`)
// and the handles are only used to cancel the registrations, which `CancelMibChangeNotify2` allows from any thread.
unsafe impl Send for NetworkWatcher {}

unsafe extern "system" fn on_interface(
    context: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: `context` is the `Arc<Shared>` pointer `start_with` leaked; it stays valid until `Drop` has cancelled
    // both registrations, after which no callback runs.
    unsafe { (*context.cast::<Shared>()).poke() }
}

unsafe extern "system" fn on_address(
    context: *const c_void,
    _row: *const MIB_UNICASTIPADDRESS_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    // SAFETY: as for `on_interface`.
    unsafe { (*context.cast::<Shared>()).poke() }
}

fn os_error(op: &'static str, code: u32) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: Some(i64::from(code)), detail: format!("error {code}") }
}

impl NetworkWatcher {
    /// Starts watching; `on_change` runs on a thread of this watcher, once per burst of changes.
    pub fn start(on_change: impl Fn() + Send + 'static) -> Result<Self> {
        Self::start_with(SETTLE, on_change)
    }

    pub fn start_with(settle: Duration, on_change: impl Fn() + Send + 'static) -> Result<Self> {
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
                .map_err(|e| PlatformError {
                    kind: ErrorKind::Os,
                    op: "NetworkWatcher::start",
                    os_code: e.raw_os_error().map(i64::from),
                    detail: e.to_string(),
                })?
        };
        let context = Arc::into_raw(Arc::clone(&shared));
        let mut registrations: [HANDLE; 2] = [core::ptr::null_mut(); 2];
        // SAFETY: the callbacks and the context outlive the registrations (see `Drop`); the handle slots are writable.
        let first = unsafe {
            NotifyIpInterfaceChange(AF_INET, Some(on_interface), context.cast(), false, &mut registrations[0])
        };
        // SAFETY: as above.
        let second = unsafe {
            NotifyUnicastIpAddressChange(AF_INET, Some(on_address), context.cast(), false, &mut registrations[1])
        };
        // A slot that failed holds NULL; `Drop` cancels the other one and skips it.
        let watcher = Self { shared, registrations, context, thread: Some(thread) };
        if first != NO_ERROR {
            return Err(os_error("NotifyIpInterfaceChange", first));
        }
        if second != NO_ERROR {
            return Err(os_error("NotifyUnicastIpAddressChange", second));
        }
        Ok(watcher)
    }

    /// Reports a change as if the OS had (for a test, or for a caller that knows better).
    pub fn poke(&self) {
        self.shared.poke();
    }
}

impl Drop for NetworkWatcher {
    fn drop(&mut self) {
        for handle in self.registrations {
            if !handle.is_null() {
                // SAFETY: the handle came from a successful registration and is cancelled once, from this thread, which
                // is not a callback thread.
                unsafe { CancelMibChangeNotify2(handle) };
            }
        }
        self.shared.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: both registrations are cancelled, so no callback holds `context` any more; this is the reference
        // `Arc::into_raw` leaked in `start_with`.
        drop(unsafe { Arc::from_raw(self.context) });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;

    #[test]
    fn registering_with_the_os_works_and_dropping_ends_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let watcher = NetworkWatcher::start(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .expect("NotifyIpInterfaceChange and NotifyUnicastIpAddressChange register");
        drop(watcher);
    }

    #[test]
    fn a_burst_of_notices_is_one_call_after_the_settle_and_a_quiet_network_makes_none() {
        let (tx, rx) = std::sync::mpsc::channel();
        let watcher = NetworkWatcher::start_with(Duration::from_millis(200), move || {
            let _ = tx.send(Instant::now());
        })
        .unwrap();
        // Nothing changes: nothing is called, and nothing wakes the thread (it waits on the condition variable).
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
        let first = Instant::now();
        for _ in 0..50 {
            watcher.poke();
            std::thread::sleep(Duration::from_millis(2));
        }
        let called = rx.recv_timeout(Duration::from_secs(5)).expect("called after the burst");
        assert!(called.duration_since(first) >= Duration::from_millis(190), "it waited for the network to settle");
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "one call for the whole burst");
        // A later change is a new burst.
        watcher.poke();
        rx.recv_timeout(Duration::from_secs(5)).expect("called again");
    }

    #[test]
    fn dropping_while_a_burst_settles_ends_the_thread_without_a_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let watcher = NetworkWatcher::start_with(Duration::from_secs(30), move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        watcher.poke();
        std::thread::sleep(Duration::from_millis(100));
        let began = Instant::now();
        drop(watcher);
        assert!(began.elapsed() < Duration::from_secs(5), "the stop cut the 30 s settle short");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
