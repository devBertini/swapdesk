//! The administrator confirmation through the hub (docs/ELEVATION.md 2 and 3.2): a UI that is not elevated asks for a
//! ticket, an approver of its user (a function call here, or the restricted loop of an elevated instance) confirms
//! it, and the UI's next `SetGrants` for exactly that change is let through once. The refusals are the point.

use std::sync::mpsc::Sender;

use swapdesk_ipc::{Bytes, ElevAction, ElevationOutcome, ElevationTicket};
use swapdesk_proto::ErrorCode;

use super::*;

const SID: &str = "S-1-5-21-1";

/// The console's UI of an administrator account whose token is filtered: it may ask, it may not approve.
fn asker() -> ConnInfo {
    ConnInfo { session_id: 1, user: user(SID), admin: false, can_elevate: true }
}

/// The elevated instance of the same user and session.
fn approver() -> ConnInfo {
    ConnInfo { session_id: 1, user: user(SID), admin: true, can_elevate: true }
}

fn unattended() -> u16 {
    PermissionSet::UNATTENDED.bits()
}

fn grants_op(id: &str, bits: u16) -> UiOp {
    UiOp::SetGrants { device_id: id.into(), grants: bits }
}

fn begin(ui: &mut Client, id: &str, bits: u16) -> Result<ElevationTicket, ApiCode> {
    match ui.call(UiOp::ElevationBegin { action: ElevAction::SetGrants { device_id: id.into(), grants: bits } })? {
        ReplyBody::ElevationTicket { ticket, .. } => Ok(ticket),
        other => panic!("not a ticket: {other:?}"),
    }
}

fn end(ui: &mut Client, ticket: ElevationTicket) -> ElevationOutcome {
    match ui.call(UiOp::ElevationEnd { ticket }).expect("End never fails") {
        ReplyBody::ElevationOutcome(o) => o,
        other => panic!("not an outcome: {other:?}"),
    }
}

/// Two machines: `m`, where the UIs connect, and `other`, a paired device that holds `CONTROL` on `m`.
fn pair() -> (Machine, Machine, String) {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    // The single-use ticket and pass on their own; the grace of D62 has its own test.
    m.hub.set_elevation_grace(Duration::ZERO);
    let id = other.hex();
    (m, other, id)
}

fn granted_here(m: &Machine) -> PermissionSet {
    m.trust.read(|s| s.devices[0].grants_here)
}

fn open_tickets(m: &Machine) -> usize {
    lock(&m.hub.hub.elevation).open_tickets()
}

