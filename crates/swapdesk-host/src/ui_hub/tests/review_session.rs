//! Review of 22/11/2026, lane B and E (permissions, session layer, swap): the hub's side of the fixes.

use swapdesk_proto::ErrorCode;
use swapdesk_session::{EndKind, HostEvent, SessionEnd};
use swapdesk_trust::AuditEvent;

use super::extras::active_as;
use super::*;
use crate::service::SyncError;

/// Two paired machines with no session, seeing each other, after their first sync. `a_allows` is what A lets B do,
/// `b_allows` what B lets A do.
fn synced(a_allows: PermissionSet, b_allows: PermissionSet) -> (Machine, Machine) {
    let a = Machine::new("pc-a");
    let b = Machine::new("pc-b");
    a.pin(&b, a_allows);
    b.pin(&a, b_allows);
    a.sees_minor(&b, false, 1);
    b.sees_minor(&a, false, 1);
    wait_there(&a, b_allows, Duration::from_secs(6), "A learns what B allows");
    wait_there(&b, a_allows, Duration::from_secs(6), "B learns what A allows");
    // Past the 3 s of the machine with the higher ID, so no appearance sync is still to come.
    std::thread::sleep(super::super::sync_dial::TIE_DELAY + Duration::from_millis(500));
    (a, b)
}

fn there(m: &Machine) -> Option<PermissionSet> {
    m.trust.read(|s| s.devices[0].grants_there)
}

fn wait_there(m: &Machine, want: PermissionSet, limit: Duration, what: &str) -> Duration {
    let t = Instant::now();
    while there(m) != Some(want) {
        assert!(t.elapsed() < limit, "{what}: still {:?} after {:?}", there(m), t.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    t.elapsed()
}

/// Review of 22/11/2026, B6: a push that the peer refused `BUSY` (its budget for this device was spent) was never
/// tried again, so the user's last change stayed undelivered until something else made a sync.
#[test]
fn a_grants_push_refused_busy_is_tried_again_until_it_is_delivered() {
    let (a, b) = synced(PermissionSet::CONTROL, PermissionSet::VIEW);
    // Spend A's budget on B (a burst of 3): the next sync is refused `BUSY`. The dialer may read that close as no
    // answer (the stream ends before the connection's close code arrives); both are what a push can meet.
    let rt = a.core.runtime().clone();
    let mut tries = Vec::new();
    for _ in 0..5 {
        let r = rt.block_on(a.core.sync_grants_async(b.session_addr(), b.id()));
        let refused = matches!(r, Err(SyncError::Busy | SyncError::Failed));
        tries.push(format!("{r:?}"));
        if refused {
            break;
        }
    }
    assert!(tries.last().is_some_and(|t| t.starts_with("Err")), "B refuses A's syncs past its budget: {tries:?}");
    let mut ui = a.client(info(false));
    let wider = PermissionSet::CONTROL.union(PermissionSet::FILES);
    assert!(ui.call(UiOp::SetGrants { device_id: b.hex(), grants: wider.bits() }).is_ok());
    // The push goes 1 s later and is refused; B earns one sync back every 10 s.
    wait_there(&b, wider, Duration::from_secs(28), "B hears the change once its budget allows");
    drop(ui);
    a.finish();
    b.finish();
}

fn host_started(other: &Machine, session_id: u64) -> HostEvent {
    HostEvent::SessionStarted {
        device: other.id(),
        session_id,
        mode: swapdesk_proto::GrantMode::Prompted,
        granted: ALL,
    }
}

fn host_ended(other: &Machine, session_id: u64, kind: EndKind, resumable: bool) -> HostEvent {
    HostEvent::Ended {
        device: other.id(),
        session_id,
        end: SessionEnd { kind, code: ErrorCode::NoError },
        resumable,
        why: None,
    }
}

fn last_hosted(m: &Machine) -> Option<usize> {
    lock(&m.hooks.hosted).last().copied()
}

fn audited_ends(m: &Machine) -> usize {
    m.audit.entries().iter().filter(|e| matches!(e.event, AuditEvent::SessionEnded(_))).count()
}

/// Review of 22/11/2026, B8: a session can end between its commit and `SessionStarted` (its first messages could
/// not be sent, a panic). The hub then heard `Ended` for a session it never heard start, and counted one live
/// session down that was another's: "keep awake" and the hosted count went off while a session still ran.
#[test]
fn an_end_the_hub_never_heard_start_counts_no_session_down() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, ALL);
    m.hub.host_ui().event(host_started(&other, 41));
    assert_eq!((m.hub.live_sessions(), last_hosted(&m)), (1, Some(1)));
    // Session 42 was committed and ended before it was ever reported started.
    m.hub.host_ui().event(host_ended(&other, 42, EndKind::Lost, false));
    assert_eq!(m.hub.live_sessions(), 1, "session 41 still runs");
    assert_eq!(last_hosted(&m), Some(1), "the hosted count went down for a session that never began");
    assert_eq!(audited_ends(&m), 0, "an end was written for a session whose start never was");
    // Session 41's own end still counts.
    m.hub.host_ui().event(host_ended(&other, 41, EndKind::PeerBye, false));
    assert_eq!((m.hub.live_sessions(), last_hosted(&m), audited_ends(&m)), (0, Some(0), 1));
    m.finish();
    other.finish();
}

