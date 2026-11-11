//! The grants sync connection, the side that answers (`docs/GRANTS-SYNC.md` 8, slice S5).

mod common;

use std::time::Duration;

use common::*;
use swapdesk_proto::*;
use swapdesk_session::{ControlIo, EndKind, HostEvent, SessionEnd};
use tokio::task::JoinHandle;

/// A raw client of `who` that opens with `GrantsSync`.
async fn sync_from(mem: &Mem, who: &Who, grants: PermissionSet) -> (ControlIo, JoinHandle<SessionEnd>) {
    let (io, task) = mem.connect_raw(who);
    io.out.set_peer_minor(PROTO_MINOR);
    io.out.send(GrantsSync { proto_minor: PROTO_MINOR, grants }).await.unwrap();
    (io, task)
}

async fn the_answer(io: &mut ControlIo) -> GrantsSync {
    match next_msg(io, "the answer").await {
        Message::GrantsSync(g) => g,
        other => panic!("not an answer: {other:?}"),
    }
}

fn peer_grants(ui: &TestUi) -> Vec<(swapdesk_trust::DeviceId, PermissionSet)> {
    ui.events()
        .iter()
        .filter_map(|e| match e {
            HostEvent::PeerGrants { device, grants } => Some((*device, *grants)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_sync_is_answered_with_the_stored_grants_for_that_device_only() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui.clone(), swapdesk_session::Registry::new());
        let (a, b) = (Who::new(), Who::new());
        pin(&mem.trust, &a, PermissionSet::VIEW);
        pin(&mem.trust, &b, PermissionSet::CONTROL | PermissionSet::FILES);
        let (mut io, task) = sync_from(&mem, &a, PermissionSet::CONTROL).await;
        let answer = the_answer(&mut io).await;
        assert_eq!(answer, GrantsSync { proto_minor: PROTO_MINOR, grants: PermissionSet::VIEW }, "a's own, not b's");
        drop(io);
        let end = task.await.unwrap();
        assert_eq!(end.kind, EndKind::PeerClosed);
        assert_eq!(peer_grants(&ui), vec![(a.id, PermissionSet::CONTROL)], "what a allows here, about a");
        assert!(
            ui.events().iter().any(
                |e| matches!(e, HostEvent::PeerMinor { device, minor } if *device == a.id && *minor == PROTO_MINOR)
            )
        );
    });
}

#[test]
fn a_sync_never_prompts_never_counts_as_a_session_and_writes_no_audit() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(swapdesk_session::PromptAnswer::Decline));
        let mem = Mem::new(ui.clone(), swapdesk_session::Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut io, task) = sync_from(&mem, &who, PermissionSet::VIEW).await;
        the_answer(&mut io).await;
        assert!(mem.host.sessions().is_empty(), "no live session while it runs");
        drop(io);
        task.await.unwrap();
        assert_eq!(ui.prompt_count(), 0, "nobody was asked");
        let session_events = ui
            .events()
            .into_iter()
            .filter(|e| !matches!(e, HostEvent::PeerGrants { .. } | HostEvent::PeerMinor { .. }))
            .collect::<Vec<_>>();
        assert!(
            session_events.is_empty(),
            "no SessionStarted, Ended or anything the audit records: {session_events:?}"
        );
    });
}

#[test]
fn a_sync_while_another_session_is_live_is_answered_not_busy() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::with(ui.clone(), swapdesk_session::Registry::new(), |c| c.max_live_sessions = 1);
        let (a, b) = (Who::new(), Who::new());
        pin(&mem.trust, &a, PermissionSet::VIEW);
        pin(&mem.trust, &b, PermissionSet::CONTROL);
        let (s, _task) = mem.open(&b, viewer_config(PermissionSet::CONTROL)).await;
        let _s = s.expect("b's session is live");
        assert_eq!(mem.host.sessions().len(), 1);
        let (mut io, _sync) = sync_from(&mem, &a, PermissionSet::EMPTY).await;
        assert_eq!(the_answer(&mut io).await.grants, PermissionSet::VIEW, "answered while the host is full");
    });
}

#[test]
fn a_second_message_a_stream_or_a_datagram_on_a_sync_connection_is_a_violation() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), swapdesk_session::Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        // A second message.
        let (mut io, task) = sync_from(&mem, &who, PermissionSet::VIEW).await;
        the_answer(&mut io).await;
        io.out.send(GrantsSync { proto_minor: PROTO_MINOR, grants: PermissionSet::VIEW }).await.unwrap();
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::Violation, ErrorCode::ProtocolViolation));
        // A sync that says minor 0: no such build can send one.
        let (io, task) = mem.connect_raw(&who);
        io.out.set_peer_minor(PROTO_MINOR);
        io.out.send(GrantsSync { proto_minor: 0, grants: PermissionSet::VIEW }).await.unwrap();
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::Violation, ErrorCode::ProtocolViolation));
        // A client that never closes is closed by the host after its linger.
        let (mut io, task) = sync_from(&mem, &who, PermissionSet::VIEW).await;
        the_answer(&mut io).await;
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::LocalBye, ErrorCode::NoError));
        drop(io);
    });
}

