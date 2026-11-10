//! The frame path alone, in this process, under a counting allocator: what one video frame costs in allocations,
//! allocated bytes and CPU time between the encoder's output and the viewer's sink.
//!
//! ```text
//!  run_helper (a tick source, an encoder that only makes a payload) ──pipe──► ServiceCore (host)
//!        ──QUIC on 127.0.0.1──► ServiceCore (viewer) ──► UiHub ──pipe──► ServiceBackend ──► a sink that counts
//! ```
//!
//! Both machines are the cores and hubs of `swapdesk-host` as the service runs them, with memory stores; the pipes are
//! anonymous OS pipes, so the bytes cross the kernel as they do on the named pipes and a write allocates nothing of
//! its own. No capture, no codec, no audio: every allocation counted is the path's (plus the cores' idle
//! housekeeping, which is small beside 60 frames a second). "Copies" cannot be counted directly; the bytes allocated
//! per frame divided by the payload size say how many payload-sized buffers a frame costs, which is what a copy is
//! made into.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{PipeReader, PipeWriter};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_fakes::{FakeNetInterfaces, MemorySecretStore, MonotonicClock, RecordingInputSink, SineSource};
use swapdesk_host::helper::{HelperDeps, run_helper};
use swapdesk_host::service::{ServiceConfig, ServiceCore, ServiceDeps};
use swapdesk_host::ui_hub::{ConnInfo, DevOptions, HubParams, MachineSettings, NoHooks, NoPresence, UiHub};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake, IpcRole};
use swapdesk_platform::{
    AdapterId, CaptureConfig, Clock, CursorUpdate, DisplayId, DisplayInfo, EncodedFrame, EncoderConfig, EncoderInfo,
    Frame, Level, Pixels, Rect, Result as PResult, ScreenSource, TraceSink, VideoEncoder,
};
use swapdesk_proto::{CursorPosition, CursorShape, H264Profile, Os, VideoHeader};
use swapdesk_trust::{Identity, MemoryAudit, SystemWallClock, TrustHandle, UserRef};
use swapdesk_ui::service_backend::{ServiceBackend, ViewerSink};
use swapdesk_ui::{Command, UiWaker};

use crate::proc;
use crate::stats::Fields;

