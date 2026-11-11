//! The connection to the service (CONTRACTS.md C2.1, C2.4 "Development mode").
//!
//! The UI pipe is `ui-<session id>` of this process's own session (`ProcessIdToSessionId` of itself), with the
//! suffix `-<n>` in development (`--instance`). The pipe is opened by `swapdesk_sys_win::pipe::connect`, which uses
//! `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` and compares `GetNamedPipeServerProcessId` with the expected PID
//! before anything is written. The expected PID is the one the SCM reports for the service (production) or
//! `--service-pid` (development). The handshake runs on a helper thread with a deadline, so a service that accepts
//! and never answers cannot keep the window from opening.
//!
//! [`connect`] is one try. The interface calls it through [`crate::reconnect::Reconnecting`], which tries again every
//! second for 30 s at launch and after a break, and answers `ServiceDown` past that (the state the UI already knows
//! how to show).

use std::sync::{Arc, mpsc};
use std::time::Duration;

use swapdesk_sys_win::pipe::{PipeConn, PipeError, connect as connect_pipe, session_of_pid, ui_pipe_name};
use swapdesk_sys_win::service_pid;
use swapdesk_ui::UiWaker;
use swapdesk_ui::service_backend::{ServiceBackend, ViewerSink};

use crate::cli::Options;
use crate::elevate::WinElevator;

/// The name the SCM knows the service by.
pub const SERVICE_NAME: &str = "SwapdeskService";
/// How long the pipe may stay busy before the connection is given up.
pub const PIPE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the handshake may take.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum ConnectError {
    /// The service's PID is unknown (not installed, not running).
    NoService(String),
    /// No session id for this process.
    Session(String),
    /// The pipe could not be opened or its server is not the expected process.
    Pipe(String),
    /// The system refused this process the pipe (access denied): its security descriptor does not list it.
    Refused(String),
    /// The handshake failed or timed out.
    Handshake(String),
}

impl core::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConnectError::NoService(e) => write!(f, "service: {e}"),
            ConnectError::Session(e) => write!(f, "session: {e}"),
            ConnectError::Pipe(e) => write!(f, "pipe: {e}"),
            ConnectError::Refused(e) => write!(f, "pipe refused: {e}"),
            ConnectError::Handshake(e) => write!(f, "handshake: {e}"),
        }
    }
}

/// `ui-<session>` or, with an instance, `ui-<session>-<n>` (the name the service lane creates in development).
pub fn pipe_name(session_id: u32, instance: Option<u32>) -> String {
    match instance {
        Some(n) => format!("{}-{n}", ui_pipe_name(session_id)),
        None => ui_pipe_name(session_id),
    }
}

/// The PID the pipe's server must have: `--service-pid`, else what the SCM reports for the service.
pub fn expected_server_pid(options: &Options) -> Result<u32, ConnectError> {
    match options.service_pid {
        Some(pid) => Ok(pid),
        None => service_pid(SERVICE_NAME).map_err(|e| ConnectError::NoService(e.to_string())),
    }
}

/// Opens and checks the UI pipe of this session.
pub fn open_pipe(options: &Options) -> Result<PipeConn, ConnectError> {
    let pid = expected_server_pid(options)?;
    let session = session_of_pid(std::process::id()).map_err(|e| ConnectError::Session(e.to_string()))?;
    connect_pipe(&pipe_name(session, options.instance), pid, PIPE_TIMEOUT).map_err(|e| match &e {
        PipeError::Os(os) if os.os_code == Some(ERROR_ACCESS_DENIED) => ConnectError::Refused(e.to_string()),
        _ => ConnectError::Pipe(e.to_string()),
    })
}

/// `ERROR_ACCESS_DENIED`, which `CreateFileW` reports for a pipe whose descriptor does not list the caller.
const ERROR_ACCESS_DENIED: i64 = 5;

/// Opens the pipe, runs the handshake and starts the backend, with `sink` receiving the viewer media.
pub fn connect(
    options: &Options,
    waker: Arc<dyn UiWaker>,
    sink: Arc<dyn ViewerSink>,
) -> Result<Arc<ServiceBackend>, ConnectError> {
    let conn = open_pipe(options)?;
    let (reader, writer) = conn.split().map_err(|e| ConnectError::Pipe(e.to_string()))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("swapdesk-ui-connect".into())
        .spawn(move || {
            let started = ServiceBackend::start_with_sink(reader, writer, waker, std::process::id(), Some(sink));
            let _ = tx.send(started);
        })
        .map_err(|e| ConnectError::Handshake(e.to_string()))?;
    match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(Ok(backend)) => {
            // The clipboard sync of the viewer role reads and writes this user's clipboard (C2.4 0x60 to 0x62).
            backend.set_clipboard(swapdesk_input_win::clipboard::WinClipboard::new());
            // "Continuar" of the administrator confirmation starts the elevated instance through UAC (ELEVATION.md 3.5).
            backend.set_elevator(Arc::new(WinElevator::new(options)));
            Ok(Arc::new(backend))
        }
        Ok(Err(e)) => Err(ConnectError::Handshake(e.to_string())),
        Err(_) => Err(ConnectError::Handshake("no HelloAck in time".into())),
    }
}

pub use crate::shared::down::DownBackend;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_names_follow_the_contract_and_the_service_lane() {
        assert_eq!(pipe_name(2, None), "ui-2");
        assert_eq!(pipe_name(2, Some(7)), "ui-2-7");
    }

    #[test]
    fn a_missing_pipe_is_a_pipe_error_not_a_hang() {
        // A pid that cannot be the server: the pipe of this fake instance does not exist, so the open fails at once.
        let options = Options { service_pid: Some(std::process::id()), instance: Some(987_654), ..Options::default() };
        let started = std::time::Instant::now();
        match open_pipe(&options) {
            Err(ConnectError::Pipe(_)) => {}
            other => panic!("expected a pipe error, got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
