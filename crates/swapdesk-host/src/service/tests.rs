//! The slice proof of 18d-1: two `ServiceCore`s in one process on 127.0.0.1 (the development option), paired by
//! writing each other into the trust stores, the host with real `run_helper` loops over in-memory pipes (the C7
//! synthetic sources) behind a spy that records what the service sent them.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_fakes::{
    FakeClock, FakeNetInterfaces, InputRecord, LogCapture, MemorySecretStore, MonotonicClock, Pacing,
    RecordingInputSink, SineSource, SyntheticScreen,
};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake, IpcRole, Message as Ipc, SessionState};
use swapdesk_platform::{Level, MonoMicros};
use swapdesk_proto::{ErrorCode, InputEvent, Intent, MouseButton, Os, PermissionSet, StreamKind, Wire};
use swapdesk_session::{ConsentRequest, HostUi, PromptAnswer, PromptRequest};
use swapdesk_trust::{DeviceId, Identity, PairedDevice, TrustHandle};
use tokio::sync::oneshot;

use super::{
    ApplyError, ConnectError, CoreEvent, HelperKind, ServiceConfig, ServiceCore, ServiceDeps, ViewerHandle,
    ViewerMessages, ViewerOut,
};
use crate::helper::{HelperDeps, run_helper};
use crate::helper_tests::Passthrough;
use crate::testpipe::{End, duplex_blocking};

mod clip_actions;
mod extras;
mod files;
mod terminal;

const WAIT: Duration = Duration::from_secs(10);
const ALL: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::AUDIO);

fn lo() -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The host's user, scripted: somebody is there and accepts every request with `grants`.
struct AutoUi {
    /// What the host's user picks in the card.
    grants: Mutex<PermissionSet>,
    prompts: AtomicUsize,
    /// What the user answers a D13 consent question (`None`: nobody answers), and how many were asked.
    consent_answer: Mutex<Option<bool>>,
    consents: AtomicUsize,
}

impl HostUi for AutoUi {
    fn somebody_present(&self) -> bool {
        true
    }

    fn prompt(&self, _request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(PromptAnswer::Accept(*lock(&self.grants)));
        rx
    }

    fn consent(&self, _request: ConsentRequest) -> oneshot::Receiver<bool> {
        self.consents.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        match *lock(&self.consent_answer) {
            Some(yes) => {
                let _ = tx.send(yes);
            }
            // Nobody answers: the sender stays alive in a thread so the question times out instead of being dropped.
            None => {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(60));
                    drop(tx);
                });
            }
        }
        rx
    }
}

struct Machine {
    core: ServiceCore,
    identity: Arc<Identity>,
    trust: Arc<TrustHandle>,
    trace: LogCapture,
    ui: Arc<AutoUi>,
}

impl Machine {
    /// `dev`: listen on loopback and accept loopback peers.
    fn new(name: &str, dev: bool, loopback_listen_only: bool) -> Self {
        Self::build(name, dev, loopback_listen_only, ALL, |cfg| cfg)
    }

    /// As `new`, with `grants` what the host's user picks for every request.
    fn with_grants(name: &str, dev: bool, loopback_listen_only: bool, grants: PermissionSet) -> Self {
        Self::build(name, dev, loopback_listen_only, grants, |cfg| cfg)
    }

