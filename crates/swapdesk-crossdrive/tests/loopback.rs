//! The two drivers against each other on one machine: two cores with their UI hubs on loopback (the dev option), the
//! host with real `run_helper` loops over the C7 fakes (the system helper's screen is a synthetic one whose cursor
//! follows what the user helper's input sink received, as the real desktop's does), and an orchestrator in this test
//! that passes the pairing codes and the go exactly as `scripts/mac/cross-session.sh` does. What it proves is the
//! script (pair, grant, accept, video, pointer, audio, end, forget on both sides); the real LAN, the installed service
//! and VideoToolbox are the orchestrator's run.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_crossdrive::{DecoderFactory, HostConfig, Media, Pump, ViewerConfig, host_drive, viewer_drive};
use swapdesk_fakes::{
    FakeClock, FakeNetInterfaces, LogCapture, MemorySecretStore, MonotonicClock, Pacing, RecordingInputSink,
    SineSource, SyntheticScreen,
};
use swapdesk_host::helper::{HelperDeps, run_helper};
use swapdesk_host::service::{ServiceConfig, ServiceCore, ServiceDeps};
use swapdesk_host::ui_hub::{ConnInfo, DevOptions, HubParams, MachineSettings, NoHooks, NoPresence, UiHub};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake, IpcRole};
use swapdesk_platform::{
    CaptureConfig, CursorUpdate, DecodedFrame, DisplayId, DisplayInfo, EncodedFrame, EncoderConfig, EncoderInfo, Frame,
    MonoMicros, Pixels, Result as PResult, ScreenSource, VideoDecoder, VideoEncoder,
};
use swapdesk_proto::{H264Profile, InputEvent, Os, VideoHeader};
use swapdesk_trust::{Identity, MemoryAudit, SystemWallClock, TrustHandle, UserRef};
use swapdesk_ui::service_backend::ServiceBackend;
use swapdesk_ui::{Command, Decision, PairingState, Permission, Permissions, Reply, UiEvent, UiWaker};

// ---------- in-memory duplex pipe ----------

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

// ---------- fakes of the host's desktop and of the viewer's decoder ----------

/// A synthetic screen of 64x48 whose cursor starts at the middle and then follows the pointer events the user helper's
/// sink received. After each move the pointer is hidden, as an application does while its user types: Windows then
/// reports the corner of the display with `visible` off.
struct FollowingScreen {
    inner: SyntheticScreen,
    input: RecordingInputSink,
    seen: usize,
    started: bool,
    hide_next: bool,
}

impl ScreenSource for FollowingScreen {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        self.inner.displays()
    }
    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> PResult<()> {
        self.inner.open(display, config)
    }
    fn next_frame(&mut self, timeout: Duration) -> PResult<Option<Frame>> {
        self.inner.next_frame(timeout)
    }
    fn reacquire(&mut self) -> PResult<()> {
        self.inner.reacquire()
    }
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        if !self.started {
            self.started = true;
            return Some(CursorUpdate { display: DisplayId(0), x: 32, y: 24, visible: true, shape: None });
        }
        if self.hide_next {
            self.hide_next = false;
            return Some(CursorUpdate { display: DisplayId(0), x: 0, y: 0, visible: false, shape: None });
        }
        let events = self.input.events();
        while self.seen < events.len() {
            let event = &events[self.seen];
            self.seen += 1;
            if let InputEvent::Pointer { x, y, .. } = event {
                let (x, y) = (i32::from(*x) * 64 / 65535, i32::from(*y) * 48 / 65535);
                self.hide_next = true;
                return Some(CursorUpdate { display: DisplayId(0), x, y, visible: true, shape: None });
            }
        }
        None
    }
    fn close(&mut self) {
        self.inner.close();
    }
}

/// The encoder of the host's tests: a 4-byte marker as the payload, a keyframe first.
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

/// A decoder that turns every payload into a lit picture of the header's size.
struct LitDecoder;

