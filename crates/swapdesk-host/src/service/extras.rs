//! Swap and chat in the service (stage 41): the handlers of `swapdesk-swap` and `swapdesk-extras` plugged into the
//! registries of both roles, and the [`ExtrasHooks`] the interface layer implements to hear about them.
//!
//! The service owns every LAN socket (D6), so these handlers live in the host registry and in every viewer registry.
//! They need nothing from the user's files, clipboard or shell: what they need from the machine is the
//! [`SwapEnv`], answered here from the trust store and the host's user (`HostUi`), and the UI card, answered by the
//! hooks. The hooks are set after the core starts (the UI hub is built before the core and binds to it after), so
//! until then a swap question is declined and chat events are dropped.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use swapdesk_extras::chat::{ChatEntry, ChatError, ChatHandler};
use swapdesk_extras::{ExtrasEvent, ExtrasSink};
use swapdesk_platform::Level;
use swapdesk_proto::{Intent, Message, PermissionSet};
use swapdesk_session::{ChannelHandler, HostService, HostUi, SessionCtx, SessionEnd};
use swapdesk_swap::{AskEnd, SwapEnv, SwapPrompt};
use swapdesk_trust::{DeviceId, TrustHandle};
use tokio::sync::oneshot;

use super::Connector;
use super::shared::Shared;
use super::viewer::{ConnectError, ViewerHandle, ViewerMessages};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a chat session tells the interface. Text only ever travels here, never into a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatNote {
    /// A message from the peer arrived.
    Received(ChatEntry),
    /// The peer read this side's messages up to `up_to`.
    ReadByPeer { up_to: u32 },
}

/// What the interface layer (the UI hub) gives the service for swap and chat. Every method runs on a session task
/// or on a runtime task: none may block.
pub trait ExtrasHooks: Send + Sync + 'static {
    /// The swap question for this machine's user ("pediu para inverter", 20 s). Dropping the sender without an answer
    /// says nobody was there to ask (the asker hears `NOBODY_PRESENT`).
    fn swap_ask(&self, prompt: SwapPrompt) -> oneshot::Receiver<bool>;

    /// The question is no longer waiting: close its card.
    fn swap_ask_closed(&self, request_id: u32, how: AskEnd);

    /// Where to dial `device` for the new connection of a swap (its last known session address).
    fn address_of(&self, device: &DeviceId) -> Option<SocketAddr>;

    /// The new viewer session of a swap (this machine was the host and now views `device`), or why it did not open
    /// within the swap token's life. The hook hands it to the viewer's UI path.
    fn swap_viewing(&self, device: DeviceId, opened: Result<(ViewerHandle, ViewerMessages), ConnectError>);

    /// This machine armed a swap token for `device` (it was to become the host) and no connection used it in its
    /// life: the swap's new connection never came, and the other side cannot say so (the old session is gone). The
    /// default does nothing; the UI hub tells its user.
    fn swap_never_came(&self, _device: DeviceId) {}

    /// A chat event of session `session_id`.
    fn chat(&self, session_id: u64, note: ChatNote);

    /// A terminal of the peer `device` opened on this machine (`admin`: the administrator terminal): the audit log
    /// gets `TerminalOpened` (F-SESS-04, F-TERM-02). The default writes nothing.
    fn terminal_opened(&self, _device: DeviceId, _admin: bool) {}

    /// A transfer or listing event of the files feature in session `session_id` (stage 44). Runs on a blocking
    /// thread or a session task: it must not block.
    fn files(&self, _session_id: u64, _event: swapdesk_files::Event) {}

    /// The account (SID) of the user logged in at the console, for the D13 step of the files feature; `None` when
    /// nobody is (or this hook does not know).
    fn console_user(&self) -> Option<String> {
        None
    }
}

/// The hooks, set once after the core starts; shared by the swap environment and every chat router.
#[derive(Default)]
pub(crate) struct HooksCell(OnceLock<Arc<dyn ExtrasHooks>>);

impl HooksCell {
    pub fn set(&self, hooks: Arc<dyn ExtrasHooks>) -> bool {
        self.0.set(hooks).is_ok()
    }

    pub fn get(&self) -> Option<&Arc<dyn ExtrasHooks>> {
        self.0.get()
    }
}

/// What the swap handler asks of this machine (PROTOCOL.md 6.3), answered from the trust store, the host's user and
/// the hooks.
pub(crate) struct CoreEnv {
    pub ui: Arc<dyn HostUi>,
    pub trust: Arc<TrustHandle>,
    pub host: OnceLock<HostService>,
    pub hooks: Arc<HooksCell>,
    pub connector: OnceLock<Weak<Connector>>,
    pub shared: Arc<Shared>,
}

