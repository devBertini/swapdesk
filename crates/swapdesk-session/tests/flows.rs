//! Flow proofs of the session: the decision (F-SESS-01, F-SESS-02), permission enforcement (F-PERM-02), the host
//! state pause, the caps, the order of a session (PROTOCOL.md 5.2) and the registry. In-memory links with a paused
//! clock where timing is the point, real QUIC through `swapdesk-trust` where the wire is.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use swapdesk_proto::*;
use swapdesk_session::{
    ChannelHandler, EndKind, HostEvent, OpenError, PromptAnswer, PromptEnd, RegisterError, Registry, StartInfo,
    ViewerEvent, ViewerSession,
};
use tokio::time::Instant;

const P: fn(PermissionSet) -> PromptAnswer = PromptAnswer::Accept;

fn display() -> Display {
    Display {
        id: 0,
        name: "Display 1".into(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        scale_milli: 1000,
        refresh_mhz: 60_000,
        rotation: 0,
        primary: true,
    }
}

fn is_error(e: &ViewerEvent, code: ErrorCode, about_type: u16, about_id: u32) -> bool {
    matches!(e, ViewerEvent::Message(Message::Error(m)) if m.code == code && !m.fatal && m.about_type == about_type && m.about_id == about_id)
}

/// A session granted through a prompt that picks `picked`.
async fn prompted(
    mem: &Mem,
    who: &Who,
    requested: PermissionSet,
) -> (ViewerSession, tokio::task::JoinHandle<SessionEnd>) {
    let (s, task) = mem.open(who, viewer_config(requested)).await;
    (s.expect("the session should open"), task)
}

use swapdesk_session::SessionEnd;

// ---------- F-SESS-02: unattended entry ----------

/// A viewer of 1.0 has no elevation step: it is let in with the whole grant, as 1.0 did.
#[test]
fn a_minor_0_viewer_gets_unattended_at_entry_as_in_1_0() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        let all = PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL;
        pin(&mem.trust, &who, all | PermissionSet::FILES);
        let (s, _task) = mem.open(&who, viewer_config_1_0(all)).await;
        let s = s.unwrap();
        assert_eq!((s.grant().mode, s.grant().granted), (GrantMode::Unattended, all));
        assert_eq!(ui.prompt_count(), 0, "no prompt");
        assert_eq!(s.ctx().peer_minor(), swapdesk_proto::PROTO_MINOR, "the host answers with its own minor");
    });
}

/// A viewer of minor 1 that entered without a prompt holds no `UNATTENDED`: the lock screen, the sign-in screen and
/// UAC pause it, as they pause a prompted session, until it elevates.
#[test]
fn an_unelevated_unattended_session_pauses_at_a_secure_desktop() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), registry_one(rec.clone()));
        let who = Who::new();
        let stored = PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL;
        pin(&mem.trust, &who, stored);
        let (s, _task) = mem.open(&who, viewer_config(stored)).await;
        let mut s = s.unwrap();
        // The plain terminal stays: only the administrator one is machine-level.
        let plain = PermissionSet::CONTROL | PermissionSet::TERMINAL;
        assert_eq!((s.grant().mode, s.grant().granted), (GrantMode::Unattended, plain));
        assert_eq!(ui.prompt_count(), 0, "no prompt");
        let host_ctx = lock(&rec.sessions)[0].clone();
        assert!(host_ctx.entered_unattended(), "the host knows nobody accepted this session");
        assert_eq!(host_ctx.peer_minor(), swapdesk_proto::PROTO_MINOR);
        assert!(host_ctx.can_inject());
        for desktop in [Desktop::Secure, Desktop::Locked, Desktop::LoginWindow] {
            mem.host.set_desktop(desktop);
            wait_event(
                &mut s,
                Duration::from_secs(1),
                "paused",
                |e| matches!(e, ViewerEvent::HostState { desktop: d, paused: true } if *d == desktop),
            )
            .await;
            assert!(host_ctx.is_paused() && !host_ctx.can_inject(), "{desktop:?}: input is dropped");
        }
        mem.host.set_desktop(Desktop::Default);
        wait_event(&mut s, Duration::from_secs(1), "resumed", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Default, paused: false })
        })
        .await;
        assert!(host_ctx.can_inject());
    });
}

#[test]
fn unattended_entry_grants_the_stored_permissions_without_a_prompt() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        *lock(&ui.info) =
            StartInfo { desktop: Desktop::Default, displays: vec![display()], wake_macs: vec![[1, 2, 3, 4, 5, 6]] };
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES);
        let (s, _task) = mem.open(&who, viewer_config(PermissionSet::UNATTENDED | PermissionSet::CONTROL)).await;
        let mut s = s.unwrap();
        assert_eq!(s.grant().mode, GrantMode::Unattended);
        assert_eq!(
            s.grant().granted,
            PermissionSet::CONTROL,
            "stored grants intersected with what was requested, without the machine-level bit: a viewer of minor 1 \
             asks for it with Elevate"
        );
        assert_eq!(ui.prompt_count(), 0, "no prompt");
        assert_eq!(s.grant().host.name, "host-pc");
        assert_ne!(s.grant().resume.token, [0; 16]);
        // 5.2 step 4: HostState, DisplayList, WakeInfo.
        wait_event(&mut s, Duration::from_secs(2), "HostState", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Default, paused: false })
        })
        .await;
        wait_event(
            &mut s,
            Duration::from_secs(2),
            "DisplayList",
            |e| matches!(e, ViewerEvent::Message(Message::DisplayList(d)) if d.displays == vec![display()]),
        )
        .await;
        wait_event(
            &mut s,
            Duration::from_secs(2),
            "WakeInfo",
            |e| matches!(e, ViewerEvent::Message(Message::WakeInfo(w)) if w.macs == vec![[1, 2, 3, 4, 5, 6]]),
        )
        .await;
        // "Entrou sem perguntar" is logged.
        assert!(ui.events().iter().any(|e| matches!(e, HostEvent::SessionStarted { mode: GrantMode::Unattended, .. })));
        // The peer's capabilities arrived on both sides.
        assert!(s.ctx().peer_capabilities().is_some());
    });
}

#[test]
fn a_device_that_holds_unattended_but_did_not_ask_for_it_gets_a_prompt() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::VIEW);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::UNATTENDED | PermissionSet::CONTROL);
        let (s, _task) = prompted(&mem, &who, PermissionSet::CONTROL).await;
        assert_eq!(s.grant().mode, GrantMode::Prompted);
        assert_eq!(ui.prompt_count(), 1);
    });
}

// ---------- F-SESS-01: the prompt ----------

#[test]
fn a_prompted_session_presets_the_stored_grants_and_applies_what_the_host_picked_never_unattended_or_admin_terminal() {
    run_paused(async {
        // The host's user widens: the stored grant is VIEW, the pick adds CONTROL and FILES and (a faulty UI) the two
        // that need a local administrator.
        let picked =
            PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL | PermissionSet::FILES;
        let ui = TestUi::accepting(picked);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut cfg = viewer_config(PermissionSet::CONTROL | PermissionSet::FILES);
        cfg.on_prompt = Some({
            let seen = seen.clone();
            Arc::new(move |secs| lock(&seen).push(secs))
        });
        let (s, _task) = mem.open(&who, cfg).await;
        let mut s = s.unwrap();
        assert_eq!(*lock(&seen), vec![20], "the viewer is told to wait, with the 20 s countdown");
        let request = lock(&ui.prompts)[0].clone();
        assert_eq!(
            (request.stored, request.requested, request.name.as_str(), request.timeout),
            (PermissionSet::VIEW, PermissionSet::CONTROL | PermissionSet::FILES, "viewer-mac", Duration::from_secs(20))
        );
        assert_eq!(s.grant().mode, GrantMode::Prompted);
        let g = s.grant().granted;
        assert!(
            g.contains(PermissionSet::CONTROL) && g.contains(PermissionSet::FILES),
            "widening beyond the stored grants is the host user's right (D8)"
        );
        assert!(
            !g.contains(PermissionSet::UNATTENDED) && !g.contains(PermissionSet::ADMIN_TERMINAL),
            "never UNATTENDED or ADMIN_TERMINAL unless stored"
        );
        wait_event(&mut s, Duration::from_secs(1), "PromptPending", |e| {
            matches!(e, ViewerEvent::PromptPending { timeout_s: 20 })
        })
        .await;
        // The stored grants are unchanged: the pick is for this session only.
        assert_eq!(mem.trust.read(|t| t.find_by_id(&who.id).unwrap().grants_here), PermissionSet::VIEW);
    });
}

#[test]
fn only_let_see_grants_view_and_nothing_else() {
    run_paused(async {
        let rec = Recorder::new(&[0x0501, 0x0102], &[]);
        let ui = TestUi::accepting(PermissionSet::VIEW);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL | PermissionSet::TERMINAL);
        let (mut s, _task) = prompted(&mem, &who, PermissionSet::CONTROL | PermissionSet::TERMINAL).await;
        assert_eq!(s.grant().granted, PermissionSet::VIEW);
        s.ctx().send(TermOpen { term_id: 1, cols: 80, rows: 24, admin: false }).await.unwrap();
        wait_event(&mut s, Duration::from_secs(1), "NOT_PERMITTED", |e| {
            is_error(e, ErrorCode::NotPermitted, 0x0501, 1)
        })
        .await;
        assert_eq!(rec.count(), 0);
    });
}

