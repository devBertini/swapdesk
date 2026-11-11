//! Terminals in the service (stage 43; flows F-TERM-01, F-TERM-02; D13): the host's handler for `TermOpen`,
//! `TermResize`, `TermClose` and the terminal stream, and the viewer's handler for `TermOpened` and `TermExited`.
//!
//! The service owns the QUIC sockets and so the terminal channel, but it never runs the user's shell itself:
//!
//! * a user terminal is a shell in the user helper ([`super::pty_proxy::PtyProxy`]), whose own token guard refuses
//!   anything but the console user's account;
//! * an administrator terminal is the one thing the service starts, through the [`TerminalEnv`]'s spawner (the
//!   service's own SYSTEM token in session 0, so it never sits on a user's desktop; F-TERM-02).
//!
//! `TermOpen` goes through, in order: the session core (direction, `TERMINAL` or `ADMIN_TERMINAL` granted, and for the
//! administrator terminal also asked for in the `Hello`, D10), the D13 step ([`consent_need`]: in an unattended
//! session a user other than the one who granted `UNATTENDED` is asked first, nobody present is a refusal) and
//! `TermHost::open` (the second line of the permission check, the four-terminal cap, sizes, ids). A refusal is a
//! non-fatal `Error` about the `TermOpen`; success is `TermOpened`, and the viewer then opens the terminal stream
//! (a bidirectional stream: kind `0x10`, `term_id` as four bytes, then raw PTY bytes both ways, PROTOCOL.md 4).
//!
//! Every wait is bounded: the shell's output waits for room on the stream (flow control reaches the PTY), the
//! viewer's bytes wait for room in the PTY's queue (the stream reader stops reading), and a terminal whose stream
//! never arrives is closed after [`ATTACH_TIMEOUT`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use swapdesk_ipc::{Bytes, MAX_TERM_ACCOUNT};
use swapdesk_platform::{ErrorKind, Level, PlatformError, Pty, PtyRequest, PtySpawner, Result as PResult};
use swapdesk_proto::{
    ErrorCode, ErrorMessage, Feature, Message, PermissionSet, StreamKind, TermClose, TermExited, TermOpen, TermOpened,
    TermResize,
};
use swapdesk_session::{ChannelHandler, IncomingStream, SessionCtx, SessionEnd};
use swapdesk_term::{
    ConsentFacts, ConsentNeed, InputTx, OpenContext, TERM_HEADER_LEN, TermEvent, TermHost, consent_need,
    parse_term_header, term_header,
};
use swapdesk_trust::TrustHandle;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};

use super::extras::HooksCell;
use super::pty_proxy::PtyProxy;
use super::shared::Shared;
use super::viewer::{TermOut, ViewerOut};

/// What a `TermOpen` waits for the viewer's stream before the terminal is closed again.
pub const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);
/// The header of the terminal stream must arrive within this.
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// Chunks of one terminal's output queued between the PTY pump and the stream writer.
const OUT_CHUNKS: usize = 16;
/// Events the PTY side of a session queues before its shells are made to wait.
const EVENT_CAPACITY: usize = 8;
/// One read of the viewer's stream.
const INPUT_CHUNK: usize = 8192;
/// How long the finished stream's last bytes get to reach the viewer before `TermExited` follows them.
const FLUSH_WAIT: Duration = Duration::from_secs(2);
/// How long the viewer waits for a `TermOpened` or a refusal: the D13 question (20 s) and the shell's start.
const OPEN_WAIT: Duration = Duration::from_secs(40);
/// Viewer input chunks queued toward the stream writer.
const VIEWER_INPUT_CHUNKS: usize = 1024;
/// The message type of `TermOpen`, as `Error.about_type` names it.
const TERM_OPEN_TYPE: u16 = 0x0501;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the machine tells the terminals; set once after the core starts (the Windows service gives the SCM-side facts).
pub trait TerminalEnv: Send + Sync + 'static {
    /// The account of the user logged in at the console, as the same kind of identifier the trust store keeps for
    /// `unattended_granted_by` (a SID on Windows); `None` at the login screen. Called for every `TermOpen`.
    fn console_user(&self) -> Option<String>;

    /// The spawner of the administrator terminal: the service's own SYSTEM token in session 0. `None`: this machine
    /// has none (a development run as a user), and an administrator terminal is refused `Unavailable`.
    fn admin_spawner(&self) -> Option<Arc<dyn PtySpawner>>;
}

