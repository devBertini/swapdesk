//! Stage 37: one reproducing test per finding of the stage-14 review of the session and video crates (the session half), written
//! to fail on the code the review was made against and to pass with its fix. The video half is in
//! `swapdesk-video` (`transport.rs` and the sequencer's unit tests); findings 4 and 13 need `HostInner::commit` and are
//! unit tests in `host.rs`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use swapdesk_proto::*;
use swapdesk_session::{
    ControlIo, EndKind, ForwardHandler, HostEvent, PromptAnswer, Registry, SessionEnd, VIDEO_CONTROL_TYPES, ViewerEvent,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const CF: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::FILES);

fn is_error(e: &ViewerEvent, code: ErrorCode, about_type: u16, about_id: u32) -> bool {
    matches!(e, ViewerEvent::Message(Message::Error(m)) if m.code == code && !m.fatal && m.about_type == about_type && m.about_id == about_id)
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

// ---------- finding 1: a prompted grant keeps stored UNATTENDED / ADMIN_TERMINAL the viewer never requested ----------

#[test]
fn f1_a_prompted_session_never_keeps_stored_unattended_or_admin_terminal() {
    run_paused(async {
        // The device holds both, but this viewer asked for neither (D10: its user is no administrator), so the host
        // prompts, and the host's user (the preset is the stored grants) accepts everything that is stored.
        let stored = PermissionSet::UNATTENDED
            | PermissionSet::ADMIN_TERMINAL
            | PermissionSet::CONTROL
            | PermissionSet::TERMINAL;
        let mem = Mem::new(TestUi::accepting(stored), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, stored);
        let (s, _t) = mem.open(&who, viewer_config(PermissionSet::CONTROL | PermissionSet::TERMINAL)).await;
        let s = s.unwrap();
        assert_eq!(s.grant().mode, GrantMode::Prompted);
        assert_eq!(
            s.grant().granted,
            PermissionSet::CONTROL | PermissionSet::TERMINAL,
            "a prompted session never holds UNATTENDED, nor ADMIN_TERMINAL the viewer did not request"
        );
    });
}

#[test]
fn f1_an_administrator_terminal_needs_the_grant_and_the_request() {
    run_paused(async {
        let all = PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL | PermissionSet::CONTROL;
        let rec = Recorder::new(&[0x0501], &[]);
        let ui = TestUi::new(Script::Answer(PromptAnswer::Decline));
        let mem = Mem::new(ui, registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, all);
        // A 1.0 viewer: its unattended entry holds ADMIN_TERMINAL at once (a viewer of minor 1 gets it with Elevate).
        let (a, _t) = mem.open(&who, viewer_config_1_0(all)).await;
        let a = a.unwrap();
        assert_eq!(a.grant().granted, all);
        let info = a.grant().clone();
        a.abandon();
        eventually(Duration::from_secs(1), "the loss", || mem.host.sessions().is_empty()).await;
        // The same device resumes, but this time its Hello does not ask for ADMIN_TERMINAL.
        let mut cfg = viewer_config_1_0(PermissionSet::CONTROL);
        cfg.resume = Some(info.resume.clone());
        let (r, _t) = mem.open(&who, cfg).await;
        let mut r = r.unwrap();
        assert_eq!(r.grant().mode, GrantMode::Resumed);
        r.ctx().send(TermOpen { term_id: 1, cols: 80, rows: 24, admin: true }).await.unwrap();
        wait_event(&mut r, Duration::from_secs(2), "NOT_PERMITTED for the administrator terminal", |e| {
            is_error(e, ErrorCode::NotPermitted, 0x0501, 1)
        })
        .await;
        assert_eq!(rec.count(), 0, "nothing was opened");
    });
}

// ---------- finding 2: Bye, grant changes and desktop events must not depend on a queue a peer can fill ----------

struct Flooded {
    mem: Mem,
    io: ControlIo,
    task: JoinHandle<SessionEnd>,
    id: u64,
    rec: Arc<Recorder>,
}

/// A viewer that sends `Ping`s and never reads: the host's replies fill its outgoing queue, and the pings fill the
/// state machine's event queue.
async fn flooded() -> Flooded {
    let rec = Recorder::new(&[], &[]);
    let mem = Mem::new(TestUi::accepting(CF), registry_one(rec.clone()));
    let who = Who::new();
    pin(&mem.trust, &who, CF);
    let (mut io, task) = mem.connect_raw(&who);
    raw_hello(&mut io, CF, None, None).await;
    let id = next_matching(&mut io, "SessionGrant", |m| {
        if let Message::SessionGrant(g) = m { Some(g.session_id) } else { None }
    })
    .await;
    let mut sent = 0u32;
    for _ in 0..6_000 {
        if io.out.try_send(Ping { nonce: sent, t_send_us: 0 }).is_ok() {
            sent += 1;
        }
        tokio::task::yield_now().await;
    }
    assert!(sent > 450, "only {sent} pings were accepted: the pipeline did not fill");
    Flooded { mem, io, task, id, rec }
}

#[test]
fn f2_a_disconnect_survives_a_full_event_queue_and_kills_the_token() {
    run_paused(async {
        let f = flooded().await;
        assert!(f.mem.host.disconnect(f.id));
        let Flooded { mem, io, task, .. } = f;
        // The viewer vanishes right after: the host must still have ended the session by its own Bye.
        drop(io);
        let end = task.await.unwrap();
        assert_eq!(
            end.kind,
            EndKind::LocalBye,
            "\"Desconectar\" was lost behind the peer's messages, so the session ended as a lost connection"
        );
        assert_eq!(mem.host.resumable_sessions(), 0, "and its resume token is still alive");
        assert!(mem.ui.events().iter().any(|e| matches!(e, HostEvent::Ended { resumable: false, .. })));
    });
}

#[test]
fn f2_a_grant_change_reaches_the_viewer_and_the_handlers_through_a_full_event_queue() {
    run_paused(async {
        let mut f = flooded().await;
        assert!(f.mem.host.set_session_grants(f.id, PermissionSet::CONTROL));
        // The viewer reads again.
        next_matching(&mut f.io, "PermissionsChanged", |m| {
            matches!(m, Message::PermissionsChanged(p) if p.granted == PermissionSet::CONTROL).then_some(())
        })
        .await;
        eventually(Duration::from_secs(5), "on_permissions", || !lock(&f.rec.perms).is_empty()).await;
    });
}

#[test]
fn f2_a_desktop_switch_reaches_the_viewer_through_a_full_event_queue() {
    run_paused(async {
        let mut f = flooded().await;
        f.mem.host.set_desktop(Desktop::Locked);
        next_matching(&mut f.io, "HostState", |m| {
            matches!(m, Message::HostState(h) if h.desktop == Desktop::Locked && h.paused).then_some(())
        })
        .await;
        assert!(lock(&f.rec.sessions)[0].is_paused());
    });
}

// ---------- finding 6: a non-fatal Error from the viewer is not a violation ----------

#[test]
fn f6_a_non_fatal_error_from_the_viewer_does_not_end_the_session() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::CONTROL, None, None).await;
        next_matching(&mut io, "SessionGrant", |m| matches!(m, Message::SessionGrant(_)).then_some(())).await;
        io.out
            .send(ErrorMessage { code: ErrorCode::Busy, fatal: false, about_type: 0x0301, about_id: 9 })
            .await
            .unwrap();
        io.out.send(Ping { nonce: 77, t_send_us: 1 }).await.unwrap();
        let pong = next_matching(&mut io, "the Pong after the Error", |m| {
            if let Message::Pong(p) = m { Some(p.nonce) } else { None }
        })
        .await;
        assert_eq!(pong, 77);
        assert!(!task.is_finished(), "the session is still live");
    });
}

