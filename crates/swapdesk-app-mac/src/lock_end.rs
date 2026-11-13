//! "Bloquear ao terminar" (F-SESS-03) on the Mac: the hub's hook locks the user's screen with the same `Lock` the quick
//! action "Bloquear a tela" runs (`swapdesk_sys_mac::actions::MacActions`, Control-Command-Q through System Events).
//!
//! The hub calls the hook only for a session that really ended (never for a lost link the core keeps resumable) and
//! only while this Mac's user owns the screen (`UiHub::set_session_active`: another user at the screen is not ours to
//! lock). The lock command may wait for its program (`osascript`, up to ten seconds), so it runs on a thread of its own.
//! Generic over the C3 [`ActionRunner`], built on the locking thread (`MacActions` holds a keep-awake guard that is not
//! `Send`): [`crate::app`] gives `MacActions::user`, tests give the fakes.

use std::sync::Arc;

use swapdesk_host::ui_hub::SystemHooks;
use swapdesk_platform::{ActionRunner, Level, TraceSink};
use swapdesk_proto::Action;

/// The Mac's system hooks: the lock on the end of a session. Keep-awake is the helper's (it runs under the user's
/// `caffeinate`), and the "service" switch has nothing to start here, so those stay the hub's do-nothing defaults.
pub struct LockOnEnd {
    /// Makes the runner a lock runs on (one per lock: the lock keeps no state).
    runner: Arc<dyn Fn() -> Box<dyn ActionRunner> + Send + Sync>,
    trace: Arc<dyn TraceSink>,
}

impl LockOnEnd {
    pub fn new<R: ActionRunner + 'static>(
        runner: impl Fn() -> R + Send + Sync + 'static,
        trace: Arc<dyn TraceSink>,
    ) -> Self {
        Self { runner: Arc::new(move || Box::new(runner())), trace }
    }
}

/// Runs the quick action `Lock` on `runner`: the one function "Bloquear a tela" and the lock on the end of a session
/// share. A failure (no Accessibility grant, say) is logged, not returned: the session is over.
pub fn lock_screen_now(runner: &mut dyn ActionRunner, trace: &dyn TraceSink) {
    match runner.run(Action::Lock) {
        Ok(()) => trace.log(Level::Info, "app", format_args!("lock on end: the screen was locked")),
        Err(e) => trace.log(Level::Warn, "app", format_args!("lock on end: the screen was not locked: {e}")),
    }
}

impl SystemHooks for LockOnEnd {
    fn lock_screen(&self, _console_session: u32) {
        let (runner, trace) = (Arc::clone(&self.runner), Arc::clone(&self.trace));
        let spawned = std::thread::Builder::new()
            .name("lock-on-end".into())
            .spawn(move || lock_screen_now(&mut *runner(), &*trace));
        if let Err(e) = spawned {
            self.trace.log(Level::Warn, "app", format_args!("lock on end: no thread to lock with: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use swapdesk_fakes::{FakeActions, LogCapture};
    use swapdesk_platform::ErrorKind;

    use super::*;

    fn wait_for_log(log: &LogCapture, what: &str) {
        let end = Instant::now() + Duration::from_secs(5);
        while !log.lines().iter().any(|l| l.text.contains(what)) {
            assert!(Instant::now() < end, "never logged {what:?}: {:?}", log.lines());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_hook_runs_the_lock_quick_action_and_nothing_else() {
        let (fake, log) = (FakeActions::new(), LogCapture::new());
        let hooks = LockOnEnd::new(
            {
                let fake = fake.clone();
                move || fake.clone()
            },
            Arc::new(log.clone()),
        );
        hooks.lock_screen(1);
        wait_for_log(&log, "the screen was locked");
        assert_eq!(fake.ran(), vec![Action::Lock]);
    }

    #[test]
    fn a_failed_lock_is_logged_and_does_not_panic() {
        let (fake, log) = (FakeActions::new(), LogCapture::new());
        fake.refuse(Action::Lock, ErrorKind::Os);
        let hooks = LockOnEnd::new(
            {
                let fake = fake.clone();
                move || fake.clone()
            },
            Arc::new(log.clone()),
        );
        hooks.lock_screen(1);
        wait_for_log(&log, "the screen was not locked");
        assert!(fake.ran().is_empty());
    }

    /// On the Mac the runner is `MacActions`: the hook's `Lock` is the very command of the quick action, and no
    /// other command runs (a test never locks the Mac: the system calls are a recorder).
    #[cfg(target_os = "macos")]
    #[test]
    fn with_mac_actions_the_hook_runs_the_command_of_the_lock_quick_action() {
        use std::sync::Mutex;

        use swapdesk_sys_mac::actions::{AwakeGuard, Cmd, MacActions, SystemCalls, plan};

        #[derive(Clone, Default)]
        struct Recorder(Arc<Mutex<Vec<Cmd>>>);

        impl SystemCalls for Recorder {
            fn run(&mut self, cmd: &Cmd) -> swapdesk_platform::Result<()> {
                self.0.lock().unwrap().push(cmd.clone());
                Ok(())
            }
            fn start_awake(&mut self) -> swapdesk_platform::Result<Box<dyn AwakeGuard>> {
                unreachable!("keep-awake is not part of the lock")
            }
        }

        let (calls, log) = (Recorder::default(), LogCapture::new());
        let hooks = LockOnEnd::new(
            {
                let calls = calls.clone();
                move || MacActions::with_calls(calls.clone())
            },
            Arc::new(log.clone()),
        );
        hooks.lock_screen(1);
        wait_for_log(&log, "the screen was locked");
        assert_eq!(*calls.0.lock().unwrap(), vec![plan(Action::Lock)]);
    }
}
