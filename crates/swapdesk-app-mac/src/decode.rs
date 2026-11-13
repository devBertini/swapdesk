//! The decode thread: the only owner of the `VtH264Decoder` (nothing in it is `Send`, C3) and of the `Feedback` and the
//! cursor tracker. The Mac counterpart of `swapdesk-app`'s `viewer::decode`, with one difference: the renderer belongs
//! to the main thread (layer edits), so a decoded picture (a `CVPixelBuffer`, `Pixels::Gpu`) and the cursor updates leave
//! through the [`Handoff`] instead of being presented here.
//!
//! Frames come from the shared `Mailbox`; the thread decodes each, queues the picture for the main thread and keeps the
//! feedback of [`Feedback`]: a keyframe request when the decoder fails, loses the thread of frame ids or has to wait, and
//! the once-a-second report. The decoder exists between `Cmd::Attach` (the viewer window's video view is up) and
//! `Cmd::Detach`; pictures that arrive before are dropped (the attach asks for a keyframe).

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_app::shared::cursor::CursorTracker;
use swapdesk_app::shared::feedback::Feedback;
use swapdesk_app::shared::mailbox::{Cmd, Frame, Item, Mailbox};
use swapdesk_app::shared::out::OutSlot;
use swapdesk_codec_mac::{DecoderOptions, DecoderOutput, VtH264Decoder};
use swapdesk_platform::{CursorUpdate, VideoDecoder};

use crate::handoff::{CursorItem, Handoff};
use crate::main_side::MainFrame;

/// How long the thread sleeps with nothing to do (the report and the retries are checked at least this often).
const IDLE: Duration = Duration::from_millis(200);

/// What the decode thread and the rest of the pipeline share (counters, flags, the last error).
#[derive(Default)]
pub struct Shared {
    /// Pictures that reached the screen (counted by the main thread).
    pub presented: AtomicU64,
    pub last_frame_id: AtomicU64,
    /// Pictures decoded.
    pub decoded: AtomicU64,
    pub lost: AtomicU64,
    pub keyframe_requests: AtomicU64,
    pub decode_errors: AtomicU64,
    /// The decoder exists (the viewer window is up).
    pub attached: AtomicBool,
    /// The remote cursor is drawn: the local pointer may be hidden.
    pub cursor_drawn: AtomicBool,
    /// The viewer controls the host: the local pointer is the one shown and the remote cursor layer stays hidden (the
    /// drawn arrow would trail it by a round trip). Written by the pipeline, read on the main thread.
    pub local_pointer: AtomicBool,
    /// The display the last picture came from.
    pub display: AtomicU16,
    pub last_error: Mutex<Option<String>>,
    /// Development: the last pointer shape the video view took (host size and hot spot, the view cursor in points).
    pub cursor_note: Mutex<Option<String>>,
    /// Development: where the renderer's layers are (`MacRenderer::geometry_note`), after the last picture.
    pub geometry_note: Mutex<Option<String>>,
    pub audio_underruns: Arc<AtomicU32>,
}

impl Shared {
    pub fn error(&self, what: &str, e: impl std::fmt::Display) {
        *self.last_error.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("{what}: {e}"));
    }
}

/// Told on the decode thread when the display of the pictures changes.
pub type DisplayWatch = Arc<dyn Fn(u16) + Send + Sync>;

struct Worker {
    mailbox: Arc<Mailbox>,
    out: OutSlot,
    shared: Arc<Shared>,
    handoff: Arc<Handoff<MainFrame>>,
    on_display: DisplayWatch,
    decoder: Option<VtH264Decoder>,
    frame: Option<(u32, u32)>,
    display: u16,
    session: u64,
    feedback: Feedback,
    cursor: CursorTracker,
}