// ---------- finding 7: the banner cannot widen a session to UNATTENDED or ADMIN_TERMINAL ----------

#[test]
fn f7_set_session_grants_never_adds_unattended_or_admin_terminal() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        // The viewer asks for everything the host's banner is tried with: the viewer holds the host's set within its request.
        let asked = CF | PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL;
        let (s, _t) = mem.open(&who, viewer_config(asked)).await;
        let mut s = s.unwrap();
        let id = s.grant().session_id;
        assert!(mem.host.set_session_grants(id, CF | PermissionSet::UNATTENDED | PermissionSet::ADMIN_TERMINAL));
        wait_event(
            &mut s,
            Duration::from_secs(1),
            "PermissionsChanged without the administrator bits",
            |e| matches!(e, ViewerEvent::PermissionsChanged(g) if *g == CF | PermissionSet::TERMINAL),
        )
        .await;
        // TERMINAL (implied by the ADMIN_TERMINAL asked for) may be added for a session; the two administrator bits may not.
        assert_eq!(s.granted(), CF | PermissionSet::TERMINAL);
    });
}

// ---------- finding 8: handlers hear their session in order, and nothing is started on a closing session ----------

#[test]
fn f8_a_stream_reaches_its_handler_after_on_start_even_when_the_start_is_slow() {
    run(async {
        let rec = Recorder::new(&[], &[StreamKind::Clipboard]);
        let ui = TestUi::accepting(PermissionSet::CONTROL | PermissionSet::CLIPBOARD);
        *lock(&ui.event_delay) = Some(Duration::from_millis(400));
        let q = Quic::start(ui, registry_one(rec.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::CONTROL | PermissionSet::CLIPBOARD, None);
        let s = q.open(&v, viewer_config(PermissionSet::CONTROL | PermissionSet::CLIPBOARD)).await.unwrap();
        let mut clip = s.ctx().open_uni(StreamKind::Clipboard).await.unwrap();
        clip.write_all(&[0; 12]).await.unwrap();
        eventually(Duration::from_secs(5), "the stream and the start", || {
            rec.streams.load(Ordering::SeqCst) == 1 && rec.started.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(lock(&rec.order).clone(), vec!["start", "stream"], "on_stream ran before on_start");
        q.stop();
    });
}

#[test]
fn f8_nothing_is_spawned_for_a_session_that_is_closing() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let mem = Mem::new(TestUi::accepting(CF), registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, CF);
        let (mut io, _task) = mem.connect_raw(&who);
        raw_hello(&mut io, CF, None, None).await;
        let id = next_matching(&mut io, "SessionGrant", |m| {
            if let Message::SessionGrant(g) = m { Some(g.session_id) } else { None }
        })
        .await;
        let ctx = lock(&rec.sessions)[0].clone();
        // The viewer reads nothing, so the host's last word (`Bye`) waits for room: the session is closing for a while.
        for _ in 0..2_000 {
            let _ = ctx.try_send(StreamStats { kbps: 0, fps: 0, encoder: String::new() });
            tokio::task::yield_now().await;
        }
        assert!(ctx.try_send(StreamStats { kbps: 0, fps: 0, encoder: String::new() }).is_err(), "the queue is full");
        assert!(mem.host.disconnect(id));
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(ctx.ended().is_none(), "the session is still saying goodbye");
        assert!(
            !ctx.spawn_gated(PermissionSet::EMPTY, async {}),
            "a task was started on a session that was already aborting its tasks, and nothing will stop it"
        );
    });
}

