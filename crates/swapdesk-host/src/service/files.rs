//! Files in the service (stage 44): the `FilesHandler` of `swapdesk-files` plugged into the registries of both roles,
//! with every disk access going through a [`PipeFs`] to the process that owns the user's files.
//!
//! The service never opens a path a peer sent (D6, C2.3): on the host side the disk is the user helper's (the helper
//! picks the root, its home, and re-checks every path), on the viewer side it is the UI's (it serves only the files the
//! user picked and the folders the user chose). The handlers here are blocking-free: the engine's disk calls run in
//! `spawn_blocking` inside `FilesHandler` and wait for the pipe's answer there, never on an executor thread.
//!
//! * **Host role** ([`FilesGate`]): D13. In an unattended session the files of a user other than the one who granted
//!   access ask that user first (`request_consent(Feature::Files)`); a prompted session had its user's say already.
//!   Until the question is answered the peer's file messages wait (a bounded queue); a refusal answers each of them
//!   with its decline code and, for a "no", stays for the rest of the session.
//! * **Viewer role** ([`ViewerFiles`]): one handler per viewer session, bound to the UI connection that owns the
//!   session after the connection opens; the hub's UI ops drive it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use swapdesk_files::{Command, DEFAULT_RECEIVE, Event, FilesHandler, FilesSink, Fs, FsCaller, PipeFs};
use swapdesk_proto::{ErrorCode, ErrorMessage, Feature, FileDecline, Message, PermissionSet, StreamKind};
use swapdesk_session::{ChannelHandler, IncomingStream, SessionCtx, SessionEnd};
use swapdesk_term::{ConsentFacts, ConsentNeed, consent_need};
use swapdesk_trust::{DeviceId, TrustHandle};

use super::extras::HooksCell;
use super::routing::HelperKind;
use super::shared::Shared;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// File messages that wait for the D13 answer, per session: more are dropped (the sender is told nothing, like any
/// message past a flood).
const MAX_WAITING: usize = 64;

/// What the D13 decision reads from the machine.
pub(crate) trait ConsentEnv: Send + Sync + 'static {
    /// The account (SID) of the administrator who granted `UNATTENDED` to the device.
    fn granted_by(&self, device: &DeviceId) -> Option<String>;
    /// The account (SID) of the user logged in at the console now.
    fn console_user(&self) -> Option<String>;
}

/// The machine's file service: the host gate and the way events reach the UI.
pub(crate) struct FilesService {
    hooks: Arc<HooksCell>,
    trust: Arc<TrustHandle>,
    shared: Arc<Shared>,
    /// The host session the handler serves now, for the events it raises (a host serves one session at a time).
    host_session: Arc<AtomicU64>,
    gate: OnceLock<Arc<FilesGate>>,
}

impl FilesService {
    pub fn new(hooks: Arc<HooksCell>, trust: Arc<TrustHandle>, shared: Arc<Shared>) -> Arc<Self> {
        let me =
            Arc::new(Self { hooks, trust, shared, host_session: Arc::new(AtomicU64::new(0)), gate: OnceLock::new() });
        let gate = FilesGate::new(
            me.host_fs(),
            Arc::new(HostSink { session: Arc::clone(&me.host_session), hooks: Arc::clone(&me.hooks) }),
            Arc::clone(&me) as Arc<dyn ConsentEnv>,
            Arc::clone(&me.host_session),
        );
        let _ = me.gate.set(gate);
        me
    }

    /// The host role's handler, for the registry.
    pub fn host_handler(&self) -> Arc<dyn ChannelHandler> {
        match self.gate.get() {
            Some(g) => g.clone(),
            None => unreachable!("the gate is set in `new`"),
        }
    }

    /// The user helper's disk, looked up at each call (the helper may restart).
    fn host_fs(&self) -> Arc<dyn Fs> {
        let shared = Arc::clone(&self.shared);
        Arc::new(PipeFs::new(Arc::new(move || shared.helper(HelperKind::User).map(|h| h.fs_caller()))))
    }

    /// A handler for the viewer session about to open.
    pub fn viewer(&self) -> Arc<ViewerFiles> {
        ViewerFiles::new(Arc::clone(&self.hooks))
    }
}

impl ConsentEnv for FilesService {
    fn granted_by(&self, device: &DeviceId) -> Option<String> {
        self.trust.read(|s| s.find_by_id(device).and_then(|d| d.unattended_granted_by.as_ref().map(|u| u.id.clone())))
    }