impl VideoDecoder for LitDecoder {
    fn decode(&mut self, header: &VideoHeader, _annex_b: &[u8]) -> PResult<Option<DecodedFrame>> {
        let (w, h) = (u32::from(header.width), u32::from(header.height));
        Ok(Some(DecodedFrame {
            display: DisplayId(header.display_id),
            frame_id: header.frame_id,
            captured_at: MonoMicros(header.capture_us),
            width: w,
            height: h,
            pixels: Pixels::Cpu { stride: w * 4, data: vec![200; (w * h * 4) as usize] },
        }))
    }
    fn reset(&mut self) {}
}

// ---------- one machine ----------

struct Machine {
    core: Arc<ServiceCore>,
    hub: UiHub,
    trust: Arc<TrustHandle>,
    log: LogCapture,
    helpers: Vec<JoinHandle<()>>,
}

/// A session port with the pairing port next to it (a typed address implies it, PROTOCOL.md 1), below Windows' dynamic
/// range (49152 and up), which hands out ephemeral ports in sequence and would give the second one to the next
/// socket bound to port 0.
fn free_port_pair() -> u16 {
    let start = 41_000 + (std::process::id() % 2_000) as u16 * 2;
    for p in (start..48_000).step_by(2) {
        let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, p));
        let b = UdpSocket::bind((Ipv4Addr::LOCALHOST, p + 1));
        if a.is_ok() && b.is_ok() {
            return p;
        }
    }
    panic!("no two free ports in a row");
}

impl Machine {
    /// `ports`: fixed (session, pairing) ports on 127.0.0.1 for the machine that is reached by a typed address.
    fn new(name: &str, os: Os, ports: Option<(u16, u16)>) -> Self {
        let identity = Arc::new(Identity::generate(0).expect("identity"));
        let secrets = Arc::new(MemorySecretStore::new());
        let trust = TrustHandle::open(secrets.clone()).expect("trust");
        let trace = LogCapture::new();
        let trace_view = trace.clone();
        let interfaces = Arc::new(FakeNetInterfaces::new(Vec::new()));
        let clock = Arc::new(MonotonicClock::new());
        let (session_port, pair_port) = ports.unwrap_or((0, 0));
        let hub = UiHub::new(HubParams {
            os,
            os_version: "test".into(),
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            secrets: secrets.clone(),
            settings: MachineSettings::new(name),
            interfaces: interfaces.clone(),
            presence: Arc::new(NoPresence),
            audit: Arc::new(MemoryAudit::new()),
            clock: clock.clone(),
            wall: Arc::new(SystemWallClock),
            trace: Arc::new(trace.clone()),
            hooks: Arc::new(NoHooks),
            dev: Some(DevOptions { pair_listen: vec![SocketAddrV4::new(Ipv4Addr::LOCALHOST, pair_port)] }),
        });
        let deps = ServiceDeps {
            identity,
            trust: Arc::clone(&trust),
            clock,
            trace: Arc::new(trace),
            ui: hub.host_ui(),
            interfaces,
        };
        let mut cfg = ServiceConfig::new(name, os, "test", deps);
        cfg.session_port = session_port;
        let cfg = cfg.dev_loopback(SocketAddrV4::new(Ipv4Addr::LOCALHOST, session_port));
        let core = Arc::new(ServiceCore::start(cfg).expect("core starts"));
        hub.bind_core(&core);
        // The UI of `ui()` runs in session 1, the console's: only the console's UI may connect.
        hub.set_console_session(Some(1));
        Self { core, hub, trust, log: trace_view, helpers: Vec::new() }
    }

