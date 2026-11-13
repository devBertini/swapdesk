//! Flow proofs of the resume rules (PROTOCOL.md 6.4, F-NET-01, F-PERM-02, F-SESS-03): a token works only after the
//! connection was lost, within 60 s of the loss, once, for the same device, and dies with a `Bye`, "Desconectar", a
//! grant change, Forget and lock-on-end; the resumed grants are the original ones intersected with what is still
//! stored. In-memory links with a paused clock for the rules and their timings; one run over real QUIC through a dead
//! link for the loss itself (and B5's reconnect time).

mod common;

use std::time::Duration;

use common::*;
use swapdesk_proto::*;
use swapdesk_session::{
    EndKind, GrantInfo, HostEvent, OpenError, PromptAnswer, Registry, ViewerConfig, ViewerEvent, ViewerSession,
};
use tokio::time::Instant;

const CF: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::FILES);

struct Fx {
    mem: Mem,
    ui: std::sync::Arc<TestUi>,
    who: Who,
}

fn fx(picked: PermissionSet, stored: PermissionSet) -> Fx {
    let ui = TestUi::accepting(picked);
    let mem = Mem::new(ui.clone(), Registry::new());
    let who = Who::new();
    pin(&mem.trust, &who, stored);
    Fx { mem, ui, who }
}

impl Fx {
    async fn session(&self, requested: PermissionSet) -> ViewerSession {
        self.mem.open(&self.who, viewer_config(requested)).await.0.expect("the session should open")
    }

    /// Opens with a resume request for `info`.
    async fn resume(&self, who: &Who, info: &GrantInfo, requested: PermissionSet) -> Result<ViewerSession, OpenError> {
        let mut cfg: ViewerConfig = viewer_config(requested);
        cfg.resume = Some(info.resume.clone());
        self.mem.open(who, cfg).await.0
    }

    /// The viewer vanishes; the host sees a lost connection and keeps the session waiting for a resume.
    async fn lose(&self, s: ViewerSession) {
        s.abandon();
        eventually(Duration::from_secs(1), "the host to see the loss", || self.mem.host.sessions().is_empty()).await;
    }

    /// After this, a request that is not resumed gets declined, so "not resumed" shows as a refusal.
    fn decline_from_now(&self) {
        *lock(&self.ui.script) = Script::Answer(PromptAnswer::Decline);
    }
}

fn assert_not_resumed(r: Result<ViewerSession, OpenError>) {
    match r {
        Err(OpenError::Refused(ErrorCode::Declined)) => {}
        Ok(s) => panic!("the token was accepted: {:?}", s.grant().mode),
        Err(e) => panic!("expected the request to fall through to the prompt (declined), got {e:?}"),
    }
}

#[test]
fn after_a_loss_the_same_device_resumes_without_a_prompt_and_gets_a_new_token() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        assert_eq!(f.ui.prompt_count(), 1);
        f.lose(s).await;
        let resumed = f.resume(&f.who, &info, CF).await.unwrap();
        assert_eq!(resumed.grant().mode, GrantMode::Resumed);
        assert_eq!(resumed.grant().session_id, info.session_id, "the same session");
        assert_eq!(resumed.grant().granted, CF);
        assert_ne!(resumed.grant().resume.token, info.resume.token, "a fresh token");
        assert_eq!(f.ui.prompt_count(), 1, "no new prompt");
        assert!(
            f.ui.events().iter().any(|e| matches!(e, HostEvent::Ended { resumable: true, .. })),
            "the host recorded a resumable end"
        );
    });
}

#[test]
fn the_hosts_user_ending_a_lost_session_kills_its_token() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        assert_eq!(f.mem.host.resumable_sessions(), 1, "the lost session waits for a resume");
        f.mem.host.drop_resume(info.session_id);
        assert_eq!(f.mem.host.resumable_sessions(), 0, "the token died with the host's user's end");
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);
    });
}

const CU: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::UNATTENDED);