    /// As [`Machine::new`], with the grants its user gives and a change to the configuration.
    fn build(
        name: &str,
        dev: bool,
        loopback_listen_only: bool,
        grants: PermissionSet,
        tweak: impl FnOnce(ServiceConfig) -> ServiceConfig,
    ) -> Self {
        let identity = Arc::new(Identity::generate(0).expect("identity"));
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).expect("trust");
        let trace = LogCapture::new();
        let ui = Arc::new(AutoUi {
            grants: Mutex::new(grants),
            prompts: AtomicUsize::new(0),
            consent_answer: Mutex::new(Some(true)),
            consents: AtomicUsize::new(0),
        });
        let deps = ServiceDeps {
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            clock: Arc::new(MonotonicClock::new()),
            trace: Arc::new(trace.clone()),
            ui: ui.clone(),
            interfaces: Arc::new(FakeNetInterfaces::new(Vec::new())),
        };
        let mut cfg = ServiceConfig::new(name, Os::Windows, "11 24H2", deps);
        cfg.session_port = 0;
        if dev {
            cfg = cfg.dev_loopback(lo());
        } else if loopback_listen_only {
            cfg.listen_extra.push(lo());
        }
        let core = ServiceCore::start(tweak(cfg)).expect("core starts");
        Self { core, identity, trust, trace, ui }
    }

    fn device(&self) -> DeviceId {
        self.identity.device_id()
    }

    fn addr(&self) -> SocketAddr {
        self.core.local_addrs()[0]
    }

    /// The pairing ceremony, done by hand: each knows the other's key; the host stores `grants_here` for the viewer.
    fn pair(host: &Machine, viewer: &Machine, grants_here: PermissionSet) {
        host.trust
            .update(|s| {
                let mut d = PairedDevice::new_paired(
                    *viewer.identity.spki(),
                    "viewer-pc".into(),
                    Os::Windows,
                    "11".into(),
                    1,
                    None,
                );
                d.grants_here = grants_here;
                s.devices.push(d);
            })
            .expect("host store");
        viewer
            .trust
            .update(|s| {
                s.devices.push(PairedDevice::new_paired(
                    *host.identity.spki(),
                    "host-pc".into(),
                    Os::Windows,
                    "11".into(),
                    1,
                    None,
                ))
            })
            .expect("viewer store");
    }
}

type TestDeps = HelperDeps<SyntheticScreen, Passthrough, RecordingInputSink, SineSource>;

/// A helper process stand-in: `run_helper` on its own thread, behind a spy that records what the service sent.
struct TestHelper {
    sink: RecordingInputSink,
    from_service: Arc<Mutex<Vec<Ipc>>>,
    thread: Option<JoinHandle<()>>,
}

impl TestHelper {
    fn spawn(core: &ServiceCore, role: IpcRole) -> Self {
        Self::spawn_with(core, role, |deps| deps)
    }

    /// As `spawn`, with `tune` given the helper's dependencies first (the terminal proxy's spawner, for instance).
    fn spawn_with(core: &ServiceCore, role: IpcRole, tune: impl FnOnce(TestDeps) -> TestDeps) -> Self {
        let sink = RecordingInputSink::new();
        let (core_end, spy_core): (End, End) = duplex_blocking();
        let (spy_helper, helper_end): (End, End) = duplex_blocking();
        let from_service = Arc::new(Mutex::new(Vec::new()));

        // service -> helper, recorded (the handshake's HelloAck is not).
        {
            let (log, mut reader, mut writer) =
                (Arc::clone(&from_service), FrameReader::new(spy_core.reader), FrameWriter::new(spy_helper.writer));
            std::thread::spawn(move || {
                while let Ok(Some(msg)) = reader.read_message() {
                    if !matches!(msg, Ipc::HelloAck { .. }) {
                        lock(&log).push(msg.clone());
                    }
                    if writer.write_message(&msg).is_err() {
                        break;
                    }
                }
            });
        }
        // helper -> service, untouched.
        {
            let (mut reader, mut writer) = (FrameReader::new(spy_helper.reader), FrameWriter::new(spy_core.writer));
            std::thread::spawn(move || {
                while let Ok(Some(msg)) = reader.read_message() {
                    if writer.write_message(&msg).is_err() {
                        break;
                    }
                }
            });
        }
        let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
        let audio_clock = Arc::clone(&clock);
        let input = sink.clone();
        let deps = tune(HelperDeps::new(
            |_| Ok(SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime)),
            |_frame, _config, _backend| Ok(Passthrough { n: 0 }),
            move || Ok(input.clone()),
            move || Ok(SineSource::new(audio_clock.clone()).realtime()),
            clock,
        ));
        let thread = std::thread::spawn(move || {
            let _ = run_helper(helper_end.reader, helper_end.writer, deps, role, 4242);
        });
        let kind = core.attach_helper(role, core_end.reader, core_end.writer).expect("attach");
        assert_eq!(Some(kind), HelperKind::of_role(role));
        Self { sink, from_service, thread: Some(thread) }
    }

    fn sent(&self) -> Vec<Ipc> {
        lock(&self.from_service).clone()
    }

    fn count(&self, f: impl Fn(&Ipc) -> bool) -> usize {
        self.sent().iter().filter(|m| f(m)).count()
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            t.join().expect("helper thread");
        }
    }
}