    /// The system helper (screen, encoder, audio) and the user helper (input) of the host, as `run_helper` loops.
    fn attach_helpers(&mut self) {
        let user_sink = RecordingInputSink::new();
        for role in [IpcRole::HelperSystem, IpcRole::HelperUser] {
            let (core_end, helper_end) = duplex();
            let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
            let audio_clock = Arc::clone(&clock);
            let input = user_sink.clone();
            let for_screen = user_sink.clone();
            let deps = HelperDeps::new(
                move |_| {
                    Ok(FollowingScreen {
                        inner: SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime),
                        input: for_screen.clone(),
                        seen: 0,
                        started: false,
                        hide_next: false,
                    })
                },
                |_frame, _config, _backend| Ok(Passthrough { n: 0 }),
                move || Ok(input.clone()),
                move || Ok(SineSource::new(audio_clock.clone()).realtime()),
                clock,
            );
            self.helpers.push(std::thread::spawn(move || {
                let _ = run_helper(helper_end.reader, helper_end.writer, deps, role, 4242);
            }));
            self.core.attach_helper(role, core_end.reader, core_end.writer).expect("attach");
        }
    }

    fn shutdown(self) {
        let Machine { core, hub, helpers, .. } = self;
        hub.shutdown();
        let end = Instant::now() + Duration::from_secs(3);
        let mut core = Some(core);
        while let Some(c) = core.take() {
            match Arc::try_unwrap(c) {
                Ok(c) => c.shutdown(),
                Err(c) if Instant::now() < end => {
                    core = Some(c);
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(c) => drop(c),
            }
        }
        for t in helpers {
            let _ = t.join();
        }
    }
}

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

/// A UI connection to `hub`, as the shells make one: the hub serves one end of a pipe, a `ServiceBackend` drives the other.
fn ui(
    hub: &UiHub,
    sink: Option<Arc<dyn swapdesk_ui::service_backend::ViewerSink>>,
) -> (ServiceBackend, JoinHandle<()>) {
    let (client_end, server_end) = duplex();
    let hub = hub.clone();
    let serve = std::thread::spawn(move || {
        let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
        Handshake::server(&mut reader, &mut writer, 1).expect("server handshake");
        let info = ConnInfo {
            session_id: 1,
            user: UserRef { id: "S-1-5-21-1".into(), name: "tester".into() },
            admin: false,
            can_elevate: false,
        };
        let _ = hub.serve(reader, writer, info);
    });
    let backend = ServiceBackend::start_with_sink(client_end.reader, client_end.writer, Arc::new(NoWake), 4321, sink)
        .expect("handshake");
    (backend, serve)
}

