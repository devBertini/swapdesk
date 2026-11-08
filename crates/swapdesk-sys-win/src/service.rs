//! Service Control Manager glue (C2.1): `swapdesk-service.exe` runs as LocalSystem under the SCM; this module is
//! the dispatcher, the control handler and the status reports, plus the query the UI uses to learn the service PID.
//!
//! [`run_service`] blocks the main thread in `StartServiceCtrlDispatcherW` until the service stops. The SCM then calls
//! `ServiceMain` on another thread, which registers the control handler, reports `RUNNING` and runs the caller's body.
//! The control handler maps `SERVICE_CONTROL_STOP`/`SHUTDOWN` to the [`StopSignal`] (and an end of the watcher),
//! `SESSIONCHANGE` and `POWEREVENT` to [`crate::watcher`] events. Compiled and unit-tested only where it runs without
//! the SCM: the dispatcher itself is exercised when the service is installed.
//!
//! Sources read on 2026-11-08: `StartServiceCtrlDispatcherW`
//! <https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-startservicectrldispatcherw>: the call fails with
//! `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` (1063) "if the program is being run as a console application", which is
//! how the binary falls back to `--console` ([`ServiceError::NotAService`]); the main thread must call it within 30 s
//! of the start; it returns once every service of the process is `SERVICE_STOPPED`.

use std::ffi::OsStr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use swapdesk_platform::PlatformError;
use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_DOES_NOT_EXIST, GetLastError,
    NO_ERROR,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, RegisterServiceCtrlHandlerExW, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_ACCEPT_POWEREVENT, SERVICE_ACCEPT_SESSIONCHANGE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_POWEREVENT,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STATUS_PROCESS,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    StartServiceCtrlDispatcherW,
};

use crate::sys::{os_error, wide};
use crate::watcher::{Raw, SessionEventSender, WinSessionWatcher, power_event, session_change_event, session_channel};

/// `ERROR_SERVICE_SPECIFIC_ERROR`: the real code is in `dwServiceSpecificExitCode`.
const ERROR_SERVICE_SPECIFIC_ERROR: u32 = 1066;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// No service of that name is installed (`ERROR_SERVICE_DOES_NOT_EXIST`).
    NotFound(String),
    /// The process was not started by the SCM (run as a console program): use development mode.
    NotAService,
    /// The service exists but has no process (it is stopped).
    NotRunning(String),
    /// `run_service` was already called in this process.
    AlreadyStarted,
    Os(PlatformError),
}

impl core::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ServiceError::NotFound(n) => write!(f, "service {n} is not installed"),
            ServiceError::NotAService => write!(f, "not started by the service control manager"),
            ServiceError::NotRunning(n) => write!(f, "service {n} is not running"),
            ServiceError::AlreadyStarted => write!(f, "the service dispatcher was already started"),
            ServiceError::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ServiceError {}

impl From<PlatformError> for ServiceError {
    fn from(e: PlatformError) -> Self {
        ServiceError::Os(e)
    }
}

/// A flag the control handler sets on stop or shutdown, waited on by the service body.
#[derive(Clone, Default)]
pub struct StopSignal {
    inner: Arc<(Mutex<bool>, Condvar)>,
}

impl StopSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self) {
        let (m, cv) = &*self.inner;
        *m.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
    }

    pub fn is_set(&self) -> bool {
        *self.inner.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Waits up to `timeout`; true when the signal is set.
    pub fn wait(&self, timeout: Duration) -> bool {
        let (m, cv) = &*self.inner;
        let guard = m.lock().unwrap_or_else(|p| p.into_inner());
        let (guard, _) = cv.wait_timeout_while(guard, timeout, |set| !*set).unwrap_or_else(|p| p.into_inner());
        *guard
    }
}

/// What the service body gets.
pub struct ServiceContext {
    /// Session and power events from the SCM; returns `ErrorKind::Closed` once the service is asked to stop.
    pub watcher: WinSessionWatcher,
    /// To push events of other sources (a display-change hook) into the same watcher.
    pub events: SessionEventSender,
    pub stop: StopSignal,
}

