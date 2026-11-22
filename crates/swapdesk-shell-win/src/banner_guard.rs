//! The host banner belongs to the person at this computer. Whoever controls the computer from afar must not see it,
//! must not be able to press its buttons ("Desconectar", "Inverter") and must not find it in the way of a click meant
//! for what lies under it.
//!
//! * **The picture.** The banner's window is kept out of screen captures (`WDA_EXCLUDEFROMCAPTURE`, `window.rs`).
//! * **The pointer.** The window is layered with `WS_EX_TRANSPARENT`, so the system hands every mouse event, whatever
//!   its origin, to the window underneath (Microsoft Learn, "Layered Windows": "the shape of the layered window will
//!   be ignored and the mouse events will be passed to other windows underneath the layered window"). The controller's
//!   clicks therefore land where they would if there were no banner. Its keys never reach the banner either: the
//!   window never takes the focus (`WS_EX_NOACTIVATE`).
//! * **The person at the computer** still uses the banner. A low-level mouse hook (`WH_MOUSE_LL`) sees every mouse
//!   event before it is delivered, with whether it was injected (`MSLLHOOKSTRUCT::flags`, `LLMHF_INJECTED`: "the
//!   event-injected (from any process) flag"; the controller's input arrives through `SendInput`). An event of a real
//!   device inside the banner's rectangle is handed to the banner (posted to its window as [`WM_BANNER_POINTER`]), and
//!   a click of the left button there is not passed on ("it may return a nonzero value to prevent the system from
//!   passing the message to ... the target window procedure", LowLevelMouseProc). An injected event is never touched.
//! * **The hook's thread** does nothing else, so it always answers at once: the system calls the hook "in the context
//!   of the thread that installed it", that thread "must have a message loop", and a hook that answers late "is
//!   silently removed without being called. There is no way for the application to know whether the hook is removed"
//!   (same page). Against that last sentence the hook is installed again every few seconds: the banner is the way to
//!   end a session, and it must not stay out of reach.
//!
//! What decides whose an event is (`swapdesk_paint::banner_guard::Guard`) holds no system call, is shared with the Mac
//! shell and is tested by itself.

use std::ffi::c_void;
use std::sync::Mutex;

use swapdesk_paint::banner_guard::{Guard, MouseEvent, Verdict};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, KillTimer, LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED, MSG,
    MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostMessageW, PostThreadMessageW, SetTimer, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_MOUSE_LL, WM_APP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_QUIT, WM_TIMER, WM_USER,
};

/// Posted to the banner's window for a real pointer over it: `wParam` is one of the `POINTER_*` kinds, `lParam` the
/// point in the window's client pixels (x in the low word, y in the high word).
pub const WM_BANNER_POINTER: u32 = WM_APP + 61;
pub const POINTER_HOVER: usize = 0;
pub const POINTER_LEAVE: usize = 1;
pub const POINTER_PRESS: usize = 2;
/// The left button went up over the banner after a press on it: a click.
pub const POINTER_RELEASE: usize = 3;
/// The left button went up away from the banner after a press on it: no click.
pub const POINTER_RELEASE_OUTSIDE: usize = 4;

/// How often the hook is installed again (see the module text).
const REINSTALL_MS: u32 = 5_000;

struct State {
    /// The banner's window, as an integer so the state can sit in a static.
    hwnd: isize,
    /// The banner's rectangle in screen pixels: left, top, right, bottom.
    rect: (i32, i32, i32, i32),
    guard: Guard,
}

/// The one banner of this process, if it is up: the hook procedure has no other way to reach it.
static STATE: Mutex<Option<State>> = Mutex::new(None);

fn state() -> std::sync::MutexGuard<'static, Option<State>> {
    STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the hook does with one event at screen point (`x`, `y`): true when the event stops here.
fn deliver(event: MouseEvent, injected: bool, x: i32, y: i32) -> bool {
    let mut g = state();
    let Some(st) = g.as_mut() else { return false };
    let (l, t, r, b) = st.rect;
    let inside = x >= l && x < r && y >= t && y < b;
    let verdict = st.guard.on_event(event, injected, inside);
    let kind = match verdict {
        Verdict::Pass => return false,
        Verdict::Hover => POINTER_HOVER,
        Verdict::Leave => POINTER_LEAVE,
        Verdict::Press => POINTER_PRESS,
        Verdict::Release { inside: true } => POINTER_RELEASE,
        Verdict::Release { inside: false } => POINTER_RELEASE_OUTSIDE,
    };
    let point = pack(x - l, y - t);
    // SAFETY: a message posted to the banner's own window; a window that is gone refuses it.
    let _ = unsafe { PostMessageW(Some(HWND(st.hwnd as *mut c_void)), WM_BANNER_POINTER, WPARAM(kind), LPARAM(point)) };
    verdict.swallows()
}