/// The environment, set once after the core starts; until then nobody is logged in and no administrator exists.
#[derive(Default)]
pub(crate) struct EnvCell(OnceLock<Arc<dyn TerminalEnv>>);

impl EnvCell {
    pub fn set(&self, env: Arc<dyn TerminalEnv>) -> bool {
        self.0.set(env).is_ok()
    }

    pub fn console_user(&self) -> Option<String> {
        self.0.get().and_then(|e| e.console_user())
    }

    fn admin_spawner(&self) -> Option<Arc<dyn PtySpawner>> {
        self.0.get().and_then(|e| e.admin_spawner())
    }
}

/// The spawner a session's `TermHost` holds: user shells in the helper, administrator shells in the service.
struct Routing {
    user: PtyProxy,
    env: Arc<EnvCell>,
}

impl PtySpawner for Routing {
    fn spawn(&self, request: &PtyRequest) -> PResult<Box<dyn Pty>> {
        if request.admin {
            let spawner = self.env.admin_spawner().ok_or(PlatformError {
                kind: ErrorKind::Unavailable,
                op: "Routing.spawn",
                os_code: None,
                detail: "this machine has no administrator terminal".into(),
            })?;
            return spawner.spawn(request);
        }
        self.user.spawn(request)
    }
}

/// What a terminal's pump tells its stream writer: bytes, then the end.
enum Out {
    Data(Vec<u8>),
    Exit(Option<i32>),
}

struct Sink {
    tx: mpsc::Sender<Out>,
    /// Taken when the stream arrives.
    rx: Option<mpsc::Receiver<Out>>,
}

/// The terminals of one host session.
struct SessionTerms {
    host: Mutex<TermHost>,
    sinks: Arc<Mutex<HashMap<u32, Sink>>>,
    events: Mutex<Option<Receiver<TermEvent>>>,
    pumping: AtomicBool,
    /// Commands applied in order: a resize never overtakes the one before it.
    cmds: CmdQueue,
}

/// Commands waiting for a session's applier before the next one is refused.
const CMD_QUEUE: usize = 256;

enum Cmd {
    /// Apply the newest size recorded for this terminal (see [`CmdQueue::resize`]).
    Resize {
        term_id: u32,
    },
    Close {
        term_id: u32,
    },
}

/// The bounded queue of a session's terminal commands, fed by the viewer. A viewer can send a resize for every pixel of
/// a window drag, so a resize is recorded as "the newest size of this terminal" and queued once: a flood costs one place
/// per terminal and the shell gets the last size. A close is idempotent: one that finds the queue full is dropped, the
/// first of them is already waiting. Nothing grows past [`CMD_QUEUE`].
struct CmdQueue {
    tx: mpsc::Sender<Cmd>,
    sizes: Mutex<HashMap<u32, (u16, u16)>>,
}

impl CmdQueue {
    fn new() -> (Self, mpsc::Receiver<Cmd>) {
        let (tx, rx) = mpsc::channel(CMD_QUEUE);
        (Self { tx, sizes: Mutex::new(HashMap::new()) }, rx)
    }

    fn resize(&self, term_id: u32, cols: u16, rows: u16) {
        // The marker is queued with the map locked (`try_send` never waits): a resize that comes meanwhile either finds
        // the entry and relies on a marker that is really queued, or finds it gone and queues its own. Without the lock
        // the entry could be removed after a newer size was recorded under it, and that size would be lost.
        let mut sizes = lock(&self.sizes);
        let first = sizes.insert(term_id, (cols, rows)).is_none();
        // Already queued: the applier will read the newer size. Not queued and no room: forget it, a later one tries again.
        if first && self.tx.try_send(Cmd::Resize { term_id }).is_err() {
            sizes.remove(&term_id);
        }
    }

    fn close(&self, term_id: u32) {
        let _ = self.tx.try_send(Cmd::Close { term_id });
    }

    /// The size the applier of `Cmd::Resize` reads: the newest, once.
    fn take_size(&self, term_id: u32) -> Option<(u16, u16)> {
        lock(&self.sizes).remove(&term_id)
    }
}

impl SessionTerms {
    /// Starts the thread that moves PTY events to the sinks, once.
    fn ensure_pump(&self) {
        if self.pumping.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(events) = lock(&self.events).take() else { return };
        let sinks = Arc::clone(&self.sinks);
        let started = std::thread::Builder::new().name("swapdesk-term-pump".into()).spawn(move || {
            // Ends when the session's `TermHost` is gone (every sender of the channel dropped).
            while let Ok(event) = events.recv() {
                let (id, out) = match event {
                    TermEvent::Output { term_id, data } => (term_id, Out::Data(data)),
                    TermEvent::Exited { term_id, exit_code } => (term_id, Out::Exit(exit_code)),
                };
                let tx = lock(&sinks).get(&id).map(|s| s.tx.clone());
                if let Some(tx) = tx {
                    // Waits for room: the back-pressure that stops the shell. A closed sink is a closed terminal.
                    let _ = tx.blocking_send(out);
                }
            }
        });
        if started.is_err() {
            self.pumping.store(false, Ordering::SeqCst);
        }
    }
}

