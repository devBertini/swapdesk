//! Clipboard sync and quick actions through the proxies (stage 42, F-CLIP-01, F-ACT-01): two `ServiceCore`s on
//! 127.0.0.1, the host's user helper a real `run_helper` loop over in-memory pipes with `FakeClipboard` and
//! `FakeActions`, the viewer's UI a scripted `Rpc` answering with another `FakeClipboard`.

use std::sync::OnceLock;
use std::sync::mpsc;

use swapdesk_fakes::{FakeActions, FakeClipboard};
use swapdesk_platform::{ClipContent, ErrorKind};
use swapdesk_proto::{Action, ClipSkip};
use swapdesk_ui::service_backend::clipboard::answer_clipboard;

use super::*;
use crate::rpc::Rpc;
use crate::service::ClipActionHooks;

const WITH_CLIPBOARD: PermissionSet = ALL.union(PermissionSet::CLIPBOARD);

fn text(s: &str) -> ClipContent {
    ClipContent::Text(s.into())
}

/// The viewer's UI: the service's clipboard calls arrive on this pipe and a thread answers them with `clip`.
fn fake_ui(clip: FakeClipboard) -> Arc<Rpc> {
    let (tx, rx) = mpsc::channel::<Ipc>();
    let cell: Arc<OnceLock<std::sync::Weak<Rpc>>> = Arc::default();
    let rpc = Rpc::new(move |m| tx.send(m).is_ok());
    let _ = cell.set(Arc::downgrade(&rpc));
    std::thread::spawn(move || {
        let mut clip = clip;
        while let Ok(msg) = rx.recv() {
            let Some(reply) = answer_clipboard(&mut clip, &msg) else { continue };
            match cell.get().and_then(std::sync::Weak::upgrade) {
                Some(rpc) => {
                    rpc.resolve(reply);
                }
                None => break,
            }
        }
    });
    rpc
}

/// A machine's interface for the clipboard and the actions: its UI's pipe, what it was told.
struct Scripted {
    ui: Option<Arc<Rpc>>,
    skipped: Mutex<Vec<(u64, ClipSkip)>>,
    ran: Mutex<Vec<(u64, Action)>>,
}

impl Scripted {
    fn new(ui: Option<Arc<Rpc>>) -> Arc<Self> {
        Arc::new(Self { ui, skipped: Mutex::default(), ran: Mutex::default() })
    }
}

impl ClipActionHooks for Scripted {
    fn ui_clipboard(&self, _session_id: u64) -> Option<Arc<Rpc>> {
        self.ui.clone()
    }

    fn clipboard_skipped(&self, session_id: u64, reason: ClipSkip) {
        lock(&self.skipped).push((session_id, reason));
    }

    fn action_starting(&self, session_id: u64, device: Option<DeviceId>, action: Action) {
        assert!(device.is_some(), "a hosted session always names its peer");
        lock(&self.ran).push((session_id, action));
    }
}

/// The host's user helper: `run_helper` with the fakes behind the clipboard and the actions.
fn user_helper(core: &ServiceCore, clip: &FakeClipboard, actions: &FakeActions) -> JoinHandle<()> {
    let (core_end, helper_end): (End, End) = duplex_blocking();
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let audio_clock = Arc::clone(&clock);
    let (clip, actions) = (clip.clone(), actions.clone());
    let deps = HelperDeps::new(
        |_| Ok(SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime)),
        |_frame, _config, _backend| Ok(Passthrough { n: 0, size: None }),
        || Ok(RecordingInputSink::new()),
        move || Ok(SineSource::new(audio_clock.clone()).realtime()),
        clock,
    )
    .with_clipboard(move || Ok(clip.clone()))
    .with_actions(move || Ok(actions.clone()));
    let thread = std::thread::spawn(move || {
        let _ = run_helper(helper_end.reader, helper_end.writer, deps, IpcRole::HelperUser, 4242);
    });
    core.attach_helper(IpcRole::HelperUser, core_end.reader, core_end.writer).expect("attach");
    thread
}

/// Reads what the viewer's channel delivers so a full queue never holds the session's events.
fn drain(mut msgs: ViewerMessages) {
    std::thread::spawn(move || {
        loop {
            if let Ok(None) = msgs.recv_timeout(Duration::from_millis(100)) {
                break;
            }
        }
    });
}