/// Everything the viewer's UI channel delivered, in order, with a read cursor.
struct Seen {
    msgs: ViewerMessages,
    all: Vec<ViewerOut>,
    cursor: usize,
}

impl Seen {
    fn new(msgs: ViewerMessages) -> Self {
        Self { msgs, all: Vec::new(), cursor: 0 }
    }

    fn pull(&mut self, limit: Duration) -> bool {
        match self.msgs.recv_timeout(limit) {
            Ok(Some(item)) => {
                self.all.push(item);
                true
            }
            _ => false,
        }
    }

    /// The next item from the cursor on that `f` picks (the ones before it are skipped), within `WAIT`.
    fn next<T>(&mut self, what: &str, mut f: impl FnMut(&ViewerOut) -> Option<T>) -> T {
        let end = Instant::now() + WAIT;
        loop {
            while self.cursor < self.all.len() {
                let i = self.cursor;
                self.cursor += 1;
                if let Some(v) = f(&self.all[i]) {
                    return v;
                }
            }
            assert!(Instant::now() < end, "timed out waiting for {what}");
            self.pull(Duration::from_millis(50));
        }
    }

    fn video(&mut self) -> (swapdesk_proto::VideoHeader, usize) {
        self.next("a ViewerVideo", |o| match o {
            ViewerOut::Message(Ipc::ViewerVideo { header, payload, .. }) => Some((*header, payload.0.len())),
            _ => None,
        })
    }

    /// Reads for `window` and says whether any item matching `f` came.
    fn any_within(&mut self, window: Duration, f: impl Fn(&ViewerOut) -> bool) -> bool {
        let end = Instant::now() + window;
        let start = self.cursor;
        while Instant::now() < end {
            self.pull(Duration::from_millis(20));
        }
        let hit = self.all[start..].iter().any(&f);
        self.cursor = self.all.len();
        hit
    }

    /// Reads what is queued now (video never stops, so this is a window, not "until quiet") and moves the cursor
    /// past it.
    fn skip_pending(&mut self) {
        let end = Instant::now() + Duration::from_millis(150);
        while Instant::now() < end {
            self.pull(Duration::from_millis(10));
        }
        self.cursor = self.all.len();
    }
}

fn is_video(o: &ViewerOut) -> bool {
    matches!(o, ViewerOut::Message(Ipc::ViewerVideo { .. }))
}

struct Rig {
    host: Machine,
    viewer: Machine,
    system: TestHelper,
    user: TestHelper,
}

impl Rig {
    fn new() -> Self {
        let host = Machine::new("host-pc", true, false);
        let viewer = Machine::new("viewer-pc", true, false);
        Machine::pair(&host, &viewer, ALL);
        let system = TestHelper::spawn(&host.core, IpcRole::HelperSystem);
        let user = TestHelper::spawn(&host.core, IpcRole::HelperUser);
        Self { host, viewer, system, user }
    }

    fn connect(&self) -> (ViewerHandle, Seen) {
        let (handle, msgs) =
            self.viewer.core.connect(self.host.addr(), self.host.device(), Intent::Control).expect("the session opens");
        (handle, Seen::new(msgs))
    }

    fn finish(mut self) {
        self.host.core.shutdown();
        self.viewer.core.shutdown();
        self.system.join();
        self.user.join();
    }
}

