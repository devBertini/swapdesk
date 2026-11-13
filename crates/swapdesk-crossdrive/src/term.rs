//! The terminal and quick actions proof across machines (F-XTERM-01), with no human at either end.
//!
//! The viewer side drives its UI backend exactly as the terminal panel does (`TermOpen`, `TermWrite`, `TermResize`,
//! `TermClose`; the shell's output arrives through [`swapdesk_ui::service_backend::ViewerSink::term_output`], which
//! the caller hands to this module as `drain`). The host needs no script of its own: its service starts the shell as
//! the logged-in user and audits the open and the action ([`crate::host`] reads that audit when asked).
//!
//! 1. `VIEWER_NEED_TERM` is logged and the orchestrator writes `term-expect-user` (the host's console user, as the
//!    host's OS names it) into the run folder;
//! 2. the terminal opens (`TERM_OPENED`); a command that prints the shell's user runs between two marker lines, and
//!    its output must be that user (`TERM_USER_OK`);
//! 3. the terminal is resized and a command prints the size the shell's terminal has now (`stty size` on a Mac, the
//!    console's window size on Windows); it must be the new one (`TERM_RESIZE_OK`);
//! 4. the terminal is closed; its id must be gone afterwards (`TERM_CLOSE_OK`).
//!
//! The commands are written for the two shells that run (PowerShell and a POSIX login shell: `;` separates, `echo`
//! prints). Markers carry a nonce and are looked for as whole lines, so the shell's echo of the command line (which
//! holds them, followed by `;`) is never taken for the output.

use std::path::Path;
use std::time::{Duration, Instant};

use swapdesk_ui::{Command, Os, Reply, UiEvent};

use crate::pump::{Pump, wait_file};

const REPLY: Duration = Duration::from_secs(10);
/// The size the terminal opens with, and the size it is resized to.
pub const OPEN_SIZE: (u16, u16) = (80, 24);
pub const RESIZED: (u16, u16) = (100, 30);

