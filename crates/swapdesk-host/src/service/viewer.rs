//! The viewer side of a session: the service opens it on the UI's behalf and relays what arrives as C2.4 viewer
//! messages (the UI pipe of the next slice writes them to the pipe), and what the UI sends as the session's input
//! stream and feedback.
//!
//! The UI is "this user" and decodes and renders itself (C2.1): the service only moves bytes. Video frames come out
//! of the [`VideoReceiver`] (ordered, decodable) as `ViewerVideo`; audio datagrams and cursor positions are read
//! from the connection's datagrams; cursor shapes arrive on their own streams.
//!
//! A session whose connection is lost is resumed here (F-NET-01): the event task keeps the handle and the receiver
//! and swaps in the session the reconnect policy of `swapdesk-extras` returns, with no new prompt.

use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;
use std::time::{Duration, Instant};

use quinn::Endpoint;
use swapdesk_extras::chat::{ChatEntry, ChatError};
use swapdesk_extras::reconnect::{GiveUp, ReconnectEvent, ReconnectPolicy, reconnect, should_reconnect};
use swapdesk_ipc::{ApiCode, Bytes, Message as Ipc, Payload, SessionState, Shape};
use swapdesk_platform::{Level, TraceSink};
use swapdesk_proto::{
    AudioControl, CursorPosition, CursorShape, DisplayList, ErrorCode, InputEvent, KeyframeRequest, Message as Ctl,
    PermissionSet, ReceiverReport, Resume, StreamKind, VideoSettings, Wire,
};
use swapdesk_session::{
    ChannelHandler, EndKind, IncomingStream, OpenError, Registry, SessionCtx, SessionEnd, ViewerConfig, ViewerEvent,
    ViewerSession,
};
use swapdesk_swap::{SwapHandler, SwapOutcome};
use swapdesk_trust::DeviceId;
use swapdesk_video::{ReceiverConfig, VideoReceiver};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

use super::clip_actions::{ActionWaiters, ClipHooksCell, may_run, viewer_clipboard};
use super::extras::{ChatRouter, HooksCell};
use super::files::ViewerFiles;
use super::terminal::ViewerTerms;
use crate::proxy::PipeClipboard;
use swapdesk_extras::clipboard::ClipboardHandler;

/// Messages toward the UI before the producers wait (control, terminal) or drop (audio, cursor).
const OUT_QUEUE: usize = 256;
/// Bytes of video toward the UI before the next frame is dropped (one frame always fits an empty budget).
const OUT_VIDEO_BYTES: usize = 6 * 1024 * 1024;
/// Video frames toward the UI before the next one is dropped, so video never fills the queue audio shares.
const OUT_VIDEO_FRAMES: usize = 32;
/// A keyframe asked for because video was dropped is asked again after this long if it has not come.
const KEY_REASK: Duration = Duration::from_millis(500);
/// Input batches queued toward the writer task.
const INPUT_QUEUE: usize = 1024;
/// How long a quick action waits for the host's result.
const ACTION_WAIT: Duration = Duration::from_secs(25);
/// A cursor shape is at most 256 x 256 RGBA plus its header.
const MAX_SHAPE_STREAM: usize = 256 * 256 * 4 + 64;

/// A session's state, with the raw reason when it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateChange {
    /// 0 until the host grants the session.
    pub session_id: u64,
    pub device: DeviceId,
    pub state: SessionState,
    /// Set with `SessionState::Ended`: how the session ended, before it was squeezed into an `ApiCode`.
    pub end: Option<SessionEnd>,
}

/// What the UI hears of a viewer session.
#[derive(Debug)]
pub enum ViewerOut {
    /// `ViewerVideo`, `ViewerAudio`, `ViewerCursorPos` or `ViewerCursorShape`, ready for the UI pipe.
    Message(Ipc),
    /// The host's display list (its `UiNotify::Displays`), also reflected in the video receiver.
    Displays {
        session_id: u64,
        list: DisplayList,
    },
    State(StateChange),
    /// A terminal's bytes or its end, in order (the UI queue holds them for room instead of dropping them).
    Term(TermOut),
}

/// What a terminal of this session sends the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermOut {
    /// Shell output (at most `MAX_TERM_CHUNK` bytes; `Debug` of the bytes is a length only).
    Output { term_id: u32, bytes: Bytes },
    /// The shell ended by itself, the host ended it, or the session ended; never sent for a terminal the UI closed.
    Exited { term_id: u32, code: Option<i32> },
}

/// Video queued toward the UI, counted in frames and bytes (the channel counts messages only).
#[derive(Default)]
pub(crate) struct VideoBudget {
    bytes: AtomicUsize,
    frames: AtomicUsize,
}

