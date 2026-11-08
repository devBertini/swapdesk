//! Stage 23 cross-machine proof: the headless Mac viewer. macOS only; see `mac.rs`.

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    mac::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("cross_viewer runs on macOS only (scripts/mac/cross-session.sh ships it to the MacBook)");
}
