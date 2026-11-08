//! Runs the macOS shell on the mock backend, for looking at every screen on a Mac:
//! `cargo run -p swapdesk-shell-mac --example mock -- [scenario]` (names as `swapdesk_ui::mock::Scenario::name`,
//! default `dispositivos`). Without an argument the mock backend answers live and the startup requests go out at once.

#[cfg(target_os = "macos")]
fn main() {
    use std::sync::Arc;

    use swapdesk_shell_mac::{Waker, run, system_dark, system_language};
    use swapdesk_ui::UiState;
    use swapdesk_ui::mock::{MockBackend, Scenario};

    let waker = Arc::new(Waker::new());
    let backend = Arc::new(MockBackend::new(waker.clone()));
    let mut state = UiState::new(system_language(), false);
    let arg = std::env::args().nth(1);
    let startup = match arg.as_deref().and_then(Scenario::from_name) {
        // A scenario carries its own appearance (the dark ones are dark).
        Some(sc) => {
            sc.apply_to(&mut state);
            Vec::new()
        }
        None => {
            let _ = state.set_dark(system_dark());
            state.startup_commands()
        }
    };
    run(state, backend, waker, startup);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the macOS shell example runs on macOS only");
}