struct HostInner {
    shared: Arc<Shared>,
    env: Arc<EnvCell>,
    trust: Arc<TrustHandle>,
    hooks: Arc<HooksCell>,
    sessions: Mutex<HashMap<u64, Arc<SessionTerms>>>,
}

/// The host's terminal handler (registered in the host registry).
pub(crate) struct HostTerminals(Arc<HostInner>);

impl HostTerminals {
    pub fn new(shared: Arc<Shared>, env: Arc<EnvCell>, trust: Arc<TrustHandle>, hooks: Arc<HooksCell>) -> Arc<Self> {
        Arc::new(Self(Arc::new(HostInner { shared, env, trust, hooks, sessions: Mutex::new(HashMap::new()) })))
    }
}

/// The D13 step: whether the logged-in user must say yes first, asked through the session, and the refusal if not.
/// This is [`swapdesk_term::run_consent`] with an asker that can wait (the question is the session's own task, so it
/// ends with the session or the permission and closes its card).
pub(crate) async fn consent_step(ctx: &SessionCtx, facts: &ConsentFacts<'_>) -> Result<(), ErrorCode> {
    match consent_need(facts) {
        ConsentNeed::NotNeeded => Ok(()),
        ConsentNeed::Refuse(code) => Err(code),
        ConsentNeed::Ask => ctx.request_consent(Feature::Terminal).await,
    }
}

fn refuse(code: ErrorCode, about_type: u16, about_id: u32) -> ErrorMessage {
    ErrorMessage { code, fatal: false, about_type, about_id }
}

impl HostInner {
    fn session(&self, ctx: &SessionCtx) -> Option<Arc<SessionTerms>> {
        lock(&self.sessions).get(&ctx.session_id()).cloned()
    }

