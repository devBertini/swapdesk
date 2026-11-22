//! Keeps the host banner for the person at this Mac (the owner's order of 22/11/2026: only who is controlled may
//! see and use it, and it must never be in the way of who controls).
//!
//! The banner's panel takes no mouse events at all (`ignoresMouseEvents`), so whatever the controller's pointer
//! does where the banner is goes to what lies under it, as if the banner were not there. The person at the Mac
//! still uses it: an event tap of this process looks at each mouse event of the session, and a press of a real
//! device over the banner is taken out of the stream and handed to the banner. What the controller sends is told
//! apart by its origin: the host's input sink posts from this very process and marks its events
//! (`swapdesk-input-mac`'s `POSTED_MARK`). The decision itself is `swapdesk_paint::banner_guard::Guard`, the same
//! one the Windows shell feeds from its low-level mouse hook.
//!
//! The tap needs the Accessibility grant, which a Mac that can be controlled already has (posting input needs it
//! too). Without it no tap is made and the banner stays an ordinary panel: nothing can inject input then.
//!
//! Apple documentation (read 22/11/2026): `CGEventTapCreate` and the tap callback
//! (developer.apple.com/documentation/coregraphics/cgevent/tapcreate(tap:place:options:eventsofinterest:callback:userinfo:)),
//! `CGEventField.eventSourceUnixProcessID` and `.eventSourceUserData`, `CGEventTapLocation.cgSessionEventTap`,
//! `NSWindow.ignoresMouseEvents`. Every `unsafe` of the guard is in this file.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use swapdesk_paint::banner_guard::{Guard, MAC_POSTED_MARK, MouseEvent, Verdict};

/// What the banner is told of the real pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pointer {
    Hover,
    Leave,
    Press,
    Release { inside: bool },
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the tap's callback reaches.
struct Shared {
    /// The banner's rectangle in the global points of the displays (x, y, width, height from the top left of the
    /// main display, as `CGEvent` locations are); `None` while the banner is not on the screen.
    rect: Mutex<Option<[f64; 4]>>,
    guard: Mutex<Guard>,
    /// The tap's own port, to switch it back on when macOS disables it.
    port: AtomicPtr<CFMachPort>,
    /// This process: what it posts is the controller's input.
    pid: i64,
    /// Mouse events seen over the banner: the controller's (left alone) and a real device's (the banner's). The
    /// proof on a live Mac reads them: a controller's pointer over the banner must count in the first only.
    posted_over: AtomicU64,
    real_over: AtomicU64,
}

/// Whether this process posted the event (the host's input sink): by the process the event came from, and by the
/// mark the sink puts on what it posts. Either says so.
fn posted_here(shared: &Shared, event: &CGEvent) -> bool {
    CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID) == shared.pid
        || CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData) == MAC_POSTED_MARK
}

/// Callback of the tap, on the tap thread's run loop. It must answer fast (macOS disables a tap that does not): two
/// locks and, for an event that is the banner's, one block queued for the main thread. Returning null drops the
/// event.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `info` is the `Shared` the tap thread holds through its `Arc` until the tap is gone.
    let shared = unsafe { &*info.cast::<Shared>() };
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        let port = shared.port.load(Ordering::Acquire);
        if !port.is_null() {
            // SAFETY: the port is alive while the thread runs; it clears the pointer before dropping it.
            CGEvent::tap_enable(unsafe { &*port }, true);
        }
        return event.as_ptr();
    }
    let what = if kind == CGEventType::LeftMouseDown {
        MouseEvent::LeftDown
    } else if kind == CGEventType::LeftMouseUp {
        MouseEvent::LeftUp
    } else if kind == CGEventType::MouseMoved || kind == CGEventType::LeftMouseDragged {
        MouseEvent::Move
    } else {
        return event.as_ptr();
    };
    // SAFETY: the event pointer is valid for the duration of the callback.
    let ev = unsafe { event.as_ref() };
    let at = CGEvent::location(Some(ev));
    let rect = *lock(&shared.rect);
    let inside = rect.is_some_and(|r| crate::model::banner_contains(r, at.x, at.y));
    let posted = posted_here(shared, ev);
    if inside {
        let count = if posted { &shared.posted_over } else { &shared.real_over };
        count.fetch_add(1, Ordering::Relaxed);
    }
    let verdict = lock(&shared.guard).on_event(what, posted, inside);
    let pointer = match verdict {
        Verdict::Pass => None,
        Verdict::Hover => Some(Pointer::Hover),
        Verdict::Leave => Some(Pointer::Leave),
        Verdict::Press => Some(Pointer::Press),
        Verdict::Release { inside } => Some(Pointer::Release { inside }),
    };
    if let (Some(pointer), Some(r)) = (pointer, rect) {
        // In the banner's own points, from its top left (its canvas is flipped).
        let local = ((at.x - r[0]) as f32, (at.y - r[1]) as f32);
        DispatchQueue::main().exec_async(move || crate::shell::banner_pointer(pointer, local));
    }
    if verdict.swallows() { std::ptr::null_mut() } else { event.as_ptr() }
}

