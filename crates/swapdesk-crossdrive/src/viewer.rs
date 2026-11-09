//! The scripted user of the controlling machine, and the media it receives.
//!
//! Markers printed through `log`, read by the orchestrator: `VIEWER_PAIR_OPEN`, `VIEWER_CODE <code>`, `VIEWER_PAIRED`,
//! `VIEWER_ACTIVE`, `POINTER ...`, `VIEWER_FORGOTTEN`, and the counts line `VIEWER_COUNTS ...`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_platform::{Pixels, VideoDecoder};
use swapdesk_proto::{CursorPosition, CursorShape, InputEvent, KeyframeRequest, VideoHeader};
use swapdesk_ui::service_backend::{ServiceBackend, ViewerSink};
use swapdesk_ui::{Command, Intent, PairingState, Reply, Role, SessionState, UiEvent};

use crate::pump::{Pump, wait_file};

const REPLY: Duration = Duration::from_secs(10);
const NO_TIME: u64 = u64::MAX;
/// Offsets (px) of the pointer moves from the anchor: a small square walked and closed, 20 px at most.
const WALK: [(i32, i32); 8] = [(10, 0), (20, 0), (20, 10), (20, 20), (10, 20), (0, 20), (0, 10), (0, 0)];
const MOVE_GAP: Duration = Duration::from_millis(100);
/// Samples of the host cursor kept.
const MAX_CURSOR_SAMPLES: usize = 8192;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Builds the decoder on the decode thread (a platform decoder is not `Send`).
pub type DecoderFactory = Box<dyn FnOnce() -> Box<dyn VideoDecoder> + Send>;

#[derive(Clone, Copy)]
struct CursorSample {
    at: Instant,
    x: u16,
    y: u16,
}

#[derive(Default)]
struct Shared {
    /// When `Connect` was sent: the zero of the first-frame latency.
    started: Mutex<Option<Instant>>,
    video_received: AtomicU64,
    video_bytes: AtomicU64,
    keyframes: AtomicU64,
    first_video_ms: AtomicU64,
    audio: AtomicU64,
    audio_bytes: AtomicU64,
    cursor_updates: AtomicU64,
    frames_decoded: AtomicU64,
    decode_errors: AtomicU64,
    last_error: Mutex<String>,
    need_keyframe: AtomicBool,
    first_frame_ms: AtomicU64,
    /// `width << 16 | height` of the last video header.
    dims: AtomicU32,
    display: AtomicU32,
    last_good: AtomicU32,
    sizes: Mutex<BTreeSet<(u32, u32)>>,
    /// Percent of sampled pixels that are not black in the last sampled picture; 101 = none yet.
    nonblack: AtomicU32,
    cursor: Mutex<Vec<CursorSample>>,
}

struct Sink {
    shared: Arc<Shared>,
    jobs: Mutex<Sender<(VideoHeader, Vec<u8>)>>,
}

impl ViewerSink for Sink {
    fn video(&self, _session_id: u64, header: VideoHeader, payload: Vec<u8>) {
        let s = &self.shared;
        s.video_received.fetch_add(1, Ordering::Relaxed);
        s.video_bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
        if header.keyframe {
            s.keyframes.fetch_add(1, Ordering::Relaxed);
        }
        s.dims.store(u32::from(header.width) << 16 | u32::from(header.height), Ordering::Relaxed);
        s.display.store(u32::from(header.display_id), Ordering::Relaxed);
        if let Some(t0) = *lock(&s.started) {
            let ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(NO_TIME);
            let _ = s.first_video_ms.compare_exchange(NO_TIME, ms, Ordering::Relaxed, Ordering::Relaxed);
        }
        let _ = lock(&self.jobs).send((header, payload));
    }

    fn audio(&self, _session_id: u64, datagram: Vec<u8>) {
        self.shared.audio.fetch_add(1, Ordering::Relaxed);
        self.shared.audio_bytes.fetch_add(datagram.len() as u64, Ordering::Relaxed);
    }

    fn cursor_pos(&self, _session_id: u64, pos: CursorPosition) {
        self.shared.cursor_updates.fetch_add(1, Ordering::Relaxed);
        let mut samples = lock(&self.shared.cursor);
        if samples.len() < MAX_CURSOR_SAMPLES {
            samples.push(CursorSample { at: Instant::now(), x: pos.x, y: pos.y });
        }
    }