#[test]
fn a_declined_request_is_refused_with_declined() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (s, task) = mem.open(&who, viewer_config(PermissionSet::CONTROL)).await;
        assert_refused(s, ErrorCode::Declined);
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::Refused, ErrorCode::Declined));
        assert!(ui.events().contains(&HostEvent::Refused { device: who.id, code: ErrorCode::Declined }));
        // An empty pick is a refusal too: nothing would be granted.
        let ui = TestUi::accepting(PermissionSet::EMPTY);
        let mem = Mem::new(ui, Registry::new());
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (s, _task) = mem.open(&who, viewer_config(PermissionSet::CONTROL)).await;
        assert_refused(s, ErrorCode::Declined);
    });
}

#[test]
fn with_nobody_at_the_host_the_request_is_refused_at_once_with_nobody_present() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        ui.set_present(false);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let prompted_hook = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.on_prompt = Some({
            let n = prompted_hook.clone();
            Arc::new(move |_| {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        });
        let t0 = Instant::now();
        let (s, _task) = mem.open(&who, cfg).await;
        assert_refused(s, ErrorCode::NobodyPresent);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "refused at once, not after the prompt timeout: {:?}",
            t0.elapsed()
        );
        assert_eq!(ui.prompt_count(), 0, "no prompt was shown");
        assert_eq!(prompted_hook.load(std::sync::atomic::Ordering::SeqCst), 0, "the viewer was not told to wait");
        // Unattended entry does not need anybody.
        pin_again(&mem, &who, PermissionSet::UNATTENDED | PermissionSet::CONTROL);
        let (s, _task) = mem.open(&who, viewer_config(PermissionSet::UNATTENDED | PermissionSet::CONTROL)).await;
        assert_eq!(s.unwrap().grant().mode, GrantMode::Unattended);
    });
}

fn pin_again(mem: &Mem, who: &Who, grants: PermissionSet) {
    set_stored(&mem.trust, who, grants);
}

#[test]
fn an_unanswered_request_is_declined_after_exactly_20_seconds() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let t0 = Instant::now();
        let mem2 = Mem { host: mem.host.clone(), ui: ui.clone(), trust: mem.trust.clone() };
        let id = who.id;
        let identity = who.identity.clone();
        let opening = tokio::spawn(async move {
            let who = Who { identity, id };
            let (s, _task) = mem2.open(&who, viewer_config(PermissionSet::CONTROL)).await;
            (s.err(), Instant::now())
        });
        tokio::time::sleep(Duration::from_millis(19_900)).await;
        assert!(!opening.is_finished(), "still waiting for the user at 19.9 s");
        assert_eq!(ui.prompt_count(), 1);
        let (err, at) = opening.await.unwrap();
        assert_eq!(err, Some(OpenError::Refused(ErrorCode::Timeout)));
        let took = at - t0;
        assert!(took >= Duration::from_secs(20) && took < Duration::from_millis(20_100), "declined at {took:?}");
        // The card is closed, as timed out.
        assert_eq!(lock(&ui.closed).iter().map(|c| c.1).collect::<Vec<_>>(), vec![PromptEnd::TimedOut]);
    });
}

#[test]
fn a_request_that_the_viewer_withdraws_closes_the_card() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::CONTROL, None, None).await;
        next_matching(&mut io, "PromptPending", |m| matches!(m, Message::PromptPending(_)).then_some(())).await;
        io.out.send(Bye { code: ErrorCode::NoError }).await.unwrap();
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::PeerBye, ErrorCode::NoError));
        assert_eq!(lock(&ui.closed).iter().map(|c| c.1).collect::<Vec<_>>(), vec![PromptEnd::Cancelled]);
        // Answering late changes nothing.
        ui.answer_held(P(PermissionSet::CONTROL));
    });
}

#[test]
fn a_viewer_dropped_while_prompted_says_bye_and_the_host_prompt_ends_cancelled() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (host_io, viewer_io) = swapdesk_session::memory_pair();
        let (host, id) = (mem.host.clone(), who.id);
        let task = tokio::spawn(async move { host.serve_io(host_io, None, id).await });
        // The open waits for the user at the host; its caller gives up after 5 s and drops it.
        let open = ViewerSession::open_io(viewer_io, None, Some([9; 16]), viewer_config(PermissionSet::CONTROL));
        let waited = tokio::time::timeout(Duration::from_secs(5), open).await;
        assert!(waited.is_err(), "the host was still asking");
        assert_eq!(ui.prompt_count(), 1);
        // The drop said `Bye { NO_ERROR }`: the host ends as for a viewer that left, long before its 20 s.
        let t0 = Instant::now();
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::PeerBye, ErrorCode::NoError));
        assert!(t0.elapsed() < Duration::from_secs(1), "ended at once, not at the prompt's deadline");
        assert_eq!(lock(&ui.closed).iter().map(|c| c.1).collect::<Vec<_>>(), vec![PromptEnd::Cancelled]);
        // Answering late changes nothing.
        ui.answer_held(P(PermissionSet::CONTROL));
    });
}

#[test]
fn a_second_session_while_one_is_live_is_refused_busy_without_a_prompt() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui.clone(), Registry::new());
        let (a, b) = (Who::new(), Who::new());
        pin(&mem.trust, &a, PermissionSet::CONTROL);
        pin(&mem.trust, &b, PermissionSet::CONTROL);
        let (first, _t1) = prompted(&mem, &a, PermissionSet::CONTROL).await;
        assert_eq!(ui.prompt_count(), 1);
        let (second, _t2) = mem.open(&b, viewer_config(PermissionSet::CONTROL)).await;
        assert_refused(second, ErrorCode::Busy);
        assert_eq!(ui.prompt_count(), 1, "the user was not asked about a session that cannot start");
        drop(first);
    });
}

#[test]
fn a_device_that_is_not_stored_is_refused_unknown_device() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), Registry::new());
        let stranger = Who::new();
        let (s, _task) = mem.open(&stranger, viewer_config(PermissionSet::CONTROL)).await;
        assert_refused(s, ErrorCode::UnknownDevice);
    });
}

#[test]
fn a_peer_that_never_sends_hello_is_dropped_after_the_handshake_timeout() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (_io, task) = mem.connect_raw(&who);
        let t0 = Instant::now();
        let end = task.await.unwrap();
        assert_eq!(end.kind, EndKind::Timeout);
        let took = t0.elapsed();
        assert!(
            took >= Duration::from_secs(10) && took < Duration::from_millis(10_600),
            "handshake timeout at {took:?}"
        );
    });
}

// ---------- swap tokens (6.3 step 4 and 5) ----------

#[test]
fn an_armed_swap_token_grants_the_stored_grants_without_a_prompt_and_never_unattended_entry() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui.clone(), Registry::new());
        let (a, other) = (Who::new(), Who::new());
        pin(&mem.trust, &a, PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES);
        pin(&mem.trust, &other, PermissionSet::CONTROL);
        let token = swapdesk_session::random_token();
        mem.host.arm_swap_token(a.id, token);

        // Another device cannot use it (it is bound to the device ID it was armed for).
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.swap_token = Some(token);
        let (s, _t) = mem.open(&other, cfg).await;
        assert_refused(s, ErrorCode::Declined);

        // The device it was armed for enters without a prompt, with the stored grants minus UNATTENDED.
        let before = ui.prompt_count();
        let mut cfg = viewer_config(PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES);
        cfg.swap_token = Some(token);
        let (s, _t) = mem.open(&a, cfg.clone()).await;
        let s = s.unwrap();
        assert_eq!(s.grant().mode, GrantMode::Swap);
        assert_eq!(s.grant().granted, PermissionSet::CONTROL | PermissionSet::FILES);
        assert!(!s.grant().granted.contains(PermissionSet::UNATTENDED));
        assert_eq!(ui.prompt_count(), before, "no prompt");
        s.disconnect().await;

        // Single use: the same token again goes the normal way.
        let (s, _t) = mem.open(&a, cfg).await;
        assert!(s.is_err() || s.as_ref().is_ok_and(|s| s.grant().mode != GrantMode::Swap));
    });
}

#[test]
fn a_swap_token_expires_after_10_seconds() {
    run_paused(async {
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui, Registry::new());
        let a = Who::new();
        pin(&mem.trust, &a, PermissionSet::CONTROL);
        let token = swapdesk_session::random_token();
        mem.host.arm_swap_token(a.id, token);
        tokio::time::sleep(Duration::from_millis(10_100)).await;
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.swap_token = Some(token);
        let (s, _t) = mem.open(&a, cfg).await;
        assert_refused(s, ErrorCode::Declined);
    });
}

// ---------- F-PERM-02: enforcement ----------

