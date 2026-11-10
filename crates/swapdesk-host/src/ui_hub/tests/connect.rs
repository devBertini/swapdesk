//! The connect entry: `Connect` answers at once with an entry, the entry reports `Connecting` and `Prompted` while the
//! host's user decides, and it ends with the session's `Active` (then its own `Ended`) or with why the session did not
//! open. Cancelling it, also during the connect phase of a wake, closes the host's question and opens no session.

use swapdesk_ipc::{ConnectWhy, IncomingRequest};

use super::extras::{active_as, ended, sleeper, waking};
use super::*;

/// A host and a viewer that have paired, with a UI each at the console.
struct Pair {
    host: Machine,
    viewer: Machine,
    host_ui: Client,
    viewer_ui: Client,
}

impl Pair {
    fn new() -> Self {
        Self::build(true)
    }

    /// `viewer_finds_host`: the viewer has an address for the host.
    fn build(viewer_finds_host: bool) -> Self {
        let mut host = Machine::new("host-pc");
        let viewer = Machine::new("viewer-pc");
        host.pin(&viewer, ALL);
        viewer.pin(&host, PermissionSet::EMPTY);
        host.attach_helper(IpcRole::HelperSystem);
        host.attach_helper(IpcRole::HelperUser);
        if viewer_finds_host {
            viewer.sees(&host, false);
        }
        host.hub.set_console_session(Some(1));
        let host_ui = host.client(info(false));
        let viewer_ui = viewer.client(info(false));
        Self { host, viewer, host_ui, viewer_ui }
    }

    /// "Controlar" is pressed on the viewer: the entry's id.
    fn press(&mut self) -> u64 {
        let op = UiOp::Connect { device_id: self.host.hex(), intent: Intent::Control, unattended: false };
        let req = self.viewer_ui.request(op);
        self.viewer_ui.entry_of(req)
    }

    /// The question on the host's screen.
    fn incoming(&mut self) -> IncomingRequest {
        self.host_ui.notify("IncomingRequest", |n| match n {
            Notify::IncomingRequest(r) => Some(r.clone()),
            _ => None,
        })
    }

    fn answer(&mut self, request: &IncomingRequest, decision: Decision) -> Result<ReplyBody, ApiCode> {
        let grants = ALL.bits();
        self.host_ui.call(UiOp::AnswerIncoming { request_id: request.request_id, decision, grants })
    }

    fn finish(self) {
        let Pair { host, viewer, host_ui, viewer_ui } = self;
        drop((host_ui, viewer_ui));
        viewer.finish();
        host.finish();
    }
}

/// The next state the UI hears of `entry`.
fn next_state(ui: &mut Client, entry: u64) -> SessionState {
    ui.notify("a state of the entry", |n| match n {
        Notify::Session { session_id, state, .. } if *session_id == entry => Some(*state),
        _ => None,
    })
}

/// Why the entry did not open a session, waiting up to `limit` for it.
fn not_opened(ui: &mut Client, entry: u64, limit: Duration) -> ConnectWhy {
    ui.take_within(limit, "the entry's NotOpened", |m| match m {
        Ipc::UiNotify(Notify::Session { session_id, state: SessionState::NotOpened { why }, .. })
            if *session_id == entry =>
        {
            Some(*why)
        }
        _ => None,
    })
}

fn is_active(m: &Ipc) -> bool {
    matches!(m, Ipc::UiNotify(Notify::Session { state: SessionState::Active { .. }, .. }))
}

