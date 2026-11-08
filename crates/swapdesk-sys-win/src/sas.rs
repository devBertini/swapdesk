//! `SendSAS(FALSE)` (research windows.md 5b): the Secure Attention Sequence (Ctrl+Alt+Del) sent by the service for an
//! `UNATTENDED` session. `SendSAS` lives in `sas.dll`, which is loaded at run time from System32 only
//! (`LOAD_LIBRARY_SEARCH_SYSTEM32`), so no import library is needed and no other folder can supply the DLL.
//!
//! The call is honoured only from a service process (LocalSystem); from any other process it does nothing or fails, so
//! [`send_sas`] is exercised only when the service is installed. Not re-read on Microsoft Learn today (outside this stage's
//! reading list): the `FALSE` argument means "the caller is a service" (`AsUser = FALSE`).

use std::ffi::OsStr;
use std::ptr::null_mut;

use swapdesk_platform::Result;
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use crate::sys::{last_error, wide};

type SendSasFn = unsafe extern "system" fn(as_user: i32);

struct Library(HMODULE);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the module came from LoadLibraryExW and is released once.
        unsafe { FreeLibrary(self.0) };
    }
}

fn load_send_sas() -> Result<(Library, SendSasFn)> {
    let name = wide(OsStr::new("sas.dll"))?;
    // SAFETY: `name` is a null-terminated wide string; no file handle; the flag restricts the search to System32.
    let module = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(last_error("LoadLibraryExW(sas.dll)"));
    }
    let lib = Library(module);
    // SAFETY: `module` is a loaded module; the name is a null-terminated ANSI string.
    let Some(proc) = (unsafe { GetProcAddress(lib.0, c"SendSAS".as_ptr().cast()) }) else {
        return Err(last_error("GetProcAddress(SendSAS)"));
    };
    // SAFETY: `SendSAS` is `void SendSAS(BOOL AsUser)` with the system calling convention; the cast changes only the pointer's type.
    let f = unsafe { core::mem::transmute::<unsafe extern "system" fn() -> isize, SendSasFn>(proc) };
    Ok((lib, f))
}

/// Whether `sas.dll` loads from System32 and exports `SendSAS` (does not send anything).
pub fn sas_available() -> bool {
    load_send_sas().is_ok()
}

/// Sends the Secure Attention Sequence with `AsUser = FALSE`. Only a service may do this.
pub fn send_sas() -> Result<()> {
    let (lib, send) = load_send_sas()?;
    // SAFETY: `send` points into `lib`, which stays loaded until after the call. `FALSE` is the service form.
    unsafe { send(0) };
    drop(lib);
    Ok(())
}