#[test]
fn a_request_outside_the_grants_gets_not_permitted_and_nothing_is_done() {
    run_paused(async {
        // The handler owns every type below: it would act on them if the session let them through.
        let rec = Recorder::new(
            &[0x0501, 0x0503, 0x0504, 0x040B, 0x0409, 0x0401, 0x0601, 0x0109, 0x0107, 0x0303, 0x0102],
            &[],
        );
        let ui = TestUi::accepting(PermissionSet::VIEW);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (mut s, _task) = prompted(&mem, &who, PermissionSet::VIEW).await;
        let ctx = s.ctx().clone();
        ctx.send(TermOpen { term_id: 1, cols: 80, rows: 24, admin: false }).await.unwrap();
        ctx.send(TermOpen { term_id: 2, cols: 80, rows: 24, admin: true }).await.unwrap();
        ctx.send(TermResize { term_id: 1, cols: 100, rows: 30 }).await.unwrap();
        ctx.send(FileGet { transfer_id: 3, path: "C:/secret".into() }).await.unwrap();
        ctx.send(DirRequest { req_id: 4, path: None }).await.unwrap();
        ctx.send(FileOffer { transfer_id: 5, name: "a.txt".into(), size: 1, mtime_ms: 0, dest: None }).await.unwrap();
        ctx.send(QuickAction { action_id: 6, kind: Action::Restart }).await.unwrap();
        ctx.send(SendSpecial { key: Special::SecureAttention }).await.unwrap();
        ctx.send(AudioControl { enabled: true }).await.unwrap();
        ctx.send(ClipboardSync { enabled: true }).await.unwrap();
        let wanted = [
            (0x0501, 1),
            (0x0501, 2),
            (0x0503, 1),
            (0x040B, 3),
            (0x0409, 4),
            (0x0401, 5),
            (0x0601, 6),
            (0x0109, 0),
            (0x0107, 0),
            (0x0303, 0),
        ];
        for (ty, id) in wanted {
            wait_event(&mut s, Duration::from_secs(2), "NOT_PERMITTED", |e| {
                is_error(e, ErrorCode::NotPermitted, ty, id)
            })
            .await;
        }
        assert_eq!(rec.count(), 0, "the handler saw nothing: nothing was done");
        // What is granted still works, and so does turning audio off.
        ctx.send(SelectDisplay { display: 0 }).await.unwrap();
        ctx.send(AudioControl { enabled: false }).await.unwrap();
        eventually(Duration::from_secs(2), "the permitted messages", || rec.count() == 2).await;
        assert!(s.ctx().is_active(), "a refusal does not end the session");
    });
}

#[test]
fn a_stream_outside_the_grants_is_stopped_with_not_permitted_over_the_wire() {
    run(async {
        let rec = Recorder::new(&[], &[StreamKind::Input, StreamKind::Clipboard, StreamKind::FileData]);
        let ui = TestUi::accepting(PermissionSet::VIEW | PermissionSet::CLIPBOARD);
        let q = Quic::start(ui, registry_one(rec.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::VIEW, None);
        let s = q.open(&v, viewer_config(PermissionSet::VIEW | PermissionSet::CLIPBOARD)).await.unwrap();
        let conn = s.ctx().connection().unwrap().clone();

        // Input needs CONTROL, which was not granted: the host stops the stream and tells the viewer.
        let mut input = conn.open_uni().await.unwrap();
        // One write per stream: the host stops a stream it refuses as soon as it has read its kind, and a second write
        // issued after that would fail with `Stopped`.
        let mut bytes = StreamKind::Input.encode().unwrap();
        bytes.push(0x06);
        input.write_all(&bytes).await.unwrap();
        let code =
            tokio::time::timeout(Duration::from_secs(3), input.stopped()).await.expect("the host answers").unwrap();
        assert_eq!(code.map(|c| c.into_inner()), Some(u64::from(ErrorCode::NotPermitted.tag())));

        // Clipboard was granted: the handler gets the stream.
        let mut clip = conn.open_uni().await.unwrap();
        clip.write_all(&StreamKind::Clipboard.encode().unwrap()).await.unwrap();
        clip.write_all(&[0; 9]).await.unwrap();
        eventually(Duration::from_secs(3), "the clipboard stream", || {
            rec.streams.load(std::sync::atomic::Ordering::SeqCst) == 1
        })
        .await;

        // File data was not granted.
        let mut file = conn.open_uni().await.unwrap();
        let mut bytes = StreamKind::FileData.encode().unwrap();
        bytes.extend_from_slice(&[0; 12]);
        file.write_all(&bytes).await.unwrap();
        let code =
            tokio::time::timeout(Duration::from_secs(3), file.stopped()).await.expect("the host answers").unwrap();
        assert_eq!(code.map(|c| c.into_inner()), Some(u64::from(ErrorCode::NotPermitted.tag())));
        assert_eq!(
            rec.streams.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the refused streams never reached the handler"
        );
        q.stop();
    });
}

#[test]
fn streams_a_session_opens_carry_their_kind_and_the_priority_of_the_protocol_table() {
    run(async {
        let rec = Recorder::new(&[], &[StreamKind::Input, StreamKind::Clipboard, StreamKind::FileData]);
        let ui = TestUi::accepting(PermissionSet::CONTROL | PermissionSet::CLIPBOARD | PermissionSet::FILES);
        let q = Quic::start(ui, registry_one(rec.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::VIEW, None);
        let s = q
            .open(&v, viewer_config(PermissionSet::CONTROL | PermissionSet::CLIPBOARD | PermissionSet::FILES))
            .await
            .unwrap();
        for (kind, priority) in [(StreamKind::Input, 4), (StreamKind::Clipboard, 0), (StreamKind::FileData, 0)] {
            let mut stream = s.ctx().open_uni(kind).await.unwrap();
            assert_eq!(stream.priority().unwrap(), priority, "{kind:?}");
            // The recorder lets a stream go as soon as it has counted it, which stops the stream: a write that comes
            // after that is refused, and says nothing about the kind or the priority.
            let _ = stream.write_all(&[0; 12]).await;
        }
        eventually(Duration::from_secs(3), "the three streams", || {
            rec.streams.load(std::sync::atomic::Ordering::SeqCst) == 3
        })
        .await;
        // The kinds of the table, as numbers: control 3, input 4, cursor 2, terminal 2, video 1, clipboard and files 0.
        assert_eq!(swapdesk_session::CONTROL_PRIORITY, 3);
        assert_eq!(
            [
                StreamKind::Input,
                StreamKind::CursorShape,
                StreamKind::Terminal,
                StreamKind::Video,
                StreamKind::Clipboard,
                StreamKind::FileData
            ]
            .map(swapdesk_session::priority_of),
            [4, 2, 2, 1, 0, 0]
        );
        assert_eq!(
            swapdesk_session::priority_of(StreamKind::Video),
            swapdesk_video::VIDEO_PRIORITY,
            "the video sender uses the table's priority"
        );
        // A kind the peer may not receive is refused before anything is opened: the host does not take input.
        let host_ctx = lock(&rec.sessions)[0].clone();
        assert_eq!(host_ctx.open_uni(StreamKind::Input).await.err(), Some(ErrorCode::Internal));
        assert_eq!(host_ctx.open_bi(StreamKind::Terminal).await.err(), Some(ErrorCode::Internal));
        q.stop();
    });
}

#[test]
fn revoking_a_grant_stops_the_feature_within_a_second_and_tells_the_viewer() {
    run(async {
        let rec = Recorder::new(&[0x0501], &[]);
        let ui = TestUi::accepting(PermissionSet::CONTROL | PermissionSet::TERMINAL | PermissionSet::FILES);
        let q = Quic::start(ui, registry_one(rec.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::VIEW, None);
        let mut s = q
            .open(&v, viewer_config(PermissionSet::CONTROL | PermissionSet::TERMINAL | PermissionSet::FILES))
            .await
            .unwrap();
        let id = s.grant().session_id;
        s.ctx().send(TermOpen { term_id: 1, cols: 80, rows: 24, admin: false }).await.unwrap();
        eventually(Duration::from_secs(3), "the feature to run", || {
            rec.ticks.load(std::sync::atomic::Ordering::SeqCst) > 3
        })
        .await;

        // The host's user takes the terminal away.
        let revoked_at = std::time::Instant::now();
        assert!(q.host.set_session_grants(id, PermissionSet::CONTROL | PermissionSet::FILES));
        // The feature's task stops: its counter stops moving.
        let mut last = rec.ticks.load(std::sync::atomic::Ordering::SeqCst);
        let mut still = 0;
        let stopped_after = loop {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let now = rec.ticks.load(std::sync::atomic::Ordering::SeqCst);
            still = if now == last { still + 1 } else { 0 };
            last = now;
            if still >= 4 {
                break revoked_at.elapsed() - Duration::from_millis(100);
            }
            assert!(
                revoked_at.elapsed() < Duration::from_secs(1),
                "the feature was still running 1 s after the revocation"
            );
        };
        eprintln!(
            "F-PERM-02: the terminal task stopped {:.0} ms after the revocation",
            stopped_after.as_secs_f64() * 1000.0
        );
        assert!(stopped_after < Duration::from_secs(1));
        // The viewer is told, the handler hears what went, and a new request is refused.
        let e = wait_event(&mut s, Duration::from_secs(1), "PermissionsChanged", |e| {
            matches!(e, ViewerEvent::PermissionsChanged(_))
        })
        .await;
        assert_eq!(e, ViewerEvent::PermissionsChanged(PermissionSet::CONTROL | PermissionSet::FILES));
        assert_eq!(s.granted(), PermissionSet::CONTROL | PermissionSet::FILES);
        eventually(Duration::from_secs(1), "on_permissions", || !lock(&rec.perms).is_empty()).await;
        let (old, now) = lock(&rec.perms)[0];
        assert!(old.contains(PermissionSet::TERMINAL) && !now.contains(PermissionSet::TERMINAL));
        s.ctx().send(TermOpen { term_id: 2, cols: 80, rows: 24, admin: false }).await.unwrap();
        wait_event(&mut s, Duration::from_secs(2), "NOT_PERMITTED", |e| {
            is_error(e, ErrorCode::NotPermitted, 0x0501, 2)
        })
        .await;
        assert_eq!(rec.count(), 1);
        q.stop();
    });
}

#[test]
fn a_task_that_needs_a_permission_is_not_started_without_it_and_the_stored_grants_never_widen_a_session() {
    run_paused(async {
        let rec = Recorder::new(&[0x0501], &[]);
        let ui = TestUi::accepting(PermissionSet::CONTROL | PermissionSet::TERMINAL);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL | PermissionSet::TERMINAL);
        let (s, _task) = prompted(&mem, &who, PermissionSet::CONTROL | PermissionSet::TERMINAL).await;
        // The stored grants shrink in Permissões: the running session keeps at most what is still stored.
        set_stored(&mem.trust, &who, PermissionSet::CONTROL);
        mem.host.stored_grants_changed(&who.id);
        eventually(Duration::from_secs(1), "the session to narrow", || s.granted() == PermissionSet::CONTROL).await;
        // Widening them does not widen the running session.
        set_stored(&mem.trust, &who, PermissionSet::CONTROL | PermissionSet::FILES);
        mem.host.stored_grants_changed(&who.id);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(s.granted(), PermissionSet::CONTROL);
        let ctx = lock(&rec.sessions)[0].clone();
        assert!(
            !ctx.spawn_gated(PermissionSet::TERMINAL, async {}),
            "TERMINAL is not held, so the task is not started"
        );
        assert!(ctx.spawn_gated(PermissionSet::CONTROL, async {}));
    });
}

// ---------- F-SESS-01: the host state pause ----------

#[test]
fn locking_the_host_pauses_video_and_drops_input_unless_the_session_is_unattended() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut s, task) = prompted(&mem, &who, PermissionSet::CONTROL).await;
        let host_ctx = lock(&rec.sessions)[0].clone();
        assert!(host_ctx.can_inject());

        mem.host.set_desktop(Desktop::Locked);
        wait_event(&mut s, Duration::from_secs(1), "paused", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Locked, paused: true })
        })
        .await;
        assert!(host_ctx.is_paused() && !host_ctx.can_inject(), "input is dropped while the desktop is Locked");
        assert!(s.ctx().is_paused(), "the viewer knows too");
        assert_eq!(lock(&rec.states).last().copied(), Some((Desktop::Locked, true)));

        mem.host.set_desktop(Desktop::Default);
        wait_event(&mut s, Duration::from_secs(1), "resumed", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Default, paused: false })
        })
        .await;
        assert!(!host_ctx.is_paused() && host_ctx.can_inject());
        s.disconnect().await;
        task.await.unwrap();

        // A session that holds UNATTENDED goes on at the login, lock and UAC screens (here the entry of a 1.0
        // viewer, which gets the bit at once).
        let who2 = Who::new();
        pin(&mem.trust, &who2, PermissionSet::UNATTENDED | PermissionSet::CONTROL);
        let (u, _task) = mem.open(&who2, viewer_config_1_0(PermissionSet::UNATTENDED | PermissionSet::CONTROL)).await;
        let mut u = u.unwrap();
        for desktop in [Desktop::Secure, Desktop::Locked, Desktop::LoginWindow] {
            mem.host.set_desktop(desktop);
            wait_event(
                &mut u,
                Duration::from_secs(1),
                "HostState",
                |e| matches!(e, ViewerEvent::HostState { desktop: d, paused: false } if *d == desktop),
            )
            .await;
        }
        // Take UNATTENDED away on the login screen: the session pauses.
        let id = u.grant().session_id;
        mem.host.set_session_grants(id, PermissionSet::CONTROL);
        wait_event(&mut u, Duration::from_secs(1), "paused after the revocation", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::LoginWindow, paused: true })
        })
        .await;
    });
}