impl VideoBudget {
    /// Takes room for a frame of `len` bytes; `false` when the UI is behind.
    fn reserve(&self, len: usize) -> bool {
        let (bytes, frames) = (self.bytes.load(Ordering::Acquire), self.frames.load(Ordering::Acquire));
        if frames > 0 && (frames >= OUT_VIDEO_FRAMES || bytes + len > OUT_VIDEO_BYTES) {
            return false;
        }
        self.bytes.fetch_add(len, Ordering::AcqRel);
        self.frames.fetch_add(1, Ordering::AcqRel);
        true
    }

    fn release(&self, len: usize) {
        self.bytes.fetch_sub(len, Ordering::AcqRel);
        self.frames.fetch_sub(1, Ordering::AcqRel);
    }

    fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Acquire)
    }
}

/// The receiving end of a viewer session's output.
pub struct ViewerMessages {
    rx: mpsc::Receiver<ViewerOut>,
    rt: Handle,
    video: Arc<VideoBudget>,
}

impl ViewerMessages {
    /// The next item; `None` when the session is over and everything was read.
    pub async fn recv(&mut self) -> Option<ViewerOut> {
        let item = self.rx.recv().await;
        self.account(item.as_ref());
        item
    }

    /// From a thread that is not on the runtime: waits up to `limit` for the next item. `Ok(None)` is the end.
    pub fn recv_timeout(&mut self, limit: Duration) -> Result<Option<ViewerOut>, Timeout> {
        let rx = &mut self.rx;
        let item = self.rt.block_on(async { tokio::time::timeout(limit, rx.recv()).await }).map_err(|_| Timeout)?;
        self.account(item.as_ref());
        Ok(item)
    }

    pub fn try_recv(&mut self) -> Option<ViewerOut> {
        let item = self.rx.try_recv().ok();
        self.account(item.as_ref());
        item
    }

    /// Bytes of video waiting to be read.
    pub fn video_bytes(&self) -> usize {
        self.video.bytes()
    }

