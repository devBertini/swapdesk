//! Stage 23 cross-machine proof: the viewer's scripted user. Windows only; see `win.rs`.

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    win::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("cross_viewer_driver runs on Windows only (it drives the installed SwapdeskService)");
}
