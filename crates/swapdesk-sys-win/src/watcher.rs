//! `SessionWatcher` (contract C3) for Windows.
//!
//! `state()` asks the OS now: the active console session id (`WTSGetActiveConsoleSessionId`), its user name
//! (`WTSQuerySessionInformationW`), whether that user is an administrator (best effort), and the desktop (always
//! `Default`: only the helper's desktop-switch watcher sees Winlogon and the secure desktop).
//!
//! `next_event` is fed by a channel. Two sources push into it through a [`SessionEventSender`]:
//! the service control handler ([`crate::service`], `SERVICE_CONTROL_SESSIONCHANGE` and `POWEREVENT`), and in
//! development mode ([`WindowWatcher`]) a hidden top-level window that receives `WM_WTSSESSION_CHANGE`
//! (`WTSRegisterSessionNotification`), `WM_POWERBROADCAST` and `WM_DISPLAYCHANGE`. The window is top-level, not
//! message-only, because message-only windows do not receive broadcast messages.
//!
//! Mapping: console connect/disconnect and logon/logoff -> `ConsoleChanged(state now)`; session lock -> `DesktopSwitched(Locked)`,
//! unlock -> `DesktopSwitched(Default)` (a reading of C3: the secure desktop of UAC is not announced here);
//! suspend -> `Suspending`; resume (automatic, critical or by user) -> `Resumed`, once per suspend; display change -> `DisplaysChanged`.
//! `NetworkChanged` has no source in this module (the owner pushes it: `swapdesk-net-win`'s `NetworkWatcher`). After a service stop the
//! watcher returns `ErrorKind::Closed` from `next_event`.

use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_platform::{ConsoleState, ErrorKind, PlatformError, Result, SessionEvent, SessionWatcher};
use swapdesk_proto::Desktop;
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_ALL_SESSIONS, WTS_CURRENT_SERVER_HANDLE, WTSFreeMemory, WTSQuerySessionInformationW, WTSQueryUserToken,
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, WTSUserName,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMECRITICAL, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
    PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowLongPtrW, TranslateMessage, WM_CLOSE, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_NCCREATE, WM_NCDESTROY, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WNDCLASSEXW,
    WTS_CONSOLE_CONNECT, WTS_CONSOLE_DISCONNECT, WTS_SESSION_LOCK, WTS_SESSION_LOGOFF, WTS_SESSION_LOGON,
    WTS_SESSION_UNLOCK,
};

use crate::launch::{NO_CONSOLE_SESSION, active_console_session};
use crate::pipe::{ADMINISTRATORS_SID, session_of_pid};
use crate::sys::{OwnedHandle, from_wide_ptr, last_error, open_process_token, token_groups, wide};

/// What the sources push; `ConsoleChanged` carries no state, the watcher reads it when the event is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Raw {
    ConsoleChanged,
    Desktop(Desktop),
    DisplaysChanged,
    Suspending,
    Resumed,
    NetworkChanged,
    /// Nothing happened: the owner asks the consumer to look at its own queues (`next_event` returns `Ok(None)`).
    Wake,
    Stop,
}

/// `SERVICE_CONTROL_SESSIONCHANGE` / `WM_WTSSESSION_CHANGE` event type to an event.
pub(crate) fn session_change_event(event_type: u32) -> Option<Raw> {
    match event_type {
        WTS_CONSOLE_CONNECT | WTS_CONSOLE_DISCONNECT | WTS_SESSION_LOGON | WTS_SESSION_LOGOFF => {
            Some(Raw::ConsoleChanged)
        }
        WTS_SESSION_LOCK => Some(Raw::Desktop(Desktop::Locked)),
        WTS_SESSION_UNLOCK => Some(Raw::Desktop(Desktop::Default)),
        _ => None,
    }
}