#[test]
fn a_confirmed_set_grants_ticket_lets_the_asking_connection_grant_unattended_once() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());

    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "before the confirmation");
    let ticket = begin(&mut ui, &id, unattended()).expect("a ticket");
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "an unconfirmed ticket");

    m.hub.confirm_elevation(&ticket, &approver(), true).expect("confirmed");
    // The resend passes, through the same path as an administrator's change: store, grantor, audit, notification.
    assert!(ui.call(grants_op(&id, unattended())).is_ok());
    assert_eq!(granted_here(&m), PermissionSet::UNATTENDED);
    assert_eq!(m.trust.read(|s| s.devices[0].unattended_granted_by.clone().map(|u| u.id)), Some(SID.to_owned()));
    let audited = m.audit.entries().into_iter().any(|e| {
        matches!(e.event, AuditEvent::GrantsChanged { to, by, .. } if to == PermissionSet::UNATTENDED && by.id == SID)
    });
    assert!(audited, "the change is in the activity log, by the asking connection's user");
    ui.notify("DevicesChanged with UNATTENDED", |n| match n {
        Notify::DevicesChanged(l) if l.iter().any(|d| d.grants_here == unattended()) => Some(()),
        _ => None,
    });

    // Narrowing is open to any console connection; widening again is a new change that needs a new confirmation.
    assert!(ui.call(grants_op(&id, PermissionSet::VIEW.bits())).is_ok());
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "the pass was used up");
    assert_eq!(granted_here(&m), PermissionSet::VIEW);

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn the_pass_is_bound_to_the_exact_action_and_to_the_asking_connection() {
    let (m, other, id) = pair();
    let third = Machine::new("third");
    m.pin(&third, PermissionSet::CONTROL);
    let mut ui = m.client(asker());
    let mut same_user = m.client(asker());

    let ticket = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&ticket, &approver(), true).expect("confirmed");

    // Other bits, another device, a second connection of the same user: none of them has the pass.
    let wider = PermissionSet::UNATTENDED.union(PermissionSet::FILES).bits();
    assert_eq!(ui.call(grants_op(&id, wider)).unwrap_err(), ApiCode::NeedsAdmin, "other bits");
    assert_eq!(ui.call(grants_op(&third.hex(), unattended())).unwrap_err(), ApiCode::NeedsAdmin, "another device");
    assert_eq!(same_user.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "another connection");
    assert_eq!(
        m.trust.read(|s| s.devices.iter().map(|d| d.grants_here).collect::<Vec<_>>()),
        vec![PermissionSet::CONTROL; 2]
    );

    // The refusals did not use the pass up: the exact change still goes through on the asking connection.
    assert!(ui.call(grants_op(&id, unattended())).is_ok());
    assert_eq!(granted_here(&m), PermissionSet::UNATTENDED);

    drop((ui, same_user));
    m.finish();
    other.finish();
    third.finish();
}

#[test]
fn a_ticket_is_single_use_and_ends_after_its_ttl() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    m.hub.set_elevation_ttls(Duration::from_millis(1500), Duration::from_secs(30));

    // Answered once: neither a second answer nor a description follows.
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    let (action, peer, left) = m.hub.describe_elevation(&t, &approver()).expect("described");
    assert_eq!(action, ElevAction::SetGrants { device_id: id.clone(), grants: unattended() });
    assert_eq!(peer, "other-pc", "the device's name");
    assert!((1..=2).contains(&left), "the seconds left, rounded up: {left}");
    m.hub.confirm_elevation(&t, &approver(), true).expect("confirmed");
    assert_eq!(m.hub.confirm_elevation(&t, &approver(), true).unwrap_err(), ApiCode::NotFound);
    assert_eq!(m.hub.confirm_elevation(&t, &approver(), false).unwrap_err(), ApiCode::NotFound);
    assert_eq!(m.hub.describe_elevation(&t, &approver()).unwrap_err(), ApiCode::NotFound);
    assert!(ui.call(grants_op(&id, unattended())).is_ok(), "the one confirmation gave one pass");
    ui.call(grants_op(&id, PermissionSet::VIEW.bits())).unwrap();

    // Not answered in time: it is gone for everyone, and no pass was made.
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    std::thread::sleep(Duration::from_millis(1700));
    assert_eq!(m.hub.describe_elevation(&t, &approver()).unwrap_err(), ApiCode::NotFound);
    assert_eq!(m.hub.confirm_elevation(&t, &approver(), true).unwrap_err(), ApiCode::NotFound);
    assert_eq!(end(&mut ui, t), ElevationOutcome::Unknown);
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin);
    assert_eq!(open_tickets(&m), 0, "the expired entry was swept");

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn the_pass_expires_after_its_ttl() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    m.hub.set_elevation_ttls(Duration::from_secs(30), Duration::from_millis(1000));

    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&t, &approver(), true).expect("confirmed");
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "the pass ran out");
    assert_eq!(granted_here(&m), PermissionSet::CONTROL);
    assert!(lock(&m.hub.hub.conns()[0].passes).is_empty(), "the expired pass was swept");

    // The window is the pass's own: a fresh confirmation used inside it works.
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&t, &approver(), true).expect("confirmed");
    assert!(ui.call(grants_op(&id, unattended())).is_ok());

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_connection_that_is_not_elevated_can_neither_describe_nor_confirm() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");

    // The asking connection itself, and any other token that is not elevated, whoever it belongs to.
    for who in [asker(), ConnInfo { can_elevate: false, ..asker() }] {
        assert_eq!(m.hub.describe_elevation(&t, &who).unwrap_err(), ApiCode::NeedsAdmin);
        assert_eq!(m.hub.confirm_elevation(&t, &who, true).unwrap_err(), ApiCode::NeedsAdmin);
    }
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "no pass was made");

    // The ticket is still open for the one that may.
    assert!(m.hub.describe_elevation(&t, &approver()).is_ok());

    drop(ui);
    m.finish();
    other.finish();
}

