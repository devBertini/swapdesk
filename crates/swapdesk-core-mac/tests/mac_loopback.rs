//! Stage 23 proof (D27) that `ServiceCore` runs on macOS: two cores in one process on loopback in dev mode, each with
//! the real `swapdesk-net-mac` interface list and a real `swapdesk-secret-mac` store, paired through the trust store, then
//! a short session: video frames, input to the host's user helper, audio, revocation, end. The capture, encode, input
//! and audio sides are the C7 fakes behind real `run_helper` loops over in-memory pipes (modelled on
//! `swapdesk-host/src/service/tests.rs` and `swapdesk-service/tests/gate_m3.rs`).
//!
//! Two flavours of the same body:
//! * `file_keychain_*`: each machine's secrets are in a throwaway keychain file made for the test (works over SSH).
//! * `login_keychain_*`: the secrets are generic-password items of the user's login keychain under unique service names
//!   ("swapdesk-test-<pid>-<nanos>-<n>"), deleted at the end. When macOS cannot serve the login keychain (an SSH session,
//!   status -25308) the test prints why and ends without asserting: run it from the owner's GUI session.
#![cfg(target_os = "macos")]

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use swapdesk_core_mac::{MacServiceParams, MacServiceSetup, mac_service_config, macos_version};
use swapdesk_fakes::{
    FakeClock, InputRecord, LogCapture, MonotonicClock, Pacing, RecordingInputSink, SineSource, SyntheticScreen,
};
use swapdesk_host::helper::{HelperDeps, run_helper};
use swapdesk_host::service::{ConnectError, HelperKind, ServiceCore, ViewerHandle, ViewerMessages, ViewerOut};
use swapdesk_ipc::{IpcRole, Message as Ipc, SessionState};
use swapdesk_net_mac::list_interfaces;
use swapdesk_platform::{
    EncodedFrame, EncoderConfig, EncoderInfo, ErrorKind, Frame, IfKind, Level, MonoMicros, NetInterfaces,
    Result as PResult, SecretStore, VideoEncoder,
};
use swapdesk_proto::{ErrorCode, H264Profile, InputEvent, Intent, MouseButton, PermissionSet};
use swapdesk_secret_mac::{
    KeychainLocation, MacSecretStore, SecretLocation, create_keychain_file, delete_keychain_file,
};
use swapdesk_session::{ConsentRequest, HostUi, PromptAnswer, PromptRequest};
use swapdesk_trust::{DeviceId, PairedDevice, TrustHandle};
use tokio::sync::oneshot;

const WAIT: Duration = Duration::from_secs(15);
const ALL: PermissionSet = PermissionSet::CONTROL.union(PermissionSet::AUDIO);
static N: AtomicU32 = AtomicU32::new(0);

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

// ---------- in-memory duplex pipe (the host crate's test pipe is private) ----------

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    left: Vec<u8>,
    pos: usize,
}

struct PipeWriter {
    tx: Sender<Vec<u8>>,
}

struct End {
    reader: PipeReader,
    writer: PipeWriter,
}