// ---------------------------------------------------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------------------------------------------------

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting calls and bytes while [`COUNTING`] is set (a growing `realloc` counts as one
/// allocation of the bytes it adds).
pub struct Counting;

// SAFETY: every call goes to `System` unchanged; the counters are atomics.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size.saturating_sub(layout.size()) as u64, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract is `System`'s.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The source, the encoder and the sink
// ---------------------------------------------------------------------------------------------------------------------

/// A display that hands out a tiny frame at every slot of the frame rate.
struct TickScreen {
    fps: u16,
    clock: Arc<dyn Clock>,
    started: Instant,
    produced: u32,
}

impl ScreenSource for TickScreen {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        Ok(vec![DisplayInfo {
            id: DisplayId(0),
            name: "Tick".to_string(),
            bounds: Rect { x: 0, y: 0, width: 1920, height: 1080 },
            scale_milli: 1000,
            refresh_mhz: u32::from(self.fps) * 1000,
            rotation: 0,
            primary: true,
            adapter: AdapterId(0),
        }])
    }
    fn open(&mut self, _display: DisplayId, config: CaptureConfig) -> PResult<()> {
        self.fps = self.fps.min(config.max_fps).max(1);
        self.started = Instant::now();
        self.produced = 0;
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> PResult<Option<Frame>> {
        let due = self.started + Duration::from_secs_f64(f64::from(self.produced) / f64::from(self.fps));
        let wait = due.saturating_duration_since(Instant::now());
        if wait > timeout {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        std::thread::sleep(wait);
        self.produced += 1;
        Ok(Some(Frame {
            display: DisplayId(0),
            captured_at: self.clock.now(),
            width: 1920,
            height: 1080,
            pixels: Pixels::Cpu { stride: 32, data: vec![0; 256] },
            dirty: Vec::new(),
            content_changed: true,
        }))
    }
    fn reacquire(&mut self) -> PResult<()> {
        Ok(())
    }
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
    fn close(&mut self) {}
}

/// An encoder that only makes the payload: `bytes` per frame, a keyframe first and whenever one is asked for.
struct Payload {
    bytes: usize,
    n: u32,
}

impl VideoEncoder for Payload {
    fn configure(&mut self, _config: &EncoderConfig) -> PResult<EncoderInfo> {
        self.n = 0;
        Ok(EncoderInfo {
            name: "payload".into(),
            hardware: false,
            profile: H264Profile::Main,
            level: 42,
            low_latency: true,
        })
    }
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        let first = self.n == 0;
        self.n += 1;
        Ok(vec![EncodedFrame {
            captured_at: frame.captured_at,
            encode_us: 1,
            keyframe: first || force_keyframe,
            config_change: first,
            width: 1920,
            height: 1080,
            annex_b: vec![0x5A; self.bytes],
        }])
    }
    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        Ok(())
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct Counter {
    frames: AtomicU64,
    bytes: AtomicU64,
}

impl ViewerSink for Counter {
    fn video(&self, _session_id: u64, _header: VideoHeader, payload: Vec<u8>) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
    }
    fn audio(&self, _session_id: u64, _datagram: Vec<u8>) {}
    fn cursor_pos(&self, _session_id: u64, _pos: CursorPosition) {}
    fn cursor_shape(&self, _session_id: u64, _shape: CursorShape) {}
}

struct NoTrace;

impl TraceSink for NoTrace {
    fn log(&self, _level: Level, _target: &'static str, _what: core::fmt::Arguments<'_>) {}
}

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

// ---------------------------------------------------------------------------------------------------------------------
// One machine, as the service wires it
// ---------------------------------------------------------------------------------------------------------------------

struct End {
    reader: PipeReader,
    writer: PipeWriter,
}

/// Two ends of a two-way pipe made of two anonymous OS pipes.
fn duplex() -> std::io::Result<(End, End)> {
    let (a_read, b_write) = std::io::pipe()?;
    let (b_read, a_write) = std::io::pipe()?;
    Ok((End { reader: a_read, writer: a_write }, End { reader: b_read, writer: b_write }))
}

struct Machine {
    core: Arc<ServiceCore>,
    hub: UiHub,
    helpers: Vec<JoinHandle<()>>,
}

/// A session port with the pairing port right after it, both free, below the range Windows hands out by itself.
fn free_port_pair() -> Result<u16, String> {
    let start = 43_000 + (std::process::id() % 1_500) as u16 * 2;
    for p in (start..47_000).step_by(2) {
        if UdpSocket::bind((Ipv4Addr::LOCALHOST, p)).is_ok() && UdpSocket::bind((Ipv4Addr::LOCALHOST, p + 1)).is_ok() {
            return Ok(p);
        }
    }
    Err("no two free UDP ports in a row".into())
}

impl Machine {
    fn new(name: &str, ports: Option<(u16, u16)>) -> Result<Self, String> {
        let identity = Arc::new(Identity::generate(0).map_err(|e| format!("identity: {e:?}"))?);
        let secrets = Arc::new(MemorySecretStore::new());
        let trust = TrustHandle::open(secrets.clone()).map_err(|e| format!("trust store: {e:?}"))?;
        let interfaces = Arc::new(FakeNetInterfaces::new(Vec::new()));
        let clock = Arc::new(MonotonicClock::new());
        let (session_port, pair_port) = ports.unwrap_or((0, 0));
        let hub = UiHub::new(HubParams {
            os: Os::Windows,
            os_version: "measure".into(),
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            secrets,
            settings: MachineSettings::new(name),
            interfaces: interfaces.clone(),
            presence: Arc::new(NoPresence),
            audit: Arc::new(MemoryAudit::new()),
            clock: clock.clone(),
            wall: Arc::new(SystemWallClock),
            trace: Arc::new(NoTrace),
            hooks: Arc::new(NoHooks),
            dev: Some(DevOptions { pair_listen: vec![SocketAddrV4::new(Ipv4Addr::LOCALHOST, pair_port)] }),
        });
        let deps = ServiceDeps { identity, trust, clock, trace: Arc::new(NoTrace), ui: hub.host_ui(), interfaces };
        let mut cfg = ServiceConfig::new(name, Os::Windows, "measure", deps);
        cfg.session_port = session_port;
        let cfg = cfg.dev_loopback(SocketAddrV4::new(Ipv4Addr::LOCALHOST, session_port));
        let core = Arc::new(ServiceCore::start(cfg).map_err(|e| format!("the core of {name}: {e:?}"))?);
        hub.bind_core(&core);
        hub.set_console_session(Some(1));
        Ok(Self { core, hub, helpers: Vec::new() })
    }

    /// The system and the user helper of a host, as `run_helper` loops on OS pipes.
    fn attach_helpers(&mut self, fps: u16, payload: usize) -> Result<(), String> {
        for role in [IpcRole::HelperSystem, IpcRole::HelperUser] {
            let (core_end, helper_end) = duplex().map_err(|e| e.to_string())?;
            let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
            let (screen_clock, audio_clock) = (Arc::clone(&clock), Arc::clone(&clock));
            let deps = HelperDeps::new(
                move |_| Ok(TickScreen { fps, clock: screen_clock.clone(), started: Instant::now(), produced: 0 }),
                move |_frame, _config, _backend| Ok(Payload { bytes: payload, n: 0 }),
                || Ok(RecordingInputSink::new()),
                move || Ok(SineSource::new(audio_clock.clone()).realtime()),
                clock,
            );
            self.helpers.push(std::thread::spawn(move || {
                let _ = run_helper(helper_end.reader, helper_end.writer, deps, role, std::process::id());
            }));
            self.core.attach_helper(role, core_end.reader, core_end.writer).map_err(|e| format!("attach: {e:?}"))?;
        }
        Ok(())
    }

    /// A UI connection, as the shells make one: the hub serves one end, a `ServiceBackend` drives the other.
    fn ui(&self, sink: Option<Arc<dyn ViewerSink>>) -> Result<Arc<ServiceBackend>, String> {
        let (client_end, server_end) = duplex().map_err(|e| e.to_string())?;
        let hub = self.hub.clone();
        std::thread::spawn(move || {
            let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
            if Handshake::server(&mut reader, &mut writer, 1).is_err() {
                return;
            }
            let user = UserRef { id: "S-1-5-21-1".into(), name: "measure".into() };
            let _ = hub.serve(reader, writer, ConnInfo { session_id: 1, user, admin: false });
        });
        ServiceBackend::start_with_sink(client_end.reader, client_end.writer, Arc::new(NoWake), 1, sink)
            .map(Arc::new)
            .map_err(|e| format!("UI handshake: {e}"))
    }

    fn shutdown(self) {
        let Machine { core, hub, helpers } = self;
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

// ---------------------------------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------------------------------

pub struct PathConfig {
    pub seconds: u64,
    /// Bytes of every frame's payload.
    pub payload: usize,
    pub fps: u16,
}

pub fn run(cfg: &PathConfig) -> Result<Fields, String> {
    let port = free_port_pair()?;
    let mut host = Machine::new("measure-host", Some((port, port + 1)))?;
    host.attach_helpers(cfg.fps, cfg.payload)?;
    let viewer = Machine::new("measure-viewer", None)?;
    let counter = Arc::new(Counter::default());
    let host_ui = host.ui(None)?;
    let viewer_ui = viewer.ui(Some(counter.clone() as Arc<dyn ViewerSink>))?;

    let device = crate::run::pair(&host_ui, &viewer_ui, &format!("127.0.0.1:{port}"))?;
    let session_id = crate::run::open_session(&host_ui, &viewer_ui, &device, false)?;
    let measured = (|| {
        let end = Instant::now() + Duration::from_secs(20);
        while counter.frames.load(Ordering::Relaxed) < u64::from(cfg.fps) {
            if Instant::now() >= end {
                return Err("no video reached the viewer's sink".to_string());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The first second is over (handshakes, the first keyframe, queues growing to their size).
        std::thread::sleep(Duration::from_secs(1));
        let me = std::process::id();
        let cpu0 = proc::cpu_seconds(me).unwrap_or(0.0);
        let (frames0, bytes0) = (counter.frames.load(Ordering::Relaxed), counter.bytes.load(Ordering::Relaxed));
        ALLOCATIONS.store(0, Ordering::SeqCst);
        ALLOCATED_BYTES.store(0, Ordering::SeqCst);
        COUNTING.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_secs(cfg.seconds));
        COUNTING.store(false, Ordering::SeqCst);
        let wall = t0.elapsed().as_secs_f64();
        let frames = counter.frames.load(Ordering::Relaxed) - frames0;
        let bytes = counter.bytes.load(Ordering::Relaxed) - bytes0;
        let cpu = proc::cpu_seconds(me).unwrap_or(0.0) - cpu0;
        Ok((wall, frames, bytes, cpu, ALLOCATIONS.load(Ordering::SeqCst), ALLOCATED_BYTES.load(Ordering::SeqCst)))
    })();
    let _ = crossdrive_ask(&viewer_ui, Command::Disconnect { session_id });
    drop((host_ui, viewer_ui));
    host.shutdown();
    viewer.shutdown();
    let (wall, frames, bytes, cpu, allocations, allocated) = measured?;

    let per_frame = |v: f64| if frames == 0 { f64::NAN } else { v / frames as f64 };
    let mut f = Fields::new();
    f.text("config", format!("frame path, {} bytes at {} fps", cfg.payload, cfg.fps));
    f.num("seconds", wall);
    f.num("path_frames", frames as f64);
    f.num("path_fps", frames as f64 / wall);
    f.num("path_payload_bytes", per_frame(bytes as f64));
    f.num("path_allocations_per_frame", per_frame(allocations as f64));
    f.num("path_allocated_bytes_per_frame", per_frame(allocated as f64));
    f.num("path_payload_copies_per_frame", per_frame(allocated as f64) / cfg.payload as f64);
    f.num("path_cpu_us_per_frame", per_frame(cpu * 1e6));
    f.num("path_cpu_core_pct", cpu / wall * 100.0);
    let expected = cfg.seconds * u64::from(cfg.fps);
    if frames * 10 < expected * 8 {
        f.text("valid", format!("no: {frames} frames reached the sink of {expected} expected"));
    } else {
        f.text("valid", "yes");
    }
    Ok(f)
}

fn crossdrive_ask(backend: &ServiceBackend, command: Command) -> Result<(), String> {
    swapdesk_crossdrive::Pump::new(backend).ask(command, Duration::from_secs(5)).map(|_| ())
}
