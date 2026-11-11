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
        Self::build_with(ALL, viewer_finds_host.then_some(0))
    }

    /// `stored`: what the host lets the viewer's device do. `host_record`: the protocol minor of the host's record
    /// as the viewer sees it on the network (`None`: the viewer does not see the host).
    fn build_with(stored: PermissionSet, host_record: Option<u16>) -> Self {
        Self::build_as(stored, host_record, administrator_account())
    }

    /// `viewer_account`: the account of the viewer machine whose interface presses the button.
    fn build_as(stored: PermissionSet, host_record: Option<u16>, viewer_account: ConnInfo) -> Self {
        let mut host = Machine::new("host-pc");
        let viewer = Machine::new("viewer-pc");
        host.pin(&viewer, stored);
        viewer.pin(&host, PermissionSet::EMPTY);
        host.attach_helper(IpcRole::HelperSystem);
        host.attach_helper(IpcRole::HelperUser);
        if let Some(minor) = host_record {
            viewer.sees_minor(&host, false, minor);
        }
        host.hub.set_console_session(Some(1));
        let host_ui = host.client(info(false));
        let viewer_ui = viewer.client(viewer_account);
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

/// An administrator account's interface, not elevated: what Swapdesk.exe is when an administrator opens it.
fn administrator_account() -> ConnInfo {
    ConnInfo { can_elevate: true, ..info(false) }
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

// ---------- entry without a prompt ----------

fn is_incoming(m: &Ipc) -> bool {
    matches!(m, Ipc::UiNotify(Notify::IncomingRequest(_)))
}

/// Waits until the host has no live session.
fn host_free(p: &Pair) {
    let t0 = Instant::now();
    while !p.host.core.host().sessions().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(5), "the host's session did not end");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The owner's defect of 10/11: "Entrar sem perguntar" was granted and approved, and the next "Controlar" asked the
/// host again. The viewer never asked for the entry; now it does toward a host of 1.1, and the host lets it in at
/// once, without the machine-level bits.
#[test]
fn a_stored_unattended_grant_lets_controlar_in_without_a_question() {
    let stored = PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES | PermissionSet::AUDIO;
    let mut p = Pair::build_with(stored, Some(swapdesk_proto::PROTO_MINOR));
    let entry = p.press();

    // The session opens, and nothing was asked at the host.
    let session = active_as(&mut p.viewer_ui, Role::Viewer, &[entry]);
    assert!(p.host_ui.nothing_within(Duration::from_millis(200), is_incoming), "the host's user is not asked");

    // What it holds: what is stored and was asked for, without UNATTENDED (and never ADMIN_TERMINAL).
    let hosted = p.host.core.host().sessions();
    assert_eq!(hosted.len(), 1);
    assert_eq!(hosted[0].granted, PermissionSet::CONTROL | PermissionSet::FILES | PermissionSet::AUDIO);

    // The host's log says how it came in.
    let started = p.host.audit.entries().into_iter().find_map(|e| match e.event {
        AuditEvent::SessionStarted { mode, .. } => Some(mode),
        _ => None,
    });
    assert_eq!(started, Some(swapdesk_proto::GrantMode::Unattended));
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: session }).is_ok());
    p.finish();
}

/// Toward a host whose record is of 1.0 the viewer keeps the behaviour of 1.0: it does not ask, and the host's user
/// is asked, although the device holds `UNATTENDED` there.
#[test]
fn toward_a_1_0_host_the_viewer_never_asks_unattended() {
    let stored = PermissionSet::UNATTENDED | PermissionSet::CONTROL;
    let mut p = Pair::build_with(stored, Some(0));
    let entry = p.press();
    let incoming = p.incoming();
    assert_eq!(incoming.device.id, p.viewer.hex());
    assert!(p.viewer_ui.nothing_within(Duration::from_millis(200), is_active), "no session before the answer");
    assert!(p.answer(&incoming, Decision::Decline).is_ok());
    assert_eq!(not_opened(&mut p.viewer_ui, entry, Duration::from_secs(5)), ConnectWhy::Declined);
    p.finish();
}

/// Once the host has said, in a session over its pinned key, that it speaks minor 1, that wins over a record of
/// 1.0: the next connect asks for the entry and is let in without a question.
#[test]
fn a_viewer_asks_unattended_only_toward_a_host_known_to_be_minor_1() {
    let stored = PermissionSet::UNATTENDED | PermissionSet::CONTROL;
    let mut p = Pair::build_with(stored, Some(0));

    // A record without the key: no unattended entry is asked for, so the host asks its user.
    let entry = p.press();
    let incoming = p.incoming();
    assert!(p.answer(&incoming, Decision::Accept).is_ok());
    let first = active_as(&mut p.viewer_ui, Role::Viewer, &[entry]);
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: first }).is_ok());
    ended(&mut p.viewer_ui, first);
    host_free(&p);

    // What the host said in that session counts now.
    let again = p.press();
    let second = active_as(&mut p.viewer_ui, Role::Viewer, &[entry, again, first]);
    assert!(p.host_ui.nothing_within(Duration::from_millis(200), is_incoming), "no second question");
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: second }).is_ok());
    p.finish();
}