    fn account(&self, item: Option<&ViewerOut>) {
        if let Some(ViewerOut::Message(Ipc::ViewerVideo { payload, .. })) = item {
            self.video.release(payload.0.len());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout;

/// Why a viewer session did not open.
#[derive(Debug)]
pub enum ConnectError {
    /// The LAN guard refused the address (why: its `Display`).
    Refused(swapdesk_lan::guard::Refusal),
    /// An address that is not IPv4.
    NotIpv4,
    /// The device is not in the trust store.
    UnknownDevice,
    /// The local socket could not be made.
    Socket(std::io::Error),
    /// The session did not open (the host said no, the connection failed).
    Open(OpenError),
}

impl core::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Refused(r) => write!(f, "refused by the LAN guard: {r}"),
            Self::NotIpv4 => f.write_str("not an IPv4 address"),
            Self::UnknownDevice => f.write_str("the device is not paired"),
            Self::Socket(e) => write!(f, "socket: {e}"),
            Self::Open(e) => write!(f, "session did not open: {e:?}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Why a message for a viewer session was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// No live session has this id.
    NoSuchSession,
    /// Input needs `CONTROL`, which the host has not granted (or took away).
    NotPermitted,
    /// The session is over.
    Closed,
    /// The input queue is full: this batch was dropped, and a `ReleaseAll` follows once the queue drains so no key
    /// stays down at the host (D9). The caller may tell the user.
    Full,
    /// Not a message a viewer's UI sends to the service.
    NotAViewerRequest,
}

/// The `ApiCode` a session end is shown as. C2.4 has no code for a normal end: a session that ended by a `Bye` of
/// either side with no error maps to `Refused` (the other side ended it), which the UI reads together with
/// `StateChange::end`.
/// `None` is a normal end (a bye with no error, a swap); `Some` names what ended the session.
pub fn end_reason(end: &SessionEnd) -> Option<ApiCode> {
    Some(match end.code {
        ErrorCode::NoError | ErrorCode::Swapped if !matches!(end.kind, EndKind::Lost | EndKind::Timeout) => {
            return None;
        }
        ErrorCode::NotPermitted => ApiCode::NotPermitted,
        ErrorCode::Busy => ApiCode::Busy,
        ErrorCode::Timeout => ApiCode::Timeout,
        ErrorCode::PairingLocked => ApiCode::Locked,
        ErrorCode::UnknownDevice | ErrorCode::NotFound | ErrorCode::Forgotten => ApiCode::NotFound,
        ErrorCode::HostShutdown => ApiCode::ServiceDown,
        ErrorCode::ProtocolViolation | ErrorCode::UnsupportedVersion | ErrorCode::Internal | ErrorCode::TooLarge => {
            ApiCode::Internal
        }
        _ => match end.kind {
            EndKind::Lost | EndKind::Timeout => ApiCode::Timeout,
            _ => ApiCode::Refused,
        },
    })
}

/// "Desconectar" while a reconnect runs: the flag every waiter of the session watches.
struct Stop {
    tx: watch::Sender<bool>,
}

impl Stop {
    fn new() -> Self {
        Self { tx: watch::channel(false).0 }
    }

    fn trigger(&self) {
        self.tx.send_replace(true);
    }

    fn is_set(&self) -> bool {
        *self.tx.borrow()
    }

    async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}

/// The session the handle drives now: replaced when a lost session is resumed.
struct CtxCell {
    current: Mutex<(SessionCtx, u64)>,
}

impl CtxCell {
    fn new(ctx: SessionCtx) -> Self {
        Self { current: Mutex::new((ctx, 0)) }
    }

    fn get(&self) -> SessionCtx {
        lock(&self.current).0.clone()
    }

    /// The session and its generation (a new one each time the session is replaced).
    fn snapshot(&self) -> (SessionCtx, u64) {
        let g = lock(&self.current);
        (g.0.clone(), g.1)
    }

    fn replace(&self, ctx: SessionCtx) {
        let mut g = lock(&self.current);
        g.0 = ctx;
        g.1 += 1;
    }
}

pub(crate) struct Inner {
    session_id: u64,
    cell: Arc<CtxCell>,
    device: DeviceId,
    input: mpsc::Sender<Vec<InputEvent>>,
    /// An input batch was dropped (a full queue, no stream): the writer sends `ReleaseAll` after the queue drains.
    input_lost: Arc<AtomicBool>,
    rt: Handle,
    stop: Arc<Stop>,
    chat: Arc<ChatRouter>,
    terms: Arc<ViewerTerms>,
    swap: SwapHandler,
    wall: Arc<dyn swapdesk_trust::WallClock>,
    clip: Arc<ClipboardHandler<PipeClipboard>>,
    actions: Arc<ActionWaiters>,
    files: Arc<ViewerFiles>,
    /// Keeps the endpoint (and its socket) alive as long as the session.
    _endpoint: Endpoint,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last handle going away is "Desconectar".
        self.stop.trigger();
        self.cell.get().bye(ErrorCode::NoError);
    }
}

/// A viewer session, as the UI drives it. Cheap to clone; the last clone dropped disconnects.
#[derive(Clone)]
pub struct ViewerHandle {
    pub(crate) inner: Arc<Inner>,
}

impl ViewerHandle {
    /// The session's id (it stays the same across a resume).
    pub fn session_id(&self) -> u64 {
        self.inner.session_id
    }

    pub fn device(&self) -> DeviceId {
        self.inner.device
    }

    /// What the host lets this session do now.
    pub fn granted(&self) -> PermissionSet {
        self.inner.cell.get().granted()
    }

    /// The session as it is now, for what the service layer needs of it (and tests). After a resume it is the new
    /// connection's session.
    pub fn ctx(&self) -> SessionCtx {
        self.inner.cell.get()
    }

    /// `ViewerInput`: the records go, in order, on the session's input stream. Never blocks: a full queue is
    /// `Full` (the batch is dropped and a `ReleaseAll` follows). Without `CONTROL` nothing is sent.
    pub fn send_input(&self, events: Vec<InputEvent>) -> Result<(), ApplyError> {
        let ctx = self.inner.cell.get();
        if !ctx.granted().contains(PermissionSet::CONTROL) {
            return Err(ApplyError::NotPermitted);
        }
        if ctx.ended().is_some() {
            return Err(ApplyError::Closed);
        }
        self.inner.input.try_send(events).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                self.inner.input_lost.store(true, Ordering::Release);
                ApplyError::Full
            }
            mpsc::error::TrySendError::Closed(_) => ApplyError::Closed,
        })
    }

    /// `ViewerKeyframe`.
    pub fn request_keyframe(&self, request: KeyframeRequest) -> Result<(), ApplyError> {
        self.send_control(request)
    }

    /// `ViewerReport`.
    pub fn report(&self, report: ReceiverReport) -> Result<(), ApplyError> {
        self.send_control(report)
    }

    /// `SelectDisplay`.
    pub fn select_display(&self, display: u16) -> Result<(), ApplyError> {
        self.send_control(swapdesk_proto::SelectDisplay { display })
    }

    /// `AudioControl`: the host captures only while it is on (D16).
    pub fn set_audio(&self, enabled: bool) -> Result<(), ApplyError> {
        self.send_control(AudioControl { enabled })
    }

    pub fn set_video_settings(&self, settings: VideoSettings) -> Result<(), ApplyError> {
        self.send_control(settings)
    }

    fn send_control(&self, msg: impl Into<Ctl>) -> Result<(), ApplyError> {
        self.inner.cell.get().try_send(msg).map_err(|_| ApplyError::Closed)
    }

    /// Applies a C2.4 message the UI sent for this session.
    pub fn apply(&self, msg: Ipc) -> Result<(), ApplyError> {
        match msg {
            Ipc::ViewerInput { events, .. } => self.send_input(events.0),
            Ipc::ViewerKeyframe { request, .. } => self.request_keyframe(request),
            Ipc::ViewerReport { report, .. } => self.report(report),
            _ => Err(ApplyError::NotAViewerRequest),
        }
    }

    /// "Desconectar": `Bye { NO_ERROR }` (and no more reconnecting); the session ends on its own task.
    pub fn disconnect(&self) {
        self.inner.stop.trigger();
        self.inner.cell.get().bye(ErrorCode::NoError);
    }

    /// From a thread that is not on the runtime: waits up to `limit` for the session to end.
    pub fn wait_ended(&self, limit: Duration) -> Option<SessionEnd> {
        let ctx = self.inner.cell.get();
        self.inner.rt.block_on(async { tokio::time::timeout(limit, ctx.wait_ended()).await }).ok()
    }

    /// "Inverter" (PROTOCOL.md 6.3): asks the host to swap. Resolves with how it ended; see [`SwapOutcome`].
    pub async fn request_swap(&self) -> SwapOutcome {
        self.inner.swap.request(&self.inner.cell.get()).await
    }

    /// Whether a swap is in flight on this session.
    pub fn swap_pending(&self) -> bool {
        self.inner.swap.pending(&self.inner.cell.get())
    }

    /// The session's files: the UI that owns the session binds its disk here, the hub's file ops run through it.
    pub fn files(&self) -> &Arc<ViewerFiles> {
        &self.inner.files
    }

    /// Sends a chat message; returns it as stored.
    pub async fn chat_send(&self, text: &str) -> Result<ChatEntry, ChatError> {
        self.inner.chat.send(&self.inner.cell.get(), text, self.inner.wall.unix_ms()).await
    }

    /// The chat panel shows everything the peer sent: the peer is told.
    pub async fn chat_mark_read(&self) {
        self.inner.chat.mark_read(&self.inner.cell.get()).await;
    }

    /// "Copiar e colar": this side's switch for the session; the host is told.
    pub fn set_clipboard_sync(&self, on: bool) {
        self.inner.clip.set_enabled(&self.inner.cell.get(), on);
    }

    /// Whether this side's clipboard sync is on.
    pub fn clipboard_sync_on(&self) -> bool {
        self.inner.clip.enabled()
    }

    /// A quick action on the host (F-ACT-01): sends `QuickAction` and waits for the host's `QuickActionResult`
    /// (the host answers at once or tells why it did not). `NoError` is "it ran".
    pub async fn run_action(&self, action: swapdesk_proto::Action) -> Result<ErrorCode, ApplyError> {
        let ctx = self.inner.cell.get();
        if !may_run(ctx.granted()) {
            return Err(ApplyError::NotPermitted);
        }
        let (action_id, rx) = self.inner.actions.register();
        if ctx.try_send(swapdesk_proto::QuickAction { action_id, kind: action }).is_err() {
            self.inner.actions.forget(action_id);
            return Err(ApplyError::Closed);
        }
        // The host's runner may wait on the machine (a suspend takes seconds): room for it, then give up.
        match tokio::time::timeout(ACTION_WAIT, rx).await {
            Ok(Ok(code)) => Ok(code),
            Ok(Err(_)) => Err(ApplyError::Closed),
            Err(_) => {
                self.inner.actions.forget(action_id);
                Ok(ErrorCode::Timeout)
            }
        }
    }

    /// The messages of this session so far.
    pub fn chat_entries(&self) -> Vec<ChatEntry> {
        self.inner.chat.session(self.inner.session_id).map(|h| h.entries()).unwrap_or_default()
    }

    // ---------- terminals (stage 43) ----------

    /// Opens terminal `term_id` (chosen by the caller, unique among this session's terminals) on the host and its
    /// stream; resolves with the account the shell runs as, or the code the host refused with (`NOT_PERMITTED`,
    /// `NOBODY_PRESENT`, `DECLINED` and `TIMEOUT` for the D13 question, `BUSY` past four terminals, `UNAVAILABLE`).
    /// It can take as long as the host's user takes to answer (20 s).
    pub async fn term_open(&self, term_id: u32, cols: u16, rows: u16, admin: bool) -> Result<String, ErrorCode> {
        self.inner.terms.open(&self.inner.cell.get(), term_id, cols, rows, admin).await
    }

    /// Keystrokes for terminal `term_id`; never waits. `false` when there is no such terminal or its queue is full.
    pub fn term_input(&self, term_id: u32, bytes: Vec<u8>) -> bool {
        self.inner.terms.input(term_id, bytes)
    }

    pub fn term_resize(&self, term_id: u32, cols: u16, rows: u16) -> bool {
        self.inner.terms.resize(&self.inner.cell.get(), term_id, cols, rows)
    }

    /// Ends terminal `term_id` on the host; nothing more is reported for it.
    pub fn term_close(&self, term_id: u32) -> bool {
        self.inner.terms.close(&self.inner.cell.get(), term_id)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The viewer's handler for what the host opens: video frame streams go to the receiver, cursor shapes to the UI.
struct ViewerStreams {
    receiver: VideoReceiver,
    out: mpsc::Sender<ViewerOut>,
    /// The session, for keyframe requests, once it is live.
    session: Arc<Mutex<Option<SessionCtx>>>,
    trace: Arc<dyn TraceSink>,
}

impl ChannelHandler for ViewerStreams {
    fn name(&self) -> &'static str {
        "viewer-streams"
    }

    fn streams(&self) -> &'static [StreamKind] {
        &[StreamKind::Video, StreamKind::CursorShape]
    }

    fn on_message(&self, _ctx: &SessionCtx, _msg: Ctl) {}

    fn on_start(&self, ctx: &SessionCtx) {
        *lock(&self.session) = Some(ctx.clone());
    }

    fn on_stream(&self, ctx: &SessionCtx, stream: IncomingStream) {
        match stream.kind {
            StreamKind::Video => self.receiver.handle_stream(stream.recv),
            StreamKind::CursorShape => {
                let (out, session_id, trace) = (self.out.clone(), ctx.session_id(), Arc::clone(&self.trace));
                let mut recv = stream.recv;
                ctx.spawn(async move {
                    let Ok(bytes) = recv.read_to_end(MAX_SHAPE_STREAM).await else { return };
                    match CursorShape::decode(&bytes) {
                        Ok(shape) => {
                            let _ = out.try_send(ViewerOut::Message(Ipc::ViewerCursorShape {
                                session_id,
                                shape: Shape(shape),
                            }));
                        }
                        Err(_) => {
                            trace.log(Level::Warn, "service", format_args!("a malformed cursor shape was dropped"))
                        }
                    }
                });
            }
            _ => {
                let _ = stream.recv;
            }
        }
    }
}