/// The text of `bytes` with the terminal's escape sequences (CSI, OSC, two-byte ones) and bells removed.
pub fn strip_escapes(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                i += 1;
                match bytes.get(i) {
                    // CSI: parameters, then a final byte in 0x40..=0x7e.
                    Some(b'[') => {
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    // OSC: up to BEL or ESC \.
                    Some(b']') => {
                        i += 1;
                        while i < bytes.len()
                            && bytes[i] != 0x07
                            && !(bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\'))
                        {
                            i += 1;
                        }
                        i += if bytes.get(i) == Some(&0x1b) { 2 } else { 1 };
                    }
                    // Any other escape: the one byte after it.
                    Some(_) => i += 1,
                    None => {}
                }
            }
            0x07 => i += 1,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The non-blank lines between the first line that is exactly `begin` and the next that is exactly `end`; `None`
/// until both are there.
pub fn lines_between(text: &str, begin: &str, end: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.split(['\r', '\n']).map(str::trim).collect();
    let b = lines.iter().position(|l| *l == begin)?;
    let e = b + 1 + lines[b + 1..].iter().position(|l| *l == end)?;
    Some(lines[b + 1..e].iter().filter(|l| !l.is_empty()).map(|l| (*l).to_string()).collect())
}

/// A user name as the two systems print it: without a domain or computer prefix (`PC\\user`), lower case.
pub fn plain_user(name: &str) -> String {
    name.trim().rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
}

/// `(rows, cols)` from a line "30 100".
pub fn parse_size(line: &str) -> Option<(u16, u16)> {
    let mut it = line.split_whitespace();
    let (rows, cols) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    it.next().is_none().then_some((rows, cols))
}

/// The command that prints the user the shell runs as.
pub fn user_command(begin: &str, end: &str) -> String {
    format!("echo {begin}; whoami; echo {end}")
}

/// The command that prints the terminal's size as "rows cols".
pub fn size_command(os: Os, begin: &str, end: &str) -> String {
    match os {
        Os::MacOs => format!("echo {begin}; stty size; echo {end}"),
        Os::Windows => format!(
            "echo {begin}; \"$($Host.UI.RawUI.WindowSize.Height) $($Host.UI.RawUI.WindowSize.Width)\"; echo {end}"
        ),
    }
}

struct Shell<'a, 'b> {
    pump: &'a mut Pump<'b>,
    term_id: u32,
    drain: &'a dyn Fn(u32) -> Vec<u8>,
    seen: Vec<u8>,
}

impl Shell<'_, '_> {
    /// Takes in what the shell printed since the last call.
    fn read(&mut self) {
        self.seen.extend((self.drain)(self.term_id));
    }

    /// Writes `command` and waits for the lines it prints between `begin` and `end`.
    fn run(&mut self, command: &str, begin: &str, end: &str, timeout: Duration) -> Result<Vec<String>, String> {
        self.read();
        self.seen.clear();
        self.pump
            .ask(Command::TermWrite { term_id: self.term_id, bytes: format!("{command}\r").into_bytes() }, REPLY)?;
        let until = Instant::now() + timeout;
        loop {
            self.read();
            if let Some(lines) = lines_between(&strip_escapes(&self.seen), begin, end) {
                return Ok(lines);
            }
            if Instant::now() >= until {
                let tail: String =
                    strip_escapes(&self.seen).chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
                return Err(format!(
                    "the shell never printed {end} after {command:?} within {}s (it printed: {tail:?})",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn nonce() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("{:06}", ms % 1_000_000)
}

/// Runs the terminal proof on viewer session `session_id` toward a host of `host_os`. `drain(term_id)` takes the
/// output the sink has collected for that terminal.
pub fn term_round(
    pump: &mut Pump<'_>,
    session_id: u64,
    dir: &Path,
    host_os: Os,
    drain: &dyn Fn(u32) -> Vec<u8>,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    log("VIEWER_NEED_TERM");
    let expected = wait_file(&dir.join("term-expect-user"), "the host's console user", timeout)?;

    let Reply::TermId(term_id) =
        pump.ask(Command::TermOpen { session_id, admin: false, cols: OPEN_SIZE.0, rows: OPEN_SIZE.1 }, timeout)?
    else {
        return Err("TermOpen did not reply a terminal id".into());
    };
    let account = pump.wait("the terminal to open", timeout, |e| match e {
        UiEvent::Term { term_id: t, event: swapdesk_ui::TermEvent::Opened { account } } if *t == term_id => {
            Some(account.clone())
        }
        _ => None,
    })?;
    log(&format!("TERM_OPENED term={term_id} account={account}"));
    let mut shell = Shell { pump, term_id, drain, seen: Vec::new() };

    // The shell needs a moment before it reads a line: its first output (a prompt) says it is up.
    let up = Instant::now() + timeout;
    while shell.seen.is_empty() && Instant::now() < up {
        shell.read();
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(700));

    // 1. Whose shell it is.
    let n = nonce();
    let (b, e) = (format!("SWU{n}B"), format!("SWU{n}E"));
    let lines = shell.run(&user_command(&b, &e), &b, &e, timeout)?;
    let [user] = lines.as_slice() else {
        return Err(format!("the user command printed {} lines, not one: {lines:?}", lines.len()));
    };
    if plain_user(user) != plain_user(&expected) {
        return Err(format!("the shell runs as {user:?}, the host's console user is {expected:?}"));
    }
    log(&format!("TERM_USER_OK user={user} expected={expected} account={account}"));

    // 2. The new size reaches the shell's terminal.
    shell.pump.ask(Command::TermResize { term_id, cols: RESIZED.0, rows: RESIZED.1 }, REPLY)?;
    std::thread::sleep(Duration::from_millis(700));
    let n = nonce();
    let (b, e) = (format!("SWS{n}B"), format!("SWS{n}E"));
    let lines = shell.run(&size_command(host_os, &b, &e), &b, &e, timeout)?;
    let [size] = lines.as_slice() else {
        return Err(format!("the size command printed {} lines, not one: {lines:?}", lines.len()));
    };
    match parse_size(size) {
        Some((rows, cols)) if (cols, rows) == RESIZED => {
            log(&format!("TERM_RESIZE_OK size={rows}x{cols} (rows x cols)"))
        }
        other => {
            return Err(format!(
                "after the resize to {}x{} the shell reports {size:?} ({other:?}), not {} {}",
                RESIZED.1, RESIZED.0, RESIZED.1, RESIZED.0
            ));
        }
    }

    // 3. Close: the id is gone for every later call.
    shell.pump.ask(Command::TermClose { term_id }, REPLY)?;
    let later = shell.pump.request(Command::TermResize { term_id, cols: 90, rows: 20 }, REPLY)?;
    if later.is_ok() {
        return Err("the closed terminal still takes a resize".into());
    }
    log(&format!("TERM_CLOSE_OK term={term_id}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_are_removed_and_text_kept() {
        let raw = b"\x1b[?25l\x1b[2J\x1b]0;title\x07PS C:\\> \x1b[93mSWU1B\x1b[0m\r\nuser\r\n";
        assert_eq!(strip_escapes(raw), "PS C:\\> SWU1B\r\nuser\r\n");
    }

    #[test]
    fn the_echo_of_the_command_is_not_the_output() {
        let echo = "PS> echo SWU1B; whoami; echo SWU1E\r\nSWU1B\r\ndesktop\\ana\r\nSWU1E\r\nPS> ";
        assert_eq!(lines_between(echo, "SWU1B", "SWU1E"), Some(vec!["desktop\\ana".to_string()]));
        assert_eq!(lines_between("echo SWU1B; whoami; echo SWU1E\r\n", "SWU1B", "SWU1E"), None, "echo only");
        assert_eq!(lines_between("SWU1B\r\nana\r\n", "SWU1B", "SWU1E"), None, "no end yet");
    }

    #[test]
    fn users_compare_without_domain_or_case() {
        assert_eq!(plain_user("DESKTOP-1\\Bertini"), "bertini");
        assert_eq!(plain_user("bertini\r"), "bertini");
        assert_ne!(plain_user("root"), plain_user("bertini"));
    }

    #[test]
    fn a_size_line_is_rows_then_columns() {
        assert_eq!(parse_size("30 100"), Some((30, 100)));
        assert_eq!(parse_size("30 100 1"), None);
        assert_eq!(parse_size("thirty"), None);
        assert_eq!((RESIZED.1, RESIZED.0), (30, 100));
    }

    #[test]
    fn the_commands_keep_their_markers_apart_from_the_output() {
        let c = user_command("B1", "E1");
        assert!(c.contains("; whoami;") && !c.lines().any(|l| l == "B1"));
        assert!(size_command(Os::MacOs, "B", "E").contains("stty size"));
        assert!(size_command(Os::Windows, "B", "E").contains("WindowSize.Height"));
    }
}