    fn new_session(self: &Arc<Self>, ctx: &SessionCtx) -> Arc<SessionTerms> {
        let user = PtyProxy::new(Arc::clone(&self.shared), {
            let env = Arc::clone(&self.env);
            Arc::new(move || env.console_user())
        });
        let spawner: Arc<dyn PtySpawner> = Arc::new(Routing { user, env: Arc::clone(&self.env) });
        let (host, events) = TermHost::new(spawner, EVENT_CAPACITY);
        let (cmds, mut rx) = CmdQueue::new();
        let terms = Arc::new(SessionTerms {
            host: Mutex::new(host),
            sinks: Arc::default(),
            events: Mutex::new(Some(events)),
            pumping: AtomicBool::new(false),
            cmds,
        });
        // The commands of this session, in order. The host's lock is held only inside blocking tasks.
        let (inner, run, session) = (Arc::clone(self), Arc::clone(&terms), ctx.clone());
        ctx.spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let (inner, run, session) = (Arc::clone(&inner), Arc::clone(&run), session.clone());
                let _ = tokio::task::spawn_blocking(move || inner.apply(&run, &session, cmd)).await;
            }
        });
        terms
    }

    fn apply(&self, run: &SessionTerms, ctx: &SessionCtx, cmd: Cmd) {
        match cmd {
            Cmd::Resize { term_id } => {
                let Some((cols, rows)) = run.cmds.take_size(term_id) else { return };
                if let Err(code) = lock(&run.host).resize(term_id, cols, rows) {
                    let _ = ctx.try_send(refuse(code, 0x0503, term_id));
                }
            }
            Cmd::Close { term_id } => {
                lock(&run.sinks).remove(&term_id);
                // The viewer asked for it: no `TermExited` follows.
                let _ = lock(&run.host).close(term_id);
            }
        }
    }

    /// `TermOpen`, after the session core accepted it: the D13 step, then the shell, then the answer.
    async fn open(self: Arc<Self>, ctx: SessionCtx, run: Arc<SessionTerms>, req: TermOpen) {
        let needed = if req.admin { PermissionSet::ADMIN_TERMINAL } else { PermissionSet::TERMINAL };
        let console_user = self.env.console_user();
        // The facts of D13, read from the trust store and the session: the one who granted `UNATTENDED`, whether this
        // session entered without a prompt (a resumed one keeps the grant), and who is logged in now.
        let granted_by = ctx.device().and_then(|d| {
            self.trust.read(|s| s.find_by_id(&d).and_then(|p| p.unattended_granted_by.as_ref().map(|u| u.id.clone())))
        });
        let facts = ConsentFacts {
            session_unattended: ctx.entered_unattended(),
            granted_by: granted_by.as_deref(),
            console_user: console_user.as_deref(),
            admin_terminal: req.admin,
            account: self.shared.account.get().map(String::as_str),
        };
        // ACCOUNTS.md 5.1 (review of S4, L10): while the session's account is away from the console another account
        // is at the keyboard: no terminal opens, the administrator terminal included (and the running ones end).
        let away = ctx.desktop_state().desktop == swapdesk_proto::Desktop::AccountAway;
        let answer = match if away { Err(ErrorCode::AccountAway) } else { consent_step(&ctx, &facts).await } {
            Err(code) => Err(code),
            // Asked again after the question, which can take its time: the account may have left meanwhile.
            Ok(()) if ctx.desktop_state().desktop == swapdesk_proto::Desktop::AccountAway => {
                Err(ErrorCode::AccountAway)
            }
            Ok(()) => {
                let user_present = console_user.is_some();
                let (inner, run2, ctx2, req2) = (Arc::clone(&self), Arc::clone(&run), ctx.clone(), req.clone());
                tokio::task::spawn_blocking(move || inner.start(&run2, &ctx2, &req2, user_present, needed))
                    .await
                    .unwrap_or(Err(ErrorCode::Internal))
            }
        };
        match answer {
            Ok(opened) => {
                let _ = ctx.send(opened).await;
                self.shared.log(
                    Level::Info,
                    format_args!(
                        "terminal {} opened in session {:#x} (admin {})",
                        req.term_id,
                        ctx.session_id(),
                        req.admin
                    ),
                );
                if let (Some(hooks), Some(device)) = (self.hooks.get(), ctx.device()) {
                    hooks.terminal_opened(device, req.admin);
                }
                // A terminal whose stream never comes is closed again.
                let (inner, run, ctx2) = (Arc::clone(&self), Arc::clone(&run), ctx.clone());
                ctx.spawn(async move {
                    tokio::time::sleep(ATTACH_TIMEOUT).await;
                    let unattached = lock(&run.sinks).get(&req.term_id).is_some_and(|s| s.rx.is_some());
                    if unattached {
                        inner.end_terminal(&run, &ctx2, req.term_id, None).await;
                    }
                });
            }
            Err(code) => {
                self.shared.log(
                    Level::Info,
                    format_args!("terminal {} refused in session {:#x}: {code:?}", req.term_id, ctx.session_id()),
                );
                // The session's cap counted this id when the request arrived: the refusal gives it back.
                ctx.release_terminal(req.term_id);
                let _ = ctx.send(refuse(code, TERM_OPEN_TYPE, req.term_id)).await;
            }
        }
    }

    /// On a blocking thread: the sink first (so no output is lost), then the shell.
    fn start(
        &self,
        run: &SessionTerms,
        ctx: &SessionCtx,
        req: &TermOpen,
        user_present: bool,
        needed: PermissionSet,
    ) -> Result<TermOpened, ErrorCode> {
        run.ensure_pump();
        let (tx, rx) = mpsc::channel(OUT_CHUNKS);
        {
            let mut sinks = lock(&run.sinks);
            if sinks.contains_key(&req.term_id) {
                return Err(ErrorCode::ProtocolViolation);
            }
            sinks.insert(req.term_id, Sink { tx, rx: Some(rx) });
        }
        let opened = lock(&run.host).open(req, &OpenContext { granted: ctx.granted(), user_present });
        match opened {
            Ok(opened) => {
                // The permission may have gone while the shell started: it does not stay.
                if !ctx.granted().contains(needed) {
                    lock(&run.sinks).remove(&req.term_id);
                    let _ = lock(&run.host).close(req.term_id);
                    return Err(ErrorCode::NotPermitted);
                }
                Ok(TermOpened {
                    term_id: opened.term_id,
                    account: opened.account.chars().take(MAX_TERM_ACCOUNT).collect(),
                })
            }
            Err(code) => {
                lock(&run.sinks).remove(&req.term_id);
                Err(code)
            }
        }
    }

    /// The shell ended or was ended by the host: the viewer is told (`TermExited`), the terminal forgotten.
    async fn end_terminal(
        self: &Arc<Self>,
        run: &Arc<SessionTerms>,
        ctx: &SessionCtx,
        term_id: u32,
        exit_code: Option<i32>,
    ) {
        lock(&run.sinks).remove(&term_id);
        let run2 = Arc::clone(run);
        let _ = tokio::task::spawn_blocking(move || lock(&run2.host).forget(term_id)).await;
        let _ = ctx.send(TermExited { term_id, exit_code }).await;
    }

    /// The viewer's stream for a terminal: header, then bytes both ways until it ends.
    async fn attach(
        self: Arc<Self>,
        ctx: SessionCtx,
        run: Arc<SessionTerms>,
        mut recv: quinn::RecvStream,
        send: quinn::SendStream,
    ) {
        let mut head = [0u8; TERM_HEADER_LEN];
        if !matches!(tokio::time::timeout(HEADER_TIMEOUT, recv.read_exact(&mut head)).await, Ok(Ok(()))) {
            return;
        }
        let Some(term_id) = parse_term_header(&head) else { return };
        let Some(rx) = lock(&run.sinks).get_mut(&term_id).and_then(|s| s.rx.take()) else {
            // Unknown, closed, or already attached: the stream is dropped, which stops it.
            return;
        };
        let run2 = Arc::clone(&run);
        let input: Option<InputTx> =
            tokio::task::spawn_blocking(move || lock(&run2.host).input_sender(term_id)).await.ok().flatten();
        let Some(input) = input else { return };
        let needed = PermissionSet::TERMINAL;
        let (inner, run3, ctx3) = (Arc::clone(&self), Arc::clone(&run), ctx.clone());
        ctx.spawn_gated(needed, async move { inner.write_out(ctx3, run3, term_id, rx, send).await });
        // The viewer's bytes into the shell, waiting for room in its queue (QUIC flow control reaches the viewer).
        let mut buf = vec![0u8; INPUT_CHUNK];
        while let Ok(Some(n)) = recv.read(&mut buf).await {
            let (tx, chunk) = (input.clone(), buf[..n].to_vec());
            if !matches!(tokio::task::spawn_blocking(move || tx.send(chunk)).await, Ok(Ok(()))) {
                break;
            }
        }
    }

    /// The shell's bytes onto the stream, then the end: the stream finishes, and `TermExited` follows once the
    /// viewer has had the last bytes.
    async fn write_out(
        self: Arc<Self>,
        ctx: SessionCtx,
        run: Arc<SessionTerms>,
        term_id: u32,
        mut rx: mpsc::Receiver<Out>,
        mut send: quinn::SendStream,
    ) {
        while let Some(out) = rx.recv().await {
            match out {
                Out::Data(bytes) => {
                    if send.write_all(&bytes).await.is_err() {
                        // The viewer reset its stream: it does not want this terminal any more.
                        self.end_terminal(&run, &ctx, term_id, None).await;
                        return;
                    }
                }
                Out::Exit(code) => {
                    let _ = send.finish();
                    let _ = tokio::time::timeout(FLUSH_WAIT, send.stopped()).await;
                    self.end_terminal(&run, &ctx, term_id, code).await;
                    return;
                }
            }
        }
        // The sink went away without an exit: the terminal was closed by the viewer or the grant. Nothing follows.
        let _ = send.finish();
    }
}