pub(crate) struct Opened {
    pub handle: ViewerHandle,
    pub messages: ViewerMessages,
}

/// What a caller hears of an attempt before the host grants: `Connecting`, then `Prompted { remaining_s }`.
pub type WaitHook = Arc<dyn Fn(SessionState) + Send + Sync>;

/// What `open` needs besides the viewer configuration.
pub(crate) struct OpenParams {
    pub rt: Handle,
    pub trace: Arc<dyn TraceSink>,
    pub wall: Arc<dyn swapdesk_trust::WallClock>,
    pub endpoint: Endpoint,
    pub identity: Arc<swapdesk_trust::Identity>,
    pub addr: std::net::SocketAddr,
    pub expected: swapdesk_trust::Spki,
    /// The machine's swap handler, registered in this session's registry.
    pub swap: SwapHandler,
    pub hooks: Arc<HooksCell>,
    pub clip_hooks: Arc<ClipHooksCell>,
    /// This session's files handler, registered in its registry.
    pub files: Arc<ViewerFiles>,
    /// Told `Connecting` when the dial starts and `Prompted` when the host starts asking its user.
    pub waiting: Option<WaitHook>,
}

/// What it takes to dial the same host again with a resume token.
struct Redial {
    endpoint: Endpoint,
    identity: Arc<swapdesk_trust::Identity>,
    addr: std::net::SocketAddr,
    expected: swapdesk_trust::Spki,
    /// The configuration of the first dial: same intent, same request (an unattended entry stays unattended).
    cfg: ViewerConfig,
}

