//! The viewer pipeline (stage 18g): what turns the service's media into a picture, sound and a cursor in the viewer
//! window, and the window's mouse and keyboard into input for the host.
//!
//! ```text
//!  service pipe reader thread                  decode thread (owns decoder + renderer)
//!  ViewerSink::video ──► Mailbox ───────────►  MfH264Decoder (GPU NV12, shell's D3D11 device)
//!  ViewerSink::cursor_* ─┘   (bounded; keyframe resync)        └► WinRenderer ► the viewer window's video visual
//!  ViewerSink::audio ───► AudioPlayer thread (jitter, Opus, WASAPI)         └► ViewerOut::keyframe / ::report
//!
//!  UI thread: ViewerHost (window size, focus, mouse, keys) ──► PointerRouter / WinGrabber / WindowKeys
//!             ObservingBackend (commands and events the UI already exchanges: session, grants, settings,
//!             scale, grab, audio, view-only) ──► input thread (batches, coalesced moves) ──► ViewerOut::input
//! ```
//!
//! Threads (C6): the UI thread never blocks (every hook only queues; `closed` waits at most [`DETACH_WAIT`] for the
//! renderer to let go of the window's visual, which must happen before the window's tree goes); the pipe's reader
//! thread only pushes into bounded queues; the decode thread is the only one that touches the decoder and the
//! renderer; the input thread owns the write side of `viewer_input`; the audio thread owns the sink.
//!
//! Control state is learned from what the UI already exchanges, through [`ObservingBackend`], not from a second
//! channel: the `Session` events carry the id and the granted permissions (input needs `Control`), `SetScale`,
//! `SetGrab`, `SetKeyboardMode`, `SetAudio`, `SetViewOnly` and `SetSettings` are seen as they are sent, and
//! `DevicesChanged` tells the host's OS (for the keyboard layout rule and the Ctrl/Cmd swap, see [`keys`]). A new
//! session resets the per-session switches (fit on, grab on, audio on, view-only off).
//!
//! The viewer's own shortcuts (D30) are recognised on both keyboard paths (`keys`); the pipeline queues them and wakes
//! the UI thread, whose drain asks [`ViewerHost::take_shortcuts`]: "release the keyboard" is the pipeline's own (the
//! window's keys and the hook stop until the next click on the picture or the next focus), the others go to the shell
//! for the state to run. `SetReleaseKey` (a UI-local command) sets which chord releases the keyboard.

pub mod decode;
pub mod keys;
pub mod pointer;

// The OS-free pieces live in [`crate::shared`] (the macOS app uses them too); the old paths stay valid.
pub use crate::shared::out::{INPUT_QUEUE, ViewerOut};
pub use crate::shared::{audio, cursor, feedback, mailbox};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_input_win::WinGrabber;
use swapdesk_platform::{CursorShape, InputGrabber, NativeSurface, Rect, ScaleMode};
use swapdesk_proto::{CursorPosition, InputEvent, VideoHeader};
use swapdesk_shell_win::{KeyMsg, MouseKind, MouseMsg, ViewerHost};
use swapdesk_ui::service_backend::ViewerSink;
use swapdesk_ui::{
    Command, DeviceView, Os, Permission, Reply, Role, Scale, SessionState, Ticket, UiBackend, UiEvent, UiWaker,
    ViewerAction,
};

use crate::shared::out::input_loop;
use audio::AudioPlayer;
use decode::Shared;
pub use decode::{PresentEvent, PresentObserver, ProbeObserver, ProbeTap};
use keys::{KeyConfig, WindowKeys, action_of};
use mailbox::{Cmd, Frame, Mailbox};
use pointer::PointerRouter;

