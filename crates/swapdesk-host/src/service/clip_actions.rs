//! Clipboard sync and quick actions in the service (stage 42).
//!
//! The service owns the LAN sockets, so the handlers live in its registries; the OS work does not (C2.3): the
//! clipboard of the host role is read and written by the user helper, the clipboard of the viewer role by the UI that
//! owns the viewer session, both through [`PipeClipboard`]. Quick actions run in the user helper when one is attached
//! and in the service's own runner when not (lock, sleep and restart only: the runner refuses what needs a user).
//!
//! * [`ClipboardHandler`] (`swapdesk-extras`) is registered once in the host registry and once per viewer session.
//! * [`ActionsHost`] answers the viewer's `QuickAction` on the host; [`ActionWaiters`] hands the host's
//!   `QuickActionResult` to the UI op that asked on the viewer.
//! * [`ClipActionHooks`] is what the interface layer (the UI hub) gives the service: the pipe of the UI that serves a
//!   viewer session's clipboard, the `ClipboardSkipped` notices and the audit of an action that ran.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use swapdesk_extras::clipboard::ClipboardHandler;
use swapdesk_extras::{ExtrasEvent, ExtrasSink};
use swapdesk_platform::{ActionRunner, ErrorKind, PlatformError};
use swapdesk_proto::{Action, ClipSkip, ErrorCode, Message, PermissionSet};
use swapdesk_session::{ChannelHandler, HostService, HostUi, SessionCtx};
use swapdesk_term::{ActionContext, ActionRelay};
use tokio::sync::oneshot;

use super::routing::HelperKind;
use super::shared::Shared;
use crate::proxy::{PipeActions, PipeClipboard, RpcSource};
use crate::rpc::Rpc;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the interface layer gives the service for the clipboard and quick actions. Every method may be called from a
/// thread that blocks or from a task: none may block for long.
pub trait ClipActionHooks: Send + Sync + 'static {
    /// The pipe of the UI that serves the clipboard of viewer session `session_id` (its UI connection), if it is
    /// connected.
    fn ui_clipboard(&self, session_id: u64) -> Option<Arc<Rpc>>;

    /// An item of the clipboard was not synced in session `session_id`, here or at the peer.
    fn clipboard_skipped(&self, session_id: u64, reason: ClipSkip);

    /// A quick action `action` ran on this host for session `session_id` (the audit log's `ActionRun`).
    fn action_ran(&self, session_id: u64, action: Action);
}

/// The hooks, set once after the core starts.
#[derive(Default)]
pub(crate) struct ClipHooksCell(OnceLock<Arc<dyn ClipActionHooks>>);

impl ClipHooksCell {
    pub fn set(&self, hooks: Arc<dyn ClipActionHooks>) -> bool {
        self.0.set(hooks).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<dyn ClipActionHooks>> {
        self.0.get()
    }
}

// ---------- clipboard ----------

/// The user helper's pipe, looked up at each call (a helper comes and goes).
struct UserHelperRpc {
    shared: Arc<Shared>,
}

impl RpcSource for UserHelperRpc {
    fn rpc(&self) -> Option<Arc<Rpc>> {
        self.shared.helper(HelperKind::User).map(|h| h.rpc())
    }
}

/// The pipe of the UI that owns viewer session `session` (0 until the host granted it: no pipe yet).
struct ViewerUiRpc {
    session: Arc<AtomicU64>,
    hooks: Arc<ClipHooksCell>,
}

impl RpcSource for ViewerUiRpc {
    fn rpc(&self) -> Option<Arc<Rpc>> {
        let session = self.session.load(Ordering::Acquire);
        if session == 0 {
            return None;
        }
        self.hooks.get()?.ui_clipboard(session)
    }
}

/// Which session a clipboard notice belongs to.
enum Whose {
    /// A viewer session: its own handler, whose id is filled in when the session is granted.
    Viewer(Arc<AtomicU64>),
    /// The host role: one clipboard serves the one live session of the machine.
    Host(Arc<OnceLock<HostService>>),
}

struct ClipSink {
    whose: Whose,
    hooks: Arc<ClipHooksCell>,
}

impl ExtrasSink for ClipSink {
    fn event(&self, event: ExtrasEvent) {
        let ExtrasEvent::ClipboardSkipped { reason, .. } = event else { return };
        let session_id = match &self.whose {
            Whose::Viewer(id) => id.load(Ordering::Acquire),
            Whose::Host(host) => host.get().and_then(|h| h.sessions().first().map(|s| s.session_id)).unwrap_or(0),
        };
        if let (Some(hooks), true) = (self.hooks.get(), session_id != 0) {
            hooks.clipboard_skipped(session_id, reason);
        }
    }
}

/// The clipboard feature of the host role: the user helper's clipboard behind the handler.
pub(crate) fn host_clipboard(
    shared: &Arc<Shared>,
    hooks: &Arc<ClipHooksCell>,
    host: &Arc<OnceLock<HostService>>,
) -> Arc<ClipboardHandler<PipeClipboard>> {
    let provider = PipeClipboard::new(Arc::new(UserHelperRpc { shared: Arc::clone(shared) }));
    let sink = Arc::new(ClipSink { whose: Whose::Host(Arc::clone(host)), hooks: Arc::clone(hooks) });
    ClipboardHandler::new(provider, true, sink)
}

/// The clipboard feature of one viewer session: the clipboard of the UI that owns it. `session` is set to the
/// session's id once the host granted it.
pub(crate) fn viewer_clipboard(hooks: &Arc<ClipHooksCell>) -> (Arc<ClipboardHandler<PipeClipboard>>, Arc<AtomicU64>) {
    let session = Arc::new(AtomicU64::new(0));
    let provider =
        PipeClipboard::new(Arc::new(ViewerUiRpc { session: Arc::clone(&session), hooks: Arc::clone(hooks) }));
    let sink = Arc::new(ClipSink { whose: Whose::Viewer(Arc::clone(&session)), hooks: Arc::clone(hooks) });
    (ClipboardHandler::new(provider, true, sink), session)
}

// ---------- quick actions ----------

/// Runs an action in the user helper when one is attached, else in the service's own runner.
pub(crate) struct RoutedRunner {
    user: PipeActions,
    fallback: Option<Box<dyn ActionRunner + Send>>,
}

impl RoutedRunner {
    pub fn new(shared: &Arc<Shared>, fallback: Option<Box<dyn ActionRunner + Send>>) -> Self {
        Self { user: PipeActions::new(Arc::new(UserHelperRpc { shared: Arc::clone(shared) })), fallback }
    }