impl Redial {
    async fn dial(&self, resume: Resume) -> Result<ViewerSession, OpenError> {
        let mut cfg = self.cfg.clone();
        cfg.resume = Some(resume);
        cfg.swap_token = None;
        cfg.on_prompt = None;
        ViewerSession::connect(&self.endpoint, &self.identity, self.addr, self.expected, cfg).await
    }
}

enum Either<A, B> {
    Left(A),
    Right(B),
}

/// Whichever of the two futures finishes first (no `select!`: the runtime is built without the macros feature).
async fn race<A: Future, B: Future>(a: A, b: B) -> Either<A::Output, B::Output> {
    let (mut a, mut b) = (pin!(a), pin!(b));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = a.as_mut().poll(cx) {
            return Poll::Ready(Either::Left(v));
        }
        if let Poll::Ready(v) = b.as_mut().poll(cx) {
            return Poll::Ready(Either::Right(v));
        }
        Poll::Pending
    })
    .await
}

/// Everything the event task keeps for the life of the handle.
struct Drive {
    rt: Handle,
    out: mpsc::Sender<ViewerOut>,
    cell: Arc<CtxCell>,
    receiver: VideoReceiver,
    session_id: u64,
    device: DeviceId,
    since_ms: u64,
    stop: Arc<Stop>,
    redial: Arc<Redial>,
    terms: Arc<ViewerTerms>,
}

