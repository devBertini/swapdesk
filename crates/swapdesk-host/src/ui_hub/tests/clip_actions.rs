//! Clipboard sync and quick actions through two hubs (stage 42): the UI ops `SetClipboardSync` and `RunAction`, the
//! viewer UI answering the service's clipboard calls on its own pipe, the `ClipboardSkipped` notification and the
//! audit of an action, with the host's user helper a real `run_helper` loop over the fakes.

use swapdesk_fakes::{FakeActions, FakeClipboard};
use swapdesk_ipc::ClipReplyBody;
use swapdesk_platform::{ClipContent, ErrorKind};
use swapdesk_proto::Action;
use swapdesk_ui::service_backend::clipboard::answer_clipboard;

use super::*;

const SET_SYNC_OP: u16 = 28;
const RUN_ACTION_OP: u16 = 29;
const WITH_CLIPBOARD: PermissionSet = ALL.union(PermissionSet::CLIPBOARD);

fn text(s: &str) -> ClipContent {
    ClipContent::Text(s.into())
}

fn attach_user_helper(m: &mut Machine, clip: &FakeClipboard, actions: &FakeActions) {
    let (core_end, helper_end) = duplex_blocking();
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let audio_clock = Arc::clone(&clock);
    let (clip, actions) = (clip.clone(), actions.clone());
    let deps = HelperDeps::new(
        |_| Ok(SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime)),
        |_frame, _config, _backend| Ok(Passthrough { n: 0 }),
        || Ok(RecordingInputSink::new()),
        move || Ok(SineSource::new(audio_clock.clone()).realtime()),
        clock,
    )
    .with_clipboard(move || Ok(clip.clone()))
    .with_actions(move || Ok(actions.clone()));
    let thread = std::thread::spawn(move || {
        let _ = run_helper(helper_end.reader, helper_end.writer, deps, IpcRole::HelperUser, 4242);
    });
    m.core.attach_helper(IpcRole::HelperUser, core_end.reader, core_end.writer).expect("attach");
    m.helpers.push((RecordingInputSink::new(), thread));
}

/// The viewer UI's side of the clipboard: answers the service's calls on its pipe with `clip` for `window`.
fn serve_clipboard(ui: &mut Client, clip: &mut FakeClipboard, window: Duration) {
    let end = Instant::now() + window;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match ui.inbox.recv_timeout(left.min(Duration::from_millis(50))) {
            Ok(m) => match answer_clipboard(clip, &m) {
                Some(reply) => ui.send(&reply),
                None => ui.backlog.push(m),
            },
            Err(RecvTimeoutError::Timeout) if Instant::now() < end => {}
            Err(_) => return,
        }
    }
}

struct Pair {
    host: Machine,
    viewer: Machine,
    host_ui: Client,
    viewer_ui: Client,
    session: u64,
}

fn open(actions: &FakeActions, host_clip: &FakeClipboard) -> Pair {
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, WITH_CLIPBOARD);
    viewer.pin(&host, PermissionSet::EMPTY);
    attach_user_helper(&mut host, host_clip, actions);
    viewer.sees(&host, false);
    host.hub.set_console_session(Some(1));
    let mut host_ui = host.client(info(false));
    let mut viewer_ui = viewer.client(info(false));
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let incoming = host_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    assert!(
        host_ui
            .call(UiOp::AnswerIncoming {
                request_id: incoming.request_id,
                decision: Decision::Accept,
                grants: WITH_CLIPBOARD.bits()
            })
            .is_ok()
    );
    let session = viewer_ui.connected(req);
    Pair { host, viewer, host_ui, viewer_ui, session }
}

