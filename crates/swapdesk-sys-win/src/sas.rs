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
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};

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

/// The policy key and value that decide who may simulate a SAS (research windows.md 5c; Microsoft Learn
/// "Policy CSP - ADMX_WinLogon", `SoftwareSASGeneration`, read on 2026-11-12).
pub const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
pub const POLICY_VALUE: &str = "SoftwareSASGeneration";

/// Whether a `SoftwareSASGeneration` value (`None`: not set) lets a service simulate the SAS.
///
/// The Learn page says: set to "Services", services can simulate it; disabled or not configured, only Ease of Access
/// applications on the secure desktop can. It does not give the numbers: the installer writes 1 for "Services"
/// (`install.ps1 -EnableSas`) and this reads 1 as yes, and 3 ("Services and Ease of Access applications", the fourth
/// option) as yes. Both numbers are GUESS until the owner's reboot proof; 0, 2, a missing value and anything else are no.
pub fn policy_lets_services_send(value: Option<u32>) -> bool {
    matches!(value, Some(1 | 3))
}

/// The `SoftwareSASGeneration` value now: `None` when it is not set (or is not a DWORD, or cannot be read).
pub fn sas_policy_value() -> Option<u32> {
    let key = wide(OsStr::new(POLICY_KEY)).ok()?;
    let value = wide(OsStr::new(POLICY_VALUE)).ok()?;
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: both names are null-terminated wide strings; `data` and `size` are live and `size` is the buffer's size;
    // the flag restricts the read to a DWORD, and a missing or differently typed value is an error code, not a read.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (rc == ERROR_SUCCESS).then_some(data)
}

/// Whether the machine's policy lets this service send a SAS now ([`policy_lets_services_send`] of
/// [`sas_policy_value`]). A `false` means a viewer's Ctrl+Alt+Del would do nothing: the service says so, and tells the
/// viewer, instead of sending it.
pub fn sas_policy_allows_service() -> bool {
    policy_lets_services_send(sas_policy_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_services_values_let_a_service_send() {
        assert!(policy_lets_services_send(Some(1)));
        assert!(policy_lets_services_send(Some(3)));
        for no in [None, Some(0), Some(2), Some(4), Some(u32::MAX)] {
            assert!(!policy_lets_services_send(no), "{no:?}");
        }
    }

    #[test]
    fn reading_the_policy_does_not_send_anything() {
        // Reads the registry only; the value on this PC is whatever the owner left (not set by default).
        let _ = sas_policy_value();
    }
}
