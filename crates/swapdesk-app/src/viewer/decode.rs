//! The decode thread: the only owner of the `MfH264Decoder` and the `WinRenderer` (nothing in either is `Send`, C3).
//!
//! Frames come from the [`Mailbox`]; the thread decodes each to an NV12 texture on the shell's own D3D11 device (the
//! renderer's device is handed to the decoder, so the picture never leaves the GPU), presents it into the viewer
//! window's video visual, and keeps the feedback of [`Feedback`]: a keyframe request when the decoder fails, loses the
//! thread of frame ids or has to wait, and the once-a-second report. Window changes arrive as commands (the shell
//! marshals `WM_SIZE` and `WM_DPICHANGED`, as the video rectangle that follows them, through
//! `ViewerHost::resized`). The mapping the UI thread needs for the pointer is published after every picture, resize
//! and scale change.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_codec_win::{DecoderOptions, DecoderOutput, MfH264Decoder};
use swapdesk_platform::{ErrorKind, NativeSurface, Rect, Renderer, ScaleMode, VideoDecoder};
use swapdesk_render_win::{Probe, RendererOptions, WinRenderer};

use super::ViewerOut;
use super::cursor::CursorTracker;
use super::feedback::Feedback;
use super::mailbox::{Cmd, Frame, Item, Mailbox};
use super::pointer::PointerMap;

/// How long the thread sleeps with nothing to do (the report and the retries are checked at least this often).
const IDLE: Duration = Duration::from_millis(200);
/// While the decoder owes a picture (its input produced none yet) the thread wakes this often to poll for it, without blocking.
const OWED_POLL: Duration = Duration::from_millis(1);
/// Polls made for one owed picture before giving up on it (about 40 ms).
const OWED_POLLS: u32 = 40;

/// One picture that reached the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentEvent {
    pub frame_id: u32,
    pub display: u16,
    pub width: u32,
    pub height: u32,
    pub decode_us: u32,
    /// Arrival in the viewer to the end of `present`.
    pub lag_us: u32,
    /// The machine's monotonic clock right after `Present` returned.
    pub presented_at_us: u64,
    /// Inside `present`: the wait for the swap chain's frame-latency object, recording the blit, the `Present` call.
    pub wait_us: u32,
    pub blt_us: u32,
    pub present_us: u32,
}

pub type PresentObserver = Arc<dyn Fn(&PresentEvent) + Send + Sync>;
pub type ProbeObserver = Arc<dyn Fn(&PresentEvent, &Probe) + Send + Sync>;

/// Measurements only: a rectangle of the swap chain (in its pixels) copied from every picture just before it is
/// presented, handed to `on_probe` on the decode thread once the GPU has finished the copy. The wait for that copy
/// comes after the picture's present time was taken; it delays the next frame's decode, which a measurement accepts
/// and a session never pays (the default is no tap).
#[derive(Clone)]
pub struct ProbeTap {
    pub rect: (u32, u32, u32, u32),
    pub on_probe: ProbeObserver,
}

/// What the decode thread shares with the rest of the pipeline.
#[derive(Default)]
pub struct Shared {
    /// The pointer mapping, once a picture is on screen.
    pub map: Mutex<Option<PointerMap>>,
    pub presented: AtomicU64,
    pub last_frame_id: AtomicU64,
    pub lost: AtomicU64,
    pub keyframe_requests: AtomicU64,
    pub decode_errors: AtomicU64,
    /// The renderer is attached.
    pub attached: AtomicBool,
    /// The remote cursor is drawn (its shape is known and the viewer does not control).
    pub cursor_drawn: AtomicBool,
    /// The viewer controls the host: the local pointer is shown with the host's shape and the remote arrow is not
    /// drawn. Written by the pipeline, read here.
    pub local_pointer: AtomicBool,
    /// The host's current cursor shape, for the local pointer.
    pub pointer_shape: Mutex<Option<Arc<swapdesk_platform::CursorShape>>>,
    /// The last renderer or decoder failure, for the trace (never content).
    pub last_error: Mutex<Option<String>>,
    pub audio_underruns: Arc<std::sync::atomic::AtomicU32>,
}

impl Shared {
    fn error(&self, what: &str, e: impl std::fmt::Display) {
        *self.last_error.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("{what}: {e}"));
    }
}

pub struct Worker {
    mailbox: Arc<Mailbox>,
    out: Arc<Mutex<Option<Arc<dyn ViewerOut>>>>,
    shared: Arc<Shared>,
    observer: Option<PresentObserver>,
    probe: Option<ProbeTap>,
    renderer: Option<WinRenderer>,
    decoder: Option<MfH264Decoder>,
    surface: Option<(NativeSurface, Rect)>,
    scale: ScaleMode,
    frame: Option<(u32, u32)>,
    display: u16,
    session: u64,
    feedback: Feedback,
    cursor: CursorTracker,
    /// Polls left for the picture the decoder owes, and the arrival of the frame that fed it.
    owed: Option<(u32, Instant)>,
}