/// ACCOUNTS.md S7 (it was `an_approver_of_another_user_or_session_is_refused`): an administrator of another account
/// approves in the asking account's Windows session (over its shoulder); an approver of another session, or one that
/// is not elevated, is refused and makes no pass.
#[test]
fn an_unelevated_or_other_session_approver_is_refused() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");

    let another_session = ConnInfo { session_id: 9, ..approver() };
    assert_eq!(m.hub.describe_elevation(&t, &another_session).unwrap_err(), ApiCode::NotPermitted);
    assert_eq!(m.hub.confirm_elevation(&t, &another_session, true).unwrap_err(), ApiCode::NotPermitted);
    let not_elevated = ConnInfo { user: user("S-1-5-21-2"), admin: false, ..approver() };
    assert_eq!(m.hub.confirm_elevation(&t, &not_elevated, true).unwrap_err(), ApiCode::NeedsAdmin);
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "no pass was made");
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here), PermissionSet::CONTROL);

    // Refused answers leave the ticket as it was: another user of the same session may still approve it.
    let over_the_shoulder = ConnInfo { user: user("S-1-5-21-2"), ..approver() };
    assert!(m.hub.confirm_elevation(&t, &over_the_shoulder, true).is_ok());

    drop(ui);
    m.finish();
    other.finish();
}

/// ACCOUNTS.md S7: a standard account's change is confirmed by an administrator of another account in the same
/// Windows session; the grant lands in the asking account's store, and the log names the administrator.
#[test]
fn an_administrator_of_another_account_in_the_same_session_confirms_a_standard_accounts_ticket() {
    let (m, other, id) = pair();
    let mut standard = m.client(info(false));
    let t = begin(&mut standard, &id, unattended()).expect("a standard account gets a ticket");
    let admin = ConnInfo { user: user("S-1-5-21-500"), admin: true, ..info(false) };
    m.hub.confirm_elevation(&t, &admin, true).expect("confirmed over the shoulder");
    assert!(standard.call(grants_op(&id, unattended())).is_ok());
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here), PermissionSet::UNATTENDED);
    drop(standard);
    m.finish();
    other.finish();
}

#[test]
fn the_grant_lands_in_the_asking_accounts_store_with_the_approver_in_the_log() {
    let (m, other, id) = pair();
    let mut standard = m.client(info(false));
    let t = begin(&mut standard, &id, unattended()).expect("a ticket");
    let admin = ConnInfo { user: user("S-1-5-21-500"), admin: true, ..info(false) };
    m.hub.confirm_elevation(&t, &admin, true).expect("confirmed");
    assert!(standard.call(grants_op(&id, unattended())).is_ok());
    // D13 keeps the asking account as the one who granted the entry; the log names who approved it.
    assert_eq!(
        m.trust.read(|s| s.devices[0].unattended_granted_by.clone().map(|u| u.id)),
        Some("S-1-5-21-1".to_owned())
    );
    let by = m.audit.entries().into_iter().find_map(|e| match e.event {
        AuditEvent::GrantsChanged { by, .. } => Some(by.id),
        _ => None,
    });
    assert_eq!(by.as_deref(), Some("S-1-5-21-500"));
    drop(standard);
    m.finish();
    other.finish();
}