/// F1 of the review of 13/11: the host's user ends a lost session of a device that holds `UNATTENDED`. The viewer
/// redials with the same request and the old token for 60 s; the token must be refused for good (DECLINED, which the
/// reconnect loop reads as final), not treated as unknown and let through `UNATTENDED` into a fresh session.
#[test]
fn a_killed_token_of_an_unattended_device_is_declined_not_let_through_unattended() {
    run_paused(async {
        let f = fx(PermissionSet::EMPTY, CU);
        let s = f.session(CU).await;
        assert_eq!(s.grant().mode, GrantMode::Unattended);
        let info = s.grant().clone();
        f.lose(s).await;
        assert_eq!(f.mem.host.resumable_sessions(), 1, "the lost session waits for a resume");
        f.mem.host.drop_resume(info.session_id);
        assert_eq!(f.mem.host.resumable_sessions(), 0, "a killed token is no resumable session");
        // The viewer redials every 0.5 s for the window: every try is declined and none starts a session.
        let t0 = Instant::now();
        for _ in 0..10 {
            assert_eq!(
                f.resume(&f.who, &info, CU).await.err(),
                Some(OpenError::Refused(ErrorCode::Declined)),
                "a redial with the killed token"
            );
            assert!(f.mem.host.sessions().is_empty(), "no session started, so no banner");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert_eq!(f.ui.prompt_count(), 0);
        assert!(t0.elapsed() < Duration::from_secs(60));
        // Past the window the entry is gone: an unknown token goes the normal way again.
        tokio::time::sleep(Duration::from_secs(56)).await;
        assert_eq!(f.mem.host.resumable_sessions(), 0);
        let r = f.resume(&f.who, &info, CU).await;
        assert!(r.is_ok(), "past the window the token is unknown, and an unattended device enters on its own");
    });
}

/// The same for a device without `UNATTENDED`: no new prompt card in front of the person who just ended it.
#[test]
fn a_killed_token_does_not_raise_a_new_prompt() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        f.mem.host.drop_resume(info.session_id);
        assert_eq!(f.ui.prompt_count(), 1);
        for _ in 0..4 {
            assert_eq!(f.resume(&f.who, &info, CF).await.err(), Some(OpenError::Refused(ErrorCode::Declined)));
        }
        assert_eq!(f.ui.prompt_count(), 1, "the person was asked once, at the first entry");
        // Another device's guess at the token learns nothing: it falls through as any wrong token does.
        let other = Who::new();
        pin(&f.mem.trust, &other, CF);
        f.decline_from_now();
        assert_not_resumed(f.resume(&other, &info, CF).await);
    });
}

/// The race of F1: the hub saw the session lost, but the viewer resumed before `drop_resume` ran. The session is
/// live again, so the host's user's "Desconectar" ends it, under the lock that found it live.
#[test]
fn drop_resume_on_a_session_that_is_live_again_ends_it() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        let mut resumed = f.resume(&f.who, &info, CF).await.unwrap();
        assert_eq!(resumed.grant().mode, GrantMode::Resumed);
        f.mem.host.drop_resume(info.session_id);
        let e =
            wait_event(&mut resumed, Duration::from_secs(1), "the end", |e| matches!(e, ViewerEvent::Ended(_))).await;
        assert_eq!(
            e,
            ViewerEvent::Ended(swapdesk_session::SessionEnd { kind: EndKind::PeerBye, code: ErrorCode::NoError })
        );
        eventually(Duration::from_secs(1), "the host to end", || f.mem.host.sessions().is_empty()).await;
        assert_eq!(f.mem.host.resumable_sessions(), 0, "no token is left to resume it");
        f.decline_from_now();
        let fresh = resumed.grant().clone();
        assert_not_resumed(f.resume(&f.who, &fresh, CF).await);
    });
}