#[test]
fn two_cores_relay_video_input_and_audio_and_end_cleanly() {
    let rig = Rig::new();
    let (handle, mut seen) = rig.connect();
    assert_eq!(handle.granted(), ALL, "what the host's user picked");
    let session_id = handle.session_id();
    assert_ne!(session_id, 0);
    assert_eq!(rig.host.ui.prompts.load(Ordering::SeqCst), 1, "a prompted session");

    // Video: the first frame is a keyframe, ids increase, the session id is the viewer's.
    let first = seen.next("the first ViewerVideo", |o| match o {
        ViewerOut::Message(Ipc::ViewerVideo { session_id: id, header, .. }) => {
            assert_eq!(*id, session_id);
            Some(*header)
        }
        _ => None,
    });
    assert!(first.keyframe && first.config, "first frame: {first:?}");
    assert_eq!((first.display_id, first.width, first.height), (0, 64, 48));
    let mut last = first.frame_id;
    for _ in 0..10 {
        let (h, len) = seen.video();
        assert!(h.frame_id > last, "frame ids increase: {last} then {}", h.frame_id);
        assert_eq!(len, h.payload_len as usize);
        last = h.frame_id;
    }
    // The viewer got the host's display list through the session.
    let displays = seen.all.iter().find_map(|o| match o {
        ViewerOut::Displays { list, .. } => Some(list.displays.clone()),
        _ => None,
    });
    assert_eq!(displays.map(|d| d.len()), Some(1), "DisplayList reached the viewer's UI channel");
    // The system helper (media) was started, the user helper (input) was not asked for video.
    assert_eq!(rig.system.count(|m| matches!(m, Ipc::StartVideo { display: 0, .. })), 1);
    assert_eq!(rig.user.count(|m| matches!(m, Ipc::StartVideo { .. })), 0);

    // Audio is off until the viewer turns it on (D16).
    assert!(!seen.any_within(Duration::from_millis(300), |o| matches!(o, ViewerOut::Message(Ipc::ViewerAudio { .. }))));
    assert_eq!(rig.system.count(|m| matches!(m, Ipc::StartAudio { .. })), 0);

    // Input: key and pointer records reach the USER helper's sink in order (prompted session), never the system one.
    let first_batch = vec![
        InputEvent::Pointer { display: 0, x: 100, y: 200 },
        InputEvent::Key { usage_page: 0x07, usage: 0x04, down: true, modifiers: 0, locks: 0 },
        InputEvent::Key { usage_page: 0x07, usage: 0x04, down: false, modifiers: 0, locks: 0 },
    ];
    let second_batch = vec![
        InputEvent::Button { button: MouseButton::Left, down: true },
        InputEvent::Wheel { dx: 0, dy: -120 },
        InputEvent::Button { button: MouseButton::Left, down: false },
    ];
    handle.send_input(first_batch.clone()).expect("input accepted");
    handle.send_input(second_batch.clone()).expect("input accepted");
    let expected: Vec<InputEvent> = first_batch.iter().chain(&second_batch).cloned().collect();
    wait_for("the input to reach the user helper's sink", || {
        (rig.user.sink.events().len() >= expected.len()).then_some(())
    });
    assert_eq!(rig.user.sink.events(), expected, "in order");
    assert!(rig.system.sink.events().is_empty(), "a prompted session never reaches the SYSTEM injector");

    // Audio: on, datagrams arrive; the host asked its media helper.
    handle.set_audio(true).expect("audio control");
    let mut seqs = Vec::new();
    for _ in 0..5 {
        let datagram = seen.next("a ViewerAudio", |o| match o {
            ViewerOut::Message(Ipc::ViewerAudio { session_id: id, datagram }) => {
                assert_eq!(*id, session_id);
                Some(datagram.0.clone())
            }
            _ => None,
        });
        let (header, _opus) = swapdesk_audio::parse_datagram(&datagram).expect("a valid audio datagram");
        seqs.push(header.seq);
    }
    assert!(seqs.windows(2).all(|w| w[1] > w[0]), "audio sequence increases: {seqs:?}");
    assert_eq!(rig.system.count(|m| matches!(m, Ipc::StartAudio { exclude_pid: 0 })), 1);

    // Revoking CONTROL: the viewer is told, its own input is refused, and a stream it opens anyway reaches no sink.
    assert!(rig.host.core.host().set_session_grants(session_id, PermissionSet::VIEW | PermissionSet::AUDIO));
    seen.next("the state without CONTROL", |o| match o {
        ViewerOut::State(c) => match c.state {
            SessionState::Active { granted, .. } if granted == (PermissionSet::VIEW | PermissionSet::AUDIO).bits() => {
                Some(())
            }
            _ => None,
        },
        _ => None,
    });
    assert_eq!(handle.send_input(first_batch.clone()), Err(ApplyError::NotPermitted));
    let stray =
        InputEvent::Key { usage_page: 0x07, usage: 0x05, down: true, modifiers: 0, locks: 0 }.encode().expect("encode");
    let ctx = handle.ctx();
    rig.viewer.core.runtime().block_on(async move {
        // The viewer's own check is bypassed on purpose: the host must enforce it.
        if let Ok(mut s) = ctx.open_uni(StreamKind::Input).await {
            let _ = s.write_all(&stray).await;
            let _ = s.finish();
        }
    });
    wait_for("ReleaseAll after the revocation", || {
        rig.user.sink.records().iter().any(|r| r.what == InputRecord::ReleaseAll).then_some(())
    });
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(rig.user.sink.events(), expected, "nothing reached the sink after CONTROL went");
    // VIEW and AUDIO are untouched: video keeps flowing.
    seen.skip_pending();
    seen.video();

    // The end: both sides end, and the helpers are told to stop everything.
    handle.disconnect();
    let viewer_end = handle.wait_ended(WAIT).expect("the viewer side ends");
    assert_eq!(viewer_end.code, ErrorCode::NoError);
    wait_for("the host to forget the session", || rig.host.core.host().sessions().is_empty().then_some(()));
    seen.next("the Ended state", |o| match o {
        ViewerOut::State(c) if matches!(c.state, SessionState::Ended { .. }) => Some(()),
        _ => None,
    });
    wait_for("StopVideo, StopAudio and ReleaseAll", || {
        (rig.system.count(|m| matches!(m, Ipc::StopVideo { display: 0 })) >= 1
            && rig.system.count(|m| matches!(m, Ipc::StopAudio)) >= 1
            && rig.user.count(|m| matches!(m, Ipc::ReleaseAll)) >= 2
            && rig.system.count(|m| matches!(m, Ipc::ReleaseAll)) >= 1)
            .then_some(())
    });
    assert_eq!(rig.system.count(|m| matches!(m, Ipc::StartVideo { .. })), 1, "video was started once");
    assert!(
        rig.host.trace.lines().iter().any(|l| l.level == Level::Info && l.text.contains("started")),
        "the session is in the trace"
    );
    drop(handle);
    rig.finish();
}