#[test]
fn a_session_that_starts_on_a_locked_desktop_starts_paused() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        lock(&ui.info).desktop = Desktop::Locked;
        let mem = Mem::new(ui, Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut s, _task) = prompted(&mem, &who, PermissionSet::CONTROL).await;
        wait_event(&mut s, Duration::from_secs(1), "paused", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Locked, paused: true })
        })
        .await;
    });
}

// ---------- caps per session ----------

#[test]
fn the_caps_of_a_session_answer_busy_and_free_their_slots() {
    run_paused(async {
        let rec = Recorder::new(&[0x0501, 0x0504, 0x040B, 0x0405], &[]);
        let ui = TestUi::accepting(PermissionSet::TERMINAL | PermissionSet::FILES);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::TERMINAL | PermissionSet::FILES);
        let (mut s, _task) = prompted(&mem, &who, PermissionSet::TERMINAL | PermissionSet::FILES).await;
        let ctx = s.ctx().clone();
        let count = |ty: u16| rec.messages().iter().filter(|m| m.type_id() == ty).count();

        // 4 terminals at most.
        for id in 1..=5 {
            ctx.send(TermOpen { term_id: id, cols: 80, rows: 24, admin: false }).await.unwrap();
        }
        wait_event(&mut s, Duration::from_secs(1), "BUSY for the 5th terminal", |e| {
            is_error(e, ErrorCode::Busy, 0x0501, 5)
        })
        .await;
        assert_eq!(count(0x0501), 4);
        ctx.send(TermClose { term_id: 2 }).await.unwrap();
        ctx.send(TermOpen { term_id: 5, cols: 80, rows: 24, admin: false }).await.unwrap();
        eventually(Duration::from_secs(1), "the 5th terminal after one closed", || count(0x0501) == 5).await;

        // 8 transfers at most.
        for id in 1..=9 {
            ctx.send(FileGet { transfer_id: id, path: "a".into() }).await.unwrap();
        }
        wait_event(&mut s, Duration::from_secs(1), "BUSY for the 9th transfer", |e| {
            is_error(e, ErrorCode::Busy, 0x040B, 9)
        })
        .await;
        assert_eq!(count(0x040B), 8);
        ctx.send(FileVerified { transfer_id: 1, ok: true }).await.unwrap();
        ctx.send(FileGet { transfer_id: 9, path: "a".into() }).await.unwrap();
        eventually(Duration::from_secs(1), "the 9th transfer after one finished", || count(0x040B) == 9).await;
    });
}

#[test]
fn at_most_500_chat_messages_per_session_on_both_sides() {
    run_paused(async {
        let rec = Recorder::new(&[0x0301], &[]);
        let ui = TestUi::accepting(PermissionSet::VIEW);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        // A well-behaved viewer's own session stops at the cap.
        let (s, _task) = prompted(&mem, &who, PermissionSet::VIEW).await;
        for id in 1..=500 {
            s.ctx().send(ChatMessage { msg_id: id, text: "oi".into(), sent_at_ms: 0 }).await.unwrap();
        }
        assert_eq!(
            s.ctx().send(ChatMessage { msg_id: 501, text: "oi".into(), sent_at_ms: 0 }).await,
            Err(swapdesk_session::SendError::Busy)
        );
        eventually(Duration::from_secs(5), "500 messages", || rec.count() == 500).await;
        s.disconnect().await;
        eventually(Duration::from_secs(1), "the host session to end", || mem.host.sessions().is_empty()).await;

        // A viewer that does not stop is answered BUSY by the host, and the 501st reaches nobody.
        let rec2 = Recorder::new(&[0x0301], &[]);
        let mem = Mem::new(TestUi::accepting(PermissionSet::VIEW), registry_one(rec2.clone()));
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (mut io, _task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::VIEW, None, None).await;
        next_matching(&mut io, "SessionGrant", |m| matches!(m, Message::SessionGrant(_)).then_some(())).await;
        for id in 1..=501 {
            io.out.send(ChatMessage { msg_id: id, text: "oi".into(), sent_at_ms: 0 }).await.unwrap();
        }
        let busy = next_matching(&mut io, "BUSY for the 501st message", |m| match m {
            Message::Error(e) if e.code == ErrorCode::Busy => Some(e.clone()),
            _ => None,
        })
        .await;
        assert_eq!((busy.about_type, busy.about_id), (0x0301, 501));
        assert_eq!(rec2.count(), 500);
    });
}

// ---------- the order of a session (5.2) and violations ----------

async fn expect_violation(
    mut io: swapdesk_session::ControlIo,
    task: tokio::task::JoinHandle<SessionEnd>,
    code: ErrorCode,
) {
    let said = next_matching(&mut io, "the fatal Error", |m| match m {
        Message::Error(e) if e.fatal => Some(e.clone()),
        _ => None,
    })
    .await;
    assert_eq!(said.code, code);
    let end = task.await.unwrap();
    assert_eq!((end.kind, end.code), (EndKind::Violation, code));
}