impl SwapEnv for CoreEnv {
    fn somebody_present(&self) -> bool {
        self.ui.somebody_present()
    }

    fn control_granted_to(&self, peer: &DeviceId) -> bool {
        self.trust.read(|s| s.find_by_id(peer).is_some_and(|d| d.grants_here.contains(PermissionSet::CONTROL)))
    }

    /// A phone does not host until it can (docs/ANDROID.md, slice A16): no swap makes one the host.
    fn peer_can_host(&self, peer: &DeviceId) -> bool {
        self.trust.read(|s| s.find_by_id(peer).is_none_or(|d| d.os != swapdesk_proto::Os::Android))
    }

    fn ask(&self, prompt: SwapPrompt) -> oneshot::Receiver<bool> {
        match self.hooks.get() {
            Some(h) => h.swap_ask(prompt),
            None => {
                // Nobody to ask: the sender is dropped, which the asker hears as nobody present.
                let (_tx, rx) = oneshot::channel();
                rx
            }
        }
    }

    fn ask_closed(&self, request_id: u32, how: AskEnd) {
        if let Some(h) = self.hooks.get() {
            h.swap_ask_closed(request_id, how);
        }
    }

    fn disarm_token(&self, device: DeviceId) {
        if let Some(host) = self.host.get() {
            host.disarm_swap_token(&device);
        }
    }

    fn arm_token(&self, device: DeviceId, token: [u8; 16]) {
        match self.host.get() {
            Some(host) => {
                host.arm_swap_token(device, token);
                // Looked at once the token is dead: the last try of the future viewer may begin just before, and
                // its handshake and the decision come on top.
                let (host, hooks) = (host.clone(), Arc::clone(&self.hooks));
                let after = host.swap_token_ttl() + SWAP_START_MARGIN;
                self.shared.rt.spawn(async move {
                    tokio::time::sleep(after).await;
                    if !host.swap_token_taken(&token)
                        && let Some(h) = hooks.get()
                    {
                        h.swap_never_came(device);
                    }
                });
            }
            None => {
                self.shared.log(Level::Error, format_args!("a swap token was armed before the host service existed"))
            }
        }
    }

    fn begin_viewing(&self, host: DeviceId, token: [u8; 16]) {
        let (Some(hooks), Some(connector)) = (self.hooks.get().cloned(), self.connector.get().and_then(Weak::upgrade))
        else {
            self.shared
                .log(Level::Warn, format_args!("a swap cannot open its new connection: no interface is attached"));
            return;
        };
        let shared = Arc::clone(&self.shared);
        self.shared.rt.spawn(async move {
            let started = tokio::time::Instant::now();
            let deadline = started + SWAP_DIAL_SPAN;
            let mut pause = SWAP_DIAL_PAUSE;
            let mut tries = 0u32;
            let opened = loop {
                tries += 1;
                let attempt = match hooks.address_of(&host) {
                    Some(addr) => {
                        shared.log(Level::Debug, format_args!("swap: dialing {addr} (try {tries})"));
                        connector
                            .connect(addr, host, Intent::Control, false, PermissionSet::EMPTY, Some(token), None)
                            .await
                    }
                    None => Err(ConnectError::UnknownDevice),
                };
                match attempt {
                    // The old session is already gone on both sides: a try that fails while the token lives is
                    // made again, or the two machines are left with no session at all.
                    Err(e) if dial_again(&e) && tokio::time::Instant::now() + pause < deadline => {
                        shared.log(Level::Debug, format_args!("swap: try {tries} failed ({e}); trying again"));
                        tokio::time::sleep(pause).await;
                        pause = (pause * 2).min(SWAP_DIAL_PAUSE_MAX);
                    }
                    other => break other,
                }
            };
            shared.log(
                Level::Debug,
                format_args!(
                    "swap: the new connection {} after {} ms and {tries} tries",
                    if opened.is_ok() { "opened" } else { "failed" },
                    started.elapsed().as_millis()
                ),
            );
            hooks.swap_viewing(host, opened);
        });
    }
}

/// How long the future viewer keeps trying the new connection of a swap: inside the 10 s the future host's token
/// lives (PROTOCOL.md 6.3 step 4), with room for the last try's handshake and decision.
const SWAP_DIAL_SPAN: Duration = Duration::from_secs(8);
/// The pause after the first failed try; it doubles up to `SWAP_DIAL_PAUSE_MAX`.
const SWAP_DIAL_PAUSE: Duration = Duration::from_millis(250);
const SWAP_DIAL_PAUSE_MAX: Duration = Duration::from_secs(1);
/// What the future host adds to the token's life before it tells its user that no connection came.
const SWAP_START_MARGIN: Duration = Duration::from_secs(3);