#[test]
fn connect_replies_an_entry_at_once_then_connecting_and_prompted_while_the_host_asks() {
    let mut p = Pair::new();
    let device = p.host.hex();

    // The reply comes while the host's user has not been shown anything yet, let alone answered.
    let entry = p.press();
    let incoming = p.incoming();
    assert_eq!(incoming.device.id, p.viewer.hex());

    // The entry says what it is doing, in order, under its own id and the host's device.
    let (id, hex) = p.viewer_ui.notify("the first state", |n| match n {
        Notify::Session { session_id, device_id, state: SessionState::Connecting } => {
            Some((*session_id, device_id.clone()))
        }
        _ => None,
    });
    assert_eq!((id, hex.as_str()), (entry, device.as_str()));
    let SessionState::Prompted { remaining_s } = next_state(&mut p.viewer_ui, entry) else {
        panic!("Prompted follows Connecting")
    };
    assert!(remaining_s > 0 && remaining_s <= 20, "the host's seconds: {remaining_s}");

    // While the host's card is open nothing opens and nothing is announced under the placeholder id 0.
    assert!(p.viewer_ui.nothing_within(Duration::from_millis(300), is_active), "no session while the host asks");
    assert!(
        p.viewer_ui.nothing_within(Duration::from_millis(50), |m| matches!(
            m,
            Ipc::UiNotify(Notify::Session { session_id: 0, .. })
        )),
        "no state under session id 0"
    );

    // The host's user accepts: the session opens.
    assert!(p.answer(&incoming, Decision::Accept).is_ok());
    let session = active_as(&mut p.viewer_ui, Role::Viewer, &[entry]);
    assert_ne!(session, entry);
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: session }).is_ok());
    p.finish();
}

#[test]
fn an_accepted_connect_sends_the_sessions_active_before_the_entry_ends() {
    let mut p = Pair::new();
    let entry = p.press();
    let incoming = p.incoming();
    assert!(p.answer(&incoming, Decision::Accept).is_ok());

    // Two things matter, in this order: the session exists in the interface, then the card goes.
    let mut order = Vec::new();
    for _ in 0..2 {
        order.push(p.viewer_ui.notify("the Active session or the entry's end", |n| match n {
            Notify::Session { session_id, state: SessionState::Active { role: Role::Viewer, .. }, .. }
                if *session_id != entry =>
            {
                Some(("active", *session_id))
            }
            Notify::Session { session_id, state: SessionState::Ended { reason: None }, .. } if *session_id == entry => {
                Some(("ended", *session_id))
            }
            _ => None,
        }));
    }
    assert_eq!(order[0].0, "active", "the session first: {order:?}");
    assert_eq!(order[1], ("ended", entry), "then the entry, once: {order:?}");
    let session = order[0].1;

    // The entry is over: nothing is left to cancel under its id, and the session is not touched.
    assert_eq!(p.viewer_ui.call(UiOp::Disconnect { session_id: entry }).unwrap_err(), ApiCode::NotFound);
    assert!(p.viewer_ui.nothing_within(Duration::from_millis(200), |m| matches!(
        m,
        Ipc::UiNotify(Notify::Session {
            session_id,
            state: SessionState::Ended { .. } | SessionState::NotOpened { .. },
            ..
        }) if *session_id == entry
    )));
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: session }).is_ok());
    p.finish();
}

