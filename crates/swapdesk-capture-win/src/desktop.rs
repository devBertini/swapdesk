//! Following the input desktop on the calling thread (windows.md 1d).
//!
//! `SetThreadDesktop` fails when the thread owns windows or hooks, so the capture thread owns none (C2.1); the
//! `EVENT_SYSTEM_DESKTOPSWITCH` hook lives on the watcher's own thread.

use crate::errors::{self, Phase};
use swapdesk_platform::Result;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop, HDESK, OpenInputDesktop,
    SetThreadDesktop,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

/// `GENERIC_ALL`: the access the thread's desktop handle is opened with.
const GENERIC_ALL: u32 = 0x1000_0000;

/// The desktop handle this thread was last moved to; the thread's original desktop is restored on drop.
pub struct InputDesktop {
    current: Option<HDESK>,
    original: Option<HDESK>,
}

impl InputDesktop {
    pub fn new() -> Self {
        Self { current: None, original: None }
    }

    /// OpenInputDesktop + SetThreadDesktop on the calling thread. On the secure desktop only a LocalSystem process may
    /// open it: the error is `Unavailable` and the thread stays where it was.
    pub fn follow(&mut self) -> Result<()> {
        // SAFETY: plain query; the returned handle belongs to the process and is not closed by us.
        let original = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.ok();
        // SAFETY: no pointer arguments; the handle is closed below or kept in `self.current`.
        let new = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL)) }
            .map_err(|e| errors::from_hresult(Phase::Duplicate, "OpenInputDesktop", e.code().0, &e.message()))?;
        // SAFETY: `new` is a live desktop handle just opened.
        if let Err(e) = unsafe { SetThreadDesktop(new) } {
            // SAFETY: `new` is ours and no thread uses it.
            let _ = unsafe { CloseDesktop(new) };
            return Err(errors::from_hresult(
                Phase::Duplicate,
                "SetThreadDesktop",
                e.code().0,
                &format!("{} (the thread must own no windows or hooks)", e.message()),
            ));
        }
        if self.original.is_none() {
            self.original = original;
        }
        if let Some(old) = self.current.replace(new) {
            // SAFETY: the thread moved off `old`; it is ours.
            let _ = unsafe { CloseDesktop(old) };
        }
        Ok(())
    }
}

impl Default for InputDesktop {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for InputDesktop {
    fn drop(&mut self) {
        if let Some(cur) = self.current.take() {
            if let Some(orig) = self.original {
                // SAFETY: puts the thread back on the desktop it started on, then closes the handle it was moved to.
                let _ = unsafe { SetThreadDesktop(orig) };
            }
            // SAFETY: ours; fails harmlessly when the thread could not leave it (the handle then lives to exit).
            let _ = unsafe { CloseDesktop(cur) };
        }
    }
}
