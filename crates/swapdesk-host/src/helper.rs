//! The helper's loop (C2.1 helper rows, C2.4): handshake as the client, the display list, then serve the service's
//! messages until `Shutdown`, `Bye` or end of stream.
//!
//! Threads: the calling thread reads the pipe; one writer thread owns the pipe's write half (see [`crate::outbox`]);
//! one capture-and-encode thread per started display (it builds its own `ScreenSource` and `VideoEncoder`, since DDA
//! sources are `!Send`); one input thread (it builds and owns the `InputSink`, applies in order); one audio thread. A fault in a
//! thread becomes a `HelperFault` and the loop goes on.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_audio::AudioSender;
use swapdesk_ipc::{
    AudioModeKind, Bytes, FrameReader, FrameWriter, Handshake, IpcError, IpcRole, MAX_FAULT_TEXT, Message, Shape, Text,
    VideoBackend,
};
use swapdesk_platform::{
    ActionRunner, AudioSource, CaptureConfig, ClipboardProvider, Clock, DisplayId, DisplayInfo, EncoderConfig,
    ErrorKind, Frame, InputSink, PlatformError, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{
    Codec, CursorPosition, CursorShapeHeader, Display, DisplayList, ErrorCode, InputEvent, MAX_VIDEO_PAYLOAD, Preset,
    VideoHeader,
};

use crate::helper_pty::{PtyFactory, PtyServer};
use crate::outbox::{DEFAULT_BYTES, Limits, Outbox, TryControl, spawn_writer};
use crate::proxy::{answer_action, answer_clipboard, no_actions, no_clipboard};

/// Longest wait of the capture loop for a frame: also how fast `StopVideo` is noticed.
const CAPTURE_POLL: Duration = Duration::from_millis(50);
/// Longest wait of the audio loop for a datagram.
const AUDIO_POLL: Duration = Duration::from_millis(20);
/// Pause between `reacquire` attempts and after a transient capture error.
const RETRY_PAUSE: Duration = Duration::from_millis(50);
/// Queued input batches before the reader waits (input is never dropped).
const INPUT_QUEUE: usize = 256;
/// Default queue of the writer: 16 messages and 4 MiB, whichever comes first (a count alone is no bound: one video
/// message can be 8 MiB).
const DEFAULT_OUTBOX: Limits = Limits::new(16, DEFAULT_BYTES);
/// Disk calls queued toward the files thread before the reader waits.
const FS_QUEUE: usize = 64;
/// Most cursor updates read per loop turn.
const CURSOR_BURST: usize = 8;
/// L4: how long the screen must stand still before one refinement frame is encoded.
const STILL_AFTER_US: u64 = 400_000;

type PResult<T> = Result<T, PlatformError>;
type ScreenFactory<S> = Arc<dyn Fn(DisplayId) -> PResult<S> + Send + Sync>;
type EncoderFactory<E> = Arc<dyn Fn(&Frame, &EncoderConfig, VideoBackend) -> PResult<E> + Send + Sync>;
type InputFactory<I> = Box<dyn Fn() -> PResult<I> + Send + Sync>;
type AudioFactory<A> = Arc<dyn Fn() -> PResult<A> + Send + Sync>;
type AudioModeFn<A> = Arc<dyn Fn(&A) -> AudioModeKind + Send + Sync>;
type ClipFactory = Arc<dyn Fn() -> PResult<Box<dyn ClipboardProvider>> + Send + Sync>;
type ActionFactory = Arc<dyn Fn() -> PResult<Box<dyn ActionRunner>> + Send + Sync>;

/// Why `run_helper` failed.
#[derive(Debug)]
pub enum HelperError {
    /// The role is not a helper role.
    BadRole(IpcRole),
    /// Handshake, framing or the pipe failed.
    Ipc(IpcError),
    /// The display enumeration failed.
    Platform(PlatformError),
    /// A thread could not be started.
    Spawn(std::io::Error),
}

impl core::fmt::Display for HelperError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadRole(r) => write!(f, "{r:?} is not a helper role"),
            Self::Ipc(e) => write!(f, "ipc: {e}"),
            Self::Platform(e) => write!(f, "platform: {e}"),
            Self::Spawn(e) => write!(f, "spawn: {e}"),
        }
    }
}

impl std::error::Error for HelperError {}

impl From<IpcError> for HelperError {
    fn from(e: IpcError) -> Self {
        Self::Ipc(e)
    }
}

/// What a finished helper run counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HelperStats {
    /// Media messages dropped because the writer's queue was full.
    pub dropped_media: u64,
    /// Messages the writer could not encode and skipped.
    pub skipped: u64,
}

/// The factories and sinks the helper's loop needs; the Windows glue fills them with the `*-win` crates and tests
/// with fakes. Factories run on the thread that uses the object, so the objects need not be `Send`.
pub struct HelperDeps<S, E, I, A> {
    screen: ScreenFactory<S>,
    encoder: EncoderFactory<E>,
    input: InputFactory<I>,
    audio: AudioFactory<A>,
    audio_mode: AudioModeFn<A>,
    clock: Arc<dyn Clock>,
    outbox: Limits,
    clipboard: Option<ClipFactory>,
    actions: Option<ActionFactory>,
    pty: Option<PtyFactory>,
    /// The disk the service may reach through this helper (the user helper only), or none.
    fs: Option<Arc<swapdesk_files::FsServer>>,
}