#[test]
fn revoking_view_pauses_and_stops_video_and_restoring_it_resumes_with_a_keyframe() {
    let rig = Rig::new();
    let (handle, mut seen) = rig.connect();
    let id = handle.session_id();
    seen.video();
    seen.video();

    assert!(rig.host.core.host().set_session_grants(id, PermissionSet::FILES));
    wait_for("StopVideo", || (rig.system.count(|m| matches!(m, Ipc::StopVideo { .. })) >= 1).then_some(()));
    // What was already in flight is reset; after a short settle no frame arrives any more.
    std::thread::sleep(Duration::from_millis(300));
    seen.skip_pending();
    assert!(!seen.any_within(Duration::from_millis(500), is_video), "no frame after VIEW was revoked");

    assert!(rig.host.core.host().set_session_grants(id, PermissionSet::VIEW | PermissionSet::FILES));
    wait_for("a second StartVideo", || (rig.system.count(|m| matches!(m, Ipc::StartVideo { .. })) >= 2).then_some(()));
    let (resumed, _) = seen.video();
    assert!(resumed.keyframe, "the stream resumes with a keyframe: {resumed:?}");
    drop(handle);
    rig.finish();
}

#[test]
fn a_helper_that_disappears_is_reported_and_can_be_replaced() {
    let host = Machine::new("host-pc", true, false);
    let events = host.core.take_events().expect("events once");
    assert!(host.core.take_events().is_none());

    // A helper that does its handshake and vanishes. The caller verified its PID (1234); the helper wrote 77 in its
    // `Hello`, and only the verified one is used.
    let (core_end, helper_end): (End, End) = duplex_blocking();
    let vanishing = std::thread::spawn(move || {
        let mut reader = FrameReader::new(helper_end.reader);
        let mut writer = FrameWriter::new(helper_end.writer);
        Handshake::client(&mut reader, &mut writer, IpcRole::HelperUser, 77).expect("client handshake");
    });
    host.core.attach_helper_pid(IpcRole::HelperUser, 1234, core_end.reader, core_end.writer).expect("attach");
    vanishing.join().expect("helper thread");
    let attached = events.recv_timeout(WAIT).expect("attached event");
    assert_eq!(attached, CoreEvent::HelperAttached { kind: HelperKind::User, pid: 1234 });
    let gone = events.recv_timeout(WAIT).expect("gone event");
    assert_eq!(gone, CoreEvent::HelperGone { kind: HelperKind::User, pid: 1234, error: None });
    wait_for("the slot to empty", || host.core.helper(HelperKind::User).is_none().then_some(()));

    // The supervisor's relaunch: a new helper takes the slot.
    let mut again = TestHelper::spawn(&host.core, IpcRole::HelperUser);
    assert!(host.core.helper(HelperKind::User).is_some());
    host.core.shutdown();
    again.join();
}