/// `SERVICE_CONTROL_POWEREVENT` / `WM_POWERBROADCAST` event type to an event.
pub(crate) fn power_event(event_type: u32) -> Option<Raw> {
    match event_type {
        PBT_APMSUSPEND => Some(Raw::Suspending),
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMECRITICAL | PBT_APMRESUMESUSPEND => Some(Raw::Resumed),
        _ => None,
    }
}

/// The pushing end; clone it into every source. Pushing never blocks.
#[derive(Clone)]
pub struct SessionEventSender {
    tx: Sender<Raw>,
    /// False between a suspend and the first resume event, so a resume reported twice reaches the consumer once.
    awake: Arc<AtomicBool>,
}

impl core::fmt::Debug for SessionEventSender {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SessionEventSender").finish_non_exhaustive()
    }
}

impl SessionEventSender {
    pub(crate) fn push(&self, raw: Raw) {
        match raw {
            Raw::Suspending => self.awake.store(false, Ordering::SeqCst),
            // `swap` returns the previous value: already awake means this resume repeats one.
            Raw::Resumed if self.awake.swap(true, Ordering::SeqCst) => return,
            _ => {}
        }
        // A closed receiver means nobody listens any more.
        let _ = self.tx.send(raw);
    }

    pub fn console_changed(&self) {
        self.push(Raw::ConsoleChanged);
    }
    pub fn desktop_switched(&self, desktop: Desktop) {
        self.push(Raw::Desktop(desktop));
    }
    pub fn displays_changed(&self) {
        self.push(Raw::DisplaysChanged);
    }
    pub fn suspending(&self) {
        self.push(Raw::Suspending);
    }
    pub fn resumed(&self) {
        self.push(Raw::Resumed);
    }
    pub fn network_changed(&self) {
        self.push(Raw::NetworkChanged);
    }
    /// Wakes the consumer without an event: its `next_event` returns `Ok(None)` at once. The owner's other sources (a
    /// queue of its own, a helper that ended) call this so the consumer sleeps until something happens instead of
    /// looking on a timer.
    pub fn wake(&self) {
        self.push(Raw::Wake);
    }
    /// Ends the watcher: the pending and every later `next_event` returns `ErrorKind::Closed`.
    pub fn stop(&self) {
        self.push(Raw::Stop);
    }
}

/// A new event channel: the sender for the sources, the watcher for the consumer.
pub fn session_channel() -> (SessionEventSender, WinSessionWatcher) {
    let (tx, rx) = mpsc::channel();
    (SessionEventSender { tx, awake: Arc::new(AtomicBool::new(true)) }, WinSessionWatcher { rx, stopped: false })
}

/// The Windows `SessionWatcher`.
pub struct WinSessionWatcher {
    rx: Receiver<Raw>,
    stopped: bool,
}

fn closed(detail: &str) -> PlatformError {
    PlatformError { kind: ErrorKind::Closed, op: "SessionWatcher", os_code: None, detail: detail.into() }
}

impl SessionWatcher for WinSessionWatcher {
    fn state(&mut self) -> Result<ConsoleState> {
        console_state()
    }

