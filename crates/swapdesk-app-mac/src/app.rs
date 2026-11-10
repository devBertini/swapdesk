//! Assembles `Swapdesk.app` (D27): the secrets, the trace log, the in-process service, the viewer pipeline and the shell,
//! then runs the AppKit loop.
//!
//! ```text
//!  main thread                                  other threads
//!  shell (swapdesk-shell-mac) ── ObservingBackend ── ServiceBackend ──(in-memory pipe)── UiHub ── ServiceCore (LAN)
//!     │ VideoSlot = MacVideoSlot                        ▲ ViewerSink = MacViewer ◄── media from the pipe's reader
//!     │ ViewerInput = MacInput                          └ ViewerOut (feedback, input) ◄── decode / input threads
//! ```
//!
//! `NSApplication::terminate` ends the process with `exit`, so `run` never returns on a normal quit: what must be undone
//! (the pipeline's threads, the service's sockets and DNS-SD goodbye, a scratch keychain) is registered with `atexit`
//! ([`set_teardown`]).
//!
//! Secrets: the logged-in user's login keychain, or with `--scratch` / `--selftest` a throwaway keychain file in the
//! temp directory (never the login keychain, never the owner's Swapdesk identity); the scratch is removed at exit.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use swapdesk_core_mac::macos_version;
use swapdesk_net_mac::MacNetInterfaces;
use swapdesk_platform::{Clock, Level, MonoMicros, SecretStore, TraceSink};
use swapdesk_secret_mac::{
    KeychainLocation, MacSecretStore, SecretLocation, create_keychain_file, delete_keychain_file,
};
use swapdesk_shell_mac::{ViewerHooks, Waker, run_with_host, system_dark, system_language};
use swapdesk_ui::mock::{MockBackend, Scenario};
use swapdesk_ui::service_backend::ViewerSink;
use swapdesk_ui::{UiBackend, UiState};

use crate::cli;
use crate::pipeline::{MacViewer, ObservingBackend, ViewerOptions};
use crate::service::{InProcessService, ServiceParams};
use crate::slot::MacVideoSlot;
use crate::trace::{DEFAULT_CAP, FileTrace};

static SCRATCH_COUNT: AtomicU32 = AtomicU32::new(0);

type Teardown = Box<dyn FnOnce() + Send>;
static TEARDOWN: Mutex<Option<Teardown>> = Mutex::new(None);

unsafe extern "C" {
    /// `atexit(3)`: registers a function `exit` calls (what `NSApplication::terminate` ends with).
    fn atexit(callback: extern "C" fn()) -> i32;
}

extern "C" fn run_teardown() {
    let job = TEARDOWN.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(job) = job {
        // On a thread of its own: `exit` has already destroyed the main thread's thread-locals when it calls this, and
        // the service's shutdown needs tokio's ("The Tokio context thread-local variable has been destroyed" aborted the
        // process here, 2026-11-08, leaving the scratch keychain behind).
        let _ = std::thread::Builder::new().name("swapdesk-teardown".into()).spawn(job).map(|t| t.join());
    }
}

/// Registers what must be undone when the process exits (the AppKit loop never returns on a quit).
fn set_teardown(job: Teardown) {
    *TEARDOWN.lock().unwrap_or_else(PoisonError::into_inner) = Some(job);
    // SAFETY: `run_teardown` is an `extern "C" fn()` with no arguments that stays valid for the life of the process;
    // `atexit` only stores the pointer.
    let rc = unsafe { atexit(run_teardown) };
    if rc != 0 {
        eprintln!("swapdesk: atexit failed ({rc}); the service is not stopped cleanly at exit");
    }
}

/// The machine's monotonic clock, shared by everything that stamps media (C3).
struct Monotonic(Instant);

impl Clock for Monotonic {
    fn now(&self) -> MonoMicros {
        MonoMicros(u64::try_from(self.0.elapsed().as_micros()).unwrap_or(u64::MAX))
    }
}

/// The throwaway keychain file, folder and items of a `--scratch` run, removed when dropped.
struct Scratch {
    dir: PathBuf,
    keychain_file: PathBuf,
    store: Arc<MacSecretStore>,
}