impl<S, E, I, A> HelperDeps<S, E, I, A> {
    /// `input`: builds the sink, called once inside the input thread (a Windows sink is `!Send`: its desktop handle is
    /// bound to the injecting thread); a failure is a `HelperFault` and input is dropped while the loop goes on.
    /// `screen`: a source for one display (called once for the enumeration with display 0, and once per capture
    /// thread). `encoder`: called with the first frame (its GPU device), the config built from it and the
    /// `StartVideo` backend; the helper then calls `configure`. `audio`: a fresh source per `StartAudio`.
    pub fn new(
        screen: impl Fn(DisplayId) -> PResult<S> + Send + Sync + 'static,
        encoder: impl Fn(&Frame, &EncoderConfig, VideoBackend) -> PResult<E> + Send + Sync + 'static,
        input: impl Fn() -> PResult<I> + Send + Sync + 'static,
        audio: impl Fn() -> PResult<A> + Send + Sync + 'static,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            screen: Arc::new(screen),
            encoder: Arc::new(encoder),
            input: Box::new(input),
            audio: Arc::new(audio),
            audio_mode: Arc::new(|_| AudioModeKind::Loopback),
            clock,
            outbox: DEFAULT_OUTBOX,
            clipboard: None,
            actions: None,
            pty: None,
            fs: None,
        }
    }

    /// The user's clipboard, built on the thread that serves `ClipChangeCount`, `ClipRead` and `ClipWrite` (stage 42).
    /// Without it those calls are answered with a failure.
    #[must_use]
    pub fn with_clipboard<C: ClipboardProvider + 'static>(
        mut self,
        f: impl Fn() -> PResult<C> + Send + Sync + 'static,
    ) -> Self {
        self.clipboard = Some(Arc::new(move || f().map(|c| Box::new(c) as Box<dyn ClipboardProvider>)));
        self
    }

    /// The user's quick actions and keep-awake, built on the thread that serves `RunAction` and `KeepAwake`.
    /// Without it those calls are answered `Unavailable`.
    #[must_use]
    pub fn with_actions<R: ActionRunner + 'static>(
        mut self,
        f: impl Fn() -> PResult<R> + Send + Sync + 'static,
    ) -> Self {
        self.actions = Some(Arc::new(move || f().map(|r| Box::new(r) as Box<dyn ActionRunner>)));
        self
    }

    /// The terminal proxy's spawner (stage 43): called with the SID of the console user the service names, it returns
    /// the `PtySpawner` that runs the shell as this process's user. Without it every `PtyOpen` is refused `Unavailable`;
    /// a system helper refuses it whatever is set.
    #[must_use]
    pub fn with_pty(
        mut self,
        factory: impl Fn(Option<String>) -> Arc<dyn swapdesk_platform::PtySpawner> + Send + Sync + 'static,
    ) -> Self {
        self.pty = Some(Arc::new(factory));
        self
    }

    /// Serves the C2.4 disk calls (`FsRequest`) of the service with `server`, on a thread of its own so a slow disk
    /// never holds up the capture or the input. Without it every call is answered `IoFailed`.
    #[must_use]
    pub fn with_fs(mut self, server: Arc<swapdesk_files::FsServer>) -> Self {
        self.fs = Some(server);
        self
    }

    /// How the capture mode is read from a started source (`AudioSource` has no such method); default `Loopback`.
    #[must_use]
    pub fn with_audio_mode(mut self, f: impl Fn(&A) -> AudioModeKind + Send + Sync + 'static) -> Self {
        self.audio_mode = Arc::new(f);
        self
    }

    /// Messages the writer queue holds before media is dropped (default 16).
    #[must_use]
    pub fn with_outbox_capacity(mut self, capacity: usize) -> Self {
        self.outbox = Limits::new(capacity, self.outbox.bytes);
        self
    }

    /// Bytes the writer queue holds before media is dropped (default 4 MiB); an empty queue takes one message of any
    /// size.
    #[must_use]
    pub fn with_outbox_bytes(mut self, bytes: usize) -> Self {
        self.outbox = self.outbox.with_bytes(bytes);
        self
    }
}

// ---------- small helpers ----------

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

fn fault(outbox: &Outbox, code: ErrorCode, what: String) {
    outbox.send_control(Message::HelperFault { code, what: Text(truncate(what, MAX_FAULT_TEXT)) });
}

fn code_of(kind: ErrorKind) -> ErrorCode {
    match kind {
        ErrorKind::PermissionDenied(_) => ErrorCode::NotPermitted,
        ErrorKind::Unavailable | ErrorKind::Closed => ErrorCode::Unavailable,
        ErrorKind::Timeout => ErrorCode::Timeout,
        _ => ErrorCode::Internal,
    }
}

fn platform_fault(outbox: &Outbox, what: &str, e: &PlatformError) {
    fault(outbox, code_of(e.kind), format!("{what}: {e}"));
}

/// Something the service logs at INFO (`HelperNote`).
fn note(outbox: &Outbox, what: String) {
    outbox.send_control(Message::HelperNote { what: Text(truncate(what, MAX_FAULT_TEXT)) });
}

/// The encoder serving a display, as the service's log shows it.
fn encoder_note(display: u16, cfg: &EncoderConfig, info: &swapdesk_platform::EncoderInfo, why: &str) -> String {
    format!(
        "video {display}: {why} {} ({}), {}x{} at {} fps, {} kbps, preset {:?}",
        info.name,
        if info.hardware { "hardware" } else { "software" },
        cfg.width,
        cfg.height,
        cfg.fps,
        cfg.kbps,
        cfg.preset
    )
}

/// A pixel position of a display of `size` pixels as the 0..=65535 of the wire (the inverse of C3's
/// `round(v * (size - 1) / 65535)`).
fn to_wire(px: i32, size: u32) -> u16 {
    if size <= 1 {
        return 0;
    }
    let span = u64::from(size - 1);
    let px = u64::try_from(px).unwrap_or(0).min(span);
    u16::try_from((px * 65535 + span / 2) / span).unwrap_or(u16::MAX)
}