    fn next_event(&mut self, timeout: Duration) -> Result<Option<SessionEvent>> {
        if self.stopped {
            return Err(closed("stopped"));
        }
        match self.rx.recv_timeout(timeout) {
            Ok(Raw::ConsoleChanged) => Ok(Some(SessionEvent::ConsoleChanged(console_state()?))),
            Ok(Raw::Desktop(d)) => Ok(Some(SessionEvent::DesktopSwitched(d))),
            Ok(Raw::DisplaysChanged) => Ok(Some(SessionEvent::DisplaysChanged)),
            Ok(Raw::Suspending) => Ok(Some(SessionEvent::Suspending)),
            Ok(Raw::Resumed) => Ok(Some(SessionEvent::Resumed)),
            Ok(Raw::NetworkChanged) => Ok(Some(SessionEvent::NetworkChanged)),
            Ok(Raw::Wake) => Ok(None),
            Ok(Raw::Stop) => {
                self.stopped = true;
                Err(closed("stopped"))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(closed("every event source is gone")),
        }
    }
}

/// The console session, its user and whether the user is an administrator, read now.
pub fn console_state() -> Result<ConsoleState> {
    let Some(session) = active_console_session() else {
        return Ok(ConsoleState { session_id: None, user: None, user_is_admin: false, desktop: Desktop::Default });
    };
    let user = session_user_name(session)?;
    let user_is_admin = user.is_some() && session_user_is_admin(session);
    Ok(ConsoleState { session_id: Some(session), user, user_is_admin, desktop: Desktop::Default })
}

/// The user name logged on in `session` (`WTSUserName`); `None` at the login screen (empty name).
pub fn session_user_name(session: u32) -> Result<Option<String>> {
    let mut buf: *mut u16 = null_mut();
    let mut bytes = 0u32;
    // SAFETY: the current server handle with a session id is the documented local form; the out-parameters are live;
    // the buffer is released with WTSFreeMemory below.
    if unsafe {
        WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, session, WTSUserName, (&raw mut buf).cast(), &mut bytes)
    } == 0
    {
        return Err(last_error("WTSQuerySessionInformationW"));
    }
    // SAFETY: on success the buffer holds a null-terminated wide string.
    let name = unsafe { from_wide_ptr(buf) };
    // SAFETY: `buf` came from WTSQuerySessionInformationW and is freed once.
    unsafe { WTSFreeMemory(buf.cast()) };
    Ok(if name.is_empty() { None } else { Some(name) })
}

/// Best effort: true when the session's user has the Administrators group in their token (also as "deny only", the way a
/// filtered token carries it). The service (SYSTEM) reads the user's token with `WTSQueryUserToken`; any other process
/// can only judge its own token, and only when the session is its own. Anything else is `false`.
fn session_user_is_admin(session: u32) -> bool {
    let has_admin =
        |token: HANDLE| token_groups(token).is_ok_and(|g| g.iter().any(|(sid, _)| sid == ADMINISTRATORS_SID));
    let mut raw: HANDLE = null_mut();
    // SAFETY: `raw` is a live out-parameter; the call needs SYSTEM with SE_TCB_NAME and simply fails otherwise.
    if unsafe { WTSQueryUserToken(session, &mut raw) } != 0 {
        let token = OwnedHandle(raw);
        return has_admin(token.0);
    }
    // SAFETY: no preconditions.
    let own = unsafe { GetCurrentProcessId() };
    if session_of_pid(own).ok() == Some(session)
        && session != NO_CONSOLE_SESSION
        && let Ok(token) = open_process_token(TOKEN_QUERY)
    {
        return has_admin(token.0);
    }
    false
}

// ---------- the hidden window (development mode) ----------

struct WindowCtx {
    sender: SessionEventSender,
}

