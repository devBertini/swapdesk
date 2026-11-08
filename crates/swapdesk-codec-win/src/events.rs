//! The event loop of an asynchronous (hardware) MFT: a callback on `IMFMediaEventGenerator::BeginGetEvent` counts
//! `METransformNeedInput` and `METransformHaveOutput` events; the encoder thread waits on them with a timeout.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use swapdesk_platform::{ErrorKind, Result};
use windows::Win32::Foundation::{E_NOINTERFACE, E_NOTIMPL, E_POINTER, S_OK};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncCallback_Vtbl, IMFAsyncResult, IMFMediaEventGenerator, MEError, METransformDrainComplete,
    METransformHaveOutput, METransformNeedInput,
};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

use crate::sys::Ctx;

#[derive(Default)]
struct State {
    need_input: u32,
    have_output: u32,
    drained: bool,
    /// HRESULT of an `MEError` event.
    error: Option<i32>,
    stopped: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
    /// The MFT the callback asks for events. The MFT holds the callback through the pending `BeginGetEvent`, so a
    /// strong reference here would be a cycle that frees neither: `EventPump::drop` takes it out, and `invoke` works
    /// on a clone it takes under the lock.
    generator: Mutex<Option<Generator>>,
}

/// The event generator of an MFT, which Media Foundation calls from its own work-queue threads.
struct Generator(IMFMediaEventGenerator);

// SAFETY: `IMFMediaEventGenerator::BeginGetEvent` and `EndGetEvent` are thread-safe by contract (the event queue is
// built for a callback on another thread), and the reference is only cloned and released, both atomic.
unsafe impl Send for Generator {}
// SAFETY: as above.
unsafe impl Sync for Generator {}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A hand-written `IMFAsyncCallback` COM object: the `implement` macro needs a direct `windows-core` dependency, which
/// is not a workspace dependency.
#[repr(C)]
struct RawCallback {
    vtbl: &'static IMFAsyncCallback_Vtbl,
    refs: AtomicU32,
    shared: Arc<Shared>,
}

static VTBL: IMFAsyncCallback_Vtbl = IMFAsyncCallback_Vtbl {
    base__: IUnknown_Vtbl { QueryInterface: query_interface, AddRef: add_ref, Release: release },
    GetParameters: get_parameters,
    Invoke: invoke,
};

unsafe extern "system" fn query_interface(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: COM passes valid pointers; `this` is a RawCallback created by `new_callback`.
    unsafe {
        let iid = &*iid;
        if *iid == IUnknown::IID || *iid == IMFAsyncCallback::IID {
            *out = this;
            add_ref(this);
            S_OK
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live RawCallback.
    unsafe { (*(this as *const RawCallback)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live RawCallback; the last release frees the Box made in `new_callback`.
    unsafe {
        let left = (*(this as *const RawCallback)).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this as *mut RawCallback));
        }
        left
    }
}

unsafe extern "system" fn get_parameters(_this: *mut c_void, _flags: *mut u32, _queue: *mut u32) -> HRESULT {
    E_NOTIMPL
}

/// The MF work queue calls this on its own thread: a panic must never unwind into Media Foundation (W1.3). A caught
/// panic is reported to the waiting thread as an error, so the encoder or decoder fails instead of hanging. Release
/// builds use `panic = "abort"` (D15), where this guard only matters to unwind builds (tests, development).
unsafe extern "system" fn invoke(this: *mut c_void, result: *mut c_void) -> HRESULT {
    // SAFETY: forwarded unchanged; the same contract as this function's.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { invoke_body(this, result) })) {
        Ok(hr) => hr,
        Err(_) => {
            // SAFETY: `this` is a live RawCallback for the duration of the call.
            let me = unsafe { &*(this as *const RawCallback) };
            me.shared.lock().error = Some(windows::Win32::Foundation::E_UNEXPECTED.0);
            me.shared.cv.notify_all();
            S_OK
        }
    }
}