type Body = Box<dyn FnOnce(ServiceContext) -> u32 + Send>;

struct Global {
    name: Vec<u16>,
    body: Mutex<Option<Body>>,
    watcher: Mutex<Option<WinSessionWatcher>>,
    events: SessionEventSender,
    stop: StopSignal,
    handle: AtomicIsize,
    status: Mutex<SERVICE_STATUS>,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

/// Runs `body` as the Windows service `name` (`SERVICE_WIN32_OWN_PROCESS`). Blocks until the service has stopped. `body` runs on the
/// `ServiceMain` thread and returns the exit code (0 is success); it should return soon after `stop` is set.
/// A panic in `body` is caught and reported as exit code 1 (the service profile unwinds, D15).
///
/// Returns [`ServiceError::NotAService`] when the process was not started by the SCM.
pub fn run_service<F>(name: &str, body: F) -> Result<(), ServiceError>
where
    F: FnOnce(ServiceContext) -> u32 + Send + 'static,
{
    let (events, watcher) = session_channel();
    let name_w = wide(OsStr::new(name))?;
    let global = Global {
        name: name_w,
        body: Mutex::new(Some(Box::new(body))),
        watcher: Mutex::new(Some(watcher)),
        events,
        stop: StopSignal::new(),
        handle: AtomicIsize::new(0),
        status: Mutex::new(SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: SERVICE_START_PENDING,
            dwControlsAccepted: 0,
            dwWin32ExitCode: 0,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: 0,
            dwWaitHint: 0,
        }),
    };
    if GLOBAL.set(global).is_err() {
        return Err(ServiceError::AlreadyStarted);
    }
    let g = GLOBAL.get().expect("just set");
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: g.name.as_ptr().cast_mut(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW { lpServiceName: null_mut(), lpServiceProc: None },
    ];
    // SAFETY: `table` is a null-terminated array of entries whose name pointer and ServiceMain stay valid for the whole (blocking) call.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_FAILED_SERVICE_CONTROLLER_CONNECT {
            ServiceError::NotAService
        } else {
            ServiceError::Os(os_error("StartServiceCtrlDispatcherW", code))
        });
    }
    Ok(())
}

fn set_status(g: &Global, state: u32, accepted: u32, check_point: u32, wait_hint_ms: u32, exit: Option<u32>) {
    let mut st = g.status.lock().unwrap_or_else(|p| p.into_inner());
    st.dwCurrentState = state;
    st.dwControlsAccepted = accepted;
    st.dwCheckPoint = check_point;
    st.dwWaitHint = wait_hint_ms;
    if let Some(code) = exit {
        if code == 0 {
            st.dwWin32ExitCode = 0;
            st.dwServiceSpecificExitCode = 0;
        } else {
            st.dwWin32ExitCode = ERROR_SERVICE_SPECIFIC_ERROR;
            st.dwServiceSpecificExitCode = code;
        }
    }
    let handle = g.handle.load(Ordering::SeqCst) as SERVICE_STATUS_HANDLE;
    if !handle.is_null() {
        // SAFETY: `handle` came from RegisterServiceCtrlHandlerExW; `st` is a live SERVICE_STATUS (guarded by the mutex).
        unsafe { SetServiceStatus(handle, &*st) };
    }
}

const ACCEPTED: u32 =
    SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE | SERVICE_ACCEPT_POWEREVENT;

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let Some(g) = GLOBAL.get() else { return };
    // SAFETY: `g.name` is a null-terminated wide string living in a static; the handler is a plain function.
    let handle = unsafe { RegisterServiceCtrlHandlerExW(g.name.as_ptr(), Some(handler_ex), null()) };
    if handle.is_null() {
        return;
    }
    g.handle.store(handle as isize, Ordering::SeqCst);
    set_status(g, SERVICE_START_PENDING, 0, 1, 10_000, None);
    let body = g.body.lock().unwrap_or_else(|p| p.into_inner()).take();
    let watcher = g.watcher.lock().unwrap_or_else(|p| p.into_inner()).take();
    set_status(g, SERVICE_RUNNING, ACCEPTED, 0, 0, None);
    let code = match (body, watcher) {
        (Some(body), Some(watcher)) => {
            let ctx = ServiceContext { watcher, events: g.events.clone(), stop: g.stop.clone() };
            catch_unwind(AssertUnwindSafe(|| body(ctx))).unwrap_or(1)
        }
        _ => 1,
    };
    set_status(g, SERVICE_STOPPED, 0, 0, 0, Some(code));
}