    fn cursor_shape(&self, _session_id: u64, _shape: CursorShape) {}
}

/// The viewer's media side: a sink for `ServiceBackend::start_with_sink` and the decode thread behind it.
pub struct Media {
    shared: Arc<Shared>,
    sink: Arc<Sink>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Media {
    pub fn new(factory: DecoderFactory) -> Self {
        let shared = Arc::new(Shared {
            first_video_ms: AtomicU64::new(NO_TIME),
            first_frame_ms: AtomicU64::new(NO_TIME),
            nonblack: AtomicU32::new(101),
            ..Shared::default()
        });
        let (tx, rx) = channel::<(VideoHeader, Vec<u8>)>();
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("crossdrive-decode".into())
            .spawn(move || {
                let mut decoder = factory();
                for (header, payload) in rx {
                    decode_one(&worker, decoder.as_mut(), &header, &payload);
                }
            })
            .expect("the decode thread starts");
        let sink = Arc::new(Sink { shared: Arc::clone(&shared), jobs: Mutex::new(tx) });
        Self { shared, sink, thread: Mutex::new(Some(thread)) }
    }

    /// The sink to give `ServiceBackend::start_with_sink`.
    pub fn sink(&self) -> Arc<dyn ViewerSink> {
        self.sink.clone()
    }

    /// Stops the decode thread once the queue is drained. The backend must not deliver video after this.
    pub fn finish(&self) {
        // Dropping the only sender ends the thread's loop; replace it with a dead one.
        let (dead, _) = channel();
        *lock(&self.sink.jobs) = dead;
        if let Some(t) = lock(&self.thread).take() {
            let _ = t.join();
        }
    }
}

fn decode_one(s: &Shared, decoder: &mut dyn VideoDecoder, header: &VideoHeader, payload: &[u8]) {
    match decoder.decode(header, payload) {
        Ok(Some(frame)) => {
            let n = s.frames_decoded.fetch_add(1, Ordering::Relaxed) + 1;
            if header.keyframe {
                s.need_keyframe.store(false, Ordering::Relaxed);
            }
            s.last_good.store(header.frame_id, Ordering::Relaxed);
            lock(&s.sizes).insert((frame.width, frame.height));
            if let Some(t0) = *lock(&s.started) {
                let ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(NO_TIME);
                let _ = s.first_frame_ms.compare_exchange(NO_TIME, ms, Ordering::Relaxed, Ordering::Relaxed);
            }
            if n % 8 == 1
                && let Pixels::Cpu { stride, data } = &frame.pixels
            {
                s.nonblack.store(
                    nonblack_percent(data, *stride as usize, frame.width as usize, frame.height as usize),
                    Ordering::Relaxed,
                );
            }
        }
        Ok(None) => {}
        Err(e) => {
            s.decode_errors.fetch_add(1, Ordering::Relaxed);
            s.need_keyframe.store(true, Ordering::Relaxed);
            *lock(&s.last_error) = e.to_string();
        }
    }
}

/// Percent of a sparse grid of BGRA pixels with any colour channel above 16.
fn nonblack_percent(data: &[u8], stride: usize, width: usize, height: usize) -> u32 {
    let (mut total, mut lit) = (0u32, 0u32);
    for y in (0..height).step_by(8) {
        for x in (0..width).step_by(8) {
            let o = y * stride + x * 4;
            if let Some(px) = data.get(o..o + 3) {
                total += 1;
                if px.iter().any(|&c| c > 16) {
                    lit += 1;
                }
            }
        }
    }
    (lit * 100).checked_div(total).unwrap_or(0)
}

pub struct ViewerConfig {
    /// The host's IPv4 address, as typed in "add by address".
    pub host_addr: String,
    /// Written by the orchestrator: the host's code.
    pub peer_code: PathBuf,
    /// Written by the orchestrator when the host has granted and may be called.
    pub go: PathBuf,
    pub pair_timeout: Duration,
    pub connect_timeout: Duration,
    /// How long to wait for the first decoded picture after the session is active.
    pub media_timeout: Duration,
    /// How long to wait for the host's cursor position, which the pointer moves are anchored on.
    pub anchor_timeout: Duration,
    /// The pointer stays within this many pixels of where it was.
    pub pointer_box_px: u32,
    /// Written by the orchestrator after `VIEWER_NEED_ANCHOR`: where the host's pointer is now, "x,y" in pointer units
    /// (0 to 65535 across the captured display), or `none` when it is not on that display.
    pub anchor_file: PathBuf,
    /// An already paired device to drive: its id. Pairing, the `go` file, the anchor file and the final forget are all
    /// skipped (the pairing outlives the run) and the session connects straight to that device, whose address the
    /// service already knows. The pointer is anchored on the host's last reported cursor position (the middle of the
    /// display when it reported none within a second). `None` is the full script that pairs and forgets.
    pub paired: Option<String>,
    /// Whether the pointer moves are sent at all.
    pub send_input: bool,
    /// The session stays open at least this long after it went active (media keeps being counted).
    pub hold: Duration,
    /// Keyboard proof: after the pointer moves, wait for this file (the orchestrator writes it once a text editor has
    /// the focus on the host), then type [`TYPED_WORD`] with physical keys and save with Cmd+S (Ctrl+S toward Windows
    /// is the host's own matter: this proof is toward the Mac). `None`: no keys.
    pub type_after: Option<PathBuf>,
    /// Pairing with a host started after the viewer: the file the orchestrator writes once that host is up (see
    /// `pair_with_fresh_host`). `None`: the host is found by its address.
    pub host_up: Option<PathBuf>,
}

/// What the keyboard proof types, as HID usages of the US layout's letter keys (page 7).
pub const TYPED_WORD: &str = "swapdesk";

impl ViewerConfig {
    pub fn new(host_addr: &str, peer_code: PathBuf, go: PathBuf, anchor_file: PathBuf) -> Self {
        Self {
            host_addr: host_addr.into(),
            peer_code,
            go,
            pair_timeout: Duration::from_secs(100),
            connect_timeout: Duration::from_secs(40),
            media_timeout: Duration::from_secs(12),
            anchor_timeout: Duration::from_secs(15),
            pointer_box_px: 50,
            anchor_file,
            paired: None,
            send_input: true,
            hold: Duration::ZERO,
            type_after: None,
            host_up: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ViewerReport {
    pub frames_decoded: u64,
    pub video_received: u64,
    pub video_bytes: u64,
    pub keyframes: u64,
    pub decode_errors: u64,
    pub last_decode_error: String,
    pub sizes: Vec<(u32, u32)>,
    pub first_video_ms: Option<u64>,
    pub first_frame_ms: Option<u64>,
    pub nonblack_pct: Option<u32>,
    pub audio_datagrams: u64,
    pub audio_bytes: u64,
    pub cursor_updates: u64,
    pub pointer_anchor_px: Option<(i32, i32)>,
    pub pointer_moves: u32,
    pub pointer_echo_samples: u32,
    pub pointer_max_deviation_px: i32,
    /// The session reached the active state as the viewer.
    pub session_active: bool,
    pub ended_clean: bool,
    pub forgotten: bool,
}

impl ViewerReport {
    /// The counts line the orchestrator prints.
    pub fn counts_line(&self) -> String {
        let sizes: Vec<String> = self.sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
        let ms = |v: Option<u64>| v.map_or("none".to_string(), |v| v.to_string());
        format!(
            "VIEWER_COUNTS frames_decoded={} video_received={} video_bytes={} keyframes={} decode_errors={} sizes={} first_video_ms={} first_frame_ms={} nonblack_pct={} audio_datagrams={} audio_bytes={} cursor_updates={} pointer_moves={} pointer_echo_samples={} pointer_max_deviation_px={}",
            self.frames_decoded,
            self.video_received,
            self.video_bytes,
            self.keyframes,
            self.decode_errors,
            if sizes.is_empty() { "none".to_string() } else { sizes.join("+") },
            ms(self.first_video_ms),
            ms(self.first_frame_ms),
            self.nonblack_pct.map_or("none".to_string(), |v| v.to_string()),
            self.audio_datagrams,
            self.audio_bytes,
            self.cursor_updates,
            self.pointer_moves,
            self.pointer_echo_samples,
            self.pointer_max_deviation_px,
        )
    }
}

#[derive(Default)]
struct Held {
    host: Option<String>,
    session: Option<u64>,
}

/// Runs the viewer side to the end. The session is ended and the device forgotten on every path that got that far.
pub fn viewer_drive(
    backend: &ServiceBackend,
    media: &Media,
    cfg: &ViewerConfig,
    log: &dyn Fn(&str),
) -> (ViewerReport, Result<(), String>) {
    let mut pump = Pump::new(backend);
    let mut held = Held::default();
    let mut report = ViewerReport::default();
    let mut result = script(&mut pump, backend, media, cfg, log, &mut held, &mut report);
    if let Some(session_id) = held.session {
        // Still connected: an error path. End it so the host's service lets go.
        let _ = pump.request(Command::Disconnect { session_id }, REPLY);
        let _ = pump.wait("the session to end", Duration::from_secs(5), |e| matches!(e, UiEvent::Session { session_id: s, state: SessionState::Ended { .. }, .. } if *s == session_id).then_some(()));
    }
    if let Some(host) = held.host.clone() {
        match forget(&mut pump, &host) {
            Ok(()) => {
                report.forgotten = true;
                log("VIEWER_FORGOTTEN");
            }
            Err(e) => {
                log(&format!("VIEWER_FORGET_FAILED {e}"));
                if result.is_ok() {
                    result = Err(format!("forget: {e}"));
                }
            }
        }
    }
    snapshot(&media.shared, &mut report);
    (report, result)
}

fn forget(pump: &mut Pump<'_>, device: &str) -> Result<(), String> {
    pump.ask(Command::ForgetDevice { device_id: device.to_string() }, REPLY)?;
    match pump.ask(Command::ListDevices, REPLY)? {
        Reply::Devices(list) if list.iter().any(|d| d.id == device && d.paired) => {
            Err("the device is still listed as paired".into())
        }
        Reply::Devices(_) => Ok(()),
        other => Err(format!("unexpected reply to ListDevices: {other:?}")),
    }
}

fn snapshot(s: &Shared, r: &mut ViewerReport) {
    let ms = |a: &AtomicU64| Some(a.load(Ordering::Relaxed)).filter(|v| *v != NO_TIME);
    r.frames_decoded = s.frames_decoded.load(Ordering::Relaxed);
    r.video_received = s.video_received.load(Ordering::Relaxed);
    r.video_bytes = s.video_bytes.load(Ordering::Relaxed);
    r.keyframes = s.keyframes.load(Ordering::Relaxed);
    r.decode_errors = s.decode_errors.load(Ordering::Relaxed);
    r.last_decode_error = lock(&s.last_error).clone();
    r.sizes = lock(&s.sizes).iter().copied().collect();
    r.first_video_ms = ms(&s.first_video_ms);
    r.first_frame_ms = ms(&s.first_frame_ms);
    r.nonblack_pct = Some(s.nonblack.load(Ordering::Relaxed)).filter(|v| *v <= 100);
    r.audio_datagrams = s.audio.load(Ordering::Relaxed);
    r.audio_bytes = s.audio_bytes.load(Ordering::Relaxed);
    r.cursor_updates = s.cursor_updates.load(Ordering::Relaxed);
}

fn script(
    pump: &mut Pump<'_>,
    backend: &ServiceBackend,
    media: &Media,
    cfg: &ViewerConfig,
    log: &dyn Fn(&str),
    held: &mut Held,
    report: &mut ViewerReport,
) -> Result<(), String> {
    // 1. Find the host by address and pair (D12: both sides open pairing; the host's script did). A device that is
    // already paired is connected to as it is.
    let host = if let Some(device) = &cfg.paired {
        log(&format!("VIEWER_USING_PAIRED {device}"));
        device.clone()
    } else {
        let host = pair_with_host(pump, cfg, log)?;
        held.host = Some(host.clone());
        log(&format!("VIEWER_PAIRED {host}"));
        // 2. Wait for the host to have granted.
        wait_file(&cfg.go, "the go from the orchestrator", cfg.pair_timeout)?;
        host
    };

    // 2. Open the session with control.
    let shared = &media.shared;
    *lock(&shared.started) = Some(Instant::now());
    let Reply::SessionId(session_id) = pump.ask(
        Command::Connect { device_id: host.clone(), intent: Intent::Control, unattended: false },
        cfg.connect_timeout,
    )?
    else {
        return Err("Connect did not reply a session id".into());
    };
    held.session = Some(session_id);
    let granted =
        pump.wait("the session to go active", cfg.connect_timeout, |e| match e {
            UiEvent::Session {
                session_id: s, state: SessionState::Active { role: Role::Viewer, granted, .. }, ..
            } if *s == session_id => Some(*granted),
            UiEvent::Session { session_id: s, state: SessionState::Ended { reason }, .. } if *s == session_id => {
                log(&format!("VIEWER_SESSION_ENDED_EARLY {reason:?}"));
                None
            }
            _ => None,
        })?;
    let active_at = Instant::now();
    report.session_active = true;
    log(&format!("VIEWER_ACTIVE session={session_id} granted={:#06x}", granted.0));
    pump.ask(Command::SetAudio { session_id, on: true }, REPLY)?;
    script_media(pump, backend, media, cfg, log, held, report, session_id, active_at)
}

/// Adds the host by address and runs the pairing ceremony; the device id it paired as.
fn pair_with_host(pump: &mut Pump<'_>, cfg: &ViewerConfig, log: &dyn Fn(&str)) -> Result<String, String> {
    if let Some(up) = &cfg.host_up {
        return pair_with_fresh_host(pump, cfg, log, up);
    }
    let Reply::Device(mut placeholder) = pump.ask(Command::AddByAddress { ipv4: cfg.host_addr.clone() }, REPLY)? else {
        return Err("AddByAddress did not reply a device".into());
    };
    // The address can still name the machine's own app, already paired here and announced before the scripted host
    // replaced it (its record outlives it). The scripted host has a fresh identity: it is the one device on the
    // network that is not paired and runs the same OS. Wait a moment for its announcement.
    if placeholder.paired {
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Reply::Devices(list)) = pump.ask(Command::ListDevices, REPLY) {
                let fresh: Vec<_> = list.iter().filter(|d| !d.paired && d.os == placeholder.os).collect();
                if let [one] = fresh.as_slice() {
                    log(&format!("VIEWER_ADDRESS_NAMED_A_PAIRED_DEVICE using the unpaired {}", one.id));
                    placeholder = (*one).clone();
                    break;
                }
            }
            if Instant::now() > end {
                return Err(
                    "the address names a paired device and no single unpaired one of its OS is on the network".into()
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    let Reply::PairingId(pairing_id) = pump.ask(Command::PairOpen { device_id: None }, REPLY)? else {
        return Err("PairOpen did not reply a pairing id".into());
    };
    log("VIEWER_PAIR_OPEN");
    let host = match pair(pump, cfg, log, pairing_id, &placeholder.id) {
        Ok(h) => h,
        Err(e) => {
            let _ = pump.request(Command::PairCancel { pairing_id }, REPLY);
            return Err(e);
        }
    };
    Ok(host)
}

/// The scripted host gets a new key on every run, and a record of an earlier run can still sit at its address on this
/// PC (a service before 1.1 keeps it). So: note the devices known now, let the orchestrator start the host (it writes
/// `up`), then pair with the one unpaired device that appears after that.
fn pair_with_fresh_host(
    pump: &mut Pump<'_>,
    cfg: &ViewerConfig,
    log: &dyn Fn(&str),
    up: &std::path::Path,
) -> Result<String, String> {
    let Reply::Devices(before) = pump.ask(Command::ListDevices, REPLY)? else {
        return Err("ListDevices did not reply the devices".into());
    };
    let known: BTreeSet<String> = before.into_iter().map(|d| d.id).collect();
    log(&format!("VIEWER_SNAPSHOT_TAKEN {} devices known", known.len()));
    wait_file(up, "the host started by the orchestrator", cfg.pair_timeout)?;
    let end = Instant::now() + Duration::from_secs(30);
    let host = loop {
        if let Ok(Reply::Devices(list)) = pump.ask(Command::ListDevices, REPLY) {
            let fresh: Vec<_> = list.into_iter().filter(|d| !d.paired && !known.contains(&d.id)).collect();
            if let [one] = fresh.as_slice() {
                break one.id.clone();
            }
        }
        if Instant::now() > end {
            return Err("no single new unpaired device appeared after the host started".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    log(&format!("VIEWER_FRESH_HOST {host}"));
    let Reply::PairingId(pairing_id) = pump.ask(Command::PairOpen { device_id: None }, REPLY)? else {
        return Err("PairOpen did not reply a pairing id".into());
    };
    log("VIEWER_PAIR_OPEN");
    pair(pump, cfg, log, pairing_id, &host).inspect_err(|_| {
        let _ = pump.request(Command::PairCancel { pairing_id }, REPLY);
    })
}

/// The session's media and pointer, then its end. Runs once the session is active.
#[allow(clippy::too_many_arguments)]
fn script_media(
    pump: &mut Pump<'_>,
    backend: &ServiceBackend,
    media: &Media,
    cfg: &ViewerConfig,
    log: &dyn Fn(&str),
    held: &mut Held,
    report: &mut ViewerReport,
    session_id: u64,
    active_at: Instant,
) -> Result<(), String> {
    let shared = &media.shared;
    let mut last_ask = Instant::now() - Duration::from_secs(1);
    // 3. Video: wait for the first decoded picture, asking for a keyframe when the decoder lost its place.
    let end = Instant::now() + cfg.media_timeout;
    while shared.frames_decoded.load(Ordering::Relaxed) == 0 && Instant::now() < end {
        request_keyframe_if_needed(backend, shared, session_id, &mut last_ask);
        std::thread::sleep(Duration::from_millis(30));
    }
    if shared.frames_decoded.load(Ordering::Relaxed) == 0 {
        return Err(format!(
            "no frame was decoded within {}s (received {}, decode errors {}, last: {})",
            cfg.media_timeout.as_secs(),
            shared.video_received.load(Ordering::Relaxed),
            shared.decode_errors.load(Ordering::Relaxed),
            lock(&shared.last_error)
        ));
    }
    log(&format!("VIEWER_FIRST_FRAME decoded={} ", shared.frames_decoded.load(Ordering::Relaxed)));

    // 4. Pointer: a few small moves around where the host's pointer is now, at most one second of them.
    let pointer = if cfg.send_input { pointer_moves(backend, shared, session_id, cfg, log, report) } else { Ok(()) };
    let pointer = pointer.and_then(|()| match &cfg.type_after {
        Some(ready) => type_word(backend, session_id, ready, cfg.anchor_timeout, log),
        None => Ok(()),
    });
    // 5. A moment more of media, and the audio; the hold keeps the session open as long as the caller asked.
    let hold_end = active_at + cfg.hold;
    let audio_end = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < hold_end || (shared.audio.load(Ordering::Relaxed) == 0 && Instant::now() < audio_end) {
        request_keyframe_if_needed(backend, shared, session_id, &mut last_ask);
        std::thread::sleep(Duration::from_millis(30));
    }

    // 6. The end.
    pump.ask(Command::Disconnect { session_id }, REPLY)?;
    let reason = pump.wait("the session to end", Duration::from_secs(10), |e| match e {
        UiEvent::Session { session_id: s, state: SessionState::Ended { reason }, .. } if *s == session_id => {
            Some(*reason)
        }
        _ => None,
    })?;
    held.session = None;
    report.ended_clean = reason.is_none();
    log(&format!("VIEWER_SESSION_ENDED reason={reason:?}"));
    pointer?;
    if !report.ended_clean {
        return Err(format!("the session ended with {reason:?}"));
    }
    Ok(())
}

fn request_keyframe_if_needed(backend: &ServiceBackend, s: &Shared, session_id: u64, last_ask: &mut Instant) {
    if s.need_keyframe.load(Ordering::Relaxed) && last_ask.elapsed() >= Duration::from_millis(500) {
        *last_ask = Instant::now();
        let display = u16::try_from(s.display.load(Ordering::Relaxed)).unwrap_or(0);
        backend.viewer_keyframe(
            session_id,
            KeyframeRequest { display, last_good_frame: s.last_good.load(Ordering::Relaxed) },
        );
    }
}

/// Moves the host's pointer a little and checks that its position came back changed. An `Err` is a failed proof, not a
/// failed session: the caller still ends the session first.
fn pointer_moves(
    backend: &ServiceBackend,
    s: &Shared,
    session_id: u64,
    cfg: &ViewerConfig,
    log: &dyn Fn(&str),
    report: &mut ViewerReport,
) -> Result<(), String> {
    // Where the host's pointer is comes from the orchestrator, which reads it from the OS on the host just now: the
    // cursor positions the host sends are only checked for movement (an idle pointer sends none, and their scale is
    // not trusted as an anchor). Without a reading on the display nothing is sent.
    let anchor = if cfg.paired.is_some() {
        // No orchestrator: the host's last cursor position (its scale is only trusted to this extent), else the middle.
        let end = Instant::now() + Duration::from_secs(1);
        while lock(&s.cursor).is_empty() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        lock(&s.cursor).last().map_or((32768, 32768), |c| (c.x, c.y))
    } else {
        log("VIEWER_NEED_ANCHOR");
        let text = wait_file(&cfg.anchor_file, "the pointer reading of the host", cfg.anchor_timeout)?;
        parse_units(&text).ok_or_else(|| {
            format!("the host's pointer is not on the captured display ({text}): none of the pointer moves were sent")
        })?
    };
    let dims = s.dims.load(Ordering::Relaxed);
    let (w, h) = (i64::from(dims >> 16), i64::from(dims & 0xFFFF));
    if w == 0 || h == 0 {
        return Err("the display size is unknown".into());
    }
    let display = u16::try_from(s.display.load(Ordering::Relaxed)).unwrap_or(0);
    let to_px = |units: u16, dim: i64| i32::try_from(i64::from(units) * dim / 65535).unwrap_or(0);
    let to_units = |px: i32, dim: i64| u16::try_from((i64::from(px) * 65535 + dim / 2) / dim).unwrap_or(u16::MAX);
    let (ax, ay) = (to_px(anchor.0, w), to_px(anchor.1, h));
    report.pointer_anchor_px = Some((ax, ay));
    let reach = i32::try_from(cfg.pointer_box_px / 2).unwrap_or(25);
    let clamp = |v: i32, dim: i64| v.clamp(0, i32::try_from(dim - 1).unwrap_or(0));
    let started = Instant::now();
    log(&format!(
        "POINTER sending {} small moves within a {} px box around ({ax},{ay}) px on a {w}x{h} display, over {} ms: input goes viewer to host only",
        WALK.len(),
        cfg.pointer_box_px,
        MOVE_GAP.as_millis() * (WALK.len() as u128)
    ));
    for (dx, dy) in WALK {
        debug_assert!(dx.abs() <= reach && dy.abs() <= reach);
        let (px, py) = (clamp(ax + dx.clamp(-reach, reach), w), clamp(ay + dy.clamp(-reach, reach), h));
        backend.viewer_input(session_id, vec![InputEvent::Pointer { display, x: to_units(px, w), y: to_units(py, h) }]);
        report.pointer_moves += 1;
        std::thread::sleep(MOVE_GAP);
    }
    // The host's pointer position comes back as cursor updates; some of them must show the moves.
    std::thread::sleep(Duration::from_millis(600));
    let samples = lock(&s.cursor).clone();
    // For the record: the host's own pointer positions around the moves (px), the first few distinct ones.
    let mut shown: Vec<String> = Vec::new();
    for c in &samples {
        let at = format!(
            "{}{}ms:({},{})",
            if c.at >= started { "+" } else { "-" },
            if c.at >= started { c.at - started } else { started - c.at }.as_millis(),
            to_px(c.x, w),
            to_px(c.y, h)
        );
        if shown.len() < 24
            && shown.last().is_none_or(|l| l.split_once(':').map(|x| x.1) != at.split_once(':').map(|x| x.1))
        {
            shown.push(at);
        }
    }
    log(&format!("POINTER host cursor samples ({} total): {}", samples.len(), shown.join(" ")));
    // Movement, not position: the distinct positions the host reported while the moves ran, and how far they spread.
    let mut seen: Vec<(i32, i32)> = Vec::new();
    for c in samples.iter().filter(|c| c.at >= started) {
        let p = (to_px(c.x, w), to_px(c.y, h));
        if !seen.contains(&p) {
            seen.push(p);
        }
    }
    let spread =
        |f: fn(&(i32, i32)) -> i32| seen.iter().map(f).max().unwrap_or(0) - seen.iter().map(f).min().unwrap_or(0);
    report.pointer_echo_samples = u32::try_from(seen.len()).unwrap_or(u32::MAX);
    report.pointer_max_deviation_px = spread(|p| p.0).max(spread(|p| p.1));
    log(&format!(
        "POINTER echo: the host reported {} distinct pointer positions during the moves, spread {} px",
        report.pointer_echo_samples, report.pointer_max_deviation_px
    ));
    // The walk spans 20 px each way, so a host that follows reports a spread of a few tens of pixels. Far more means
    // somebody else moved the pointer too; none means the moves did not arrive.
    if report.pointer_echo_samples < 4 || !(5..=120).contains(&report.pointer_max_deviation_px) {
        return Err(format!(
            "the host's pointer did not follow the moves ({} positions, spread {} px)",
            report.pointer_echo_samples, report.pointer_max_deviation_px
        ));
    }
    Ok(())
}

/// "x,y" in pointer units; anything else (the orchestrator writes `none`) is no reading.
fn parse_units(text: &str) -> Option<(u16, u16)> {
    let (x, y) = text.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// The ceremony as the user sees it: the code appears, it is compared with the host's, the verdict goes back.
fn pair(
    pump: &mut Pump<'_>,
    cfg: &ViewerConfig,
    log: &dyn Fn(&str),
    pairing_id: u32,
    placeholder: &str,
) -> Result<String, String> {
    pump.ask(Command::PairStart { pairing_id, device_id: placeholder.to_string() }, REPLY)?;
    let code = pump.wait("the pairing code", cfg.pair_timeout, |e| match e {
        UiEvent::Pairing { pairing_id: id, state: PairingState::ShowCode { code, .. } } if *id == pairing_id => {
            Some(Ok(code.clone()))
        }
        UiEvent::Pairing { pairing_id: id, state: PairingState::Failed { reason } } if *id == pairing_id => {
            Some(Err(format!("pairing failed: {reason:?}")))
        }
        _ => None,
    })??;
    log(&format!("VIEWER_CODE {code}"));
    let peer = wait_file(&cfg.peer_code, "the host's code", cfg.pair_timeout)?;
    let matches = peer == code;
    pump.ask(Command::PairConfirm { pairing_id, matches }, REPLY)?;
    if !matches {
        return Err(format!("the codes differ (viewer {code}, host {peer})"));
    }
    pump.wait("the pairing to finish", cfg.pair_timeout, |e| match e {
        UiEvent::Pairing { pairing_id: id, state: PairingState::Done { device_id } } if *id == pairing_id => {
            Some(Ok(device_id.clone()))
        }
        UiEvent::Pairing { pairing_id: id, state: PairingState::Failed { reason } } if *id == pairing_id => {
            Some(Err(format!("pairing failed: {reason:?}")))
        }
        _ => None,
    })?
}

/// The HID usage (page 7) of a lower-case letter: `a` is 4, `z` is 29.
fn letter_usage(c: char) -> Option<u16> {
    c.is_ascii_lowercase().then(|| 4 + (c as u16 - u16::from(b'a')))
}

/// The keyboard proof: waits for `ready`, types [`TYPED_WORD`] key by key, then Cmd+S.
fn type_word(
    backend: &ServiceBackend,
    session_id: u64,
    ready: &std::path::Path,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    const GAP: Duration = Duration::from_millis(40);
    const LEFT_GUI: u16 = 0xE3;
    const GUI_BIT: u8 = 0x08;
    const S: u16 = 22;
    log("VIEWER_NEED_TYPE");
    wait_file(ready, "a text editor focused on the host", timeout)?;
    let key =
        |usage: u16, down: bool, modifiers: u8| InputEvent::Key { usage_page: 7, usage, down, modifiers, locks: 0 };
    for c in TYPED_WORD.chars() {
        let usage = letter_usage(c).ok_or_else(|| format!("{c:?} is not a letter key"))?;
        backend.viewer_input(session_id, vec![key(usage, true, 0)]);
        std::thread::sleep(GAP);
        backend.viewer_input(session_id, vec![key(usage, false, 0)]);
        std::thread::sleep(GAP);
    }
    for (usage, down, modifiers) in
        [(LEFT_GUI, true, GUI_BIT), (S, true, GUI_BIT), (S, false, GUI_BIT), (LEFT_GUI, false, 0)]
    {
        backend.viewer_input(session_id, vec![key(usage, down, modifiers)]);
        std::thread::sleep(GAP);
    }
    log(&format!("TYPED {} letter keys, then Cmd+S", TYPED_WORD.len()));
    Ok(())
}

#[cfg(test)]
mod typing_tests {
    use super::*;

    #[test]
    fn the_typed_word_is_letter_keys_of_page_7() {
        assert_eq!(letter_usage('a'), Some(4));
        assert_eq!(letter_usage('z'), Some(29));
        assert_eq!(letter_usage('S'), None);
        let usages: Vec<u16> = TYPED_WORD.chars().filter_map(letter_usage).collect();
        assert_eq!(usages, vec![22, 26, 4, 19, 7, 8, 22, 14], "s w a p d e s k");
    }
}
