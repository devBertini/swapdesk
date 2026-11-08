//! Process-exclusion loopback (windows.md 6b): `ActivateAudioInterfaceAsync` on the virtual process-loopback device
//! with `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`.
//!
//! The completion handler is a hand-written COM object (the `implement` macro needs a direct `windows-core`
//! dependency, which is not a workspace dependency; the same approach as `swapdesk-codec-win`). It also answers for
//! `IAgileObject`, which `ActivateAudioInterfaceAsync` requires of its handler.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use swapdesk_platform::{ErrorKind, PlatformError};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Media::Audio::{
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Vtbl, IAudioClient,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

use crate::com::{ComGuard, failure, os_error};

type Done = Arc<(Mutex<bool>, Condvar)>;

#[repr(C)]
struct RawHandler {
    vtbl: &'static IActivateAudioInterfaceCompletionHandler_Vtbl,
    refs: AtomicU32,
    done: Done,
}

static VTBL: IActivateAudioInterfaceCompletionHandler_Vtbl = IActivateAudioInterfaceCompletionHandler_Vtbl {
    base__: IUnknown_Vtbl { QueryInterface: query_interface, AddRef: add_ref, Release: release },
    ActivateCompleted: activate_completed,
};

/// IAgileObject {94EA2B94-E9CC-49E0-C0FF-EE64CA8F5B90}: a marker interface with no methods of its own.
const IID_AGILE_OBJECT: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);

unsafe extern "system" fn query_interface(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM passes valid pointers; `this` is a RawHandler made by `new_handler`.
    unsafe {
        let iid = &*iid;
        if *iid == IUnknown::IID || *iid == IActivateAudioInterfaceCompletionHandler::IID || *iid == IID_AGILE_OBJECT {
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
    // SAFETY: `this` is a live RawHandler.
    unsafe { (*(this as *const RawHandler)).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live RawHandler; the last release frees the Box made in `new_handler`.
    unsafe {
        let left = (*(this as *const RawHandler)).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this as *mut RawHandler));
        }
        left
    }
}

unsafe extern "system" fn activate_completed(this: *mut c_void, _operation: *mut c_void) -> HRESULT {
    // SAFETY: `this` is a live RawHandler; the operation is read later by the thread that waits.
    unsafe {
        let me = &*(this as *const RawHandler);
        let (lock, cv) = &*me.done;
        *lock.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
    }
    S_OK
}

fn new_handler(done: Done) -> IActivateAudioInterfaceCompletionHandler {
    let raw = Box::into_raw(Box::new(RawHandler { vtbl: &VTBL, refs: AtomicU32::new(1), done }));
    // SAFETY: the pointer is a COM object with the vtable of this interface and one reference, which the wrapper takes.
    unsafe { IActivateAudioInterfaceCompletionHandler::from_raw(raw.cast()) }
}

/// An `IAudioClient` on the process-loopback virtual device that leaves `pid` and its child processes out. The client
/// supports no `GetMixFormat`: the caller initialises it with an explicit format. Needs COM on the calling thread.
pub(crate) fn activate_exclude_process(pid: u32) -> Result<IAudioClient, PlatformError> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // Never dropped: PROPVARIANT's Drop would PropVariantClear the blob and free `params`, which is on the stack.
    let propvariant = std::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&params as *const AUDIOCLIENT_ACTIVATION_PARAMS).cast_mut().cast(),
                    },
                },
            }),
        },
    });
    let done: Done = Arc::new((Mutex::new(false), Condvar::new()));
    let handler = new_handler(Arc::clone(&done));
    // SAFETY: `propvariant` and `params` outlive the call (the blob is copied by the API before it returns); the
    // handler is a valid agile COM object.
    let op: IActivateAudioInterfaceAsyncOperation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*propvariant as *const PROPVARIANT),
            &handler,
        )
    }
    .map_err(|e| os_error("ActivateAudioInterfaceAsync", &e))?;

    let (lock, cv) = &*done;
    let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    let (guard, _) = cv.wait_timeout_while(guard, Duration::from_secs(5), |d| !*d).unwrap_or_else(|p| p.into_inner());
    if !*guard {
        return Err(failure(ErrorKind::Timeout, "ActivateAudioInterfaceAsync", "no completion within 5 s"));
    }
    drop(guard);

    let mut result = HRESULT(0);
    let mut unknown: Option<IUnknown> = None;
    // SAFETY: the operation completed; the out pointers are valid.
    unsafe { op.GetActivateResult(&mut result, &mut unknown) }.map_err(|e| os_error("GetActivateResult", &e))?;
    if result.is_err() {
        return Err(os_error("ActivateAudioInterfaceAsync(result)", &windows::core::Error::from(result)));
    }
    let unknown = unknown.ok_or_else(|| failure(ErrorKind::Os, "GetActivateResult", "no interface returned"))?;
    unknown.cast::<IAudioClient>().map_err(|e| os_error("QueryInterface(IAudioClient)", &e))
}

/// What the process-exclusion loopback probe found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessLoopbackReport {
    /// The virtual device activated and initialised in the 48 kHz stereo f32 format.
    pub available: bool,
    /// The Windows build number (20348 is the first that has process loopback).
    pub build: Option<u32>,
    /// The failing call and its HRESULT, or "ok".
    pub detail: String,
}

fn windows_build() -> Option<u32> {
    let mut info =
        OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // SAFETY: valid, correctly sized out structure.
    let status = unsafe { RtlGetVersion(&mut info) };
    status.is_ok().then_some(info.dwBuildNumber)
}

/// Opens process-exclusion loopback excluding this process, to learn whether it works here, then closes it. Records
/// nothing and keeps nothing.
pub fn probe_process_loopback() -> ProcessLoopbackReport {
    let build = windows_build();
    let _com = ComGuard::init();
    let detail = match crate::capture::open_process_loopback(std::process::id()) {
        Ok(open) => {
            return ProcessLoopbackReport {
                available: true,
                build,
                detail: format!("ok (initialize attempt {})", open.attempt),
            };
        }
        Err(e) => e.to_string(),
    };
    ProcessLoopbackReport { available: false, build, detail }
}
