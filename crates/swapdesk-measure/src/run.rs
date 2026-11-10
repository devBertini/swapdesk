//! One measured session through the whole chain, on this PC:
//!
//! ```text
//!  helper stand-in (synthetic screen, hardware encoder, sine) ──pipe──► swapdesk-service (host instance)
//!        ──QUIC on 127.0.0.1──► swapdesk-service (viewer instance) ──UI pipe──► this process:
//!        ViewerPipeline (Media Foundation decoder, WinRenderer) in a window of its own, AudioPlayer over a sink
//!        that finds the markers
//! ```
//!
//! The two services are the real `swapdesk-service.exe --console --instance <n> --dev-loopback`; each starts its
//! helpers itself. This process plays both users through the real UI pipes (pairing, the request, the answer) and is
//! the viewer. What is read: the barcode of every presented picture (B1, B2), the arrival and present times of every
//! frame (the stages), the marker onsets against the source's marker log (B3), this process's private working set
//! (B9) and the CPU time of the host's service and helpers (B10).

use std::collections::HashMap;
use std::fs::File;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_app::shared::audio::AudioPlayer;
use swapdesk_app::viewer::{PipelineStats, PresentEvent, ProbeTap, ViewerOptions, ViewerPipeline};
use swapdesk_capture_win::QpcClock;
use swapdesk_crossdrive::Pump;
use swapdesk_fakes::BarcodeReader;
use swapdesk_platform::{AudioBlock, AudioSink, Clock, Rect, ScaleMode};
use swapdesk_proto::{CursorPosition, CursorShape, VideoHeader};
use swapdesk_render_win::Probe;
use swapdesk_render_win::layout::Layout;
use swapdesk_shell_win::ViewerHost;
use swapdesk_sys_win::pipe::{connect, session_of_pid, ui_pipe_name};
use swapdesk_ui::service_backend::{ServiceBackend, ViewerSink};
use swapdesk_ui::{
    Command, Decision, Intent, PairingState, Permission, Permissions, Reply, Role, SessionState, UiBackend, UiEvent,
    UiWaker,
};

use crate::harness::{Pref, ShellStub, monitors, window_monitor};
use crate::proc;
use crate::source::{Family, SPEC_VAR, SourceSpec, adapter_of, cost_file};
use crate::stats::{Fields, Series};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const WAIT: Duration = Duration::from_secs(20);
/// The barcode of a frame at least 512 pixels wide: 32 cells by 4 rows of 16 pixels.
const BARCODE: (u32, u32) = (512, 64);
/// The session and pairing UDP ports of instance 0 (`--instance n` adds `10 * n` to both).
const SESSION_PORT: u16 = 47820;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u16,
    pub family: Family,
    /// Seconds measured, after `warmup`.
    pub seconds: u64,
    /// Seconds of session before the measured window (the decoder, the renderer and the jitter buffer settle).
    pub warmup: u64,
    /// Where the logs of this run go.
    pub logs: PathBuf,
    /// The two `--instance` numbers (viewer, host).
    pub instances: (u16, u16),
    /// The service executable to measure; `None` looks for the build of this checkout.
    pub service: Option<PathBuf>,
    /// Put the viewer's window on `\\.\DISPLAYn`; `None` is the primary monitor.
    pub display: Option<u32>,
    /// The adapter of the viewer's device: the one the shell picks (minimum power), or the high-performance one
    /// (`SWAPDESK_ADAPTER=high` in the application).
    pub viewer_high: bool,
    /// Leave the viewer's window in the background instead of giving it the foreground: what a viewer shows while
    /// its user works in another window.
    pub background: bool,
}

// ---------------------------------------------------------------------------------------------------------------------
// The services
// ---------------------------------------------------------------------------------------------------------------------

fn dev_dir(instance: u16) -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("%LOCALAPPDATA% is not set")?;
    Ok(PathBuf::from(base).join("Swapdesk-dev").join(instance.to_string()))
}

/// Deletes what an earlier run left in a dev folder, file by file (nothing but this instance's own tree).
fn clear_dir(dir: &Path) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(()) };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            clear_dir(&p)?;
            let _ = std::fs::remove_dir(&p);
        } else {
            std::fs::remove_file(&p).map_err(|err| format!("cannot delete {}: {err}", p.display()))?;
        }
    }
    Ok(())
}

/// The service as it ships and this executable under the helper's name, in a folder of their own: the service starts
/// `swapdesk-helper.exe` from its own folder. The service is `service` when given, else the `release-service` build
/// (the profile the shipped service is built with) next to this build's folder, else the one beside this executable.
fn stage(service: Option<&Path>) -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = me.parent().ok_or("this executable has no folder")?;
    let candidates =
        [dir.join("..").join("release-service").join("swapdesk-service.exe"), dir.join("swapdesk-service.exe")];
    let service = match service {
        Some(s) => s.to_path_buf(),
        None => candidates.iter().find(|c| c.exists()).cloned().ok_or_else(|| {
            format!(
                "no swapdesk-service.exe at {} or {}: build it first",
                candidates[0].display(),
                candidates[1].display()
            )
        })?,
    };
    let stage = dir.join("measure-stage");
    std::fs::create_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let staged = stage.join("swapdesk-service.exe");
    for (from, to) in [(&service, &staged), (&me, &stage.join("swapdesk-helper.exe"))] {
        std::fs::copy(from, to).map_err(|e| format!("copy {} to {}: {e}", from.display(), to.display()))?;
    }
    Ok(staged)
}

struct Service {
    instance: u16,
    child: Child,
    dir: PathBuf,
    helpers: Vec<u32>,
}

