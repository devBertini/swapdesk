//! "Bloquear ao terminar" (F-SESS-03) on Windows: the hub's hook locks the console through the same `ActionRunner` `Lock`
//! the quick action "Bloquear a tela" runs. Nothing here locks the machine: the system calls are a recorder.

use std::sync::mpsc;
use std::time::Duration;

use swapdesk_platform::{ErrorKind, PlatformError, Result};
use swapdesk_sys_win::actions::{Scope, SystemCalls, WinActions};

use super::*;

/// Keeps what the trace was told.
#[derive(Default)]
struct Lines(Mutex<Vec<(Level, String)>>);

impl TraceSink for Lines {
    fn log(&self, level: Level, _target: &str, args: std::fmt::Arguments<'_>) {
        lock(&self.0).push((level, args.to_string()));
    }
}

/// A runner that reports each action it is asked for, and may fail.
struct Reporting {
    tx: mpsc::Sender<Action>,
    fail: bool,
}

impl ActionRunner for Reporting {
    fn run(&mut self, action: Action) -> Result<()> {
        let _ = self.tx.send(action);
        if self.fail {
            return Err(PlatformError {
                kind: ErrorKind::Os,
                op: "Reporting.run",
                os_code: Some(5),
                detail: "denied".into(),
            });
        }
        Ok(())
    }

    fn keep_awake(&mut self, _on: bool) -> Result<()> {
        Ok(())
    }
}

#[test]
fn the_hook_runs_the_lock_quick_action_and_says_which_console_session() {
    let (tx, rx) = mpsc::channel();
    let trace = Arc::new(Lines::default());
    let hooks = WinHooks::with_locker(trace.clone(), Reporting { tx, fail: false });
    hooks.lock_screen(5);
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Action::Lock));
    let end = Instant::now() + Duration::from_secs(5);
    while !lock(&trace.0).iter().any(|(l, m)| *l == Level::Info && m.contains("console session 5 locked")) {
        assert!(Instant::now() < end, "the outcome was never logged: {:?}", lock(&trace.0));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_failed_lock_is_logged_as_a_warning_and_nothing_else() {
    let (tx, rx) = mpsc::channel();
    let trace = Arc::new(Lines::default());
    let hooks = WinHooks::with_locker(trace.clone(), Reporting { tx, fail: true });
    hooks.lock_screen(3);
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Action::Lock));
    let end = Instant::now() + Duration::from_secs(5);
    while !lock(&trace.0).iter().any(|(l, m)| *l == Level::Warn && m.contains("console session 3 not locked")) {
        assert!(Instant::now() < end, "the failure was never logged: {:?}", lock(&trace.0));
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the machine would have been asked, in order.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<&'static str>>>);

impl SystemCalls for Calls {
    fn lock_workstation(&mut self) -> Result<()> {
        lock(&self.0).push("lock_workstation");
        Ok(())
    }
    fn lock_console_session(&mut self) -> Result<()> {
        lock(&self.0).push("lock_console_session");
        Ok(())
    }
    fn console_user_present(&mut self) -> bool {
        true
    }
    fn suspend(&mut self) -> Result<()> {
        lock(&self.0).push("suspend");
        Ok(())
    }
    fn reboot(&mut self) -> Result<()> {
        lock(&self.0).push("reboot");
        Ok(())
    }
    fn launch_task_manager(&mut self) -> Result<()> {
        lock(&self.0).push("launch_task_manager");
        Ok(())
    }
}

/// The service's runner is the one the quick action falls back to: its `Lock` locks the console session from session
/// 0 (a `LockWorkStation` there would lock session 0 and nothing else), and no other system call is made.
#[test]
fn the_service_lock_is_the_console_session_lock_and_nothing_else() {
    let calls = Calls::default();
    let mut runner = WinActions::with_calls(calls.clone(), Scope::Service);
    let trace = Lines::default();
    lock_console(&mut runner, &trace, 1);
    assert_eq!(*lock(&calls.0), vec!["lock_console_session"]);
}