#[test]
fn the_pid_of_a_helper_is_the_verified_one_and_never_the_hellos() {
    let host = Machine::new("host-pc", true, false);
    let events = host.core.take_events().expect("events once");
    // The supervisor verified the pipe client's PID (31337) while the helper claims 1 in its `Hello`.
    let (core_end, helper_end): (End, End) = duplex_blocking();
    let liar = std::thread::spawn(move || {
        let mut reader = FrameReader::new(helper_end.reader);
        let mut writer = FrameWriter::new(helper_end.writer);
        Handshake::client(&mut reader, &mut writer, IpcRole::HelperSystem, 1).expect("client handshake");
        // Stay connected until the core hangs up.
        let _ = reader.read_message();
    });
    let kind =
        host.core.attach_helper_pid(IpcRole::HelperSystem, 31337, core_end.reader, core_end.writer).expect("attach");
    assert_eq!(host.core.helper(kind).expect("attached").pid(), 31337, "the link keeps the verified PID");
    assert_eq!(
        events.recv_timeout(WAIT).expect("attached event"),
        CoreEvent::HelperAttached { kind: HelperKind::System, pid: 31337 }
    );

    // An in-process helper (the Mac app's, the tests') is this process.
    let (core_end2, helper_end2): (End, End) = duplex_blocking();
    let inproc = std::thread::spawn(move || {
        let mut reader = FrameReader::new(helper_end2.reader);
        let mut writer = FrameWriter::new(helper_end2.writer);
        Handshake::client(&mut reader, &mut writer, IpcRole::HelperUser, 9).expect("client handshake");
        let _ = reader.read_message();
    });
    let kind = host.core.attach_helper(IpcRole::HelperUser, core_end2.reader, core_end2.writer).expect("attach");
    assert_eq!(host.core.helper(kind).expect("attached").pid(), std::process::id());
    host.core.shutdown();
    liar.join().expect("helper thread");
    inproc.join().expect("helper thread");
}

