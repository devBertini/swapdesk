//! `ActionRunner` (C3) on macOS: the quick actions of F-ACT-01 and keep-awake, for the user scope (the Aqua helper runs
//! them with the logged-in user's rights; no process is started as root).
//!
//! What each action runs (every program by absolute path, see [`plan`]):
//!
//! * Lock: Control-Command-Q, the shortcut Apple documents for "Lock Screen", sent through System Events
//!   (`/usr/bin/osascript`). `CGSession -suspend` no longer exists on macOS 27, `pmset displaysleepnow` only sleeps the
//!   display (it locks only when the password is required immediately), and the login framework's lock call is private.
//!   Sending the keystroke needs the Accessibility grant that hosting needs anyway; without it `osascript` fails and
//!   the failure is reported.
//!   Checked on the build Mac (macOS 27.2, 12/11/2026): `/System/Library/CoreServices/Menu Extras/User.menu` does not
//!   exist, so the old `CGSession -suspend` is gone, and `man pmset` lists `displaysleepnow` as "causes display to go
//!   to sleep immediately" only. GUESS (no Apple page read, for the main session to check): that Control-Command-Q
//!   is still the "Lock Screen" shortcut on macOS 27 (support.apple.com, "Mac keyboard shortcuts"), and that
//!   `osascript` started by this app is covered by the app's own Accessibility grant (TCC attributes a child process
//!   to the app responsible for it) instead of needing a grant of its own. The first real lock on a hosted Mac is a
//!   hand check; no test here sends it.
//! * Sleep: `/usr/bin/pmset sleepnow` (`man pmset`: "causes an immediate system sleep"). GUESS: it needs no
//!   administrator rights for the logged-in user; the man page does not say.
//! * Restart: AppleScript `tell application "System Events" to restart`, which asks the open applications to close and
//!   lets the user cancel, as macOS does. `osascript` waits for that, so it runs detached.
//! * Task monitor: `/usr/bin/open -a "Activity Monitor"`.
//! * Keep-awake: a `/usr/bin/caffeinate -di -w <pid>` child (no display or idle sleep while this process lives),
//!   stopped when keep-awake goes off or `MacActions` is dropped.
//!
//! What the OS actually does sits behind [`SystemCalls`], so the decisions are tested with a recorder and a test never
//! sleeps, locks or restarts the Mac.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use swapdesk_platform::{ActionRunner, ErrorKind, PlatformError, Result};
use swapdesk_proto::Action;

/// How long a command that must finish (lock, sleep, open) gets before it counts as failed.
const WAIT: Duration = Duration::from_secs(10);

/// A program to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: &'static str,
    pub args: Vec<String>,
    /// Whether to wait for it and read its exit status; a command that waits for the user (restart) is detached.
    pub wait: bool,
}

impl Cmd {
    fn new(program: &'static str, args: &[&str], wait: bool) -> Cmd {
        Cmd { program, args: args.iter().map(|a| (*a).to_string()).collect(), wait }
    }
}

const OSASCRIPT: &str = "/usr/bin/osascript";
const LOCK_SCRIPT: &str = "tell application \"System Events\" to keystroke \"q\" using {control down, command down}";
const RESTART_SCRIPT: &str = "tell application \"System Events\" to restart";

/// The command of a quick action (pure).
pub fn plan(action: Action) -> Cmd {
    match action {
        Action::Lock => Cmd::new(OSASCRIPT, &["-e", LOCK_SCRIPT], true),
        Action::Sleep => Cmd::new("/usr/bin/pmset", &["sleepnow"], true),
        Action::Restart => Cmd::new(OSASCRIPT, &["-e", RESTART_SCRIPT], false),
        Action::TaskMonitor => Cmd::new("/usr/bin/open", &["-a", "Activity Monitor"], true),
    }
}

/// The keep-awake command for a process id (pure): `-d` no display sleep, `-i` no idle sleep, `-w` until the pid exits.
pub fn caffeinate(pid: u32) -> Cmd {
    Cmd::new("/usr/bin/caffeinate", &["-di", "-w", &pid.to_string()], false)
}

/// A running keep-awake request.
pub trait AwakeGuard {
    /// Ends the request.
    fn stop(&mut self);
    /// Whether it still holds.
    fn is_on(&mut self) -> bool;
}

/// The effects on the machine; the real ones are [`MacCalls`].
pub trait SystemCalls {
    /// Runs a quick action's command.
    fn run(&mut self, cmd: &Cmd) -> Result<()>;
    /// Starts the keep-awake request.
    fn start_awake(&mut self) -> Result<Box<dyn AwakeGuard>>;
}