#[test]
fn a_token_is_single_use() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let first = s.grant().clone();
        f.lose(s).await;
        let s2 = f.resume(&f.who, &first, CF).await.unwrap();
        let second = s2.grant().clone();
        f.lose(s2).await;
        f.decline_from_now();
        // The used token is dead even though the session was lost again.
        assert_not_resumed(f.resume(&f.who, &first, CF).await);
        // The token of the resumed session works, once.
        *lock(&f.ui.script) = Script::Answer(PromptAnswer::Accept(CF));
        let s3 = f.resume(&f.who, &second, CF).await.unwrap();
        assert_eq!(s3.grant().mode, GrantMode::Resumed);
    });
}

#[test]
fn a_token_is_bound_to_its_device_and_a_wrong_try_does_not_burn_it() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        let other = Who::new();
        pin(&f.mem.trust, &other, CF);
        f.decline_from_now();
        assert_not_resumed(f.resume(&other, &info, CF).await);
        // The owner still resumes.
        *lock(&f.ui.script) = Script::Answer(PromptAnswer::Decline);
        let again = f.resume(&f.who, &info, CF).await.unwrap();
        assert_eq!(again.grant().mode, GrantMode::Resumed);
    });
}

#[test]
fn a_token_does_not_work_while_the_old_connection_is_alive_but_waits_for_it_to_be_seen_lost() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.decline_from_now();
        // The old connection answers its pings: it is alive. The resume waits for the loss, then gives up (the
        // wait is 8 s) and the request goes the normal way.
        let t0 = Instant::now();
        let r = f.resume(&f.who, &info, CF).await;
        // Another session (the old one) is still live, so the request is refused BUSY, not resumed.
        assert_eq!(r.err(), Some(OpenError::Refused(ErrorCode::Busy)));
        let waited = t0.elapsed();
        assert!(waited >= Duration::from_secs(8) && waited < Duration::from_millis(8_500), "waited {waited:?}");
        assert!(s.ctx().is_active(), "the live session was left alone");
        // Now the old connection vanishes while a resume waits: the resume is granted as soon as the loss is seen.
        *lock(&f.ui.script) = Script::Answer(PromptAnswer::Decline);
        let vanish = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            s.abandon();
        });
        let t = Instant::now();
        let r = f.resume(&f.who, &info, CF).await;
        let took = t.elapsed();
        vanish.await.unwrap();
        assert_eq!(r.unwrap().grant().mode, GrantMode::Resumed);
        assert!(took >= Duration::from_secs(2) && took < Duration::from_millis(2_600), "resumed {took:?} after asking");
    });
}

#[test]
fn a_bye_kills_the_token() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        let end = s.disconnect().await;
        assert_eq!(end.kind, EndKind::LocalBye);
        eventually(Duration::from_secs(1), "the host to end", || f.mem.host.sessions().is_empty()).await;
        assert_eq!(f.mem.host.resumable_sessions(), 0);
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);
    });
}

#[test]
fn the_hosts_disconnect_kills_the_token() {
    run_paused(async {
        let f = fx(CF, CF);
        let mut s = f.session(CF).await;
        let info = s.grant().clone();
        assert!(f.mem.host.disconnect(info.session_id));
        let e = wait_event(&mut s, Duration::from_secs(1), "the end", |e| matches!(e, ViewerEvent::Ended(_))).await;
        assert_eq!(
            e,
            ViewerEvent::Ended(swapdesk_session::SessionEnd { kind: EndKind::PeerBye, code: ErrorCode::NoError })
        );
        eventually(Duration::from_secs(1), "the host to end", || f.mem.host.sessions().is_empty()).await;
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);
    });
}

#[test]
fn a_grant_change_kills_the_token() {
    run_paused(async {
        // Through the banner...
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        assert!(f.mem.host.set_session_grants(info.session_id, PermissionSet::CONTROL));
        f.lose(s).await;
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);

        // ...and through Permissões (a stored change, even one that leaves the session as it was).
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.mem.host.stored_grants_changed(&f.who.id);
        f.lose(s).await;
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);
    });
}