unsafe extern "system" fn handler_ex(
    control: u32,
    event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    let Some(g) = GLOBAL.get() else { return ERROR_CALL_NOT_IMPLEMENTED };
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            set_status(g, SERVICE_STOP_PENDING, 0, 1, 15_000, None);
            g.stop.set();
            g.events.push(Raw::Stop);
            NO_ERROR
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            if let Some(raw) = session_change_event(event_type) {
                g.events.push(raw);
            }
            NO_ERROR
        }
        SERVICE_CONTROL_POWEREVENT => {
            if let Some(raw) = power_event(event_type) {
                g.events.push(raw);
            }
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => {
            let (state, accepted) = {
                let st = g.status.lock().unwrap_or_else(|p| p.into_inner());
                (st.dwCurrentState, st.dwControlsAccepted)
            };
            set_status(g, state, accepted, 0, 0, None);
            NO_ERROR
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

// ---------- querying a service ----------

struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenSCManagerW/OpenServiceW and is closed once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// State and process of a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceStatus {
    /// `SERVICE_STOPPED` (1), `SERVICE_START_PENDING` (2), `SERVICE_RUNNING` (4) ...
    pub state: u32,
    /// 0 when the service has no process.
    pub pid: u32,
}

/// `QueryServiceStatusEx` for the service `name`. Works as a standard user for services whose security allows it.
pub fn query_service(name: &str) -> Result<ServiceStatus, ServiceError> {
    if name.is_empty() || name.len() > 256 || name.contains(['/', '\\']) {
        return Err(PlatformError {
            kind: swapdesk_platform::ErrorKind::InvalidInput,
            op: "query_service",
            os_code: None,
            detail: "not a valid service name".into(),
        }
        .into());
    }
    let name_w = wide(OsStr::new(name))?;
    // SAFETY: null machine and database names mean the local active database.
    let scm = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
    if scm.is_null() {
        // SAFETY: no preconditions.
        return Err(os_error("OpenSCManagerW", unsafe { GetLastError() }).into());
    }
    let scm = ScHandle(scm);
    // SAFETY: `scm` is open with SC_MANAGER_CONNECT; `name_w` is a null-terminated wide string.
    let svc = unsafe { OpenServiceW(scm.0, name_w.as_ptr(), SERVICE_QUERY_STATUS) };
    if svc.is_null() {
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        return Err(if code == ERROR_SERVICE_DOES_NOT_EXIST {
            ServiceError::NotFound(name.to_string())
        } else {
            os_error("OpenServiceW", code).into()
        });
    }
    let svc = ScHandle(svc);
    // SAFETY: SERVICE_STATUS_PROCESS is plain data; zeroed is valid.
    let mut info: SERVICE_STATUS_PROCESS = unsafe { core::mem::zeroed() };
    let mut needed = 0u32;
    // SAFETY: `svc` has SERVICE_QUERY_STATUS; the buffer is `info` with its exact size.
    let ok = unsafe {
        QueryServiceStatusEx(
            svc.0,
            SC_STATUS_PROCESS_INFO,
            (&raw mut info).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        // SAFETY: no preconditions.
        return Err(os_error("QueryServiceStatusEx", unsafe { GetLastError() }).into());
    }
    Ok(ServiceStatus { state: info.dwCurrentState, pid: info.dwProcessId })
}

/// The PID the SCM reports for the service: what the UI and the helpers compare `GetNamedPipeServerProcessId` with (C2.1).
pub fn service_pid(name: &str) -> Result<u32, ServiceError> {
    let st = query_service(name)?;
    if st.pid == 0 { Err(ServiceError::NotRunning(name.to_string())) } else { Ok(st.pid) }
}