#[test]
fn a_message_out_of_order_before_the_grant_is_a_fatal_violation() {
    run_paused(async {
        let mem = Mem::new(TestUi::new(Script::Hold), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        // Capabilities before Hello.
        let (io, task) = mem.connect_raw(&who);
        io.out.send(capabilities()).await.unwrap();
        expect_violation(io, task, ErrorCode::ProtocolViolation).await;
        // A second Hello.
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::VIEW, None, None).await;
        raw_hello(&mut io, PermissionSet::VIEW, None, None).await;
        expect_violation(io, task, ErrorCode::ProtocolViolation).await;
        // A request while the prompt is open: nothing but session and keepalive messages flows before the grant.
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::VIEW, None, None).await;
        next_matching(&mut io, "PromptPending", |m| matches!(m, Message::PromptPending(_)).then_some(())).await;
        io.out.send(FileGet { transfer_id: 1, path: "x".into() }).await.unwrap();
        expect_violation(io, task, ErrorCode::ProtocolViolation).await;
        // A host-only message from the viewer, and a pairing message.
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::VIEW, None, None).await;
        io.out
            .send(SessionGrant {
                session_id: 1,
                granted: PermissionSet::CONTROL,
                mode: GrantMode::Prompted,
                resume_token: [0; 16],
            })
            .await
            .unwrap();
        expect_violation(io, task, ErrorCode::ProtocolViolation).await;
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::VIEW, None, None).await;
        io.out.send(PairConfirm { matches: true }).await.unwrap();
        expect_violation(io, task, ErrorCode::ProtocolViolation).await;
    });
}

#[test]
fn a_live_session_refuses_host_only_messages_and_a_repeated_handshake() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui, Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        for bad in [
            Message::from(HostState { desktop: Desktop::Locked, paused: true }),
            Hello {
                proto_minor: 0,
                app_version: String::new(),
                name: "x".into(),
                os: Os::MacOs,
                os_version: String::new(),
                intent: Intent::View,
                requested: PermissionSet::VIEW,
                resume: None,
                swap_token: None,
            }
            .into(),
            capabilities().into(),
            DisplayList { displays: vec![] }.into(),
        ] {
            let (mut io, task) = mem.connect_raw(&who);
            raw_hello(&mut io, PermissionSet::CONTROL, None, None).await;
            next_matching(&mut io, "SessionGrant", |m| matches!(m, Message::SessionGrant(_)).then_some(())).await;
            io.out.send(bad).await.unwrap();
            expect_violation(io, task, ErrorCode::ProtocolViolation).await;
        }
    });
}

#[test]
fn the_host_answers_pings_and_pings_the_viewer_once_a_second() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut io, _task) = mem.connect_raw(&who);
        io.out.send(Ping { nonce: 5, t_send_us: 99 }).await.unwrap();
        raw_hello(&mut io, PermissionSet::CONTROL, None, None).await;
        let pong =
            next_matching(&mut io, "Pong", |m| if let Message::Pong(p) = m { Some(p.clone()) } else { None }).await;
        assert_eq!((pong.nonce, pong.t_send_us), (5, 99), "a Pong echoes the Ping's nonce and send time");
        next_matching(&mut io, "SessionGrant", |m| matches!(m, Message::SessionGrant(_)).then_some(())).await;
        let t0 = Instant::now();
        for _ in 0..3 {
            next_matching(&mut io, "Ping", |m| matches!(m, Message::Ping(_)).then_some(())).await;
        }
        let took = t0.elapsed();
        assert!(took >= Duration::from_millis(1_800) && took <= Duration::from_millis(3_000), "3 pings in {took:?}");
    });
}

// ---------- the registry ----------

struct Claims(&'static [u16], &'static [StreamKind]);

impl ChannelHandler for Claims {
    fn name(&self) -> &'static str {
        "claims"
    }
    fn messages(&self) -> &'static [u16] {
        self.0
    }
    fn streams(&self) -> &'static [StreamKind] {
        self.1
    }
    fn on_message(&self, _: &swapdesk_session::SessionCtx, _: Message) {}
}

#[test]
fn the_registry_refuses_core_unknown_and_duplicate_claims_and_registers_nothing_on_failure() {
    let mut r = Registry::new();
    for core in [0x0001, 0x0005, 0x0009, 0x0F01, 0x0FFF, 0x0701] {
        assert_eq!(r.register(Arc::new(Claims(Box::leak(Box::new([core])), &[]))), Err(RegisterError::Reserved(core)));
    }
    assert_eq!(r.register(Arc::new(Claims(&[0x7777], &[]))), Err(RegisterError::UnknownMessage(0x7777)));
    r.register(Arc::new(Claims(&[0x0501], &[StreamKind::Terminal]))).unwrap();
    assert_eq!(r.register(Arc::new(Claims(&[0x0502, 0x0501], &[]))), Err(RegisterError::DuplicateMessage(0x0501)));
    assert_eq!(
        r.register(Arc::new(Claims(&[0x0502], &[StreamKind::Terminal]))),
        Err(RegisterError::DuplicateStream(StreamKind::Terminal))
    );
    // The refused handler registered nothing, not even the type that was free.
    r.register(Arc::new(Claims(&[0x0502], &[]))).unwrap();
}

#[test]
fn a_permitted_message_with_no_feature_installed_is_answered_unavailable() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::TERMINAL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::TERMINAL);
        let (mut s, _task) = prompted(&mem, &who, PermissionSet::TERMINAL).await;
        s.ctx().send(TermOpen { term_id: 9, cols: 80, rows: 24, admin: false }).await.unwrap();
        wait_event(&mut s, Duration::from_secs(1), "UNAVAILABLE", |e| is_error(e, ErrorCode::Unavailable, 0x0501, 9))
            .await;
    });
}

struct Panics;

impl ChannelHandler for Panics {
    fn name(&self) -> &'static str {
        "panics"
    }
    fn messages(&self) -> &'static [u16] {
        &[0x040B]
    }
    fn on_message(&self, _: &swapdesk_session::SessionCtx, _: Message) {
        panic!("a feature crate has a bug (this panic is the test)");
    }
}

#[test]
fn a_panic_in_a_handler_ends_only_its_session_and_frees_the_host() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::FILES);
        let mem = Mem::new(ui, registry_of(&[Arc::new(Panics)]));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::FILES);
        let (mut s, task) = prompted(&mem, &who, PermissionSet::FILES).await;
        s.ctx().send(FileGet { transfer_id: 1, path: "x".into() }).await.unwrap();
        assert!(task.await.unwrap_err().is_panic(), "the session task panicked");
        // The viewer sees a lost connection, the host forgot the session, and the next session starts.
        wait_event(
            &mut s,
            Duration::from_secs(10),
            "the end",
            |e| matches!(e, ViewerEvent::Ended(end) if end.kind == EndKind::Lost),
        )
        .await;
        assert!(mem.host.sessions().is_empty());
        let (again, _t) = prompted(&mem, &who, PermissionSet::FILES).await;
        assert_eq!(again.grant().mode, GrantMode::Prompted);
    });
}

#[test]
fn handlers_hear_the_start_and_the_end_of_a_session() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let viewer_rec = Recorder::new(&[], &[]);
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.registry = registry_one(viewer_rec.clone());
        let (s, task) = mem.open(&who, cfg).await;
        let s = s.unwrap();
        assert_eq!(rec.started.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(viewer_rec.started.load(std::sync::atomic::Ordering::SeqCst), 1);
        let end = s.disconnect().await;
        assert_eq!((end.kind, end.code), (EndKind::LocalBye, ErrorCode::NoError));
        let host_end = task.await.unwrap();
        assert_eq!((host_end.kind, host_end.code), (EndKind::PeerBye, ErrorCode::NoError));
        assert_eq!(lock(&rec.ended).clone(), vec![host_end], "the host's handler hears how it ended");
        assert_eq!(lock(&viewer_rec.ended).clone(), vec![end], "and so does the viewer's");
    });
}

// ---------- D13: consent ----------

#[test]
fn consent_asks_the_logged_in_user_and_refuses_when_nobody_is_present_or_it_times_out() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::UNATTENDED | PermissionSet::TERMINAL | PermissionSet::FILES);
        let (s, _task) = mem
            .open(&who, viewer_config(PermissionSet::UNATTENDED | PermissionSet::TERMINAL | PermissionSet::FILES))
            .await;
        let mut s = s.unwrap();
        let ctx = lock(&rec.sessions)[0].clone();

        // Nobody present: refused at once, the viewer is not told to wait.
        ui.set_present(false);
        let t0 = Instant::now();
        assert_eq!(ctx.request_consent(Feature::Terminal).await, Err(ErrorCode::NobodyPresent));
        assert!(t0.elapsed() < Duration::from_secs(1));
        assert!(lock(&ui.consents).is_empty());

        // Present, and the user agrees.
        ui.set_present(true);
        assert_eq!(ctx.request_consent(Feature::Files).await, Ok(()));
        wait_event(&mut s, Duration::from_secs(1), "ConsentPending", |e| matches!(e, ViewerEvent::Message(Message::ConsentPending(c)) if c.feature == Feature::Files && c.timeout_s == 20)).await;

        // Present, and the user declines.
        ui.answer_consent_with(Some(false));
        assert_eq!(ctx.request_consent(Feature::Terminal).await, Err(ErrorCode::Declined));

        // Present, and nobody answers: TIMEOUT at 20 s.
        ui.answer_consent_with(None);
        let t0 = Instant::now();
        assert_eq!(ctx.request_consent(Feature::Files).await, Err(ErrorCode::Timeout));
        assert_eq!(t0.elapsed(), Duration::from_secs(20));
        let answers: Vec<ErrorCode> = ui
            .events()
            .iter()
            .filter_map(|e| if let HostEvent::ConsentAsked { answer, .. } = e { Some(*answer) } else { None })
            .collect();
        assert_eq!(
            answers,
            vec![ErrorCode::NobodyPresent, ErrorCode::NoError, ErrorCode::Declined, ErrorCode::Timeout]
        );
    });
}

// ---------- the wire: violations that only a real stream can show ----------