impl ChannelHandler for HostTerminals {
    fn name(&self) -> &'static str {
        "terminal"
    }

    fn messages(&self) -> &'static [u16] {
        &[0x0501, 0x0503, 0x0504]
    }

    fn streams(&self) -> &'static [StreamKind] {
        &[StreamKind::Terminal]
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        let inner = &self.0;
        match msg {
            Message::TermOpen(req) => {
                let needed = if req.admin { PermissionSet::ADMIN_TERMINAL } else { PermissionSet::TERMINAL };
                let run = match inner.session(ctx) {
                    Some(r) => r,
                    None => {
                        let run = inner.new_session(ctx);
                        lock(&inner.sessions).insert(ctx.session_id(), Arc::clone(&run));
                        run
                    }
                };
                let (inner, ctx2) = (Arc::clone(inner), ctx.clone());
                // Gated: a revoked `TERMINAL` ends the question and the start (F-PERM-02).
                let id = req.term_id;
                if !ctx.spawn_gated(needed, async move { inner.open(ctx2, run, req).await }) {
                    // The grant went between the session core's check and here, or the session is closing.
                    ctx.release_terminal(id);
                    let _ = ctx.try_send(refuse(ErrorCode::NotPermitted, TERM_OPEN_TYPE, id));
                }
            }
            Message::TermResize(TermResize { term_id, cols, rows }) => {
                if let Some(run) = inner.session(ctx) {
                    run.cmds.resize(term_id, cols, rows);
                }
            }
            Message::TermClose(TermClose { term_id }) => {
                if let Some(run) = inner.session(ctx) {
                    run.cmds.close(term_id);
                }
            }
            _ => {}
        }
    }

    fn on_stream(&self, ctx: &SessionCtx, stream: IncomingStream) {
        let IncomingStream { kind, recv, send } = stream;
        let (StreamKind::Terminal, Some(send), Some(run)) = (kind, send, self.0.session(ctx)) else { return };
        let (inner, ctx2) = (Arc::clone(&self.0), ctx.clone());
        ctx.spawn_gated(PermissionSet::TERMINAL, async move { inner.attach(ctx2, run, recv, send).await });
    }

    fn on_permissions(&self, ctx: &SessionCtx, _old: PermissionSet, now: PermissionSet) {
        let Some(run) = self.0.session(ctx) else { return };
        let (inner, ctx2) = (Arc::clone(&self.0), ctx.clone());
        // F-PERM-02: what the new grants no longer cover ends now; the viewer hears it as `TermExited`.
        tokio::spawn(async move {
            let run2 = Arc::clone(&run);
            let gone = tokio::task::spawn_blocking(move || lock(&run2.host).enforce(now)).await.unwrap_or_default();
            for term_id in gone {
                lock(&run.sinks).remove(&term_id);
                let _ = ctx2.send(TermExited { term_id, exit_code: None }).await;
            }
            drop(inner);
        });
    }

    /// ACCOUNTS.md 5.1 (review of S6, finding 4): when the session's account leaves the console, another account is
    /// at the keyboard: every shell of the session ends, the administrator terminal included.
    fn on_state(&self, ctx: &SessionCtx, desktop: swapdesk_proto::Desktop, _paused: bool) {
        if desktop == swapdesk_proto::Desktop::AccountAway {
            self.on_permissions(ctx, PermissionSet::EMPTY, PermissionSet::EMPTY);
        }
    }

    fn on_end(&self, ctx: &SessionCtx, _end: &SessionEnd) {
        let Some(run) = lock(&self.0.sessions).remove(&ctx.session_id()) else { return };
        // Every shell of the session ends with it; killing one may wait for the helper's pipe, so not on this task.
        tokio::task::spawn_blocking(move || {
            lock(&run.sinks).clear();
            lock(&run.host).close_all();
        });
    }
}