#[test]
fn each_refusal_ends_the_entry_with_its_own_why() {
    // Declined: the host's user says no.
    let mut p = Pair::new();
    let entry = p.press();
    let incoming = p.incoming();
    assert!(p.answer(&incoming, Decision::Decline).is_ok());
    assert_eq!(not_opened(&mut p.viewer_ui, entry, WAIT), ConnectWhy::Declined);
    // The entry is over: no `Ended` follows `NotOpened`, no session was announced, nothing is left to cancel.
    assert!(p.viewer_ui.nothing_within(Duration::from_millis(200), |m| is_active(m)
        || matches!(m, Ipc::UiNotify(Notify::Session { state: SessionState::Ended { .. }, .. }))));
    assert_eq!(p.viewer_ui.call(UiOp::Disconnect { session_id: entry }).unwrap_err(), ApiCode::NotFound);
    p.finish();

    // Nobody present: nobody is at the host's console.
    let mut p = Pair::new();
    p.host.hub.set_console_session(None);
    let entry = p.press();
    assert_eq!(not_opened(&mut p.viewer_ui, entry, WAIT), ConnectWhy::NobodyPresent);
    p.finish();

    // Busy: a second press while the host still asks about the first is refused, and the first goes on waiting.
    let mut p = Pair::new();
    let first = p.press();
    let incoming = p.incoming();
    let second = p.press();
    assert_ne!(first, second);
    assert_eq!(not_opened(&mut p.viewer_ui, second, WAIT), ConnectWhy::Busy);
    assert!(
        p.viewer_ui.nothing_within(Duration::from_millis(200), |m| matches!(
            m,
            Ipc::UiNotify(Notify::Session { session_id, state: SessionState::NotOpened { .. }, .. })
                if *session_id == first
        )),
        "the first entry still waits"
    );
    assert!(p.answer(&incoming, Decision::Decline).is_ok());
    assert_eq!(not_opened(&mut p.viewer_ui, first, WAIT), ConnectWhy::Declined);
    p.finish();

    // Forgotten: the host's user removes this computer while the question is open, then answers it.
    let mut p = Pair::new();
    let entry = p.press();
    let incoming = p.incoming();
    assert!(p.host_ui.call(UiOp::ForgetDevice { device_id: p.viewer.hex() }).is_ok());
    assert!(p.answer(&incoming, Decision::Accept).is_ok());
    assert_eq!(not_opened(&mut p.viewer_ui, entry, WAIT), ConnectWhy::Forgotten);
    p.finish();

    // Unreachable: the viewer has no address for the host.
    let mut p = Pair::build(false);
    let entry = p.press();
    assert_eq!(not_opened(&mut p.viewer_ui, entry, WAIT), ConnectWhy::Unreachable);
    p.finish();

    // Timed out: the host's user does not answer within the host's 20 s (the host's own deadline: it is not
    // configurable, so this case runs in real time).
    let mut p = Pair::new();
    let started = Instant::now();
    let entry = p.press();
    let incoming = p.incoming();
    assert_eq!(not_opened(&mut p.viewer_ui, entry, Duration::from_secs(30)), ConnectWhy::TimedOut);
    assert!(started.elapsed() >= Duration::from_secs(19), "the host's 20 s ran: {:?}", started.elapsed());
    // The host's card closed with its own timeout: a late answer finds nothing.
    assert_eq!(p.answer(&incoming, Decision::Accept).unwrap_err(), ApiCode::NotFound);
    p.finish();
}

#[test]
fn cancelling_a_connect_entry_closes_the_hosts_card_and_opens_no_session() {
    let mut p = Pair::new();
    let entry = p.press();
    let incoming = p.incoming();
    assert!(matches!(next_state(&mut p.viewer_ui, entry), SessionState::Connecting));
    assert!(matches!(next_state(&mut p.viewer_ui, entry), SessionState::Prompted { .. }));

    // "Cancelar": the entry ends as a normal end, at once.
    let cancelled = Instant::now();
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: entry }).is_ok());
    assert!(matches!(next_state(&mut p.viewer_ui, entry), SessionState::Ended { reason: None }));

    // The host's card closes well before its own 20 s, and its user's late answer finds nothing.
    p.host_ui.notify("the card closing", |n| match n {
        Notify::IncomingClosed { request_id, .. } if *request_id == incoming.request_id => Some(()),
        _ => None,
    });
    assert!(cancelled.elapsed() < Duration::from_secs(5), "closed by the viewer's bye, not by the timeout");
    assert_eq!(p.answer(&incoming, Decision::Accept).unwrap_err(), ApiCode::NotFound);

    // No session opened on either side, and the entry says nothing more (no state after its end).
    assert!(p.viewer_ui.nothing_within(Duration::from_millis(500), |m| matches!(
        m,
        Ipc::UiNotify(Notify::Session { session_id, .. }) if *session_id == entry
    ) || is_active(m)));
    assert!(p.host.core.host().sessions().is_empty(), "the host holds no session");
    assert!(p.host_ui.nothing_within(Duration::from_millis(100), is_active), "the host's banner shows no session");
    // A second cancel finds nothing.
    assert_eq!(p.viewer_ui.call(UiOp::Disconnect { session_id: entry }).unwrap_err(), ApiCode::NotFound);

    // The device can be asked again right away: nothing of the cancelled attempt is left at the host.
    let again = p.press();
    let incoming = p.incoming();
    assert!(p.answer(&incoming, Decision::Decline).is_ok());
    assert_eq!(not_opened(&mut p.viewer_ui, again, WAIT), ConnectWhy::Declined);
    p.finish();
}

