//! Test support: a lock across test processes (keyboard focus and the cursor are global), per-monitor DPI awareness, a
//! test window that takes the foreground and records what its message loop receives, and a cursor guard.
//!
//! Safety rule of these tests: input is injected only while the test window is the foreground window (checked before
//! every batch), so nothing is ever typed into another window. A test that cannot get the foreground fails.
#![allow(dead_code)]

use std::cell::RefCell;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    AttachThreadInput, CreateMutexW, GetCurrentThreadId, INFINITE, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ACTIVATE_KEYBOARD_LAYOUT_FLAGS, ActivateKeyboardLayout, GetKeyboardLayout, GetKeyboardLayoutList, HKL,
    LoadKeyboardLayoutW, SetFocus,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetForegroundWindow,
    GetWindowThreadProcessId, MSG, PM_REMOVE, PeekMessageW, RegisterClassW, SW_SHOW, SetCursorPos, SetForegroundWindow,
    ShowWindow, TranslateMessage, WM_CHAR, WM_DEADCHAR, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

/// Held for a test's whole run; other test processes wait.
pub struct Serial(HANDLE);

pub fn serial() -> Serial {
    // SAFETY: a named mutex shared by the test processes of this crate; waited on with no timeout.
    let h = unsafe { CreateMutexW(None, false, w!("Local\\swapdesk-input-win-tests")) }.expect("mutex");
    // SAFETY: valid handle just created.
    unsafe { WaitForSingleObject(h, INFINITE) };
    Serial(h)
}

impl Drop for Serial {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex (it waited on it).
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Per-monitor v2 for the process, so window and cursor coordinates are physical pixels.
pub fn aware() {
    // SAFETY: process-wide setting; fails harmlessly when already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Puts the cursor back where it was when the guard was made.
pub struct CursorGuard(POINT);

impl CursorGuard {
    pub fn new() -> Self {
        let mut p = POINT::default();
        // SAFETY: valid out pointer.
        unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
        Self(p)
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        // SAFETY: plain call.
        let _ = unsafe { SetCursorPos(self.0.x, self.0.y) };
    }
}

pub fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    // SAFETY: valid out pointer.
    unsafe { GetCursorPos(&mut p) }.expect("GetCursorPos");
    (p.x, p.y)
}

/// What the test window's procedure saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ev {
    KeyDown { vk: u32, scan: u8, ext: bool },
    KeyUp { vk: u32, scan: u8, ext: bool },
    Char(u16),
    DeadChar(u16),
    Move { x: i32, y: i32 },
    ButtonDown { button: u8, x: i32, y: i32 },
    ButtonUp { button: u8, x: i32, y: i32 },
    Wheel { delta: i16 },
    HWheel { delta: i16 },
}

thread_local! {
    static LOG: RefCell<Vec<Ev>> = const { RefCell::new(Vec::new()) };
}

fn lo(l: isize) -> i32 {
    i32::from((l & 0xFFFF) as u16 as i16)
}

fn hi(l: isize) -> i32 {
    i32::from(((l >> 16) & 0xFFFF) as u16 as i16)
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let lp = lparam.0;
    let scan = ((lp >> 16) & 0xFF) as u8;
    let ext = (lp >> 24) & 1 == 1;
    let wp = wparam.0;
    let ev = match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => Some(Ev::KeyDown { vk: wp as u32, scan, ext }),
        WM_KEYUP | WM_SYSKEYUP => Some(Ev::KeyUp { vk: wp as u32, scan, ext }),
        WM_CHAR | WM_SYSCHAR => Some(Ev::Char(wp as u16)),
        WM_DEADCHAR => Some(Ev::DeadChar(wp as u16)),
        WM_MOUSEMOVE => Some(Ev::Move { x: lo(lp), y: hi(lp) }),
        WM_LBUTTONDOWN => Some(Ev::ButtonDown { button: 1, x: lo(lp), y: hi(lp) }),
        WM_LBUTTONUP => Some(Ev::ButtonUp { button: 1, x: lo(lp), y: hi(lp) }),
        WM_RBUTTONDOWN => Some(Ev::ButtonDown { button: 2, x: lo(lp), y: hi(lp) }),
        WM_RBUTTONUP => Some(Ev::ButtonUp { button: 2, x: lo(lp), y: hi(lp) }),
        WM_XBUTTONDOWN => Some(Ev::ButtonDown { button: 3 + ((wp >> 16) & 0xFFFF) as u8, x: lo(lp), y: hi(lp) }),
        WM_XBUTTONUP => Some(Ev::ButtonUp { button: 3 + ((wp >> 16) & 0xFFFF) as u8, x: lo(lp), y: hi(lp) }),
        WM_MOUSEWHEEL => Some(Ev::Wheel { delta: ((wp >> 16) & 0xFFFF) as u16 as i16 }),
        WM_MOUSEHWHEEL => Some(Ev::HWheel { delta: ((wp >> 16) & 0xFFFF) as u16 as i16 }),
        _ => None,
    };
    if let Some(ev) = ev {
        LOG.with(|l| l.borrow_mut().push(ev));
    }
    // SAFETY: default handling for every message.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A topmost window on one display that takes the foreground and the keyboard focus.
pub struct TestWindow {
    pub hwnd: HWND,
    /// Client origin in physical virtual-desktop pixels.
    pub origin: (i32, i32),
    pub size: (i32, i32),
    prev_layout: HKL,
}