// ---------- the viewer's side ----------

type OpenResult = Result<String, ErrorCode>;

/// A terminal the viewer has open: where its bytes go and where its exit code arrives.
struct Live {
    input: mpsc::Sender<Vec<u8>>,
    exit: watch::Sender<Option<Option<i32>>>,
}

#[derive(Default)]
struct ViewerState {
    opening: HashMap<u32, oneshot::Sender<OpenResult>>,
    live: HashMap<u32, Live>,
}

/// The viewer's terminal handler (registered in every viewer registry): it opens terminals on the host, relays their
/// bytes to the UI as [`ViewerOut::Term`] and tells it when one ends.
pub(crate) struct ViewerTerms {
    out: mpsc::Sender<ViewerOut>,
    rt: Handle,
    state: Mutex<ViewerState>,
}

impl ViewerTerms {
    pub fn new(out: mpsc::Sender<ViewerOut>, rt: Handle) -> Arc<Self> {
        Arc::new(Self { out, rt, state: Mutex::new(ViewerState::default()) })
    }

    /// The host refused the open of `term_id` (a non-fatal `Error` about the `TermOpen`).
    pub fn refused(&self, term_id: u32, code: ErrorCode) {
        if let Some(tx) = lock(&self.state).opening.remove(&term_id) {
            let _ = tx.send(Err(code));
        }
    }