pub fn spawn(
    mailbox: Arc<Mailbox>,
    out: Arc<Mutex<Option<Arc<dyn ViewerOut>>>>,
    shared: Arc<Shared>,
    observer: Option<PresentObserver>,
    probe: Option<ProbeTap>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("swapdesk-viewer-decode".into()).spawn(move || {
        let mut w = Worker {
            mailbox,
            out,
            shared,
            observer,
            probe,
            renderer: None,
            decoder: None,
            surface: None,
            scale: ScaleMode::Fit,
            frame: None,
            display: 0,
            session: 0,
            feedback: Feedback::new(Instant::now()),
            cursor: CursorTracker::new(),
            owed: None,
        };
        w.run();
    })
}

impl Worker {
    fn out(&self) -> Option<Arc<dyn ViewerOut>> {
        self.out.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn run(&mut self) {
        loop {
            let wait = if self.owed.is_some() { OWED_POLL } else { IDLE };
            match self.mailbox.next(wait) {
                Item::Cmd(c) => self.command(c),
                Item::Shape(s) => {
                    self.cursor.on_shape(s);
                    let update = self.cursor.refresh(self.frame);
                    self.apply_cursor(update);
                }
                Item::Cursor(session, pos) => {
                    self.session = session;
                    let update = self.cursor.on_position(&pos, self.frame);
                    self.apply_cursor(update);
                }
                Item::Reset => {
                    self.owed = None;
                    if let Some(d) = self.decoder.as_mut() {
                        d.reset();
                    }
                    self.request_keyframe(true);
                }
                Item::Frame(f) => self.frame_in(f),
                Item::Timeout => {
                    self.poll_owed();
                    // A decoder still waiting for a keyframe asks again after the retry interval.
                    if self.decoder.as_ref().is_some_and(MfH264Decoder::waiting_for_keyframe) {
                        self.request_keyframe(false);
                    }
                }
                Item::Closed => break,
            }
            let dropped = self.mailbox.take_dropped();
            if dropped > 0 {
                self.feedback.on_lost(u32::try_from(dropped).unwrap_or(u32::MAX));
                self.shared.lost.fetch_add(dropped, Ordering::Relaxed);
            }
            let underruns = self.shared.audio_underruns.load(Ordering::Relaxed);
            if let Some(report) = self.feedback.report(Instant::now(), underruns)
                && self.session != 0
                && let Some(out) = self.out()
            {
                out.report(self.session, report);
            }
        }
        self.detach();
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Attach { surface, area, scale } => {
                self.scale = scale;
                self.surface = Some((surface, area));
                self.attach();
            }
            Cmd::Resize(area) => {
                if let Some((_, a)) = self.surface.as_mut() {
                    *a = area;
                }
                if let Some(r) = self.renderer.as_mut()
                    && let Err(e) = r.resize(area)
                {
                    self.shared.error("resize", e);
                }
                self.publish_map();
            }
            Cmd::Scale(mode) => {
                self.scale = mode;
                if let Some(r) = self.renderer.as_mut() {
                    // Draws the last picture again under the new scale (a static host sends no new one).
                    let _ = r.set_scale(mode);
                }
                self.publish_map();
            }
            Cmd::CursorMode => {
                let update = self.cursor.refresh(self.frame);
                self.apply_cursor(update);
            }
            Cmd::Detach(ack) => {
                self.surface = None;
                self.detach();
                let _ = ack.send(());
            }
        }
    }

    /// Builds the renderer on the window's surface and the decoder on the renderer's device.
    fn attach(&mut self) {
        self.detach_renderer();
        let Some((surface, area)) = self.surface else { return };
        let mut renderer = WinRenderer::new(RendererOptions::default());
        if let Err(e) = renderer.attach(surface, area) {
            self.shared.error("attach", e);
            return;
        }
        if let Some(tap) = &self.probe {
            renderer.set_probe(Some(tap.rect));
        }
        self.decoder = Some(MfH264Decoder::new(DecoderOptions {
            output: DecoderOutput::Gpu,
            device: renderer.device(),
            ..DecoderOptions::default()
        }));
        self.renderer = Some(renderer);
        self.shared.attached.store(true, Ordering::SeqCst);
        // The stream started before the window did: the first picture needs a keyframe.
        self.request_keyframe(true);
    }