// ---------- finding 9: one pending decision at a time ----------

#[test]
fn f9_a_second_request_while_a_prompt_is_open_is_busy_without_a_second_prompt() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        let mem = Mem::new(ui.clone(), Registry::new());
        let (a, b) = (Who::new(), Who::new());
        pin(&mem.trust, &a, PermissionSet::VIEW);
        pin(&mem.trust, &b, PermissionSet::VIEW);
        let (mut io_a, _ta) = mem.connect_raw(&a);
        raw_hello(&mut io_a, PermissionSet::CONTROL, None, None).await;
        next_matching(&mut io_a, "PromptPending", |m| matches!(m, Message::PromptPending(_)).then_some(())).await;
        // Another device, and the same device again.
        let (second, _t) = mem.open(&b, viewer_config(PermissionSet::CONTROL)).await;
        assert_refused(second, ErrorCode::Busy);
        let (third, _t) = mem.open(&a, viewer_config(PermissionSet::CONTROL)).await;
        assert_refused(third, ErrorCode::Busy);
        assert_eq!(ui.prompt_count(), 1, "the user was asked once");
    });
}

// ---------- finding 10: Deciding has a deadline ----------

#[test]
fn f10_a_decision_that_never_arrives_ends_the_session_with_a_timeout() {
    run_paused(async {
        let ui = TestUi::new(Script::Hold);
        ui.panic_on_prompt.store(true, Ordering::SeqCst);
        let mem = Mem::new(ui, Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (mut io, task) = mem.connect_raw(&who);
        raw_hello(&mut io, PermissionSet::CONTROL, None, None).await;
        let t0 = Instant::now();
        let end = tokio::time::timeout(Duration::from_secs(60), task)
            .await
            .expect("the host session never ended: it waits for a decision that its crashed task will not send")
            .unwrap();
        assert_eq!(end.kind, EndKind::Timeout);
        assert!(t0.elapsed() <= Duration::from_secs(40), "ended after {:?}", t0.elapsed());
    });
}

// ---------- finding 11: a stream before the grant is a violation at once on the host ----------

#[test]
fn f11_a_stream_opened_while_the_prompt_is_open_is_a_violation_even_if_the_grant_follows() {
    run(async {
        let rec = Recorder::new(&[], &[StreamKind::Input]);
        let ui = TestUi::new(Script::Hold);
        let q = Quic::start(ui.clone(), registry_one(rec.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::CONTROL, None);
        let conn = swapdesk_trust::connect_session(&v.endpoint, &v.who.identity, q.addr, *q.host_identity.spki())
            .await
            .unwrap();
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        let hello = Hello {
            proto_minor: 0,
            app_version: "t".into(),
            name: "raw".into(),
            os: Os::MacOs,
            os_version: "15".into(),
            intent: Intent::Control,
            requested: PermissionSet::CONTROL,
            resume: None,
            swap_token: None,
        };
        send.write_all(&Message::from(hello).encode_frame().unwrap()).await.unwrap();
        send.write_all(&Message::from(capabilities()).encode_frame().unwrap()).await.unwrap();
        eventually(Duration::from_secs(3), "the prompt", || ui.prompt_count() == 1).await;
        let mut uni = conn.open_uni().await.unwrap();
        let mut bytes = StreamKind::Input.encode().unwrap();
        bytes.push(0x06);
        uni.write_all(&bytes).await.unwrap();
        // The user answers well within the 2 s a stream used to be allowed to wait for the grant.
        tokio::time::sleep(Duration::from_millis(300)).await;
        ui.answer_held(PromptAnswer::Accept(PermissionSet::CONTROL));
        assert_eq!(
            closed_with(&conn, "a stream before the grant").await,
            u64::from(ErrorCode::ProtocolViolation.tag())
        );
        assert_eq!(rec.streams.load(Ordering::SeqCst), 0, "the stream reached a handler");
        q.stop();
    });
}

// ---------- finding 12: a stored-grant change removes only what was revoked ----------

#[test]
fn f12_a_stored_change_keeps_the_per_session_additions_and_removes_only_what_was_revoked() {
    run_paused(async {
        // D8: the stored grant is VIEW; the host's user widened this session to CONTROL and FILES.
        let mem = Mem::new(TestUi::accepting(PermissionSet::VIEW | CF), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::VIEW);
        let (s, _t) = mem.open(&who, viewer_config(CF)).await;
        let s = s.unwrap();
        assert_eq!(s.granted(), CF);
        // An unrelated change in Permissões (TERMINAL added to what is stored) revokes nothing.
        set_stored(&mem.trust, &who, PermissionSet::VIEW | PermissionSet::TERMINAL);
        mem.host.stored_grants_changed(&who.id);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(s.granted(), CF, "nothing was revoked, but the session lost its per-session additions");
        // Revoking VIEW takes what implies it (CONTROL) and nothing else.
        set_stored(&mem.trust, &who, PermissionSet::TERMINAL);
        mem.host.stored_grants_changed(&who.id);
        eventually(Duration::from_secs(1), "the revocation", || s.granted() == PermissionSet::FILES).await;
    });
}

// ---------- finding 14: a locally closed connection is not a lost one ----------

#[test]
fn f14_a_connection_this_side_closes_is_a_local_end_not_a_lost_connection() {
    run(async {
        let q = Quic::start(TestUi::accepting(PermissionSet::CONTROL), Registry::new(), None, |_| {}).await;
        let v = q.viewer(PermissionSet::CONTROL, None);
        let s = q.open(&v, viewer_config(PermissionSet::CONTROL)).await.unwrap();
        s.ctx().connection().unwrap().close(quinn::VarInt::from_u32(0), b"");
        let end =
            tokio::time::timeout(Duration::from_secs(6), s.ctx().wait_ended()).await.expect("the session did not end");
        assert_eq!(
            end.kind,
            EndKind::LocalBye,
            "closed on purpose from here, yet treated as a lost connection (resumable)"
        );
        q.stop();
    });
}

// ---------- finding 15: dropping a ViewerSession ends it ----------

#[test]
fn f15_dropping_a_viewer_session_says_bye_and_stops_its_tasks() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(CF), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, CF);
        let (s, task) = mem.open(&who, viewer_config(CF)).await;
        let s = s.unwrap();
        let ctx = s.ctx().clone();
        let ticks = Arc::new(AtomicUsize::new(0));
        ctx.spawn({
            let ticks = ticks.clone();
            async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(s);
        let end = tokio::time::timeout(Duration::from_secs(5), ctx.wait_ended())
            .await
            .expect("the dropped viewer session is still running");
        assert_eq!(end.kind, EndKind::LocalBye);
        let host_end =
            tokio::time::timeout(Duration::from_secs(5), task).await.expect("the host never saw the end").unwrap();
        assert_eq!(
            (host_end.kind, host_end.code),
            (EndKind::PeerBye, ErrorCode::NoError),
            "the host saw a lost connection (resumable) instead of a Bye"
        );
        let seen = ticks.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), seen, "a task of the session kept running");
    });
}