    fn console_user(&self) -> Option<String> {
        self.hooks.get().and_then(|h| h.console_user())
    }
}

// ---------- events ----------

/// Host events: the session is the one the gate serves now.
struct HostSink {
    session: Arc<AtomicU64>,
    hooks: Arc<HooksCell>,
}

impl FilesSink for HostSink {
    fn event(&self, event: Event) {
        if let Some(h) = self.hooks.get() {
            h.files(self.session.load(Ordering::SeqCst), event);
        }
    }
}

struct ViewerSink {
    session: Arc<AtomicU64>,
    hooks: Arc<HooksCell>,
}

impl FilesSink for ViewerSink {
    fn event(&self, event: Event) {
        if let Some(h) = self.hooks.get() {
            h.files(self.session.load(Ordering::SeqCst), event);
        }
    }
}

// ---------- host role: the D13 gate ----------

enum Verdict {
    /// Not asked yet in this session.
    Unknown,
    /// The question is out; what the peer sent meanwhile waits here.
    Asking(Vec<Message>),
    Allowed,
    /// "No" from the user: for the rest of the session.
    Refused(ErrorCode),
}

struct GateState {
    session_id: u64,
    verdict: Verdict,
}

struct GateInner {
    handler: Arc<FilesHandler>,
    env: Arc<dyn ConsentEnv>,
    state: Mutex<GateState>,
    session: Arc<AtomicU64>,
}

/// The host's files handler behind the D13 step.
pub(crate) struct FilesGate(Arc<GateInner>);

impl FilesGate {
    fn new(fs: Arc<dyn Fs>, sink: Arc<dyn FilesSink>, env: Arc<dyn ConsentEnv>, session: Arc<AtomicU64>) -> Arc<Self> {
        let handler = FilesHandler::with_fs(
            fs,
            PathBuf::new(),
            Some(PathBuf::from(DEFAULT_RECEIVE)),
            swapdesk_files::DEFAULT_MAX_FILE_SIZE,
            sink,
        );
        Arc::new(Self(Arc::new(GateInner {
            handler,
            env,
            state: Mutex::new(GateState { session_id: 0, verdict: Verdict::Unknown }),
            session,
        })))
    }
}

/// What the viewer is told for a message that is refused: an offer is declined, a request fails.
fn refuse(ctx: &SessionCtx, msg: &Message, code: ErrorCode) {
    let out: Message = match msg {
        Message::FileOffer(m) => FileDecline { transfer_id: m.transfer_id, code }.into(),
        Message::FileGet(m) => ErrorMessage { code, fatal: false, about_type: 0x040B, about_id: m.transfer_id }.into(),
        Message::DirRequest(m) => ErrorMessage { code, fatal: false, about_type: 0x0409, about_id: m.req_id }.into(),
        _ => return,
    };
    let sender = ctx.clone();
    ctx.spawn(async move {
        let _ = sender.send(out).await;
    });
}

async fn decide(env: &dyn ConsentEnv, ctx: &SessionCtx) -> Result<(), ErrorCode> {
    let granted_by = ctx.device().and_then(|d| env.granted_by(&d));
    let console_user = env.console_user();
    let facts = ConsentFacts {
        // How the session entered, kept by the session itself across a resume: a table of this service's own lost
        // it there (a resume starts the session again with the mode `Resumed`).
        session_unattended: ctx.entered_unattended(),
        granted_by: granted_by.as_deref(),
        console_user: console_user.as_deref(),
        admin_terminal: false,
    };
    match consent_need(&facts) {
        ConsentNeed::NotNeeded => Ok(()),
        ConsentNeed::Refuse(code) => Err(code),
        ConsentNeed::Ask => ctx.request_consent(Feature::Files).await,
    }
}