    fn detach_renderer(&mut self) {
        self.decoder = None;
        if let Some(mut r) = self.renderer.take() {
            r.detach();
        }
        self.frame = None;
        self.shared.attached.store(false, Ordering::SeqCst);
        self.shared.cursor_drawn.store(false, Ordering::SeqCst);
        *self.shared.pointer_shape.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.shared.map.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn detach(&mut self) {
        self.detach_renderer();
    }

    fn publish_map(&self) {
        let map = match (&self.surface, self.frame) {
            (Some((_, area)), Some(frame)) => {
                Some(PointerMap { area: *area, frame, display: self.display, mode: self.scale })
            }
            _ => None,
        };
        *self.shared.map.lock().unwrap_or_else(PoisonError::into_inner) = map;
    }

    fn request_keyframe(&mut self, urgent: bool) {
        if self.session == 0 {
            return;
        }
        if let Some(req) = self.feedback.keyframe(Instant::now(), self.display, urgent)
            && let Some(out) = self.out()
        {
            self.shared.keyframe_requests.fetch_add(1, Ordering::Relaxed);
            out.keyframe(self.session, req);
        }
    }

    fn apply_cursor(&mut self, update: Option<swapdesk_platform::CursorUpdate>) {
        let Some(update) = update else { return };
        if let Some(shape) = &update.shape {
            *self.shared.pointer_shape.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(shape.clone()));
        }
        let Some(r) = self.renderer.as_mut() else { return };
        let local = self.shared.local_pointer.load(Ordering::SeqCst);
        let shown = crate::shared::cursor::shown(&update, local);
        match r.set_cursor(&shown) {
            Ok(()) => self.shared.cursor_drawn.store(self.cursor.has_shape() && shown.visible, Ordering::SeqCst),
            Err(e) => self.shared.error("cursor", e),
        }
    }

    /// Presents a decoded picture and publishes what the pointer and the feedback need.
    fn show(&mut self, picture: swapdesk_platform::DecodedFrame, arrived: Instant) {
        let (Some(decoder), Some(renderer)) = (self.decoder.as_ref(), self.renderer.as_mut()) else { return };
        let decode_us = decoder.last_decode_us();
        let (width, height, frame_id, display) = (picture.width, picture.height, picture.frame_id, picture.display.0);
        match renderer.present(picture, self.scale) {
            Ok(stats) => {
                let lag_us = u32::try_from(arrived.elapsed().as_micros()).unwrap_or(u32::MAX);
                let timing = renderer.timing();
                let event = PresentEvent {
                    frame_id,
                    display,
                    width,
                    height,
                    decode_us,
                    lag_us,
                    presented_at_us: stats.presented_at.0,
                    wait_us: timing.wait_us,
                    blt_us: timing.blt_us,
                    present_us: timing.present_us,
                };
                if let Some(tap) = &self.probe {
                    match renderer.take_probe() {
                        Ok(Some(pixels)) => (tap.on_probe)(&event, &pixels),
                        Ok(None) => {}
                        Err(e) => self.shared.error("probe", e),
                    }
                }
                self.feedback.on_presented(frame_id, decode_us, lag_us);
                self.shared.presented.fetch_add(1, Ordering::Relaxed);
                self.shared.last_frame_id.store(u64::from(frame_id), Ordering::Relaxed);
                let first_or_resized = self.frame != Some((width, height)) || self.display != display;
                self.frame = Some((width, height));
                self.display = display;
                self.publish_map();
                if first_or_resized {
                    let update = self.cursor.refresh(self.frame);
                    self.apply_cursor(update);
                }
                if let Some(obs) = &self.observer {
                    obs(&event);
                }
            }
            Err(e) => {
                self.feedback.on_lost(1);
                self.shared.lost.fetch_add(1, Ordering::Relaxed);
                self.shared.error("present", &e);
                if e.kind == ErrorKind::Unavailable {
                    // A lost device: attach again on the same surface, then wait for a keyframe.
                    self.attach();
                }
            }
        }
    }

    /// One non-blocking look for the picture the decoder owes (a frame whose picture was late, or the last one before a pause).
    fn poll_owed(&mut self) {
        let Some((left, arrived)) = self.owed else { return };
        let Some(decoder) = self.decoder.as_mut() else {
            self.owed = None;
            return;
        };
        match decoder.poll() {
            Ok(Some(picture)) => {
                self.owed = None;
                self.show(picture, arrived);
            }
            Ok(None) => self.owed = if left > 1 { Some((left - 1, arrived)) } else { None },
            Err(e) => {
                self.owed = None;
                decoder.reset();
                self.feedback.on_lost(1);
                self.shared.lost.fetch_add(1, Ordering::Relaxed);
                self.shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                self.shared.error("decode", e);
                self.request_keyframe(true);
            }
        }
    }

    fn frame_in(&mut self, f: Frame) {
        self.session = f.session_id;
        self.display = f.header.display_id;
        let (Some(decoder), true) = (self.decoder.as_mut(), self.renderer.is_some()) else {
            return; // no window yet: the first picture waits for the keyframe the attach asks for
        };
        let result = decoder.decode(&f.header, &f.payload);
        let owes = decoder.owes_picture();
        self.owed = if owes { Some((OWED_POLLS, f.arrived)) } else { None };
        match result {
            Ok(Some(picture)) => self.show(picture, f.arrived),
            Ok(None) => {
                if decoder.waiting_for_keyframe() {
                    self.request_keyframe(false);
                }
            }
            Err(e) => {
                decoder.reset();
                self.feedback.on_lost(1);
                self.shared.lost.fetch_add(1, Ordering::Relaxed);
                self.shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                self.shared.error("decode", e);
                self.request_keyframe(true);
            }
        }
    }
}