async fn raw_connection(q: &Quic, v: &QuicViewer) -> quinn::Connection {
    swapdesk_trust::connect_session(&v.endpoint, &v.who.identity, q.addr, *q.host_identity.spki()).await.unwrap()
}

async fn closed_with(conn: &quinn::Connection, what: &str) -> u64 {
    match tokio::time::timeout(Duration::from_secs(6), conn.closed())
        .await
        .unwrap_or_else(|_| panic!("the host did not close the connection: {what}"))
    {
        quinn::ConnectionError::ApplicationClosed(c) => c.error_code.into_inner(),
        other => panic!("{what}: expected an application close, got {other:?}"),
    }
}

#[test]
fn the_wire_format_violations_close_the_connection_with_the_right_code() {
    run(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let q = Quic::start(ui, Registry::new(), None, |_| {}).await;

        // An unknown message type on the control stream.
        let v = q.viewer(PermissionSet::CONTROL, None);
        let conn = raw_connection(&q, &v).await;
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        send.write_all(&[0x3F, 0x00]).await.unwrap();
        assert_eq!(closed_with(&conn, "unknown type").await, u64::from(ErrorCode::ProtocolViolation.tag()));

        // A frame over 65 535 bytes is TOO_LARGE, refused before its payload is read.
        let v = q.viewer(PermissionSet::CONTROL, None);
        let conn = raw_connection(&q, &v).await;
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        let mut head = Vec::new();
        let mut w = swapdesk_proto::Writer::new();
        w.put_varint(0x0F01).unwrap();
        w.put_varint(65_536).unwrap();
        head.extend_from_slice(&w.into_bytes());
        send.write_all(&head).await.unwrap();
        assert_eq!(closed_with(&conn, "oversize frame").await, u64::from(ErrorCode::TooLarge.tag()));

        // A payload that does not decode (trailing bytes) is a violation.
        let v = q.viewer(PermissionSet::CONTROL, None);
        let conn = raw_connection(&q, &v).await;
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        let mut frame = Message::from(Ping { nonce: 1, t_send_us: 2 }).encode_frame().unwrap();
        frame[1] += 1;
        frame.push(0);
        send.write_all(&frame).await.unwrap();
        assert_eq!(closed_with(&conn, "trailing bytes").await, u64::from(ErrorCode::ProtocolViolation.tag()));

        // An unknown stream kind, a stream the viewer may not open (video flows host to viewer), and a stream before
        // the grant are all violations.
        for (kind_bytes, what) in [
            (vec![0x3E], "unknown stream kind"),
            (StreamKind::Video.encode().unwrap(), "a video stream from the viewer"),
        ] {
            let v = q.viewer(PermissionSet::CONTROL, None);
            let s = q.open(&v, viewer_config(PermissionSet::CONTROL)).await.unwrap();
            let conn = s.ctx().connection().unwrap().clone();
            let mut uni = conn.open_uni().await.unwrap();
            // One write: the host answers the kind byte alone with a fatal Error, and the viewer's session then closes
            // its own connection (a second write issued after that, which is what a loaded machine does, fails with
            // `ConnectionLost(LocallyClosed)`).
            let mut bytes = kind_bytes.clone();
            bytes.extend_from_slice(&[0; 8]);
            uni.write_all(&bytes).await.unwrap();
            // The viewer's own session sees the host's fatal Error and ends with it.
            let end = tokio::time::timeout(Duration::from_secs(6), s.ctx().wait_ended())
                .await
                .unwrap_or_else(|_| panic!("{what}: the host did not end the session"));
            assert_eq!((end.kind, end.code), (EndKind::Violation, ErrorCode::ProtocolViolation), "{what}");
        }
        let v = q.viewer(PermissionSet::CONTROL, None);
        let conn = raw_connection(&q, &v).await;
        // The control stream exists (the host starts the session on it) but there is no grant yet.
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        send.write_all(&Message::from(Ping { nonce: 1, t_send_us: 0 }).encode_frame().unwrap()).await.unwrap();
        let mut uni = conn.open_uni().await.unwrap();
        // One write, for the same reason as above: the host stops the stream as soon as it has read the kind.
        let mut bytes = StreamKind::Input.encode().unwrap();
        bytes.push(0x06);
        uni.write_all(&bytes).await.unwrap();
        assert_eq!(
            closed_with(&conn, "a stream before the grant").await,
            u64::from(ErrorCode::ProtocolViolation.tag())
        );
        q.stop();
    });
}

#[test]
fn a_viewer_that_the_host_does_not_know_never_gets_a_session_over_the_wire() {
    run(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let q = Quic::start(ui.clone(), Registry::new(), None, |_| {}).await;
        // Not pinned: the handshake itself fails (PROTOCOL.md 2.2); nothing reaches the session layer.
        let stranger = QuicViewer {
            who: Who::new(),
            endpoint: swapdesk_trust::endpoint_from_socket(std::net::UdpSocket::bind(lo()).unwrap(), None).unwrap(),
            link: None,
        };
        let r = q.open(&stranger, viewer_config(PermissionSet::CONTROL)).await;
        assert!(r.is_err());
        assert_eq!(ui.prompt_count(), 0);
        q.stop();
    });
}

// ---------- protocol minor 1 in a session ----------

/// A build of minor 1 takes `StoredGrants` after the grant, either way, without acting on it, and refuses `Elevate`
/// in a session that entered with a prompt (ACCOUNTS.md 9.3), without ending it.
#[test]
fn stored_grants_is_taken_after_the_grant_and_elevate_is_refused_without_ending_the_session() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut s, _task) = prompted(&mem, &who, PermissionSet::CONTROL).await;
        let host_ctx = lock(&rec.sessions)[0].clone();
        assert_eq!((host_ctx.peer_minor(), s.ctx().peer_minor()), (PROTO_MINOR, PROTO_MINOR));

        // Viewer to host and host to viewer: nothing happens, the session lives.
        s.ctx().send(StoredGrants { grants: PermissionSet::VIEW }).await.unwrap();
        host_ctx.send(StoredGrants { grants: PermissionSet::CONTROL }).await.unwrap();
        // A prompted session cannot be elevated: a refusal that is not fatal, about this message.
        s.ctx().send(Elevate {}).await.unwrap();
        wait_event(&mut s, Duration::from_secs(1), "the refusal of Elevate", |e| {
            is_error(e, ErrorCode::NotPermitted, 0x000C, 0)
        })
        .await;
        assert!(s.ctx().ended().is_none() && host_ctx.ended().is_none(), "the session goes on");
        assert_eq!(host_ctx.granted(), PermissionSet::CONTROL, "nothing was added");
    });
}

/// A peer that announced minor 0 cannot have sent a message of minor 1: that is `UNSUPPORTED_VERSION`, fatal.
#[test]
fn a_minor_1_message_from_a_peer_that_announced_minor_0_ends_the_session() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui, Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (s, task) = mem.open(&who, viewer_config_1_0(PermissionSet::CONTROL)).await;
        let s = s.unwrap();
        // The viewer's own send path believes the host's minor (1), so the message leaves; the host knows better.
        s.ctx().send(StoredGrants { grants: PermissionSet::VIEW }).await.unwrap();
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::Violation, ErrorCode::UnsupportedVersion));
    });
}

/// The send path keeps PROTOCOL.md 1 by itself: what a 1.0 viewer is sent holds no value of minor 1, whoever
/// produced it, and no minor-1 type is written to it.
#[test]
fn a_minor_0_viewer_is_never_sent_a_minor_1_value_or_type() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let (old, new) = (Who::new(), Who::new());
        pin(&mem.trust, &old, PermissionSet::CONTROL);
        pin(&mem.trust, &new, PermissionSet::CONTROL);

        let (s, _task) = mem.open(&old, viewer_config_1_0(PermissionSet::CONTROL)).await;
        let mut s = s.unwrap();
        let host_ctx = lock(&rec.sessions).last().unwrap().clone();
        assert_eq!(host_ctx.peer_minor(), 0);
        // A type of minor 1 is not written; the next message proves the stream is intact.
        host_ctx.send(StoredGrants { grants: PermissionSet::VIEW }).await.unwrap();
        mem.host.set_desktop(Desktop::AccountAway);
        wait_event(&mut s, Duration::from_secs(1), "the 1.0 form of the pause", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Locked, paused: true })
        })
        .await;
        assert!(s.ctx().ended().is_none(), "nothing the 1.0 viewer cannot read reached it");
        s.disconnect().await;
        eventually(Duration::from_secs(1), "the end", || mem.host.sessions().is_empty()).await;

        // A viewer of minor 1 is told the real state.
        let (s, _task) = mem.open(&new, viewer_config(PermissionSet::CONTROL)).await;
        let mut s = s.unwrap();
        mem.host.set_desktop(Desktop::AccountAway);
        wait_event(&mut s, Duration::from_secs(1), "the pause of minor 1", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::AccountAway, paused: true })
        })
        .await;
    });
}

// ---------- GRANTS-SYNC.md 3: `StoredGrants` in a session ----------

fn peer_grants_of(ui: &TestUi) -> Vec<PermissionSet> {
    ui.events()
        .iter()
        .filter_map(|e| match e {
            HostEvent::PeerGrants { grants, .. } => Some(*grants),
            _ => None,
        })
        .collect()
}

