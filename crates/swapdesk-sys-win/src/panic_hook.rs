//! A panic leaves a trace (evaluation item D12). The UI and the helpers build with `panic = "abort"` (D15), so
//! without this a panic would vanish. [`install_panic_hook`] makes the hook append one record to `<dir>/trace.log`
//! (same line shape as [`FileTrace`](crate::FileTrace): `<unix ms> ERROR panic <text>`, one line), write the marker
//! `<dir>/last-crash.txt` (the same facts with the backtrace on its own lines, at most 16 KiB) and then call the
//! hook that was there before.
//!
//! F-SEC-01: a record holds the time, the binary name, the thread name, the panic message (cut to 512 characters)
//! with its source location, and the backtrace of code addresses and function names. Nothing else of the process.
//!
//! The next start of the UI calls [`take_crash_marker`]: it renames the marker to `last-crash.seen.txt` and says
//! whether there was one, so a crash is announced once.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The marker a crash leaves.
pub const CRASH_MARKER: &str = "last-crash.txt";
/// The marker once its notice was shown.
pub const CRASH_SEEN: &str = "last-crash.seen.txt";
/// The marker's size cap.
pub const MARKER_CAP_BYTES: usize = 16 * 1024;
const MESSAGE_CAP_CHARS: usize = 512;
const TRACE_FILE: &str = "trace.log";

/// What a panic carries, as plain text.
pub struct CrashFacts<'a> {
    pub unix_ms: u128,
    pub binary: &'a str,
    pub thread: &'a str,
    pub message: &'a str,
    pub location: &'a str,
    pub backtrace: &'a str,
}

fn cut_bytes(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The trace line (no line break inside, one trailing `\n`) and the marker text for `facts`.
pub fn format_crash(facts: &CrashFacts<'_>) -> (String, String) {
    let message: String = facts.message.chars().take(MESSAGE_CAP_CHARS).collect();
    let head = format!("binary {} thread {} panicked at {}: {}", facts.binary, facts.thread, facts.location, message);
    let flat = |s: &str| s.replace(['\r', '\n'], " ");
    let line = format!("{} ERROR panic {} backtrace: {}\n", facts.unix_ms, flat(&head), flat(facts.backtrace));
    let marker = format!(
        "time (unix ms): {}\nbinary: {}\nthread: {}\nlocation: {}\nmessage: {}\nbacktrace:\n{}\n",
        facts.unix_ms, facts.binary, facts.thread, facts.location, message, facts.backtrace
    );
    (line, cut_bytes(marker, MARKER_CAP_BYTES))
}

/// Appends the trace line and writes the marker. Errors are swallowed: a panic hook must not panic.
pub fn write_crash(dir: &Path, facts: &CrashFacts<'_>) {
    let (line, marker) = format_crash(facts);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(dir.join(TRACE_FILE)) {
        let _ = f.write_all(line.as_bytes());
    }
    let _ = fs::write(dir.join(CRASH_MARKER), marker);
}

/// Installs the hook for `binary`'s panics, writing into `dir`, which must exist.
pub fn install_panic_hook(dir: &Path, binary: &str) {
    let dir: PathBuf = dir.to_path_buf();
    let binary = binary.to_owned();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no text)".into());
        let location = info
            .location()
            .map_or_else(|| "(unknown)".to_owned(), |l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let facts = CrashFacts {
            unix_ms,
            binary: &binary,
            thread: thread.name().unwrap_or("(unnamed)"),
            message: &message,
            location: &location,
            backtrace: &backtrace,
        };
        write_crash(&dir, &facts);
        previous(info);
    }));
}

/// True when the last run left a crash marker; the marker is renamed to `last-crash.seen.txt` (replacing an older
/// one) so the next start finds none.
pub fn take_crash_marker(dir: &Path) -> bool {
    let marker = dir.join(CRASH_MARKER);
    if !marker.is_file() {
        return false;
    }
    let _ = fs::rename(&marker, dir.join(CRASH_SEEN));
    true
}