    /// Asks the host for terminal `term_id` and opens its stream; resolves with the account the shell runs as.
    pub async fn open(
        self: &Arc<Self>,
        ctx: &SessionCtx,
        term_id: u32,
        cols: u16,
        rows: u16,
        admin: bool,
    ) -> OpenResult {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = lock(&self.state);
            if st.opening.contains_key(&term_id) || st.live.contains_key(&term_id) {
                return Err(ErrorCode::ProtocolViolation);
            }
            st.opening.insert(term_id, tx);
        }
        let forget = |me: &Self| {
            lock(&me.state).opening.remove(&term_id);
        };
        if ctx.send(TermOpen { term_id, cols, rows, admin }).await.is_err() {
            forget(self);
            return Err(ErrorCode::Unavailable);
        }
        let account = match tokio::time::timeout(OPEN_WAIT, rx).await {
            Ok(Ok(Ok(account))) => account,
            Ok(Ok(Err(code))) => return Err(code),
            // The sender went away: the session ended (or the open was dropped by `on_end`).
            Ok(Err(_)) => return Err(ErrorCode::Unavailable),
            Err(_) => {
                forget(self);
                let _ = ctx.try_send(TermClose { term_id });
                return Err(ErrorCode::Timeout);
            }
        };
        let (mut send, recv) = match ctx.open_bi(StreamKind::Terminal).await {
            Ok(s) => s,
            Err(code) => {
                let _ = ctx.try_send(TermClose { term_id });
                return Err(code);
            }
        };
        if send.write_all(&term_header(term_id)).await.is_err() {
            let _ = ctx.try_send(TermClose { term_id });
            return Err(ErrorCode::Unavailable);
        }
        let (input_tx, input_rx) = mpsc::channel(VIEWER_INPUT_CHUNKS);
        let (exit_tx, exit_rx) = watch::channel(None);
        lock(&self.state).live.insert(term_id, Live { input: input_tx, exit: exit_tx });
        self.rt.spawn(write_in(input_rx, send));
        self.rt.spawn(read_out(Arc::clone(self), term_id, recv, exit_rx));
        Ok(account)
    }

    /// Keystrokes for a terminal: never waits; a full queue loses them (the host is not reading).
    pub fn input(&self, term_id: u32, bytes: Vec<u8>) -> bool {
        lock(&self.state).live.get(&term_id).is_some_and(|l| l.input.try_send(bytes).is_ok())
    }

    pub fn resize(&self, ctx: &SessionCtx, term_id: u32, cols: u16, rows: u16) -> bool {
        lock(&self.state).live.contains_key(&term_id) && ctx.try_send(TermResize { term_id, cols, rows }).is_ok()
    }

    /// The UI closes a terminal: the host ends the shell, and nothing more is reported for it.
    pub fn close(&self, ctx: &SessionCtx, term_id: u32) -> bool {
        let live = lock(&self.state).live.remove(&term_id).is_some();
        if live {
            let _ = ctx.try_send(TermClose { term_id });
        }
        live
    }
}

/// The UI's bytes onto the terminal stream.
async fn write_in(mut rx: mpsc::Receiver<Vec<u8>>, mut send: quinn::SendStream) {
    while let Some(bytes) = rx.recv().await {
        if send.write_all(&bytes).await.is_err() {
            return;
        }
    }
    let _ = send.finish();
}

/// The shell's bytes to the UI, waiting for room (the UI queue's back-pressure reaches the host's PTY through QUIC
/// flow control); at the end of the stream, the exit code, and `Exited` unless the UI closed the terminal first.
async fn read_out(
    terms: Arc<ViewerTerms>,
    term_id: u32,
    mut recv: quinn::RecvStream,
    mut exit: watch::Receiver<Option<Option<i32>>>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(Some(n)) = recv.read(&mut buf).await {
        let msg = ViewerOut::Term(TermOut::Output { term_id, bytes: Bytes(buf[..n].to_vec()) });
        if terms.out.send(msg).await.is_err() {
            return;
        }
    }
    // The stream ended: the host's `TermExited` follows it (or the session ended and `on_end` reports it).
    let code = match tokio::time::timeout(FLUSH_WAIT, exit.wait_for(|v| v.is_some())).await {
        Ok(Ok(v)) => *v,
        _ => None,
    };
    let still_open = lock(&terms.state).live.remove(&term_id).is_some();
    if still_open {
        let _ = terms.out.send(ViewerOut::Term(TermOut::Exited { term_id, code: code.flatten() })).await;
    }
}