pub struct MacActions<S: SystemCalls = MacCalls> {
    calls: S,
    awake: Option<Box<dyn AwakeGuard>>,
}

impl MacActions<MacCalls> {
    /// For the user helper (the Aqua session).
    pub fn user() -> Self {
        Self::with_calls(MacCalls)
    }
}

impl<S: SystemCalls> MacActions<S> {
    pub fn with_calls(calls: S) -> Self {
        Self { calls, awake: None }
    }

    /// Whether the keep-awake request is on.
    pub fn is_awake_requested(&mut self) -> bool {
        self.awake.as_mut().is_some_and(|a| a.is_on())
    }

    pub fn calls(&self) -> &S {
        &self.calls
    }
}

impl<S: SystemCalls> ActionRunner for MacActions<S> {
    fn run(&mut self, action: Action) -> Result<()> {
        self.calls.run(&plan(action))
    }

    fn keep_awake(&mut self, on: bool) -> Result<()> {
        if !on {
            if let Some(mut guard) = self.awake.take() {
                guard.stop();
            }
            return Ok(());
        }
        if self.is_awake_requested() {
            return Ok(());
        }
        // A request whose process died (killed from outside) is replaced.
        self.awake = Some(self.calls.start_awake()?);
        Ok(())
    }
}

impl<S: SystemCalls> Drop for MacActions<S> {
    fn drop(&mut self) {
        if let Some(mut guard) = self.awake.take() {
            guard.stop();
        }
    }
}

/// The real calls.
#[derive(Debug, Clone, Copy, Default)]
pub struct MacCalls;

fn spawn_error(cmd: &Cmd, e: &std::io::Error) -> PlatformError {
    PlatformError {
        kind: ErrorKind::Os,
        op: "ActionRunner.run",
        os_code: e.raw_os_error().map(i64::from),
        detail: format!("{}: {e}", cmd.program),
    }
}

fn command(cmd: &Cmd) -> Command {
    let mut c = Command::new(cmd.program);
    c.args(&cmd.args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    c
}

impl SystemCalls for MacCalls {
    fn run(&mut self, cmd: &Cmd) -> Result<()> {
        let mut child = command(cmd).spawn().map_err(|e| spawn_error(cmd, &e))?;
        if !cmd.wait {
            // Reaped on a thread of its own: it may wait for the user (the restart dialog) for a long time.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(());
        }
        let deadline = Instant::now() + WAIT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    return Err(PlatformError {
                        kind: ErrorKind::Os,
                        op: "ActionRunner.run",
                        os_code: status.code().map(i64::from),
                        detail: format!("{} ended with {status}", cmd.program),
                    });
                }
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(PlatformError {
                        kind: ErrorKind::Timeout,
                        op: "ActionRunner.run",
                        os_code: None,
                        detail: format!("{} did not finish", cmd.program),
                    });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(spawn_error(cmd, &e)),
            }
        }
    }

    fn start_awake(&mut self) -> Result<Box<dyn AwakeGuard>> {
        let cmd = caffeinate(std::process::id());
        let child = command(&cmd).spawn().map_err(|e| spawn_error(&cmd, &e))?;
        Ok(Box::new(Caffeinate(child)))
    }
}

/// The `caffeinate` child; it also ends by itself when this process does (`-w`).
struct Caffeinate(Child);