/// ACCOUNTS.md S7 (it was `an_account_that_is_not_an_administrator_gets_no_ticket`): a standard account gets a
/// ticket; without a confirmation it still changes nothing.
#[test]
fn a_standard_account_gets_a_ticket() {
    let (m, other, id) = pair();
    let mut standard = m.client(info(false));
    assert!(!info(false).can_elevate);
    assert!(begin(&mut standard, &id, unattended()).is_ok());
    assert_eq!(open_tickets(&m), 1);
    assert_eq!(standard.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin);
    drop(standard);
    m.finish();
    other.finish();
}

#[test]
fn a_ui_of_another_session_gets_no_ticket() {
    let (m, other, id) = pair();
    let away = ConnInfo { session_id: 9, user: user("S-1-5-21-2"), admin: false, can_elevate: true };
    let mut ui = m.client(away);
    assert_eq!(begin(&mut ui, &id, unattended()).unwrap_err(), ApiCode::NotPermitted, "not the console's UI");
    assert_eq!(open_tickets(&m), 0);

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_ticket_is_refused_for_a_change_the_command_would_refuse() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    // As `SetGrants` validates: reserved bits, a broken implication, a malformed id, a device that is not paired.
    assert_eq!(begin(&mut ui, &id, 0x8000).unwrap_err(), ApiCode::Invalid);
    assert_eq!(begin(&mut ui, &id, PermissionSet::BIT_UNATTENDED).unwrap_err(), ApiCode::Invalid);
    assert_eq!(begin(&mut ui, "nope", unattended()).unwrap_err(), ApiCode::Invalid);
    assert_eq!(begin(&mut ui, &device_id_hex(&[9; 16]), unattended()).unwrap_err(), ApiCode::NotFound);
    assert_eq!(open_tickets(&m), 0);

    // The device id's case does not matter, the pass is for the device.
    let upper = id.to_uppercase();
    let t = begin(&mut ui, &upper, unattended()).expect("a ticket");
    let (action, ..) = m.hub.describe_elevation(&t, &approver()).expect("described");
    assert_eq!(action, ElevAction::SetGrants { device_id: id.clone(), grants: unattended() });
    m.hub.confirm_elevation(&t, &approver(), true).expect("confirmed");
    assert!(ui.call(grants_op(&upper, unattended())).is_ok());

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn declining_leaves_no_pass_and_end_reports_each_outcome() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    let mut other_ui = m.client(asker());

    // Declined: no pass, nothing answers it again, End says so.
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&t, &approver(), false).expect("declined");
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin);
    assert_eq!(m.hub.confirm_elevation(&t, &approver(), true).unwrap_err(), ApiCode::NotFound);
    assert_eq!(end(&mut ui, t), ElevationOutcome::Declined);
    assert_eq!(end(&mut ui, t), ElevationOutcome::Unknown, "End removes the record");

    // Never answered: End closes it.
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    assert_eq!(end(&mut other_ui, t), ElevationOutcome::Unknown, "another connection's ticket");
    assert!(m.hub.describe_elevation(&t, &approver()).is_ok(), "which End of another connection left open");
    assert_eq!(end(&mut ui, t), ElevationOutcome::Open);
    assert_eq!(m.hub.confirm_elevation(&t, &approver(), true).unwrap_err(), ApiCode::NotFound, "closed by End");
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin);

    // Confirmed.
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&t, &approver(), true).expect("confirmed");
    assert_eq!(end(&mut ui, t), ElevationOutcome::Confirmed);
    assert!(ui.call(grants_op(&id, unattended())).is_ok(), "ending the ticket keeps the pass");

    // A ticket nobody issued.
    assert_eq!(end(&mut ui, ElevationTicket([7; 16])), ElevationOutcome::Unknown);
    assert_eq!(open_tickets(&m), 0);

    drop((ui, other_ui));
    m.finish();
    other.finish();
}