fn to_proto(d: &DisplayInfo) -> Display {
    Display {
        id: d.id.0,
        name: truncate(d.name.clone(), 64),
        x: d.bounds.x,
        y: d.bounds.y,
        width: d.bounds.width,
        height: d.bounds.height,
        scale_milli: d.scale_milli,
        refresh_mhz: d.refresh_mhz,
        rotation: d.rotation,
        primary: d.primary,
    }
}

// ---------- video ----------

#[derive(Default)]
struct VideoCtl {
    stop: AtomicBool,
    force_key: AtomicBool,
    rate: Mutex<Option<(u32, u16)>>,
}

struct VideoRun {
    ctl: Arc<VideoCtl>,
    handle: JoinHandle<()>,
}

#[derive(Clone, Copy)]
struct StartParams {
    display: u16,
    fps: u16,
    kbps: u32,
    preset: Preset,
    backend: VideoBackend,
}

struct CaptureEnv<S, E> {
    screen: ScreenFactory<S>,
    encoder: EncoderFactory<E>,
    clock: Arc<dyn Clock>,
    outbox: Outbox,
    /// The displays as they were last listed: at the start, and again whenever a video starts or a capture fails,
    /// because a monitor can be plugged in or a lid closed at any time (see [`CaptureEnv::refresh_displays`]).
    displays: Mutex<Vec<DisplayInfo>>,
    /// Where a new display list goes for the input thread. Emptied when the helper ends: the input thread ends only
    /// when every sender is gone, and this one lives as long as the last capture thread.
    input_tx: Mutex<Option<SyncSender<InputCmd>>>,
    /// Bumped on every `AccessLost`; the input thread follows the input desktop when it changes.
    desktop_epoch: Arc<AtomicU64>,
}

impl<S: ScreenSource, E> CaptureEnv<S, E> {
    fn display(&self, id: u16) -> Option<DisplayInfo> {
        self.displays.lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().find(|d| d.id.0 == id).cloned()
    }

    /// Lists the displays again. When the list changed since it was last sent (a monitor plugged or unplugged, the
    /// lid closed, another primary), the service gets the new list, so it stops asking for a display that is gone and
    /// streams one that exists, and the input thread gets it, so the pointer lands on the right pixels. A listing
    /// that fails changes nothing. Returns whether the list changed.
    fn refresh_displays(&self) -> bool {
        let listed = (self.screen)(DisplayId(0)).and_then(|mut source| {
            let list = source.displays();
            source.close();
            list
        });
        let Ok(list) = listed else { return false };
        {
            let mut known = self.displays.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if *known == list {
                return false;
            }
            known.clone_from(&list);
        }
        let wire = DisplayList { displays: list.iter().take(swapdesk_ipc::MAX_DISPLAYS).map(to_proto).collect() };
        self.outbox.send_control(Message::Displays(wire));
        // The queue is the input thread's; a full one drops this copy and the next refresh sends it again only if the
        // list changes again, so the reader is never held up by it.
        if let Some(tx) = self.input_tx.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            let _ = tx.try_send(InputCmd::Displays(list));
        }
        true
    }

    /// The helper is ending: lets go of the input thread's queue.
    fn close_input(&self) {
        self.input_tx.lock().unwrap_or_else(PoisonError::into_inner).take();
    }
}

fn capture_main<S: ScreenSource, E: VideoEncoder>(
    env: &CaptureEnv<S, E>,
    p: StartParams,
    ctl: &VideoCtl,
    ids: &AtomicU32,
) {
    if let Err(e) = capture_run(env, p, ctl, ids) {
        platform_fault(&env.outbox, &format!("video {}", p.display), &e);
    }
}

fn capture_run<S: ScreenSource, E: VideoEncoder>(
    env: &CaptureEnv<S, E>,
    p: StartParams,
    ctl: &VideoCtl,
    ids: &AtomicU32,
) -> PResult<()> {
    let mut source = (env.screen)(DisplayId(p.display))?;
    if let Err(e) = source.open(DisplayId(p.display), CaptureConfig { max_fps: p.fps }) {
        // The display may have gone between the listing and now: the service learns the displays that exist.
        env.refresh_displays();
        return Err(e);
    }
    let result = capture_loop(env, p, ctl, ids, &mut source);
    source.close();
    result
}

