//! Flow proofs of the liveness probe (F-FIND-01: a device that stops answering is shown offline within 10 s). A
//! peer that goes silent without closing its connection (asleep, crashed, a dead link) is reported offline by the
//! session's own probe long before the transport's idle timeout would end it: in memory with a paused clock for the
//! exact limits, over real QUIC through a dead link for the measured time.

mod common;

use std::time::Duration;

use common::*;
use swapdesk_proto::*;
use swapdesk_session::{EndKind, HostEvent, Registry, ViewerEvent, ViewerSession, memory_pair};
use tokio::time::Instant;

const CF: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::FILES);

fn offline_events(ui: &TestUi) -> usize {
    ui.events().iter().filter(|e| matches!(e, HostEvent::PeerOffline { .. })).count()
}

#[test]
fn a_silent_viewer_is_reported_offline_at_5_seconds_and_given_up_at_8() {
    run_paused(async {
        let ui = TestUi::accepting(CF);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, CF);
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, CF, None, None).await;
        next_matching(&mut io, "SessionGrant", |m| matches!(m, Message::SessionGrant(_)).then_some(())).await;
        let granted_at = Instant::now();
        // The raw viewer says nothing more and never answers a Ping.
        tokio::time::sleep(Duration::from_millis(4_900)).await;
        assert_eq!(offline_events(&ui), 0, "not yet at 4.9 s");
        eventually(Duration::from_secs(2), "the offline report", || offline_events(&ui) == 1).await;
        let at = granted_at.elapsed();
        assert!(at >= Duration::from_secs(5) && at < Duration::from_millis(5_500), "reported offline after {at:?}");
        assert!(at < Duration::from_secs(10), "F-FIND-01: within 10 s");
        // Given up at 8 s: the connection is lost, and the session may be resumed.
        let end = task.await.unwrap();
        let at = granted_at.elapsed();
        assert_eq!(end.kind, EndKind::Lost);
        assert!(at >= Duration::from_secs(8) && at < Duration::from_millis(8_500), "given up after {at:?}");
        assert!(ui.events().iter().any(|e| matches!(e, HostEvent::Ended { resumable: true, .. })));
    });
}

#[test]
fn a_viewer_that_speaks_again_is_back_online() {
    run_paused(async {
        let ui = TestUi::accepting(CF);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, CF);
        let (mut io, _task) = mem.connect_raw(&who);
        raw_hello(&mut io, CF, None, None).await;
        next_matching(&mut io, "SessionGrant", |m| matches!(m, Message::SessionGrant(_)).then_some(())).await;
        eventually(Duration::from_secs(7), "the offline report", || offline_events(&ui) == 1).await;
        io.out.send(Ping { nonce: 1, t_send_us: 0 }).await.unwrap();
        eventually(Duration::from_secs(1), "back online", || {
            ui.events().iter().any(|e| matches!(e, HostEvent::PeerOnline { .. }))
        })
        .await;
        // And the silence counts from the last message: no second report for another 5 s.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert_eq!(offline_events(&ui), 1);
    });
}

#[test]
fn a_silent_host_is_reported_offline_to_the_viewer_at_5_seconds_and_given_up_at_8() {
    run_paused(async {
        let (mut host, viewer_io) = memory_pair();
        let opening =
            tokio::spawn(
                async move { ViewerSession::open_io(viewer_io, None, Some([9; 16]), viewer_config(CF)).await },
            );
        // The test is the host: it reads the Hello, answers, grants, and then goes quiet.
        next_matching(&mut host, "Hello", |m| matches!(m, Message::Hello(_)).then_some(())).await;
        host.out
            .send(HelloAck {
                proto_minor: 0,
                app_version: "t".into(),
                name: "silent".into(),
                os: Os::Windows,
                os_version: "11".into(),
            })
            .await
            .unwrap();
        host.out.send(capabilities()).await.unwrap();
        host.out
            .send(SessionGrant { session_id: 7, granted: CF, mode: GrantMode::Prompted, resume_token: [3; 16] })
            .await
            .unwrap();
        let mut s = opening.await.unwrap().unwrap();
        let granted_at = Instant::now();
        let e = wait_event(&mut s, Duration::from_secs(10), "the host to be reported offline", |e| {
            matches!(e, ViewerEvent::PeerOffline)
        })
        .await;
        assert_eq!(e, ViewerEvent::PeerOffline);
        let at = granted_at.elapsed();
        assert!(at >= Duration::from_secs(5) && at < Duration::from_millis(5_500), "reported offline after {at:?}");
        let e = wait_event(&mut s, Duration::from_secs(10), "the end", |e| matches!(e, ViewerEvent::Ended(_))).await;
        assert_eq!(
            e,
            ViewerEvent::Ended(swapdesk_session::SessionEnd { kind: EndKind::Lost, code: ErrorCode::NoError })
        );
        let at = granted_at.elapsed();
        assert!(at >= Duration::from_secs(8) && at < Duration::from_millis(8_500), "given up after {at:?}");
    });
}

#[test]
fn over_real_quic_a_silent_peer_is_reported_offline_within_10_seconds() {
    run(async {
        let ui = TestUi::accepting(CF);
        let q = Quic::start(ui.clone(), Registry::new(), Some(swapdesk_netsim::Impairment::default()), |_| {}).await;
        let v = q.viewer(CF, Some(swapdesk_netsim::Impairment::default()));
        let mut s = q.open(&v, viewer_config(CF)).await.unwrap();
        // Let the probe settle: a few pings each way.
        tokio::time::sleep(Duration::from_millis(2_500)).await;

        // The viewer's machine goes silent (asleep, unplugged): its link drops everything, both ways. No close
        // reaches the host; only the probe can tell.
        v.link.as_ref().unwrap().set_link_up(false);
        let silent_since = std::time::Instant::now();
        eventually(Duration::from_secs(10), "the host to report the viewer offline", || offline_events(&ui) >= 1).await;
        let host_saw = silent_since.elapsed();
        wait_event(&mut s, Duration::from_secs(10), "the viewer to report the host offline", |e| {
            matches!(e, ViewerEvent::PeerOffline)
        })
        .await;
        let viewer_saw = silent_since.elapsed();
        eprintln!(
            "F-FIND-01: a silent peer reported offline after {:.2} s by the host, {:.2} s by the viewer",
            host_saw.as_secs_f64(),
            viewer_saw.as_secs_f64()
        );
        assert!(host_saw < Duration::from_secs(10), "the host took {host_saw:?}");
        assert!(viewer_saw < Duration::from_secs(10), "the viewer took {viewer_saw:?}");
        // The transport's idle timeout then ends the session as lost.
        let end = tokio::time::timeout(Duration::from_secs(10), s.ctx().wait_ended()).await.expect("the session ends");
        assert_eq!(end.kind, EndKind::Lost);
        q.stop();
    });
}