#[test]
fn closing_the_asking_connection_drops_its_ticket_and_passes() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    // One ticket confirmed (a pass), a second one open.
    let first = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&first, &approver(), true).expect("confirmed");
    let second = begin(&mut ui, &id, unattended()).expect("a ticket");
    let conn = m.hub.hub.conns().pop().expect("the connection");
    assert_eq!((open_tickets(&m), lock(&conn.passes).len()), (1, 1));

    ui.send(&Ipc::Bye { code: ErrorCode::NoError });
    wait_for("the connection to end", || (m.hub.connections() == 0).then_some(()));
    assert_eq!(open_tickets(&m), 0);
    assert!(lock(&conn.passes).is_empty());
    assert_eq!(m.hub.describe_elevation(&second, &approver()).unwrap_err(), ApiCode::NotFound);
    assert_eq!(m.hub.confirm_elevation(&second, &approver(), true).unwrap_err(), ApiCode::NotFound);

    // Another connection of the same user starts with nothing.
    let mut again = m.client(asker());
    assert_eq!(again.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin);

    drop(again);
    m.finish();
    other.finish();
}

#[test]
fn a_new_begin_replaces_the_connections_open_ticket() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());

    let first = begin(&mut ui, &id, unattended()).expect("a ticket");
    let second = begin(&mut ui, &id, unattended()).expect("a ticket");
    assert_ne!(first.0, second.0, "a fresh ticket each time");
    assert_eq!(open_tickets(&m), 1);
    assert_eq!(m.hub.describe_elevation(&first, &approver()).unwrap_err(), ApiCode::NotFound);
    assert_eq!(m.hub.confirm_elevation(&first, &approver(), true).unwrap_err(), ApiCode::NotFound);
    assert_eq!(end(&mut ui, first), ElevationOutcome::Unknown);
    assert!(m.hub.describe_elevation(&second, &approver()).is_ok());

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn the_sixteenth_open_ticket_is_the_last() {
    let (m, other, id) = pair();
    let mut uis: Vec<Client> = (0..17).map(|_| m.client(asker())).collect();
    let mut tickets = Vec::new();
    for ui in uis.iter_mut().take(16) {
        tickets.push(begin(ui, &id, unattended()).expect("one of the sixteen"));
    }
    assert_eq!(begin(&mut uis[16], &id, unattended()).unwrap_err(), ApiCode::Busy);
    assert_eq!(open_tickets(&m), 16);

    // A connection that holds one replaces it; that is not a seventeenth.
    assert!(begin(&mut uis[0], &id, unattended()).is_ok());
    assert_eq!(open_tickets(&m), 16);
    // A ticket that ended makes room.
    assert_eq!(end(&mut uis[1], tickets[1]), ElevationOutcome::Open);
    assert!(begin(&mut uis[16], &id, unattended()).is_ok());
    assert_eq!(open_tickets(&m), 16);

    drop(uis);
    m.finish();
    other.finish();
}

#[test]
fn the_trace_names_the_action_and_the_device_never_the_ticket() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&t, &approver(), true).expect("confirmed");
    let declined = begin(&mut ui, &id, unattended()).expect("a ticket");
    m.hub.confirm_elevation(&declined, &approver(), false).expect("declined");
    assert_eq!(end(&mut ui, declined), ElevationOutcome::Declined);

    let lines: Vec<String> = m.log.lines().iter().map(|l| l.text.clone()).collect();
    for what in ["opened", "confirmed", "declined", "ended"] {
        let wanted = format!("elevation ticket {what}: SetGrants for device {id}");
        assert!(lines.iter().any(|l| l.contains(&wanted)), "no trace line {wanted:?} in {lines:?}");
    }
    for ticket in [t, declined] {
        let hex: String = ticket.0.iter().map(|b| format!("{b:02x}")).collect();
        assert!(lines.iter().all(|l| !l.to_lowercase().contains(&hex)), "a ticket reached the trace");
    }

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_ui_connection_cannot_answer_for_a_ticket() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    let mut elevated_ui = m.client(approver());
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");

    // Ops 36 and 37 belong to the restricted loop; an elevated UI connection is no approver either.
    for who in [&mut ui, &mut elevated_ui] {
        assert_eq!(who.call(UiOp::ElevationDescribe { ticket: t }).unwrap_err(), ApiCode::NotPermitted);
        assert_eq!(who.call(UiOp::ElevationConfirm { ticket: t, approve: true }).unwrap_err(), ApiCode::NotPermitted);
    }
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "no pass was made");
    assert!(m.hub.describe_elevation(&t, &approver()).is_ok(), "the ticket is untouched");

    drop((ui, elevated_ui));
    m.finish();
    other.finish();
}