/// D45 (it was `a_standard_account_of_the_viewer_machine_does_not_ask_for_the_entry`, D10's interim rule): each
/// account has its own key, so the other machine's grant is this account's alone, and a standard account enters
/// without a prompt like an administrator. What reaches the machine level asks this machine's confirmation later.
#[test]
fn a_standard_account_of_the_viewer_machine_enters_without_a_prompt() {
    let stored = PermissionSet::UNATTENDED | PermissionSet::CONTROL;
    let mut p = Pair::build_as(stored, Some(swapdesk_proto::PROTO_MINOR), info(false));
    let entry = p.press();
    let session = active_as(&mut p.viewer_ui, Role::Viewer, &[entry]);
    assert!(p.host_ui.nothing_within(Duration::from_millis(200), is_incoming), "the host's user is not asked");
    let hosted = p.host.core.host().sessions();
    assert!(!hosted[0].granted.contains(PermissionSet::UNATTENDED), "without the machine-level bits");
    assert!(p.viewer_ui.call(UiOp::Disconnect { session_id: session }).is_ok());
    p.finish();
}

/// A viewer asks a 1.1 host for the terminal with every connect (an entry without a prompt grants only what was
/// asked for). On the card of a device with nothing stored that must not tick "Terminal": one "Aceitar" for a
/// request to watch or control would open a shell.
#[test]
fn the_card_of_a_device_with_nothing_stored_never_ticks_the_terminal() {
    let mut p = Pair::build_with(PermissionSet::EMPTY, Some(swapdesk_proto::PROTO_MINOR));
    let _entry = p.press();
    let incoming = p.incoming();
    let preset = PermissionSet::from_bits(incoming.preset).expect("a valid set");
    assert!(preset.contains(PermissionSet::CONTROL), "what the viewer's user asked for is ticked: {preset:?}");
    for bit in [PermissionSet::BIT_TERMINAL, PermissionSet::BIT_ADMIN_TERMINAL, PermissionSet::BIT_UNATTENDED] {
        assert_eq!(preset.bits() & bit, 0, "{preset:?}");
    }
    assert!(p.answer(&incoming, Decision::Decline).is_ok());
    p.finish();
}

// ---------- ELEVATION.md slice 7: the viewer-side actions of a session ----------

/// The elevated instance of the asking user, in the same session.
fn approver() -> ConnInfo {
    ConnInfo { admin: true, ..administrator_account() }
}

fn ticket_for(ui: &mut Client, action: swapdesk_ipc::ElevAction) -> Result<swapdesk_ipc::ElevationTicket, ApiCode> {
    match ui.call(UiOp::ElevationBegin { action })? {
        ReplyBody::ElevationTicket { ticket, .. } => Ok(ticket),
        other => panic!("not a ticket: {other:?}"),
    }
}

fn admin_open(ui: &mut Client, session_id: u64) -> Result<ReplyBody, ApiCode> {
    ui.call(UiOp::TermOpen { session_id, cols: 80, rows: 24, admin: true })
}

/// An entry without a prompt toward a host of 1.1, live in the viewer's interface.
fn unattended_pair() -> (Pair, u64) {
    let stored = PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL;
    let mut p = Pair::build_with(stored, Some(swapdesk_proto::PROTO_MINOR));
    let entry = p.press();
    let session = active_as(&mut p.viewer_ui, Role::Viewer, &[entry]);
    (p, session)
}

/// A prompted session that holds the administrator terminal (stored, asked for, and ticked at the host).
fn prompted_pair() -> (Pair, u64) {
    let mut p =
        Pair::build_with(PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL, Some(swapdesk_proto::PROTO_MINOR));
    let entry = p.press();
    let incoming = p.incoming();
    let ticked = (PermissionSet::CONTROL | PermissionSet::ADMIN_TERMINAL).bits();
    let op = UiOp::AnswerIncoming { request_id: incoming.request_id, decision: Decision::Accept, grants: ticked };
    assert!(p.host_ui.call(op).is_ok());
    let session = active_as(&mut p.viewer_ui, Role::Viewer, &[entry]);
    assert!(p.host.core.host().sessions()[0].granted.contains(PermissionSet::ADMIN_TERMINAL));
    (p, session)
}