unsafe fn invoke_body(this: *mut c_void, result: *mut c_void) -> HRESULT {
    // SAFETY: `this` is a live RawCallback; `result` is the async result of our request, borrowed for the call.
    unsafe {
        let me = &*(this as *const RawCallback);
        let Some(result) = IMFAsyncResult::from_raw_borrowed(&result) else { return E_POINTER };
        // The pump was dropped: the request ends here and nothing is asked again.
        let Some(generator) =
            me.shared.generator.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|g| g.0.clone())
        else {
            me.shared.cv.notify_all();
            return S_OK;
        };
        let mut again = true;
        match generator.EndGetEvent(result) {
            Ok(event) => {
                let ty = event.GetType().unwrap_or(0);
                let mut st = me.shared.lock();
                if ty == METransformNeedInput.0 as u32 {
                    st.need_input += 1;
                } else if ty == METransformHaveOutput.0 as u32 {
                    st.have_output += 1;
                } else if ty == METransformDrainComplete.0 as u32 {
                    st.drained = true;
                } else if ty == MEError.0 as u32 {
                    st.error = Some(event.GetStatus().map(|h| h.0).unwrap_or(-1));
                }
                if st.stopped {
                    again = false;
                }
                drop(st);
            }
            Err(e) => {
                let mut st = me.shared.lock();
                // A shut-down queue ends the loop; anything else is an error for the waiting thread.
                if !st.stopped {
                    st.error = Some(e.code().0);
                }
                again = false;
            }
        }
        me.shared.cv.notify_all();
        if again {
            let this_ptr = this;
            if let Some(cb) = IMFAsyncCallback::from_raw_borrowed(&this_ptr) {
                let _ = generator.BeginGetEvent(cb, None);
            }
        }
        S_OK
    }
}

fn new_callback(shared: Arc<Shared>) -> IMFAsyncCallback {
    let raw = Box::new(RawCallback { vtbl: &VTBL, refs: AtomicU32::new(1), shared });
    // SAFETY: the pointer is a COM object whose first field is the vtable pointer; the interface takes over the one
    // reference created above.
    unsafe { IMFAsyncCallback::from_raw(Box::into_raw(raw).cast()) }
}

/// Counts the events of one async MFT.
pub(crate) struct EventPump {
    shared: Arc<Shared>,
}

impl EventPump {
    pub(crate) fn start(generator: IMFMediaEventGenerator) -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            generator: Mutex::new(Some(Generator(generator.clone()))),
        });
        let callback = new_callback(shared.clone());
        // SAFETY: the callback lives as long as the pending request holds it.
        unsafe { generator.BeginGetEvent(&callback, None) }.ctx("IMFMediaEventGenerator::BeginGetEvent")?;
        Ok(Self { shared })
    }

    fn check(st: &State) -> Result<()> {
        match st.error {
            Some(code) => Err(swapdesk_platform::PlatformError {
                kind: ErrorKind::Os,
                op: "MFT event MEError",
                os_code: Some(i64::from(code)),
                detail: String::new(),
            }),
            None => Ok(()),
        }
    }

    /// Waits for a free input slot (`METransformNeedInput`), consuming it. `Ok(false)` on timeout.
    pub(crate) fn take_need_input(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            Self::check(&st)?;
            if st.need_input > 0 {
                st.need_input -= 1;
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            st = self.shared.cv.wait_timeout(st, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// Waits for a finished output (`METransformHaveOutput`), consuming it. `Ok(false)` on timeout.
    pub(crate) fn take_have_output(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            if st.have_output > 0 {
                st.have_output -= 1;
                return Ok(true);
            }
            Self::check(&st)?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            st = self.shared.cv.wait_timeout(st, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// Waits for `METransformDrainComplete` (after `MFT_MESSAGE_COMMAND_DRAIN`). Outputs may arrive first.
    pub(crate) fn wait_drained(&self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut st = self.shared.lock();
        loop {
            Self::check(&st)?;
            if st.drained {
                st.drained = false;
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            st = self.shared.cv.wait_timeout(st, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }

    /// Outputs announced but not yet taken.
    pub(crate) fn pending_outputs(&self) -> u32 {
        self.shared.lock().have_output
    }

    /// Ends the loop and lets go of the MFT, so that releasing the MFT frees it and, with it, the callback. Idempotent.
    pub(crate) fn stop(&self) {
        self.shared.lock().stopped = true;
        let generator = self.shared.generator.lock().unwrap_or_else(|p| p.into_inner()).take();
        drop(generator);
        self.shared.cv.notify_all();
    }
}

impl Drop for EventPump {
    fn drop(&mut self) {
        self.stop();
    }
}
