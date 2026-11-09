use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use swapdesk_platform::{ActionRunner, ConsoleState, ErrorKind, PlatformError, Result, SessionEvent, SessionWatcher};
use swapdesk_proto::{Action, Desktop};

use crate::lock;

struct WatcherInner {
    state: ConsoleState,
    events: VecDeque<SessionEvent>,
}

/// Scripted session facts. `push` queues an event (a `ConsoleChanged` or `DesktopSwitched` event also updates what
/// `state()` returns once it is delivered); `next_event` waits for one up to the timeout. Clones share the script.
#[derive(Clone)]
pub struct FakeSessionWatcher {
    inner: Arc<(Mutex<WatcherInner>, Condvar)>,
}

impl FakeSessionWatcher {
    pub fn new(state: ConsoleState) -> Self {
        Self { inner: Arc::new((Mutex::new(WatcherInner { state, events: VecDeque::new() }), Condvar::new())) }
    }

    /// A user "alex" logged in on session 1 at the default desktop.
    pub fn logged_in() -> Self {
        Self::new(ConsoleState {
            session_id: Some(1),
            user: Some("alex".to_string()),
            user_is_admin: false,
            desktop: Desktop::Default,
        })
    }

    pub fn push(&self, event: SessionEvent) {
        lock(&self.inner.0).events.push_back(event);
        self.inner.1.notify_all();
    }
}

impl SessionWatcher for FakeSessionWatcher {
    fn state(&mut self) -> Result<ConsoleState> {
        Ok(lock(&self.inner.0).state.clone())
    }

    fn next_event(&mut self, timeout: Duration) -> Result<Option<SessionEvent>> {
        let guard = lock(&self.inner.0);
        let (mut guard, _) =
            self.inner.1.wait_timeout_while(guard, timeout, |g| g.events.is_empty()).unwrap_or_else(|e| e.into_inner());
        let Some(event) = guard.events.pop_front() else { return Ok(None) };
        match &event {
            SessionEvent::ConsoleChanged(state) => guard.state = state.clone(),
            SessionEvent::DesktopSwitched(desktop) => guard.state.desktop = *desktop,
            _ => {}
        }
        Ok(Some(event))
    }
}

#[derive(Default)]
struct ActionsInner {
    ran: Vec<Action>,
    awake: Vec<bool>,
    refused: Vec<(Action, ErrorKind)>,
}

/// Records the quick actions run and the `keep_awake` calls; an action can be scripted to be refused.
/// Clones share the record.
#[derive(Clone, Default)]
pub struct FakeActions(Arc<Mutex<ActionsInner>>);

impl FakeActions {
    pub fn new() -> Self {
        Self::default()
    }

    /// `run(action)` fails with `kind` from now on.
    pub fn refuse(&self, action: Action, kind: ErrorKind) {
        lock(&self.0).refused.push((action, kind));
    }

    /// Actions that ran, in order.
    pub fn ran(&self) -> Vec<Action> {
        lock(&self.0).ran.clone()
    }

    /// Every `keep_awake` argument, in order.
    pub fn awake_calls(&self) -> Vec<bool> {
        lock(&self.0).awake.clone()
    }
}

impl ActionRunner for FakeActions {
    fn run(&mut self, action: Action) -> Result<()> {
        let mut g = lock(&self.0);
        if let Some((_, kind)) = g.refused.iter().find(|(a, _)| *a == action) {
            return Err(PlatformError {
                kind: *kind,
                op: "FakeActions.run",
                os_code: None,
                detail: format!("{action:?} refused"),
            });
        }
        g.ran.push(action);
        Ok(())
    }

    fn keep_awake(&mut self, on: bool) -> Result<()> {
        lock(&self.0).awake.push(on);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn scripted_events_update_the_state_in_order() {
        let mut w = FakeSessionWatcher::logged_in();
        let script = w.clone();
        assert_eq!(w.state().unwrap().desktop, Desktop::Default);
        script.push(SessionEvent::DesktopSwitched(Desktop::Secure));
        script.push(SessionEvent::Suspending);
        script.push(SessionEvent::ConsoleChanged(ConsoleState {
            session_id: None,
            user: None,
            user_is_admin: false,
            desktop: Desktop::LoginWindow,
        }));
        assert_eq!(w.next_event(Duration::ZERO).unwrap(), Some(SessionEvent::DesktopSwitched(Desktop::Secure)));
        assert_eq!(w.state().unwrap().desktop, Desktop::Secure);
        assert_eq!(w.next_event(Duration::ZERO).unwrap(), Some(SessionEvent::Suspending));
        assert!(matches!(w.next_event(Duration::ZERO).unwrap(), Some(SessionEvent::ConsoleChanged(_))));
        assert_eq!(w.state().unwrap().user, None);
        let t = Instant::now();
        assert_eq!(w.next_event(Duration::from_millis(30)).unwrap(), None);
        assert!(t.elapsed() >= Duration::from_millis(25));
    }

    #[test]
    fn a_waiting_reader_wakes_on_push() {
        let mut w = FakeSessionWatcher::logged_in();
        let script = w.clone();
        let h = std::thread::spawn(move || w.next_event(Duration::from_secs(5)).unwrap());
        std::thread::sleep(Duration::from_millis(20));
        script.push(SessionEvent::Resumed);
        assert_eq!(h.join().unwrap(), Some(SessionEvent::Resumed));
    }

    #[test]
    fn actions_are_recorded_and_can_be_refused() {
        let mut a = FakeActions::new();
        let rec = a.clone();
        a.run(Action::Lock).unwrap();
        a.keep_awake(true).unwrap();
        rec.refuse(Action::TaskMonitor, ErrorKind::Unsupported);
        let e = a.run(Action::TaskMonitor).err().unwrap();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        a.run(Action::Sleep).unwrap();
        a.keep_awake(false).unwrap();
        assert_eq!(rec.ran(), vec![Action::Lock, Action::Sleep]);
        assert_eq!(rec.awake_calls(), vec![true, false]);
    }
}