/// Whether a failed try of a swap's new connection may be made again: only a failure that cannot have spent the
/// token (it is single use, and a try past it would enter as any connect, through a prompt or `UNATTENDED`). Nothing
/// was sent (no address, the LAN guard, the socket, no connection made), or the future host refused `BUSY`, which
/// it decides before it takes the token (a `BUSY` at its commit, after the take, means another session started
/// meanwhile, and the next try meets that session's `BUSY` before the token again).
fn dial_again(e: &ConnectError) -> bool {
    use swapdesk_proto::ErrorCode;
    use swapdesk_session::OpenError;
    match e {
        ConnectError::UnknownDevice | ConnectError::Refused(_) | ConnectError::Socket(_) => true,
        ConnectError::Open(OpenError::Unreachable | OpenError::Refused(ErrorCode::Busy)) => true,
        ConnectError::NotIpv4 | ConnectError::Open(_) => false,
    }
}

/// One chat log per live session: created when the session starts, dropped when it ends (F-CHAT-01). A host
/// registry has one router for every session it serves; a viewer registry has one for its own.
pub(crate) struct ChatRouter {
    sessions: Mutex<HashMap<u64, Arc<ChatHandler>>>,
    hooks: Arc<HooksCell>,
}

impl ChatRouter {
    pub fn new(hooks: Arc<HooksCell>) -> Arc<Self> {
        Arc::new(Self { sessions: Mutex::new(HashMap::new()), hooks })
    }

    pub fn session(&self, session_id: u64) -> Option<Arc<ChatHandler>> {
        lock(&self.sessions).get(&session_id).cloned()
    }

    /// Sends `text` in session `ctx`; returns the message as stored.
    pub async fn send(&self, ctx: &SessionCtx, text: &str, now_ms: u64) -> Result<ChatEntry, ChatError> {
        let handler = self.session(ctx.session_id()).ok_or(ChatError::Closed)?;
        let msg_id = handler.send(ctx, text, now_ms).await?;
        handler.entries().into_iter().find(|e| e.mine && e.msg_id == msg_id).ok_or(ChatError::Closed)
    }

    /// The chat panel shows everything: the peer is told.
    pub async fn mark_read(&self, ctx: &SessionCtx) {
        if let Some(handler) = self.session(ctx.session_id()) {
            handler.mark_read(ctx).await;
        }
    }
}

/// Turns the handler's id-only events into notes with the message, for the hooks.
struct Sink {
    session_id: u64,
    handler: OnceLock<Weak<ChatHandler>>,
    hooks: Arc<HooksCell>,
}

impl ExtrasSink for Sink {
    fn event(&self, event: ExtrasEvent) {
        let Some(hooks) = self.hooks.get() else { return };
        match event {
            ExtrasEvent::ChatReceived { msg_id } => {
                let entry = self
                    .handler
                    .get()
                    .and_then(Weak::upgrade)
                    .and_then(|h| h.entries().into_iter().find(|e| !e.mine && e.msg_id == msg_id));
                if let Some(entry) = entry {
                    hooks.chat(self.session_id, ChatNote::Received(entry));
                }
            }
            ExtrasEvent::ChatReadByPeer { up_to } => hooks.chat(self.session_id, ChatNote::ReadByPeer { up_to }),
            ExtrasEvent::ClipboardSkipped { .. } | ExtrasEvent::ClipboardReceived { .. } => {}
        }
    }
}

impl ChannelHandler for ChatRouter {
    fn name(&self) -> &'static str {
        "chat"
    }

    fn messages(&self) -> &'static [u16] {
        &[0x0301, 0x0302]
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        if let Some(h) = self.session(ctx.session_id()) {
            h.on_message(ctx, msg);
        }
    }

    fn on_start(&self, ctx: &SessionCtx) {
        let session_id = ctx.session_id();
        let sink = Arc::new(Sink { session_id, handler: OnceLock::new(), hooks: Arc::clone(&self.hooks) });
        let handler = ChatHandler::new(sink.clone());
        let _ = sink.handler.set(Arc::downgrade(&handler));
        handler.on_start(ctx);
        lock(&self.sessions).insert(session_id, handler);
    }

    fn on_end(&self, ctx: &SessionCtx, end: &SessionEnd) {
        if let Some(h) = lock(&self.sessions).remove(&ctx.session_id()) {
            h.on_end(ctx, end);
        }
    }
}