fn capture_loop<S: ScreenSource, E: VideoEncoder>(
    env: &CaptureEnv<S, E>,
    p: StartParams,
    ctl: &VideoCtl,
    ids: &AtomicU32,
    source: &mut S,
) -> PResult<()> {
    let info = env.display(p.display);
    let adapter = info.as_ref().map(|d| d.adapter).unwrap_or(swapdesk_platform::AdapterId(0));
    let mut dims = info.as_ref().map_or((0, 0), |d| (d.bounds.width, d.bounds.height));
    let (mut fps, mut kbps) = (p.fps, p.kbps);
    let mut encoder: Option<E> = None;
    let mut config: Option<EncoderConfig> = None;
    let mut discontinuity = true;
    let mut force = true;
    let mut faulted = false;
    let mut still = Still::default();
    let mut cursor = CursorState::default();

    while !ctl.stop.load(Ordering::Acquire) && !env.outbox.is_closed() {
        if let Some((k, f)) = lock(&ctl.rate).take() {
            (kbps, fps) = (k, f);
            if let (Some(enc), Some(cfg)) = (encoder.as_mut(), config.as_mut())
                && let Err(e) = enc.set_rate(k, f)
            {
                // A rate the encoder cannot follow live (or a session it lost) does not end the video: the encoder is
                // configured again at the new rate, and the stream restarts with a keyframe. Only a failed
                // configure ends the loop, as at the start.
                (cfg.kbps, cfg.fps) = (k, f);
                let info = enc.configure(cfg)?;
                note(&env.outbox, encoder_note(p.display, cfg, &info, &format!("set_rate failed ({e}), rebuilt:")));
                discontinuity = true;
                force = true;
            }
        }
        match source.next_frame(CAPTURE_POLL) {
            Ok(frame) => {
                faulted = false;
                if let Some(frame) = &frame {
                    dims = (frame.width, frame.height);
                }
                pump_cursor(env, source, p.display, dims, &mut cursor);
                let Some(frame) = frame else {
                    if let Some(enc) = encoder.as_mut() {
                        refine_still(env, p, ids, enc, &mut still, &mut discontinuity, &mut force);
                    }
                    continue;
                };
                let size_changed = config.is_some_and(|c| c.width != frame.width || c.height != frame.height);
                if encoder.is_none() || size_changed {
                    let cfg = EncoderConfig {
                        width: frame.width,
                        height: frame.height,
                        fps,
                        kbps,
                        preset: p.preset,
                        adapter,
                    };
                    let enc = match encoder.as_mut() {
                        Some(e) => e,
                        None => encoder.insert((env.encoder)(&frame, &cfg, p.backend)?),
                    };
                    let info = enc.configure(&cfg)?;
                    note(&env.outbox, encoder_note(p.display, &cfg, &info, "encoding with"));
                    config = Some(cfg);
                    discontinuity = true;
                    force = true;
                }
                let Some(enc) = encoder.as_mut() else { continue };
                let wanted = force || ctl.force_key.swap(false, Ordering::AcqRel);
                let started = env.clock.now();
                let out = enc.encode(&frame, wanted)?;
                let spent = env.clock.now().0.saturating_sub(started.0);
                let encode_us = u32::try_from(spent).unwrap_or(u32::MAX);
                // Best effort force: ask again until a keyframe comes out.
                force = wanted && !out.iter().any(|f| f.keyframe);
                emit_frames(env, p, ids, out, encode_us, &mut discontinuity, &mut force);
                still.frame = Some(frame);
                still.since = env.clock.now().0;
                still.refined = false;
            }
            Err(e) if e.kind == ErrorKind::AccessLost => {
                env.desktop_epoch.fetch_add(1, Ordering::AcqRel);
                loop {
                    if ctl.stop.load(Ordering::Acquire) || env.outbox.is_closed() {
                        return Ok(());
                    }
                    match source.reacquire() {
                        Ok(()) => break,
                        Err(e) => {
                            if !faulted {
                                platform_fault(&env.outbox, &format!("video {}: reacquire", p.display), &e);
                                faulted = true;
                            }
                            // The display itself may be gone (unplugged, the lid closed): the service then gets the
                            // displays that exist and starts the video on one of them; this thread has nothing left
                            // to capture.
                            if env.refresh_displays() && env.display(p.display).is_none() {
                                return Ok(());
                            }
                            std::thread::sleep(RETRY_PAUSE);
                        }
                    }
                }
                faulted = false;
                discontinuity = true;
                force = true;
            }
            Err(e) if matches!(e.kind, ErrorKind::Unavailable | ErrorKind::Timeout) => {
                if !faulted {
                    platform_fault(&env.outbox, &format!("video {}", p.display), &e);
                    faulted = true;
                }
                std::thread::sleep(RETRY_PAUSE);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Sends what the encoder produced for one display as video frames, keeping the discontinuity and force-keyframe
/// bookkeeping of the capture loop.
fn emit_frames<S: ScreenSource, E: VideoEncoder>(
    env: &CaptureEnv<S, E>,
    p: StartParams,
    ids: &AtomicU32,
    out: Vec<swapdesk_platform::EncodedFrame>,
    encode_us: u32,
    discontinuity: &mut bool,
    force: &mut bool,
) {
    for e in out {
        let Ok(payload_len) = u32::try_from(e.annex_b.len()) else { continue };
        if payload_len > MAX_VIDEO_PAYLOAD {
            fault(&env.outbox, ErrorCode::TooLarge, format!("video {}: frame over limit", p.display));
            *discontinuity = true;
            *force = true;
            continue;
        }
        let header = VideoHeader {
            keyframe: e.keyframe,
            config: e.config_change,
            discontinuity: *discontinuity,
            display_id: p.display,
            frame_id: ids.fetch_add(1, Ordering::AcqRel),
            capture_us: e.captured_at.0,
            encode_us: if encode_us == 0 { e.encode_us } else { encode_us },
            width: e.width,
            height: e.height,
            codec: Codec::H264,
            payload_len,
        };
        let sent = env.outbox.send_media(Message::VideoFrame { header, payload: Bytes(e.annex_b) });
        if sent {
            *discontinuity = false;
        } else {
            // A dropped frame breaks the chain: the next one restarts it.
            *discontinuity = true;
            *force = true;
        }
    }
}

/// L4: the last picture the screen showed and when, for the still-screen refinement.
#[derive(Default)]
struct Still {
    frame: Option<Frame>,
    /// Clock reading when `frame` was encoded.
    since: u64,
    /// The refinement frame of this picture was sent (or refused).
    refined: bool,
    /// A refusal was reported (once per session).
    refuse_logged: bool,
}

/// L4: when the screen has not changed for [`STILL_AFTER_US`], encodes the last picture once as a high-quality IDR and
/// sends it like any frame; nothing more follows until the next change. An encoder that refuses is not asked again.
fn refine_still<S: ScreenSource, E: VideoEncoder>(
    env: &CaptureEnv<S, E>,
    p: StartParams,
    ids: &AtomicU32,
    enc: &mut E,
    still: &mut Still,
    discontinuity: &mut bool,
    force: &mut bool,
) {
    let Some(frame) = still.frame.as_ref() else { return };
    if still.refined || env.clock.now().0.saturating_sub(still.since) < STILL_AFTER_US {
        return;
    }
    still.refined = true;
    let started = env.clock.now();
    match enc.refine(frame) {
        Ok(out) => {
            let spent = env.clock.now().0.saturating_sub(started.0);
            emit_frames(env, p, ids, out, u32::try_from(spent).unwrap_or(u32::MAX), discontinuity, force);
        }
        Err(e) => {
            // Once per session, and the picture stays as the last frame left it.
            if !still.refuse_logged {
                still.refuse_logged = true;
                platform_fault(&env.outbox, &format!("video {}: still-screen refinement off", p.display), &e);
            }
        }
    }
}

/// What the capture thread remembers about the cursor between two looks.
#[derive(Default)]
struct CursorState {
    seq: u32,
    /// The shape the viewer holds: positions name it, so it moves on only once its message is queued.
    shape_id: u32,
    /// A shape the queue had no room for, sent again at the next look (positions keep naming the old one meanwhile).
    unsent: Option<(u32, Message)>,
}

fn pump_cursor<S: ScreenSource, E>(
    env: &CaptureEnv<S, E>,
    source: &mut S,
    display: u16,
    dims: (u32, u32),
    cursor: &mut CursorState,
) {
    // A shape is up to 256 KiB: it goes through the media path, so a full pipe makes the capture thread drop it and
    // look again, never wait inside `write`.
    if let Some((id, msg)) = cursor.unsent.take() {
        match env.outbox.try_send_media(msg) {
            TryControl::Sent => cursor.shape_id = id,
            TryControl::Full(back) => cursor.unsent = Some((id, back)),
            TryControl::Closed => {}
        }
    }
    for _ in 0..CURSOR_BURST {
        let Some(update) = source.take_cursor() else { break };
        if let Some(shape) = update.shape {
            let id = shape.id;
            let msg = Message::CursorShape(Shape(swapdesk_proto::CursorShape {
                header: CursorShapeHeader {
                    shape_id: id,
                    width: shape.width,
                    height: shape.height,
                    hot_x: shape.hot_x,
                    hot_y: shape.hot_y,
                },
                rgba: shape.rgba,
            }));
            // The newest shape replaces one that was never sent: the older would only be shown for an instant.
            cursor.unsent = None;
            match env.outbox.try_send_media(msg) {
                TryControl::Sent => cursor.shape_id = id,
                TryControl::Full(back) => cursor.unsent = Some((id, back)),
                TryControl::Closed => {}
            }
        }
        let pos = CursorPosition {
            visible: update.visible,
            display,
            seq: cursor.seq,
            x: to_wire(update.x, dims.0),
            y: to_wire(update.y, dims.1),
            shape_id: cursor.shape_id,
        };
        cursor.seq = cursor.seq.wrapping_add(1);
        env.outbox.send_media(Message::CursorPos(pos));
    }
}

// ---------- input ----------

enum InputCmd {
    Events(Vec<InputEvent>),
    ReleaseAll,
    /// The displays changed: the sink maps the pointer onto the new ones.
    Displays(Vec<DisplayInfo>),
}

fn input_main<I: InputSink>(
    factory: &InputFactory<I>,
    displays: &[DisplayInfo],
    rx: &Receiver<InputCmd>,
    outbox: &Outbox,
    epoch: &AtomicU64,
) {
    let mut sink = match factory() {
        Ok(sink) => sink,
        Err(e) => {
            // Input is dropped from now on; the reader never blocks on a full queue.
            platform_fault(outbox, "input sink", &e);
            while rx.recv().is_ok() {}
            return;
        }
    };
    sink.displays_changed(displays);
    let mut faulted = false;
    let report = |faulted: &mut bool, what: &str, e: &PlatformError| {
        if !*faulted {
            platform_fault(outbox, what, e);
            *faulted = true;
        }
    };
    let mut seen = epoch.load(Ordering::Acquire);
    if let Err(e) = sink.follow_input_desktop() {
        report(&mut faulted, "input desktop", &e);
    }
    while let Ok(cmd) = rx.recv() {
        let now = epoch.load(Ordering::Acquire);
        if now != seen {
            seen = now;
            if let Err(e) = sink.follow_input_desktop() {
                report(&mut faulted, "input desktop", &e);
            }
        }
        let mut failed = None;
        match cmd {
            InputCmd::Events(events) => {
                for event in &events {
                    let r =
                        if matches!(event, InputEvent::ReleaseAll) { sink.release_all() } else { sink.apply(event) };
                    if let Err(e) = r {
                        failed = Some(e);
                    }
                }
            }
            InputCmd::Displays(list) => sink.displays_changed(&list),
            InputCmd::ReleaseAll => {
                if let Err(e) = sink.release_all() {
                    failed = Some(e);
                }
            }
        }
        match failed {
            Some(e) => report(&mut faulted, "input", &e),
            None => faulted = false,
        }
    }
    // Nothing may stay pressed when the service goes away.
    let _ = sink.release_all();
}

// ---------- audio ----------

struct AudioEnv<A> {
    source: AudioFactory<A>,
    mode: AudioModeFn<A>,
    outbox: Outbox,
}

struct AudioRun {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

fn audio_main<A: AudioSource>(env: &AudioEnv<A>, exclude_pid: Option<u32>, stop: &AtomicBool, muted: &AtomicBool) {
    let outbox = &env.outbox;
    let run = || -> Result<(), String> {
        let source = (env.source)().map_err(|e| format!("audio source: {e}"))?;
        let mut sender = AudioSender::new(source, exclude_pid).map_err(|e| format!("audio: {e}"))?;
        let mut is_muted = muted.load(Ordering::Acquire);
        sender.set_muted(is_muted).map_err(|e| format!("audio: {e}"))?;
        sender.set_enabled(true).map_err(|e| format!("audio: {e}"))?;
        outbox.send_control(Message::AudioMode { mode: (env.mode)(sender.source()) });
        let result = loop {
            if stop.load(Ordering::Acquire) || outbox.is_closed() {
                break Ok(());
            }
            let want = muted.load(Ordering::Acquire);
            if want != is_muted {
                is_muted = want;
                if let Err(e) = sender.set_muted(want) {
                    break Err(format!("audio: {e}"));
                }
            }
            match sender.poll(AUDIO_POLL) {
                Ok(Some(datagram)) => {
                    outbox.send_media(Message::AudioPacket(Bytes(datagram)));
                }
                Ok(None) => {}
                Err(e) => break Err(format!("audio: {e}")),
            }
        };
        let _ = sender.set_enabled(false);
        result
    };
    if let Err(what) = run() {
        fault(outbox, ErrorCode::Unavailable, what);
    }
    outbox.send_control(Message::AudioMode { mode: AudioModeKind::Off });
}

// ---------- clipboard and quick actions ----------

/// Messages queued for the clipboard and actions thread before the reader waits.
const EXTRAS_QUEUE: usize = 64;

/// Serves the clipboard trio and the action pair, one message at a time and in order, on a thread of its own: the
/// platform objects are built here on first use, and a slow clipboard never holds the pipe's reader.
fn extras_main(clip: Option<ClipFactory>, actions: Option<ActionFactory>, rx: &Receiver<Message>, outbox: &Outbox) {
    let mut clipboard: Option<Box<dyn ClipboardProvider>> = None;
    let mut runner: Option<Box<dyn ActionRunner>> = None;
    while let Ok(msg) = rx.recv() {
        let reply = match &msg {
            Message::ClipChangeCount { .. } | Message::ClipRead { .. } | Message::ClipWrite { .. } => {
                if clipboard.is_none() {
                    match clip.as_ref().map(|f| f()) {
                        Some(Ok(c)) => clipboard = Some(c),
                        Some(Err(e)) => platform_fault(outbox, "clipboard", &e),
                        None => {}
                    }
                }
                match clipboard.as_mut() {
                    Some(c) => answer_clipboard(c.as_mut(), &msg),
                    None => no_clipboard(&msg),
                }
            }
            Message::RunAction { .. } | Message::KeepAwake { .. } => {
                if runner.is_none() {
                    match actions.as_ref().map(|f| f()) {
                        Some(Ok(r)) => runner = Some(r),
                        Some(Err(e)) => platform_fault(outbox, "actions", &e),
                        None => {}
                    }
                }
                match runner.as_mut() {
                    Some(r) => answer_action(r.as_mut(), &msg),
                    None => no_actions(&msg),
                }
            }
            _ => None,
        };
        if let Some(reply) = reply {
            outbox.send_control(reply);
        }
    }
}

// ---------- the loop ----------

struct Host<S, E, A> {
    capture: Arc<CaptureEnv<S, E>>,
    audio_env: Arc<AudioEnv<A>>,
    videos: HashMap<u16, VideoRun>,
    frame_ids: HashMap<u16, Arc<AtomicU32>>,
    audio: Option<AudioRun>,
    /// Capture and audio threads told to stop that have not ended yet. The thread that reads the pipe never joins them:
    /// one stuck in a send would hide the `Shutdown` that follows. They are joined at the end of the run.
    retiring: HashMap<u16, JoinHandle<()>>,
    /// The same for the audio thread (there is one).
    retiring_audio: Option<JoinHandle<()>>,
    muted: Arc<AtomicBool>,
    input_tx: SyncSender<InputCmd>,
    extras_tx: SyncSender<Message>,
    outbox: Outbox,
    pty: PtyServer,
    /// Disk calls toward the files thread; `None` when this helper serves no disk.
    fs_tx: Option<SyncSender<Message>>,
}

impl<S, E, A> Host<S, E, A>
where
    S: ScreenSource + 'static,
    E: VideoEncoder + 'static,
    A: AudioSource + 'static,
{
    /// `false` when the loop must end.
    fn handle(&mut self, msg: Message) -> bool {
        match msg {
            Message::Shutdown | Message::Bye { .. } => return false,
            Message::StartVideo { display, fps, kbps, quality, backend } => {
                self.start_video(StartParams { display, fps: fps.max(1), kbps, preset: quality, backend });
            }
            Message::StopVideo { display } => self.stop_video(display),
            Message::SetRate { display, kbps, fps } => {
                if let Some(run) = self.videos.get(&display) {
                    *lock(&run.ctl.rate) = Some((kbps, fps.max(1)));
                }
            }
            Message::ForceKeyframe { display } => {
                if let Some(run) = self.videos.get(&display) {
                    run.ctl.force_key.store(true, Ordering::Release);
                }
            }
            Message::Input { events } => {
                let _ = self.input_tx.send(InputCmd::Events(events.0));
            }
            Message::ReleaseAll => {
                let _ = self.input_tx.send(InputCmd::ReleaseAll);
            }
            Message::StartAudio { exclude_pid } => self.start_audio((exclude_pid != 0).then_some(exclude_pid)),
            Message::StopAudio => self.stop_audio(),
            Message::SetAudioMuted { muted } => self.muted.store(muted, Ordering::Release),
            m @ (Message::ClipChangeCount { .. }
            | Message::ClipRead { .. }
            | Message::ClipWrite { .. }
            | Message::RunAction { .. }
            | Message::KeepAwake { .. }) => {
                // Never dropped: the service waits for the answer. A full queue makes the reader wait.
                let _ = self.extras_tx.send(m);
            }
            m @ (Message::PtyOpen { .. }
            | Message::PtyInput { .. }
            | Message::PtyResize { .. }
            | Message::PtyClose { .. }) => {
                self.pty.handle(m);
            }
            Message::FsRequest { req, call } => match &self.fs_tx {
                // The reader waits when the queue is full: calls are answered in order and never dropped.
                Some(tx) => {
                    let _ = tx.send(Message::FsRequest { req, call });
                }
                None if call.is_fire_and_forget() => {}
                None => {
                    self.outbox.send_control(Message::FsReply { req, result: Err(ErrorCode::IoFailed) });
                }
            },
            other => fault(
                &self.outbox,
                ErrorCode::ProtocolViolation,
                format!("unexpected message {:#x}", other.type_code()),
            ),
        }
        true
    }

    fn start_video(&mut self, p: StartParams) {
        // The list sent at the start can be hours old: a monitor plugged in since then changed the ids. A session
        // that starts now must find the displays of now (the service asks again with the list this sends).
        self.capture.refresh_displays();
        if self.capture.display(p.display).is_none() {
            fault(&self.outbox, ErrorCode::NotFound, format!("video {}: no such display", p.display));
            return;
        }
        // A second StartVideo for a display restarts it with the new settings: the new thread waits for the old one to
        // end before it opens the source, so the reader of the pipe never does.
        // A thread stopped earlier and still ending counts too: it is this display's, and it still holds the source.
        let previous = self
            .videos
            .remove(&p.display)
            .map(|run| {
                run.ctl.stop.store(true, Ordering::Release);
                run.handle
            })
            .or_else(|| self.retiring.remove(&p.display));
        let ctl = Arc::new(VideoCtl::default());
        let ids = Arc::clone(self.frame_ids.entry(p.display).or_default());
        let env = Arc::clone(&self.capture);
        let thread_ctl = Arc::clone(&ctl);
        let spawned = std::thread::Builder::new().name(format!("helper-capture-{}", p.display)).spawn(move || {
            if let Some(old) = previous {
                let _ = old.join();
            }
            capture_main(&env, p, &thread_ctl, &ids);
        });
        match spawned {
            Ok(handle) => {
                self.videos.insert(p.display, VideoRun { ctl, handle });
            }
            Err(e) => fault(&self.outbox, ErrorCode::Internal, format!("video {}: spawn: {e}", p.display)),
        }
        self.reap();
    }

    /// Tells the capture thread of `display` to stop and goes on; the thread ends on its own (within one capture poll).
    fn stop_video(&mut self, display: u16) {
        if let Some(run) = self.videos.remove(&display) {
            run.ctl.stop.store(true, Ordering::Release);
            // Kept under its display: a `StartVideo` for it waits for this thread before it opens the source again.
            self.retiring.insert(display, run.handle);
        }
        self.reap();
    }

    fn start_audio(&mut self, exclude_pid: Option<u32>) {
        let previous = self
            .audio
            .take()
            .map(|run| {
                run.stop.store(true, Ordering::Release);
                run.handle
            })
            .or_else(|| self.retiring_audio.take());
        let stop = Arc::new(AtomicBool::new(false));
        let env = Arc::clone(&self.audio_env);
        let muted = Arc::clone(&self.muted);
        let thread_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new().name("helper-audio".into()).spawn(move || {
            if let Some(old) = previous {
                let _ = old.join();
            }
            audio_main(&env, exclude_pid, &thread_stop, &muted);
        });
        match spawned {
            Ok(handle) => self.audio = Some(AudioRun { stop, handle }),
            Err(e) => fault(&self.outbox, ErrorCode::Internal, format!("audio: spawn: {e}")),
        }
        self.reap();
    }

    fn stop_audio(&mut self) {
        if let Some(run) = self.audio.take() {
            run.stop.store(true, Ordering::Release);
            self.retiring_audio = Some(run.handle);
        }
        self.reap();
    }

    /// Joins the stopped threads that have ended (it does not wait for the others).
    fn reap(&mut self) {
        let (done, running): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.retiring).into_iter().partition(|(_, h)| h.is_finished());
        self.retiring = running.into_iter().collect();
        for (_, h) in done {
            let _ = h.join();
        }
        if self.retiring_audio.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(h) = self.retiring_audio.take()
        {
            let _ = h.join();
        }
    }

    fn stop_all(&mut self) {
        let displays: Vec<u16> = self.videos.keys().copied().collect();
        for d in displays {
            self.stop_video(d);
        }
        self.stop_audio();
        // The end of the run: now it is right to wait for every thread.
        for (_, h) in std::mem::take(&mut self.retiring) {
            let _ = h.join();
        }
        if let Some(h) = self.retiring_audio.take() {
            let _ = h.join();
        }
        self.pty.stop();
    }
}

/// Runs the helper over `reader` / `writer` as `role` (a helper role) with process id `pid`: the client handshake,
/// the `Displays` message, then the service's messages until `Shutdown`, `Bye` or the end of the stream, all of which
/// end with `Ok` after every thread is joined. A broken stream in the middle of a message is `Err`.
///
/// `I` need not be `Send`: the input thread builds the sink itself (it calls `follow_input_desktop` on it).
pub fn run_helper<R, W, S, E, I, A>(
    reader: R,
    writer: W,
    deps: HelperDeps<S, E, I, A>,
    role: IpcRole,
    pid: u32,
) -> Result<HelperStats, HelperError>
where
    R: Read,
    W: Write + Send + 'static,
    S: ScreenSource + 'static,
    E: VideoEncoder + 'static,
    I: InputSink + 'static,
    A: AudioSource + 'static,
{
    if !matches!(role, IpcRole::HelperSystem | IpcRole::HelperUser) {
        return Err(HelperError::BadRole(role));
    }
    let mut reader = FrameReader::new(reader);
    let mut framed = FrameWriter::new(writer);
    Handshake::client(&mut reader, &mut framed, role, pid)?;

    let HelperDeps {
        screen,
        encoder,
        input,
        audio,
        audio_mode,
        clock,
        outbox: outbox_limits,
        clipboard,
        actions,
        pty,
        fs,
    } = deps;

    let displays = {
        let mut source = screen(DisplayId(0)).map_err(HelperError::Platform)?;
        let list = source.displays().map_err(HelperError::Platform)?;
        source.close();
        list
    };
    let wire_list = DisplayList { displays: displays.iter().take(swapdesk_ipc::MAX_DISPLAYS).map(to_proto).collect() };

    let (outbox, rx) = Outbox::with_limits(outbox_limits);
    let writer_thread = spawn_writer(&outbox, rx, framed).map_err(HelperError::Spawn)?;
    outbox.send_control(Message::Displays(wire_list));

    let desktop_epoch = Arc::new(AtomicU64::new(0));
    let (input_tx, input_rx) = sync_channel(INPUT_QUEUE);
    let input_thread = {
        let outbox = outbox.clone();
        let epoch = Arc::clone(&desktop_epoch);
        let for_input = displays.clone();
        std::thread::Builder::new()
            .name("helper-input".into())
            .spawn(move || input_main(&input, &for_input, &input_rx, &outbox, &epoch))
            .map_err(HelperError::Spawn)?
    };

    let (extras_tx, extras_rx) = sync_channel(EXTRAS_QUEUE);
    let extras_thread = {
        let outbox = outbox.clone();
        std::thread::Builder::new()
            .name("helper-extras".into())
            .spawn(move || extras_main(clipboard, actions, &extras_rx, &outbox))
            .map_err(HelperError::Spawn)?
    };

    let (fs_tx, fs_thread) = match fs {
        Some(server) => {
            let (tx, rx) = sync_channel::<Message>(FS_QUEUE);
            let outbox = outbox.clone();
            let thread = std::thread::Builder::new()
                .name("helper-fs".into())
                .spawn(move || {
                    while let Ok(msg) = rx.recv() {
                        if let Some(reply) = server.serve(msg) {
                            outbox.send_control(reply);
                        }
                    }
                })
                .map_err(HelperError::Spawn)?;
            (Some(tx), Some(thread))
        }
        None => (None, None),
    };

    let mut host = Host {
        capture: Arc::new(CaptureEnv {
            screen,
            encoder,
            clock,
            outbox: outbox.clone(),
            displays: Mutex::new(displays),
            input_tx: Mutex::new(Some(input_tx.clone())),
            desktop_epoch,
        }),
        audio_env: Arc::new(AudioEnv { source: audio, mode: audio_mode, outbox: outbox.clone() }),
        videos: HashMap::new(),
        frame_ids: HashMap::new(),
        audio: None,
        retiring: HashMap::new(),
        retiring_audio: None,
        muted: Arc::new(AtomicBool::new(false)),
        input_tx,
        extras_tx,
        outbox: outbox.clone(),
        pty: PtyServer::new(pty, role, &outbox).map_err(HelperError::Spawn)?,
        fs_tx,
    };

    let mut result = loop {
        match reader.read_message() {
            Ok(None) => break Ok(()),
            Err(e) => break Err(HelperError::Ipc(e)),
            Ok(Some(msg)) => {
                if !host.handle(msg) {
                    break Ok(());
                }
            }
        }
    };

    host.stop_all();
    let Host {
        input_tx,
        extras_tx,
        capture,
        audio_env,
        outbox: host_outbox,
        videos: _,
        frame_ids: _,
        audio: _,
        retiring: _,
        retiring_audio: _,
        muted: _,
        pty,
        fs_tx,
    } = host;
    // The terminal server holds a clone of the outbox too: the writer ends only when every clone is gone.
    drop(pty);
    drop(host_outbox);
    drop(input_tx);
    capture.close_input();
    drop(extras_tx);
    drop(fs_tx);
    if let Some(t) = fs_thread {
        let _ = t.join();
    }
    let _ = input_thread.join();
    let _ = extras_thread.join();
    drop(capture);
    drop(audio_env);
    let dropped_media = outbox.dropped();
    drop(outbox);
    let report = writer_thread.join().ok();
    let mut stats = HelperStats { dropped_media, skipped: 0 };
    if let Some(report) = report {
        stats.skipped = report.skipped;
        if result.is_ok()
            && let Some(e) = report.io_error
        {
            result = Err(HelperError::Ipc(e));
        }
    }
    result.map(|()| stats)
}

#[cfg(test)]
#[path = "helper_refine_tests.rs"]
mod refine_tests;

#[cfg(test)]
#[path = "helper_stall_tests.rs"]
mod stall_tests;

#[cfg(test)]
#[path = "helper_rate_tests.rs"]
mod rate_tests;