fn field(lines: &Mutex<Vec<String>>, marker: &str) -> Option<String> {
    let l = lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    l.iter().find_map(|line| line.strip_prefix(marker).map(|rest| rest.trim().to_string()))
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "swapdesk-crossdrive-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

#[test]
fn the_two_drivers_pair_run_a_session_and_forget_each_other() {
    let dir = scratch_dir();
    let session_port = free_port_pair();
    let mut host = Machine::new("host-pc", Os::Windows, Some((session_port, session_port + 1)));
    host.attach_helpers();
    host.hub.set_console_session(Some(1));
    let viewer = Machine::new("viewer-mac", Os::MacOs, None);

    let media = Media::new(Box::new(|| -> Box<dyn VideoDecoder> { Box::new(LitDecoder) }) as DecoderFactory);
    let (host_ui, host_serve) = ui(&host.hub, None);
    let (viewer_ui, viewer_serve) = ui(&viewer.hub, Some(media.sink()));

    let host_lines = Mutex::new(Vec::<String>::new());
    let viewer_lines = Mutex::new(Vec::<String>::new());
    let host_cfg = HostConfig {
        peer_code: dir.join("viewer_code"),
        pair_timeout: Duration::from_secs(30),
        session_timeout: Duration::from_secs(40),
        grants: Permissions::NONE.with(Permission::Control).with(Permission::Audio),
    };
    let mut viewer_cfg = ViewerConfig::new(
        &format!("127.0.0.1:{session_port}"),
        dir.join("host_code"),
        dir.join("go"),
        dir.join("anchor"),
    );
    viewer_cfg.pair_timeout = Duration::from_secs(30);

    let (host_result, viewer_result) = std::thread::scope(|scope| {
        let host_task = scope.spawn(|| {
            host_drive(&host_ui, &host_cfg, &|l| {
                host_lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(l.to_string())
            })
        });
        let viewer_task = scope.spawn(|| {
            viewer_drive(&viewer_ui, &media, &viewer_cfg, &|l| {
                viewer_lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(l.to_string())
            })
        });
        // The orchestrator: codes cross, then the go.
        let (mut sent_host, mut sent_viewer, mut sent_go, mut sent_anchor) = (false, false, false, false);
        let end = Instant::now() + Duration::from_secs(90);
        while !(host_task.is_finished() && viewer_task.is_finished()) && Instant::now() < end {
            if !sent_viewer && let Some(code) = field(&viewer_lines, "VIEWER_CODE ") {
                std::fs::write(dir.join("viewer_code"), code).expect("write");
                sent_viewer = true;
            }
            if !sent_host && let Some(code) = field(&host_lines, "HOST_CODE ") {
                std::fs::write(dir.join("host_code"), code).expect("write");
                sent_host = true;
            }
            if !sent_anchor
                && viewer_lines
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .any(|l| l == "VIEWER_NEED_ANCHOR")
            {
                // The synthetic desktop's pointer starts in the middle.
                std::fs::write(dir.join("anchor"), "32768,32768").expect("write");
                sent_anchor = true;
            }
            if !sent_go && field(&host_lines, "HOST_READY ").is_some() {
                std::fs::write(dir.join("go"), "go").expect("write");
                sent_go = true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        (host_task.join().expect("host task"), viewer_task.join().expect("viewer task"))
    });
    let (host_out, host_res) = host_result;
    let (report, viewer_res) = viewer_result;
    println!("host trace: {:?}", host.log.lines().iter().map(|l| l.text.clone()).collect::<Vec<_>>());
    println!("host lines: {:#?}", host_lines.lock().unwrap());
    println!("viewer lines: {:#?}", viewer_lines.lock().unwrap());
    println!("{}", report.counts_line());

    assert_eq!(host_res, Ok(()), "host script");
    assert_eq!(viewer_res, Ok(()), "viewer script");
    assert!(host_out.session_active && host_out.session_ended && host_out.forgotten, "{host_out:?}");
    assert!(report.frames_decoded >= 1, "frames decoded: {report:?}");
    assert_eq!(report.sizes, vec![(64, 48)]);
    assert!(report.first_frame_ms.is_some());
    assert_eq!(report.nonblack_pct, Some(100));
    assert!(report.audio_datagrams >= 1, "audio: {report:?}");
    assert_eq!(report.pointer_moves, 8);
    assert!(report.pointer_echo_samples >= 3, "the host's cursor followed: {report:?}");
    // The walk spans 20 px each way: an update that hides the pointer is not a position and adds nothing to it.
    assert!(report.pointer_max_deviation_px <= 24, "a hidden pointer counted as a position: {report:?}");
    assert!(report.ended_clean && report.forgotten);
    // Nothing stays paired on either side.
    assert_eq!(host.trust.read(|s| s.devices.len()), 0, "host forgot the viewer");
    assert_eq!(viewer.trust.read(|s| s.devices.len()), 0, "viewer forgot the host");

    drop((host_ui, viewer_ui));
    let _ = (host_serve.join(), viewer_serve.join());
    media.finish();
    host.shutdown();
    viewer.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn different_codes_end_the_pairing_without_a_device() {
    let dir = scratch_dir();
    let session_port = free_port_pair();
    let mut host = Machine::new("host-pc", Os::Windows, Some((session_port, session_port + 1)));
    host.attach_helpers();
    host.hub.set_console_session(Some(1));
    let viewer = Machine::new("viewer-mac", Os::MacOs, None);
    let media = Media::new(Box::new(|| -> Box<dyn VideoDecoder> { Box::new(LitDecoder) }) as DecoderFactory);
    let (host_ui, host_serve) = ui(&host.hub, None);
    let (viewer_ui, viewer_serve) = ui(&viewer.hub, Some(media.sink()));

    let host_cfg = HostConfig {
        peer_code: dir.join("viewer_code"),
        pair_timeout: Duration::from_secs(20),
        session_timeout: Duration::from_secs(5),
        grants: Permissions::NONE.with(Permission::Control),
    };
    let mut viewer_cfg = ViewerConfig::new(
        &format!("127.0.0.1:{session_port}"),
        dir.join("host_code"),
        dir.join("go"),
        dir.join("anchor"),
    );
    viewer_cfg.pair_timeout = Duration::from_secs(20);
    // Wrong codes on both sides: neither user confirms.
    std::fs::write(dir.join("viewer_code"), "000000").expect("write");
    std::fs::write(dir.join("host_code"), "000000").expect("write");

    let (host_result, viewer_result) = std::thread::scope(|scope| {
        let h = scope.spawn(|| host_drive(&host_ui, &host_cfg, &|_| {}));
        let v = scope.spawn(|| viewer_drive(&viewer_ui, &media, &viewer_cfg, &|_| {}));
        (h.join().expect("host"), v.join().expect("viewer"))
    });
    assert!(host_result.1.as_ref().is_err_and(|e| e.contains("codes differ")), "{:?}", host_result.1);
    assert!(viewer_result.1.as_ref().is_err_and(|e| e.contains("codes differ")), "{:?}", viewer_result.1);
    assert_eq!(host.trust.read(|s| s.devices.len()), 0);
    assert_eq!(viewer.trust.read(|s| s.devices.len()), 0);

    drop((host_ui, viewer_ui));
    let _ = (host_serve.join(), viewer_serve.join());
    media.finish();
    host.shutdown();
    viewer.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

/// Pairs `viewer_ui` with `host_ui` by hand (the ceremony of the drivers, without their forget): the device ids as
/// `(on the host, on the viewer)`.
fn pair_by_hand(host_ui: &ServiceBackend, viewer_ui: &ServiceBackend, host_addr: &str) -> (String, String) {
    const T: Duration = Duration::from_secs(30);
    let (mut hp, mut vp) = (Pump::new(host_ui), Pump::new(viewer_ui));
    let Reply::PairingId(host_pairing) = hp.ask(Command::PairOpen { device_id: None }, T).expect("host open") else {
        panic!("host pairing id")
    };
    let Reply::Device(placeholder) =
        vp.ask(Command::AddByAddress { ipv4: host_addr.into() }, T).expect("add by address")
    else {
        panic!("device")
    };
    let Reply::PairingId(viewer_pairing) = vp.ask(Command::PairOpen { device_id: None }, T).expect("viewer open")
    else {
        panic!("viewer pairing id")
    };
    vp.ask(Command::PairStart { pairing_id: viewer_pairing, device_id: placeholder.id }, T).expect("start");
    let show = |p: &mut Pump<'_>, id: u32| {
        p.wait("the code", T, |e| match e {
            UiEvent::Pairing { pairing_id, state: PairingState::ShowCode { .. } } if *pairing_id == id => Some(()),
            _ => None,
        })
        .expect("code shown");
    };
    show(&mut hp, host_pairing);
    show(&mut vp, viewer_pairing);
    hp.ask(Command::PairConfirm { pairing_id: host_pairing, matches: true }, T).expect("host confirm");
    vp.ask(Command::PairConfirm { pairing_id: viewer_pairing, matches: true }, T).expect("viewer confirm");
    let done = |p: &mut Pump<'_>, id: u32| {
        p.wait("the pairing to finish", T, |e| match e {
            UiEvent::Pairing { pairing_id, state: PairingState::Done { device_id } } if *pairing_id == id => {
                Some(device_id.clone())
            }
            _ => None,
        })
        .expect("pairing done")
    };
    (done(&mut hp, host_pairing), done(&mut vp, viewer_pairing))
}

#[test]
fn a_paired_device_is_driven_without_pairing_again_and_stays_paired() {
    let session_port = free_port_pair();
    let mut host = Machine::new("host-pc", Os::Windows, Some((session_port, session_port + 1)));
    host.attach_helpers();
    host.hub.set_console_session(Some(1));
    let viewer = Machine::new("viewer-mac", Os::MacOs, None);
    let media = Media::new(Box::new(|| -> Box<dyn VideoDecoder> { Box::new(LitDecoder) }) as DecoderFactory);
    let (host_ui, host_serve) = ui(&host.hub, None);
    let (viewer_ui, viewer_serve) = ui(&viewer.hub, Some(media.sink()));

    let (_host_side, viewer_side) = pair_by_hand(&host_ui, &viewer_ui, &format!("127.0.0.1:{session_port}"));
    assert_eq!(host.trust.read(|s| s.devices.len()), 1, "paired on the host");
    assert_eq!(viewer.trust.read(|s| s.devices.len()), 1, "paired on the viewer");

    // No orchestrator files exist: the paired mode needs none.
    let mut cfg = ViewerConfig::new(
        "unused",
        PathBuf::from("no-such-peer-code"),
        PathBuf::from("no-such-go"),
        PathBuf::from("no-such-anchor"),
    );
    cfg.paired = Some(viewer_side);
    cfg.hold = Duration::from_secs(2);
    let lines = Mutex::new(Vec::<String>::new());
    let grants = Permissions::NONE.with(Permission::Control).with(Permission::Audio);

    let (report, result) = std::thread::scope(|scope| {
        // The host's user: accepts the one request.
        let accept = scope.spawn(|| {
            let mut pump = Pump::new(&host_ui);
            let request_id = pump
                .wait("the request", Duration::from_secs(40), |e| match e {
                    UiEvent::IncomingRequest(r) => Some(r.request_id),
                    _ => None,
                })
                .expect("a request");
            pump.ask(
                Command::AnswerIncoming { request_id, decision: Decision::Accept { grants } },
                Duration::from_secs(10),
            )
            .expect("answer");
        });
        let out = viewer_drive(&viewer_ui, &media, &cfg, &|l| {
            lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(l.to_string())
        });
        accept.join().expect("accept task");
        out
    });
    println!("viewer lines: {:#?}", lines.lock().unwrap());
    println!("{}", report.counts_line());
    assert_eq!(result, Ok(()), "viewer script");
    assert!(report.session_active && report.ended_clean);
    assert!(!report.forgotten, "a paired device is not forgotten");
    assert!(report.frames_decoded >= 1 && report.audio_datagrams >= 1, "{report:?}");
    assert_eq!(report.nonblack_pct, Some(100));
    assert_eq!(report.pointer_moves, 8);
    assert!(report.pointer_echo_samples >= 3, "{report:?}");
    {
        let l = lines.lock().unwrap();
        assert!(l.iter().any(|x| x.starts_with("VIEWER_USING_PAIRED")));
        assert!(!l.iter().any(|x| x == "VIEWER_PAIR_OPEN" || x == "VIEWER_FORGOTTEN" || x == "VIEWER_NEED_ANCHOR"));
    }
    assert_eq!(host.trust.read(|s| s.devices.len()), 1, "still paired on the host");
    assert_eq!(viewer.trust.read(|s| s.devices.len()), 1, "still paired on the viewer");

    drop((host_ui, viewer_ui));
    let _ = (host_serve.join(), viewer_serve.join());
    media.finish();
    host.shutdown();
    viewer.shutdown();
}