impl ChannelHandler for FilesGate {
    fn name(&self) -> &'static str {
        "files"
    }

    fn messages(&self) -> &'static [u16] {
        self.0.handler.messages()
    }

    fn streams(&self) -> &'static [StreamKind] {
        self.0.handler.streams()
    }

    fn on_start(&self, ctx: &SessionCtx) {
        self.0.session.store(ctx.session_id(), Ordering::SeqCst);
        let mut st = lock(&self.0.state);
        // The same session started again (a resume) keeps the user's answer; a new one asks again.
        if st.session_id != ctx.session_id() {
            *st = GateState { session_id: ctx.session_id(), verdict: Verdict::Unknown };
        }
        drop(st);
        self.0.handler.on_start(ctx);
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        let mut st = lock(&self.0.state);
        match &mut st.verdict {
            Verdict::Allowed => {
                drop(st);
                self.0.handler.on_message(ctx, msg);
            }
            Verdict::Refused(code) => {
                let code = *code;
                drop(st);
                refuse(ctx, &msg, code);
            }
            Verdict::Asking(queue) => {
                if queue.len() < MAX_WAITING {
                    queue.push(msg);
                }
            }
            Verdict::Unknown => {
                st.verdict = Verdict::Asking(vec![msg]);
                drop(st);
                let inner = Arc::clone(&self.0);
                let task_ctx = ctx.clone();
                ctx.spawn(async move {
                    let answer = decide(inner.env.as_ref(), &task_ctx).await;
                    let (queue, verdict) = {
                        let mut st = lock(&inner.state);
                        let queue = match std::mem::replace(&mut st.verdict, Verdict::Unknown) {
                            Verdict::Asking(q) => q,
                            // The session was restarted meanwhile: this answer belongs to an older one.
                            other => {
                                st.verdict = other;
                                return;
                            }
                        };
                        st.verdict = match answer {
                            Ok(()) => Verdict::Allowed,
                            Err(ErrorCode::Declined) => Verdict::Refused(ErrorCode::Declined),
                            // Nobody there, or no answer in time: asked again with the next request.
                            Err(_) => Verdict::Unknown,
                        };
                        (queue, answer)
                    };
                    match verdict {
                        Ok(()) => {
                            for m in queue {
                                inner.handler.on_message(&task_ctx, m);
                            }
                        }
                        Err(code) => {
                            for m in &queue {
                                refuse(&task_ctx, m, code);
                            }
                        }
                    }
                });
            }
        }
    }

    fn on_stream(&self, ctx: &SessionCtx, stream: IncomingStream) {
        // A data stream follows an accepted offer, which only an allowed session has.
        if matches!(lock(&self.0.state).verdict, Verdict::Allowed) {
            self.0.handler.on_stream(ctx, stream);
        }
    }

    fn on_permissions(&self, ctx: &SessionCtx, old: PermissionSet, now: PermissionSet) {
        self.0.handler.on_permissions(ctx, old, now);
    }

    fn on_end(&self, ctx: &SessionCtx, end: &SessionEnd) {
        self.0.handler.on_end(ctx, end);
    }
}

// ---------- viewer role ----------

/// One viewer session's files: its handler, and the disk of the UI that owns the session.
pub struct ViewerFiles {
    handler: Arc<FilesHandler>,
    caller: Arc<Mutex<Option<Arc<dyn FsCaller>>>>,
    session: Arc<AtomicU64>,
}

impl ViewerFiles {
    fn new(hooks: Arc<HooksCell>) -> Arc<Self> {
        let session = Arc::new(AtomicU64::new(0));
        let caller: Arc<Mutex<Option<Arc<dyn FsCaller>>>> = Arc::new(Mutex::new(None));
        let source = Arc::clone(&caller);
        let fs: Arc<dyn Fs> = Arc::new(PipeFs::new(Arc::new(move || lock(&source).clone())));
        // The viewer has no folder of its own to list or default to: receive folders and picked files are the UI's
        // tokens, named per transfer.
        let handler = FilesHandler::with_fs(
            fs,
            PathBuf::new(),
            None,
            swapdesk_files::DEFAULT_MAX_FILE_SIZE,
            Arc::new(ViewerSink { session: Arc::clone(&session), hooks }),
        );
        Arc::new(Self { handler, caller, session })
    }

    /// For the viewer session's registry.
    pub(crate) fn channel(&self) -> Arc<dyn ChannelHandler> {
        self.handler.clone()
    }

    pub(crate) fn set_session(&self, session_id: u64) {
        self.session.store(session_id, Ordering::SeqCst);
    }

    /// The UI connection that owns the session now serves its disk.
    pub fn bind(&self, caller: Arc<dyn FsCaller>) {
        *lock(&self.caller) = Some(caller);
    }

    /// Runs a command in the live session; false when the session is between two connections.
    pub fn command(&self, cmd: Command) -> bool {
        self.handler.command(cmd)
    }

    /// Does the live connection's engine hold transfer `id`?
    pub fn knows(&self, id: u32) -> bool {
        self.handler.knows(id)
    }

    /// Transfers the live connection's engine holds.
    pub fn active(&self) -> usize {
        self.handler.active()
    }
}