#[test]
fn forgetting_a_device_ends_its_session_and_kills_its_tokens_even_if_it_is_paired_again() {
    run_paused(async {
        // A live session: it ends with Bye { FORGOTTEN }.
        let f = fx(CF, CF);
        let mut s = f.session(CF).await;
        let live_info = s.grant().clone();
        f.mem.host.forget_device(&f.who.id);
        let e = wait_event(&mut s, Duration::from_secs(1), "the end", |e| matches!(e, ViewerEvent::Ended(_))).await;
        assert_eq!(
            e,
            ViewerEvent::Ended(swapdesk_session::SessionEnd { kind: EndKind::PeerBye, code: ErrorCode::Forgotten })
        );

        // A lost session: the token dies when the device is forgotten, and a new pairing of the same key does not
        // bring it back.
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        f.mem.host.forget_device(&f.who.id);
        f.mem.trust.update(|t| t.forget(&f.who.id)).unwrap();
        pin(&f.mem.trust, &f.who, CF);
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);
        let _ = live_info;
    });
}

#[test]
fn lock_on_end_kills_the_token_and_locks_the_host() {
    run_paused(async {
        let f = fx(CF, CF);
        f.ui.set_lock_on_end(true);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        assert_eq!(
            f.ui.locks.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the host locked its screen when the session ended"
        );
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);
        // With it off, a lost session is resumable and nothing is locked.
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        f.lose(s).await;
        assert_eq!(f.ui.locks.load(std::sync::atomic::Ordering::SeqCst), 0);
    });
}

#[test]
fn a_token_lasts_60_seconds_from_the_loss() {
    run_paused(async {
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        tokio::time::sleep(Duration::from_secs(59)).await;
        let s2 = f.resume(&f.who, &info, CF).await.unwrap();
        assert_eq!(s2.grant().mode, GrantMode::Resumed, "at 59 s");
        let info2 = s2.grant().clone();
        f.lose(s2).await;
        tokio::time::sleep(Duration::from_secs(61)).await;
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info2, CF).await);
        assert_eq!(f.mem.host.resumable_sessions(), 0, "the expired entry is gone");
    });
}

#[test]
fn a_resumed_session_gets_the_original_grants_intersected_with_what_is_still_stored() {
    run_paused(async {
        // FILES was removed in Permissões while the viewer was away.
        let f = fx(CF, CF);
        let s = f.session(CF).await;
        let info = s.grant().clone();
        f.lose(s).await;
        set_stored(&f.mem.trust, &f.who, PermissionSet::CONTROL | PermissionSet::TERMINAL);
        let r = f.resume(&f.who, &info, CF | PermissionSet::TERMINAL).await.unwrap();
        assert_eq!(
            r.grant().granted,
            PermissionSet::CONTROL,
            "FILES is gone; TERMINAL was never in the session, so it is not added"
        );

        // A per-session addition of D8 survives only if it is stored: the host's user widened VIEW to CONTROL|FILES
        // for that session; the stored grant is VIEW.
        let f = fx(CF, PermissionSet::VIEW);
        let s = f.session(CF).await;
        assert_eq!(s.grant().granted, CF);
        let info = s.grant().clone();
        f.lose(s).await;
        let r = f.resume(&f.who, &info, CF).await.unwrap();
        assert_eq!((r.grant().mode, r.grant().granted), (GrantMode::Resumed, PermissionSet::VIEW));
    });
}

#[test]
fn an_unattended_session_resumes_with_unattended_while_it_is_still_stored() {
    run_paused(async {
        let all = PermissionSet::UNATTENDED | PermissionSet::CONTROL;
        // A 1.0 viewer holds the bit from its entry; a viewer of minor 1 entered without it. Each resumes with what
        // it had.
        for (minor, held) in [(0u16, all), (swapdesk_proto::PROTO_MINOR, PermissionSet::CONTROL)] {
            let f = fx(PermissionSet::EMPTY, all);
            let mut cfg = viewer_config(all);
            cfg.proto_minor = minor;
            let s = f.mem.open(&f.who, cfg.clone()).await.0.expect("the session should open");
            assert_eq!((s.grant().mode, s.grant().granted), (GrantMode::Unattended, held), "minor {minor}");
            let info = s.grant().clone();
            f.lose(s).await;
            cfg.resume = Some(info.resume.clone());
            let r = f.mem.open(&f.who, cfg).await.0.unwrap();
            assert_eq!((r.grant().mode, r.grant().granted), (GrantMode::Resumed, held), "minor {minor}");
            assert_eq!(f.ui.prompt_count(), 0);
        }
    });
}

