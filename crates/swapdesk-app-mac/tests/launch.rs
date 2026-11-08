//! Launch check: the signed bundle opens through LaunchServices (`open`), its main window is on screen and the in-process
//! service listens, with the throwaway secrets of `--selftest`, then the app quits by itself and removes its scratch.
//!
//! Needs the bundle (`bash scripts/mac/build-app.sh`, or `SWAPDESK_APP=<path to Swapdesk.app>`) and a logged-in GUI
//! session at the Mac (an SSH session can usually start a GUI app with `open` when someone is logged in at the console;
//! with nobody logged in, or the screen asleep, it cannot). Opens a window for a moment, so it is
//! `#[ignore = "interactive: ..."]`: run it with
//! `cargo nextest run -p swapdesk-app-mac --run-ignored only -E 'test(/interactive/)'`.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The app's own watchdog is 30 s; this is a little more.
const WAIT: Duration = Duration::from_secs(60);

fn bundle() -> PathBuf {
    std::env::var_os("SWAPDESK_APP")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/bundle/Swapdesk.app"))
}

fn value<'a>(report: &'a str, key: &str) -> Option<&'a str> {
    report.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

#[test]
#[ignore = "interactive: opens the app"]
fn interactive_launch_opens_the_bundle_with_open_and_the_selftest_sees_its_window_and_service() {
    let app = bundle();
    assert!(
        app.join("Contents/MacOS/Swapdesk").exists(),
        "build the bundle first (bash scripts/mac/build-app.sh): {}",
        app.display()
    );
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let file = std::env::temp_dir().join(format!("swapdesk-selftest-{}-{nanos}.txt", std::process::id()));
    let _ = std::fs::remove_file(&file);

    // `-n` a new instance, `-W` until it quits; the bundle is opened as the user would open it.
    let mut open = Command::new("open")
        .args(["-n", "-W", "-a"])
        .arg(&app)
        .args(["--args", "--selftest"])
        .arg(&file)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("`open` starts");
    let end = Instant::now() + WAIT;
    let status = loop {
        if let Some(s) = open.try_wait().expect("wait") {
            break s;
        }
        if Instant::now() > end {
            let _ = open.kill();
            panic!("the app did not quit within {WAIT:?}; selftest file present: {}", file.exists());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut stderr = String::new();
    if let Some(mut e) = open.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut e, &mut stderr);
    }
    assert!(status.success(), "`open` failed ({status}): {stderr}");

    let report =
        std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("no selftest file ({e}); `open` said: {stderr}"));
    let _ = std::fs::remove_file(&file);
    assert_eq!(value(&report, "selftest"), Some("ok"), "{report}");
    let windows: usize = value(&report, "windows").and_then(|v| v.parse().ok()).unwrap_or(0);
    assert!(windows >= 1, "no window: {report}");
    assert!(
        report.lines().any(|l| l.starts_with("window.") && l.contains("Swapdesk") && l.ends_with("|visible=true")),
        "the main window is not on screen: {report}"
    );
    assert!(value(&report, "service_addrs").is_some_and(|a| a.contains(':')), "the service does not listen: {report}");
    assert_eq!(value(&report, "pipeline.attached"), Some("false"), "no session, no video view: {report}");
    assert_eq!(value(&report, "pipeline.input_session"), Some("0"), "{report}");

    // The scratch keychain file and folder of that run are gone. The app's `exit` handler removes them, and `open -W`
    // returns when the process is gone to LaunchServices, which can be before the handler ends: wait a few seconds.
    let pid = value(&report, "pid").expect("pid");
    let prefix = format!("swapdesk-app-{pid}-");
    let leftovers = || -> Vec<PathBuf> {
        std::fs::read_dir(std::env::temp_dir())
            .map(|d| {
                d.flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)).map(|e| e.path()).collect()
            })
            .unwrap_or_default()
    };
    let end = Instant::now() + Duration::from_secs(15);
    while !leftovers().is_empty() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
    let left = leftovers();
    assert!(left.is_empty(), "the run's scratch was not removed: {left:?}");
}