impl Scratch {
    fn new() -> Result<Self, String> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let tag =
            format!("swapdesk-app-{}-{nanos}-{}", std::process::id(), SCRATCH_COUNT.fetch_add(1, Ordering::SeqCst));
        let dir = std::env::temp_dir().join(&tag);
        std::fs::create_dir_all(&dir).map_err(|e| format!("scratch folder: {e}"))?;
        let home = PathBuf::from(std::env::var_os("HOME").ok_or("$HOME is not set")?);
        let keychain_file = dir.join(format!("{tag}.keychain-db"));
        create_keychain_file(&keychain_file, "swapdesk-scratch-password")
            .map_err(|e| format!("throwaway keychain: {e}"))?;
        let mut location = SecretLocation::login(&home).with(tag, dir.join("folder"));
        location.keychain = KeychainLocation::File(keychain_file.clone());
        let store = Arc::new(MacSecretStore::open(&location).map_err(|e| format!("secret store: {e}"))?);
        Ok(Self { dir, keychain_file, store })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.store.delete_all();
        let _ = delete_keychain_file(&self.keychain_file);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// "MacBook de Fulano": the name the owner gave this Mac (System Settings > General > About), `Mac` when unreadable.
fn computer_name() -> String {
    std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".to_string())
}

fn usage() -> String {
    let names: Vec<&str> = Scenario::ALL.iter().map(|s| s.name()).collect();
    format!("{}\n\nScenarios: {}", cli::USAGE, names.join(", "))
}

/// Runs the app with the process arguments (after the program name). Returns only on an error or when the loop ends
/// without `terminate`; a normal quit exits the process from inside AppKit.
pub fn run(args: &[String]) -> Result<(), String> {
    let options = cli::parse(args).map_err(|e| format!("{e}\n\n{}", usage()))?;
    if options.help {
        println!("{}", usage());
        return Ok(());
    }
    let waker = Arc::new(Waker::new());
    let mut state = UiState::new(system_language(), system_dark());

    if let Some(name) = &options.mock {
        let scenario =
            Scenario::from_name(name).ok_or_else(|| format!("Unknown scenario \"{name}\".\n\n{}", usage()))?;
        let backend: Arc<dyn UiBackend> = Arc::new(MockBackend::new(waker.clone()));
        scenario.apply_to(&mut state);
        if let Some(path) = &options.shot {
            swapdesk_shell_mac::set_shot(path.clone());
        }
        let startup = if scenario == Scenario::Carregando { state.startup_commands() } else { Vec::new() };
        if let Some(file) = options.selftest.clone() {
            crate::selftest::schedule_mock(file, Duration::from_secs(2));
        }
        run_with_host(state, backend, waker, startup, ViewerHooks::default());
        return Ok(());
    }

    let scratch = if options.use_scratch() { Some(Scratch::new()?) } else { None };
    let store: Arc<MacSecretStore> = match &scratch {
        Some(s) => Arc::clone(&s.store),
        None => {
            let home = std::env::var_os("HOME").ok_or("$HOME is not set")?;
            Arc::new(
                MacSecretStore::open(&SecretLocation::login(Path::new(&home)))
                    .map_err(|e| format!("secret store: {e}"))?,
            )
        }
    };
    // Development: `SWAPDESK_DEBUG=1` (e.g. `open --env SWAPDESK_DEBUG=1 Swapdesk.app`) logs at debug level too.
    let debug = options.use_scratch() || std::env::var_os("SWAPDESK_DEBUG").is_some();
    let level = if debug { Level::Debug } else { Level::Info };
    let trace: Arc<dyn TraceSink> =
        Arc::new(FileTrace::open(store.dir(), "trace.log", level, DEFAULT_CAP).map_err(|e| format!("trace log: {e}"))?);
    let clock: Arc<dyn Clock> = Arc::new(Monotonic(Instant::now()));

    // C5.1: the per-user settings (ui.bin under ~/Library/Application Support/Swapdesk) are read before the first
    // paint. A scratch run (self-test, host drive) never touches the user's file.
    let prefs_trace = Arc::clone(&trace);
    if !options.use_scratch()
        && let Some(home) = std::env::var_os("HOME")
    {
        swapdesk_ui::prefs::set_note_hook(move |line| prefs_trace.log(Level::Info, "app", format_args!("{line}")));
        let dir = Path::new(&home).join("Library").join("Application Support").join("Swapdesk");
        state.load_prefs(Box::new(swapdesk_ui::prefs::FilePrefs::in_dir(&dir)));
    }

    let viewer =
        MacViewer::new(ViewerOptions::default()).map_err(|e| format!("the viewer pipeline did not start: {e}"))?;
    let secrets: Arc<dyn SecretStore> = store.clone();
    let mut params = ServiceParams::new(
        &computer_name(),
        &macos_version(),
        secrets,
        store.dir().to_path_buf(),
        Arc::new(MacNetInterfaces),
        Arc::clone(&clock),
        Arc::clone(&trace),
    );
    // A scratch run is a development instance: the system picks the ports (a Mac that runs its own Swapdesk keeps
    // 47820/47821), and the self-test does not announce itself on the network. The host drive keeps the real ports:
    // the viewer adds this Mac by its address, which means port 47820.
    if options.use_scratch() && options.drive_host.is_none() {
        params.ports = Some((0, 0));
    }
    params.discovery = options.selftest.is_none();
    // The network's changes come from SystemConfiguration's dynamic store, once per burst, without a timer.
    params.network_notices = Some(Box::new(|changed| {
        swapdesk_net_mac::NetworkWatcher::start(changed)
            .map(|w| Box::new(w) as Box<dyn Send>)
            .map_err(|e| format!("network notices: {e}"))
    }));
    // This Mac hosts (stage 24): the helper loop runs in this process. A self-test run stays a viewer only.
    if options.selftest.is_none() {
        params.host_helper = Some(crate::host::host_helper(Arc::clone(&clock)));
    }
    let sink: Arc<dyn ViewerSink> = viewer.clone();
    let service = InProcessService::start(params, waker.clone(), Some(sink))
        .map_err(|e| format!("the service did not start: {e}"))?;
    let backend = service.backend();
    viewer.set_out(backend.clone());
    let user = state.user_prefs();
    viewer.set_user_prefs(user.keyboard_mode, user.swap_ctrl_cmd);
    if options.selftest.is_none() {
        crate::host::ask_for_grants(&*trace);
    }
    let addrs = service.local_addrs().iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
    trace.log(Level::Info, "app", format_args!("Swapdesk.app started (scratch: {})", options.use_scratch()));

    // D32: the app hosts from inside its own process, so it must come back after a login. A normal run only.
    if options.registers_login_item() {
        let (level, line) = crate::login_item::register();
        trace.log(level, "app", format_args!("{line}"));
    }
    let note_trace = Arc::clone(&trace);
    let hooks = ViewerHooks {
        video: Some(Arc::new(MacVideoSlot::new(&viewer))),
        input: Some(Arc::new(viewer.input())),
        note: Some(Arc::new(move |text: &str| note_trace.log(Level::Info, "app", format_args!("{text}")))),
    };
    // The host drive owns the backend's events: the window gets a backend that says nothing.
    let observing: Arc<dyn UiBackend> = match &options.drive_host {
        Some(dir) => {
            let window = Arc::new(crate::drive::Silent::default());
            let wake_ui = Arc::clone(&waker);
            let tee = crate::drive::Tee {
                real: Arc::clone(&backend),
                window: Arc::clone(&window),
                wake: Arc::new(move || swapdesk_ui::UiWaker::wake(&*wake_ui)),
            };
            crate::drive::start(dir, tee, Arc::clone(&trace))?;
            swapdesk_shell_mac::set_snapshot_dir(dir.to_path_buf());
            window
        }
        None => Arc::new(ObservingBackend::new(backend, Arc::clone(&viewer))),
    };
    let startup = if options.drive_host.is_some() { Vec::new() } else { state.startup_commands() };

    if let Some(file) = options.selftest.clone() {
        crate::selftest::schedule(file, Duration::from_secs(2), addrs, Arc::clone(&viewer));
    }
    // Development: a script on another machine drives the windows (dev-harness builds).
    if let Some(dir) = &options.ui_script {
        crate::ui_script::start(dir, Arc::clone(&viewer))?;
    }

    let stop_viewer = Arc::clone(&viewer);
    set_teardown(Box::new(move || {
        stop_viewer.shutdown();
        let mut service = service;
        service.shutdown();
        drop(scratch);
    }));
    run_with_host(state, observing, waker, startup, hooks);
    // `terminate` exits from inside the loop; this is reached only if the loop ends by other means.
    run_teardown();
    Ok(())
}