/// One sync of `who`: the first thing the host sent back, and how the connection ended.
async fn once(mem: &Mem, who: &Who) -> (Option<swapdesk_session::Inbound>, SessionEnd) {
    let (mut io, task) = sync_from(mem, who, PermissionSet::VIEW).await;
    let first = tokio::time::timeout(Duration::from_secs(1), io.rx.recv()).await.ok().flatten();
    drop(io);
    (first, task.await.unwrap())
}

#[test]
fn a_sync_flood_from_one_device_is_closed_busy_past_its_budget() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui.clone(), swapdesk_session::Registry::new());
        let (who, other) = (Who::new(), Who::new());
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        pin(&mem.trust, &other, PermissionSet::VIEW);
        // The burst of three is answered.
        for n in 0..3 {
            let (first, _) = once(&mem, &who).await;
            assert!(matches!(first, Some(Ok(Message::GrantsSync(_)))), "sync {n} is answered");
        }
        // The fourth is closed BUSY, unanswered and unapplied.
        let before = peer_grants(&ui).len();
        let (first, end) = once(&mem, &who).await;
        assert!(!matches!(first, Some(Ok(Message::GrantsSync(_)))), "not answered: {first:?}");
        assert_eq!((end.kind, end.code), (EndKind::Refused, ErrorCode::Busy));
        assert_eq!(peer_grants(&ui).len(), before, "not applied");
        // Another device has its own budget.
        let (first, _) = once(&mem, &other).await;
        assert!(matches!(first, Some(Ok(Message::GrantsSync(_)))));
        // Six a minute come back: after ten seconds there is one again.
        tokio::time::sleep(Duration::from_secs(11)).await;
        let (first, _) = once(&mem, &who).await;
        assert!(matches!(first, Some(Ok(Message::GrantsSync(_)))), "earned back");
    });
}

/// Over QUIC: a stream the client opens after its sync, or a datagram, is a violation too (the memory link above has
/// neither).
#[test]
fn over_quic_a_stream_or_a_datagram_after_the_sync_is_a_violation() {
    run(async {
        for extra in ["uni", "datagram"] {
            let q =
                Quic::start(TestUi::accepting(PermissionSet::CONTROL), swapdesk_session::Registry::new(), None, |_| {})
                    .await;
            let v = q.viewer(PermissionSet::VIEW, None);
            let conn = swapdesk_trust::connect_session(&v.endpoint, &v.who.identity, q.addr, *q.host_identity.spki())
                .await
                .expect("the pinned handshake");
            let (send, recv) = conn.open_bi().await.unwrap();
            let mut io = ControlIo::from_quinn(send, recv);
            io.out.set_peer_minor(PROTO_MINOR);
            io.out.send(GrantsSync { proto_minor: PROTO_MINOR, grants: PermissionSet::EMPTY }).await.unwrap();
            assert_eq!(the_answer(&mut io).await.grants, PermissionSet::VIEW);
            if extra == "uni" {
                let mut s = conn.open_uni().await.unwrap();
                s.write_all(&[0]).await.unwrap();
            } else {
                conn.send_datagram(vec![0u8].into()).unwrap();
            }
            let reason = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await.expect("closed by the host");
            match reason {
                quinn::ConnectionError::ApplicationClosed(c) => {
                    assert_eq!(c.error_code.into_inner(), u64::from(ErrorCode::ProtocolViolation.tag()), "{extra}");
                }
                other => panic!("{extra}: {other:?}"),
            }
            q.stop();
        }
    });
}

/// GRANTS-SYNC.md 8 S9 (ACCOUNTS.md 5.6): a sync takes no host reservation. Another account's core hosts a session
/// on the same machine (the reservation is held); a sync to this core is still answered.
#[test]
fn a_sync_takes_no_host_reservation() {
    run_paused(async {
        let hosting = swapdesk_session::MachineHosting::new();
        let ui_b = TestUi::accepting(PermissionSet::CONTROL);
        let core_b = Mem::with(ui_b, swapdesk_session::Registry::new(), |c| c.machine = Some(hosting.clone()));
        let ui_a = TestUi::accepting(PermissionSet::CONTROL);
        let core_a = Mem::with(ui_a, swapdesk_session::Registry::new(), |c| c.machine = Some(hosting.clone()));
        let (a, b) = (Who::new(), Who::new());
        pin(&core_a.trust, &a, PermissionSet::VIEW);
        pin(&core_b.trust, &b, PermissionSet::CONTROL);
        let (s, _task) = core_b.open(&b, viewer_config(PermissionSet::CONTROL)).await;
        let _s = s.expect("the other account's session is live");
        let (mut io, _sync) = sync_from(&core_a, &a, PermissionSet::EMPTY).await;
        assert_eq!(the_answer(&mut io).await.grants, PermissionSet::VIEW, "answered while another core hosts");
        assert_eq!(core_b.host.sessions().len(), 1, "the other session goes on");
    });
}
