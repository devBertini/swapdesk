//! One interface per Windows session (F-SYS-01): the first `Swapdesk.exe` holds the named mutex
//! `Local\Swapdesk-ui` (`Local\` is per session, so another signed-in user has their own); a second start finds it
//! taken, brings the first one's window forward ([`bring_first_forward`]) and exits.

use std::ffi::OsStr;
use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM};
use windows_sys::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetClassNameW, GetWindowLongW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow, WS_EX_TOOLWINDOW,
};

use crate::sys::{Result, last_error, wide};

/// The window class `swapdesk-shell-win` registers (`window.rs`, `CLASS_NAME`); a test in `swapdesk-app` keeps the two equal.
pub const WINDOW_CLASS: &str = "SwapdeskWindow";

/// The mutex name; a development `--instance <n>` gets its own so several can run side by side.
pub fn mutex_name(instance: Option<u32>) -> String {
    match instance {
        Some(n) => format!("Local\\Swapdesk-ui-{n}"),
        None => "Local\\Swapdesk-ui".to_owned(),
    }
}

/// Held for the life of the first interface; dropping it frees the name.
pub struct SingleInstance(HANDLE);

// SAFETY: a mutex handle may be closed from any thread; the type has no other use.
unsafe impl Send for SingleInstance {}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateMutexW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The answer of [`claim`].
pub enum Claim {
    /// This process is the interface of the session; keep the value alive.
    First(SingleInstance),
    /// Another interface runs already.
    Second,
}

/// Takes the session's interface name. An error means the mutex could not be made at all.
pub fn claim(instance: Option<u32>) -> Result<Claim> {
    let name = wide(OsStr::new(&mutex_name(instance)))?;
    // SAFETY: `name` is NUL-terminated and outlives the call; no security attributes, not initially owned.
    let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(last_error("CreateMutexW"));
    }
    // SAFETY: read straight after the call that sets it.
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if exists {
        // SAFETY: the handle is ours and is not used again.
        unsafe { CloseHandle(handle) };
        Ok(Claim::Second)
    } else {
        Ok(Claim::First(SingleInstance(handle)))
    }
}

/// One top-level window of the Swapdesk class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub hwnd: isize,
    pub visible: bool,
    pub tool_window: bool,
    pub minimized: bool,
}

/// The window to bring forward: an ordinary (not tool) window, a visible one first; the banner of a host session is a
/// tool window and is never chosen.
pub fn pick_window(candidates: &[Candidate]) -> Option<Candidate> {
    candidates.iter().copied().filter(|c| !c.tool_window).min_by_key(|c| u8::from(!c.visible))
}

struct Scan {
    own_pid: u32,
    found: Vec<Candidate>,
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> i32 {
    // SAFETY: `lparam` is the `&mut Scan` that `bring_first_forward` passed and keeps alive for the enumeration.
    let scan = unsafe { &mut *(lparam as *mut Scan) };
    let mut class = [0u16; 64];
    // SAFETY: the buffer holds 64 units, the length passed.
    let n = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), 64) };
    let n = usize::try_from(n).unwrap_or(0);
    if String::from_utf16_lossy(&class[..n]) != WINDOW_CLASS {
        return 1;
    }
    let mut pid = 0u32;
    // SAFETY: `hwnd` came from EnumWindows; `pid` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == scan.own_pid {
        return 1;
    }
    // SAFETY: plain queries on a window handle EnumWindows just gave.
    let (visible, ex, minimized) =
        unsafe { (IsWindowVisible(hwnd) != 0, GetWindowLongW(hwnd, GWL_EXSTYLE) as u32, IsIconic(hwnd) != 0) };
    scan.found.push(Candidate { hwnd: hwnd as isize, visible, tool_window: ex & WS_EX_TOOLWINDOW != 0, minimized });
    1
}

/// Restores and raises the first interface's window; false when none was found or Windows refused the focus.
pub fn bring_first_forward() -> bool {
    // SAFETY: GetCurrentProcessId has no preconditions.
    let mut scan = Scan { own_pid: unsafe { GetCurrentProcessId() }, found: Vec::new() };
    // SAFETY: `collect` only dereferences `lparam` as the `Scan` that lives across this call.
    unsafe { EnumWindows(Some(collect), &mut scan as *mut Scan as LPARAM) };
    let Some(c) = pick_window(&scan.found) else { return false };
    let hwnd = c.hwnd as HWND;
    // SAFETY: the handle was found a moment ago; a window that vanished makes these calls fail harmlessly.
    unsafe {
        if c.minimized {
            ShowWindow(hwnd, SW_RESTORE);
        } else if !c.visible {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(hwnd: isize, visible: bool, tool: bool) -> Candidate {
        Candidate { hwnd, visible, tool_window: tool, minimized: false }
    }

    #[test]
    fn the_mutex_is_per_session_and_per_development_instance() {
        assert_eq!(mutex_name(None), "Local\\Swapdesk-ui");
        assert_eq!(mutex_name(Some(3)), "Local\\Swapdesk-ui-3");
    }

    #[test]
    fn the_banner_is_never_brought_forward_and_a_visible_window_wins() {
        assert_eq!(pick_window(&[]), None);
        assert_eq!(pick_window(&[c(1, true, true)]), None);
        assert_eq!(pick_window(&[c(1, true, true), c(2, false, false), c(3, true, false)]).map(|x| x.hwnd), Some(3));
        assert_eq!(pick_window(&[c(1, true, true), c(2, false, false)]).map(|x| x.hwnd), Some(2));
    }

    #[test]
    fn a_second_claim_in_the_same_session_is_refused_until_the_first_ends() {
        // A name no real run uses: an instance number far above any development use.
        let n = Some(4_000_000_000 + std::process::id() % 1000);
        let Ok(Claim::First(first)) = claim(n) else { panic!("the first claim must win") };
        assert!(matches!(claim(n), Ok(Claim::Second)));
        drop(first);
        assert!(matches!(claim(n), Ok(Claim::First(_))));
    }
}
