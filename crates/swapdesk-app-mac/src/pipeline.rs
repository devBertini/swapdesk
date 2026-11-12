//! The Mac viewer pipeline: what turns the service's media into a picture, sound and a cursor in the viewer window, and
//! the window's mouse and keyboard into input for the host. The counterpart of `swapdesk-app`'s `viewer`.
//!
//! ```text
//!  ServiceBackend reader thread            decode thread (owns VtH264Decoder, Feedback, cursor tracker)
//!  ViewerSink::video ──► Mailbox ────────► decode (Gpu pictures) ──► Handoff ──► main thread: MacRenderer
//!  ViewerSink::cursor_* ─┘  (bounded; keyframe resync)                          (VideoSlot, layer edits)
//!  ViewerSink::audio ───► AudioPlayer thread (jitter, Opus, MacAudioSink)
//!
//!  main thread: the shell's video view ──► MacInput (ViewerInput) ──┐
//!               CGEventTap thread (MacGrabber, system shortcuts) ───┴► input channel ► input thread ► ViewerOut::input
//!               ObservingBackend: the commands and events the UI already exchanges (session, grants, settings, scale,
//!               grab, audio, view-only) set what the pipeline allows
//! ```
//!
//! Threads: the reader thread only pushes into bounded queues; the decode thread owns the decoder; everything that edits
//! a layer runs on the main thread; the input thread owns the write side of `viewer_input`; the audio thread owns the
//! sink. Control state is learned from what the UI exchanges, as in the Windows app: the `Session` events carry the id
//! and the granted permissions (input needs `Control`), `SetScale`, `SetGrab`, `SetKeyboardMode`, `SetAudio`,
//! `SetViewOnly` and `SetSettings` are seen as they are sent, and `DevicesChanged` tells the host's OS.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use swapdesk_app::shared::audio::AudioPlayer;
use swapdesk_app::shared::mailbox::{Frame, Mailbox};
use swapdesk_app::shared::out::{INPUT_QUEUE, OutSlot, ViewerOut, input_loop};
use swapdesk_audio_mac::{MacAudioSink, SinkConfig};
use swapdesk_input_mac::{InputConfig, MacGrabber, MacInput, detect_keyboard_kind};
use swapdesk_keys::KbKind;
use swapdesk_platform::{CursorShape, InputGrabber, ScaleMode};
use swapdesk_proto::{CursorPosition, InputEvent, VideoHeader};
use swapdesk_ui::service_backend::ViewerSink;
use swapdesk_ui::{Command, DeviceView, Os, Permission, Reply, Role, Scale, SessionState, Ticket, UiBackend, UiEvent};

use crate::decode::{self, Shared};
use crate::handoff::Handoff;
use crate::layout_state::LayoutState;
use crate::main_side::{MainFrame, PresentObserver};
use crate::prefs::{KeyPrefs, input_config};

/// How the pipeline is built.
pub struct ViewerOptions {
    /// Called on the main thread for every picture that reached the screen (tests, traces).
    pub on_present: Option<PresentObserver>,
    /// Start the audio thread (the real Core Audio sink). Tests turn it off.
    pub audio: bool,
    /// Honour "capturar atalhos do sistema" with the event tap. Tests turn it off so nothing intercepts the real keyboard.
    pub system_keys: bool,
    /// How the decode thread asks the main thread to drain the hand-over. The app's is a block on the main dispatch
    /// queue ([`crate::slot::wake_main`]); tests drain by hand.
    pub wake: Box<dyn Fn() + Send + Sync>,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        Self { on_present: None, audio: true, system_keys: true, wake: Box::new(crate::slot::wake_main) }
    }
}

/// Counters of the pipeline since it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PipelineStats {
    pub presented: u64,
    pub decoded: u64,
    pub last_frame_id: Option<u32>,
    pub lost: u64,
    pub decode_errors: u64,
    pub keyframe_requests: u64,
    pub attached: bool,
}

struct Session {
    id: u64,
    device_id: String,
    can_control: bool,
}

/// State owned by the UI thread (hooks and observed commands and events).
struct Ui {
    session: Option<Session>,
    view_only: bool,
    grab_on: bool,
    prefs: KeyPrefs,
    scale: ScaleMode,
    os_by_device: HashMap<String, Os>,
    display: u16,
    grabber: MacGrabber,
    /// The grabber is started: the view's keys and the tap feed the input channel.
    grabbing: bool,
}

