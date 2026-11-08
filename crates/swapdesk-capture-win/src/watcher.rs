//! Desktop-switch and display-change notifications on a thread of their own (contract C2.1: the capture thread owns no
//! window and no hook, so `SetThreadDesktop` works there).
//!
//! The thread runs a message loop with a hidden top-level window (it receives the `WM_DISPLAYCHANGE` broadcast on a
//! display added, removed or switched to another mode) and a `WINEVENT_OUTOFCONTEXT` hook for
//! `EVENT_SYSTEM_DESKTOPSWITCH`. Events are advisory: the consumer re-enumerates displays (`diff_displays`) and calls
//! `reacquire`; a burst of messages for one change is normal.

use crate::errors::{self};
use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;
use swapdesk_platform::{ErrorKind, Result};
use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, EVENT_SYSTEM_DESKTOPSWITCH, GetMessageW, MSG,
    PostMessageW, PostThreadMessageW, RegisterClassW, WINEVENT_OUTOFCONTEXT, WM_DISPLAYCHANGE, WM_QUIT, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEvent {
    /// The input desktop changed (lock, UAC, Ctrl+Alt+Del screen, back). Call `reacquire`.
    DesktopSwitch,
    /// A display was added, removed or changed mode. Re-enumerate and diff.
    DisplayChange,
}

thread_local! {
    static SENDER: RefCell<Option<Sender<WatchEvent>>> = const { RefCell::new(None) };
}

fn emit(event: WatchEvent) {
    SENDER.with(|s| {
        if let Some(tx) = s.borrow().as_ref() {
            let _ = tx.send(event);
        }
    });
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DISPLAYCHANGE {
        emit(WatchEvent::DisplayChange);
    }
    // SAFETY: forwards the message unchanged to the default procedure.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn desktop_switch_hook(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == EVENT_SYSTEM_DESKTOPSWITCH {
        emit(WatchEvent::DesktopSwitch);
    }
}

struct Started {
    thread_id: u32,
    hwnd: isize,
}

/// The watcher thread's handle. Dropping it stops the thread and removes the hook and the window.
pub struct DisplayWatcher {
    rx: Receiver<WatchEvent>,
    thread_id: u32,
    hwnd: isize,
    join: Option<JoinHandle<()>>,
}

impl DisplayWatcher {
    pub fn start() -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Started>>();
        let join = std::thread::Builder::new()
            .name("swapdesk-capture-watcher".into())
            .spawn(move || run(tx, ready_tx))
            .map_err(|e| errors::error(ErrorKind::Os, "spawn watcher", None, e.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(s)) => Ok(Self { rx, thread_id: s.thread_id, hwnd: s.hwnd, join: Some(join) }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => Err(errors::error(ErrorKind::Timeout, "start watcher", None, "the watcher thread did not start")),
        }
    }

    /// Waits for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<WatchEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    pub fn try_recv(&self) -> Option<WatchEvent> {
        self.rx.try_recv().ok()
    }

    /// Posts a `WM_DISPLAYCHANGE` to the watcher's own window, as the system would: for tests, which must not
    /// broadcast to every window of the machine.
    pub fn simulate_display_change(&self) {
        // SAFETY: the window belongs to the watcher thread and lives until the watcher is dropped.
        let _ = unsafe { PostMessageW(Some(HWND(self.hwnd as *mut _)), WM_DISPLAYCHANGE, WPARAM(0), LPARAM(0)) };
    }
}

impl Drop for DisplayWatcher {
    fn drop(&mut self) {
        // SAFETY: posts WM_QUIT to the watcher thread's queue.
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn fail(ready: &Sender<Result<Started>>, op: &'static str, e: &windows::core::Error) {
    let _ = ready.send(Err(errors::from_hresult(errors::Phase::Duplicate, op, e.code().0, &e.message())));
}

fn run(tx: Sender<WatchEvent>, ready: Sender<Result<Started>>) {
    SENDER.with(|s| *s.borrow_mut() = Some(tx));
    // SAFETY: plain query for the current module.
    let hinstance = match unsafe { GetModuleHandleW(PCWSTR::null()) } {
        Ok(h) => h,
        Err(e) => return fail(&ready, "GetModuleHandleW", &e),
    };
    let class_name = w!("SwapdeskCaptureWatcher");
    let class = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // SAFETY: `class` is fully initialised and its strings are 'static.
    if unsafe { RegisterClassW(&class) } == 0 {
        // SAFETY: reads the thread's last error right after the failing call.
        if unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return fail(&ready, "RegisterClassW", &windows::core::Error::from_thread());
        }
    }
    // SAFETY: a hidden pop-up window of the class above, owned by this thread.
    let hwnd = match unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!(""),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    } {
        Ok(h) => h,
        Err(e) => return fail(&ready, "CreateWindowExW", &e),
    };
    // SAFETY: the callback is a plain function with the WINEVENTPROC signature; out of context, so it runs on this
    // thread inside the message loop.
    let hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            None,
            Some(desktop_switch_hook),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.0.is_null() {
        // SAFETY: the window was created above on this thread.
        let _ = unsafe { DestroyWindow(hwnd) };
        return fail(&ready, "SetWinEventHook", &windows::core::Error::from_thread());
    }
    // SAFETY: plain query.
    let thread_id = unsafe { GetCurrentThreadId() };
    let _ = ready.send(Ok(Started { thread_id, hwnd: hwnd.0 as isize }));

    let mut msg = MSG::default();
    // SAFETY: standard message loop; `GetMessageW` returns 0 on WM_QUIT and -1 on error.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        // SAFETY: dispatches a message this thread just received.
        unsafe { DispatchMessageW(&msg) };
    }
    // SAFETY: both were created on this thread.
    unsafe {
        let _ = UnhookWinEvent(hook);
        let _ = DestroyWindow(hwnd);
    }
    SENDER.with(|s| *s.borrow_mut() = None);
}