fn duplex() -> (End, End) {
    let (a_tx, a_rx) = channel();
    let (b_tx, b_rx) = channel();
    (
        End { reader: PipeReader { rx: b_rx, left: Vec::new(), pos: 0 }, writer: PipeWriter { tx: a_tx } },
        End { reader: PipeReader { rx: a_rx, left: Vec::new(), pos: 0 }, writer: PipeWriter { tx: b_tx } },
    )
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.left.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.left = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.left.len() - self.pos);
        buf[..n].copy_from_slice(&self.left[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.tx.send(buf.to_vec()).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Emits a 4-byte marker (the encode counter) as the payload; keyframe on the first and on forced frames.
struct Passthrough {
    n: u32,
}

impl VideoEncoder for Passthrough {
    fn configure(&mut self, _config: &EncoderConfig) -> PResult<EncoderInfo> {
        self.n = 0;
        Ok(EncoderInfo {
            name: "passthrough".into(),
            hardware: false,
            profile: H264Profile::Main,
            level: 40,
            low_latency: true,
        })
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        let first = self.n == 0;
        let marker = self.n.to_be_bytes().to_vec();
        self.n += 1;
        Ok(vec![EncodedFrame {
            captured_at: frame.captured_at,
            encode_us: 7,
            keyframe: first || force_keyframe,
            config_change: first,
            width: u16::try_from(frame.width).expect("width"),
            height: u16::try_from(frame.height).expect("height"),
            annex_b: marker,
        }])
    }

    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        Ok(())
    }

    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

/// The host's user, scripted: somebody is there and accepts every request with `ALL`.
struct AutoUi {
    prompts: AtomicUsize,
}

impl HostUi for AutoUi {
    fn somebody_present(&self) -> bool {
        true
    }

    fn prompt(&self, _request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(PromptAnswer::Accept(ALL));
        rx
    }

    fn consent(&self, _request: ConsentRequest) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(true);
        rx
    }
}

// ---------- one machine: a real secret store, the real interface list, a core ----------

fn unique_tag() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("swapdesk-test-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

/// Scratch state of one machine, removed when dropped: its keychain items (and keychain file) and its folder.
struct Scratch {
    dir: PathBuf,
    keychain_file: Option<PathBuf>,
    store: Arc<MacSecretStore>,
}

impl Scratch {
    fn new(file_keychain: bool) -> Self {
        let tag = unique_tag();
        let dir = std::env::temp_dir().join(&tag);
        std::fs::create_dir_all(&dir).expect("scratch folder");
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let mut location = SecretLocation::login(&home).with(tag.clone(), dir.join("folder"));
        let keychain_file = file_keychain.then(|| dir.join(format!("{tag}.keychain-db")));
        if let Some(path) = &keychain_file {
            create_keychain_file(path, "swapdesk-test-password").expect("a throwaway keychain is created");
            location.keychain = KeychainLocation::File(path.clone());
        }
        let store = Arc::new(MacSecretStore::open(&location).expect("the store opens"));
        Self { dir, keychain_file, store }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.store.delete_all();
        if let Some(k) = &self.keychain_file {
            let _ = delete_keychain_file(k);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Machine {
    core: ServiceCore,
    setup_identity: Arc<swapdesk_trust::Identity>,
    trust: Arc<TrustHandle>,
    trace: LogCapture,
    ui: Arc<AutoUi>,
    scratch: Scratch,
}

impl Machine {
    fn new(name: &str, file_keychain: bool) -> Self {
        let scratch = Scratch::new(file_keychain);
        let trace = LogCapture::new();
        let ui = Arc::new(AutoUi { prompts: AtomicUsize::new(0) });
        let setup = build_setup(name, &scratch, &trace, &ui);
        let MacServiceSetup { config, identity, trust, settings } = setup;
        assert_eq!(settings.name, name);
        let core = ServiceCore::start(config).expect("the core starts on the real interface list plus loopback");
        Self { core, setup_identity: identity, trust, trace, ui, scratch }
    }

    fn device(&self) -> DeviceId {
        self.setup_identity.device_id()
    }

    /// The loopback address of the session endpoint (the real interfaces' addresses are listed too).
    fn loopback_addr(&self) -> SocketAddr {
        *self.core.local_addrs().iter().find(|a| a.ip().is_loopback()).expect("a loopback endpoint")
    }
}

fn build_setup(name: &str, scratch: &Scratch, trace: &LogCapture, ui: &Arc<AutoUi>) -> MacServiceSetup {
    mac_service_config(MacServiceParams {
        default_name: name.into(),
        os_version: macos_version(),
        secrets: scratch.store.clone(),
        interfaces: Arc::new(swapdesk_net_mac::MacNetInterfaces),
        clock: Arc::new(MonotonicClock::new()),
        trace: Arc::new(trace.clone()),
        ui: ui.clone(),
        dev_loopback: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        ports: Some((0, 0)),
    })
    .expect("mac_service_config")
}

/// The pairing ceremony, done by hand as in the host tests: each knows the other's key, the host stores `grants_here`.
fn pair(host: &Machine, viewer: &Machine) {
    host.trust
        .update(|s| {
            let mut d = PairedDevice::new_paired(
                *viewer.setup_identity.spki(),
                "viewer-mac".into(),
                swapdesk_proto::Os::MacOs,
                "27".into(),
                1,
                None,
            );
            d.grants_here = ALL;
            s.devices.push(d);
        })
        .expect("host store");
    viewer
        .trust
        .update(|s| {
            s.devices.push(PairedDevice::new_paired(
                *host.setup_identity.spki(),
                "host-mac".into(),
                swapdesk_proto::Os::MacOs,
                "27".into(),
                1,
                None,
            ))
        })
        .expect("viewer store");
}

/// A helper process stand-in: `run_helper` on its own thread over in-memory pipes, with the C7 fakes.
struct TestHelper {
    sink: RecordingInputSink,
    thread: Option<JoinHandle<()>>,
}

impl TestHelper {
    fn spawn(core: &ServiceCore, role: IpcRole) -> Self {
        let sink = RecordingInputSink::new();
        let (core_end, helper_end) = duplex();
        let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
        let audio_clock = Arc::clone(&clock);
        let input = sink.clone();
        let deps = HelperDeps::new(
            |_| Ok(SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime)),
            |_frame, _config, _backend| Ok(Passthrough { n: 0 }),
            move || Ok(input.clone()),
            move || Ok(SineSource::new(audio_clock.clone()).realtime()),
            clock,
        );
        let thread = std::thread::spawn(move || {
            let _ = run_helper(helper_end.reader, helper_end.writer, deps, role, 4242);
        });
        let kind = core.attach_helper(role, core_end.reader, core_end.writer).expect("attach");
        assert_eq!(Some(kind), HelperKind::of_role(role));
        Self { sink, thread: Some(thread) }
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            t.join().expect("helper thread");
        }
    }
}

/// Everything the viewer's channel delivered, with a read cursor.
struct Seen {
    msgs: ViewerMessages,
    all: Vec<ViewerOut>,
    cursor: usize,
}

impl Seen {
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
            if let Ok(Some(item)) = self.msgs.recv_timeout(Duration::from_millis(50)) {
                self.all.push(item);
            }
        }
    }

    fn video(&mut self) -> (swapdesk_proto::VideoHeader, usize) {
        self.next("a ViewerVideo", |o| match o {
            ViewerOut::Message(Ipc::ViewerVideo { header, payload, .. }) => Some((*header, payload.0.len())),
            _ => None,
        })
    }
}

/// The interfaces the Mac reports and which of them the guard may use, printed for the run's record.
fn report_interfaces() {
    println!("{:<8} {:<9} {:<3} {:<12} ipv4", "name", "kind", "up", "approved");
    for r in list_interfaces().expect("interfaces") {
        let i = &r.interface;
        let approved = i.up && matches!(i.kind, IfKind::Ethernet | IfKind::WiFi);
        println!("{:<8} {:<9} {:<3} {:<12} {:?}", i.name, format!("{:?}", i.kind), i.up, approved, i.ipv4);
    }
}

fn session(file_keychain: bool) {
    report_interfaces();
    let host = Machine::new("host-mac", file_keychain);
    let viewer = Machine::new("viewer-mac", file_keychain);
    assert_ne!(host.device(), viewer.device());

    // The core listens only where the guard approves: every non-loopback endpoint is on an up Wi-Fi or Ethernet interface.
    let interfaces = swapdesk_net_mac::MacNetInterfaces.list().expect("interfaces");
    for core in [&host.core, &viewer.core] {
        for addr in core.local_addrs() {
            let SocketAddr::V4(v4) = addr else { panic!("IPv4 only: {addr}") };
            if v4.ip().is_loopback() {
                continue;
            }
            let owner = interfaces
                .iter()
                .find(|i| i.ipv4.iter().any(|(ip, _)| ip == v4.ip()))
                .unwrap_or_else(|| panic!("no interface owns {addr}"));
            assert!(
                owner.up && matches!(owner.kind, IfKind::Ethernet | IfKind::WiFi),
                "{addr} is on {} ({:?})",
                owner.name,
                owner.kind
            );
        }
    }

    pair(&host, &viewer);
    // The identity and the pairing are in the keychain, not only in memory: a second open of the store sees them.
    for m in [&host, &viewer] {
        let again = TrustHandle::open(m.scratch.store.clone()).expect("reopen the trust store");
        assert_eq!(again.read(|s| s.devices.len()), 1, "trust.bin was written to the keychain");
        let identity =
            swapdesk_trust::Identity::load_or_create(&*m.scratch.store, 0).expect("identity from the keychain");
        assert_eq!(identity.device_id(), m.device(), "identity.bin was written to the keychain and reads back");
    }

    let mut system = TestHelper::spawn(&host.core, IpcRole::HelperSystem);
    let mut user = TestHelper::spawn(&host.core, IpcRole::HelperUser);

    let (handle, msgs): (ViewerHandle, ViewerMessages) =
        viewer.core.connect(host.loopback_addr(), host.device(), Intent::Control).expect("the session opens");
    let mut seen = Seen { msgs, all: Vec::new(), cursor: 0 };
    assert_eq!(handle.granted(), ALL, "what the host's user picked");
    assert_eq!(host.ui.prompts.load(Ordering::SeqCst), 1, "a prompted session");
    let session_id = handle.session_id();

    // Video: the first frame is a keyframe, ids increase.
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

    // Input reaches the USER helper's sink in order, never the system one (a prompted session).
    let first_batch = vec![
        InputEvent::Pointer { display: 0, x: 100, y: 200 },
        InputEvent::Key { usage_page: 0x07, usage: 0x04, down: true, modifiers: 0, locks: 0 },
    ];
    let second_batch = vec![
        InputEvent::Button { button: MouseButton::Left, down: true },
        InputEvent::Button { button: MouseButton::Left, down: false },
    ];
    handle.send_input(first_batch.clone()).expect("input accepted");
    handle.send_input(second_batch.clone()).expect("input accepted");
    let expected: Vec<InputEvent> = first_batch.iter().chain(&second_batch).cloned().collect();
    wait_for("the input to reach the user helper's sink", || {
        (user.sink.events().len() >= expected.len()).then_some(())
    });
    assert_eq!(user.sink.events(), expected, "in order");
    assert!(system.sink.events().is_empty());

    // Audio on: datagrams arrive.
    handle.set_audio(true).expect("audio control");
    let datagram = seen.next("a ViewerAudio", |o| match o {
        ViewerOut::Message(Ipc::ViewerAudio { datagram, .. }) => Some(datagram.0.clone()),
        _ => None,
    });
    assert!(swapdesk_audio::parse_datagram(&datagram).is_ok(), "a valid audio datagram");

    // Revoking CONTROL reaches the viewer and releases the host's keys.
    assert!(host.core.host().set_session_grants(session_id, PermissionSet::VIEW | PermissionSet::AUDIO));
    seen.next("the state without CONTROL", |o| match o {
        ViewerOut::State(c) => match c.state {
            SessionState::Active { granted, .. } if granted == (PermissionSet::VIEW | PermissionSet::AUDIO).bits() => {
                Some(())
            }
            _ => None,
        },
        _ => None,
    });
    wait_for("ReleaseAll after the revocation", || {
        user.sink.records().iter().any(|r| r.what == InputRecord::ReleaseAll).then_some(())
    });

    // The end: both sides end cleanly.
    handle.disconnect();
    let ended = handle.wait_ended(WAIT).expect("the viewer side ends");
    assert_eq!(ended.code, ErrorCode::NoError);
    wait_for("the host to forget the session", || host.core.host().sessions().is_empty().then_some(()));
    assert!(
        host.trace.lines().iter().any(|l| l.level == Level::Info && l.text.contains("started")),
        "the session is in the host's trace"
    );
    drop(handle);

    let (
        Machine { core: host_core, scratch: host_scratch, .. },
        Machine { core: viewer_core, scratch: viewer_scratch, .. },
    ) = (host, viewer);
    host_core.shutdown();
    viewer_core.shutdown();
    system.join();
    user.join();
    drop((host_scratch, viewer_scratch));
}

#[test]
fn file_keychain_two_cores_pair_and_run_a_session_on_loopback() {
    session(true);
}

#[test]
fn login_keychain_two_cores_pair_and_run_a_session_on_loopback() {
    // Probe the login keychain with the same kind of write the stores will make.
    let probe = Scratch::new(false);
    match probe.store.store("probe", b"x").and_then(|()| probe.store.delete("probe")) {
        Ok(()) => {}
        Err(e) if e.kind == ErrorKind::Unavailable => {
            println!(
                "SKIPPED: the login keychain is not reachable from this session ({e}); run this test from the owner's GUI session"
            );
            return;
        }
        Err(e) => panic!("login keychain probe: {e}"),
    }
    drop(probe);
    session(false);
    println!("login keychain session: ok");
}

#[test]
fn a_core_refuses_a_peer_it_was_not_paired_with() {
    let host = Machine::new("host-mac", true);
    let stranger = Machine::new("stranger-mac", true);
    // The stranger knows the host's key (it could have seen it) but the host never paired the stranger.
    stranger
        .trust
        .update(|s| {
            s.devices.push(PairedDevice::new_paired(
                *host.setup_identity.spki(),
                "host-mac".into(),
                swapdesk_proto::Os::MacOs,
                "27".into(),
                1,
                None,
            ))
        })
        .expect("store");
    let r = stranger.core.connect(host.loopback_addr(), host.device(), Intent::View);
    assert!(matches!(r, Err(ConnectError::Open(_))), "an unpaired device is refused: {:?}", r.err());
    assert!(host.core.host().sessions().is_empty());
    assert_eq!(host.ui.prompts.load(Ordering::SeqCst), 0, "no prompt for an unpaired peer");
}