// ---------- finding 16: the viewer gates the streams the host opens ----------

#[test]
fn f16_the_viewer_stops_a_clipboard_stream_it_was_not_granted() {
    run(async {
        let host_rec = Recorder::new(&[], &[]);
        let viewer_rec = Recorder::new(&[], &[StreamKind::Clipboard]);
        let q =
            Quic::start(TestUi::accepting(PermissionSet::CONTROL), registry_one(host_rec.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::CONTROL, None);
        let mut cfg = viewer_config(PermissionSet::CONTROL);
        cfg.registry = registry_one(viewer_rec.clone());
        let _s = q.open(&v, cfg).await.unwrap();
        eventually(Duration::from_secs(3), "the host session", || !lock(&host_rec.sessions).is_empty()).await;
        let host_ctx = lock(&host_rec.sessions)[0].clone();
        let mut stream = host_ctx.open_uni(StreamKind::Clipboard).await.unwrap();
        // The viewer may stop the stream while the bytes are still being written (seen under load): the refusal is
        // the same either way.
        let code = match stream.write_all(&[0; 12]).await {
            Err(quinn::WriteError::Stopped(c)) => Some(c),
            Err(e) => panic!("writing the clipboard stream: {e}"),
            Ok(()) => tokio::time::timeout(Duration::from_secs(3), stream.stopped())
                .await
                .expect("the viewer answers")
                .unwrap(),
        };
        assert_eq!(code.map(|c| c.into_inner()), Some(u64::from(ErrorCode::NotPermitted.tag())));
        assert_eq!(
            viewer_rec.streams.load(Ordering::SeqCst),
            0,
            "the stream reached the handler of a viewer that has no CLIPBOARD"
        );
        q.stop();
    });
}

// ---------- finding 18: no token or message body in Debug output ----------

#[test]
fn f18_debug_output_carries_no_token_and_no_message_body() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(CF), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, CF);
        let (s, _t) = mem.open(&who, viewer_config(CF)).await;
        let s = s.unwrap();
        let info = s.grant().clone();
        let dump = format!("{info:?}");
        let token = format!("{:?}", info.resume.token);
        assert!(!dump.contains(&token), "the resume token is in the Debug output: {dump}");
        let chat = ViewerEvent::Message(Message::ChatMessage(ChatMessage {
            msg_id: 1,
            text: "a private sentence".into(),
            sent_at_ms: 0,
        }));
        assert!(!format!("{chat:?}").contains("private"), "a chat body is in the Debug output");
    });
}