#[test]
fn a_session_ticket_is_refused_unless_it_is_this_connections_and_can_be_elevated() {
    use swapdesk_ipc::ElevAction::ElevateSession;
    let (mut p, session) = unattended_pair();
    assert!(ticket_for(&mut p.viewer_ui, ElevateSession { session_id: session }).is_ok());
    assert_eq!(ticket_for(&mut p.viewer_ui, ElevateSession { session_id: session + 1 }), Err(ApiCode::NotFound));
    let mut other = p.viewer.client(administrator_account());
    assert_eq!(
        ticket_for(&mut other, ElevateSession { session_id: session }),
        Err(ApiCode::NotFound),
        "another connection's"
    );
    drop(other);
    p.finish();
    // A prompted session cannot be elevated.
    let (mut p, session) = prompted_pair();
    assert_eq!(ticket_for(&mut p.viewer_ui, ElevateSession { session_id: session }), Err(ApiCode::Invalid));
    p.finish();
}

#[test]
fn an_admin_terminal_ticket_needs_a_session_holding_admin_terminal() {
    use swapdesk_ipc::ElevAction::OpenAdminTerminal;
    // An entry without a prompt does not hold it before the elevation.
    let (mut p, session) = unattended_pair();
    assert_eq!(ticket_for(&mut p.viewer_ui, OpenAdminTerminal { session_id: session }), Err(ApiCode::Invalid));
    p.finish();
    let (mut p, session) = prompted_pair();
    assert!(ticket_for(&mut p.viewer_ui, OpenAdminTerminal { session_id: session }).is_ok());
    p.finish();
}

/// ACCOUNTS.md 9.3 and 8.3: "Elevar controle" and the administrator terminal need this machine's confirmation.
#[test]
fn elevate_and_the_administrator_terminal_need_the_viewer_side_pass() {
    let (mut p, session) = unattended_pair();
    assert_eq!(p.viewer_ui.call(UiOp::ElevateSession { session_id: session }).unwrap_err(), ApiCode::NeedsAdmin);
    let ticket =
        ticket_for(&mut p.viewer_ui, swapdesk_ipc::ElevAction::ElevateSession { session_id: session }).unwrap();
    p.viewer.hub.confirm_elevation(&ticket, &approver(), true).expect("confirmed");
    assert!(p.viewer_ui.call(UiOp::ElevateSession { session_id: session }).is_ok(), "the pass lets it through");
    let hosted = p.host.core.host().sessions();
    assert!(hosted[0].granted.contains(PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL));
    let elevated = p.host.audit.entries().into_iter().any(|e| e.event == AuditEvent::Elevated);
    assert!(elevated, "the host logs it");
    p.finish();
    // The administrator terminal of a prompted session: the same.
    let (mut p, session) = prompted_pair();
    assert_eq!(admin_open(&mut p.viewer_ui, session).unwrap_err(), ApiCode::NeedsAdmin);
    p.finish();
}

#[test]
fn an_elevated_session_opens_the_administrator_terminal_without_a_second_pass() {
    let (mut p, session) = unattended_pair();
    let ticket =
        ticket_for(&mut p.viewer_ui, swapdesk_ipc::ElevAction::ElevateSession { session_id: session }).unwrap();
    p.viewer.hub.confirm_elevation(&ticket, &approver(), true).expect("confirmed");
    assert!(p.viewer_ui.call(UiOp::ElevateSession { session_id: session }).is_ok());
    // The elevation was confirmed on this machine: the terminal asks nothing more, twice.
    assert!(admin_open(&mut p.viewer_ui, session).is_ok());
    assert!(admin_open(&mut p.viewer_ui, session).is_ok());
    p.finish();
}

#[test]
fn an_admin_terminal_pass_opens_one_terminal() {
    let (mut p, session) = prompted_pair();
    let ticket =
        ticket_for(&mut p.viewer_ui, swapdesk_ipc::ElevAction::OpenAdminTerminal { session_id: session }).unwrap();
    p.viewer.hub.confirm_elevation(&ticket, &approver(), true).expect("confirmed");
    assert!(admin_open(&mut p.viewer_ui, session).is_ok(), "the pass");
    assert_eq!(admin_open(&mut p.viewer_ui, session).unwrap_err(), ApiCode::NeedsAdmin, "used once");
    p.finish();
}

/// Review of S6, finding 2: a host of minor 0 (a 1.0 build, or one a forged record made look newer) lets a session
/// in with the machine-level bits already. This machine never confirmed that: its administrator terminal still asks
/// for the confirmation here.
#[test]
fn a_session_the_host_let_in_elevated_still_asks_this_machine_for_the_administrator_terminal() {
    let stored = PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL;
    let mut host = Machine::with_minor("host-pc", 0);
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, stored);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    viewer.sees_minor(&host, false, 1);
    host.hub.set_console_session(Some(1));
    let host_ui = host.client(info(false));
    let viewer_ui = viewer.client(administrator_account());
    let mut p = Pair { host, viewer, host_ui, viewer_ui };
    let entry = p.press();
    let session = active_as(&mut p.viewer_ui, Role::Viewer, &[entry]);
    let hosted = p.host.core.host().sessions();
    assert!(hosted[0].granted.contains(PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL), "1.0's grant");
    assert_eq!(admin_open(&mut p.viewer_ui, session).unwrap_err(), ApiCode::NeedsAdmin);
    p.finish();
}