fn active(ctx: &SessionCtx, since_ms: u64) -> SessionState {
    SessionState::Active { since_ms, role: swapdesk_ipc::Role::Viewer, granted: ctx.granted().bits() }
}

impl Drive {
    async fn emit(&self, state: SessionState, end: Option<SessionEnd>) {
        let _ = self
            .out
            .send(ViewerOut::State(StateChange { session_id: self.session_id, device: self.device, state, end }))
            .await;
    }

    /// Audio and cursor positions of a connection: realtime, so a UI that falls behind loses them.
    fn start_datagrams(&self, ctx: &SessionCtx) {
        let Some(conn) = ctx.connection().cloned() else { return };
        let (out, session_id) = (self.out.clone(), self.session_id);
        self.rt.spawn(async move {
            while let Ok(datagram) = conn.read_datagram().await {
                let msg = match datagram.first() {
                    Some(0x01) if datagram.len() <= swapdesk_ipc::MAX_AUDIO_DATAGRAM => {
                        Ipc::ViewerAudio { session_id, datagram: Bytes(datagram.to_vec()) }
                    }
                    Some(0x02) => match CursorPosition::decode(&datagram) {
                        Ok(pos) => Ipc::ViewerCursorPos { session_id, pos },
                        Err(_) => continue,
                    },
                    _ => continue,
                };
                let _ = out.try_send(ViewerOut::Message(msg));
            }
        });
    }

    /// The events of the live session until it ends. `None` when the channel closed with no end.
    async fn events(&self, session: &mut ViewerSession) -> Option<SessionEnd> {
        let ctx = session.ctx().clone();
        while let Some(event) = session.next_event().await {
            let state = match event {
                ViewerEvent::PermissionsChanged(_) | ViewerEvent::PeerOnline => Some(active(&ctx, self.since_ms)),
                ViewerEvent::HostState { desktop, paused } => {
                    Some(if paused { SessionState::Paused { desktop } } else { active(&ctx, self.since_ms) })
                }
                ViewerEvent::PeerOffline => Some(SessionState::Reconnecting { attempt: 0 }),
                ViewerEvent::PromptPending { .. } => None,
                ViewerEvent::Message(Ctl::DisplayList(list)) => {
                    let ids: Vec<u16> = list.displays.iter().map(|d| d.id).collect();
                    self.receiver.set_displays(&ids);
                    let _ = self.out.send(ViewerOut::Displays { session_id: self.session_id, list }).await;
                    None
                }
                // A refusal of a `TermOpen` (non-fatal `Error`: not permitted, nobody present, declined, busy, ...).
                ViewerEvent::Message(Ctl::Error(e)) => {
                    if e.about_type == 0x0501 {
                        self.terms.refused(e.about_id, e.code);
                    }
                    None
                }
                ViewerEvent::Message(_) => None,
                ViewerEvent::Ended(end) => return Some(end),
            };
            if let Some(state) = state {
                self.emit(state, None).await;
            }
        }
        None
    }

    /// The session was lost: dials again with the resume token until the host takes it back, the answer is final or
    /// the 60 s end ("Reconectando N"). `Some` is the resumed session; `None` means the end was already reported.
    async fn resume(&self, token: Resume) -> Option<ViewerSession> {
        let (out, session_id, device) = (self.out.clone(), self.session_id, self.device);
        let notify = move |e: ReconnectEvent| {
            if let ReconnectEvent::Reconnecting { attempt } = e {
                // A state the UI may miss is replaced by the next one: never wait here.
                let _ = out.try_send(ViewerOut::State(StateChange {
                    session_id,
                    device,
                    state: SessionState::Reconnecting { attempt },
                    end: None,
                }));
            }
        };
        let redial = Arc::clone(&self.redial);
        let dial = move |resume: Resume| {
            let redial = Arc::clone(&redial);
            async move { redial.dial(resume).await }
        };
        let result = race(reconnect(ReconnectPolicy::default(), token, dial, notify), self.stop.wait()).await;
        match result {
            Either::Left(Ok(session)) => {
                let ctx = session.ctx().clone();
                self.cell.replace(ctx.clone());
                self.start_datagrams(&ctx);
                self.emit(active(&ctx, self.since_ms), None).await;
                Some(session)
            }
            Either::Left(Err(why)) => {
                let end = match why {
                    GiveUp::WindowElapsed => SessionEnd { kind: EndKind::Lost, code: ErrorCode::Timeout },
                    GiveUp::Refused(code) => SessionEnd { kind: EndKind::Refused, code },
                    GiveUp::Protocol(code) => SessionEnd { kind: EndKind::Violation, code },
                };
                self.emit(SessionState::Ended { reason: end_reason(&end) }, Some(end)).await;
                None
            }
            Either::Right(()) => {
                let end = SessionEnd { kind: EndKind::LocalBye, code: ErrorCode::NoError };
                self.emit(SessionState::Ended { reason: None }, Some(end)).await;
                None
            }
        }
    }
}