/// How long `ViewerHost::closed` waits for the decode thread to detach the renderer.
pub const DETACH_WAIT: Duration = Duration::from_millis(500);
pub struct ViewerOptions {
    /// Called on the decode thread for every picture that reached the screen (tests, traces).
    pub on_present: Option<PresentObserver>,
    /// Start the audio thread (the real WASAPI sink). Tests turn it off.
    pub audio: bool,
    /// Measurements only: pixels of every presented picture (see [`ProbeTap`]). `None` in the application.
    pub probe: Option<ProbeTap>,
    /// Tests only: every present takes at least this long, as on a desktop that holds back the pictures of a
    /// window that is not in front. `None` in the application.
    pub present_delay: Option<Duration>,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        Self { on_present: None, audio: true, probe: None, present_delay: None }
    }
}

/// Counters of the pipeline since it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PipelineStats {
    pub presented: u64,
    pub last_frame_id: Option<u32>,
    /// Pictures decoded and not shown because a newer frame was already waiting.
    pub skipped: u64,
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
    keys: KeyConfig,
    scale: ScaleMode,
    os_by_device: HashMap<String, Os>,
    focused: bool,
    pointer: PointerRouter,
    window_keys: WindowKeys,
    grabber: Option<WinGrabber>,
    /// "Release the keyboard" fired: the keys go to this PC until a click on the picture or the next focus.
    released: bool,
}

/// The viewer shortcuts the keyboard paths recognised and the UI thread has not taken yet, and who to wake for them.
/// Its own lock, never the `ui` one: the hook thread pushes while the UI thread may hold `ui` and wait on the hook.
#[derive(Default)]
struct Shortcuts {
    queue: Mutex<VecDeque<ViewerAction>>,
    waker: Mutex<Option<Arc<dyn UiWaker>>>,
}

impl Shortcuts {
    fn push(&self, action: ViewerAction) {
        lock(&self.queue).push_back(action);
        if let Some(w) = lock(&self.waker).as_ref() {
            w.wake();
        }
    }
}

pub struct ViewerPipeline {
    mailbox: Arc<Mailbox>,
    shared: Arc<Shared>,
    out: Arc<Mutex<Option<Arc<dyn ViewerOut>>>>,
    audio: Option<AudioPlayer>,
    audio_on: AtomicBool,
    input_tx: SyncSender<InputEvent>,
    /// The session input goes to; 0 when input is not allowed (the input thread drops what it holds).
    input_session: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    ui: Mutex<Ui>,
    shortcuts: Arc<Shortcuts>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn scale_mode(s: Scale) -> ScaleMode {
    match s {
        Scale::Fit => ScaleMode::Fit,
        Scale::OneToOne => ScaleMode::OneToOne,
        Scale::Scroll => ScaleMode::Scroll { x: 0, y: 0 },
    }
}

impl ViewerPipeline {
    pub fn new(options: ViewerOptions) -> std::io::Result<Arc<Self>> {
        let mailbox = Arc::new(Mailbox::new());
        let shared = Arc::new(Shared::default());
        let out: Arc<Mutex<Option<Arc<dyn ViewerOut>>>> = Arc::new(Mutex::new(None));
        let input_session = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = sync_channel(INPUT_QUEUE);
        let audio = options.audio.then(|| AudioPlayer::start(Arc::clone(&shared.audio_underruns)));
        let decode = decode::spawn(
            Arc::clone(&mailbox),
            Arc::clone(&out),
            Arc::clone(&shared),
            options.on_present,
            options.probe,
            options.present_delay,
        )?;
        let (o, s, st) = (Arc::clone(&out), Arc::clone(&input_session), Arc::clone(&stop));
        let input = std::thread::Builder::new()
            .name("swapdesk-viewer-input".into())
            .spawn(move || input_loop(&input_rx, &o, &s, &st))?;
        Ok(Arc::new(Self {
            mailbox,
            shared,
            out,
            audio,
            audio_on: AtomicBool::new(true),
            input_tx,
            input_session,
            stop,
            ui: Mutex::new(Ui {
                session: None,
                view_only: false,
                grab_on: true,
                keys: KeyConfig::default(),
                scale: ScaleMode::Fit,
                os_by_device: HashMap::new(),
                focused: false,
                pointer: PointerRouter::new(),
                window_keys: WindowKeys::new(KeyConfig::default()),
                grabber: None,
                released: false,
            }),
            shortcuts: Arc::new(Shortcuts::default()),
            threads: Mutex::new(vec![decode, input]),
        }))
    }