pub struct MacViewer {
    pub(crate) mailbox: Arc<Mailbox>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) handoff: Arc<Handoff<MainFrame>>,
    pub(crate) layout: Arc<LayoutState>,
    pub(crate) observer: Option<PresentObserver>,
    out: OutSlot,
    audio: Option<AudioPlayer>,
    audio_on: AtomicBool,
    input: MacInput,
    input_tx: SyncSender<InputEvent>,
    /// The session input goes to; 0 when input is not allowed (the input thread drops what it holds).
    input_session: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    keyboard: KbKind,
    system_keys: bool,
    ui: Mutex<Ui>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn scale_mode(s: Scale) -> ScaleMode {
    match s {
        Scale::Fit => ScaleMode::Fit,
        Scale::OneToOne => ScaleMode::OneToOne,
        Scale::Scroll => ScaleMode::Scroll { x: 0, y: 0 },
    }
}

impl MacViewer {
    pub fn new(options: ViewerOptions) -> std::io::Result<Arc<Self>> {
        let ViewerOptions { on_present, audio, system_keys, wake } = options;
        let mailbox = Arc::new(Mailbox::new());
        let shared = Arc::new(Shared::default());
        let out: OutSlot = Arc::new(Mutex::new(None));
        let handoff = Arc::new(Handoff::new(wake));
        let layout = Arc::new(LayoutState::default());
        let input_session = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = sync_channel(INPUT_QUEUE);
        let keyboard = detect_keyboard_kind();
        let prefs = KeyPrefs::default();
        let input = MacInput::new(input_config(&prefs, 0, keyboard));
        let map_layout = Arc::clone(&layout);
        input.set_pointer_map(Some(Arc::new(move |x, y| map_layout.map(x, y))));
        let audio = audio.then(|| {
            AudioPlayer::start_with(Arc::clone(&shared.audio_underruns), || MacAudioSink::new(SinkConfig::default()))
        });
        let this = Arc::new(Self {
            mailbox: Arc::clone(&mailbox),
            shared: Arc::clone(&shared),
            handoff: Arc::clone(&handoff),
            layout,
            observer: on_present,
            out: Arc::clone(&out),
            audio,
            audio_on: AtomicBool::new(true),
            input: input.clone(),
            input_tx,
            input_session: Arc::clone(&input_session),
            stop: Arc::clone(&stop),
            keyboard,
            system_keys,
            ui: Mutex::new(Ui {
                session: None,
                view_only: false,
                grab_on: true,
                prefs,
                scale: ScaleMode::Fit,
                os_by_device: HashMap::new(),
                display: 0,
                grabber: MacGrabber::new(input),
                grabbing: false,
            }),
            threads: Mutex::new(Vec::new()),
        });
        // The decode thread tells the pipeline when the pictures come from another display (the pointer addresses it).
        let weak = Arc::downgrade(&this);
        let on_display: decode::DisplayWatch = Arc::new(move |d| {
            if let Some(p) = weak.upgrade() {
                p.display_changed(d);
            }
        });
        let decode = decode::spawn(mailbox, Arc::clone(&out), shared, handoff, on_display)?;
        let input_thread = std::thread::Builder::new()
            .name("swapdesk-viewer-input".into())
            .spawn(move || input_loop(&input_rx, &out, &input_session, &stop))?;
        lock(&this.threads).extend([decode, input_thread]);
        Ok(this)
    }

    /// Where feedback and input go; set once the backend exists (the backend needs this pipeline as its sink first).
    pub fn set_out(&self, out: Arc<dyn ViewerOut>) {
        *lock(&self.out) = Some(out);
    }

    /// The viewer's input, to hand to the shell as its `ViewerInput` (a clone of the same input the grabber feeds).
    pub fn input(&self) -> MacInput {
        self.input.clone()
    }

    pub fn layout(&self) -> Arc<LayoutState> {
        Arc::clone(&self.layout)
    }

    pub fn stats(&self) -> PipelineStats {
        let s = &self.shared;
        PipelineStats {
            presented: s.presented.load(Ordering::Relaxed),
            decoded: s.decoded.load(Ordering::Relaxed),
            last_frame_id: (s.presented.load(Ordering::Relaxed) > 0)
                .then(|| s.last_frame_id.load(Ordering::Relaxed) as u32),
            lost: s.lost.load(Ordering::Relaxed),
            decode_errors: s.decode_errors.load(Ordering::Relaxed),
            keyframe_requests: s.keyframe_requests.load(Ordering::Relaxed),
            attached: s.attached.load(Ordering::SeqCst),
        }
    }