// ---------- finding 19: queued forwarded messages are re-checked ----------

#[test]
fn f19_a_forwarded_message_is_not_permitted_after_its_grant_was_revoked() {
    run_paused(async {
        let (handler, mut rx) = ForwardHandler::new("video-control", VIDEO_CONTROL_TYPES, 16);
        let mem = Mem::new(TestUi::accepting(PermissionSet::CONTROL), registry_one(handler));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::CONTROL);
        let (s, _t) = mem.open(&who, viewer_config(PermissionSet::CONTROL)).await;
        let s = s.unwrap();
        s.ctx().send(KeyframeRequest { display: 0, last_good_frame: 1 }).await.unwrap();
        let first = rx.recv().await.unwrap();
        assert!(first.still_permitted(), "VIEW is granted");
        // A second request waits in the queue while the host's user takes VIEW away.
        s.ctx().send(KeyframeRequest { display: 0, last_good_frame: 2 }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(mem.host.set_session_grants(s.grant().session_id, PermissionSet::FILES));
        let second = rx.recv().await.unwrap();
        assert!(!second.still_permitted(), "the consumer would act on a request the session no longer allows");
    });
}

// ---------- finding 20 (a) and (d) ----------

#[test]
fn f20a_a_request_nobody_handles_does_not_take_a_cap_slot() {
    run_paused(async {
        let mem = Mem::new(TestUi::accepting(PermissionSet::TERMINAL), Registry::new());
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::TERMINAL);
        let (s, _t) = mem.open(&who, viewer_config(PermissionSet::TERMINAL)).await;
        let mut s = s.unwrap();
        // No terminal feature is installed: each is UNAVAILABLE, and none may use up one of the 4 slots.
        for id in 1..=6 {
            s.ctx().send(TermOpen { term_id: id, cols: 80, rows: 24, admin: false }).await.unwrap();
            wait_event(&mut s, Duration::from_secs(1), "UNAVAILABLE", |e| {
                is_error(e, ErrorCode::Unavailable, 0x0501, id)
            })
            .await;
        }
    });
}

