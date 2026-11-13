//! The reconnect of F-NET-01 / F-XNET-01 over real QUIC with the policy that runs in the product: the link between
//! two machines drops for 10 s in the middle of a session, the viewer dials again with its resume token (the first
//! dials while the link is still down, as `reconnect` does), and the session must be back within B5's 3 s of the
//! link's return, the same session, with no new prompt. The harness is the session crate's (`common`).

#[path = "../../swapdesk-session/tests/common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use common::*;
use swapdesk_extras::reconnect::{ReconnectPolicy, reconnect};
use swapdesk_proto::{GrantMode, PermissionSet};
use swapdesk_session::{EndKind, ViewerEvent};

const CF: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::FILES);

#[derive(Clone, Copy, Debug)]
enum Cut {
    /// The viewer's machine loses its network (the Mac's Wi-Fi with the Mac as the viewer).
    Viewer,
    /// The host's machine loses its network (the Mac's Wi-Fi with the Mac as the host).
    Host,
}

/// `drop`: how long the link stays down. The product dials as soon as it sees the loss, so a link that returns while
/// that dial still waits for one of QUIC's retransmissions (after 1 s, 3 s, 7 s) is the case that decides B5.
async fn drop_and_resume(cut: Cut, drop: Duration) {
    let ui = TestUi::accepting(CF);
    let q = Quic::start(
        ui.clone(),
        swapdesk_session::Registry::new(),
        Some(swapdesk_netsim::Impairment::default()),
        |_| {},
    )
    .await;
    let v = q.viewer(CF, Some(swapdesk_netsim::Impairment::default()));
    let mut s = q.open(&v, viewer_config(CF)).await.unwrap();
    let info = s.grant().clone();
    let link = match cut {
        Cut::Viewer => v.link.clone().unwrap(),
        Cut::Host => q.host_link.clone().unwrap(),
    };

    link.set_link_up(false);
    let down_at = Instant::now();
    // The viewer's service starts dialling when its session ends as lost.
    wait_event(
        &mut s,
        Duration::from_secs(10),
        "the viewer to see the loss",
        |e| matches!(e, ViewerEvent::Ended(end) if end.kind == EndKind::Lost),
    )
    .await;
    let lost_after = down_at.elapsed();

    // The link returns DROP after it went down, whatever the dials are doing.
    let up_in = drop.saturating_sub(down_at.elapsed());
    let up_at = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));
    let flipper = {
        let (link, up_at) = (link.clone(), up_at.clone());
        tokio::spawn(async move {
            tokio::time::sleep(up_in).await;
            link.set_link_up(true);
            *up_at.lock().unwrap() = Some(Instant::now());
        })
    };

    let mut attempts = Vec::new();
    let resumed = reconnect(
        ReconnectPolicy::default(),
        info.resume.clone(),
        |resume| {
            let mut cfg = viewer_config(CF);
            cfg.resume = Some(resume);
            let (q, v) = (&q, &v);
            // As the service's redial does: each attempt's handshake is bounded.
            async move {
                swapdesk_session::ViewerSession::connect_within(
                    &v.endpoint,
                    &v.who.identity,
                    q.addr,
                    *q.host_identity.spki(),
                    cfg,
                    ReconnectPolicy::default().dial_limit,
                )
                .await
            }
        },
        |e| attempts.push((down_at.elapsed(), e)),
    )
    .await
    .expect("the session should come back");
    let back = Instant::now();
    let up = up_at.lock().unwrap().expect("the link came back before the session did");
    let took = back.duration_since(up);
    eprintln!(
        "{cut:?} {drop:?}: lost after {lost_after:?}; back {took:?} after the link's return; attempts {attempts:?}"
    );
    flipper.await.unwrap();

    assert_eq!((resumed.grant().mode, resumed.grant().session_id), (GrantMode::Resumed, info.session_id));
    assert_eq!(ui.prompt_count(), 1, "no new prompt");
    assert!(took < Duration::from_secs(3), "{cut:?} {drop:?}: the session was back {took:?} after the link's return");
    q.stop();
}

#[test]
fn a_10_second_drop_of_the_viewers_network_resumes_within_3_seconds_of_its_return() {
    run(drop_and_resume(Cut::Viewer, Duration::from_secs(10)));
}

#[test]
fn a_10_second_drop_of_the_hosts_network_resumes_within_3_seconds_of_its_return() {
    run(drop_and_resume(Cut::Host, Duration::from_secs(10)));
}

/// Found by this test before the dial was bounded: the link came back just after QUIC's retransmission at 10 s of
/// the dial that began at 7 s, and the next came 3.2 s later (the real Wi-Fi of the Mac took 7 s to give an address
/// back after it was switched on, so its return falls anywhere in this range).
#[test]
fn a_drop_that_ends_between_two_retransmissions_of_the_dial_resumes_within_3_seconds_too() {
    run(drop_and_resume(Cut::Viewer, Duration::from_millis(10_100)));
}
