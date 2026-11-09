//! The OS half of [`crate::watch`]: a thread that keeps an `SCDynamicStore` session on its `CFRunLoop` and pokes the
//! settle half when a watched key changes.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;

use objc2_core_foundation::{CFArray, CFRetained, CFRunLoop, CFString, kCFRunLoopDefaultMode};
use objc2_system_configuration::{SCDynamicStore, SCDynamicStoreContext};
use swapdesk_platform::{ErrorKind, PlatformError, Result};

use crate::watch::Shared;

/// The primary IPv4 service changed (the default route moved to another interface or went away).
const GLOBAL_IPV4: &str = "State:/Network/Global/IPv4";
/// The IPv4 state (addresses) of any interface changed.
const INTERFACE_IPV4: &str = "State:/Network/Interface/[^/]+/IPv4";

/// A run loop handle that may be stopped from the owner's thread.
struct RunLoop(CFRetained<CFRunLoop>);

// SAFETY: the owner's thread only calls `CFRunLoopStop` on it, which Core Foundation allows from any thread (it is how
// another thread ends a run loop); the reference count of a CF object is thread-safe.
unsafe impl Send for RunLoop {}

pub(crate) struct StoreThread {
    run_loop: RunLoop,
    stopped: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

unsafe extern "C-unwind" fn on_store_change(
    _store: NonNull<SCDynamicStore>,
    _keys: NonNull<CFArray>,
    info: *mut c_void,
) {
    // SAFETY: `info` is the `Arc<Shared>` the store thread holds for as long as the store exists (the store is released
    // before the thread drops its `Arc`), so it points to a live `Shared`, which is `Sync`.
    unsafe { (*info.cast::<Shared>()).poke() }
}

fn os_error(op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: None, detail: detail.to_string() }
}

impl StoreThread {
    pub(crate) fn start(shared: Arc<Shared>) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<std::result::Result<RunLoop, &'static str>>();
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let stopped = Arc::clone(&stopped);
            std::thread::Builder::new()
                .name("swapdesk-net-store".into())
                .spawn(move || run(&shared, &stopped, &tx))
                .map_err(|e| os_error("NetworkWatcher store thread", &e.to_string()))?
        };
        match rx.recv() {
            Ok(Ok(run_loop)) => Ok(Self { run_loop, stopped, thread: Some(thread) }),
            Ok(Err(what)) => {
                let _ = thread.join();
                Err(os_error(what, "SystemConfiguration refused"))
            }
            Err(_) => {
                let _ = thread.join();
                Err(os_error("NetworkWatcher store thread", "ended before it was ready"))
            }
        }
    }
}

/// The store thread: opens the session, watches the keys and sleeps in the run loop until stopped.
fn run(
    shared: &Arc<Shared>,
    stopped: &std::sync::atomic::AtomicBool,
    ready: &mpsc::Sender<std::result::Result<RunLoop, &'static str>>,
) {
    let mut context = SCDynamicStoreContext {
        version: 0,
        info: Arc::as_ptr(shared).cast_mut().cast::<c_void>(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    let name = CFString::from_static_str("Swapdesk network watcher");
    // SAFETY: the callout has the signature SystemConfiguration calls; `context` is read during the call (the store
    // copies it) and its `info` outlives the store (see `on_store_change`).
    let Some(store) = (unsafe { SCDynamicStore::new(None, &name, Some(on_store_change), &mut context) }) else {
        let _ = ready.send(Err("SCDynamicStoreCreate"));
        return;
    };
    let keys = CFArray::from_retained_objects(&[CFString::from_static_str(GLOBAL_IPV4)]);
    let patterns = CFArray::from_retained_objects(&[CFString::from_static_str(INTERFACE_IPV4)]);
    // SAFETY: both arrays hold CFStrings, as the call requires.
    if !unsafe { store.set_notification_keys(Some(keys.as_opaque()), Some(patterns.as_opaque())) } {
        let _ = ready.send(Err("SCDynamicStoreSetNotificationKeys"));
        return;
    }
    let (Some(source), Some(run_loop)) = (SCDynamicStore::new_run_loop_source(None, &store, 0), CFRunLoop::current())
    else {
        let _ = ready.send(Err("SCDynamicStoreCreateRunLoopSource"));
        return;
    };
    // SAFETY: a static of Core Foundation, read only.
    let mode = unsafe { kCFRunLoopDefaultMode };
    run_loop.add_source(Some(&source), mode);
    if ready.send(Ok(RunLoop(run_loop.clone()))).is_err() {
        return;
    }
    // `CFRunLoopStop` before the loop is entered is not lost: it also wakes the loop, and the wake-up waits in the
    // loop's port, so the next run returns at once and the flag ends it.
    while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
        CFRunLoop::run();
    }
    run_loop.remove_source(Some(&source), mode);
    source.invalidate();
    // `store` is released here, before `shared` (borrowed from the closure's `Arc`) can go.
    drop(store);
}

impl Drop for StoreThread {
    fn drop(&mut self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        self.run_loop.0.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
