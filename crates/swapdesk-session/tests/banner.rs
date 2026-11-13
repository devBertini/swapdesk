//! F-SESS-03, "banner during every session" (security review finding F3): a host whose banner window is not
//! connected lets nobody in without a prompt and pauses and then ends the sessions it already has. The launcher and
//! the window's presence are the test UI's fakes; the paused clock makes the bound exact.

mod common;

use std::time::Duration;

use common::*;
use swapdesk_proto::*;
use swapdesk_session::{EndKind, HostEvent, PromptAnswer, Registry, ViewerEvent};
use tokio::time::Instant;

const UNATTENDED_CONTROL: PermissionSet = PermissionSet::UNATTENDED.union(PermissionSet::CONTROL);

fn started(ui: &TestUi) -> bool {
    ui.events().iter().any(|e| matches!(e, HostEvent::SessionStarted { .. }))
}

#[test]
fn an_unattended_entry_with_no_window_starts_it_and_delivers_nothing_until_it_connects() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        ui.set_banner_window(false);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, UNATTENDED_CONTROL);
        let mut open = Box::pin(mem.open(&who, viewer_config(UNATTENDED_CONTROL)));
        // Seconds pass with the window missing: the launcher ran once, and no session exists, so no picture and no
        // input can reach anyone.
        assert!(
            tokio::time::timeout(Duration::from_secs(5), &mut open).await.is_err(),
            "the entry went in with no window to show the banner"
        );
        assert_eq!(ui.banner_launches(), 1, "the window is asked for once");
        assert!(mem.host.sessions().is_empty(), "nothing is live");
        assert!(!started(&ui), "no session started");
        assert_eq!(ui.prompt_count(), 0, "no prompt either: the entry is unattended");
        // The window connects: the entry goes through as it does today.
        ui.set_banner_window(true);
        let (s, _task) = open.await;
        let s = s.unwrap();
        assert_eq!(s.grant().mode, GrantMode::Unattended);
        assert_eq!(ui.banner_launches(), 1, "no second launch");
        assert!(started(&ui));
    });
}

#[test]
fn an_unattended_entry_with_the_window_connected_goes_as_before_and_launches_nothing() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, UNATTENDED_CONTROL);
        let t0 = Instant::now();
        let (s, _task) = mem.open(&who, viewer_config(UNATTENDED_CONTROL)).await;
        let s = s.unwrap();
        assert_eq!(s.grant().mode, GrantMode::Unattended);
        assert_eq!(ui.banner_launches(), 0);
        assert!(t0.elapsed() < Duration::from_secs(1), "no waiting: {:?}", t0.elapsed());
    });
}

#[test]
fn an_unattended_entry_whose_window_never_connects_is_refused_after_the_bound() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        ui.set_banner_window(false);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, UNATTENDED_CONTROL);
        let t0 = Instant::now();
        let (r, _task) = mem.open(&who, viewer_config(UNATTENDED_CONTROL)).await;
        let waited = t0.elapsed();
        assert_refused(r, ErrorCode::Timeout);
        assert!(
            waited >= Duration::from_secs(15) && waited < Duration::from_secs(16),
            "refused at the 15 s bound, not {waited:?}"
        );
        assert!(ui.banner_launches() >= 1);
        assert!(mem.host.sessions().is_empty());
        assert!(!started(&ui));
        // The device is not locked out: the next try with the window there goes in.
        ui.set_banner_window(true);
        let (s, _task) = mem.open(&who, viewer_config(UNATTENDED_CONTROL)).await;
        assert!(s.is_ok());
    });
}

#[test]
fn quitting_the_window_mid_session_pauses_it_and_it_resumes_when_the_window_is_back() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui.clone(), registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, UNATTENDED_CONTROL);
        let (s, _task) = mem.open(&who, viewer_config(UNATTENDED_CONTROL)).await;
        let mut s = s.unwrap();
        let host_ctx = lock(&rec.sessions)[0].clone();
        assert!(host_ctx.can_inject());

        // The user quits Swapdesk from the tray.
        ui.set_banner_window(false);
        wait_event(&mut s, Duration::from_secs(2), "the pause", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Default, paused: true })
        })
        .await;
        assert!(host_ctx.is_paused() && !host_ctx.can_inject(), "input is dropped without a banner");
        assert_eq!(lock(&rec.states).last().copied(), Some((Desktop::Default, true)), "the handlers stop the picture");
        assert_eq!(ui.banner_launches(), 1, "the window is asked for");
        // A desktop change does not lift the pause.
        mem.host.set_desktop(Desktop::Locked);
        mem.host.set_desktop(Desktop::Default);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(host_ctx.is_paused(), "still paused");

        // The window is back within the bound.
        ui.set_banner_window(true);
        wait_event(&mut s, Duration::from_secs(2), "the resume", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Default, paused: false })
        })
        .await;
        assert!(host_ctx.can_inject());
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(s.ctx().ended().is_none(), "the session went on");
    });
}

#[test]
fn a_window_that_stays_away_ends_the_session_after_the_bound() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, UNATTENDED_CONTROL);
        let (s, task) = mem.open(&who, viewer_config(UNATTENDED_CONTROL)).await;
        let s = s.unwrap();
        let t0 = Instant::now();
        ui.set_banner_window(false);
        let end = task.await.unwrap();
        let waited = t0.elapsed();
        assert_eq!((end.kind, end.code), (EndKind::LocalBye, ErrorCode::Timeout));
        assert!(
            waited >= Duration::from_secs(15) && waited < Duration::from_secs(16),
            "ended at the bound: {waited:?}"
        );
        assert!(mem.host.sessions().is_empty());
        eventually(Duration::from_secs(2), "the viewer to hear it", || s.ctx().ended().is_some()).await;
        assert_eq!(s.ctx().ended().map(|e| e.code), Some(ErrorCode::Timeout));
        // The device is not locked out: with the window back, the next entry goes in.
        ui.set_banner_window(true);
        let (again, _task) = mem.open(&who, viewer_config(UNATTENDED_CONTROL)).await;
        assert!(again.is_ok());
    });
}

#[test]
fn a_prompted_session_whose_window_goes_away_is_paused_too() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui.clone(), registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (s, _task) = mem.open(&who, viewer_config(PermissionSet::CONTROL)).await;
        let mut s = s.unwrap();
        ui.set_banner_window(false);
        wait_event(&mut s, Duration::from_secs(2), "the pause", |e| {
            matches!(e, ViewerEvent::HostState { paused: true, .. })
        })
        .await;
        let host_ctx = lock(&rec.sessions)[0].clone();
        assert!(!host_ctx.can_inject());
    });
}