#[test]
fn the_viewer_ui_serves_its_clipboard_to_the_service_and_the_switch_and_notices_travel_the_pipe() {
    let (actions, host_clip, mut viewer_clip) = (FakeActions::new(), FakeClipboard::new(), FakeClipboard::new());
    let mut p = open(&actions, &host_clip);
    let session = p.session;

    // The switch is this connection's to flip, on a session it owns; any other session is not found.
    assert!(matches!(p.viewer_ui.call(UiOp::SetClipboardSync { session_id: session, on: true }), Ok(ReplyBody::Done)));
    assert_eq!(
        p.viewer_ui.call(UiOp::SetClipboardSync { session_id: session + 1, on: true }).err(),
        Some(ApiCode::NotFound)
    );
    assert_eq!(
        p.viewer_ui.call(UiOp::RunAction { session_id: session + 1, action: Action::Lock }).err(),
        Some(ApiCode::NotFound)
    );
    assert_eq!(SET_SYNC_OP, UiOp::SetClipboardSync { session_id: 0, on: true }.code());
    assert_eq!(RUN_ACTION_OP, UiOp::RunAction { session_id: 0, action: Action::Lock }.code());

    // A copy at the viewer UI reaches the host's clipboard through the helper.
    let end = Instant::now() + WAIT;
    while host_clip.writes().is_empty() {
        assert!(Instant::now() < end, "the copy never reached the host");
        viewer_clip.copy_externally(text("do viewer"), false);
        serve_clipboard(&mut p.viewer_ui, &mut viewer_clip, Duration::from_millis(400));
    }
    assert_eq!(host_clip.writes()[0], text("do viewer"));
    // And the other way, written into the UI's clipboard.
    let end = Instant::now() + WAIT;
    while viewer_clip.writes().is_empty() {
        assert!(Instant::now() < end, "the copy never reached the viewer");
        host_clip.copy_externally(text("do host"), false);
        serve_clipboard(&mut p.viewer_ui, &mut viewer_clip, Duration::from_millis(400));
    }
    assert_eq!(viewer_clip.writes()[0], text("do host"));

    // A concealed copy is skipped, and both UIs are told why (a rejection code for "marked private").
    viewer_clip.copy_externally(text("hunter2"), true);
    serve_clipboard(&mut p.viewer_ui, &mut viewer_clip, Duration::from_millis(800));
    let at_viewer = p.viewer_ui.notify("the skip at the viewer", |n| match n {
        Notify::ClipboardSkipped { session_id, reason } if *session_id == session => Some(*reason),
        _ => None,
    });
    assert_eq!(at_viewer, ApiCode::NotPermitted);
    let at_host = p.host_ui.notify("the peer's skip at the host", |n| match n {
        Notify::ClipboardSkipped { session_id, reason } if *session_id == session => Some(*reason),
        _ => None,
    });
    assert_eq!(at_host, ApiCode::NotPermitted);
    assert!(!host_clip.writes().contains(&text("hunter2")));

    // The host's own user flips its side of the same session.
    assert!(matches!(p.host_ui.call(UiOp::SetClipboardSync { session_id: session, on: false }), Ok(ReplyBody::Done)));

    drop((p.host_ui, p.viewer_ui));
    p.viewer.finish();
    p.host.finish();
}

#[test]
fn run_action_replies_when_the_host_did_and_the_action_is_audited_at_the_host() {
    let (actions, host_clip) = (FakeActions::new(), FakeClipboard::new());
    actions.refuse(Action::Restart, ErrorKind::Unsupported);
    let mut p = open(&actions, &host_clip);
    let session = p.session;

    assert!(matches!(
        p.viewer_ui.call(UiOp::RunAction { session_id: session, action: Action::Lock }),
        Ok(ReplyBody::Done)
    ));
    assert_eq!(actions.ran(), vec![Action::Lock]);
    wait_for("the action in the host's audit", || {
        p.host.audit.entries().iter().any(|e| e.event == AuditEvent::ActionRun(Action::Lock)).then_some(())
    });
    // Refused by the system at the host: the reply says so and nothing is audited as run.
    assert_eq!(
        p.viewer_ui.call(UiOp::RunAction { session_id: session, action: Action::Restart }).err(),
        Some(ApiCode::NotPermitted)
    );
    assert_eq!(p.host.audit.entries().iter().filter(|e| matches!(e.event, AuditEvent::ActionRun(_))).count(), 1);
    // A view-only choice of this UI covers actions too.
    assert!(p.viewer_ui.call(UiOp::SetViewOnly { session_id: session, on: true }).is_ok());
    assert_eq!(
        p.viewer_ui.call(UiOp::RunAction { session_id: session, action: Action::Sleep }).err(),
        Some(ApiCode::NotPermitted)
    );
    assert_eq!(actions.ran(), vec![Action::Lock]);
    // The host's console UI does not own a viewer session: it cannot ask for an action.
    assert_eq!(
        p.host_ui.call(UiOp::RunAction { session_id: session, action: Action::Lock }).err(),
        Some(ApiCode::NotFound)
    );

    drop((p.host_ui, p.viewer_ui));
    p.viewer.finish();
    p.host.finish();
}

#[test]
fn a_late_clipboard_reply_is_dropped_quietly_and_a_clipboard_request_from_a_ui_ends_the_connection() {
    let m = Machine::new("host-pc");
    let mut ui = m.client(info(false));
    // A reply nobody waits for is dropped quietly (a late answer after a timeout).
    ui.send(&Ipc::ClipReply { req: 99, body: ClipReplyBody::Written });
    assert!(ui.call(UiOp::GetThisDevice).is_ok(), "the connection is still up");
    // A request type is the service's to send, not the UI's.
    ui.send(&Ipc::ClipRead { req: 1 });
    ui.take("the close", |m| matches!(m, Ipc::Bye { .. }).then_some(()));
    drop(ui);
    m.finish();
}
