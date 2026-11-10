//! `Swapdesk --ui-script <dir>` (development, `dev-harness` feature): the real app, with its real service and a
//! scratch identity, driven by a script on another machine the way a person at the Mac would drive it. It exists to
//! prove the windows in a real session with no human at the Mac: the script clicks painted controls, types into the
//! text fields and, over the viewer's video rectangle, moves the pointer and types toward the host.
//!
//! Protocol, through files in `<dir>`:
//! - `ready` appears once the app listens;
//! - the script writes `cmd.<n>` (n = 1, 2, ...), whole (a temporary name, then a rename), one command in it;
//! - the app runs it on its main queue and writes `reply.<n>`: a first line `ok` or `err <why>`, then the answer.
//!
//! Commands:
//! - `state`: what the windows show (`swapdesk_shell_mac::dev_state`) and the viewer pipeline's counters;
//! - `click <window> <control>`: a mouse click on a painted control (`main`, `viewer`, `banner`; the control by the
//!   name `state` lists, such as `Edit(Address)` or `TbDisconnect`);
//! - `keys <window> <text>`: key presses to whatever has the keyboard in that window (`\r` Return, `\t` Tab, `\\` a
//!   backslash);
//! - `video click <fx> <fy>` and `video move <fx> <fy>`: the mouse on the viewer's video, at a fraction of it from
//!   its top-left corner; `video keys <text>`: key presses toward the host;
//! - `shot <window> <png>`: a picture of the window as it is on screen;
//! - `quit`: the app ends (its scratch identity goes with it).
//!
//! The command language ([`parse`]) is built and tested on every host; the part that runs it is macOS only.

use std::path::PathBuf;

/// One command of the script.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    State,
    Click { window: String, control: String },
    Keys { window: String, text: String },
    VideoClick { fx: f64, fy: f64 },
    VideoMove { fx: f64, fy: f64 },
    VideoKeys { text: String },
    Shot { window: String, path: PathBuf },
    Quit,
}

/// `\r`, `\t` and `\\` in a command's text become Return, Tab and a backslash; any other character stays.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A fraction of the video rectangle: a number from 0 to 1.
fn fraction(word: Option<&str>, name: &str) -> Result<f64, String> {
    let word = word.ok_or_else(|| format!("{name} is missing"))?;
    let value: f64 = word.parse().map_err(|_| format!("{name} {word:?} is not a number"))?;
    if (0.0..=1.0).contains(&value) { Ok(value) } else { Err(format!("{name} {value} is not between 0 and 1")) }
}

/// The text after `skip` words of `line`, as typed (leading spaces of the text itself are kept after the one
/// separating space).
fn rest_after(line: &str, skip: usize) -> Option<&str> {
    let mut rest = line;
    for _ in 0..skip {
        let at = rest.find(' ')?;
        rest = &rest[at + 1..];
    }
    (!rest.is_empty()).then_some(rest)
}

/// Reads one command line.
pub fn parse(line: &str) -> Result<Cmd, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut words = line.split(' ');
    let verb = words.next().unwrap_or_default();
    match verb {
        "state" => Ok(Cmd::State),
        "quit" => Ok(Cmd::Quit),
        "click" => {
            let window = words.next().filter(|w| !w.is_empty()).ok_or("click needs a window and a control")?;
            let control = rest_after(line, 2).ok_or("click needs a control")?;
            Ok(Cmd::Click { window: window.to_owned(), control: control.trim().to_owned() })
        }
        "keys" => {
            let window = words.next().filter(|w| !w.is_empty()).ok_or("keys needs a window and a text")?;
            let text = rest_after(line, 2).ok_or("keys needs a text")?;
            Ok(Cmd::Keys { window: window.to_owned(), text: unescape(text) })
        }
        "video" => match words.next() {
            Some("click") => {
                Ok(Cmd::VideoClick { fx: fraction(words.next(), "fx")?, fy: fraction(words.next(), "fy")? })
            }
            Some("move") => Ok(Cmd::VideoMove { fx: fraction(words.next(), "fx")?, fy: fraction(words.next(), "fy")? }),
            Some("keys") => {
                Ok(Cmd::VideoKeys { text: unescape(rest_after(line, 2).ok_or("video keys needs a text")?) })
            }
            other => Err(format!("unknown video action {other:?} (click, move, keys)")),
        },
        "shot" => {
            let window = words.next().filter(|w| !w.is_empty()).ok_or("shot needs a window and a file")?;
            let path = rest_after(line, 2).ok_or("shot needs a file")?;
            Ok(Cmd::Shot { window: window.to_owned(), path: PathBuf::from(path.trim()) })
        }
        other => Err(format!("unknown command {other:?}")),
    }
}

