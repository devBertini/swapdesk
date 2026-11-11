//! `swapdesk-app-mac` (the executable `scripts/mac/build-app.sh` installs as `Swapdesk.app/Contents/MacOS/Swapdesk`): see `swapdesk_app_mac::app`.
//!
//! A GUI program: a failure before the first window goes to stderr and the exit status (an app started by `open` has
//! no terminal; the trace log in `~/Library/Application Support/Swapdesk` keeps what the service reported).

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The update watcher (UPDATES.md 7.2): the old bundle's binary, started by the app it replaces; no window.
    if let [flag, pid, old, new, running] = args.as_slice()
        && flag == "--update-watch"
    {
        std::process::exit(swapdesk_app_mac::update::watch(pid, old, new, running));
    }
    if let Err(e) = swapdesk_app_mac::app::run(&args) {
        eprintln!("Swapdesk could not start: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    // `Swapdesk.app` is built and proven on the MacBook (stage 23); `Swapdesk.exe` is `swapdesk-app`.
    eprintln!("Swapdesk for macOS only runs on macOS.");
    std::process::exit(2);
}