/// A resume of a session that entered without a prompt is still such a session for the host (what D13 asks about),
/// although a viewer of minor 1 holds no `UNATTENDED` bit that would say so.
#[test]
fn a_resumed_unattended_entry_is_still_an_unattended_entry() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::accepting(CF);
        let mem = Mem::new(ui.clone(), registry_one(rec.clone()));
        let all = PermissionSet::UNATTENDED | PermissionSet::CONTROL;
        let (unattended, prompted) = (Who::new(), Who::new());
        pin(&mem.trust, &unattended, all);
        pin(&mem.trust, &prompted, CF);
        let lose = |s: ViewerSession| async {
            s.abandon();
            eventually(Duration::from_secs(1), "the loss", || mem.host.sessions().is_empty()).await;
        };

        let s = mem.open(&unattended, viewer_config(all)).await.0.unwrap();
        assert!(lock(&rec.sessions).last().unwrap().entered_unattended());
        let info = s.grant().clone();
        lose(s).await;
        let mut cfg = viewer_config(all);
        cfg.resume = Some(info.resume.clone());
        let r = mem.open(&unattended, cfg).await.0.unwrap();
        assert_eq!(r.grant().mode, GrantMode::Resumed);
        assert!(lock(&rec.sessions).last().unwrap().entered_unattended(), "the resumed session keeps how it entered");
        r.disconnect().await;
        eventually(Duration::from_secs(1), "the end", || mem.host.sessions().is_empty()).await;

        // A session somebody accepted, and its resume, are not.
        let s = mem.open(&prompted, viewer_config(CF)).await.0.unwrap();
        assert_eq!(s.grant().mode, GrantMode::Prompted);
        assert!(!lock(&rec.sessions).last().unwrap().entered_unattended());
        let info = s.grant().clone();
        lose(s).await;
        let mut cfg = viewer_config(CF);
        cfg.resume = Some(info.resume.clone());
        let r = mem.open(&prompted, cfg).await.0.unwrap();
        assert_eq!(r.grant().mode, GrantMode::Resumed);
        assert!(!lock(&rec.sessions).last().unwrap().entered_unattended());
    });
}

#[test]
fn a_host_reboot_loses_every_token_unattended_devices_reconnect_through_unattended_others_get_a_prompt() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui.clone(), Registry::new());
        let (unattended, plain) = (Who::new(), Who::new());
        let all = PermissionSet::UNATTENDED | PermissionSet::CONTROL;
        pin(&mem.trust, &unattended, all);
        pin(&mem.trust, &plain, CF);
        *lock(&ui.script) = Script::Answer(PromptAnswer::Accept(CF));
        let (a, _t) = mem.open(&unattended, viewer_config(all)).await;
        let a = a.unwrap();
        let info_a = a.grant().clone();
        a.abandon();
        eventually(Duration::from_secs(1), "the loss", || mem.host.sessions().is_empty()).await;
        let (b, _t) = mem.open(&plain, viewer_config(CF)).await;
        let b = b.unwrap();
        let info_b = b.grant().clone();
        b.abandon();
        eventually(Duration::from_secs(1), "the loss", || mem.host.sessions().is_empty()).await;

        // The host reboots: a new service over the same disk. Nothing is in memory.
        let rebooted = swapdesk_session::HostService::new(
            common::host_config(),
            mem.trust.clone(),
            ui.clone(),
            Registry::new(),
            std::sync::Arc::new(swapdesk_fakes::FakeClock::default()),
        );
        let mem2 = Mem { host: rebooted, ui: ui.clone(), trust: mem.trust.clone() };
        let prompts = ui.prompt_count();
        let mut cfg = viewer_config(all);
        cfg.resume = Some(info_a.resume.clone());
        let (r, _t) = mem2.open(&unattended, cfg).await;
        let r = r.unwrap();
        assert_eq!(
            (r.grant().mode, r.grant().granted),
            (GrantMode::Unattended, PermissionSet::CONTROL),
            "through UNATTENDED, not through the stale token (a viewer of minor 1 enters without the bit)"
        );
        assert_eq!(ui.prompt_count(), prompts);
        r.abandon();
        eventually(Duration::from_secs(1), "the loss", || mem2.host.sessions().is_empty()).await;
        // The device without UNATTENDED is asked again.
        *lock(&ui.script) = Script::Answer(PromptAnswer::Decline);
        let mut cfg = viewer_config(CF);
        cfg.resume = Some(info_b.resume.clone());
        let (r, _t) = mem2.open(&plain, cfg).await;
        assert_not_resumed(r);
        assert_eq!(ui.prompt_count(), prompts + 1);
    });
}