#[test]
fn f20d_a_consent_card_is_closed_when_the_question_is_cancelled() {
    run_paused(async {
        let rec = Recorder::new(&[], &[]);
        let ui = TestUi::new(Script::Hold);
        ui.answer_consent_with(None);
        let mem = Mem::new(ui.clone(), registry_one(rec.clone()));
        let who = Who::new();
        pin(&mem.trust, &who, PermissionSet::UNATTENDED | PermissionSet::FILES);
        let (s, _t) = mem.open(&who, viewer_config(PermissionSet::UNATTENDED | PermissionSet::FILES)).await;
        let _s = s.unwrap();
        let ctx = lock(&rec.sessions)[0].clone();
        let asking = tokio::spawn({
            let ctx = ctx.clone();
            async move { ctx.request_consent(Feature::Files).await }
        });
        eventually(Duration::from_secs(1), "the consent card", || !lock(&ui.consents).is_empty()).await;
        // The session task that asked is aborted (a revocation, the end of the session).
        asking.abort();
        let _ = asking.await;
        eventually(Duration::from_secs(1), "the card to be closed", || !lock(&ui.consents_closed).is_empty()).await;
        assert_eq!(lock(&ui.consents_closed).clone(), vec![swapdesk_session::PromptEnd::Cancelled]);
    });
}

// ---------- found while proving the flaky tests: a stale stream reset before its kind is not a violation ----------

#[test]
fn a_frame_stream_the_peer_resets_before_its_kind_arrives_does_not_end_the_session() {
    run(async {
        let host_rec = Recorder::new(&[], &[]);
        let q =
            Quic::start(TestUi::accepting(PermissionSet::CONTROL), registry_one(host_rec.clone()), None, |_| {}).await;
        let v = q.viewer(PermissionSet::CONTROL, None);
        let s = q.open(&v, viewer_config(PermissionSet::CONTROL)).await.unwrap();
        eventually(Duration::from_secs(3), "the host session", || !lock(&host_rec.sessions).is_empty()).await;
        let host_ctx = lock(&host_rec.sessions)[0].clone();
        // The host resets a stale video frame (a newer keyframe, a pause): the stream can be gone before its first byte
        // was read. That is the sender doing what PROTOCOL.md 4 tells it to.
        for _ in 0..20 {
            let mut stream = host_ctx.connection().unwrap().open_uni().await.unwrap();
            stream.reset(quinn::VarInt::from_u32(0x1001)).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            s.ctx().is_active() && host_ctx.is_active(),
            "the session ended: {:?} / {:?}",
            s.ctx().ended(),
            host_ctx.ended()
        );
        q.stop();
    });
}