/// A client point as an `lParam`: x in the low word, y in the high word (both fit: the banner is small).
pub(crate) fn pack(x: i32, y: i32) -> isize {
    ((y as u16 as isize) << 16) | (x as u16 as isize)
}

/// The point of a [`WM_BANNER_POINTER`] message.
pub fn unpack(lparam: isize) -> (i32, i32) {
    (i32::from(lparam as u16 as i16), i32::from((lparam >> 16) as u16 as i16))
}

/// `LowLevelMouseProc`. Never unwinds into the system.
unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let event = match wparam.0 as u32 {
            WM_MOUSEMOVE => Some(MouseEvent::Move),
            WM_LBUTTONDOWN => Some(MouseEvent::LeftDown),
            WM_LBUTTONUP => Some(MouseEvent::LeftUp),
            _ => None,
        };
        if let Some(event) = event {
            // SAFETY: with `HC_ACTION` the system passes a pointer to an `MSLLHOOKSTRUCT` that lives for the call.
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let injected = info.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
            let (x, y) = (info.pt.x, info.pt.y);
            if std::panic::catch_unwind(|| deliver(event, injected, x, y)).unwrap_or(false) {
                return LRESULT(1);
            }
        }
    }
    // SAFETY: the next hook of the chain, with the arguments as they came.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn install() -> Option<HHOOK> {
    // SAFETY: a low-level hook of this process's own module, on the calling thread.
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), Some(HINSTANCE(module.0)), 0).ok()
    }
}

/// The hook's thread: installs the hook, installs it again every [`REINSTALL_MS`], and ends on `WM_QUIT`.
fn hook_thread(started: std::sync::mpsc::Sender<Option<u32>>) {
    // SAFETY: plain Win32 calls on this thread: its id, its message queue, its hook and its timer.
    unsafe {
        let mut msg = MSG::default();
        // The thread has a message queue before anyone posts to it.
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let mut hook = install();
        let _ = started.send(hook.is_some().then(|| GetCurrentThreadId()));
        if hook.is_none() {
            return;
        }
        let timer = SetTimer(None, 0, REINSTALL_MS, None);
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.message == WM_TIMER {
                // The new one first, so no event falls between the two.
                let fresh = install();
                if fresh.is_some()
                    && let Some(old) = std::mem::replace(&mut hook, fresh)
                {
                    let _ = UnhookWindowsHookEx(old);
                }
            }
        }
        let _ = KillTimer(None, timer);
        if let Some(h) = hook {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

/// The banner's hook while the banner is up; dropping it removes the hook.
pub struct BannerGuard {
    thread_id: u32,
    join: Option<std::thread::JoinHandle<()>>,
}

impl BannerGuard {
    /// Starts watching the real pointer for the banner `hwnd`. `None` when the hook could not be installed: the
    /// caller then leaves the banner a plain window that takes the mouse itself.
    pub fn start(hwnd: HWND) -> Option<BannerGuard> {
        *state() = Some(State { hwnd: hwnd.0 as isize, rect: (0, 0, 0, 0), guard: Guard::default() });
        let (tx, rx) = std::sync::mpsc::channel();
        let join = std::thread::Builder::new().name("banner-pointer".into()).spawn(move || hook_thread(tx)).ok()?;
        match rx.recv().ok().flatten() {
            Some(thread_id) => Some(BannerGuard { thread_id, join: Some(join) }),
            None => {
                *state() = None;
                let _ = join.join();
                None
            }
        }
    }

    /// The banner's window moved or changed size: `rect` is its rectangle in screen pixels.
    pub fn set_rect(&self, rect: RECT) {
        if let Some(st) = state().as_mut() {
            st.rect = (rect.left, rect.top, rect.right, rect.bottom);
        }
    }
}

impl Drop for BannerGuard {
    fn drop(&mut self) {
        *state() = None;
        // SAFETY: a message to the hook's own thread, which ends its loop on it.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_survives_the_trip_through_the_message() {
        for (x, y) in [(0, 0), (480, 55), (-3, 70), (12, -1)] {
            assert_eq!(unpack(pack(x, y)), (x, y));
        }
    }
}
