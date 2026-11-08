//! Test support: a lock across test processes (DDA allows four duplications per session), per-monitor DPI awareness,
//! a small flickering window that gives the desktop something to duplicate (its two colours are known, so the test can
//! check pixels without ever looking at the owner's screen) and process counters.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use swapdesk_capture_win::displays;
use swapdesk_platform::{DisplayInfo, Rect};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, HBRUSH, ReleaseDC};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentThread, GetGuiResources,
    GetProcessHandleCount, GetProcessTimes, GetThreadTimes, INFINITE, ReleaseMutex, WaitForSingleObject,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, IDC_CROSS, LoadCursorW, MSG, PM_REMOVE,
    PeekMessageW, RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::{PCWSTR, w};

/// Held for a test's whole capture; other test processes wait.
pub struct Serial(HANDLE);

pub fn serial() -> Serial {
    // SAFETY: a named mutex shared by the test processes of this crate; waited on with no timeout.
    let h = unsafe { CreateMutexW(None, false, w!("Local\\swapdesk-capture-win-tests")) }.expect("mutex");
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

/// Per-monitor v2 for the process, so window and cursor coordinates below are physical pixels.
pub fn aware() {
    // SAFETY: process-wide setting; fails harmlessly when already set.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

pub fn primary() -> DisplayInfo {
    displays().expect("displays").into_iter().find(|d| d.primary).expect("a primary display")
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: default handling for every message.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub const SIZE: u32 = 160;
/// The two colours the window alternates between: nearly the same magenta, so it does not strobe.
pub const COLOR_A: (u8, u8, u8) = (255, 0, 255);
pub const COLOR_B: (u8, u8, u8) = (247, 0, 247);

/// A topmost, non-activating square on the primary display that repaints once per DWM frame.
pub struct Flicker {
    stop: Arc<AtomicBool>,
    /// Repaints done so far.
    pub paints: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
    /// Physical virtual-desktop pixels.
    pub rect: Rect,
}

impl Flicker {
    pub fn start(display: &DisplayInfo) -> Self {
        let rect = Rect { x: display.bounds.x + 80, y: display.bounds.y + 80, width: SIZE, height: SIZE };
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let flag = stop.clone();
        let paints = Arc::new(AtomicU64::new(0));
        let paint_count = paints.clone();
        let join = std::thread::spawn(move || {
            // SAFETY: a topmost no-activate window owned by this thread; painted with GDI until `stop`.
            unsafe {
                let hinstance = GetModuleHandleW(PCWSTR::null()).expect("module");
                let class = w!("SwapdeskCaptureTestFlicker");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(proc),
                    hInstance: hinstance.into(),
                    lpszClassName: class,
                    hCursor: LoadCursorW(None, IDC_CROSS).expect("cursor"),
                    ..Default::default()
                };
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    class,
                    w!("capture test"),
                    WS_POPUP,
                    rect.x,
                    rect.y,
                    rect.width as i32,
                    rect.height as i32,
                    None,
                    None,
                    Some(hinstance.into()),
                    None,
                )
                .expect("window");
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let brush = |(r, g, b): (u8, u8, u8)| {
                    CreateSolidBrush(windows::Win32::Foundation::COLORREF(
                        u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16),
                    ))
                };
                let brushes: [HBRUSH; 2] = [brush(COLOR_A), brush(COLOR_B)];
                let _ = ready_tx.send(());
                let mut i = 0usize;
                let mut msg = MSG::default();
                while !flag.load(Ordering::Relaxed) {
                    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    let dc = GetDC(Some(hwnd));
                    let area =
                        windows::Win32::Foundation::RECT { left: 0, top: 0, right: SIZE as i32, bottom: SIZE as i32 };
                    FillRect(dc, &area, brushes[i & 1]);
                    ReleaseDC(Some(hwnd), dc);
                    i += 1;
                    paint_count.fetch_add(1, Ordering::Relaxed);
                    if DwmFlush().is_err() {
                        std::thread::sleep(Duration::from_millis(8));
                    }
                }
                for b in brushes {
                    let _ = DeleteObject(b.into());
                }
                let _ = DestroyWindow(hwnd);
            }
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).expect("flicker window did not start");
        // Let DWM present the window before the first capture.
        std::thread::sleep(Duration::from_millis(300));
        Self { stop, paints, join: Some(join), rect }
    }

    /// The window's centre inside the display (display-local pixels).
    pub fn center_local(&self, display: &DisplayInfo) -> (i32, i32) {
        (
            self.rect.x + (self.rect.width as i32) / 2 - display.bounds.x,
            self.rect.y + (self.rect.height as i32) / 2 - display.bounds.y,
        )
    }
}

impl Drop for Flicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Whether a BGRA pixel is one of the two flicker colours.
pub fn is_flicker_pixel(bgra: &[u8]) -> bool {
    let (b, g, r) = (bgra[0], bgra[1], bgra[2]);
    r >= 240 && b >= 240 && g <= 8
}

pub fn filetime_ms(ft: FILETIME) -> f64 {
    (((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)) as f64) / 10_000.0
}

/// CPU time (kernel + user) of the calling thread, in milliseconds.
pub fn thread_cpu_ms() -> f64 {
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers; the pseudo handle of the calling thread.
    let _ = unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) };
    filetime_ms(k) + filetime_ms(u)
}

/// CPU time (kernel + user) of the process, in milliseconds.
pub fn process_cpu_ms() -> f64 {
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: valid out pointers; the pseudo handle of this process.
    let _ = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    filetime_ms(k) + filetime_ms(u)
}

#[derive(Debug, Clone, Copy)]
pub struct Counters {
    pub private_bytes: u64,
    pub working_set: u64,
    pub handles: u32,
    pub gdi_objects: u32,
    pub user_objects: u32,
}

pub fn counters() -> Counters {
    let mut pmc =
        PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: `pmc` is a valid, sized out struct (the EX layout starts with PROCESS_MEMORY_COUNTERS).
    let _ = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut pmc as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            pmc.cb,
        )
    };
    let mut handles = 0u32;
    // SAFETY: valid out pointer.
    let _ = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut handles) };
    // SAFETY: plain queries on this process.
    let (gdi, user) = unsafe {
        (GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS), GetGuiResources(GetCurrentProcess(), GR_USEROBJECTS))
    };
    Counters {
        private_bytes: pmc.PrivateUsage as u64,
        working_set: pmc.WorkingSetSize as u64,
        handles,
        gdi_objects: gdi,
        user_objects: user,
    }
}

/// Frames per 1 s window of a run (budget B2 is a median over these windows).
#[derive(Default)]
pub struct PerSecond(Vec<u32>);

impl PerSecond {
    pub fn add(&mut self, elapsed: Duration) {
        let i = elapsed.as_secs() as usize;
        if self.0.len() <= i {
            self.0.resize(i + 1, 0);
        }
        self.0[i] += 1;
    }

    /// Complete windows only (the last, partial one is dropped).
    fn complete(&self) -> Vec<u32> {
        let n = self.0.len().saturating_sub(1);
        self.0[..n].to_vec()
    }

    pub fn median(&self) -> f64 {
        let mut w = self.complete();
        if w.is_empty() {
            return 0.0;
        }
        w.sort_unstable();
        f64::from(w[w.len() / 2])
    }

    pub fn min(&self) -> u32 {
        self.complete().into_iter().min().unwrap_or(0)
    }

    pub fn windows_below(&self, fps: u32) -> usize {
        self.complete().into_iter().filter(|&c| c < fps).count()
    }

    pub fn windows(&self) -> usize {
        self.complete().len()
    }
}