/// The host tells what it stored for the viewer, not what this session holds: a prompted pick that differs from the
/// store does not change the value.
#[test]
fn the_viewer_hears_the_hosts_stored_grants_not_the_sessions() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL | PermissionSet::FILES);
        let mem = Mem::new(ui, Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (mut s, _task) = prompted(&mem, &who, PermissionSet::CONTROL | PermissionSet::FILES).await;
        assert!(s.grant().granted.contains(PermissionSet::FILES), "the session holds the pick");
        wait_event(
            &mut s,
            Duration::from_secs(1),
            "the stored grants of the host",
            |e| matches!(e, ViewerEvent::PeerGrants(g) if *g == PermissionSet::VIEW),
        )
        .await;
        assert!(s.granted().contains(PermissionSet::FILES), "display only: the session is unchanged");
    });
}

#[test]
fn the_host_hears_the_viewers_stored_grants() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.stored_grants = Some(PermissionSet::VIEW | PermissionSet::FILES);
        let (s, _task) = mem.open(&who, cfg).await;
        let s = s.unwrap();
        eventually(Duration::from_secs(1), "PeerGrants on the host", || {
            peer_grants_of(&ui) == vec![PermissionSet::VIEW | PermissionSet::FILES]
        })
        .await;
        let about = ui.find_event(|e| match e {
            HostEvent::PeerGrants { device, .. } => Some(*device),
            _ => None,
        });
        assert_eq!(about, Some(who.id), "about the authenticated device");
        assert_eq!(s.granted(), PermissionSet::CONTROL, "display only: the session is unchanged");
    });
}

/// A change on either side while the session lives reaches the other; an unchanged value is not told again.
#[test]
fn a_stored_grants_change_during_a_session_reaches_the_peer() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.stored_grants = Some(PermissionSet::VIEW);
        let (s, _task) = mem.open(&who, cfg).await;
        let mut s = s.unwrap();
        wait_event(
            &mut s,
            Duration::from_secs(1),
            "the first value",
            |e| matches!(e, ViewerEvent::PeerGrants(g) if *g == PermissionSet::CONTROL),
        )
        .await;

        // The user at the host changes Permissões.
        set_stored(&mem.trust, &who, PermissionSet::VIEW | PermissionSet::FILES);
        mem.host.stored_grants_changed(&who.id);
        wait_event(
            &mut s,
            Duration::from_secs(1),
            "the new value of the host",
            |e| matches!(e, ViewerEvent::PeerGrants(g) if *g == PermissionSet::VIEW | PermissionSet::FILES),
        )
        .await;

        // The user at the viewer changes Permissões on that side; the same value again is not told twice.
        let new = PermissionSet::CONTROL | PermissionSet::CLIPBOARD;
        s.ctx().set_own_stored(new);
        s.ctx().set_own_stored(new);
        eventually(Duration::from_secs(1), "the new value of the viewer on the host", || {
            peer_grants_of(&ui).last() == Some(&new)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(peer_grants_of(&ui), vec![PermissionSet::VIEW, new], "each value once");
    });
}

/// Before `SessionGrant` it is a protocol violation, on the host and on the viewer.
#[test]
fn stored_grants_before_the_grant_is_a_protocol_violation() {
    run_paused(async {
        // The host: a raw viewer of minor 1 sends it right after its `Hello`.
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (io, task) = mem.connect_raw(&who);
        io.out.set_peer_minor(PROTO_MINOR);
        io.out.send(hello_of_minor(PROTO_MINOR)).await.unwrap();
        io.out.send(StoredGrants { grants: PermissionSet::VIEW }).await.unwrap();
        let end = task.await.unwrap();
        assert_eq!((end.kind, end.code), (EndKind::Violation, ErrorCode::ProtocolViolation));

        // The viewer: a raw host of minor 1 sends it before the grant.
        let (mut host, viewer) = swapdesk_session::memory_pair();
        let open = tokio::spawn(ViewerSession::open_io(viewer, None, None, viewer_config(PermissionSet::CONTROL)));
        raw_host_handshake(&mut host, PROTO_MINOR).await;
        host.out.set_peer_minor(PROTO_MINOR);
        host.out.send(StoredGrants { grants: PermissionSet::VIEW }).await.unwrap();
        assert!(open.await.unwrap().is_err(), "the session does not open");
    });
}

/// A host that announced minor 0 cannot have sent it: the viewer ends the session with `UNSUPPORTED_VERSION`.
/// (The host side of the same rule: `a_minor_1_message_from_a_peer_that_announced_minor_0_ends_the_session`.)
#[test]
fn stored_grants_from_a_minor_0_peer_is_unsupported_version() {
    run_paused(async {
        let (mut host, viewer) = swapdesk_session::memory_pair();
        let open = tokio::spawn(ViewerSession::open_io(viewer, None, None, viewer_config(PermissionSet::CONTROL)));
        raw_host_handshake(&mut host, 0).await;
        raw_grant(&mut host).await;
        let s = open.await.unwrap().expect("the session opens");
        // The send path of the raw host would drop it for a minor-0 peer; it plays a broken 1.0 host on purpose.
        host.out.set_peer_minor(PROTO_MINOR);
        host.out.send(StoredGrants { grants: PermissionSet::VIEW }).await.unwrap();
        let end = s.ctx().wait_ended().await;
        assert_eq!((end.kind, end.code), (EndKind::Violation, ErrorCode::UnsupportedVersion));
    });
}

/// Guard (green before and after): a viewer and a host that announce minor 0 are sent only types of 1.0, whatever
/// the other side has stored.
#[test]
fn a_minor_0_peer_is_sent_exactly_the_1_0_sequence() {
    run_paused(async {
        // A 1.0 viewer of a host that stored grants for it.
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut io, _task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::CONTROL, None, None).await;
        let mut seen = Vec::new();
        let mut keep = |m: &Message, want: fn(&Message) -> bool| {
            seen.push(m.type_id());
            want(m).then_some(())
        };
        next_matching(&mut io, "the grant", |m| keep(m, |m| matches!(m, Message::SessionGrant(_)))).await;
        set_stored(&mem.trust, &who, PermissionSet::VIEW);
        mem.host.stored_grants_changed(&who.id);
        next_matching(&mut io, "the narrowed session", |m| keep(m, |m| matches!(m, Message::PermissionsChanged(_))))
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(m) = io.rx.try_recv() {
            seen.push(m.unwrap().type_id());
        }
        for t in seen {
            assert_eq!(Message::type_min_minor(t), 0, "{t:#06x} reached a 1.0 viewer");
        }

        // A 1.0 host of a viewer that stored grants for it: the viewer sends it nothing of minor 1.
        let (mut host, viewer) = swapdesk_session::memory_pair();
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.stored_grants = Some(PermissionSet::VIEW);
        let open = tokio::spawn(ViewerSession::open_io(viewer, None, None, cfg));
        let mut sent = raw_host_handshake(&mut host, 0).await;
        raw_grant(&mut host).await;
        let s = open.await.unwrap().expect("the session opens");
        s.ctx().set_own_stored(PermissionSet::CONTROL);
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(m) = host.rx.try_recv() {
            sent.push(m.unwrap());
        }
        for m in sent {
            assert_eq!(Message::type_min_minor(m.type_id()), 0, "{:#06x} reached a 1.0 host", m.type_id());
        }
    });
}

fn hello_of_minor(minor: u16) -> Hello {
    Hello {
        proto_minor: minor,
        app_version: "t".into(),
        name: "raw".into(),
        os: Os::MacOs,
        os_version: "15".into(),
        intent: Intent::Control,
        requested: PermissionSet::CONTROL,
        resume: None,
        swap_token: None,
    }
}

/// Plays a host of `minor` up to the decision: reads the `Hello` and `Capabilities` of the viewer, answers both.
/// Returns what the viewer sent.
async fn raw_host_handshake(host: &mut swapdesk_session::ControlIo, minor: u16) -> Vec<Message> {
    let hello = next_msg(host, "Hello").await;
    let caps = next_msg(host, "Capabilities").await;
    host.out
        .send(HelloAck {
            proto_minor: minor,
            app_version: "t".into(),
            name: "raw-host".into(),
            os: Os::Windows,
            os_version: "11".into(),
        })
        .await
        .unwrap();
    host.out.send(capabilities()).await.unwrap();
    vec![hello, caps]
}

async fn raw_grant(host: &mut swapdesk_session::ControlIo) {
    host.out
        .send(SessionGrant {
            session_id: 7,
            granted: PermissionSet::CONTROL,
            mode: GrantMode::Prompted,
            resume_token: [1; 16],
        })
        .await
        .unwrap();
}

// ---------- ACCOUNTS.md 5.6: one hosted session per machine, across the accounts' cores ----------