/// Review of 22/11/2026, B8: the same for a resume that commits and ends before it is reported started again: the
/// lost session's first end already counted it down and wrote its line; its card still follows the core.
#[test]
fn a_resume_that_ends_before_it_started_again_counts_nothing_down_and_still_ends_the_card() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, ALL);
    let mut ui = m.client(info(false));
    m.hub.host_ui().event(host_started(&other, 41));
    m.hub.host_ui().event(host_started(&other, 43));
    m.hub.host_ui().event(host_ended(&other, 41, EndKind::Lost, true));
    assert_eq!((m.hub.live_sessions(), audited_ends(&m)), (1, 1));
    // The resume of 41 was committed, then the viewer said Bye before 41 was reported started again.
    m.hub.host_ui().event(host_ended(&other, 41, EndKind::PeerBye, false));
    assert_eq!(m.hub.live_sessions(), 1, "session 43 still runs");
    assert_eq!(last_hosted(&m), Some(1));
    assert_eq!(audited_ends(&m), 1, "the loss already wrote this session's end");
    ui.notify("the lost session's card ends", |n| match n {
        Notify::Session { session_id: 41, state: SessionState::Ended { .. }, .. } => Some(()),
        _ => None,
    });
    assert!(!m.hub.hub.is_lost(41));
    drop(ui);
    m.finish();
    other.finish();
}

// ---------- E2: a swap whose new connection fails ----------

/// A host machine and a viewer machine that paired and opened a session (the viewer asked, the host accepted).
fn swap_duo() -> (Machine, Machine, Client, Client, u64) {
    let host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, ALL);
    viewer.pin(&host, PermissionSet::CONTROL);
    viewer.sees(&host, false);
    host.sees(&viewer, false);
    let mut host_ui = host.client(info(false));
    let mut viewer_ui = viewer.client(info(false));
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let incoming = host_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    let answer =
        UiOp::AnswerIncoming { request_id: incoming.request_id, decision: Decision::Accept, grants: ALL.bits() };
    assert!(host_ui.call(answer).is_ok());
    let id = viewer_ui.connected(req);
    active_as(&mut viewer_ui, Role::Viewer, &[]);
    active_as(&mut host_ui, Role::Host, &[]);
    (host, viewer, host_ui, viewer_ui, id)
}

/// The viewer asks to swap and the host's user accepts: the host is the one that dials the new connection.
fn swap_accepted(host_ui: &mut Client, viewer_ui: &mut Client, session_id: u64) {
    let req = viewer_ui.request(UiOp::RequestSwap { session_id });
    assert!(matches!(viewer_ui.reply(req, 22), Ok(ReplyBody::RequestId(_))));
    let card = host_ui.notify("the swap card", |n| match n {
        Notify::Swap { note: swapdesk_ipc::SwapNote::Asked { request_id, .. }, .. } => Some(*request_id),
        _ => None,
    });
    assert!(host_ui.call(UiOp::AnswerSwap { request_id: card, accept: true }).is_ok());
}

