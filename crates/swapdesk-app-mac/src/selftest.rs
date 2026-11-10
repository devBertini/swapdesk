//! `Swapdesk --selftest <file>`: development check that the app really came up under launchd / `open`.
//!
//! A few seconds after the run loop starts, a block on the main queue writes what the process sees to `<file>` (one
//! `key=value` per line, written to a temporary name and renamed so a reader never sees half of it) and asks the app
//! to quit. A watchdog thread ends the process with status 3 if the run loop never gets there. The launch check in
//! `tests/launch.rs` reads the file.
//!
//! Lines: `selftest=ok`, `pid`, `windows` (count), `window.<n>=<title>|visible=<bool>`, `service_addrs`,
//! `status_item`, `main_window.visible`, `main_window.after_close.visible` (closing the window only hides
//! it), `main_window.after_reopen.visible` (the Dock-icon reopen), `pipeline.attached` (the viewer window's video view exists: false without a session), `pipeline.input_session`,
//! `activation_policy`, `click.activated` (a mouse click sent to a sidebar entry of the main window changed the screen).
//!
//! Apple documentation: `NSApplication.windows`, `NSWindow.isVisible` / `title`, `NSApplication.terminate(_:)`,
//! `NSApplication.activationPolicy()` (developer.apple.com/documentation/appkit/nsapplication).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2_app_kit::NSApplication;
use objc2_foundation::MainThreadMarker;

use crate::pipeline::MacViewer;

/// How long the run loop has to reach the check before the watchdog ends the process.
const WATCHDOG: Duration = Duration::from_secs(30);

/// The name the mock self-test types into the rename field.
const MOCK_NAME: &str = "Mac da sala";
/// How long the mock backend gets to answer the rename before the report is written.
const MOCK_ANSWER: Duration = Duration::from_millis(1500);

/// Ends the process with status 3 if the run loop never reaches the check.
fn watchdog(file: &std::path::Path) {
    let file = file.with_extension("watchdog");
    std::thread::spawn(move || {
        std::thread::sleep(WATCHDOG);
        let _ = std::fs::write(file, "the run loop did not reach the self-test\n");
        std::process::exit(3);
    });
}

/// Writes the report where a reader never sees half of it and asks the app to quit.
fn finish(file: &std::path::Path, report: String) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let tmp = file.with_extension("tmp");
    if std::fs::write(&tmp, report).and_then(|()| std::fs::rename(&tmp, file)).is_err() {
        std::process::exit(4);
    }
    NSApplication::sharedApplication(mtm).terminate(None);
}

/// Queues the check `delay` after the run loop starts.
pub fn schedule(file: PathBuf, delay: Duration, service_addrs: String, viewer: Arc<MacViewer>) {
    watchdog(&file);
    let when = DispatchTime::try_from(delay).unwrap_or(DispatchTime::NOW);
    let queued = DispatchQueue::main().after(when, move || {
        let Some(mtm) = MainThreadMarker::new() else { return };
        let report = report(mtm, &service_addrs, &viewer);
        finish(&file, report);
    });
    if queued.is_err() {
        std::process::exit(5);
    }
}

/// `Swapdesk --mock <scenario> --selftest <file>`: the interface alone, on the mock backend. `delay` after the run
/// loop starts, a click on the sidebar, then a paired device renamed through its native text field with real key
/// events; once the mock backend had time to answer, the report is written: `selftest=ok`, `mode=mock`,
/// `click.activated`, `field.typed` (the field held exactly the typed name), `rename.ok` (the device carries it and
/// the field closed).
pub fn schedule_mock(file: PathBuf, delay: Duration) {
    watchdog(&file);
    let when = DispatchTime::try_from(delay).unwrap_or(DispatchTime::NOW);
    let queued = DispatchQueue::main().after(when, move || {
        let click = swapdesk_shell_mac::selftest_click() == Some(true);
        let typed = swapdesk_shell_mac::selftest_rename_begin(MOCK_NAME) == Some(true);
        let later = DispatchTime::try_from(MOCK_ANSWER).unwrap_or(DispatchTime::NOW);
        let queued = DispatchQueue::main().after(later, move || {
            let renamed = swapdesk_shell_mac::selftest_rename_end(MOCK_NAME) == Some(true);
            let report =
                format!("selftest=ok\nmode=mock\nclick.activated={click}\nfield.typed={typed}\nrename.ok={renamed}\n");
            finish(&file, report);
        });
        if queued.is_err() {
            std::process::exit(5);
        }
    });
    if queued.is_err() {
        std::process::exit(5);
    }
}

fn report(mtm: MainThreadMarker, service_addrs: &str, viewer: &MacViewer) -> String {
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    let mut out = String::from("selftest=ok\n");
    out.push_str(&format!("pid={}\n", std::process::id()));
    out.push_str(&format!("activation_policy={:?}\n", app.activationPolicy()));
    out.push_str(&format!("windows={}\n", windows.count()));
    for (i, w) in windows.to_vec().iter().enumerate() {
        out.push_str(&format!("window.{i}={}|visible={}\n", w.title(), w.isVisible()));
    }
    // D32: closing the main window only hides it, the status item stays, and a Dock-icon click shows it again.
    out.push_str(&format!("status_item={}\n", swapdesk_shell_mac::status_item_present()));
    out.push_str(&format!("main_window.visible={:?}\n", swapdesk_shell_mac::main_window_visible()));
    // The painted main window takes the mouse: a click on a sidebar entry opens its screen.
    out.push_str(&format!("click.activated={}\n", swapdesk_shell_mac::selftest_click() == Some(true)));
    swapdesk_shell_mac::close_main_window();
    out.push_str(&format!("main_window.after_close.visible={:?}\n", swapdesk_shell_mac::main_window_visible()));
    swapdesk_shell_mac::reopen_main_window();
    out.push_str(&format!("main_window.after_reopen.visible={:?}\n", swapdesk_shell_mac::main_window_visible()));
    out.push_str(&format!("service_addrs={service_addrs}\n"));
    let stats = viewer.stats();
    out.push_str(&format!("pipeline.attached={}\n", stats.attached));
    out.push_str(&format!("pipeline.input_session={}\n", viewer.input_session()));
    out
}