/// `%LOCALAPPDATA%\Swapdesk\logs`, created: where the UI and the helpers of a user keep their trace. The shell's
/// "Abrir a pasta de logs" opens the same folder (`swapdesk_shell_win::logs_dir`; a test in the app crate keeps
/// the two equal).
pub fn user_log_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let dir = PathBuf::from(base).join("Swapdesk").join("logs");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("swapdesk-panic-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn facts<'a>(message: &'a str, backtrace: &'a str) -> CrashFacts<'a> {
        CrashFacts {
            unix_ms: 1_700_000_000_123,
            binary: "Swapdesk",
            thread: "main",
            message,
            location: "src/x.rs:1:2",
            backtrace,
        }
    }

    #[test]
    fn the_record_is_one_line_and_the_marker_holds_every_fact() {
        let (line, marker) = format_crash(&facts("boom\nsecond", "  0: a\n  1: b"));
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        assert!(
            line.starts_with(
                "1700000000123 ERROR panic binary Swapdesk thread main panicked at src/x.rs:1:2: boom second"
            ),
            "{line}"
        );
        for part in
            ["1700000000123", "binary: Swapdesk", "thread: main", "location: src/x.rs:1:2", "message: boom", "0: a"]
        {
            assert!(marker.contains(part), "{part} missing from {marker}");
        }
    }

    #[test]
    fn the_marker_is_cut_at_16_kib_on_a_character_boundary_and_the_message_at_512_characters() {
        let long_trace = "é".repeat(20_000);
        let (_, marker) = format_crash(&facts(&"Q".repeat(2000), &long_trace));
        assert!(marker.len() <= MARKER_CAP_BYTES);
        assert_eq!(marker.matches('Q').count(), 512);
    }

    #[test]
    fn write_crash_appends_to_the_trace_and_replaces_the_marker() {
        let dir = temp("write");
        fs::write(dir.join(TRACE_FILE), "1 INFO x earlier\n").unwrap();
        write_crash(&dir, &facts("first", "bt"));
        write_crash(&dir, &facts("second", "bt"));
        let trace = fs::read_to_string(dir.join(TRACE_FILE)).unwrap();
        assert_eq!(trace.lines().count(), 3);
        assert!(trace.starts_with("1 INFO x earlier\n") && trace.contains("first") && trace.contains("second"));
        let marker = fs::read_to_string(dir.join(CRASH_MARKER)).unwrap();
        assert!(marker.contains("message: second") && !marker.contains("first"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_panic_runs_the_hook_then_the_previous_one() {
        // nextest runs each test in its own process, so the global hook is this test's own.
        static PREVIOUS_RAN: AtomicBool = AtomicBool::new(false);
        std::panic::set_hook(Box::new(|_| PREVIOUS_RAN.store(true, Ordering::SeqCst)));
        let dir = temp("hook");
        install_panic_hook(&dir, "test-binary");
        let r = std::thread::Builder::new().name("worker-7".into()).spawn(|| panic!("kaboom {}", 42)).unwrap().join();
        assert!(r.is_err());
        let trace = fs::read_to_string(dir.join(TRACE_FILE)).unwrap();
        assert!(
            trace.contains(" ERROR panic binary test-binary thread worker-7 panicked at ")
                && trace.contains("kaboom 42"),
            "{trace}"
        );
        assert!(trace.contains("panic_hook.rs"), "the location names this file: {trace}");
        let marker = fs::read_to_string(dir.join(CRASH_MARKER)).unwrap();
        assert!(
            marker.contains("thread: worker-7")
                && marker.contains("message: kaboom 42")
                && marker.contains("backtrace:")
        );
        assert!(PREVIOUS_RAN.load(Ordering::SeqCst), "chained to the previous hook");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_marker_is_taken_once() {
        let dir = temp("take");
        assert!(!take_crash_marker(&dir));
        fs::write(dir.join(CRASH_MARKER), "x").unwrap();
        assert!(take_crash_marker(&dir));
        assert!(dir.join(CRASH_SEEN).is_file() && !dir.join(CRASH_MARKER).exists());
        assert!(!take_crash_marker(&dir), "the second start finds none");
        fs::write(dir.join(CRASH_MARKER), "y").unwrap();
        assert!(take_crash_marker(&dir), "a newer crash replaces the seen file");
        assert_eq!(fs::read_to_string(dir.join(CRASH_SEEN)).unwrap(), "y");
        let _ = fs::remove_dir_all(&dir);
    }
}