/// A `CFRunLoop` another thread may stop.
struct RunLoopHandle(CFRetained<CFRunLoop>);

// SAFETY: a Core Foundation run loop is documented as thread safe (Threading Programming Guide, Thread Safety
// Summary); the only use made of this handle is `CFRunLoopStop` from the owner's thread.
unsafe impl Send for RunLoopHandle {}

fn run_tap(shared: &Arc<Shared>, ready: &SyncSender<Option<RunLoopHandle>>) {
    let mask: u64 = (1 << CGEventType::LeftMouseDown.0)
        | (1 << CGEventType::LeftMouseUp.0)
        | (1 << CGEventType::MouseMoved.0)
        | (1 << CGEventType::LeftMouseDragged.0);
    // SAFETY: the callback is a valid function and `shared` outlives the tap (this function returns only after the
    // tap is disabled and invalidated, and it holds the `Arc` until then).
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(tap_callback),
            Arc::as_ptr(shared).cast_mut().cast(),
        )
    };
    // No tap without the Accessibility grant: the banner then stays an ordinary panel.
    let Some(tap) = tap else {
        let _ = ready.send(None);
        return;
    };
    let (Some(source), Some(run_loop)) = (CFMachPort::new_run_loop_source(None, Some(&tap), 0), CFRunLoop::current())
    else {
        let _ = ready.send(None);
        return;
    };
    // SAFETY: the mode constant is a valid CFString for the process.
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    shared.port.store(std::ptr::from_ref::<CFMachPort>(&tap).cast_mut(), Ordering::Release);
    CGEvent::tap_enable(&tap, true);
    if ready.send(Some(RunLoopHandle(run_loop))).is_err() {
        // The starter gave up waiting: do not leave a tap behind.
        shared.port.store(std::ptr::null_mut(), Ordering::Release);
        tap.invalidate();
        return;
    }
    CFRunLoop::run();
    shared.port.store(std::ptr::null_mut(), Ordering::Release);
    CGEvent::tap_enable(&tap, false);
    tap.invalidate();
}

/// The guard of one banner: the tap and its thread, for as long as the banner is on the screen.
pub struct BannerGuard {
    shared: Arc<Shared>,
    run_loop: RunLoopHandle,
    join: Option<JoinHandle<()>>,
}

impl BannerGuard {
    /// Starts the tap. `None` when it cannot be made (no Accessibility grant, no thread): the caller leaves the
    /// banner an ordinary panel.
    pub fn start() -> Option<BannerGuard> {
        let shared = Arc::new(Shared {
            rect: Mutex::new(None),
            guard: Mutex::new(Guard::default()),
            port: AtomicPtr::new(std::ptr::null_mut()),
            pid: i64::from(std::process::id()),
            posted_over: AtomicU64::new(0),
            real_over: AtomicU64::new(0),
        });
        let (tx, rx) = sync_channel::<Option<RunLoopHandle>>(1);
        let on_thread = Arc::clone(&shared);
        let join = std::thread::Builder::new()
            .name("swapdesk-banner-guard".to_owned())
            .spawn(move || run_tap(&on_thread, &tx))
            .ok()?;
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Some(run_loop)) => Some(BannerGuard { shared, run_loop, join: Some(join) }),
            Ok(None) => {
                let _ = join.join();
                None
            }
            // The thread did not report: it ends by itself when it finds nobody listening.
            Err(_) => None,
        }
    }

    /// Where the banner is now (see `model::banner_cg_rect`), or `None` when it is not on the screen.
    pub fn set_rect(&self, rect: Option<[f64; 4]>) {
        *lock(&self.shared.rect) = rect;
    }

    /// Mouse events seen over the banner so far: the controller's, and a real device's.
    pub fn counts(&self) -> (u64, u64) {
        (self.shared.posted_over.load(Ordering::Relaxed), self.shared.real_over.load(Ordering::Relaxed))
    }
}

impl Drop for BannerGuard {
    fn drop(&mut self) {
        let Some(join) = self.join.take() else { return };
        // A stop that comes before the thread entered its run loop is lost, so it is asked again until the thread
        // has ended. The main thread calls this: it never waits longer than half a second (a thread that did not
        // end by then ends with the next stop of its loop, or with the process).
        for _ in 0..50 {
            self.run_loop.0.stop();
            if join.is_finished() {
                let _ = join.join();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