// ---------- the elevated instance's connection ----------

/// The client end of an elevator connection: the handshake as role `Elevator`, then what the service sends.
struct ElevatorPipe {
    writer: FrameWriter<PipeWriter>,
    inbox: Receiver<Option<Ipc>>,
    serve: JoinHandle<Result<(), IpcError>>,
    next_req: u32,
}

impl ElevatorPipe {
    fn connect(hub: &UiHub, info: ConnInfo) -> Self {
        // The pipe with a read limit, so a hub that stays silent fails the test instead of hanging it.
        let (client_end, server_end) = crate::testpipe::duplex();
        let hub = hub.clone();
        let serve = std::thread::spawn(move || {
            let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
            let hello = Handshake::server(&mut reader, &mut writer, 7)?;
            assert_eq!(hello.role, IpcRole::Elevator);
            hub.serve_elevator(reader, writer, info)
        });
        let (mut reader, mut writer) = (FrameReader::new(client_end.reader), FrameWriter::new(client_end.writer));
        Handshake::client(&mut reader, &mut writer, IpcRole::Elevator, 4321).expect("handshake");
        let (tx, inbox): (Sender<Option<Ipc>>, _) = channel();
        std::thread::spawn(move || {
            loop {
                let msg = reader.read_message().ok().flatten();
                let last = msg.is_none();
                if tx.send(msg).is_err() || last {
                    break;
                }
            }
        });
        Self { writer, inbox, serve, next_req: 1 }
    }

    fn call(&mut self, op: UiOp) -> u32 {
        let req = self.next_req;
        self.next_req += 1;
        self.writer.write_message(&Ipc::UiRequest { req, op }).expect("write");
        req
    }

    /// The next message, `None` when the pipe ended.
    fn next(&mut self) -> Option<Ipc> {
        self.inbox.recv_timeout(WAIT).expect("a message or the end of the pipe")
    }

    /// The reply to the request just sent, decoded with `op`.
    fn reply(&mut self, req: u32, op: u16) -> Result<ReplyBody, ApiCode> {
        match self.next() {
            Some(Ipc::UiReply { req: r, status, body }) if r == req => match status_code(status).expect("a status") {
                Some(code) => Err(code),
                None => Ok(ReplyBody::decode(op, &body.0).expect("the body decodes")),
            },
            other => panic!("not the reply to {req}: {other:?}"),
        }
    }

    /// The service closes with `Bye(ProtocolViolation)` and the pipe ends; `serve_elevator` reports a protocol error.
    fn expect_violation(mut self) {
        assert!(matches!(self.next(), Some(Ipc::Bye { code: ErrorCode::ProtocolViolation })));
        assert!(self.next().is_none(), "the pipe ends after the Bye");
        assert!(self.serve.join().expect("serve ends").is_err());
    }
}