impl TestWindow {
    /// `at`: top-left in physical virtual-desktop pixels.
    pub fn open(at: (i32, i32), size: (i32, i32)) -> Self {
        // SAFETY: standard class registration and window creation on this thread, which pumps its messages in `pump`.
        unsafe {
            let hinst = GetModuleHandleW(None).expect("module").into();
            let class = w!("SwapdeskInputWinTest");
            let wc =
                WNDCLASSW { lpfnWndProc: Some(proc), hInstance: hinst, lpszClassName: class, ..Default::default() };
            RegisterClassW(&wc); // 0 when it exists already: harmless
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST,
                class,
                w!("swapdesk input test"),
                WS_POPUP | WS_VISIBLE,
                at.0,
                at.1,
                size.0,
                size.1,
                None,
                None,
                Some(hinst),
                None,
            )
            .expect("CreateWindowExW");
            let _ = ShowWindow(hwnd, SW_SHOW);
            let prev_layout = GetKeyboardLayout(0);
            let mut win = Self { hwnd, origin: (0, 0), size, prev_layout };
            win.origin = {
                let mut p = POINT { x: 0, y: 0 };
                let _ = ClientToScreen(hwnd, &mut p);
                (p.x, p.y)
            };
            win.take_foreground();
            win
        }
    }

    /// Foreground and focus through the standard input-attach route; the window must be foreground afterwards.
    pub fn take_foreground(&self) {
        for _ in 0..20 {
            // SAFETY: attaches this thread's input to the foreground thread's for the call, then detaches.
            unsafe {
                let fg = GetForegroundWindow();
                let fg_thread = GetWindowThreadProcessId(fg, None);
                let me = GetCurrentThreadId();
                let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool();
                let _ = SetForegroundWindow(self.hwnd);
                let _ = SetFocus(Some(self.hwnd));
                if attached {
                    let _ = AttachThreadInput(me, fg_thread, false);
                }
            }
            pump(Duration::from_millis(30));
            if self.is_foreground() {
                return;
            }
        }
        panic!("the test window could not take the foreground; nothing was injected");
    }

    pub fn is_foreground(&self) -> bool {
        // SAFETY: plain query.
        unsafe { GetForegroundWindow() == self.hwnd }
    }

    /// Called before every injection: if something else took the foreground (a person at the PC), the window takes it
    /// back; if that fails the test panics and nothing is injected.
    pub fn guard(&self) {
        if !self.is_foreground() {
            self.take_foreground();
        }
        assert!(self.is_foreground(), "the test window lost the foreground: refusing to inject");
    }

    /// Window-client point of a screen pixel.
    pub fn to_client(&self, x: i32, y: i32) -> (i32, i32) {
        let mut p = POINT { x, y };
        // SAFETY: valid window and point.
        let _ = unsafe { ScreenToClient(self.hwnd, &mut p) };
        (p.x, p.y)
    }

    /// Runs the message loop for `d`, then returns everything recorded since the last `take`.
    pub fn settle(&self, d: Duration) -> Vec<Ev> {
        pump(d);
        take()
    }

    /// Pumps until `done` holds for the events so far, or `budget` passes; returns everything recorded.
    pub fn wait_for(&self, budget: Duration, done: impl Fn(&[Ev]) -> bool) -> Vec<Ev> {
        let start = Instant::now();
        let mut all: Vec<Ev> = Vec::new();
        loop {
            pump(Duration::from_millis(15));
            all.extend(take());
            if done(&all) || start.elapsed() > budget {
                return all;
            }
        }
    }

    /// Switches this thread's keyboard layout (`klid` like "00010416" for Portuguese Brazilian ABNT2). Returns whether it
    /// had to be loaded into the system first.
    pub fn use_layout(&self, klid: PCWSTR) -> bool {
        // SAFETY: layout calls on this thread.
        unsafe {
            let installed: Vec<HKL> = {
                let n = GetKeyboardLayoutList(None);
                let mut v = vec![HKL::default(); usize::try_from(n).unwrap_or(0)];
                GetKeyboardLayoutList(Some(&mut v));
                v
            };
            let hkl = LoadKeyboardLayoutW(klid, ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0)).expect("LoadKeyboardLayoutW");
            let was_loaded = installed.contains(&hkl);
            ActivateKeyboardLayout(hkl, ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0)).expect("ActivateKeyboardLayout");
            !was_loaded
        }
    }
}

impl Drop for TestWindow {
    fn drop(&mut self) {
        // SAFETY: restores this thread's layout and destroys the window created in `open`.
        unsafe {
            let _ = ActivateKeyboardLayout(self.prev_layout, ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0));
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Dispatches this thread's messages for `d`.
pub fn pump(d: Duration) {
    let start = Instant::now();
    loop {
        let mut any = false;
        let mut msg = MSG::default();
        // SAFETY: standard message loop step on the window's thread.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                any = true;
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if start.elapsed() >= d {
            return;
        }
        if !any {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Everything recorded so far; empties the record.
pub fn take() -> Vec<Ev> {
    LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// The text of the `Char` events, in order, surrogate pairs joined.
pub fn chars(evs: &[Ev]) -> String {
    let units: Vec<u16> = evs
        .iter()
        .filter_map(|e| match e {
            Ev::Char(c) => Some(*c),
            _ => None,
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// `(scan, ext)` of every key-down, in order.
pub fn key_downs(evs: &[Ev]) -> Vec<(u8, bool)> {
    evs.iter()
        .filter_map(|e| match e {
            Ev::KeyDown { scan, ext, .. } => Some((*scan, *ext)),
            _ => None,
        })
        .collect()
}