impl AwakeGuard for Caffeinate {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }

    fn is_on(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[derive(Default)]
    struct Log {
        ran: Vec<Cmd>,
        started: u32,
        stopped: u32,
        /// Whether the guards report that they still hold.
        alive: bool,
        fail_run: bool,
        fail_start: bool,
    }

    struct Recorder(Rc<RefCell<Log>>);

    struct Guard(Rc<RefCell<Log>>);

    impl AwakeGuard for Guard {
        fn stop(&mut self) {
            self.0.borrow_mut().stopped += 1;
        }
        fn is_on(&mut self) -> bool {
            self.0.borrow().alive
        }
    }

    fn denied() -> PlatformError {
        PlatformError { kind: ErrorKind::Os, op: "test", os_code: Some(1), detail: "denied".into() }
    }

    impl SystemCalls for Recorder {
        fn run(&mut self, cmd: &Cmd) -> Result<()> {
            let mut log = self.0.borrow_mut();
            log.ran.push(cmd.clone());
            if log.fail_run { Err(denied()) } else { Ok(()) }
        }
        fn start_awake(&mut self) -> Result<Box<dyn AwakeGuard>> {
            let mut log = self.0.borrow_mut();
            if log.fail_start {
                return Err(denied());
            }
            log.started += 1;
            log.alive = true;
            Ok(Box::new(Guard(Rc::clone(&self.0))))
        }
    }

    fn rig() -> (MacActions<Recorder>, Rc<RefCell<Log>>) {
        let log = Rc::new(RefCell::new(Log::default()));
        (MacActions::with_calls(Recorder(Rc::clone(&log))), log)
    }

    #[test]
    fn every_action_runs_its_own_command_by_absolute_path() {
        for action in [Action::Lock, Action::Sleep, Action::Restart, Action::TaskMonitor] {
            assert!(plan(action).program.starts_with('/'), "{action:?} names its program by absolute path");
        }
        assert_eq!(plan(Action::Sleep), Cmd::new("/usr/bin/pmset", &["sleepnow"], true));
        assert_eq!(plan(Action::TaskMonitor), Cmd::new("/usr/bin/open", &["-a", "Activity Monitor"], true));
        let lock = plan(Action::Lock);
        assert_eq!(lock.program, "/usr/bin/osascript");
        assert!(lock.args[1].contains("control down, command down") && lock.args[1].contains("\"q\""));
        let restart = plan(Action::Restart);
        assert_eq!(restart.program, "/usr/bin/osascript");
        assert!(restart.args[1].ends_with("to restart"));
        assert!(!restart.wait, "the restart dialog may wait for the user");
        assert!(plan(Action::Lock).wait && plan(Action::Sleep).wait && plan(Action::TaskMonitor).wait);
    }

    #[test]
    fn run_hands_the_planned_command_to_the_system_and_reports_its_failure() {
        let (mut actions, log) = rig();
        actions.run(Action::Sleep).unwrap();
        actions.run(Action::TaskMonitor).unwrap();
        assert_eq!(log.borrow().ran, vec![plan(Action::Sleep), plan(Action::TaskMonitor)]);
        log.borrow_mut().fail_run = true;
        assert_eq!(actions.run(Action::Lock).unwrap_err().kind, ErrorKind::Os);
    }

    #[test]
    fn keep_awake_starts_once_and_stops_on_off() {
        let (mut actions, log) = rig();
        actions.keep_awake(false).unwrap();
        assert_eq!(log.borrow().started, 0, "off with nothing on does nothing");
        actions.keep_awake(true).unwrap();
        actions.keep_awake(true).unwrap();
        assert_eq!(log.borrow().started, 1, "a second request does not start a second one");
        assert!(actions.is_awake_requested());
        actions.keep_awake(false).unwrap();
        assert_eq!(log.borrow().stopped, 1);
        assert!(!actions.is_awake_requested());
        actions.keep_awake(false).unwrap();
        assert_eq!(log.borrow().stopped, 1, "off twice stops once");
    }

    #[test]
    fn keep_awake_restarts_a_request_whose_process_died_and_stops_on_drop() {
        let (mut actions, log) = rig();
        actions.keep_awake(true).unwrap();
        log.borrow_mut().alive = false;
        actions.keep_awake(true).unwrap();
        assert_eq!(log.borrow().started, 2, "a dead request is replaced");
        drop(actions);
        assert_eq!(log.borrow().stopped, 1, "dropping ends the live request");
    }

    #[test]
    fn a_failed_keep_awake_start_is_an_error_and_leaves_it_off() {
        let (mut actions, log) = rig();
        log.borrow_mut().fail_start = true;
        assert!(actions.keep_awake(true).is_err());
        assert!(!actions.is_awake_requested());
    }

    #[test]
    fn caffeinate_follows_the_process_and_blocks_display_and_idle_sleep() {
        let cmd = caffeinate(1234);
        assert_eq!(cmd.program, "/usr/bin/caffeinate");
        assert_eq!(cmd.args, ["-di", "-w", "1234"]);
    }

    /// The real caffeinate child starts, holds and stops (it prevents sleep only while the test runs).
    #[test]
    fn the_real_keep_awake_starts_and_stops_a_caffeinate_child() {
        let mut actions = MacActions::user();
        actions.keep_awake(true).unwrap();
        assert!(actions.is_awake_requested());
        actions.keep_awake(false).unwrap();
        assert!(!actions.is_awake_requested());
    }
}