#[test]
fn a_host_shutdown_ends_every_session_with_host_shutdown_and_drops_every_token() {
    run_paused(async {
        let f = fx(CF, CF);
        let mut s = f.session(CF).await;
        let info = s.grant().clone();
        f.mem.host.shutdown();
        let e = wait_event(&mut s, Duration::from_secs(1), "the end", |e| matches!(e, ViewerEvent::Ended(_))).await;
        assert_eq!(
            e,
            ViewerEvent::Ended(swapdesk_session::SessionEnd { kind: EndKind::PeerBye, code: ErrorCode::HostShutdown })
        );
        eventually(Duration::from_secs(1), "the host to end", || f.mem.host.sessions().is_empty()).await;
        assert_eq!(f.mem.host.resumable_sessions(), 0);
        f.decline_from_now();
        assert_not_resumed(f.resume(&f.who, &info, CF).await);
    });
}

// ---------- over real QUIC: the loss itself, and B5 ----------

#[test]
fn over_real_quic_a_dead_link_is_a_lost_connection_and_the_viewer_resumes_within_3_seconds_of_the_link_coming_back() {
    run(async {
        let ui = TestUi::accepting(CF);
        let q = Quic::start(ui.clone(), Registry::new(), Some(swapdesk_netsim::Impairment::default()), |_| {}).await;
        let v = q.viewer(CF, Some(swapdesk_netsim::Impairment::default()));
        let mut s = q.open(&v, viewer_config(CF)).await.unwrap();
        let info = s.grant().clone();

        // The network drops for 10 s (B5's scenario): both sides give up when QUIC's idle timeout (6 s) passes.
        v.link.as_ref().unwrap().set_link_up(false);
        wait_event(
            &mut s,
            Duration::from_secs(10),
            "the viewer to see the loss",
            |e| matches!(e, ViewerEvent::Ended(end) if end.kind == EndKind::Lost),
        )
        .await;
        eventually(Duration::from_secs(10), "the host to see the loss", || q.host.sessions().is_empty()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(q.host.resumable_sessions(), 1, "the lost session waits for a resume");

        // The link comes back; the viewer resumes.
        v.link.as_ref().unwrap().set_link_up(true);
        let back = std::time::Instant::now();
        let mut cfg = viewer_config(CF);
        cfg.resume = Some(info.resume.clone());
        let r = q.open(&v, cfg).await.unwrap();
        let took = back.elapsed();
        eprintln!("B5 (session part): link back to a resumed session in {:.1} ms", took.as_secs_f64() * 1000.0);
        assert_eq!(
            (r.grant().mode, r.grant().granted, r.grant().session_id),
            (GrantMode::Resumed, CF, info.session_id)
        );
        assert!(took < Duration::from_secs(3), "resume took {took:?}");
        assert_eq!(ui.prompt_count(), 1, "no new prompt");
        q.stop();
    });
}
