//! Starting a program as an administrator through UAC (ELEVATION.md 3.5): `ShellExecuteExW` with the `runas` verb,
//! and a handle to the process it started.
//!
//! The call runs on a thread of its own: the shell wants COM initialised as a single-threaded apartment on the thread
//! that calls it, and a caller's thread may already hold another apartment. Whether the call returns before or after
//! the person answers the prompt is not documented; the caller does not depend on it, because the service decides how
//! an elevation ended and the exit code only says why.

use std::ffi::OsStr;
use std::path::Path;
use std::ptr::null_mut;
use std::time::Duration;

use swapdesk_platform::PlatformError;
use windows_sys::Win32::Foundation::{ERROR_CANCELLED, GetLastError, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::sys::{OwnedHandle, invalid, os_error, wide};

/// Why the program was not started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAsError {
    /// The person answered "No" at the prompt, or closed the credentials prompt (`ERROR_CANCELLED`).
    Declined,
    /// Any other failure of the call or of what it needs.
    Os(PlatformError),
}

impl core::fmt::Display for RunAsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RunAsError::Declined => f.write_str("the elevation prompt was declined"),
            RunAsError::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RunAsError {}

/// How a wait on the elevated process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The process ended; its exit code when the system gave it.
    Ended { exit_code: Option<u32> },
    /// The time passed first.
    TimedOut,
}

/// The process `ShellExecuteExW` started, when it gave a handle to it. The handle closes on drop.
pub struct ElevatedProcess {
    /// None when the call started something that is not a process of its own: `hProcess` came back NULL.
    process: Option<OwnedHandle>,
}

impl ElevatedProcess {
    /// Waits up to `timeout` for the process to end and reads its exit code. A NULL handle gives `Ended` at once
    /// with no code: there is nothing to wait for. The handle of an elevated process may lack the right to read the
    /// exit code (the design does not depend on it): that also gives no code.
    pub fn wait(&self, timeout: Duration) -> WaitOutcome {
        let Some(process) = &self.process else { return WaitOutcome::Ended { exit_code: None } };
        // INFINITE is u32::MAX: one millisecond less keeps a long timeout finite.
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1).min(u32::MAX - 1);
        // SAFETY: `process.0` is a live process handle that SEE_MASK_NOCLOSEPROCESS gave us and that stays open until drop.
        match unsafe { WaitForSingleObject(process.0, ms) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: as above; `code` is a live out-parameter.
                let ok = unsafe { GetExitCodeProcess(process.0, &mut code) };
                WaitOutcome::Ended { exit_code: (ok != 0).then_some(code) }
            }
            WAIT_TIMEOUT => WaitOutcome::TimedOut,
            // WAIT_FAILED or an abandoned object: the wait told nothing about the process.
            _ => WaitOutcome::Ended { exit_code: None },
        }
    }

    #[cfg(test)]
    fn from_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> Self {
        Self { process: Some(OwnedHandle(handle)) }
    }
}

/// The error of a failed `ShellExecuteExW` with this `GetLastError` value.
pub fn run_as_error(code: u32) -> RunAsError {
    if code == ERROR_CANCELLED { RunAsError::Declined } else { RunAsError::Os(os_error("ShellExecuteExW", code)) }
}

/// Starts `exe` with `params` (the command line after the program name, already quoted) elevated: the system asks
/// for consent, or for an administrator's credentials, and the program runs as an administrator. `owner` is the
/// window the prompt belongs to (an `HWND` value), none for no owner. Returns when the call returns.
pub fn run_as_admin(exe: &Path, params: &str, owner: Option<isize>) -> Result<ElevatedProcess, RunAsError> {
    let file = wide(exe.as_os_str()).map_err(RunAsError::Os)?;
    let params = wide(OsStr::new(params)).map_err(RunAsError::Os)?;
    let dir = exe.parent().map(|p| wide(p.as_os_str())).transpose().map_err(RunAsError::Os)?;
    let verb = wide(OsStr::new("runas")).map_err(RunAsError::Os)?;
    std::thread::Builder::new()
        .name("swapdesk-runas".into())
        .spawn(move || execute(&verb, &file, &params, dir.as_deref(), owner))
        .map_err(|e| RunAsError::Os(invalid("run_as_admin", format!("thread: {e}"))))?
        .join()
        .unwrap_or_else(|_| Err(RunAsError::Os(invalid("run_as_admin", "the launch thread panicked"))))
}

/// The call itself, on the thread that initialised COM for it.
fn execute(
    verb: &[u16],
    file: &[u16],
    params: &[u16],
    dir: Option<&[u16]>,
    owner: Option<isize>,
) -> Result<ElevatedProcess, RunAsError> {
    // SAFETY: a thread that has not initialised COM yet; the reserved argument is null as documented.
    let hr = unsafe { CoInitializeEx(core::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) };
    if hr < 0 {
        return Err(RunAsError::Os(invalid("CoInitializeEx", format!("HRESULT {hr:#x}"))));
    }
    // SAFETY: plain data; zeroed is valid (null pointers, zero numbers).
    let mut info: SHELLEXECUTEINFOW = unsafe { core::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.hwnd = owner.map_or(null_mut(), |h| h as HWND);
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.lpDirectory = dir.map_or(core::ptr::null(), |d| d.as_ptr());
    info.nShow = SW_SHOWNORMAL;
    // SAFETY: `info` is filled as documented; `verb`, `file`, `params` and `dir` are null-terminated wide strings that
    // outlive the call; COM is initialised on this thread (the success of CoInitializeEx is balanced below).
    let ok = unsafe { ShellExecuteExW(&mut info) };
    let result = if ok == 0 {
        // SAFETY: GetLastError has no preconditions; read before any other call can replace the value.
        Err(run_as_error(unsafe { GetLastError() }))
    } else {
        // `hProcess` is NULL when nothing of its own was started; a handle is ours to close (OwnedHandle does).
        let process = (!info.hProcess.is_null()).then(|| OwnedHandle(info.hProcess));
        Ok(ElevatedProcess { process })
    };
    // SAFETY: balances the successful CoInitializeEx above, on the same thread.
    unsafe { CoUninitialize() };
    result
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};

    use super::*;

    fn handle_of(pid: u32) -> ElevatedProcess {
        // SAFETY: a plain query of a process this test started.
        let h = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        assert!(!h.is_null(), "OpenProcess failed");
        ElevatedProcess::from_handle(h)
    }

    #[test]
    fn error_cancelled_is_declined_and_any_other_code_is_an_os_error() {
        assert_eq!(run_as_error(1223), RunAsError::Declined);
        match run_as_error(5) {
            RunAsError::Os(e) => assert_eq!(e.os_code, Some(5)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wait_reads_the_exit_code_of_an_ended_process() {
        let mut child = Command::new("cmd").args(["/c", "exit 3"]).stdout(Stdio::null()).spawn().unwrap();
        let process = handle_of(child.id());
        assert_eq!(process.wait(Duration::from_secs(20)), WaitOutcome::Ended { exit_code: Some(3) });
        let _ = child.wait();
    }

    #[test]
    fn wait_times_out_on_a_running_process_and_a_null_handle_ends_at_once() {
        let mut child = Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
        let process = handle_of(child.id());
        assert_eq!(process.wait(Duration::from_millis(50)), WaitOutcome::TimedOut);
        child.kill().unwrap();
        let _ = child.wait();
        assert_eq!(
            ElevatedProcess { process: None }.wait(Duration::from_secs(60)),
            WaitOutcome::Ended { exit_code: None }
        );
    }
}