    fn none(op: &'static str) -> PlatformError {
        PlatformError {
            kind: ErrorKind::Unavailable,
            op,
            os_code: None,
            detail: "neither a user helper nor a service runner is available".into(),
        }
    }
}

impl ActionRunner for RoutedRunner {
    fn run(&mut self, action: Action) -> swapdesk_platform::Result<()> {
        if self.user.available() {
            return self.user.run(action);
        }
        match self.fallback.as_mut() {
            Some(r) => r.run(action),
            None => Err(Self::none("quick action")),
        }
    }

    fn keep_awake(&mut self, on: bool) -> swapdesk_platform::Result<()> {
        if self.user.available() {
            return self.user.keep_awake(on);
        }
        match self.fallback.as_mut() {
            Some(r) => r.keep_awake(on),
            None => Err(Self::none("keep awake")),
        }
    }
}

struct ActionsInner {
    relay: Mutex<ActionRelay<RoutedRunner>>,
    ui: Arc<dyn HostUi>,
    shared: Arc<Shared>,
    hooks: Arc<ClipHooksCell>,
}

/// The host's side of `QuickAction` (0x0601): `CONTROL` is required, the runner runs off the executor, the answer goes
/// back as `QuickActionResult` and an action that ran is audited.
pub(crate) struct ActionsHost {
    inner: Arc<ActionsInner>,
}

impl ActionsHost {
    pub fn new(
        shared: &Arc<Shared>,
        ui: Arc<dyn HostUi>,
        hooks: &Arc<ClipHooksCell>,
        fallback: Option<Box<dyn ActionRunner + Send>>,
    ) -> Arc<Self> {
        let relay = ActionRelay::new(RoutedRunner::new(shared, fallback));
        Arc::new(Self {
            inner: Arc::new(ActionsInner {
                relay: Mutex::new(relay),
                ui,
                shared: Arc::clone(shared),
                hooks: Arc::clone(hooks),
            }),
        })
    }
}

impl ChannelHandler for ActionsHost {
    fn name(&self) -> &'static str {
        "quick-actions"
    }

    fn messages(&self) -> &'static [u16] {
        &[0x0601]
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        let Message::QuickAction(req) = msg else { return };
        let (inner, ctx) = (Arc::clone(&self.inner), ctx.clone());
        // The runner may wait for the helper's pipe (a suspend takes seconds): a blocking thread, never the executor.
        drop(self.inner.shared.rt.spawn_blocking(move || {
            let facts = ActionContext { granted: ctx.granted(), user_present: inner.ui.somebody_present() };
            let done = lock(&inner.relay).handle(&req, &facts);
            let ok = done.result.code == ErrorCode::NoError;
            let _ = ctx.try_send(done.result);
            if let (true, Some(h)) = (done.ran && ok, inner.hooks.get()) {
                h.action_ran(ctx.session_id(), done.action);
            }
        }));
    }
}

/// The viewer's side of `QuickActionResult` (0x0602): hands each result to the call that sent the request.
#[derive(Default)]
pub(crate) struct ActionWaiters {
    next: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<ErrorCode>>>,
}

impl ActionWaiters {
    /// A new request number and the receiver of its result.
    pub fn register(&self) -> (u32, oneshot::Receiver<ErrorCode>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        (id, rx)
    }

    pub fn forget(&self, id: u32) {
        lock(&self.pending).remove(&id);
    }
}

impl ChannelHandler for ActionWaiters {
    fn name(&self) -> &'static str {
        "quick-action-results"
    }

    fn messages(&self) -> &'static [u16] {
        &[0x0602]
    }

    fn on_message(&self, _ctx: &SessionCtx, msg: Message) {
        if let Message::QuickActionResult(r) = msg
            && let Some(tx) = lock(&self.pending).remove(&r.action_id)
        {
            let _ = tx.send(r.code);
        }
    }

    fn on_end(&self, _ctx: &SessionCtx, _end: &swapdesk_session::SessionEnd) {
        // Nobody will answer now: the waiters see the channel close.
        lock(&self.pending).clear();
    }
}

/// `PermissionSet::CONTROL` is what the viewer needs before it asks.
pub(crate) fn may_run(granted: PermissionSet) -> bool {
    granted.contains(PermissionSet::CONTROL)
}