const CLASS_NAME: &str = "SwapdeskSessionWatcher";

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: for WM_NCCREATE `lparam` points to the CREATESTRUCTW whose `lpCreateParams` is what CreateWindowExW was given.
        let cs = unsafe { &*(lparam as *const CREATESTRUCTW) };
        // SAFETY: the window is ours and alive; storing the context pointer is the documented use of GWLP_USERDATA.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize) };
        // SAFETY: default handling of the same message.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: the window is alive during its own messages.
    let ctx = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowCtx;
    // SAFETY: the pointer is the Box leaked in `WindowWatcher::spawn`, set at WM_NCCREATE and cleared at WM_NCDESTROY, on this thread.
    let ctx = unsafe { ctx.as_ref() };
    match (msg, ctx) {
        (WM_WTSSESSION_CHANGE, Some(c)) => {
            if let Some(raw) = session_change_event(wparam as u32) {
                c.sender.push(raw);
            }
            0
        }
        (WM_POWERBROADCAST, Some(c)) => {
            if let Some(raw) = power_event(wparam as u32) {
                c.sender.push(raw);
            }
            1 // TRUE: the request is granted
        }
        (WM_DISPLAYCHANGE, Some(c)) => {
            c.sender.displays_changed();
            0
        }
        (WM_CLOSE, _) => {
            // SAFETY: the window is ours and alive.
            unsafe { DestroyWindow(hwnd) };
            0
        }
        (WM_DESTROY, _) => {
            // SAFETY: unregistering a window that may have registered; a failure is harmless.
            unsafe { WTSUnRegisterSessionNotification(hwnd) };
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            0
        }
        (WM_NCDESTROY, _) => {
            if ctx.is_some() {
                // SAFETY: last message of the window: clear the pointer, then free the Box leaked at spawn, once.
                unsafe {
                    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowCtx;
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(p));
                }
            }
            // SAFETY: default handling of the same message.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        // SAFETY: default handling of the same message.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// A hidden window on its own thread that turns session, power and display messages into [`SessionEventSender`]
/// pushes. Dropping it closes the window and joins the thread.
pub struct WindowWatcher {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

impl WindowWatcher {
    pub fn spawn(sender: SessionEventSender) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize>>();
        let thread = std::thread::Builder::new()
            .name("swapdesk-session-window".into())
            .spawn(move || window_thread(sender, ready_tx))
            .map_err(|e| PlatformError {
                kind: ErrorKind::Os,
                op: "WindowWatcher::spawn",
                os_code: e.raw_os_error().map(i64::from),
                detail: e.to_string(),
            })?;
        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(hwnd)) => Ok(Self { hwnd, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => Err(PlatformError {
                kind: ErrorKind::Timeout,
                op: "WindowWatcher::spawn",
                os_code: None,
                detail: "the window thread did not report".into(),
            }),
        }
    }

    /// The window handle, for tests that post messages to it.
    pub fn hwnd(&self) -> isize {
        self.hwnd
    }
}

impl Drop for WindowWatcher {
    fn drop(&mut self) {
        // SAFETY: posting to a window handle is safe from any thread; if the window is gone the call just fails.
        unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn window_thread(sender: SessionEventSender, ready: Sender<Result<isize>>) {
    let class = match wide(std::ffi::OsStr::new(CLASS_NAME)) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: null asks for the module of the current process.
    let hinstance = unsafe { GetModuleHandleW(core::ptr::null()) };
    // SAFETY: plain data; zeroed is valid and the used fields are set below.
    let mut wc: WNDCLASSEXW = unsafe { core::mem::zeroed() };
    wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
    wc.lpfnWndProc = Some(window_proc);
    wc.hInstance = hinstance;
    wc.lpszClassName = class.as_ptr();
    // SAFETY: `wc` is fully initialised and `class` outlives the call. A second watcher finds the class registered (error 1410), which is fine.
    unsafe { RegisterClassExW(&wc) };
    let ctx = Box::into_raw(Box::new(WindowCtx { sender }));
    // SAFETY: the class is registered; a top-level window of size 0 that is never shown; `ctx` is passed to WM_NCCREATE.
    let hwnd = unsafe {
        CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinstance, ctx.cast())
    };
    if hwnd.is_null() {
        let e = last_error("CreateWindowExW");
        // SAFETY: the window was never created, so WM_NCDESTROY will not free `ctx`: free it here, once.
        drop(unsafe { Box::from_raw(ctx) });
        let _ = ready.send(Err(e));
        return;
    }
    // SAFETY: `hwnd` is our live window. Failure (no WTS service) only means no session events from this window.
    unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_ALL_SESSIONS) };
    let _ = ready.send(Ok(hwnd as isize));
    // SAFETY: plain data.
    let mut msg: MSG = unsafe { core::mem::zeroed() };
    // SAFETY: the standard message loop of the thread that owns the window.
    while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
        // SAFETY: `msg` was filled by GetMessageW.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