impl ChannelHandler for ViewerTerms {
    fn name(&self) -> &'static str {
        "viewer-terminal"
    }

    fn messages(&self) -> &'static [u16] {
        &[0x0502, 0x0505]
    }

    fn on_message(&self, _ctx: &SessionCtx, msg: Message) {
        match msg {
            Message::TermOpened(TermOpened { term_id, account }) => {
                if let Some(tx) = lock(&self.state).opening.remove(&term_id) {
                    let _ = tx.send(Ok(account));
                }
            }
            Message::TermExited(TermExited { term_id, exit_code }) => {
                let st = lock(&self.state);
                if let Some(live) = st.live.get(&term_id) {
                    live.exit.send_replace(Some(exit_code));
                    return;
                }
                drop(st);
                // The host ended a terminal whose stream never opened: nothing else will say so.
                let out = self.out.clone();
                self.rt.spawn(async move {
                    let _ = out.send(ViewerOut::Term(TermOut::Exited { term_id, code: exit_code })).await;
                });
            }
            _ => {}
        }
    }

    fn on_end(&self, _ctx: &SessionCtx, _end: &SessionEnd) {
        let (opening, live) = {
            let mut st = lock(&self.state);
            (std::mem::take(&mut st.opening), std::mem::take(&mut st.live))
        };
        for (_, tx) in opening {
            let _ = tx.send(Err(ErrorCode::Unavailable));
        }
        // Terminals do not survive their connection: the UI is told each one ended (a resumed session starts clean).
        let out = self.out.clone();
        self.rt.spawn(async move {
            for term_id in live.keys() {
                let _ = out.send(ViewerOut::Term(TermOut::Exited { term_id: *term_id, code: None })).await;
            }
        });
    }
}

#[cfg(test)]
mod cmd_queue_tests {
    use super::*;

    #[test]
    fn a_flood_of_resizes_costs_one_place_per_terminal_and_the_last_size_wins() {
        let (q, mut rx) = CmdQueue::new();
        for i in 0..10_000u16 {
            q.resize(1, 80 + i % 100, 24);
        }
        q.resize(2, 100, 40);
        q.resize(1, 132, 43);
        assert_eq!(rx.len(), 2, "one marker per terminal, however many resizes came");
        assert!(matches!(rx.try_recv(), Ok(Cmd::Resize { term_id: 1 })));
        assert_eq!(q.take_size(1), Some((132, 43)), "the newest size of terminal 1");
        assert!(matches!(rx.try_recv(), Ok(Cmd::Resize { term_id: 2 })));
        assert_eq!(q.take_size(2), Some((100, 40)));
        assert_eq!(q.take_size(1), None, "taken once");
        // After the marker was taken a new resize queues a new one.
        q.resize(1, 90, 30);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Resize { term_id: 1 })));
    }

    #[test]
    fn resizes_racing_a_consumer_over_a_nearly_full_queue_never_leave_an_entry_without_its_marker() {
        let (q, mut rx) = CmdQueue::new();
        let q = Arc::new(q);
        // All but a few places taken, so the queue flips between full and not while the consumer drains.
        for _ in 0..CMD_QUEUE - 2 {
            q.close(9);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..4u16)
            .map(|w| {
                let q = Arc::clone(&q);
                std::thread::spawn(move || {
                    for i in 0..3000u16 {
                        q.resize(1, 80 + w, 24 + i % 7);
                        q.resize(2, 90 + w, 30);
                    }
                })
            })
            .collect();
        let consumer = {
            let (q, stop) = (Arc::clone(&q), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut seen = 0;
                while !stop.load(Ordering::SeqCst) {
                    if let Ok(Cmd::Resize { term_id }) = rx.try_recv() {
                        seen += usize::from(q.take_size(term_id).is_some());
                    }
                }
                (rx, seen)
            })
        };
        for w in workers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::SeqCst);
        let (mut rx, seen) = consumer.join().unwrap();
        assert!(seen > 0, "the consumer applied sizes while the writers ran");
        let mut markers = 0;
        while let Ok(cmd) = rx.try_recv() {
            markers += usize::from(matches!(cmd, Cmd::Resize { .. }));
        }
        let entries = lock(&q.sizes).len();
        assert!(
            entries <= markers,
            "{entries} sizes recorded but only {markers} markers queued: a size nobody will apply"
        );
    }

    #[test]
    fn closes_are_bounded_and_a_resize_that_finds_no_room_leaves_no_trace() {
        let (q, mut rx) = CmdQueue::new();
        for _ in 0..CMD_QUEUE * 3 {
            q.close(1);
        }
        assert_eq!(rx.len(), CMD_QUEUE, "the queue stops at its bound");
        // Full: the resize is dropped and its size is not left behind (the map is bounded by the queue too).
        q.resize(9, 80, 24);
        assert_eq!(q.take_size(9), None);
        // Room again: a resize queues as usual.
        while rx.try_recv().is_ok() {}
        q.resize(9, 80, 24);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Resize { term_id: 9 })));
        assert_eq!(q.take_size(9), Some((80, 24)));
    }
}