#[test]
fn an_attach_with_the_wrong_role_or_the_ui_role_is_refused() {
    let host = Machine::new("host-pc", true, false);
    let (core_end, helper_end): (End, End) = duplex_blocking();
    let t = std::thread::spawn(move || {
        let mut reader = FrameReader::new(helper_end.reader);
        let mut writer = FrameWriter::new(helper_end.writer);
        let _ = Handshake::client(&mut reader, &mut writer, IpcRole::HelperUser, 1);
    });
    let r = host.core.attach_helper(IpcRole::HelperSystem, core_end.reader, core_end.writer);
    assert!(
        matches!(
            r,
            Err(super::AttachError::RoleMismatch { expected: IpcRole::HelperSystem, got: IpcRole::HelperUser })
        ),
        "{r:?}"
    );
    t.join().expect("thread");
    let (a, _b): (End, End) = duplex_blocking();
    assert!(matches!(
        host.core.attach_helper(IpcRole::Ui, a.reader, a.writer),
        Err(super::AttachError::NotAHelperRole(IpcRole::Ui))
    ));
    assert!(host.core.helper(HelperKind::System).is_none() && host.core.helper(HelperKind::User).is_none());
}

#[test]
fn the_guard_keeps_refusing_loopback_unless_the_development_option_is_on() {
    // A listens on loopback but has no development option: its guard judges every peer.
    let a = Machine::new("a", false, true);
    let b = Machine::new("b", true, false);
    Machine::pair(&a, &b, ALL);
    let r = b.core.connect(a.addr(), a.device(), Intent::Control);
    assert!(matches!(r, Err(ConnectError::Open(_))), "the host refused the connection");
    assert!(a.core.host().sessions().is_empty());
    assert_eq!(a.ui.prompts.load(Ordering::SeqCst), 0, "no prompt for a refused peer");
    wait_for("the refusal in A's trace", || {
        a.trace
            .lines()
            .iter()
            .any(|l| l.level == Level::Warn && l.text.contains("refused a connection") && l.text.contains("loopback"))
            .then_some(())
    });

    // C has no development option: it will not even dial a loopback address.
    let c = Machine::new("c", false, false);
    Machine::pair(&b, &c, ALL);
    let lo_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 47820));
    let r = c.core.connect(lo_addr, b.device(), Intent::View);
    assert!(matches!(r, Err(ConnectError::Refused(swapdesk_lan::guard::Refusal::Loopback))), "{:?}", r.err());
    // ... nor a device it has not paired.
    let r = b.core.connect(a.addr(), [7; 16], Intent::View);
    assert!(matches!(r, Err(ConnectError::UnknownDevice)));
}

#[test]
fn a_peer_at_a_blocked_address_is_dropped_before_the_handshake_and_costs_no_prompt() {
    // B is paired with A and dials from loopback (the development option); A has that address in `blocked`.
    let a = Machine::new("a", true, false);
    let b = Machine::new("b", true, false);
    Machine::pair(&a, &b, ALL);
    a.trust.update(|s| s.blocked.push([127, 0, 0, 1])).expect("host store");
    let r = b.core.connect(a.addr(), a.device(), Intent::Control);
    assert!(r.is_err(), "the blocked peer got a session");
    assert!(a.core.host().sessions().is_empty());
    assert_eq!(a.ui.prompts.load(Ordering::SeqCst), 0, "no prompt for a blocked peer");
    wait_for("the ignored connection in A's trace", || {
        a.trace.lines().iter().any(|l| l.text.contains("blocked address")).then_some(())
    });
}

#[test]
fn a_core_refuses_to_dial_its_own_endpoint() {
    let a = Machine::new("a", true, false);
    let r = a.core.connect(a.addr(), a.device(), Intent::View);
    assert!(matches!(r, Err(ConnectError::Refused(swapdesk_lan::guard::Refusal::OwnAddress))), "{:?}", r.err());
}