pub fn spawn(
    mailbox: Arc<Mailbox>,
    out: OutSlot,
    shared: Arc<Shared>,
    handoff: Arc<Handoff<MainFrame>>,
    on_display: DisplayWatch,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("swapdesk-viewer-decode".into()).spawn(move || {
        let mut w = Worker {
            mailbox,
            out,
            shared,
            handoff,
            on_display,
            decoder: None,
            frame: None,
            display: 0,
            session: 0,
            feedback: Feedback::new(Instant::now()),
            cursor: CursorTracker::new(),
        };
        w.run();
    })
}

impl Worker {
    fn out(&self) -> Option<Arc<dyn swapdesk_app::shared::out::ViewerOut>> {
        self.out.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn run(&mut self) {
        loop {
            match self.mailbox.next(IDLE) {
                Item::Cmd(c) => self.command(c),
                Item::Shape(s) => {
                    self.cursor.on_shape(s);
                    let update = self.cursor.refresh(self.frame);
                    self.send_cursor(update);
                }
                Item::Cursor(session, pos) => {
                    self.session = session;
                    let update = self.cursor.on_position(&pos, self.frame);
                    self.send_cursor(update);
                }
                Item::Reset => {
                    if let Some(d) = self.decoder.as_mut() {
                        d.reset();
                    }
                    self.request_keyframe(true);
                }
                Item::Frame(f) => self.frame_in(f),
                Item::Timeout => {
                    // A decoder still waiting for a keyframe asks again after the retry interval.
                    if self.decoder.as_ref().is_some_and(VtH264Decoder::waiting_for_keyframe) {
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
            Cmd::Attach { .. } => {
                self.decoder = Some(VtH264Decoder::new(DecoderOptions { output: DecoderOutput::Gpu }));
                self.frame = None;
                self.shared.attached.store(true, Ordering::SeqCst);
                // The stream started before the window did: the first picture needs a keyframe.
                self.request_keyframe(true);
            }
            // The main thread applies the window's size and the scale mode to the renderer itself.
            Cmd::Resize(_) | Cmd::Scale(_) | Cmd::CursorMode => {}
            Cmd::Detach(ack) => {
                self.detach();
                let _ = ack.send(());
            }
        }
    }

    fn detach(&mut self) {
        self.decoder = None;
        self.frame = None;
        self.shared.attached.store(false, Ordering::SeqCst);
        self.shared.cursor_drawn.store(false, Ordering::SeqCst);
        self.handoff.clear();
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

    fn send_cursor(&mut self, update: Option<CursorUpdate>) {
        let Some(update) = update else { return };
        let drawn = self.cursor.has_shape() && update.visible;
        self.handoff.push_cursor(CursorItem { update, drawn });
    }

    fn frame_in(&mut self, f: Frame) {
        self.session = f.session_id;
        let Some(decoder) = self.decoder.as_mut() else {
            self.display = f.header.display_id;
            return; // no window yet: the first picture waits for the keyframe the attach asks for
        };
        match decoder.decode(&f.header, &f.payload) {
            Ok(Some(picture)) => {
                let decode_us = decoder.last_decode_us();
                let (width, height, frame_id, display) =
                    (picture.width, picture.height, picture.frame_id, picture.display.0);
                self.shared.decoded.fetch_add(1, Ordering::Relaxed);
                let lag_us = u32::try_from(f.arrived.elapsed().as_micros()).unwrap_or(u32::MAX);
                // The picture reaches the screen on the main thread; what the report calls "presented" is "handed over".
                self.feedback.on_presented(frame_id, decode_us, lag_us);
                let first_or_resized = self.frame != Some((width, height)) || self.display != display;
                let display_changed = self.display != display || self.frame.is_none();
                self.frame = Some((width, height));
                self.display = display;
                self.handoff.push_frame(MainFrame::new(picture, decode_us, f.arrived));
                if display_changed {
                    self.shared.display.store(display, Ordering::Relaxed);
                    (self.on_display)(display);
                }
                if first_or_resized {
                    let update = self.cursor.refresh(self.frame);
                    self.send_cursor(update);
                }
            }
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