/// Two accounts of one machine, each with a device that holds `UNATTENDED`, at the sign-in screen (nobody present):
/// both devices enter at once, and exactly one session opens; the other is `BUSY` until it ends.
#[test]
fn two_unattended_entries_at_the_sign_in_screen_at_once_admit_exactly_one() {
    run_paused(async {
        let machine = swapdesk_session::MachineHosting::new();
        let account = |m: std::sync::Arc<swapdesk_session::MachineHosting>| {
            let ui = TestUi::accepting(PermissionSet::CONTROL);
            ui.set_present(false);
            Mem::with(ui, Registry::new(), move |c| c.machine = Some(m))
        };
        let (a, b) = (account(machine.clone()), account(machine.clone()));
        let (wa, wb) = (Who::new(), Who::new());
        let stored = PermissionSet::CONTROL | PermissionSet::UNATTENDED;
        pin(&a.trust, &wa, stored);
        pin(&b.trust, &wb, stored);
        let asks = PermissionSet::CONTROL | PermissionSet::UNATTENDED;
        // Both at once: each open runs as a task of its own.
        let open = |host: swapdesk_session::HostService, id: swapdesk_trust::DeviceId| {
            tokio::spawn(async move {
                let (host_io, viewer_io) = swapdesk_session::memory_pair();
                let task = tokio::spawn(async move { host.serve_io(host_io, None, id).await });
                let s = ViewerSession::open_io(viewer_io, None, Some([9; 16]), viewer_config(asks)).await;
                (s, task)
            })
        };
        let (ta, tb) = (open(a.host.clone(), wa.id), open(b.host.clone(), wb.id));
        let (sa, sb) = (ta.await.unwrap().0, tb.await.unwrap().0);
        let opened = [sa.is_ok(), sb.is_ok()];
        assert_eq!(opened.iter().filter(|o| **o).count(), 1, "exactly one: {opened:?}");
        let (winner, loser_mem, loser_who) = match (sa, sb) {
            (Ok(s), Err(e)) => {
                assert!(matches!(e, OpenError::Refused(ErrorCode::Busy)), "{e:?}");
                (s, &b, &wb)
            }
            (Err(e), Ok(s)) => {
                assert!(matches!(e, OpenError::Refused(ErrorCode::Busy)), "{e:?}");
                (s, &a, &wa)
            }
            _ => unreachable!(),
        };
        assert!(machine.is_held());
        // The session ends: the machine is free, and the other account hosts.
        winner.disconnect().await;
        eventually(Duration::from_secs(2), "the machine is free", || !machine.is_held()).await;
        let (again, _task) = loser_mem.open(loser_who, viewer_config(asks)).await;
        assert!(again.is_ok(), "the other account hosts now");
    });
}

// ---------- ACCOUNTS.md 5.1: an account hosts only its own console ----------

fn set_owner(ui: &TestUi, owner: swapdesk_session::ConsoleOwner) {
    *lock(&ui.owner) = owner;
}

/// Another account at the console: a request is refused, `ACCOUNT_AWAY` to a viewer of minor 1 and
/// `NOBODY_PRESENT` to one of 1.0, whatever it holds.
#[test]
fn a_request_to_an_account_not_at_the_console_is_refused_account_away_or_nobody_present_by_minor() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        set_owner(&ui, swapdesk_session::ConsoleOwner::Other);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL | PermissionSet::UNATTENDED);
        let asks = PermissionSet::CONTROL | PermissionSet::UNATTENDED;
        let (s, _t) = mem.open(&who, viewer_config(asks)).await;
        assert_refused(s, ErrorCode::AccountAway);
        let (s, _t) = mem.open(&who, viewer_config_1_0(asks)).await;
        assert_refused(s, ErrorCode::NobodyPresent);
        assert_eq!(ui.prompt_count(), 0, "nobody was asked");
    });
}

/// The sign-in screen belongs to no account: a device holding `UNATTENDED` enters, a prompted request finds nobody.
#[test]
fn at_the_sign_in_screen_an_unattended_device_of_any_account_enters() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        ui.set_present(false);
        set_owner(&ui, swapdesk_session::ConsoleOwner::Nobody);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL | PermissionSet::UNATTENDED);
        let (s, _t) = mem.open(&who, viewer_config(PermissionSet::CONTROL | PermissionSet::UNATTENDED)).await;
        assert_eq!(s.expect("enters").grant().mode, GrantMode::Unattended);
    });
}

#[test]
fn a_prompted_request_at_the_sign_in_screen_is_nobody_present() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        ui.set_present(false);
        set_owner(&ui, swapdesk_session::ConsoleOwner::Nobody);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (s, _t) = mem.open(&who, viewer_config(PermissionSet::CONTROL)).await;
        assert_refused(s, ErrorCode::NobodyPresent);
    });
}

/// During a session another account takes the console: the session pauses at once (`AccountAway` to minor 1,
/// `Locked` paused to 1.0), goes on if the account comes back within the minute, and ends with `ACCOUNT_AWAY`
/// otherwise.
#[test]
fn when_another_account_takes_the_console_the_session_pauses_then_ends() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL | PermissionSet::UNATTENDED);
        let (s, task) = mem.open(&who, viewer_config(PermissionSet::CONTROL | PermissionSet::UNATTENDED)).await;
        let mut s = s.unwrap();
        // It leaves and comes back within the minute: the session goes on.
        mem.host.set_desktop(Desktop::AccountAway);
        wait_event(&mut s, Duration::from_secs(1), "paused", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::AccountAway, paused: true })
        })
        .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        mem.host.set_desktop(Desktop::Default);
        wait_event(&mut s, Duration::from_secs(1), "resumed", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Default, paused: false })
        })
        .await;
        tokio::time::sleep(Duration::from_secs(45)).await;
        assert!(s.ctx().ended().is_none(), "the minute counts from the last departure");
        // It leaves for good: the session ends after the minute.
        mem.host.set_desktop(Desktop::AccountAway);
        let end = tokio::time::timeout(Duration::from_secs(70), task).await.expect("ends").unwrap();
        assert_eq!(end.code, ErrorCode::AccountAway);
    });
}

/// Review of S4 (ACCOUNTS.md 5.1): a session committed while its account is already away starts paused, and the
/// minute counts from its start.
#[test]
fn a_session_that_starts_while_its_account_is_away_ends_after_the_minute() {
    run_paused(async {
        let ui = TestUi::accepting(PermissionSet::CONTROL);
        ui.info.lock().unwrap().desktop = Desktop::AccountAway;
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL | PermissionSet::UNATTENDED);
        let (s, task) = mem.open(&who, viewer_config(PermissionSet::CONTROL | PermissionSet::UNATTENDED)).await;
        let s = s.unwrap();
        assert!(s.ctx().ended().is_none());
        let end = tokio::time::timeout(Duration::from_secs(70), task).await.expect("ends").unwrap();
        assert_eq!(end.code, ErrorCode::AccountAway);
    });
}

/// ACCOUNTS.md 9: a session that entered without a prompt is elevated by `Elevate`: it gains `UNATTENDED` (and
/// `ADMIN_TERMINAL`, stored and requested), stops pausing on a secure desktop, the host logs `Elevated` and the
/// resume token dies. Asked again it answers the same; once the device no longer holds `UNATTENDED` it refuses.
#[test]
fn elevate_adds_the_machine_level_bits_to_an_unattended_entry_and_is_logged() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        lock(&ui.info).desktop = Desktop::Locked;
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        let stored = PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL;
        pin(&mem.trust, &who, stored);
        let (s, _task) = mem.open(&who, viewer_config(stored)).await;
        let mut s = s.unwrap();
        assert_eq!(
            s.grant().granted,
            PermissionSet::CONTROL | PermissionSet::TERMINAL,
            "entered without the machine-level bits (the terminal as the user stays)"
        );
        wait_event(&mut s, Duration::from_secs(1), "paused on the lock screen", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Locked, paused: true })
        })
        .await;
        let resumable = mem.host.resumable_sessions();
        s.ctx().send(Elevate {}).await.unwrap();
        wait_event(
            &mut s,
            Duration::from_secs(1),
            "elevated",
            |e| matches!(e, ViewerEvent::PermissionsChanged(g) if *g == stored),
        )
        .await;
        wait_event(&mut s, Duration::from_secs(1), "no longer paused", |e| {
            matches!(e, ViewerEvent::HostState { desktop: Desktop::Locked, paused: false })
        })
        .await;
        assert!(
            ui.events().iter().any(|e| matches!(e, HostEvent::Elevated { granted, .. } if *granted == stored)),
            "logged as Elevated"
        );
        assert!(!ui.events().iter().any(|e| matches!(e, HostEvent::GrantsChanged { .. })), "not as a grant change");
        assert_eq!(mem.host.resumable_sessions(), resumable - 1, "the resume token died");
        // Again: the same answer.
        s.ctx().send(Elevate {}).await.unwrap();
        wait_event(
            &mut s,
            Duration::from_secs(1),
            "the same answer",
            |e| matches!(e, ViewerEvent::PermissionsChanged(g) if *g == stored),
        )
        .await;
        // The device loses UNATTENDED in Permissões: the session narrows, and Elevate is refused.
        set_stored(&mem.trust, &who, PermissionSet::CONTROL);
        mem.host.stored_grants_changed(&who.id);
        s.ctx().send(Elevate {}).await.unwrap();
        wait_event(&mut s, Duration::from_secs(1), "refused", |e| is_error(e, ErrorCode::NotPermitted, 0x000C, 0))
            .await;
        assert!(s.ctx().ended().is_none());
    });
}

/// `ADMIN_TERMINAL` comes with the elevation only when the viewer asked for it.
#[test]
fn elevate_adds_the_administrator_terminal_only_when_it_was_requested() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL);
        let (s, _task) = mem.open(&who, viewer_config(PermissionSet::UNATTENDED | PermissionSet::CONTROL)).await;
        let mut s = s.unwrap();
        s.ctx().send(Elevate {}).await.unwrap();
        wait_event(&mut s, Duration::from_secs(1), "elevated", |e| {
            matches!(e, ViewerEvent::PermissionsChanged(g) if *g == PermissionSet::UNATTENDED | PermissionSet::CONTROL)
        })
        .await;
    });
}