#[test]
fn an_elevator_connection_gets_no_notification_and_no_other_op() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    let ticket = begin(&mut ui, &id, unattended()).expect("a ticket");

    // It is not one of the hub's connections.
    let mut elevator = ElevatorPipe::connect(&m.hub, approver());
    assert_eq!(m.hub.connections(), 1, "the UI only");

    // A change that notifies every connection reaches the UI and not the elevator.
    assert!(ui.call(UiOp::RenameDevice { device_id: id.clone(), name: "renamed".into() }).is_ok());
    ui.notify("DevicesChanged", |n| matches!(n, Notify::DevicesChanged(_)).then_some(()));
    assert!(elevator.inbox.recv_timeout(Duration::from_millis(300)).is_err(), "the elevator was sent something");

    // Describe, then Confirm: the two things it is for.
    let req = elevator.call(UiOp::ElevationDescribe { ticket });
    let ReplyBody::ElevationAction { action, peer_name, remaining_s } = elevator.reply(req, 36).expect("described")
    else {
        panic!("not an action")
    };
    assert_eq!(action, ElevAction::SetGrants { device_id: id.clone(), grants: unattended() });
    assert_eq!(peer_name, "other-pc", "the name the device had when the ticket was opened");
    assert!(remaining_s > 100);
    let req = elevator.call(UiOp::ElevationConfirm { ticket, approve: true });
    assert!(matches!(elevator.reply(req, 37), Ok(ReplyBody::Done)));
    // It ends after the confirmation.
    assert!(elevator.next().is_none(), "the service ended the connection");
    assert!(elevator.serve.join().expect("serve ends").is_ok());
    assert_eq!(m.hub.connections(), 1);

    // The pass is the UI's.
    assert!(ui.call(grants_op(&id, unattended())).is_ok());

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn an_elevator_connection_serves_nothing_but_one_describe_and_one_confirm() {
    let (m, other, id) = pair();
    let mut ui = m.client(asker());
    let t = begin(&mut ui, &id, unattended()).expect("a ticket");

    // Any other op, from a UI's repertoire or the asking side of this very feature.
    for op in [
        UiOp::ListDevices,
        UiOp::ElevationBegin { action: ElevAction::SetGrants { device_id: id.clone(), grants: unattended() } },
        UiOp::ElevationEnd { ticket: t },
        UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::VIEW.bits() },
    ] {
        let mut elevator = ElevatorPipe::connect(&m.hub, approver());
        elevator.call(op);
        elevator.expect_violation();
    }
    // A message that is no request at all.
    let mut elevator = ElevatorPipe::connect(&m.hub, approver());
    elevator.writer.write_message(&Ipc::TermInput { term_id: 1, bytes: Bytes(vec![b'x']) }).expect("write");
    elevator.expect_violation();

    // A second Describe, and a second Confirm, which here would come first.
    let mut elevator = ElevatorPipe::connect(&m.hub, approver());
    let req = elevator.call(UiOp::ElevationDescribe { ticket: t });
    assert!(elevator.reply(req, 36).is_ok());
    elevator.call(UiOp::ElevationDescribe { ticket: t });
    elevator.expect_violation();
    assert_eq!(m.hub.connections(), 1);

    // Nothing above changed the ticket, and a refused elevator (not elevated) is told so and may try again no more.
    let mut weak = ElevatorPipe::connect(&m.hub, asker());
    let req = weak.call(UiOp::ElevationConfirm { ticket: t, approve: true });
    assert_eq!(weak.reply(req, 37).unwrap_err(), ApiCode::NeedsAdmin);
    assert!(weak.next().is_none(), "ended after its Confirm");
    assert_eq!(ui.call(grants_op(&id, unattended())).unwrap_err(), ApiCode::NeedsAdmin, "no pass was made");
    assert!(m.hub.describe_elevation(&t, &approver()).is_ok(), "the ticket is still open");

    // Declining through the pipe ends it as well, and leaves no pass.
    let mut elevator = ElevatorPipe::connect(&m.hub, approver());
    let req = elevator.call(UiOp::ElevationConfirm { ticket: t, approve: false });
    assert!(matches!(elevator.reply(req, 37), Ok(ReplyBody::Done)));
    assert!(elevator.next().is_none());
    assert_eq!(end(&mut ui, t), ElevationOutcome::Declined);

    drop(ui);
    m.finish();
    other.finish();
}