    /// The last renderer or decoder failure, for diagnostics.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.last_error).clone()
    }

    /// Development: the last pointer shape the video view took (see `Shared::cursor_note`).
    pub fn cursor_note(&self) -> Option<String> {
        lock(&self.shared.cursor_note).clone()
    }

    /// Development: where the renderer's layers are (see `Shared::geometry_note`).
    pub fn geometry_note(&self) -> Option<String> {
        lock(&self.shared.geometry_note).clone()
    }

    /// Stops every thread of the pipeline. Safe to call twice.
    pub fn shutdown(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut ui = lock(&self.ui);
            if ui.grabbing {
                ui.grabber.stop();
                ui.grabbing = false;
            }
        }
        self.mailbox.close();
        for t in std::mem::take(&mut *lock(&self.threads)) {
            let _ = t.join();
        }
        *lock(&self.out) = None;
    }

    // ----- input plumbing -----

    fn can_input(ui: &Ui) -> bool {
        ui.session.as_ref().is_some_and(|s| s.can_control) && !ui.view_only
    }

    fn current_config(&self, ui: &Ui) -> InputConfig {
        input_config(&ui.prefs, ui.display, self.keyboard)
    }

    fn apply_config(&self, ui: &Ui) {
        self.input.set_config(self.current_config(ui));
    }

    fn display_changed(&self, display: u16) {
        let mut ui = lock(&self.ui);
        if ui.display != display {
            ui.display = display;
            self.apply_config(&ui);
        }
    }

    /// Re-evaluates what depends on the session, the grants and the switches: whether the viewer's keys and pointer feed
    /// the input channel, and whether the event tap takes the system shortcuts.
    fn refresh_input(&self, ui: &mut Ui) {
        let allowed = Self::can_input(ui);
        let id = ui.session.as_ref().map_or(0, |s| s.id);
        if self.shared.local_pointer.swap(allowed, Ordering::SeqCst) != allowed {
            self.handoff.poke();
        }
        if allowed {
            self.input_session.store(id, Ordering::SeqCst);
            if !ui.grabbing && ui.grabber.start(self.input_tx.clone()).is_ok() {
                ui.grabbing = true;
            }
            if ui.grabbing && self.system_keys {
                // A missing TCC grant is reported through `MacInput::grab_status`; nothing prompts from here.
                let _ = ui.grabber.set_grab_system_keys(ui.grab_on);
            }
        } else {
            if ui.grabbing {
                // Whatever the host still holds is let go while the session id is still the right one: sent here and
                // now, not through the channel, which the input thread stops serving once the id is 0.
                if id != 0
                    && let Some(out) = lock(&self.out).clone()
                {
                    out.input(id, vec![InputEvent::ReleaseAll]);
                }
                ui.grabber.stop();
                ui.grabbing = false;
            }
            self.input_session.store(0, Ordering::SeqCst);
        }
    }

    fn set_session(&self, ui: &mut Ui, new: Option<(u64, String, bool)>) {
        let old_id = ui.session.as_ref().map(|s| s.id);
        match new {
            Some((id, device_id, can_control)) => {
                if old_id != Some(id) {
                    // A new session starts with the switches the UI starts it with.
                    ui.grab_on = true;
                    ui.view_only = false;
                    ui.scale = ScaleMode::Fit;
                    self.audio_on.store(true, Ordering::Relaxed);
                    crate::slot::with_main(|m| m.set_scale(ScaleMode::Fit));
                }
                ui.prefs.host_os = ui.os_by_device.get(&device_id).copied().unwrap_or(Os::Windows);
                ui.session = Some(Session { id, device_id, can_control });
            }
            None => ui.session = None,
        }
        self.apply_config(ui);
        self.refresh_input(ui);
    }

    // ----- what the UI exchanges with the backend (main thread) -----

    /// Called for every command the UI sends, before it goes on.
    /// The per-user keyboard settings read from `ui.bin` at start (C5.1), before any session.
    pub fn set_user_prefs(&self, mode: swapdesk_ui::KeyboardMode, swap_ctrl_cmd: bool) {
        let mut ui = lock(&self.ui);
        ui.prefs.mode = mode;
        ui.prefs.swap_ctrl_cmd = swap_ctrl_cmd;
        self.apply_config(&ui);
    }

    pub fn observe_command(&self, command: &Command) {
        let mut ui = lock(&self.ui);
        match command {
            Command::SetScale { scale, .. } => {
                let mode = scale_mode(*scale);
                ui.scale = mode;
                crate::slot::with_main(|m| m.set_scale(mode));
            }
            Command::SetGrab { on, .. } => {
                ui.grab_on = *on;
                self.refresh_input(&mut ui);
            }
            Command::SetKeyboardMode { mode, .. } => {
                ui.prefs.mode = *mode;
                self.apply_config(&ui);
            }
            Command::SetAudio { on, .. } => self.audio_on.store(*on, Ordering::Relaxed),
            Command::SetViewOnly { on, .. } => {
                ui.view_only = *on;
                self.refresh_input(&mut ui);
            }
            Command::SetSettings { settings } => {
                ui.prefs.mode = settings.keyboard_mode;
                ui.prefs.swap_ctrl_cmd = settings.swap_ctrl_cmd;
                self.apply_config(&ui);
            }
            _ => {}
        }
    }

    /// Called for every event the UI drains, before it is applied.
    pub fn observe_event(&self, event: &UiEvent) {
        let mut ui = lock(&self.ui);
        match event {
            UiEvent::Session { session_id, device_id, state } => {
                let same = ui.session.as_ref().is_some_and(|s| s.id == *session_id);
                match state {
                    SessionState::Active { role: Role::Viewer, granted, .. } => {
                        self.set_session(
                            &mut ui,
                            Some((*session_id, device_id.clone(), granted.has(Permission::Control))),
                        );
                    }
                    // `Connecting` is a connect entry waiting for the host, not a session: it has no media to show.
                    SessionState::Paused { .. } | SessionState::Reconnecting { .. } => {
                        if !same {
                            self.set_session(&mut ui, Some((*session_id, device_id.clone(), false)));
                        }
                    }
                    SessionState::Ended { .. } if same => self.set_session(&mut ui, None),
                    _ => {}
                }
            }
            UiEvent::DevicesChanged(list) => self.learn_devices(&mut ui, list),
            // The keyboard mode and the Ctrl/Cmd swap are per user (C5.1): the service's copy is a default, so they come
            // from `set_user_prefs` at start and from the UI's own `SetSettings`, never from these events.
            UiEvent::Reply { result: Ok(Reply::Devices(list)), .. } => self.learn_devices(&mut ui, list),
            UiEvent::Reply { result: Ok(Reply::Device(d)), .. } => self.learn_devices(&mut ui, std::slice::from_ref(d)),
            _ => {}
        }
    }

    fn learn_devices(&self, ui: &mut Ui, list: &[DeviceView]) {
        for d in list {
            ui.os_by_device.insert(d.id.clone(), d.os);
        }
        if let Some(os) = ui.session.as_ref().and_then(|s| ui.os_by_device.get(&s.device_id)).copied()
            && ui.prefs.host_os != os
        {
            ui.prefs.host_os = os;
            self.apply_config(ui);
        }
    }

    /// The session id input goes to (0: input is off). For tests and the self-test.
    pub fn input_session(&self) -> u64 {
        self.input_session.load(Ordering::SeqCst)
    }

    /// Whether the viewer's keys and pointer feed the input channel now.
    pub fn grabbing(&self) -> bool {
        lock(&self.ui).grabbing
    }

    pub fn audio_underruns(&self) -> u32 {
        self.audio.as_ref().map_or(0, AudioPlayer::underruns)
    }
}