impl Service {
    fn start(exe: &Path, instance: u16, logs: &Path, spec: &SourceSpec) -> Result<Self, String> {
        let dir = dev_dir(instance)?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        clear_dir(&dir)?;
        let out = File::create(logs.join(format!("service-{instance}.out"))).map_err(|e| e.to_string())?;
        let err = out.try_clone().map_err(|e| e.to_string())?;
        let child = Process::new(exe)
            .args(["--console", "--instance", &instance.to_string(), "--dev-loopback"])
            .env(SPEC_VAR, spec.to_env())
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("start {}: {e}", exe.display()))?;
        Ok(Self { instance, child, dir, helpers: Vec::new() })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn trace(&self) -> String {
        std::fs::read_to_string(self.dir.join("trace.log")).unwrap_or_default()
    }

    /// The helpers the service said it started, by PID.
    fn refresh_helpers(&mut self) -> &[u32] {
        for line in self.trace().lines() {
            if let Some(rest) = line.split("helper started (pid ").nth(1)
                && let Some(pid) = rest.split(')').next().and_then(|n| n.trim().parse::<u32>().ok())
                && !self.helpers.contains(&pid)
            {
                self.helpers.push(pid);
            }
        }
        &self.helpers
    }

    /// The encoder the host's helper reported for the stream: its name, without the size and the rate that follow.
    fn encoder_line(&self) -> Option<String> {
        self.trace()
            .lines()
            .rev()
            .find_map(|l| l.split("encoding with ").nth(1))
            .map(|s| s.split("), ").next().map_or_else(|| s.trim().to_string(), |name| format!("{})", name.trim())))
    }

    fn keep_trace(&self, logs: &Path) {
        let _ = std::fs::copy(self.dir.join("trace.log"), logs.join(format!("service-{}-trace.log", self.instance)));
    }