/// The keys a recording sink holds down at the end of its log (a `ReleaseAll` lets go of all).
fn keys_down(sink: &RecordingInputSink) -> Vec<u16> {
    let mut down: Vec<u16> = Vec::new();
    for r in sink.records() {
        match r.what {
            InputRecord::Event(InputEvent::Key { usage, down: true, .. }) => {
                if !down.contains(&usage) {
                    down.push(usage);
                }
            }
            InputRecord::Event(InputEvent::Key { usage, down: false, .. }) => down.retain(|u| *u != usage),
            InputRecord::ReleaseAll => down.clear(),
            _ => {}
        }
    }
    down
}

/// D9 (stage 64): a full input queue is its own error, and the dropped batch (here the key-up) is made good by a
/// `ReleaseAll` behind the queue, so no key stays down at the host.
#[test]
fn a_full_input_queue_is_a_distinct_error_and_every_key_ends_up_released() {
    let rig = Rig::new();
    let (handle, mut seen) = rig.connect();
    seen.video();
    let key = |usage: u16, down: bool| InputEvent::Key { usage_page: 0x07, usage, down, modifiers: 0, locks: 0 };
    let mut dropped_key_up = false;
    for attempt in 0..200u16 {
        let usage = 0x04 + attempt % 20;
        let _ = handle.send_input(vec![key(usage, true)]);
        let mut full = false;
        for n in 0..200_000u32 {
            match handle.send_input(vec![InputEvent::Pointer { display: 0, x: (n % 60_000) as u16, y: 1 }]) {
                Ok(()) => {}
                Err(ApplyError::Full) => {
                    full = true;
                    break;
                }
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
        assert!(full, "the writer never fell behind a tight loop of 200000 batches");
        if handle.send_input(vec![key(usage, false)]) == Err(ApplyError::Full) {
            dropped_key_up = true;
            break;
        }
    }
    assert!(dropped_key_up, "no key-up was ever dropped: the test did not exercise the overflow");

    // The host's sink settles: the last thing it saw is a ReleaseAll and nothing is down.
    let mut seen_len = 0;
    let mut quiet_since = Instant::now();
    wait_for("the input to settle on a ReleaseAll", || {
        let records = rig.user.sink.records();
        if records.len() != seen_len {
            seen_len = records.len();
            quiet_since = Instant::now();
        }
        (quiet_since.elapsed() > Duration::from_millis(400)
            && records.last().is_some_and(|r| r.what == InputRecord::ReleaseAll))
        .then_some(())
    });
    assert!(keys_down(&rig.user.sink).is_empty(), "keys still down at the host: {:?}", keys_down(&rig.user.sink));
    rig.finish();
}

/// C11 (stage 64): a UI that does not read gets a bounded amount of video, not a queue of 256 frames of up to 8 MiB;
/// the frames dropped are followed by a keyframe, which the viewer asks the host for.
#[test]
fn a_ui_that_does_not_read_holds_bounded_video_and_gets_a_keyframe_after_the_gap() {
    let rig = Rig::new();
    let (_handle, mut seen) = rig.connect();
    seen.video();
    std::thread::sleep(Duration::from_millis(2500));
    assert!(seen.msgs.video_bytes() <= 6 * 1024 * 1024, "{} bytes of video queued", seen.msgs.video_bytes());
    let mut queued = Vec::new();
    while let Some(item) = seen.msgs.try_recv() {
        if let ViewerOut::Message(Ipc::ViewerVideo { header, .. }) = item {
            queued.push(header.frame_id);
        }
    }
    assert!(!queued.is_empty() && queued.len() <= 34, "{} frames were queued", queued.len());
    let last = *queued.last().expect("some video");
    let key_id = seen.next("a keyframe after the gap", |o| match o {
        ViewerOut::Message(Ipc::ViewerVideo { header, .. }) if header.keyframe && header.frame_id > last + 1 => {
            Some(header.frame_id)
        }
        _ => None,
    });
    assert!(key_id > last + 1, "frames {last}..{key_id} were dropped, then a keyframe");
    assert!(
        rig.system.count(|m| matches!(m, Ipc::ForceKeyframe { .. })) >= 1,
        "the host's helper was asked for the keyframe"
    );
    rig.finish();
}
