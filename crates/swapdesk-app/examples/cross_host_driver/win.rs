//! Development tool of the stage 23 cross-machine proof (`scripts/mac/cross-session.sh`): the host's scripted user.
//!
//! Connects to the UI pipe of the installed `SwapdeskService` as the logged-in user, exactly as `Swapdesk.exe` does
//! (`swapdesk_app::connect::open_pipe`: identification-level open, server PID compared with the SCM's), then runs
//! `swapdesk_crossdrive::host_drive`: opens pairing, confirms the code the viewer reports, grants the viewer VIEW,
//! CONTROL and AUDIO, answers the incoming request with Accept, waits for the session to end and forgets the device.
//!
//!     cross_host_driver --peer-code <file> [--pair-timeout <s>] [--session-timeout <s>]
//!
//! The viewer's code arrives in `<file>` (written by the orchestrator). Markers go to stdout; the exit code is 0 only
//! when the whole script ran and the device was forgotten. It injects no input itself: the viewer's pointer moves are
//! what the service's user helper receives.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use swapdesk_app::cli::Options;
use swapdesk_app::connect::open_pipe;
use swapdesk_crossdrive::{HostConfig, host_drive};
use swapdesk_ui::service_backend::ServiceBackend;
use swapdesk_ui::{Permission, Permissions, UiWaker};

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn seconds(args: &[String], flag: &str, default: u64) -> Duration {
    Duration::from_secs(value(args, flag).and_then(|v| v.parse().ok()).unwrap_or(default))
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(peer_code) = value(&args, "--peer-code") else {
        eprintln!("usage: cross_host_driver --peer-code <file> [--pair-timeout <s>] [--session-timeout <s>]");
        return ExitCode::from(2);
    };
    let log = |line: &str| println!("{line}");
    let fail = |why: &str| {
        println!("HOST_RESULT FAIL {why}");
        ExitCode::FAILURE
    };

    // The same open as the shell's: the pipe's server must be the service the SCM reports.
    let conn = match open_pipe(&Options::default()) {
        Ok(c) => c,
        Err(e) => return fail(&format!("cannot open the UI pipe: {e}")),
    };
    let (reader, writer) = match conn.split() {
        Ok(rw) => rw,
        Err(e) => return fail(&format!("cannot split the pipe: {e}")),
    };
    let backend = match ServiceBackend::start(reader, writer, Arc::new(NoWake), std::process::id()) {
        Ok(b) => b,
        Err(e) => return fail(&format!("handshake: {e}")),
    };
    log(&format!("HOST_CONNECTED win_session={}", backend.win_session()));

    let cfg = HostConfig {
        peer_code: PathBuf::from(peer_code),
        pair_timeout: seconds(&args, "--pair-timeout", 100),
        session_timeout: seconds(&args, "--session-timeout", 120),
        grants: Permissions::NONE.with(Permission::Control).with(Permission::Audio),
    };
    let (outcome, result) = host_drive(&backend, &cfg, &log);
    match result {
        Ok(()) if outcome.session_active && outcome.session_ended && outcome.forgotten => {
            println!("HOST_RESULT PASS");
            ExitCode::SUCCESS
        }
        Ok(()) => fail(&format!("incomplete: {outcome:?}")),
        Err(e) => fail(&format!("{e} ({outcome:?})")),
    }
}