    /// Where feedback and input go; set once the backend exists (the backend needs this pipeline as its sink first).
    pub fn set_out(&self, out: Arc<dyn ViewerOut>) {
        *lock(&self.out) = Some(out);
    }

    /// The per-user keyboard settings read from `ui.bin` at start (C5.1), before any session.
    pub fn set_user_prefs(
        &self,
        mode: swapdesk_ui::KeyboardMode,
        swap_ctrl_cmd: bool,
        release: swapdesk_ui::ReleaseKey,
    ) {
        let mut ui = lock(&self.ui);
        ui.keys.mode = mode;
        ui.keys.swap_ctrl_cmd = swap_ctrl_cmd;
        ui.keys.release = release;
        self.apply_key_config(&mut ui);
    }

    /// Who to wake when a viewer shortcut was recognised on the hook thread (the shell's waker).
    pub fn set_waker(&self, waker: Arc<dyn UiWaker>) {
        *lock(&self.shortcuts.waker) = Some(waker);
    }

    pub fn stats(&self) -> PipelineStats {
        let s = &self.shared;
        PipelineStats {
            presented: s.presented.load(Ordering::Relaxed),
            last_frame_id: (s.presented.load(Ordering::Relaxed) > 0)
                .then(|| s.last_frame_id.load(Ordering::Relaxed) as u32),
            skipped: s.skipped.load(Ordering::Relaxed),
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

    /// Stops every thread of the pipeline after detaching the renderer. Safe to call twice.
    pub fn shutdown(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut ui = lock(&self.ui);
            if let Some(mut g) = ui.grabber.take() {
                g.stop();
            }
        }
        self.mailbox.close();
        for t in std::mem::take(&mut *lock(&self.threads)) {
            let _ = t.join();
        }
        *lock(&self.out) = None;
    }

    // ----- input plumbing (UI thread) -----

    fn send_events(&self, events: Vec<InputEvent>) {
        for e in events {
            // A full queue means the writer is behind: the move or key is dropped, as a slow link would.
            let _ = self.input_tx.try_send(e);
        }
    }

    fn can_input(ui: &Ui) -> bool {
        ui.session.as_ref().is_some_and(|s| s.can_control) && !ui.view_only
    }

    /// Re-evaluates what depends on the session, the grants, the switches and the focus.
    fn refresh_input(&self, ui: &mut Ui) {
        let allowed = Self::can_input(ui);
        let id = ui.session.as_ref().map_or(0, |s| s.id);
        if self.shared.local_pointer.swap(allowed, Ordering::SeqCst) != allowed {
            self.mailbox.push_cmd(Cmd::CursorMode);
        }
        if !allowed {
            // Whatever the host still holds is let go while the session id is still the right one.
            let mut released = ui.pointer.release_all();
            released.extend(ui.window_keys.reset());
            self.send_events(released);
        }
        self.input_session.store(if allowed { id } else { 0 }, Ordering::SeqCst);
        self.update_grabber(ui);
    }

    fn update_grabber(&self, ui: &mut Ui) {
        let want = ui.focused && ui.grab_on && Self::can_input(ui) && !ui.released;
        match (want, ui.grabber.is_some()) {
            (true, false) => {
                let mut g = WinGrabber::new(ui.keys.grab_config());
                let shortcuts = Arc::clone(&self.shortcuts);
                g.set_shortcut_handler(Arc::new(move |a| shortcuts.push(action_of(a))));
                let _ = g.set_grab_system_keys(true);
                if g.start(self.input_tx.clone()).is_ok() {
                    ui.window_keys.reset();
                    ui.grabber = Some(g);
                }
            }
            (false, true) => {
                if let Some(mut g) = ui.grabber.take() {
                    g.stop(); // sends ReleaseAll
                }
            }
            (true, true) => {
                if let Some(g) = ui.grabber.as_mut() {
                    g.set_config(ui.keys.grab_config());
                }
            }
            (false, false) => {}
        }
    }

    fn apply_key_config(&self, ui: &mut Ui) {
        ui.window_keys.set_config(ui.keys);
        if let Some(g) = ui.grabber.as_mut() {
            g.set_config(ui.keys.grab_config());
        }
    }

    fn set_session(&self, ui: &mut Ui, new: Option<(u64, String, bool)>) {
        let old_id = ui.session.as_ref().map(|s| s.id);
        match new {
            Some((id, device_id, can_control)) => {
                if old_id != Some(id) {
                    // A new session starts with the switches the UI starts it with.
                    ui.grab_on = true;
                    ui.released = false;
                    ui.view_only = false;
                    ui.scale = ScaleMode::Fit;
                    self.audio_on.store(true, Ordering::Relaxed);
                    self.mailbox.push_cmd(Cmd::Scale(ScaleMode::Fit));
                }
                ui.keys.host_os = ui.os_by_device.get(&device_id).copied().unwrap_or(Os::Windows);
                ui.session = Some(Session { id, device_id, can_control });
            }
            None => ui.session = None,
        }
        self.apply_key_config(ui);
        self.refresh_input(ui);
    }

    // ----- what the UI exchanges with the backend (UI thread) -----

    /// Called for every command the UI sends, before it goes on.
    pub fn observe_command(&self, command: &Command) {
        let mut ui = lock(&self.ui);
        match command {
            Command::SetScale { scale, .. } => {
                let mode = scale_mode(*scale);
                ui.scale = mode;
                self.mailbox.push_cmd(Cmd::Scale(mode));
            }
            Command::SetGrab { on, .. } => {
                ui.grab_on = *on;
                self.update_grabber(&mut ui);
            }
            Command::SetReleaseKey { key } => {
                ui.keys.release = *key;
                self.apply_key_config(&mut ui);
            }
            Command::SetKeyboardMode { mode, .. } => {
                ui.keys.mode = *mode;
                self.apply_key_config(&mut ui);
            }
            Command::SetAudio { on, .. } => self.audio_on.store(*on, Ordering::Relaxed),
            Command::SetViewOnly { on, .. } => {
                ui.view_only = *on;
                self.refresh_input(&mut ui);
            }
            Command::SetSettings { settings } => {
                ui.keys.mode = settings.keyboard_mode;
                ui.keys.swap_ctrl_cmd = settings.swap_ctrl_cmd;
                self.apply_key_config(&mut ui);
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
            && ui.keys.host_os != os
        {
            ui.keys.host_os = os;
            self.apply_key_config(ui);
        }
    }
}

impl Drop for ViewerPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ----- the service's media (reader thread of the pipe) -----

impl ViewerSink for ViewerPipeline {
    fn video(&self, session_id: u64, header: VideoHeader, payload: Vec<u8>) {
        self.mailbox.push_frame(Frame { session_id, header, payload, arrived: Instant::now() });
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

    /// The drawer's terminal: its output goes to the shell's router, which waits for room in the panel's bounded channel
    /// (back-pressure up the pipe to the host's shell) and wakes the UI thread.
    fn term_output(&self, term_id: u32, bytes: Vec<u8>) {
        swapdesk_shell_win::terminal_output(term_id, bytes);
    }
}

// ----- the window (UI thread) -----

impl ViewerHost for ViewerPipeline {
    fn opened(&self, surface: NativeSurface, area: Rect, _scale: f32) {
        let scale = lock(&self.ui).scale;
        self.mailbox.push_cmd(Cmd::Attach { surface, area, scale });
    }

    fn resized(&self, area: Rect, _scale: f32) {
        self.mailbox.push_cmd(Cmd::Resize(area));
    }

    fn closed(&self) {
        let (tx, rx) = channel();
        self.mailbox.push_cmd(Cmd::Detach(tx));
        let _ = rx.recv_timeout(DETACH_WAIT);
        let mut ui = lock(&self.ui);
        ui.focused = false;
        self.refresh_input(&mut ui);
    }

    fn focus(&self, focused: bool) {
        let mut ui = lock(&self.ui);
        ui.focused = focused;
        if focused {
            ui.released = false; // coming back to the window captures the keyboard again
        } else {
            // The host lets go of every key and button the viewer still held.
            let mut released = ui.pointer.release_all();
            released.extend(ui.window_keys.reset());
            self.send_events(released);
        }
        self.update_grabber(&mut ui);
    }

    fn mouse(&self, msg: MouseMsg) -> bool {
        let mut ui = lock(&self.ui);
        if !Self::can_input(&ui) {
            return false;
        }
        if ui.released && matches!(msg.kind, MouseKind::Button { down: true, .. }) && msg.inside {
            // A click on the picture takes the keyboard back.
            ui.released = false;
            self.update_grabber(&mut ui);
        }
        let map = *lock(&self.shared.map);
        let events = ui.pointer.mouse(&msg, map.as_ref());
        drop(ui);
        self.send_events(events);
        true
    }

    fn captures_keyboard(&self) -> bool {
        let ui = lock(&self.ui);
        Self::can_input(&ui) && !ui.released
    }

    fn key(&self, msg: &KeyMsg) -> bool {
        let mut ui = lock(&self.ui);
        if !Self::can_input(&ui) || ui.released {
            return false;
        }
        if msg.vk == 0 {
            // The text an IME composed (its key-downs came as `VK_PROCESSKEY` and were dropped): a text record. The
            // hook never sees it, so this is the same with the system-wide capture on or off.
            let text = WindowKeys::text(msg);
            drop(ui);
            self.send_events(text.into_iter().collect());
            return true;
        }
        if ui.grabber.is_some() {
            return true; // the system-wide hook sees this key (and the shortcuts); the window's copy is dropped
        }
        let (events, shortcut) = ui.window_keys.process(msg);
        drop(ui);
        self.send_events(events);
        if let Some(action) = shortcut {
            self.shortcuts.push(action);
        }
        true
    }

    fn take_shortcuts(&self) -> Vec<ViewerAction> {
        let taken: Vec<ViewerAction> = lock(&self.shortcuts.queue).drain(..).collect();
        let mut out = Vec::new();
        for action in taken {
            if action == ViewerAction::ReleaseKeyboard {
                let mut ui = lock(&self.ui);
                if Self::can_input(&ui) && !ui.released {
                    ui.released = true;
                    // The host lets go of every key; the window's keys and the hook stop until the next click or focus.
                    let mut released = ui.window_keys.reset();
                    released.extend(ui.pointer.release_all());
                    self.send_events(released);
                    self.update_grabber(&mut ui);
                }
            } else {
                out.push(action);
            }
        }
        out
    }

    fn pointer_shape(&self) -> Option<Arc<swapdesk_platform::CursorShape>> {
        if !Self::can_input(&lock(&self.ui)) {
            return None;
        }
        self.shared.pointer_shape.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// The UI's backend with the pipeline listening in: commands and events pass through unchanged.
pub struct ObservingBackend {
    inner: Arc<dyn UiBackend>,
    pipeline: Arc<ViewerPipeline>,
}

impl ObservingBackend {
    pub fn new(inner: Arc<dyn UiBackend>, pipeline: Arc<ViewerPipeline>) -> Self {
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
