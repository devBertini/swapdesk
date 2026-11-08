//! Interactive window checks: they open real windows on a Mac with a logged-in session, so they are off unless asked
//! (`interactive: opens windows`). Run on the Mac:
//! `SWAPDESK_INTERACTIVE=1 cargo run -p swapdesk-shell-mac --example window_checks`.
//! It is an example, not a test target, because AppKit needs the main thread (libtest's test threads are not) and a
//! harness-less test breaks `cargo nextest run --workspace`.

#[cfg(target_os = "macos")]
fn main() {
    if std::env::var("SWAPDESK_INTERACTIVE").as_deref() != Ok("1") {
        println!("skipped: interactive, opens windows (set SWAPDESK_INTERACTIVE=1)");
        return;
    }
    match swapdesk_shell_mac::selftest::run_window_checks() {
        Ok(()) => println!("window checks: ok"),
        Err(e) => {
            eprintln!("window checks FAILED: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("skipped: macOS only");
}