#[test]
fn a_silent_elevator_connection_ends_after_its_lifetime_and_a_bye_ends_it_at_once() {
    let (m, other, _id) = pair();
    lock(&m.hub.hub.elevation).elevator_lifetime = Duration::from_millis(300);

    let mut silent = ElevatorPipe::connect(&m.hub, approver());
    assert!(matches!(silent.next(), Some(Ipc::Bye { code: ErrorCode::Timeout })));
    assert!(silent.next().is_none());
    assert!(silent.serve.join().expect("serve ends").is_ok());

    lock(&m.hub.hub.elevation).elevator_lifetime = Duration::from_secs(125);
    let mut leaving = ElevatorPipe::connect(&m.hub, approver());
    leaving.writer.write_message(&Ipc::Bye { code: ErrorCode::NoError }).expect("write");
    assert!(leaving.serve.join().expect("serve ends").is_ok());

    m.finish();
    other.finish();
}

fn begin_ttl(ui: &mut Client, id: &str, bits: u16) -> (ElevationTicket, u16) {
    match ui.call(UiOp::ElevationBegin { action: ElevAction::SetGrants { device_id: id.into(), grants: bits } }) {
        Ok(ReplyBody::ElevationTicket { ticket, ttl_s }) => (ticket, ttl_s),
        other => panic!("not a ticket: {other:?}"),
    }
}

/// D62 (owner, 11/11: one UAC prompt, then only the app's own confirmation for each permission): after an
/// administrator approved a device's permissions on a connection, the next change of the same device on the same
/// connection gets a ticket already confirmed (`ttl_s` 0) by the same approver; another device or another connection
/// needs the administrator again.
#[test]
fn an_approval_covers_further_changes_of_the_same_device_on_the_same_connection_only() {
    let (m, other, id) = pair();
    m.hub.set_elevation_grace(Duration::from_secs(600));
    let third = Machine::new("third");
    m.pin(&third, PermissionSet::CONTROL);
    let mut ui = m.client(asker());
    let mut same_user = m.client(asker());

    let (t, ttl) = begin_ttl(&mut ui, &id, unattended());
    assert!(ttl > 0, "the first change needs the administrator");
    m.hub.confirm_elevation(&t, &approver(), true).expect("confirmed");
    assert_eq!(end(&mut ui, t), ElevationOutcome::Confirmed);
    assert!(ui.call(grants_op(&id, unattended())).is_ok());

    // The next change of the same device: NeedsAdmin first (the interface then shows its step), then a ticket
    // confirmed at once, and the change goes through with the same approver.
    let wider = PermissionSet::UNATTENDED.union(PermissionSet::CONTROL).union(PermissionSet::FILES).bits();
    assert_eq!(ui.call(grants_op(&id, wider)).unwrap_err(), ApiCode::NeedsAdmin);
    let (t, ttl) = begin_ttl(&mut ui, &id, wider);
    assert_eq!(ttl, 0, "confirmed by the approval of a moment ago");
    assert_eq!(end(&mut ui, t), ElevationOutcome::Confirmed);
    assert!(ui.call(grants_op(&id, wider)).is_ok());
    assert_eq!(granted_here(&m).bits(), wider);
    let by_approver = m
        .audit
        .entries()
        .into_iter()
        .filter(|e| matches!(&e.event, AuditEvent::GrantsChanged { by, .. } if by.id == SID));
    assert_eq!(by_approver.count(), 2, "both changes name the approver");

    // Another device, another connection: the administrator again.
    assert!(begin_ttl(&mut ui, &third.hex(), unattended()).1 > 0, "another device");
    assert!(begin_ttl(&mut same_user, &id, wider).1 > 0, "another connection");

    drop((ui, same_user));
    m.finish();
    other.finish();
    third.finish();
}