#[test]
fn an_entry_belongs_to_the_connection_that_made_it() {
    let mut p = Pair::new();
    let entry = p.press();
    let incoming = p.incoming();
    let mut other_ui = p.viewer.client(info(false));
    assert_eq!(other_ui.call(UiOp::Disconnect { session_id: entry }).unwrap_err(), ApiCode::NotFound);
    assert!(p.answer(&incoming, Decision::Decline).is_ok());
    assert_eq!(not_opened(&mut p.viewer_ui, entry, WAIT), ConnectWhy::Declined);
    drop(other_ui);
    p.finish();
}

#[test]
fn a_ui_that_goes_away_while_its_entry_waits_closes_the_hosts_card() {
    let mut p = Pair::new();
    let _entry = p.press();
    let incoming = p.incoming();
    let Pair { host, viewer, mut host_ui, mut viewer_ui } = p;
    viewer_ui.send(&Ipc::Bye { code: swapdesk_proto::ErrorCode::NoError });
    drop(viewer_ui);
    host_ui.notify("the card closing", |n| match n {
        Notify::IncomingClosed { request_id, .. } if *request_id == incoming.request_id => Some(()),
        _ => None,
    });
    assert!(host.core.host().sessions().is_empty());
    drop(host_ui);
    viewer.finish();
    host.finish();
}

#[test]
fn cancelling_during_the_connect_phase_of_a_wake_opens_no_session() {
    let mut other = Machine::new("other-pc");
    let (m, sent) = sleeper(&other);
    other.pin(&m, ALL);
    other.attach_helper(IpcRole::HelperUser);
    other.hub.set_console_session(Some(1));
    m.hub.set_console_session(Some(1));
    let mut ui = m.client(info(false));
    let mut other_ui = other.client(info(false));
    // The device is asleep: the wake begins.
    let req = ui.request(UiOp::Wake { device_id: other.hex(), intent: Intent::Control });
    let ReplyBody::SessionId(wake_id) = ui.reply(req, 26).expect("the attempt starts") else { panic!() };
    waking(&mut ui, wake_id);
    wait_for("the magic packet", || (!lock(&sent).is_empty()).then_some(()));

    // It appears: the same entry goes on as a connect, and the other machine's user is asked.
    m.sees(&other, false);
    let incoming = other_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    assert!(matches!(next_state_after_waking(&mut ui, wake_id), SessionState::Connecting));
    assert!(matches!(next_state(&mut ui, wake_id), SessionState::Prompted { .. }));

    // "Cancelar" in this phase: the entry ends and the other machine's card closes, with no session.
    assert!(ui.call(UiOp::Disconnect { session_id: wake_id }).is_ok());
    assert_eq!(ended(&mut ui, wake_id), None, "a normal end");
    other_ui.notify("the card closing", |n| match n {
        Notify::IncomingClosed { request_id, .. } if *request_id == incoming.request_id => Some(()),
        _ => None,
    });
    assert_eq!(
        other_ui
            .call(UiOp::AnswerIncoming {
                request_id: incoming.request_id,
                decision: Decision::Accept,
                grants: ALL.bits()
            })
            .unwrap_err(),
        ApiCode::NotFound,
        "the question is gone"
    );
    assert!(ui.nothing_within(Duration::from_millis(500), is_active), "no session opened");
    assert!(other.core.host().sessions().is_empty());
    drop((ui, other_ui));
    m.finish();
    other.finish();
}

/// The next `Connecting` or other non-waking state of the wake's entry (the countdown ticks are skipped).
fn next_state_after_waking(ui: &mut Client, entry: u64) -> SessionState {
    ui.notify("a state after waking", |n| match n {
        Notify::Session { session_id, state, .. }
            if *session_id == entry && !matches!(state, SessionState::Waking { .. }) =>
        {
            Some(*state)
        }
        _ => None,
    })
}