fn pause(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

#[test]
fn a_copy_crosses_both_ways_through_the_helper_and_the_ui_and_the_switch_stops_it() {
    let host = Machine::build("host-pc", true, false, WITH_CLIPBOARD, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, WITH_CLIPBOARD);
    let (host_clip, viewer_clip) = (FakeClipboard::new(), FakeClipboard::new());
    viewer_clip.copy_externally(text("copied before the session"), false);
    let (host_hooks, viewer_hooks) = (Scripted::new(None), Scripted::new(Some(fake_ui(viewer_clip.clone()))));
    assert!(host.core.set_clip_actions(host_hooks.clone()));
    assert!(viewer.core.set_clip_actions(viewer_hooks.clone()));
    assert!(!viewer.core.set_clip_actions(Scripted::new(None)), "the hooks are set once");
    let _helper = user_helper(&host.core, &host_clip, &FakeActions::new());

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    let session = handle.session_id();
    assert!(handle.granted().contains(PermissionSet::CLIPBOARD), "the viewer asked for it and the host's user gave it");

    // Viewer to host: the copy is read through the UI's pipe and written through the user helper.
    // What the viewer had copied before the session is offered when the session starts.
    wait_for("the viewer's earlier copy at the host", || {
        host_clip.writes().contains(&text("copied before the session")).then_some(())
    });
    wait_for("the viewer's copy at the host", || {
        viewer_clip.copy_externally(text("from the viewer"), false);
        pause(300);
        host_clip.writes().contains(&text("from the viewer")).then_some(())
    });

    // Host to viewer: read in the helper, written through the UI's pipe.
    let back = wait_for("the host's copy at the viewer", || {
        host_clip.copy_externally(text("from the host"), false);
        pause(300);
        viewer_clip.writes().first().cloned()
    });
    assert_eq!(back, text("from the host"));

    // "Copiar e colar" off at the viewer: nothing it copies goes out, and it can be turned on again.
    assert!(viewer.core.clipboard_sync_on(session));
    viewer.core.set_clipboard_sync(session, false).expect("a live session");
    assert!(!viewer.core.clipboard_sync_on(session));
    pause(600);
    let seen = host_clip.writes().len();
    viewer_clip.copy_externally(text("while off"), false);
    pause(1000);
    assert_eq!(host_clip.writes().len(), seen, "no write at the host while the switch is off");
    viewer.core.set_clipboard_sync(session, true).expect("a live session");
    wait_for("the copy after the switch is on again", || {
        viewer_clip.copy_externally(text("on again"), false);
        pause(300);
        host_clip.writes().contains(&text("on again")).then_some(())
    });
    assert!(!host_clip.writes().contains(&text("while off")), "what was copied while off is not sent later");
    assert_eq!(viewer.core.set_clipboard_sync(session + 1, true), Err(ApplyError::NoSuchSession));

    // A concealed copy is never sent: both interfaces hear that it was skipped (here and at the peer).
    viewer_clip.copy_externally(text("hunter2"), true);
    wait_for("the skip notices", || {
        (!lock(&viewer_hooks.skipped).is_empty() && !lock(&host_hooks.skipped).is_empty()).then_some(())
    });
    assert_eq!(lock(&viewer_hooks.skipped)[0], (session, ClipSkip::Concealed));
    assert_eq!(lock(&host_hooks.skipped)[0], (session, ClipSkip::Concealed));
    assert!(!host_clip.writes().contains(&text("hunter2")));

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

#[test]
fn a_quick_action_runs_in_the_user_helper_and_is_audited() {
    let host = Machine::build("host-pc", true, false, WITH_CLIPBOARD, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, WITH_CLIPBOARD);
    let host_hooks = Scripted::new(None);
    host.core.set_clip_actions(host_hooks.clone());
    let (clip, actions) = (FakeClipboard::new(), FakeActions::new());
    actions.refuse(Action::Restart, ErrorKind::Unsupported);
    let _helper = user_helper(&host.core, &clip, &actions);

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    let rt = viewer.core.runtime();
    assert_eq!(rt.block_on(handle.run_action(Action::Lock)), Ok(ErrorCode::NoError));
    assert_eq!(actions.ran(), vec![Action::Lock], "the helper ran it, as the user");
    // A refusal is the answer, not a crash; the attempt was announced to the audit before the runner refused.
    assert_eq!(rt.block_on(handle.run_action(Action::Restart)), Ok(ErrorCode::NotPermitted));
    assert_eq!(actions.ran(), vec![Action::Lock]);
    assert_eq!(
        *lock(&host_hooks.ran),
        vec![(handle.session_id(), Action::Lock), (handle.session_id(), Action::Restart)]
    );

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

#[test]
fn with_no_user_helper_the_service_runs_the_action_itself_and_refuses_what_needs_a_user() {
    let service_actions = FakeActions::new();
    service_actions.refuse(Action::TaskMonitor, ErrorKind::Unsupported);
    let runner = service_actions.clone();
    let host = Machine::build("host-pc", true, false, WITH_CLIPBOARD, move |c| c.with_service_actions(runner));
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, WITH_CLIPBOARD);
    let host_hooks = Scripted::new(None);
    host.core.set_clip_actions(host_hooks.clone());

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    let rt = viewer.core.runtime();
    assert_eq!(rt.block_on(handle.run_action(Action::Sleep)), Ok(ErrorCode::NoError));
    assert_eq!(rt.block_on(handle.run_action(Action::TaskMonitor)), Ok(ErrorCode::NotPermitted));
    assert_eq!(service_actions.ran(), vec![Action::Sleep]);
    // The refused Task Manager was announced too: the entry comes before the runner's answer.
    assert_eq!(
        *lock(&host_hooks.ran),
        vec![(handle.session_id(), Action::Sleep), (handle.session_id(), Action::TaskMonitor)]
    );

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

#[test]
fn a_view_only_session_may_not_ask_for_an_action_and_without_a_helper_or_runner_it_is_unavailable() {
    let host = Machine::build("host-pc", true, false, PermissionSet::VIEW, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, PermissionSet::VIEW);
    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    let rt = viewer.core.runtime();
    assert!(!handle.granted().contains(PermissionSet::CONTROL));
    assert_eq!(
        rt.block_on(handle.run_action(Action::Lock)),
        Err(ApplyError::NotPermitted),
        "refused at the viewer, before anything is sent"
    );
    drop(handle);

    // CONTROL granted, but neither a user helper nor a service runner: the host answers `UNAVAILABLE`.
    let host2 = Machine::build("host-pc-2", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host2, &viewer, WITH_CLIPBOARD);
    let (handle, msgs) = viewer.core.connect(host2.addr(), host2.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    assert_eq!(rt.block_on(handle.run_action(Action::Lock)), Ok(ErrorCode::Unavailable));
    drop(handle);
    host.core.shutdown();
    host2.core.shutdown();
    viewer.core.shutdown();
}

#[test]
fn what_the_host_had_copied_before_the_session_is_offered_to_the_viewer_when_it_starts() {
    let host = Machine::build("host-pc", true, false, WITH_CLIPBOARD, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, WITH_CLIPBOARD);
    let (host_clip, viewer_clip) = (FakeClipboard::new(), FakeClipboard::new());
    host_clip.copy_externally(text("host copy from before"), false);
    assert!(host.core.set_clip_actions(Scripted::new(None)));
    assert!(viewer.core.set_clip_actions(Scripted::new(Some(fake_ui(viewer_clip.clone())))));
    let _helper = user_helper(&host.core, &host_clip, &FakeActions::new());

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    wait_for("the host's earlier copy at the viewer", || {
        viewer_clip.writes().contains(&text("host copy from before")).then_some(())
    });
    pause(800);
    assert_eq!(viewer_clip.writes(), vec![text("host copy from before")], "offered once, never echoed back");
    assert!(host_clip.writes().is_empty(), "nothing was written at the host");

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

#[test]
fn a_session_without_the_clipboard_permission_offers_what_was_copied_to_nobody() {
    let host = Machine::build("host-pc", true, false, PermissionSet::CONTROL, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, PermissionSet::CONTROL);
    let (host_clip, viewer_clip) = (FakeClipboard::new(), FakeClipboard::new());
    host_clip.copy_externally(text("host copy"), false);
    viewer_clip.copy_externally(text("viewer copy"), false);
    host.core.set_clip_actions(Scripted::new(None));
    viewer.core.set_clip_actions(Scripted::new(Some(fake_ui(viewer_clip.clone()))));
    let _helper = user_helper(&host.core, &host_clip, &FakeActions::new());

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    assert!(!handle.granted().contains(PermissionSet::CLIPBOARD));
    pause(1500);
    assert!(host_clip.writes().is_empty() && viewer_clip.writes().is_empty(), "nothing crossed");

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

#[test]
fn turning_the_sync_on_in_a_session_offers_the_current_clipboard_once() {
    let host = Machine::build("host-pc", true, false, WITH_CLIPBOARD, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, WITH_CLIPBOARD);
    let (host_clip, viewer_clip) = (FakeClipboard::new(), FakeClipboard::new());
    host.core.set_clip_actions(Scripted::new(None));
    viewer.core.set_clip_actions(Scripted::new(Some(fake_ui(viewer_clip.clone()))));
    let _helper = user_helper(&host.core, &host_clip, &FakeActions::new());

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    let session = handle.session_id();
    viewer.core.set_clipboard_sync(session, false).expect("a live session");
    pause(600);
    viewer_clip.copy_externally(text("copied while off"), false);
    pause(800);
    assert!(host_clip.writes().is_empty());
    viewer.core.set_clipboard_sync(session, true).expect("a live session");
    wait_for("the copy made while off, offered on turning on", || {
        host_clip.writes().contains(&text("copied while off")).then_some(())
    });
    pause(800);
    assert_eq!(host_clip.writes(), vec![text("copied while off")], "once");

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

/// Security review of 13/11 (F2): a viewer that asked only to see holds only that, whatever the host's user granted
/// beyond the request (D8): what it had copied is not sent, what the host copies is not written, no input goes out.
#[test]
fn a_view_request_sends_no_clipboard_and_no_input_when_the_host_grants_more() {
    let host = Machine::build("host-pc", true, false, WITH_CLIPBOARD, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, WITH_CLIPBOARD);
    let (host_clip, viewer_clip) = (FakeClipboard::new(), FakeClipboard::new());
    host_clip.copy_externally(text("host copy"), false);
    viewer_clip.copy_externally(text("viewer secret"), false);
    host.core.set_clip_actions(Scripted::new(None));
    viewer.core.set_clip_actions(Scripted::new(Some(fake_ui(viewer_clip.clone()))));
    let _helper = user_helper(&host.core, &host_clip, &FakeActions::new());

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::View).expect("the session opens");
    drain(msgs);
    pause(1500);
    viewer_clip.copy_externally(text("viewer secret 2"), false);
    pause(1000);
    assert!(host_clip.writes().is_empty(), "nothing of the viewer's clipboard reached the host");
    assert!(viewer_clip.writes().is_empty(), "nothing of the host's clipboard was written at the viewer");
    assert_eq!(
        handle.send_input(vec![swapdesk_proto::InputEvent::Text("x".into())]),
        Err(ApplyError::NotPermitted),
        "no input leaves a View viewer"
    );
    assert!(
        !handle.granted().contains(PermissionSet::CLIPBOARD) && !handle.granted().contains(PermissionSet::CONTROL),
        "the viewer holds what it asked for, not the extra the host's user granted"
    );

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}

/// Security review of 13/11 (F1): one clipboard item is in flight per session, the newest wins. A peer that declares
/// 16 MiB and goes quiet holds the slot only until the next item, which stops the older stream and is applied.
#[test]
fn a_second_clipboard_item_supersedes_a_stalled_first_one() {
    let host = Machine::build("host-pc", true, false, WITH_CLIPBOARD, |c| c);
    let viewer = Machine::build("viewer-pc", true, false, WITH_CLIPBOARD, |c| c);
    Machine::pair(&host, &viewer, WITH_CLIPBOARD);
    let (host_clip, viewer_clip) = (FakeClipboard::new(), FakeClipboard::new());
    host.core.set_clip_actions(Scripted::new(None));
    viewer.core.set_clip_actions(Scripted::new(Some(fake_ui(viewer_clip.clone()))));
    let _helper = user_helper(&host.core, &host_clip, &FakeActions::new());

    let (handle, msgs) = viewer.core.connect(host.addr(), host.device(), Intent::Control).expect("the session opens");
    drain(msgs);
    let ctx = handle.ctx();
    let rt = viewer.core.runtime();
    let open = |item_bytes: Vec<u8>, finish: bool| {
        rt.block_on(async {
            let mut send = ctx.open_uni(swapdesk_proto::StreamKind::Clipboard).await.expect("a clipboard stream");
            send.write_all(&item_bytes).await.expect("written");
            if finish {
                send.finish().expect("finished");
            }
            send
        })
    };
    // The first item declares 16 MiB and sends its header and a little more.
    let mut stalled = swapdesk_proto::ClipboardItem {
        seq: 1,
        payload: swapdesk_proto::ClipboardPayload::Png(vec![7; swapdesk_proto::MAX_CLIPBOARD_PNG as usize]),
    }
    .encode()
    .unwrap();
    stalled.truncate(swapdesk_proto::CLIPBOARD_HEADER_LEN + 100);
    let first = open(stalled, false);
    pause(300);
    assert!(host_clip.writes().is_empty());
    // The second one is whole: it is applied, and the first is stopped by the receiver.
    let second =
        swapdesk_proto::ClipboardItem { seq: 2, payload: swapdesk_proto::ClipboardPayload::Text("newest".into()) };
    let _second = open(second.encode().unwrap(), true);
    wait_for("the newest item at the host", || host_clip.writes().contains(&text("newest")).then_some(()));
    let stopped = rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), first.stopped()).await });
    assert!(matches!(stopped, Ok(Ok(Some(_)))), "the older stream was stopped by the receiver: {stopped:?}");
    assert_eq!(host_clip.writes(), vec![text("newest")], "only the newest item was written");

    drop(handle);
    host.core.shutdown();
    viewer.core.shutdown();
}