impl Drop for MacViewer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ----- the service's media (reader thread of the pipe) -----

impl ViewerSink for MacViewer {
    fn video(&self, session_id: u64, header: VideoHeader, payload: Vec<u8>) {
        self.mailbox.push_frame(Frame { session_id, header, payload, arrived: std::time::Instant::now() });
    }

    fn audio(&self, _session_id: u64, datagram: Vec<u8>) {
        if self.audio_on.load(Ordering::Relaxed)
            && let Some(a) = &self.audio
        {
            a.push(datagram);
        }
    }

    fn cursor_pos(&self, session_id: u64, pos: CursorPosition) {
        self.mailbox.set_cursor(session_id, pos);
    }

    fn cursor_shape(&self, _session_id: u64, shape: swapdesk_proto::CursorShape) {
        let h = shape.header;
        self.mailbox.push_shape(CursorShape {
            id: h.shape_id,
            width: h.width,
            height: h.height,
            hot_x: h.hot_x,
            hot_y: h.hot_y,
            rgba: shape.rgba,
        });
    }

    /// The drawer's terminal: its output goes to the shared panel's router, which waits for room in the panel's
    /// bounded channel (back-pressure up the pipe to the host's shell) and wakes the UI thread.
    fn term_output(&self, term_id: u32, bytes: Vec<u8>) {
        swapdesk_shell_mac::terminal_output(term_id, bytes);
    }
}

/// The UI's backend with the pipeline listening in: commands and events pass through unchanged.
pub struct ObservingBackend {
    inner: Arc<dyn UiBackend>,
    pipeline: Arc<MacViewer>,
}

impl ObservingBackend {
    pub fn new(inner: Arc<dyn UiBackend>, pipeline: Arc<MacViewer>) -> Self {
        Self { inner, pipeline }
    }
}

impl UiBackend for ObservingBackend {
    fn send(&self, command: Command) -> Ticket {
        self.pipeline.observe_command(&command);
        self.inner.send(command)
    }

    fn drain(&self) -> Vec<UiEvent> {
        let events = self.inner.drain();
        for e in &events {
            self.pipeline.observe_event(e);
        }
        events
    }
}
