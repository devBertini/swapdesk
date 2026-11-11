//! `Swapdesk.exe`: picks the backend, wires the viewer pipeline and starts the interface.
//!
//! - `--mock <scenario>` (development builds, cargo feature `mock`): the scripted `MockBackend`, no service.
//! - Otherwise: the UI pipe of this session (`ui-<session id>`), checked against the service's PID (the SCM's for
//!   `SwapdeskService`, or `--service-pid <pid>` in development; `--instance <n>` adds the suffix `-<n>` to the pipe
//!   name). When the service cannot be reached the UI shows its service-down state.
//!
//! - `--elevated <ticket>`: only the elevated instance that confirms one action with the service, then exit.
//!
//! A GUI subsystem program: nothing is printed, a failure is a message box.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::sync::Arc;

    use swapdesk_app::cli;
    use swapdesk_app::connect;
    use swapdesk_app::reconnect::{Connector, Linked, Reconnecting, Timing};
    use swapdesk_app::viewer::{ObservingBackend, ViewerOptions, ViewerPipeline};
    use swapdesk_shell_win::{Waker, message_box, run, run_with_host, system_dark, system_language};
    use swapdesk_ui::mock::{MockBackend, Scenario};
    use swapdesk_ui::{UiBackend, UiState};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match cli::parse(&args) {
        Ok(o) => o,
        Err(e) => {
            // A wrong command line is a script's or a developer's mistake: one line on stderr (no window), exit 2.
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if options.help {
        return message_box(&usage());
    }
    // The elevated instance is only that: its confirmation window, no viewer pipeline, no preferences, no autostart, no startup
    // commands, no panic record. Its exit code says how the confirmation ended (ELEVATION.md 3.5).
    if let Some(ticket) = &options.elevated {
        let code = swapdesk_app::elevate::run_instance(&options, ticket, swapdesk_app::elevate::confirm_in_window);
        std::process::exit(i32::from(code));
    }
    let language = system_language();
    let waker = Arc::new(Waker::new());
    let mut state = UiState::new(language, system_dark());
    // D12: a panic leaves a record; the next start announces the last one once (the marker is renamed as the
    // notice is handed to the state, before the first frame).
    if let Some(dir) = swapdesk_sys_win::user_log_dir() {
        swapdesk_sys_win::install_panic_hook(&dir, "Swapdesk");
        if swapdesk_sys_win::take_crash_marker(&dir) {
            state.show_crash_notice();
        }
    }

    if let Some(name) = &options.mock {
        if !cfg!(feature = "mock") {
            return message_box("This build has no mock backend (build with the `mock` feature).");
        }
        let Some(scenario) = Scenario::from_name(name) else {
            return message_box(&format!("Unknown scenario \"{name}\".\n\n{}", usage()));
        };
        let backend: Arc<dyn UiBackend> = Arc::new(MockBackend::new(waker.clone()));
        scenario.apply_to(&mut state);
        // "carregando" leaves its data unanswered: the requests go out and the mock answers them after its delays, so
        // the placeholders can be seen. Every other scenario already holds its data.
        let startup = if scenario == Scenario::Carregando { state.startup_commands() } else { Vec::new() };
        if let Err(e) = run(state, backend, waker, startup) {
            message_box(&format!("Swapdesk could not start: {e}"));
        }
        return;
    }

    let pipeline = match ViewerPipeline::new(ViewerOptions::default()) {
        Ok(p) => p,
        Err(e) => return message_box(&format!("Swapdesk could not start the viewer: {e}")),
    };
    pipeline.set_waker(waker.clone());
    // C5.1: the per-user settings (ui.bin under %APPDATA%/Swapdesk) are read before the first paint. A development
    // instance (`--instance`, `--service-pid`) keeps its own defaults and never touches the user's file.
    if options.instance.is_none() && options.service_pid.is_none() {
        swapdesk_ui::prefs::set_note_hook(|line| swapdesk_shell_win::trace_line(&format!("swapdesk: {line}")));
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let dir = std::path::Path::new(&appdata).join("Swapdesk");
            state.load_prefs(Box::new(swapdesk_ui::prefs::FilePrefs::in_dir(&dir)));
            let p = state.user_prefs();
            pipeline.set_user_prefs(p.keyboard_mode, p.swap_ctrl_cmd, p.release_key);
        }
    }
    // The service's pipe, tried for 30 s at launch and again after a break (a service that starts after the sign-in, a
    // restarted service); past that, every command answers ServiceDown and the UI shows that state.
    let connector: Connector = {
        let (options, waker, pipeline) = (options.clone(), waker.clone(), pipeline.clone());
        Box::new(move || {
            let service = connect::connect(&options, waker.clone(), pipeline.clone()).ok()?;
            pipeline.set_out(service.clone());
            let link = service.clone();
            Some(Linked {
                backend: Arc::new(ObservingBackend::new(service, pipeline.clone())),
                is_down: Box::new(move || link.is_down()),
            })
        })
    };
    let backend: Arc<dyn UiBackend> = Arc::new(Reconnecting::start(connector, waker.clone(), Timing::default()));
    let startup = state.startup_commands();
    if let Err(e) = run_with_host(state, backend, waker, startup, Some(pipeline.clone())) {
        message_box(&format!("Swapdesk could not start: {e}"));
    }
    pipeline.shutdown();
}

#[cfg(windows)]
fn usage() -> String {
    let names: Vec<&str> = swapdesk_ui::mock::Scenario::ALL.iter().map(|s| s.name()).collect();
    let dev = if swapdesk_app::cli::DEV_FLAGS { " [--service-pid <pid>] [--instance <n>]" } else { "" };
    format!(
        "Swapdesk{dev}\nSwapdesk --elevated <ticket>{dev}\nSwapdesk --mock <scenario>\n\nScenarios: {}",
        names.join(", ")
    )
}

#[cfg(not(windows))]
fn main() {
    // The macOS shell (`swapdesk-shell-mac`) is built and proven on the MacBook (stage 23).
}