    /// Asks the service to stop (its stop file) and waits for it; what is still alive after that is ended.
    fn stop(&mut self) {
        self.refresh_helpers();
        let _ = std::fs::write(self.dir.join("stop"), b"stop");
        let end = Instant::now() + Duration::from_secs(8);
        while Instant::now() < end && self.child.try_wait().ok().flatten().is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        for &pid in &self.helpers {
            proc::kill_if_image(pid, "swapdesk-helper.exe");
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

/// A UI connection on the real UI pipe of `service`, as `Swapdesk.exe` makes one.
fn ui(service: &Service, sink: Option<Arc<dyn ViewerSink>>) -> Result<Arc<ServiceBackend>, String> {
    let session = session_of_pid(std::process::id()).map_err(|e| e.to_string())?;
    let name = format!("{}-{}", ui_pipe_name(session), service.instance);
    let end = Instant::now() + WAIT;
    let conn = loop {
        match connect(&name, service.pid(), Duration::from_secs(1)) {
            Ok(c) => break c,
            Err(e) if Instant::now() >= end => return Err(format!("the UI pipe {name}: {e}")),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let (reader, writer) = conn.split().map_err(|e| e.to_string())?;
    ServiceBackend::start_with_sink(reader, writer, Arc::new(NoWake), std::process::id(), sink)
        .map(Arc::new)
        .map_err(|e| format!("handshake on {name}: {e}"))
}

// ---------------------------------------------------------------------------------------------------------------------
// What the viewer records
// ---------------------------------------------------------------------------------------------------------------------

/// A frame as it reached this process.
#[derive(Clone, Copy)]
struct Arrival {
    capture_us: u64,
    encode_us: u32,
    arrived_us: u64,
    bytes: u32,
    keyframe: bool,
}

/// One presented picture whose barcode was read.
#[derive(Clone, Copy)]
struct Sample {
    counter: u32,
    /// The barcode's time: the slot of the frame on the source.
    captured_us: u64,
    presented_us: u64,
    arrival: Option<Arrival>,
    decode_us: u32,
    lag_us: u32,
    wait_us: u32,
    blt_us: u32,
    present_us: u32,
}

#[derive(Default)]
struct Record {
    pending: Mutex<HashMap<u32, Arrival>>,
    arrivals: Mutex<Vec<Arrival>>,
    samples: Mutex<Vec<Sample>>,
    /// Presented pictures whose barcode failed its checksum.
    unreadable: AtomicU32,
    audio_datagrams: AtomicU64,
    onsets: Mutex<Vec<u64>>,
    /// Gaps the audio sink found with nothing queued: when, on the machine's clock.
    audio_gaps: Mutex<Vec<u64>>,
}

impl Record {
    /// The probe holds the barcode's rectangle as it was drawn in the window (scaled when the picture is): it is
    /// sampled back to the barcode's own size and read with its checksum.
    fn on_probe(&self, event: &PresentEvent, probe: &Probe) {
        let (bw, bh) = BARCODE;
        let mut canon = vec![0u8; (bw * bh * 4) as usize];
        for y in 0..bh {
            let sy = ((u64::from(y) * 2 + 1) * u64::from(probe.height) / (u64::from(bh) * 2)) as u32;
            for x in 0..bw {
                let sx = ((u64::from(x) * 2 + 1) * u64::from(probe.width) / (u64::from(bw) * 2)) as u32;
                let from = (sy * probe.stride + sx * 4) as usize;
                let to = ((y * bw + x) * 4) as usize;
                if let Some(px) = probe.data.get(from..from + 4) {
                    canon[to..to + 4].copy_from_slice(px);
                }
            }
        }
        let arrival = lock(&self.pending).remove(&event.frame_id);
        let Some(reading) = BarcodeReader::new().read_bgra(&canon, bw * 4, bw, bh) else {
            self.unreadable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        lock(&self.samples).push(Sample {
            counter: reading.counter,
            captured_us: reading.captured_at.0,
            presented_us: event.presented_at_us,
            arrival,
            decode_us: event.decode_us,
            lag_us: event.lag_us,
            wait_us: event.wait_us,
            blt_us: event.blt_us,
            present_us: event.present_us,
        });
    }
}

/// The sink of the viewer's UI connection: every frame is noted with its arrival time and goes on to the pipeline;
/// the audio goes to a player of its own, the same `AudioPlayer` the pipeline uses, over [`MarkerSink`].
struct Tap {
    record: Arc<Record>,
    pipeline: Arc<ViewerPipeline>,
    audio: AudioPlayer,
}

impl ViewerSink for Tap {
    fn video(&self, session_id: u64, header: VideoHeader, payload: Vec<u8>) {
        let arrival = Arrival {
            capture_us: header.capture_us,
            encode_us: header.encode_us,
            arrived_us: QpcClock.now().0,
            bytes: header.payload_len,
            keyframe: header.keyframe,
        };
        {
            let mut pending = lock(&self.record.pending);
            // Frames the viewer never presents (skipped for a newer one) would stay here for ever.
            if pending.len() > 600 {
                let oldest = arrival.arrived_us.saturating_sub(5_000_000);
                pending.retain(|_, a| a.arrived_us >= oldest);
            }
            pending.insert(header.frame_id, arrival);
        }
        lock(&self.record.arrivals).push(arrival);
        self.pipeline.video(session_id, header, payload);
    }

    fn audio(&self, _session_id: u64, datagram: Vec<u8>) {
        self.record.audio_datagrams.fetch_add(1, Ordering::Relaxed);
        self.audio.push(datagram);
    }

    fn cursor_pos(&self, session_id: u64, pos: CursorPosition) {
        self.pipeline.cursor_pos(session_id, pos);
    }

    fn cursor_shape(&self, session_id: u64, shape: CursorShape) {
        self.pipeline.cursor_shape(session_id, shape);
    }
}

/// The audio sink of the measurement (the method of `RecordingAudioSink`, C7, without keeping the audio): it plays
/// on the machine's clock, a block written with nothing queued starts at once and otherwise right after what is
/// queued, and a marker onset is the first frame over the threshold with none in the 10 ms before it.
struct MarkerSink {
    record: Arc<Record>,
    started: bool,
    written_any: bool,
    frames: u64,
    play_end: u64,
    last_loud: Option<u64>,
}

impl MarkerSink {
    const RATE: u64 = 48_000;
    const THRESHOLD: f32 = 0.5;
    const QUIET_FRAMES: u64 = 480;

    fn new(record: Arc<Record>) -> Self {
        Self { record, started: false, written_any: false, frames: 0, play_end: 0, last_loud: None }
    }
}

impl AudioSink for MarkerSink {
    fn start(&mut self) -> swapdesk_platform::Result<()> {
        self.started = true;
        Ok(())
    }

    fn write(&mut self, block: &AudioBlock) -> swapdesk_platform::Result<()> {
        let now = QpcClock.now().0;
        let play_start = if !self.written_any {
            now
        } else if now > self.play_end {
            lock(&self.record.audio_gaps).push(now);
            now
        } else {
            self.play_end
        };
        self.written_any = true;
        let frames = (block.samples.len() / 2) as u64;
        for i in 0..frames {
            let g = self.frames + i;
            if block.samples[i as usize * 2].abs() > Self::THRESHOLD {
                if self.last_loud.is_none_or(|l| g - l > Self::QUIET_FRAMES) {
                    lock(&self.record.onsets).push(play_start + i * 1_000_000 / Self::RATE);
                }
                self.last_loud = Some(g);
            }
        }
        self.frames += frames;
        self.play_end = play_start + frames * 1_000_000 / Self::RATE;
        Ok(())
    }

    fn queued(&self) -> Duration {
        Duration::from_micros(self.play_end.saturating_sub(QpcClock.now().0))
    }

    fn stop(&mut self) {
        self.started = false;
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The two users
// ---------------------------------------------------------------------------------------------------------------------

/// The pairing ceremony as two users run it: both open "Parear", the viewer names the host by its address, both see
/// the code and confirm. Returns the host's device id as the viewer knows it.
pub fn pair(host: &ServiceBackend, viewer: &ServiceBackend, host_addr: &str) -> Result<String, String> {
    let (mut hp, mut vp) = (Pump::new(host), Pump::new(viewer));
    let Reply::PairingId(host_pairing) = hp.ask(Command::PairOpen { device_id: None }, WAIT)? else {
        return Err("the host's PairOpen did not reply a pairing id".into());
    };
    let Reply::Device(placeholder) = vp.ask(Command::AddByAddress { ipv4: host_addr.into() }, WAIT)? else {
        return Err("AddByAddress did not reply a device".into());
    };
    let Reply::PairingId(viewer_pairing) = vp.ask(Command::PairOpen { device_id: None }, WAIT)? else {
        return Err("the viewer's PairOpen did not reply a pairing id".into());
    };
    vp.ask(Command::PairStart { pairing_id: viewer_pairing, device_id: placeholder.id }, WAIT)?;
    let shown = |p: &mut Pump<'_>, id: u32| {
        p.wait("the pairing code", WAIT, |e| match e {
            UiEvent::Pairing { pairing_id, state: PairingState::ShowCode { .. } } if *pairing_id == id => Some(Ok(())),
            UiEvent::Pairing { pairing_id, state: PairingState::Failed { reason } } if *pairing_id == id => {
                Some(Err(format!("pairing failed: {reason:?}")))
            }
            _ => None,
        })?
    };
    shown(&mut hp, host_pairing)?;
    shown(&mut vp, viewer_pairing)?;
    hp.ask(Command::PairConfirm { pairing_id: host_pairing, matches: true }, WAIT)?;
    vp.ask(Command::PairConfirm { pairing_id: viewer_pairing, matches: true }, WAIT)?;
    let done = |p: &mut Pump<'_>, id: u32| {
        p.wait("the pairing to finish", WAIT, |e| match e {
            UiEvent::Pairing { pairing_id, state: PairingState::Done { device_id } } if *pairing_id == id => {
                Some(Ok(device_id.clone()))
            }
            UiEvent::Pairing { pairing_id, state: PairingState::Failed { reason } } if *pairing_id == id => {
                Some(Err(format!("pairing failed: {reason:?}")))
            }
            _ => None,
        })?
    };
    done(&mut hp, host_pairing)?;
    done(&mut vp, viewer_pairing)
}

/// The viewer asks for a session with control, the host's user accepts with control (and audio when `audio`, which
/// the viewer then turns on); the session id.
pub fn open_session(host: &ServiceBackend, viewer: &ServiceBackend, device: &str, audio: bool) -> Result<u64, String> {
    let (mut hp, mut vp) = (Pump::new(host), Pump::new(viewer));
    let ticket = viewer.send(Command::Connect { device_id: device.into(), intent: Intent::Control, unattended: false });
    let request_id = hp.wait("the viewer's request", WAIT, |e| match e {
        UiEvent::IncomingRequest(r) => Some(r.request_id),
        _ => None,
    })?;
    let grants = Permissions::NONE.with(Permission::Control);
    let grants = if audio { grants.with(Permission::Audio) } else { grants };
    hp.ask(Command::AnswerIncoming { request_id, decision: Decision::Accept { grants } }, WAIT)?;
    // The reply is the id of the connect's entry; the session has its own id, the one of its first `Active`.
    let entry = vp.wait("the reply to Connect", WAIT, |e| match e {
        UiEvent::Reply { ticket: t, result } if *t == ticket => Some(match result {
            Ok(Reply::SessionId(id)) => Ok(*id),
            Ok(other) => Err(format!("Connect replied {other:?}")),
            Err(e) => Err(format!("Connect refused: {:?}", e.code)),
        }),
        _ => None,
    })??;
    let session_id = vp.wait("the session to go active", WAIT, |e| match e {
        UiEvent::Session { session_id: s, state: SessionState::Active { role: Role::Viewer, .. }, .. } => Some(Ok(*s)),
        UiEvent::Session { session_id: s, state: SessionState::NotOpened { why }, .. } if *s == entry => {
            Some(Err(format!("the connect did not open a session: {why:?}")))
        }
        _ => None,
    })??;
    if audio {
        vp.ask(Command::SetAudio { session_id, on: true }, WAIT)?;
    }
    Ok(session_id)
}

fn close_session(host: &ServiceBackend, viewer: &ServiceBackend, session_id: u64, device: &str) {
    let (mut hp, mut vp) = (Pump::new(host), Pump::new(viewer));
    let short = Duration::from_secs(5);
    let _ = vp.ask(Command::Disconnect { session_id }, short);
    let _ = vp.wait("the session to end", short, |e| match e {
        UiEvent::Session { session_id: s, state: SessionState::Ended { .. }, .. } if *s == session_id => Some(()),
        _ => None,
    });
    let _ = vp.ask(Command::ForgetDevice { device_id: device.into() }, short);
    if let Ok(Reply::Devices(list)) = hp.ask(Command::ListDevices, short) {
        for d in list.into_iter().filter(|d| d.paired) {
            let _ = hp.ask(Command::ForgetDevice { device_id: d.id }, short);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------------------------------

/// CPU seconds of each process group at one instant.
#[derive(Clone, Copy, Default)]
struct Cpu {
    host_service: f64,
    host_helpers: f64,
    viewer_service: f64,
    viewer_helpers: f64,
    viewer: f64,
}

fn cpu_now(host: &Service, viewer: &Service) -> Cpu {
    let sum = |pids: &[u32]| pids.iter().filter_map(|p| proc::cpu_seconds(*p)).sum::<f64>();
    Cpu {
        host_service: proc::cpu_seconds(host.pid()).unwrap_or(0.0),
        host_helpers: sum(&host.helpers),
        viewer_service: proc::cpu_seconds(viewer.pid()).unwrap_or(0.0),
        viewer_helpers: sum(&viewer.helpers),
        viewer: proc::cpu_seconds(std::process::id()).unwrap_or(0.0),
    }
}

/// The measured window on the machine's clock, with the CPU readings at its two ends.
struct Window {
    start_us: u64,
    end_us: u64,
    cpu: (Cpu, Cpu),
    audio_underruns: u32,
    /// The pipeline's counters at the two ends.
    pipeline: (PipelineStats, PipelineStats),
}

/// The client area of the viewer's window for a picture of `frame`: the picture's own size when the primary monitor
/// has room for it, else the largest size of the same shape that fits (the viewer then scales, as the application
/// does in a window smaller than the host's screen).
fn client_for(frame: (u32, u32), display: Option<u32>) -> (u32, u32) {
    let wanted = display.map(|n| format!(r"\\.\DISPLAY{n}"));
    let monitor = monitors()
        .into_iter()
        .find(|(name, r)| wanted.as_ref().map_or(r.left == 0 && r.top == 0, |w| name == w))
        .map(|(_, r)| r);
    let Some(r) = monitor else { return frame };
    let room = (((r.right - r.left) - 160).max(320) as u32, ((r.bottom - r.top) - 200).max(200) as u32);
    if frame.0 <= room.0 && frame.1 <= room.1 {
        return frame;
    }
    let by_width = u64::from(room.0) * u64::from(frame.1) <= u64::from(room.1) * u64::from(frame.0);
    let width = if by_width { room.0 } else { (u64::from(room.1) * u64::from(frame.0) / u64::from(frame.1)) as u32 };
    let width = width & !1;
    (width, (u64::from(width) * u64::from(frame.1) / u64::from(frame.0)) as u32)
}

/// Makes `hwnd` the foreground window and returns the window that was. A process in the background may take the
/// foreground only while its thread is attached to the input of the thread that has it; no input is injected.
fn take_foreground(hwnd: windows::Win32::Foundation::HWND) -> windows::Win32::Foundation::HWND {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow};
    // SAFETY: plain window calls on our own window and on the foreground window's thread id.
    unsafe {
        let before = GetForegroundWindow();
        let (mine, theirs) = (GetCurrentThreadId(), GetWindowThreadProcessId(before, None));
        let attached = theirs != 0 && theirs != mine && AttachThreadInput(mine, theirs, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(mine, theirs, false);
        }
        before
    }
}

fn has_foreground(hwnd: windows::Win32::Foundation::HWND) -> bool {
    // SAFETY: a getter.
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() == hwnd }
}

/// Gives the foreground back to the window that had it before the run.
fn give_foreground_back(before: windows::Win32::Foundation::HWND) {
    if !before.is_invalid() {
        // SAFETY: a window call; a window that is gone makes it fail harmlessly.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(before);
        }
    }
}

/// The part of the helper's processor cycles the synthetic source used between two instants, from the lines
/// `time own_cycles process_cycles` the source appends to `path`; 0 when the file does not cover the window.
fn source_share(path: &Path, start_us: u64, end_us: u64) -> f64 {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let rows: Vec<(u64, u64, u64)> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace().map(|v| v.parse::<u64>().ok());
            Some((it.next()??, it.next()??, it.next()??))
        })
        .collect();
    let at = |t: u64| rows.iter().rev().find(|r| r.0 <= t).copied();
    match (at(start_us), at(end_us)) {
        (Some(a), Some(b)) if b.2 > a.2 => ((b.1 - a.1) as f64 / (b.2 - a.2) as f64).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

fn markers(path: &Path) -> Vec<u64> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out: Vec<u64> =
        text.lines().filter_map(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok())).collect();
    out.sort_unstable();
    out
}

pub fn run(cfg: &RunConfig) -> Result<Fields, String> {
    std::fs::create_dir_all(&cfg.logs).map_err(|e| format!("{}: {e}", cfg.logs.display()))?;
    let service_exe = stage(cfg.service.as_deref())?;
    let marker_file = cfg.logs.join("markers.txt");
    let _ = std::fs::remove_file(&marker_file);
    let _ = std::fs::remove_file(cost_file(&marker_file));
    let spec = SourceSpec {
        width: cfg.width,
        height: cfg.height,
        fps: cfg.fps,
        family: cfg.family,
        markers: marker_file.clone(),
    };
    let (_, encoder_adapter) = adapter_of(cfg.family).map_err(|e| e.to_string())?;
    let idle_spec = SourceSpec { markers: cfg.logs.join("markers-viewer.txt"), ..spec.clone() };
    let mut host = Service::start(&service_exe, cfg.instances.1, &cfg.logs, &spec)?;
    let mut viewer = Service::start(&service_exe, cfg.instances.0, &cfg.logs, &idle_spec)?;

    // The viewer's window and pipeline, on this thread (the UI thread of the stand-in shell).
    let client = client_for((cfg.width, cfg.height), cfg.display);
    let pref = if cfg.viewer_high { Pref::High } else { Pref::Min };
    let shell = ShellStub::new(pref, client, cfg.display).map_err(|e| format!("the viewer window: {e}"))?;
    // The viewer's window takes the foreground, as a viewer in use has it: a desktop may hand a background window's
    // pictures to the screen at a lower rate (this PC: 30 a second), and the run would then measure that limit.
    let had_foreground = (!cfg.background).then(|| take_foreground(shell.hwnd));
    let client = shell.client_size();
    let layout = Layout::compute((cfg.width, cfg.height), client, ScaleMode::Fit).ok_or("an empty window")?;
    let rect = (
        layout.dest.x,
        layout.dest.y,
        (u64::from(BARCODE.0) * u64::from(layout.dest.w)).div_ceil(u64::from(cfg.width)) as u32,
        (u64::from(BARCODE.1) * u64::from(layout.dest.h)).div_ceil(u64::from(cfg.height)) as u32,
    );
    let record = Arc::new(Record::default());
    let probe_record = Arc::clone(&record);
    let pipeline = ViewerPipeline::new(ViewerOptions {
        on_present: None,
        audio: false,
        probe: Some(ProbeTap { rect, on_probe: Arc::new(move |event, probe| probe_record.on_probe(event, probe)) }),
    })
    .map_err(|e| format!("the viewer pipeline: {e}"))?;
    let underruns = Arc::new(AtomicU32::new(0));
    let sink_record = Arc::clone(&record);
    let tap = Arc::new(Tap {
        record: Arc::clone(&record),
        pipeline: Arc::clone(&pipeline),
        audio: AudioPlayer::start_with(Arc::clone(&underruns), move || MarkerSink::new(sink_record)),
    });
    let viewer_ui = ui(&viewer, Some(tap.clone() as Arc<dyn ViewerSink>))?;
    let host_ui = ui(&host, None)?;
    pipeline.set_out(viewer_ui.clone());
    let surface = shell.surface();
    let area = Rect { x: 0, y: 0, width: client.0, height: client.1 };

    // The two users and the clock of the run, on a thread of their own: this thread keeps the window's messages
    // moving and samples the memory.
    let measuring = AtomicBool::new(false);
    let mut memory: Vec<(f64, f64)> = Vec::new();
    // How long the viewer's window was not the foreground window inside the measured window.
    let mut background = Duration::ZERO;
    let me = std::process::id();
    let script = |host: &mut Service, viewer: &mut Service| -> Result<Window, String> {
        let end = Instant::now() + WAIT;
        while host.refresh_helpers().len() < 2 || viewer.refresh_helpers().len() < 2 {
            if Instant::now() >= end {
                return Err("the services did not start their helpers".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let host_addr = format!("127.0.0.1:{}", SESSION_PORT + 10 * host.instance);
        let device = pair(&host_ui, &viewer_ui, &host_addr)?;
        pipeline.opened(surface, area, 1.0);
        let end = Instant::now() + WAIT;
        while !pipeline.stats().attached {
            if Instant::now() >= end {
                return Err(format!("the renderer did not attach: {:?}", pipeline.last_error()));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let session_id = open_session(&host_ui, &viewer_ui, &device, true)?;
        let body = || -> Result<Window, String> {
            let end = Instant::now() + WAIT;
            while lock(&record.samples).is_empty() {
                if Instant::now() >= end {
                    return Err(format!(
                        "no picture with a readable barcode: {} frames arrived, {} unreadable, pipeline {:?}, last error {:?}",
                        lock(&record.arrivals).len(),
                        record.unreadable.load(Ordering::Relaxed),
                        pipeline.stats(),
                        pipeline.last_error()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_secs(cfg.warmup));
            let underruns_before = underruns.load(Ordering::Relaxed);
            let (start_us, cpu0, stats0) = (QpcClock.now().0, cpu_now(host, viewer), pipeline.stats());
            measuring.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(cfg.seconds));
            measuring.store(false, Ordering::SeqCst);
            let (end_us, cpu1, stats1) = (QpcClock.now().0, cpu_now(host, viewer), pipeline.stats());
            Ok(Window {
                start_us,
                end_us,
                cpu: (cpu0, cpu1),
                audio_underruns: underruns.load(Ordering::Relaxed).wrapping_sub(underruns_before),
                pipeline: (stats0, stats1),
            })
        };
        let window = body();
        close_session(&host_ui, &viewer_ui, session_id, &device);
        window
    };
    let window = std::thread::scope(|scope| {
        let task = scope.spawn(|| script(&mut host, &mut viewer));
        let t0 = Instant::now();
        let mut next = Duration::ZERO;
        let mut last = Instant::now();
        while !task.is_finished() {
            shell.pump();
            let now = Instant::now();
            if measuring.load(Ordering::SeqCst) {
                if !has_foreground(shell.hwnd) {
                    background += now.duration_since(last);
                }
                if t0.elapsed() >= next {
                    if let Some(mb) = proc::private_working_set_mb(me) {
                        memory.push((t0.elapsed().as_secs_f64(), mb));
                    }
                    next = t0.elapsed() + Duration::from_secs(2);
                }
            }
            last = now;
            std::thread::sleep(Duration::from_millis(2));
        }
        task.join().map_err(|_| "the script thread panicked".to_string())
    });
    let monitor = window_monitor(shell.hwnd);
    if let Some(before) = had_foreground {
        give_foreground_back(before);
    }
    let last_error = pipeline.last_error();
    pipeline.closed();
    pipeline.shutdown();
    drop((viewer_ui, host_ui));
    let encoder = host.encoder_line();
    host.stop();
    viewer.stop();
    host.keep_trace(&cfg.logs);
    viewer.keep_trace(&cfg.logs);
    let window = window??;

    // ----- the numbers -----
    let in_window = |t: u64| t >= window.start_us && t < window.end_us;
    let samples: Vec<Sample> = lock(&record.samples).iter().copied().filter(|s| in_window(s.presented_us)).collect();
    let arrivals: Vec<Arrival> = lock(&record.arrivals).iter().copied().filter(|a| in_window(a.arrived_us)).collect();
    let seconds = (window.end_us - window.start_us) as f64 / 1e6;
    let mut f = Fields::new();
    f.text("config", format!("{}x{}@{} {}", cfg.width, cfg.height, cfg.fps, cfg.family.name()));
    f.text("encoder", encoder.unwrap_or_else(|| "unknown".into()));
    f.text("encoder_adapter", encoder_adapter);
    f.text("viewer_adapter", shell.adapter_name.clone());
    f.text("window", format!("{}x{}", client.0, client.1));
    f.text("monitor", format!("{} at {} Hz", monitor.0, monitor.1));
    f.text("viewer_window", if cfg.background { "background" } else { "foreground" });
    f.num("seconds", seconds);
    f.num("warmup_s", cfg.warmup as f64);

    // B1: the barcode's time to the clock right after Present.
    let latency = Series::of(samples.iter().map(|s| s.presented_us.saturating_sub(s.captured_us) as f64 / 1000.0));
    f.series("b1_ms", &latency);

    // B2: presented pictures per second and frames of the source that never reached the screen.
    let mut per_second = vec![0u32; seconds as usize];
    for s in &samples {
        let i = ((s.presented_us - window.start_us) / 1_000_000) as usize;
        if let Some(n) = per_second.get_mut(i) {
            *n += 1;
        }
    }
    let fps = Series::of(per_second.iter().map(|n| f64::from(*n)));
    f.num("b2_fps_median", fps.median());
    f.num("b2_fps_min", fps.min());
    f.num("b2_windows", per_second.len() as f64);
    f.num("b2_windows_under_59", per_second.iter().filter(|n| **n < 59).count() as f64);
    let mut counters: Vec<u32> = samples.iter().map(|s| s.counter).collect();
    counters.sort_unstable();
    counters.dedup();
    let span = counters.last().zip(counters.first()).map_or(0, |(l, f)| l - f + 1);
    let lost = span.saturating_sub(counters.len() as u32);
    f.num("frames_presented", samples.len() as f64);
    f.num("frames_source", f64::from(span));
    f.num("frames_lost", f64::from(lost));
    f.num("b2_lost_percent", if span == 0 { 100.0 } else { f64::from(lost) / f64::from(span) * 100.0 });
    f.num("barcodes_unreadable", f64::from(record.unreadable.load(Ordering::Relaxed)));
    // What the viewer's pipeline counted inside the window, and before it (the start of the session).
    let (p0, p1) = window.pipeline;
    f.num("pipeline_lost", (p1.lost - p0.lost) as f64);
    f.num("decode_errors", (p1.decode_errors - p0.decode_errors) as f64);
    f.num("keyframe_requests", (p1.keyframe_requests - p0.keyframe_requests) as f64);
    f.num("startup_pipeline_lost", p0.lost as f64);
    f.num("startup_keyframe_requests", p0.keyframe_requests as f64);
    f.text("pipeline_last_error", last_error.unwrap_or_else(|| "none".into()));

    // The stages of a presented frame.
    let with: Vec<(&Sample, Arrival)> = samples.iter().filter_map(|s| s.arrival.map(|a| (s, a))).collect();
    let ms = |v: u64| v as f64 / 1000.0;
    f.series("stage_encode_ms", &Series::of(with.iter().map(|(_, a)| ms(u64::from(a.encode_us)))));
    f.series(
        "stage_transport_ms",
        &Series::of(with.iter().map(|(_, a)| ms(a.arrived_us.saturating_sub(a.capture_us + u64::from(a.encode_us))))),
    );
    f.series(
        "stage_queue_ms",
        &Series::of(
            with.iter().map(|(s, _)| {
                ms(u64::from(s.lag_us.saturating_sub(s.decode_us + s.wait_us + s.blt_us + s.present_us)))
            }),
        ),
    );
    f.series("stage_decode_ms", &Series::of(with.iter().map(|(s, _)| ms(u64::from(s.decode_us)))));
    f.series("stage_wait_ms", &Series::of(with.iter().map(|(s, _)| ms(u64::from(s.wait_us)))));
    f.series("stage_blit_ms", &Series::of(with.iter().map(|(s, _)| ms(u64::from(s.blt_us)))));
    f.series("stage_present_ms", &Series::of(with.iter().map(|(s, _)| ms(u64::from(s.present_us)))));
    f.num("stamp_mismatch", with.iter().filter(|(s, a)| s.captured_us != a.capture_us).count() as f64);

    // The stream.
    let bytes: u64 = arrivals.iter().map(|a| u64::from(a.bytes)).sum();
    f.num("stream_mbps", bytes as f64 * 8.0 / seconds / 1e6);
    f.num("stream_frames", arrivals.len() as f64);
    f.num("stream_keyframes", arrivals.iter().filter(|a| a.keyframe).count() as f64);
    f.num("stream_frame_kb_median", Series::of(arrivals.iter().map(|a| f64::from(a.bytes) / 1024.0)).median());

    // B3: each onset against the newest marker played before it.
    let marks = markers(&marker_file);
    let onsets: Vec<u64> = lock(&record.onsets).iter().copied().filter(|t| in_window(*t)).collect();
    let delays = Series::of(onsets.iter().filter_map(|&played| {
        let i = marks.partition_point(|&m| m <= played);
        (i > 0).then(|| (played - marks[i - 1]) as f64 / 1000.0)
    }));
    f.series("b3_ms", &delays);
    f.num("b3_markers", marks.iter().filter(|m| in_window(**m)).count() as f64);
    f.num("b3_onsets", onsets.len() as f64);
    f.num("b3_underruns", f64::from(window.audio_underruns));
    f.num("b3_sink_gaps", lock(&record.audio_gaps).iter().filter(|t| in_window(**t)).count() as f64);
    f.num("audio_datagrams", record.audio_datagrams.load(Ordering::Relaxed) as f64);
    // Positive: the sound of an instant is heard after its picture is seen.
    f.num("av_offset_ms", delays.median() - latency.median());

    // B10 and the rest of the CPU: percent of one core over the window.
    let (a, b) = window.cpu;
    let core = |before: f64, after: f64| (after - before) / seconds * 100.0;
    let cores = f64::from(proc::logical_processors());
    // The synthetic source runs inside the host's helper (it draws, uploads and waits for each slot): its share of
    // the helper's processor cycles is taken out, so what stays is what a helper spends on a session.
    let source_share = source_share(&cost_file(&marker_file), window.start_us, window.end_us);
    let helpers_all = core(a.host_helpers, b.host_helpers);
    let helpers = helpers_all * (1.0 - source_share);
    let host_cpu = core(a.host_service, b.host_service) + helpers;
    f.num("cpu_host_service_core_pct", core(a.host_service, b.host_service));
    f.num("cpu_host_helpers_core_pct", helpers);
    f.num("cpu_host_source_core_pct", helpers_all - helpers);
    f.num("cpu_viewer_service_core_pct", core(a.viewer_service, b.viewer_service));
    f.num("cpu_viewer_helpers_core_pct", core(a.viewer_helpers, b.viewer_helpers));
    f.num("cpu_viewer_core_pct", core(a.viewer, b.viewer));
    f.num("b10_host_core_pct", host_cpu);
    f.num("b10_host_total_pct", host_cpu / cores);
    f.num("logical_processors", cores);

    // B9: this process's private working set over the window.
    let mem = Series::of(memory.iter().map(|m| m.1));
    f.num("b9_first_mb", memory.first().map_or(f64::NAN, |m| m.1));
    f.num("b9_last_mb", memory.last().map_or(f64::NAN, |m| m.1));
    f.num("b9_median_mb", mem.median());
    f.num("b9_max_mb", mem.max());
    f.num("b9_slope_mb_per_hour", crate::stats::slope(&memory) * 3600.0);
    f.num("b9_samples", memory.len() as f64);

    f.num("window_background_ms", background.as_secs_f64() * 1000.0);

    // The run measured something only when the session really carried pictures and sound, in a window the desktop
    // showed at its own rate.
    let expected = (cfg.seconds * u64::from(cfg.fps)) as usize;
    let present_median = Series::of(with.iter().map(|(s, _)| ms(u64::from(s.present_us)))).median();
    if cfg.background {
        // The window was left in the background on purpose: the run shows what the desktop does to it.
        f.text("valid", "yes");
    } else if background > Duration::from_millis(200) {
        f.text(
            "valid",
            format!(
                "no: the viewer's window was in the background for {} ms of the run (measure on an idle PC)",
                background.as_millis()
            ),
        );
    } else if present_median > 500.0 / f64::from(cfg.fps) {
        f.text(
            "valid",
            format!(
                "no: Present blocks {present_median:.1} ms: the desktop is not taking the pictures at the stream's rate (a frame limiter or a power mode)"
            ),
        );
    } else if samples.len() * 2 < expected {
        f.text("valid", format!("no: {} pictures read in a window of {expected} frames", samples.len()));
    } else if onsets.len() as u64 + 3 < cfg.seconds {
        f.text("valid", format!("no: {} marker onsets in {} s of audio", onsets.len(), cfg.seconds));
    } else {
        f.text("valid", "yes");
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_fakes::draw_barcode;
    use swapdesk_platform::MonoMicros;

    /// The barcode as the renderer leaves it in a window `scale` times the picture's size (nearest pixel).
    fn scaled_barcode(counter: u32, captured_at: u64, scale: f64) -> Probe {
        let (bw, bh) = BARCODE;
        let mut full = vec![0u8; (bw * bh * 4) as usize];
        assert!(draw_barcode(&mut full, bw * 4, bw, bh, counter, MonoMicros(captured_at)));
        let (w, h) = ((f64::from(bw) * scale).ceil() as u32, (f64::from(bh) * scale).ceil() as u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let (sx, sy) = ((f64::from(x) / scale) as u32, (f64::from(y) / scale) as u32);
                let from = ((sy.min(bh - 1) * bw + sx.min(bw - 1)) * 4) as usize;
                let to = ((y * w + x) * 4) as usize;
                data[to..to + 4].copy_from_slice(&full[from..from + 4]);
            }
        }
        Probe { width: w, height: h, stride: w * 4, data }
    }

    fn event(frame_id: u32) -> PresentEvent {
        PresentEvent {
            frame_id,
            display: 0,
            width: 2560,
            height: 1600,
            decode_us: 700,
            lag_us: 2_000,
            presented_at_us: 9_000_000,
            wait_us: 1,
            blt_us: 500,
            present_us: 300,
        }
    }

    #[test]
    fn a_barcode_is_read_from_a_window_that_shows_the_picture_at_its_size_or_scaled_down() {
        for (n, scale) in [1.0, 0.8, 0.625, 0.5].into_iter().enumerate() {
            let record = Record::default();
            let arrival = Arrival {
                capture_us: 8_990_000,
                encode_us: 3_000,
                arrived_us: 8_994_000,
                bytes: 40_000,
                keyframe: false,
            };
            lock(&record.pending).insert(7, arrival);
            record.on_probe(&event(7), &scaled_barcode(1234 + n as u32, 8_990_000, scale));
            let samples = lock(&record.samples);
            assert_eq!(samples.len(), 1, "scale {scale}");
            assert_eq!((samples[0].counter, samples[0].captured_us), (1234 + n as u32, 8_990_000), "scale {scale}");
            assert!(samples[0].arrival.is_some(), "the frame's arrival is joined to its picture");
            assert_eq!(record.unreadable.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn a_picture_without_a_barcode_is_counted_and_not_sampled() {
        let record = Record::default();
        let gray = Probe { width: 512, height: 64, stride: 512 * 4, data: vec![128; 512 * 64 * 4] };
        record.on_probe(&event(1), &gray);
        assert!(lock(&record.samples).is_empty());
        assert_eq!(record.unreadable.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_sources_share_is_its_cycles_over_the_helpers_between_the_two_ends_of_the_window() {
        let dir = std::env::temp_dir().join(format!("swapdesk-measure-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch folder");
        let path = dir.join("source-cost.txt");
        std::fs::write(&path, "100 0 0\n200 10 100\n300 40 200\n400 70 300\nbroken line\n500 100 400\n")
            .expect("write");
        // From the reading at 200 to the one at 400: 60 of the helper's 200 cycles were the source's.
        assert!((source_share(&path, 250, 450) - 0.3).abs() < 1e-9);
        // A window the file does not cover takes nothing out.
        assert_eq!(source_share(&path, 10, 50), 0.0);
        assert_eq!(source_share(&dir.join("missing.txt"), 250, 450), 0.0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_marker_sink_finds_one_onset_per_burst_and_a_gap_when_nothing_is_queued() {
        let record = Arc::new(Record::default());
        let mut sink = MarkerSink::new(Arc::clone(&record));
        sink.start().expect("start");
        let quiet = AudioBlock { captured_at: MonoMicros(0), samples: vec![0.1; 960] };
        let mut burst = quiet.clone();
        for s in burst.samples.iter_mut().take(480) {
            *s = 0.7;
        }
        sink.write(&quiet).expect("write");
        sink.write(&burst).expect("write");
        sink.write(&quiet).expect("write");
        assert_eq!(lock(&record.onsets).len(), 1, "240 loud frames in a row are one onset");
        assert!(lock(&record.audio_gaps).is_empty(), "blocks written back to back queue behind each other");
        // 30 ms of audio were queued: after 80 ms the queue ran dry, and the next block finds the gap.
        std::thread::sleep(Duration::from_millis(80));
        sink.write(&burst).expect("write");
        assert_eq!(lock(&record.audio_gaps).len(), 1);
        assert_eq!(lock(&record.onsets).len(), 2);
    }
}
