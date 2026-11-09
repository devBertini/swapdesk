//! `Swapdesk --drive-host <dir>` (development, `dev-harness` feature): the cross-machine proof with this Mac as the
//! host and no human at it.
//!
//! A thread runs `swapdesk_crossdrive::host_drive` on the app's own backend, exactly as a person at the window would:
//! it opens pairing, reads the viewer's code from `<dir>/peer-code.txt` (written by the orchestrator), confirms it,
//! grants control and audio, accepts the session and waits for it to end; the device paired in the run is forgotten.
//! The identity is the scratch one (a throwaway keychain), so the owner's pairings are never touched. Markers go to
//! `<dir>/host.log` (`HOST_CODE <code>`, `HOST_READY <id>`, ...) and the last line is `HOST_RESULT PASS` or
//! `HOST_RESULT FAIL <why>`; the app then quits.

use std::path::Path;
use std::sync::Arc;

use swapdesk_platform::TraceSink;
use swapdesk_ui::service_backend::ServiceBackend;
use swapdesk_ui::{Command, Ticket, UiBackend, UiEvent};

/// The window's backend while the drive owns the real one: it queues nothing and reports nothing.
pub struct Silent;

impl UiBackend for Silent {
    fn send(&self, _command: Command) -> Ticket {
        Ticket(0)
    }

    fn drain(&self) -> Vec<UiEvent> {
        Vec::new()
    }
}

#[cfg(not(feature = "dev-harness"))]
pub fn start(_dir: &Path, _backend: Arc<ServiceBackend>, _trace: Arc<dyn TraceSink>) -> Result<(), String> {
    Err("--drive-host needs a build with the dev-harness feature (scripts/mac/build-app.sh --dev-harness)".into())
}

#[cfg(feature = "dev-harness")]
pub fn start(dir: &Path, backend: Arc<ServiceBackend>, trace: Arc<dyn TraceSink>) -> Result<(), String> {
    use std::io::Write;
    use std::time::Duration;

    use swapdesk_crossdrive::{HostConfig, host_drive};
    use swapdesk_platform::Level;
    use swapdesk_ui::{Permission, Permissions};

    std::fs::create_dir_all(dir).map_err(|e| format!("drive folder {}: {e}", dir.display()))?;
    let log_path = dir.join("host.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("{}: {e}", log_path.display()))?;
    let cfg = HostConfig {
        peer_code: dir.join("peer-code.txt"),
        pair_timeout: Duration::from_secs(150),
        session_timeout: Duration::from_secs(180),
        grants: Permissions::NONE.with(Permission::Control).with(Permission::Audio),
    };
    std::thread::Builder::new()
        .name("swapdesk-drive-host".into())
        .spawn(move || {
            let file = std::sync::Mutex::new(file);
            let log = |line: &str| {
                trace.log(Level::Info, "drive", format_args!("{line}"));
                let mut f = file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = writeln!(f, "{line}");
                let _ = f.flush();
            };
            log("HOST_DRIVE_START");
            let (out, result) = host_drive(&*backend, &cfg, &log);
            let code = match (&result, out.session_active && out.session_ended) {
                (Ok(()), true) => {
                    log("HOST_RESULT PASS");
                    0
                }
                (Ok(()), false) => {
                    log("HOST_RESULT FAIL the session did not go active and end");
                    1
                }
                (Err(e), _) => {
                    log(&format!("HOST_RESULT FAIL {e}"));
                    1
                }
            };
            // `exit` runs the registered teardown (the scratch keychain, the service, the pipeline).
            std::process::exit(code);
        })
        .map_err(|e| format!("drive thread: {e}"))?;
    Ok(())
}