fn not_opened(ui: &mut Client, device_hex: &str, limit: Duration) -> swapdesk_ipc::ConnectWhy {
    ui.take_within(limit, "a connect that did not open", |m| match m {
        Ipc::UiNotify(Notify::Session { device_id, state: SessionState::NotOpened { why }, .. })
            if device_id == device_hex =>
        {
            Some(*why)
        }
        _ => None,
    })
}

/// Review of 22/11/2026, E2: the machine that becomes the viewer dialled the swap's new connection once; when that
/// failed (here: it had no address for the other machine for a moment), the old session was already gone on both
/// sides and the two machines were left with no session.
#[test]
fn a_swap_whose_first_dial_fails_dials_again_within_the_token_life() {
    let (host, viewer, mut host_ui, mut viewer_ui, first) = swap_duo();
    // The host machine loses the viewer machine's record for a moment.
    host.presence.set(Vec::new());
    swap_accepted(&mut host_ui, &mut viewer_ui, first);
    std::thread::sleep(Duration::from_millis(600));
    host.sees(&viewer, false);
    let second = active_as(&mut host_ui, Role::Viewer, &[first]);
    assert_eq!(active_as(&mut viewer_ui, Role::Host, &[first]), second, "one new session, seen from both sides");
    // The token was used: once its life is over, nobody is told that the connection never came.
    let never_came =
        |m: &Ipc| matches!(m, Ipc::UiNotify(Notify::Session { state: SessionState::NotOpened { .. }, .. }));
    assert!(viewer_ui.nothing_within(Duration::from_secs(14), never_came), "a swap that worked was reported failed");
    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}

/// Review of 22/11/2026, E2: when the new connection cannot be made within the token's life, both machines are
/// told, as of a connect that did not open: the one that dialled with why, the other that nothing came.
#[test]
fn a_swap_whose_new_connection_never_opens_tells_both_machines() {
    let (host, viewer, mut host_ui, mut viewer_ui, first) = swap_duo();
    host.presence.set(Vec::new());
    swap_accepted(&mut host_ui, &mut viewer_ui, first);
    let limit = Duration::from_secs(16);
    assert_eq!(not_opened(&mut host_ui, &viewer.hex(), limit), swapdesk_ipc::ConnectWhy::Failed, "no address");
    assert_eq!(not_opened(&mut viewer_ui, &host.hex(), limit), swapdesk_ipc::ConnectWhy::Unreachable);
    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}

/// Review of 22/11/2026, E2 with E8: a swap whose `SwapCancel` crossed the accept leaves the old session running and
/// its token unused; the look after the token's life must not then report a connection that never came, about a
/// device this machine is still in a session with.
#[test]
fn a_token_left_unused_by_a_cancelled_swap_reports_nothing_while_the_session_runs() {
    use crate::service::ExtrasHooks;
    let (host, viewer, host_ui, mut viewer_ui, _first) = swap_duo();
    let hooks = super::super::extras::HubExtras { hub: Arc::downgrade(&viewer.hub.hub) };
    hooks.swap_never_came(host.id());
    let not_opened =
        |m: &Ipc| matches!(m, Ipc::UiNotify(Notify::Session { state: SessionState::NotOpened { .. }, .. }));
    assert!(
        viewer_ui.nothing_within(Duration::from_millis(400), not_opened),
        "a running session was reported never come"
    );
    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}

/// Review of 22/11/2026, B6: the same for a push that got no answer (a lost packet, a peer busy for a moment).
#[test]
fn a_grants_push_that_got_no_answer_is_tried_again() {
    let (a, b) = synced(PermissionSet::CONTROL, PermissionSet::VIEW);
    let wider = PermissionSet::CONTROL.union(PermissionSet::FILES);
    let b_id = b.id();
    a.trust
        .update(|s| s.devices.iter_mut().filter(|d| d.device_id == b_id).for_each(|d| d.grants_here = wider))
        .expect("store");
    let started = tokio::time::Instant::now();
    lock(&a.hub.hub.sync).changed_for_test(b_id, started);
    a.hub.hub.sync_finished(b_id, started, Err(SyncError::Failed));
    wait_there(&b, wider, Duration::from_secs(6), "B hears the change on the next try");
    a.finish();
    b.finish();
}