/// Opens a viewer session on `p.endpoint` and starts its relays. `cfg.registry` is filled here.
pub(crate) async fn open(p: OpenParams, mut cfg: ViewerConfig) -> Result<Opened, ConnectError> {
    let OpenParams { rt, trace, wall, endpoint, identity, addr, expected, swap, hooks, clip_hooks, files, waiting } = p;
    let device = swapdesk_trust::device_id(&expected);
    let (out_tx, out_rx) = mpsc::channel(OUT_QUEUE);
    // Before the grant there is no session to carry states: the caller's hook hears them.
    if let Some(wait) = &waiting {
        wait(SessionState::Connecting);
    }

    // The receiver asks for keyframes through the session, which exists only after the grant.
    let session: Arc<Mutex<Option<SessionCtx>>> = Arc::default();
    let feedback = {
        let session = Arc::clone(&session);
        move |req: KeyframeRequest| {
            if let Some(ctx) = lock(&session).clone() {
                let _ = ctx.try_send(req);
            }
        }
    };
    let (receiver, mut frames) = VideoReceiver::new(ReceiverConfig::default(), Arc::new(feedback));
    let mut registry = Registry::new();
    let streams = Arc::new(ViewerStreams {
        receiver: receiver.clone(),
        out: out_tx.clone(),
        session: Arc::clone(&session),
        trace: Arc::clone(&trace),
    });
    // Nothing else is registered on this registry, so these cannot clash.
    let _ = registry.register(streams);
    let _ = registry.register(swap.handler());
    let chat = ChatRouter::new(hooks);
    let _ = registry.register(chat.clone());
    // The clipboard of this session is the clipboard of the UI that owns it; the host's quick-action results land here.
    let (clip, clip_session) = viewer_clipboard(&clip_hooks);
    let actions = Arc::new(ActionWaiters::default());
    let _ = registry.register(clip.clone());
    let _ = registry.register(actions.clone());
    let terms = ViewerTerms::new(out_tx.clone(), rt.clone());
    let _ = registry.register(terms.clone());
    let _ = registry.register(files.channel());
    cfg.registry = registry;
    let redial_cfg = cfg.clone();
    cfg.on_prompt = waiting.map(|wait| {
        Arc::new(move |remaining_s: u16| wait(SessionState::Prompted { remaining_s })) as Arc<dyn Fn(u16) + Send + Sync>
    });

    let session_handle =
        ViewerSession::connect(&endpoint, &identity, addr, expected, cfg).await.map_err(ConnectError::Open)?;
    let ctx = session_handle.ctx().clone();
    *lock(&session) = Some(ctx.clone());
    let session_id = session_handle.grant().session_id;
    clip_session.store(session_id, std::sync::atomic::Ordering::Release);
    files.set_session(session_id);
    let since_ms = wall.unix_ms();
    let cell = Arc::new(CtxCell::new(ctx.clone()));
    let stop = Arc::new(Stop::new());
    let drive = Drive {
        rt: rt.clone(),
        out: out_tx.clone(),
        cell: Arc::clone(&cell),
        receiver,
        session_id,
        device,
        since_ms,
        stop: Arc::clone(&stop),
        redial: Arc::new(Redial { endpoint: endpoint.clone(), identity, addr, expected, cfg: redial_cfg }),
        terms: Arc::clone(&terms),
    };
    drive.emit(active(&ctx, since_ms), None).await;

    // Video: ordered, decodable frames to the UI. When the UI falls behind (its budget in frames or bytes is spent, or
    // the queue is full) the frame is dropped, and so is every P-frame after it until a keyframe, which is asked for.
    let video_budget = Arc::new(VideoBudget::default());
    {
        let out = out_tx.clone();
        let (budget, cell) = (Arc::clone(&video_budget), Arc::clone(&cell));
        rt.spawn(async move {
            let (mut skipping, mut last_good, mut asked): (bool, u32, Option<Instant>) = (false, 0, None);
            while let Some(f) = frames.recv().await {
                let key = f.header.keyframe || f.header.discontinuity;
                let display = f.header.display_id;
                let ask = |asked: &mut Option<Instant>, last_good: u32| {
                    if asked.is_none_or(|t| t.elapsed() >= KEY_REASK) {
                        *asked = Some(Instant::now());
                        let _ = cell.get().try_send(KeyframeRequest { display, last_good_frame: last_good });
                    }
                };
                if skipping && !key {
                    ask(&mut asked, last_good);
                    continue;
                }
                let (id, len) = (f.header.frame_id, f.payload.len());
                if !budget.reserve(len) {
                    skipping = true;
                    ask(&mut asked, last_good);
                    continue;
                }
                let msg = Ipc::ViewerVideo { session_id, header: f.header, payload: Payload::from(f.payload) };
                match out.try_send(ViewerOut::Message(msg)) {
                    Ok(()) => {
                        if key {
                            skipping = false;
                            asked = None;
                        }
                        last_good = id;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        budget.release(len);
                        skipping = true;
                        ask(&mut asked, last_good);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        });
    }
    drive.start_datagrams(&ctx);
    // The input stream: one long-lived unidirectional stream, opened with the first batch and again after a break
    // or a resume.
    let (input_tx, mut input_rx) = mpsc::channel::<Vec<InputEvent>>(INPUT_QUEUE);
    let input_lost = Arc::new(AtomicBool::new(false));
    {
        let cell = Arc::clone(&cell);
        let trace = Arc::clone(&trace);
        let lost = Arc::clone(&input_lost);
        rt.spawn(async move {
            let mut stream: Option<quinn::SendStream> = None;
            let mut seen = 0;
            while let Some(events) = input_rx.recv().await {
                let (ctx, generation) = cell.snapshot();
                if generation != seen {
                    stream = None;
                    seen = generation;
                }
                if !ctx.granted().contains(PermissionSet::CONTROL) {
                    stream = None;
                    continue;
                }
                let mut bytes = Vec::new();
                for e in &events {
                    match e.encode() {
                        Ok(b) => bytes.extend_from_slice(&b),
                        Err(_) => break,
                    }
                }
                if stream.is_none() {
                    match ctx.open_uni(StreamKind::Input).await {
                        Ok(s) => stream = Some(s),
                        Err(c) => {
                            trace.log(Level::Debug, "service", format_args!("input stream not opened: {c:?}"));
                            // The batch is lost, key-ups included: let go of everything once a stream is back (D9).
                            lost.store(true, Ordering::Release);
                            continue;
                        }
                    }
                }
                if let Some(s) = stream.as_mut()
                    && s.write_all(&bytes).await.is_err()
                {
                    // The host stopped the stream (CONTROL revoked): the next batch opens a new one if allowed.
                    stream = None;
                    lost.store(true, Ordering::Release);
                }
                // After a dropped batch, `ReleaseAll` goes behind everything already queued (so no queued key-down
                // lands after it) as soon as the queue is empty.
                if input_rx.is_empty() && lost.swap(false, Ordering::AcqRel) {
                    let release = InputEvent::ReleaseAll.encode().unwrap_or_default();
                    if stream.is_none()
                        && let Ok(s) = ctx.open_uni(StreamKind::Input).await
                    {
                        stream = Some(s);
                    }
                    let sent = match stream.as_mut() {
                        Some(s) => s.write_all(&release).await.is_ok(),
                        None => false,
                    };
                    if !sent {
                        stream = None;
                        lost.store(true, Ordering::Release);
                    }
                }
            }
            if let Some(mut s) = stream {
                let _ = s.finish();
            }
        });
    }
    // Session events: state changes, the display list, the end, and the resume of a lost connection.
    {
        let rt_inner = rt.clone();
        rt.spawn(async move {
            let mut live = session_handle;
            loop {
                let end = drive.events(&mut live).await;
                let Some(end) = end else { break };
                if should_reconnect(&end) && !drive.stop.is_set() {
                    let token = live.grant().resume.clone();
                    drop(live);
                    match drive.resume(token).await {
                        Some(next) => {
                            live = next;
                            continue;
                        }
                        None => return,
                    }
                }
                drive.emit(SessionState::Ended { reason: end_reason(&end) }, Some(end)).await;
                break;
            }
            drop(live);
            drop(rt_inner);
        });
    }

    let inner = Inner {
        session_id,
        cell,
        device,
        input: input_tx,
        input_lost,
        rt: rt.clone(),
        stop,
        chat,
        terms,
        swap,
        wall,
        clip,
        actions,
        files,
        _endpoint: endpoint,
    };
    Ok(Opened {
        handle: ViewerHandle { inner: Arc::new(inner) },
        messages: ViewerMessages { rx: out_rx, rt, video: video_budget },
    })
}