#[cfg(all(target_os = "macos", not(feature = "dev-harness")))]
pub fn start(_dir: &std::path::Path, _viewer: std::sync::Arc<crate::pipeline::MacViewer>) -> Result<(), String> {
    Err("--ui-script needs a build with the dev-harness feature (scripts/mac/build-app.sh --dev-harness)".into())
}

#[cfg(all(target_os = "macos", feature = "dev-harness"))]
pub use mac::start;

#[cfg(all(target_os = "macos", feature = "dev-harness"))]
mod mac {
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use dispatch2::{DispatchQueue, DispatchTime};
    use objc2_app_kit::NSApplication;
    use objc2_foundation::MainThreadMarker;

    use super::{Cmd, parse};
    use crate::pipeline::MacViewer;

    /// How often the main queue looks for the next command.
    const POLL: Duration = Duration::from_millis(50);

    struct Ctx {
        dir: PathBuf,
        viewer: Arc<MacViewer>,
    }

    /// Starts listening in `dir` (made if missing; commands and replies of an earlier run are removed).
    pub fn start(dir: &Path, viewer: Arc<MacViewer>) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("script folder {}: {e}", dir.display()))?;
        for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("cmd.") || name.starts_with("reply.") || name == "ready" {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        schedule(Arc::new(Ctx { dir: dir.to_path_buf(), viewer }), 1, true);
        Ok(())
    }

    fn schedule(ctx: Arc<Ctx>, next: u64, first: bool) {
        let when = DispatchTime::try_from(POLL).unwrap_or(DispatchTime::NOW);
        let queued = DispatchQueue::main().after(when, move || tick(ctx, next, first));
        if queued.is_err() {
            std::process::exit(5);
        }
    }

    /// One look for command `next`, on the main queue.
    fn tick(ctx: Arc<Ctx>, next: u64, first: bool) {
        if first {
            // The run loop turns: the windows exist.
            let _ = std::fs::write(ctx.dir.join("ready"), "ready\n");
        }
        let mut next = next;
        if let Ok(line) = std::fs::read_to_string(ctx.dir.join(format!("cmd.{next}"))) {
            let command = parse(&line);
            let quit = command == Ok(Cmd::Quit);
            let reply = match command.and_then(|c| run(&ctx, &c)) {
                Ok(answer) => format!("ok\n{answer}"),
                Err(why) => format!("err {why}\n"),
            };
            let tmp = ctx.dir.join(format!("reply.{next}.tmp"));
            let done = ctx.dir.join(format!("reply.{next}"));
            if std::fs::write(&tmp, reply).and_then(|()| std::fs::rename(&tmp, &done)).is_err() {
                std::process::exit(4);
            }
            next += 1;
            if quit {
                if let Some(mtm) = MainThreadMarker::new() {
                    NSApplication::sharedApplication(mtm).terminate(None);
                }
                return;
            }
        }
        schedule(ctx, next, false);
    }

    fn run(ctx: &Ctx, command: &Cmd) -> Result<String, String> {
        match command {
            Cmd::State => {
                let mut out = swapdesk_shell_mac::dev_state();
                let stats = ctx.viewer.stats();
                let _ = writeln!(out, "pipeline.attached={}", stats.attached);
                let _ = writeln!(out, "pipeline.decoded={}", stats.decoded);
                let _ = writeln!(out, "pipeline.presented={}", stats.presented);
                let _ = writeln!(out, "pipeline.decode_errors={}", stats.decode_errors);
                let _ = writeln!(out, "pipeline.input_session={}", ctx.viewer.input_session());
                let _ = writeln!(out, "pipeline.last_error={:?}", ctx.viewer.last_error());
                let _ = writeln!(out, "pipeline.grabbing={}", ctx.viewer.grabbing());
                // Where the pointer map puts the middle of the video area (the wire's 0 to 65535), if it maps at all.
                let layout = ctx.viewer.layout();
                match layout.get() {
                    Some(l) => {
                        let (cx, cy) = (l.area.x + (l.area.width / 2) as i32, l.area.y + (l.area.height / 2) as i32);
                        let _ = writeln!(
                            out,
                            "pipeline.layout=area {} {} {} {} frame {}x{} centre maps to {:?}",
                            l.area.x,
                            l.area.y,
                            l.area.width,
                            l.area.height,
                            l.frame.0,
                            l.frame.1,
                            layout.map(cx, cy)
                        );
                    }
                    None => out.push_str(
                        "pipeline.layout=none
",
                    ),
                }
                Ok(out)
            }
            Cmd::Click { window, control } => swapdesk_shell_mac::dev_click(window, control).map(|()| String::new()),
            Cmd::Keys { window, text } => swapdesk_shell_mac::dev_keys(window, text).map(|()| String::new()),
            Cmd::VideoClick { fx, fy } => swapdesk_shell_mac::dev_video("click", *fx, *fy, "").map(|()| String::new()),
            Cmd::VideoMove { fx, fy } => swapdesk_shell_mac::dev_video("move", *fx, *fy, "").map(|()| String::new()),
            Cmd::VideoKeys { text } => swapdesk_shell_mac::dev_video("keys", 0.0, 0.0, text).map(|()| String::new()),
            Cmd::Shot { window, path } => swapdesk_shell_mac::dev_shot(window, path).map(|()| String::new()),
            Cmd::Quit => Ok(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_parses_with_its_arguments() {
        assert_eq!(parse("state"), Ok(Cmd::State));
        assert_eq!(parse("quit\n"), Ok(Cmd::Quit));
        assert_eq!(
            parse("click main Edit(Address)"),
            Ok(Cmd::Click { window: "main".into(), control: "Edit(Address)".into() })
        );
        assert_eq!(
            parse("click viewer TbDisconnect\r\n"),
            Ok(Cmd::Click { window: "viewer".into(), control: "TbDisconnect".into() })
        );
        assert_eq!(parse("video click 0.5 0.25"), Ok(Cmd::VideoClick { fx: 0.5, fy: 0.25 }));
        assert_eq!(parse("video move 0 1"), Ok(Cmd::VideoMove { fx: 0.0, fy: 1.0 }));
        assert_eq!(parse("video keys swapdesk"), Ok(Cmd::VideoKeys { text: "swapdesk".into() }));
        assert_eq!(
            parse("shot viewer /tmp/a b/viewer.png"),
            Ok(Cmd::Shot { window: "viewer".into(), path: PathBuf::from("/tmp/a b/viewer.png") })
        );
    }

    #[test]
    fn typed_text_keeps_its_spaces_and_takes_return_tab_and_backslash_escapes() {
        assert_eq!(parse("keys main 192.168.1.3"), Ok(Cmd::Keys { window: "main".into(), text: "192.168.1.3".into() }));
        assert_eq!(
            parse(r"keys main Mac da sala\r"),
            Ok(Cmd::Keys { window: "main".into(), text: "Mac da sala\r".into() })
        );
        assert_eq!(parse(r"keys main a\tb\\c\x"), Ok(Cmd::Keys { window: "main".into(), text: "a\tb\\c\\x".into() }));
        assert_eq!(parse(r"video keys two  spaces\"), Ok(Cmd::VideoKeys { text: "two  spaces\\".into() }));
    }

    #[test]
    fn a_wrong_command_says_what_is_wrong() {
        for (line, part) in [
            ("", "unknown command"),
            ("dance", "unknown command"),
            ("click", "needs a window"),
            ("click main", "needs a control"),
            ("keys main", "needs a text"),
            ("video", "unknown video action"),
            ("video jump 0 0", "unknown video action"),
            ("video click 0.5", "fy is missing"),
            ("video click half 0.5", "is not a number"),
            ("video move 1.5 0.5", "is not between 0 and 1"),
            ("video keys", "needs a text"),
            ("shot viewer", "needs a file"),
        ] {
            let error = parse(line).expect_err(line);
            assert!(error.contains(part), "{line:?}: {error}");
        }
    }
}
