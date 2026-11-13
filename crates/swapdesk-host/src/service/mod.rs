//! The OS-free core of `swapdesk-service.exe` (C2.1, stage 18d-1): the session endpoint as host and as viewer, the
//! helper links, and the relays between them. It owns a tokio runtime; everything it offers to the binary is a plain
//! method or a channel, so the UI pipe of the next slice and the Windows supervisor sit on top of it unchanged.
//!
//! * [`ServiceCore::start`] binds the session endpoint on the approved interface addresses (LAN guard: physical
//!   Ethernet and Wi-Fi, on-link peers; `docs/PROTOCOL.md` section 1), runs [`swapdesk_session::HostService`] on it
//!   and keeps a viewer path ready.
//! * [`ServiceCore::attach_helper`] drives one helper over C2.4 (a reader thread, a writer path) and reports its
//!   end as [`CoreEvent::HelperGone`]; relaunching it is the supervisor's job.
//! * Host sessions: [`host_relay`] (helper frames to the `VideoSender`, input to the right helper, audio to
//!   datagrams, revocation and end).
//! * Viewer sessions: [`ServiceCore::connect`] returns a [`ViewerHandle`] and the [`ViewerMessages`] of the C2.4
//!   `Viewer*` messages (see [`viewer`]).

mod clip_actions;
mod config;
mod extras;
mod files;
mod helper_link;
mod host_relay;
mod listen;
mod media_path;
mod pty_proxy;
mod routing;
mod shared;
mod terminal;
mod viewer;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use quinn::{Endpoint, Incoming};
use swapdesk_extras::chat::{ChatEntry, ChatError};
use swapdesk_ipc::{IpcRole, Message as Ipc};
use swapdesk_lan::guard::{Refusal, check_incoming, find_on_link};
use swapdesk_lan::socket::{bind_quic_all_approved, bind_quic_socket};
use swapdesk_platform::{Level, NetInterfaces};
use swapdesk_proto::GrantsSync;
use swapdesk_proto::{Intent, PermissionSet};
use swapdesk_session::{
    ConsentRequest, ControlIo, HostConfig, HostEvent, HostService, HostUi, PromptAnswer, PromptEnd, PromptRequest,
    Registry, StartInfo, ViewerConfig,
};
use swapdesk_swap::{SwapHandler, SwapOutcome};
use swapdesk_trust::{
    Admission, DeviceId, SessionLimits, TrustHandle, accept_session, connect_session, endpoint_from_socket,
    open_session_endpoint, turn_away,
};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::oneshot;

pub use clip_actions::ClipActionHooks;
pub use config::{
    DEFAULT_PAIR_PORT, DEFAULT_SESSION_PORT, ServiceConfig, ServiceDeps, capabilities_with_hevc, default_capabilities,
    session_codec,
};
pub use extras::{ChatNote, ExtrasHooks};
pub use files::ViewerFiles;
pub use helper_link::{AttachError, HelperEvent, HelperLink};
pub use routing::{
    HelperKind, InputRoute, SpecialVerdict, effective_desktop, input_helper, input_route, media_helper, special_verdict,
};
pub use shared::CoreEvent;
pub use terminal::TerminalEnv;
pub use viewer::{
    ApplyError, ConnectError, StateChange, TermOut, Timeout, ViewerHandle, ViewerMessages, ViewerOut, WaitHook,
    end_reason,
};

use clip_actions::{ActionsHost, ClipHooksCell};
use extras::{ChatRouter, CoreEnv, HooksCell};
use files::FilesService;
use host_relay::HostRelays;
use shared::{Shared, pump};
use terminal::{EnvCell, HostTerminals};

/// Worker threads of the core's runtime: a session is one connection and a few tasks; the heavy work (capture,
/// encode) lives in the helpers.
const WORKERS: usize = 4;
/// Handshakes in flight at once (PROTOCOL.md 1: 16 per machine); one more is ignored.
const MAX_HANDSHAKES: usize = 16;

/// Why a core did not start.
#[derive(Debug)]
pub enum ServiceError {
    Runtime(std::io::Error),
    /// The session endpoint could not be made on `addr`.
    Bind {
        addr: SocketAddrV4,
        error: String,
    },
}

impl core::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "runtime: {e}"),
            Self::Bind { addr, error } => write!(f, "cannot listen on {addr}: {error}"),
        }
    }
}

impl std::error::Error for ServiceError {}

/// What the viewer path needs.
struct Dialer {
    cfg_name: String,
    os: swapdesk_proto::Os,
    os_version: String,
    app_version: String,
    capabilities: swapdesk_proto::Capabilities,
    /// The machine decodes HEVC in hardware (`ServiceCore::set_media_caps`): a viewer session then lists it.
    hevc_decode: Arc<AtomicBool>,
    identity: Arc<swapdesk_trust::Identity>,
    trust: Arc<TrustHandle>,
    clock: Arc<dyn swapdesk_platform::Clock>,
    wall: Arc<dyn swapdesk_trust::WallClock>,
    interfaces: Arc<dyn NetInterfaces>,
    allow_loopback: bool,
    /// The session endpoint's sockets (the own-endpoint check of the development path reads them).
    listeners: Arc<listen::Listeners>,
}

/// How long a grants sync may take, from the dial to the answer (GRANTS-SYNC.md 4.2).
pub const SYNC_LIMIT: Duration = Duration::from_secs(5);

/// Why a grants sync gave no answer.
#[derive(Debug)]
pub enum SyncError {
    /// The address did not pass the LAN guard, or the device is not paired.
    Guard(ConnectError),
    /// The device, authenticated by its pinned key, refused this side's key.
    UnknownKey,
    /// What answered at the address is not the device (its key failed this side's check): a forged or stale record.
    WrongPeer,
    /// The device closed it as a violation: it does not speak minor 1, whatever its record says.
    Violation,
    /// The device is over its budget, or already syncing with this side.
    Busy,
    /// Anything else: a lost or silent connection.
    Failed,
}

/// What an application close of the device means.
fn closed_why(e: &quinn::ConnectionError) -> SyncError {
    use swapdesk_proto::ErrorCode;
    match e {
        quinn::ConnectionError::ApplicationClosed(c) => match u16::try_from(c.error_code.into_inner()) {
            Ok(code) if code == ErrorCode::ProtocolViolation.tag() || code == ErrorCode::UnsupportedVersion.tag() => {
                SyncError::Violation
            }
            Ok(code) if code == ErrorCode::Busy.tag() => SyncError::Busy,
            Ok(code) if code == ErrorCode::UnknownDevice.tag() => SyncError::UnknownKey,
            _ => SyncError::Failed,
        },
        // A TLS alert (QUIC's crypto error codes, 0x100 to 0x1ff): the device's verifier refused this side's key.
        quinn::ConnectionError::ConnectionClosed(c) if (0x100..=0x1ff).contains(&u64::from(c.error_code)) => {
            SyncError::UnknownKey
        }
        _ => SyncError::Failed,
    }
}

/// Opens viewer sessions. Shared by the core and by the swap environment, which opens the new connection of a swap
/// from a session task.
pub(crate) struct Connector {
    dialer: Arc<Dialer>,
    shared: Arc<Shared>,
    viewers: Mutex<HashMap<u64, Weak<viewer::Inner>>>,
    swap: SwapHandler,
    hooks: Arc<HooksCell>,
    clip_hooks: Arc<ClipHooksCell>,
    files: Arc<FilesService>,
}

impl Connector {
    /// What every dial to a paired device starts with: the address passes the LAN guard (IPv4, on link, not our own),
    /// the device's pinned key and stored grants come from the trust store (never from the address), and a socket is
    /// bound on the local address of that link with TTL 1.
    async fn endpoint_for(
        &self,
        addr: SocketAddr,
        device: DeviceId,
    ) -> Result<(Endpoint, swapdesk_trust::Spki, PermissionSet), ConnectError> {
        let d = &self.dialer;
        let SocketAddr::V4(v4) = addr else { return Err(ConnectError::NotIpv4) };
        let local = if d.allow_loopback && v4.ip().is_loopback() {
            // Development only. A session with this machine's own endpoint is still refused (PROTOCOL.md 1).
            if d.listeners.addrs().contains(&addr) {
                return Err(ConnectError::Refused(Refusal::OwnAddress));
            }
            Ipv4Addr::LOCALHOST
        } else {
            let read = std::time::Instant::now();
            let list = crate::netcache::list_warm_async(&d.interfaces)
                .await
                .map_err(|_| ConnectError::Refused(Refusal::UnknownLocalAddress))?;
            self.shared.log(
                Level::Debug,
                format_args!("dial {addr}: interface list read in {} ms", read.elapsed().as_millis()),
            );
            find_on_link(&list, *v4.ip()).map_err(ConnectError::Refused)?.local
        };
        let (spki, stored) = d
            .trust
            .read(|s| s.devices.iter().find(|p| p.device_id == device).map(|p| (p.spki, p.grants_here)))
            .ok_or(ConnectError::UnknownDevice)?;
        let socket = bind_quic_socket(SocketAddrV4::new(local, 0)).map_err(ConnectError::Socket)?;
        let endpoint = endpoint_from_socket(socket, None)
            .map_err(|e| ConnectError::Socket(std::io::Error::other(e.to_string())))?;
        Ok((endpoint, spki, stored))
    }

    /// A grants sync with `device` at `addr` (GRANTS-SYNC.md 4.2): one `GrantsSync` each way over a connection its
    /// pinned key authenticates, carrying what this side stored for the device; the answer carries what the device
    /// stored for this side. Gives up `SYNC_LIMIT` after it started.
    pub(crate) async fn sync_grants(&self, addr: SocketAddr, device: DeviceId) -> Result<GrantsSync, SyncError> {
        let attempt = async {
            let (endpoint, spki, stored) = self.endpoint_for(addr, device).await.map_err(SyncError::Guard)?;
            let conn = connect_session(&endpoint, &self.dialer.identity, addr, spki).await.map_err(|e| match e {
                // Whatever answered failed this side's own check of the key: a forged or stale record sent the
                // dial to the wrong machine. Nothing is known about what the device itself thinks of this side.
                swapdesk_trust::TrustError::WrongPeer => SyncError::WrongPeer,
                // Every other handshake failure is the peer's unauthenticated word or a lost packet. That the
                // device forgot this side is read only after the handshake, from what its pinned key authenticated
                // (`closed_why`).
                _ => SyncError::Failed,
            })?;
            let (send, recv) = conn.open_bi().await.map_err(|e| closed_why(&e))?;
            let mut io = ControlIo::from_quinn(send, recv);
            io.out.set_peer_minor(swapdesk_proto::PROTO_MINOR);
            io.out
                .send(GrantsSync { proto_minor: swapdesk_proto::PROTO_MINOR, grants: stored })
                .await
                .map_err(|_| SyncError::Failed)?;
            let answer = match io.rx.recv().await {
                Some(Ok(swapdesk_proto::Message::GrantsSync(g))) if g.proto_minor >= 1 => Ok(g),
                Some(Ok(_)) => Err(SyncError::Violation),
                Some(Err(_)) | None => Err(conn.close_reason().map_or(SyncError::Failed, |e| closed_why(&e))),
            };
            // No session: the client closes the connection itself.
            conn.close(quinn::VarInt::from_u32(0), b"");
            endpoint.wait_idle().await;
            answer
        };
        tokio::time::timeout(SYNC_LIMIT, attempt).await.unwrap_or(Err(SyncError::Failed))
    }

    /// Opens a session to `addr` as a viewer. The device must be paired (its key is read from the trust store, never
    /// from the address) and the address must pass the LAN guard. `swap_token` enters a swap (PROTOCOL.md 6.3).
    /// `waiting` hears `Connecting` and `Prompted` while the host decides.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn connect(
        &self,
        addr: SocketAddr,
        device: DeviceId,
        intent: Intent,
        unattended: bool,
        extra: PermissionSet,
        swap_token: Option<[u8; 16]>,
        waiting: Option<WaitHook>,
    ) -> Result<(ViewerHandle, ViewerMessages), ConnectError> {
        let d = &self.dialer;
        let started = std::time::Instant::now();
        let (endpoint, spki, stored) = self.endpoint_for(addr, device).await?;
        self.shared.log(
            Level::Debug,
            format_args!("dial {addr}: local endpoint ready after {} ms", started.elapsed().as_millis()),
        );
        let mut cfg = ViewerConfig::new(
            &d.cfg_name,
            d.os,
            &d.os_version,
            &d.app_version,
            config::with_hevc_decode(d.capabilities.clone(), d.hevc_decode.load(Ordering::Relaxed)),
            Arc::clone(&d.clock),
        );
        cfg.intent = intent;
        cfg.requested =
            if unattended { requested_for(intent).union(PermissionSet::UNATTENDED) } else { requested_for(intent) };
        cfg.requested = cfg.requested.union(extra);
        cfg.swap_token = swap_token;
        // A host of minor 1 hears what this machine stored for it (GRANTS-SYNC.md 3.2).
        cfg.stored_grants = Some(stored);
        let opened = viewer::open(
            viewer::OpenParams {
                rt: self.shared.rt.clone(),
                trace: Arc::clone(&self.shared.trace),
                wall: Arc::clone(&d.wall),
                endpoint,
                identity: Arc::clone(&d.identity),
                addr,
                expected: spki,
                swap: self.swap.clone(),
                hooks: Arc::clone(&self.hooks),
                clip_hooks: Arc::clone(&self.clip_hooks),
                files: self.files.viewer(),
                waiting,
            },
            cfg,
        )
        .await?;
        self.shared.log(
            Level::Debug,
            format_args!(
                "dial {addr}: session open after {} ms (handshake and the host's answer)",
                started.elapsed().as_millis()
            ),
        );
        lock(&self.viewers).insert(opened.handle.session_id(), Arc::downgrade(&opened.handle.inner));
        Ok((opened.handle, opened.messages))
    }
}

/// Hands the host's UI calls through, adding what only the core knows: the displays of the media helper.
/// Who is at the console, read now (ACCOUNTS.md 5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsoleRead {
    /// The sign-in screen.
    Nobody,
    /// A user, by SID.
    User(String),
    /// Someone is logged in but who could not be read: treated as another account (fails closed).
    Unreadable,
}

impl ConsoleRead {
    /// What a hub keeps as the console user: an unreadable user is a value that matches no account.
    pub fn hub_user(self) -> Option<String> {
        match self {
            Self::Nobody => None,
            Self::User(sid) => Some(sid),
            Self::Unreadable => Some(UNREADABLE_USER.to_string()),
        }
    }
}

/// The console user a hub keeps while who is logged in cannot be read: not a SID, so no account matches it.
pub const UNREADABLE_USER: &str = "unreadable";

/// Reads [`ConsoleRead`] now; set by the service on a machine of several accounts.
pub type ConsoleProbe = Arc<dyn Fn() -> ConsoleRead + Send + Sync>;

/// Where the media helper's report of the input desktop goes; set by the account's hub (see `Hub::set_helper_desktop`).
pub type DesktopSink = Arc<dyn Fn(swapdesk_proto::Desktop) + Send + Sync>;

/// Sends the secure attention sequence now (Windows: `SendSAS(FALSE)` from the service process); the error says why
/// it could not. Set by the service binary; the session's gate (UNATTENDED, not away) has been passed before it runs.
pub type SasSender = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

struct CoreUi {
    inner: Arc<dyn HostUi>,
    shared: Arc<Shared>,
}

impl HostUi for CoreUi {
    fn somebody_present(&self) -> bool {
        self.inner.somebody_present()
    }

    fn current_desktop(&self) -> Option<swapdesk_proto::Desktop> {
        self.inner.current_desktop()
    }

    fn console_owner(&self) -> swapdesk_session::ConsoleOwner {
        self.inner.console_owner()
    }

    fn prompt(&self, request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
        self.inner.prompt(request)
    }

    fn prompt_closed(&self, request_id: u64, how: PromptEnd) {
        self.inner.prompt_closed(request_id, how);
    }

    fn consent(&self, request: ConsentRequest) -> oneshot::Receiver<bool> {
        self.inner.consent(request)
    }

    fn consent_closed(&self, request: &ConsentRequest, how: PromptEnd) {
        self.inner.consent_closed(request, how);
    }

    fn lock_on_end(&self) -> bool {
        self.inner.lock_on_end()
    }

    fn lock_screen(&self) {
        self.inner.lock_screen();
    }

    fn start_info(&self) -> StartInfo {
        let mut info = self.inner.start_info();
        if info.displays.is_empty() {
            info.displays = self.shared.media_displays();
        }
        info
    }

    fn event(&self, event: HostEvent) {
        match &event {
            HostEvent::SessionStarted { session_id, mode, granted, .. } => {
                self.shared.log(
                    Level::Info,
                    format_args!("session {session_id:#x} started: {mode:?}, grants {:#06x}", granted.bits()),
                );
            }
            HostEvent::Ended { session_id, end, .. } => {
                self.shared
                    .log(Level::Info, format_args!("session {session_id:#x} ended: {:?} {:?}", end.kind, end.code));
            }
            _ => {}
        }
        self.inner.event(event);
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What every account's core of one machine shares (ACCOUNTS.md 5.6): the one hosted session, and the caps before
/// a handshake (16 in flight, 4 per address), so several accounts never multiply them.
#[derive(Clone)]
pub struct MachineShared {
    pub hosting: Arc<swapdesk_session::MachineHosting>,
    handshakes: Arc<AtomicUsize>,
    limits: SessionLimits,
}

impl MachineShared {
    pub fn new() -> Self {
        Self {
            hosting: swapdesk_session::MachineHosting::new(),
            handshakes: Arc::new(AtomicUsize::new(0)),
            limits: SessionLimits::new(),
        }
    }

    /// Handshakes in flight now, across the machine.
    pub fn handshakes_in_flight(&self) -> usize {
        self.handshakes.load(Ordering::SeqCst)
    }
}

impl Default for MachineShared {
    fn default() -> Self {
        Self::new()
    }
}

/// What the accept loops need to judge an incoming connection.
struct Gate {
    interfaces: Arc<dyn NetInterfaces>,
    /// For the devices blocked by MAC (D65).
    neighbors: Arc<dyn swapdesk_platform::NeighborTable>,
    allow_loopback: bool,
    trust: Arc<TrustHandle>,
    host: HostService,
    shared: Arc<Shared>,
    /// Handshakes in flight, shared by the machine's cores.
    handshakes: Arc<AtomicUsize>,
    /// Connections per remote address (PROTOCOL.md 1: 4 at once, `retry()` over the limit), shared likewise.
    limits: SessionLimits,
}

/// A place among the handshakes in flight, given back when dropped.
struct Slot(Arc<Gate>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.handshakes.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Gate {
    /// PROTOCOL.md 1, before any handshake: on-link in the subnet of the approved interface that owns the local
    /// address, not loopback, not one of this machine's own addresses. Loopback is let through only when the core
    /// was built with the development option, and then only between two loopback addresses.
    ///
    /// The interface list is read off the runtime's workers (and from a copy no older than a second, see
    /// [`crate::netcache`]): an incoming connection is something a peer can make as often as it likes.
    async fn judge(&self, incoming: &Incoming) -> Result<(), Refusal> {
        let remote = incoming.remote_address().ip();
        let local = incoming.local_ip();
        if self.allow_loopback && remote.is_loopback() && local.is_some_and(|l| l.is_loopback()) {
            return Ok(());
        }
        // The guard reads the interfaces of now, never a warm copy: an incoming session is judged on-link against the
        // machine as it is (the dial alone takes the warm copy, for the swap's time).
        let list = crate::netcache::list_async(&self.interfaces).await.map_err(|_| Refusal::UnknownLocalAddress)?;
        let local = local.ok_or(Refusal::UnknownLocalAddress)?;
        let link = check_incoming(&list, local, remote)?;
        if let Some(owner) = list.iter().find(|i| i.index == link.if_index) {
            self.shared.note_link(local, owner.kind);
        }
        Ok(())
    }

    /// The peer's address is in the trust store's `blocked` list, or the device behind it is blocked by its MAC
    /// (D65): it gets no answer at all, before any other check and before a handshake or a place is spent on it.
    fn is_blocked(&self, incoming: &Incoming) -> bool {
        let ip = incoming.remote_address().ip();
        self.trust.read(|s| s.is_blocked(ip)) || crate::blocking::blocked_by_mac(&self.trust, &*self.neighbors, ip)
    }

    fn reserve(self: &Arc<Self>) -> Option<Slot> {
        self.handshakes
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < MAX_HANDSHAKES).then_some(n + 1))
            .ok()?;
        Some(Slot(Arc::clone(self)))
    }
}

async fn accept_loop(gate: Arc<Gate>, endpoint: Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        if gate.is_blocked(&incoming) {
            gate.shared.log(
                Level::Debug,
                format_args!("ignored a connection from the blocked address {}", incoming.remote_address().ip()),
            );
            incoming.ignore();
            continue;
        }
        if let Err(why) = gate.judge(&incoming).await {
            gate.shared
                .log(Level::Warn, format_args!("refused a connection from {}: {why}", incoming.remote_address()));
            incoming.refuse();
            continue;
        }
        // Before any place is taken: an address that has not proved itself gets `retry()` (a forged source cannot
        // answer it, so it holds no per-address place and no handshake place). A validated address is then held to
        // its 4 connections.
        let address = match gate.limits.judge(&incoming) {
            Admission::Admit(address) => address,
            Admission::Retry => {
                turn_away(incoming, true);
                continue;
            }
            Admission::Ignore => {
                turn_away(incoming, false);
                continue;
            }
        };
        let Some(slot) = gate.reserve() else {
            incoming.ignore();
            continue;
        };
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let accepted = accept_session(incoming, &*gate.trust).await;
            drop(slot);
            match accepted {
                Ok((conn, device)) => {
                    gate.host.serve(conn, device).await;
                }
                Err(e) => gate.shared.log(Level::Debug, format_args!("a session handshake failed: {e}")),
            }
            // The address's place is held until the connection is over, not only through the handshake.
            drop(address);
        });
    }
}

/// Why a chat message was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatSendError {
    /// No live session has this id.
    NoSuchSession,
    Chat(ChatError),
}

/// The permissions a viewer asks for with `intent`: what it means to do, plus audio (off until the viewer turns it
/// on), with control the clipboard, and files for a controlling viewer (stage 44: an unattended session grants the
/// stored permissions the viewer asked for, so a viewer that did not ask for `FILES` could never use them there). The
/// host's user decides what is granted.
pub fn requested_for(intent: Intent) -> PermissionSet {
    match intent {
        Intent::View => PermissionSet::VIEW | PermissionSet::AUDIO,
        // Clipboard sync is asked with control (a copy at the viewer writes the host's clipboard): the host's user
        // still decides whether it is granted.
        Intent::Control => {
            PermissionSet::CONTROL | PermissionSet::AUDIO | PermissionSet::CLIPBOARD | PermissionSet::FILES
        }
    }
}

/// The core of the service. Dropping it ends its sessions, tells the helpers to shut down and stops the runtime.
pub struct ServiceCore {
    shared: Arc<Shared>,
    host: HostService,
    dialer: Arc<Dialer>,
    connector: Arc<Connector>,
    hooks: Arc<HooksCell>,
    clip_hooks: Arc<ClipHooksCell>,
    host_clip: Arc<swapdesk_extras::clipboard::ClipboardHandler<crate::proxy::PipeClipboard>>,
    host_chat: Arc<ChatRouter>,
    term_env: Arc<EnvCell>,
    swap: SwapHandler,
    runtime: Option<Runtime>,
    listeners: Arc<listen::Listeners>,
    events: Mutex<Option<std::sync::mpsc::Receiver<CoreEvent>>>,
    win_session: u32,
}

impl ServiceCore {
    /// Builds the runtime, binds the session endpoint on every approved interface address (and on
    /// `cfg.listen_extra`) and starts accepting.
    pub fn start(mut cfg: ServiceConfig) -> Result<Self, ServiceError> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .thread_name("swapdesk-service")
            .enable_all()
            .build()
            .map_err(ServiceError::Runtime)?;
        let enter = runtime.enter();
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(shared::CORE_EVENTS);
        let shared =
            Arc::new(Shared::new(runtime.handle().clone(), Arc::clone(&cfg.trace), cfg.audio_exclude_pid, events_tx));
        shared.host_hevc_hardware.store(cfg.host_hevc_hardware, Ordering::Relaxed);

        let mut registry = Registry::new();
        // The registry is empty: this cannot clash. A failure would be a bug of this crate and would show as a session
        // with no video, so it is logged rather than hidden.
        if let Err(e) = registry.register(Arc::new(HostRelays { shared: Arc::clone(&shared) })) {
            shared.log(Level::Error, format_args!("the host relay could not be registered: {e:?}"));
        }
        // Swap and chat (stage 41): the same handlers serve the host role here and every viewer session. The hooks
        // that reach the interface are set after the start, so the environment holds them in a cell.
        let hooks = Arc::new(HooksCell::default());
        let env = Arc::new(CoreEnv {
            ui: Arc::clone(&cfg.ui),
            trust: Arc::clone(&cfg.trust),
            host: OnceLock::new(),
            hooks: Arc::clone(&hooks),
            connector: OnceLock::new(),
            shared: Arc::clone(&shared),
        });
        let swap = SwapHandler::new(env.clone());
        let host_chat = ChatRouter::new(Arc::clone(&hooks));
        // Terminals (stage 43): shells run in the user helper, or, for the administrator terminal, in this process.
        let term_env = Arc::new(EnvCell::default());
        let host_terms =
            HostTerminals::new(Arc::clone(&shared), Arc::clone(&term_env), Arc::clone(&cfg.trust), Arc::clone(&hooks));
        // Files (stage 44): the host's handler reaches the user's disk through the user helper, behind D13.
        let files = FilesService::new(Arc::clone(&hooks), Arc::clone(&cfg.trust), Arc::clone(&shared));
        for (what, result) in [
            ("swap", registry.register(swap.handler())),
            ("chat", registry.register(host_chat.clone())),
            ("terminal", registry.register(host_terms.clone())),
            ("files", registry.register(files.host_handler())),
        ] {
            if let Err(e) = result {
                shared.log(Level::Error, format_args!("the {what} handler could not be registered: {e:?}"));
            }
        }
        // Clipboard sync and quick actions (stage 42): the OS work runs in the user helper, the handlers here.
        let clip_hooks = Arc::new(ClipHooksCell::default());
        let host_cell: Arc<OnceLock<HostService>> = Arc::default();
        let host_clip = clip_actions::host_clipboard(&shared, &clip_hooks, &host_cell);
        let actions = ActionsHost::new(&shared, Arc::clone(&cfg.ui), &clip_hooks, cfg.service_actions.take());
        for (what, result) in
            [("clipboard", registry.register(host_clip.clone())), ("quick actions", registry.register(actions))]
        {
            if let Err(e) = result {
                shared.log(Level::Error, format_args!("the {what} handler could not be registered: {e:?}"));
            }
        }
        let mut host_cfg =
            HostConfig::new(&cfg.name, cfg.os, &cfg.os_version, &cfg.app_version, cfg.capabilities.clone());
        host_cfg.max_live_sessions = 1;
        host_cfg.proto_minor = cfg.proto_minor;
        // One machine, one hosted session and one set of handshake caps, whatever the number of accounts.
        let machine = cfg.machine.clone().unwrap_or_default();
        host_cfg.machine = Some(Arc::clone(&machine.hosting));
        let ui = Arc::new(CoreUi { inner: Arc::clone(&cfg.ui), shared: Arc::clone(&shared) });
        let host = HostService::new(host_cfg, Arc::clone(&cfg.trust), ui, registry, Arc::clone(&cfg.clock));
        let _ = env.host.set(host.clone());
        let _ = host_cell.set(host.clone());

        // One endpoint per approved interface address (PROTOCOL.md 1), and the development addresses.
        // (socket, is a development address)
        let mut sockets = Vec::new();
        let mut held_wildcard = None;
        // With a wildcard binder: one exclusive socket on 0.0.0.0, held from the start (ACCOUNTS.md 5.5). A port another
        // program holds fails the start, so the machine can give the account another slot.
        if let Some(binder) = &cfg.wildcard {
            let at = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, cfg.session_port);
            let socket = binder(cfg.session_port).map_err(|e| ServiceError::Bind { addr: at, error: e.to_string() })?;
            socket.set_ttl(swapdesk_lan::socket::QUIC_TTL).map_err(ServiceError::Runtime)?;
            held_wildcard = Some(socket.try_clone().map_err(ServiceError::Runtime)?);
            sockets.push((socket, true));
        }
        // An account that receives no connections listens on no approved address (ACCOUNTS.md 6).
        let per_address = cfg.receiving && cfg.wildcard.is_none();
        match cfg.interfaces.list().map(|l| if per_address { l } else { Vec::new() }) {
            Ok(list) => {
                for (addr, socket) in bind_quic_all_approved(&list, cfg.session_port) {
                    match socket {
                        Ok(s) => sockets.push((s, false)),
                        Err(e) => shared
                            .log(Level::Warn, format_args!("cannot listen on {} ({}): {e}", addr.addr, addr.if_name)),
                    }
                }
            }
            Err(e) => shared.log(Level::Warn, format_args!("the interface list is unavailable: {e}")),
        }
        for addr in &cfg.listen_extra {
            sockets.push((
                bind_quic_socket(*addr).map_err(|e| ServiceError::Bind { addr: *addr, error: e.to_string() })?,
                true,
            ));
        }
        let mut endpoints = Vec::new();
        for (socket, extra) in sockets {
            let local = socket.local_addr().map_err(ServiceError::Runtime)?;
            let endpoint =
                open_session_endpoint(socket, &cfg.identity, cfg.trust.clone()).map_err(|e| ServiceError::Bind {
                    addr: match local {
                        SocketAddr::V4(v4) => v4,
                        SocketAddr::V6(_) => SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
                    },
                    error: e.to_string(),
                })?;
            shared.log(Level::Info, format_args!("session endpoint on {local}"));
            endpoints.push((endpoint, extra));
        }

        let gate = Arc::new(Gate {
            interfaces: Arc::clone(&cfg.interfaces),
            neighbors: Arc::clone(&cfg.neighbors),
            allow_loopback: cfg.allow_loopback_peers,
            trust: Arc::clone(&cfg.trust),
            host: host.clone(),
            shared: Arc::clone(&shared),
            handshakes: Arc::clone(&machine.handshakes),
            limits: machine.limits.clone(),
        });
        let listeners = Arc::new(listen::Listeners::new(
            cfg.session_port,
            Arc::clone(&cfg.identity),
            Arc::clone(&cfg.trust),
            Arc::clone(&gate),
            Arc::clone(&shared),
        ));
        listeners.set_wildcard(cfg.wildcard.is_some());
        if let Some(held) = held_wildcard {
            listeners.set_held(held);
        }
        for (endpoint, extra) in endpoints {
            listeners.adopt(endpoint, extra).map_err(ServiceError::Runtime)?;
        }
        listeners.set_receiving(cfg.receiving);
        let dialer = Arc::new(Dialer {
            cfg_name: cfg.name.clone(),
            os: cfg.os,
            os_version: cfg.os_version.clone(),
            app_version: cfg.app_version.clone(),
            capabilities: cfg.capabilities.clone(),
            hevc_decode: Arc::clone(&shared.hevc_decode),
            identity: Arc::clone(&cfg.identity),
            trust: Arc::clone(&cfg.trust),
            clock: Arc::clone(&cfg.clock),
            wall: Arc::clone(&cfg.wall),
            interfaces: Arc::clone(&cfg.interfaces),
            allow_loopback: cfg.allow_loopback_peers,
            listeners: Arc::clone(&listeners),
        });
        drop(enter);
        let connector = Arc::new(Connector {
            dialer: Arc::clone(&dialer),
            shared: Arc::clone(&shared),
            viewers: Mutex::new(HashMap::new()),
            swap: swap.clone(),
            hooks: Arc::clone(&hooks),
            clip_hooks: Arc::clone(&clip_hooks),
            files: Arc::clone(&files),
        });
        let _ = env.connector.set(Arc::downgrade(&connector));
        Ok(Self {
            shared,
            host,
            dialer,
            connector,
            hooks,
            clip_hooks,
            host_clip,
            host_chat,
            term_env,
            swap,
            runtime: Some(runtime),
            listeners,
            events: Mutex::new(Some(events_rx)),
            win_session: cfg.win_session,
        })
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        &self.shared.rt
    }

    /// The core's runtime, for the owner's own async work (the UI pipe tasks of the next slice).
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.shared.rt
    }

    /// The addresses the session endpoint listens on now.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.listeners.addrs()
    }

    /// The account this core serves (its user's SID), on a machine of several: the terminal and the files then run
    /// only as that account (ACCOUNTS.md 5.4).
    pub fn set_account(&self, sid: &str) {
        let _ = self.shared.account.set(sid.to_string());
    }

    /// How the fence reads who is at the console now (ACCOUNTS.md 5.1).
    pub fn set_console_probe(&self, probe: ConsoleProbe) {
        let _ = self.shared.console_probe.set(probe);
    }

    /// Where the media helper's report of the input desktop goes (the account's hub, which combines it with the
    /// console's owner and the lock state).
    pub fn set_desktop_sink(&self, sink: DesktopSink) {
        let _ = self.shared.desktop_sink.set(sink);
    }

    /// What this machine's GPU does in hardware, as the interface probed it (F-VIEW-07): a hosted session picks HEVC
    /// only with `hevc_encode`, and a viewer session lists an HEVC decoder only with `hevc_decode`.
    pub fn set_media_caps(&self, hevc_encode: bool, hevc_decode: bool) {
        self.shared.host_hevc_hardware.store(hevc_encode, Ordering::Relaxed);
        self.shared.hevc_decode.store(hevc_decode, Ordering::Relaxed);
    }

    /// How the secure attention sequence is sent for a viewer's `SendSpecial` (F-UNAT-01).
    pub fn set_sas_sender(&self, sender: SasSender) {
        let _ = self.shared.sas_sender.set(sender);
    }

    /// Whether this account receives connections now (ACCOUNTS.md 6: only with a pairing, or while pairing mode is
    /// open). Off closes every approved address's endpoint (the sessions on them go on until they end); on listens on
    /// them again. Returns whether the listening addresses changed.
    pub fn set_receiving(&self, on: bool) -> bool {
        if self.listeners.receiving() == on {
            return false;
        }
        self.listeners.set_receiving(on);
        self.network_changed()
    }

    /// Whether this account receives connections now.
    pub fn receiving(&self) -> bool {
        self.listeners.receiving()
    }

    /// The network changed: the session endpoint follows the approved addresses (a new one is listened on, one that
    /// went stops taking connections; one that stayed, and its sessions, are not touched). Returns whether the
    /// listening addresses changed. The owner calls it on every network notice (the UI hub's `network_changed`).
    pub fn network_changed(&self) -> bool {
        match self.dialer.interfaces.list() {
            Ok(list) => self.listeners.follow(&list),
            Err(e) => {
                self.shared.log(Level::Warn, format_args!("the interface list is unavailable: {e}"));
                false
            }
        }
    }

    /// The host service: the banner's and Permissões' calls (`set_session_grants`, `disconnect`, `sessions`, ...).
    pub fn host(&self) -> &HostService {
        &self.host
    }

    /// The core's events, once: the owner reads them (a supervisor thread, the UI dispatcher).
    pub fn take_events(&self) -> Option<std::sync::mpsc::Receiver<CoreEvent>> {
        lock(&self.events).take()
    }

    /// Runs `wake` after every event is queued (from the thread that queued it: keep it short and never blocking), so
    /// the owner can sleep until [`take_events`](Self::take_events)'s receiver has something. Once: a second call is
    /// ignored. Events queued before it are not announced.
    pub fn set_event_waker(&self, wake: Arc<dyn Fn() + Send + Sync>) -> bool {
        self.shared.set_waker(wake)
    }

    /// Advisory events (`HelperFault`, `SasRequested`) dropped because the owner was not reading its queue.
    pub fn events_dropped(&self) -> u64 {
        self.shared.events_dropped()
    }

    // ---------- helpers ----------

    /// Attaches a helper that runs inside this process (the Mac app's in-process helper, the tests'): the helper's
    /// process id is this process's own. A helper in another process goes through [`ServiceCore::attach_helper_pid`]
    /// with the PID the caller verified.
    pub fn attach_helper<R, W>(&self, role: IpcRole, reader: R, writer: W) -> Result<HelperKind, AttachError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        self.attach_helper_pid(role, std::process::id(), reader, writer)
    }

    /// Attaches a helper: the service side of the C2.4 handshake on `reader` / `writer`, then a reader thread and a
    /// writer path. `role` is what the caller expects (the pipe it created for that helper); a `Hello` with another
    /// role is refused. `pid` is the helper's process id as the caller verified it (the supervisor's check of the pipe
    /// client's PID): the `Hello`'s own `pid` is ignored, and `HelperGone` reports this one. A second helper of the
    /// same kind replaces the first, which is told to shut down. Blocks until the helper's `Hello` arrives: call it
    /// from the supervisor's thread. The end of the pipe is reported as `CoreEvent::HelperGone`.
    pub fn attach_helper_pid<R, W>(
        &self,
        role: IpcRole,
        pid: u32,
        reader: R,
        writer: W,
    ) -> Result<HelperKind, AttachError>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let helper_link::Attached { link, events } = helper_link::attach(role, self.win_session, pid, reader, writer)?;
        let link = Arc::new(link);
        let kind = link.kind();
        if let Some(old) = self.shared.install(Arc::clone(&link)) {
            old.send_blocking(Ipc::Shutdown);
        }
        self.shared.log(Level::Info, format_args!("{kind:?} helper attached (pid {})", link.pid()));
        self.shared.emit(CoreEvent::HelperAttached { kind, pid: link.pid() });
        self.handle().spawn(pump(Arc::clone(&self.shared), self.host.clone(), link, events));
        self.shared.wake_all();
        Ok(kind)
    }

    /// The attached helper of `kind`, if its pipe is open.
    pub fn helper(&self, kind: HelperKind) -> Option<Arc<HelperLink>> {
        self.shared.helper(kind)
    }

    /// The host user's mute: the media helper sends header-only datagrams with the MUTED flag, and the viewers are told
    /// (`HostMute`).
    pub fn set_audio_muted(&self, muted: bool) {
        let shared = Arc::clone(&self.shared);
        self.handle().spawn(async move {
            if let Some((_, helper)) = shared.media() {
                helper.send(Ipc::SetAudioMuted { muted }).await;
            }
            shared.mute_all(muted);
        });
    }

    // ---------- viewer ----------

    /// Opens a session to `addr` as a viewer, with `intent`. The device must be paired (its key is read from the
    /// trust store, never from the address) and the address must pass the LAN guard. Blocks until the host decided
    /// (up to the prompt's 20 s plus the handshake): call it from a thread that is not on the runtime; async callers
    /// use [`ServiceCore::connect_async`].
    pub fn connect(
        &self,
        addr: SocketAddr,
        device: DeviceId,
        intent: Intent,
    ) -> Result<(ViewerHandle, ViewerMessages), ConnectError> {
        self.connect_with(addr, device, intent, false)
    }

    /// [`ServiceCore::connect`] with `unattended`: the viewer also asks for `UNATTENDED` (entry without a prompt,
    /// F-SESS-02). Whether to ask is the caller's rule (the hub asks toward a host of protocol minor 1, for an
    /// administrator account); the host lets it in only when the device holds `UNATTENDED`.
    pub fn connect_with(
        &self,
        addr: SocketAddr,
        device: DeviceId,
        intent: Intent,
        unattended: bool,
    ) -> Result<(ViewerHandle, ViewerMessages), ConnectError> {
        self.handle().block_on(self.connect_with_async(addr, device, intent, unattended))
    }

    pub async fn connect_async(
        &self,
        addr: SocketAddr,
        device: DeviceId,
        intent: Intent,
    ) -> Result<(ViewerHandle, ViewerMessages), ConnectError> {
        self.connect_with_async(addr, device, intent, false).await
    }

    pub async fn connect_with_async(
        &self,
        addr: SocketAddr,
        device: DeviceId,
        intent: Intent,
        unattended: bool,
    ) -> Result<(ViewerHandle, ViewerMessages), ConnectError> {
        self.connector.connect(addr, device, intent, unattended, PermissionSet::EMPTY, None, None).await
    }

    /// [`ServiceCore::connect_with`] asking for `extra` permissions too: an unattended entry grants only what was asked
    /// for (`stored & requested`), so a viewer that means to use the terminal in one asks for `TERMINAL`; a viewer
    /// whose user is a local administrator may ask for `ADMIN_TERMINAL` (D10), which the host grants only when the
    /// device holds it. The caller (the UI hub) makes the administrator check.
    pub fn connect_asking(
        &self,
        addr: SocketAddr,
        device: DeviceId,
        intent: Intent,
        unattended: bool,
        extra: PermissionSet,
    ) -> Result<(ViewerHandle, ViewerMessages), ConnectError> {
        self.handle().block_on(self.connect_asking_async(addr, device, intent, unattended, extra, None))
    }

    /// [`ServiceCore::connect_asking`] for a caller already on the runtime: it awaits the host's decision and drops
    /// the attempt when its future is dropped (the viewer then tells the host with a `Bye`). `waiting` hears
    /// `Connecting` when the dial starts and `Prompted` when the host starts asking its user.
    pub async fn connect_asking_async(
        &self,
        addr: SocketAddr,
        device: DeviceId,
        intent: Intent,
        unattended: bool,
        extra: PermissionSet,
        waiting: Option<WaitHook>,
    ) -> Result<(ViewerHandle, ViewerMessages), ConnectError> {
        self.connector.connect(addr, device, intent, unattended, extra, None, waiting).await
    }

    /// A grants sync with `device` at `addr` (see `Connector::sync_grants`).
    pub async fn sync_grants_async(&self, addr: SocketAddr, device: DeviceId) -> Result<GrantsSync, SyncError> {
        self.connector.sync_grants(addr, device).await
    }

    /// The live viewer session `session_id`.
    pub fn viewer(&self, session_id: u64) -> Option<ViewerHandle> {
        let mut viewers = lock(&self.connector.viewers);
        viewers.retain(|_, w| w.strong_count() > 0);
        viewers.get(&session_id).and_then(Weak::upgrade).map(|inner| ViewerHandle { inner })
    }

    /// Applies a C2.4 message the UI sent for a viewer session (`ViewerInput`, `ViewerKeyframe`, `ViewerReport`),
    /// finding the session by the id it carries.
    pub fn viewer_message(&self, msg: Ipc) -> Result<(), ApplyError> {
        let session_id = match &msg {
            Ipc::ViewerInput { session_id, .. }
            | Ipc::ViewerKeyframe { session_id, .. }
            | Ipc::ViewerReport { session_id, .. } => *session_id,
            _ => return Err(ApplyError::NotAViewerRequest),
        };
        self.viewer(session_id).ok_or(ApplyError::NoSuchSession)?.apply(msg)
    }

    // ---------- terminals (stage 43) ----------

    /// Gives the core what only the machine knows about terminals: who is logged in at the console, and the
    /// administrator spawner (the service's own SYSTEM token). Once: a second call is ignored. Until it is called
    /// nobody is logged in (user terminals are refused `NOBODY_PRESENT`) and there is no administrator terminal.
    pub fn set_terminal_env(&self, env: Arc<dyn TerminalEnv>) -> bool {
        self.term_env.set(env)
    }

    /// The account the terminals see at the console right now (what a `TermOpen` is checked against); `None` while
    /// nobody is, or before [`ServiceCore::set_terminal_env`] is called.
    pub fn terminal_console_user(&self) -> Option<String> {
        self.term_env.console_user()
    }

    // ---------- swap, chat (stage 41) ----------

    /// Gives the core the interface it reports swap questions and chat to (the UI hub does it in `bind_core`). Once:
    /// a second call is ignored. Until it is called a swap question is declined and chat events are dropped.
    pub fn set_extras(&self, hooks: Arc<dyn ExtrasHooks>) -> bool {
        self.hooks.set(hooks)
    }

    /// The session of either role, for the swap and chat calls.
    fn session_ctx(&self, session_id: u64) -> Option<swapdesk_session::SessionCtx> {
        self.viewer(session_id).map(|v| v.ctx()).or_else(|| self.host.session(session_id))
    }

    /// "Inverter": asks the other side of session `session_id` (either role) to swap. The future owns what it needs, so
    /// a task can run it without holding the core; it resolves with how the swap ended (up to 22 s).
    pub fn swap_request(
        &self,
        session_id: u64,
    ) -> Result<impl Future<Output = SwapOutcome> + Send + 'static, ApplyError> {
        let ctx = self.session_ctx(session_id).ok_or(ApplyError::NoSuchSession)?;
        let swap = self.swap.clone();
        Ok(async move { swap.request(&ctx).await })
    }

    /// [`swap_request`](Self::swap_request), awaited.
    pub async fn request_swap(&self, session_id: u64) -> Result<SwapOutcome, ApplyError> {
        Ok(self.swap_request(session_id)?.await)
    }

    /// Whether a swap is in flight on the session.
    pub fn swap_pending(&self, session_id: u64) -> bool {
        self.session_ctx(session_id).is_some_and(|ctx| self.swap.pending(&ctx))
    }

    /// Sends a chat message in session `session_id` (either role); the future resolves with it as stored.
    pub fn chat_send_task(
        &self,
        session_id: u64,
        text: String,
    ) -> Result<impl Future<Output = Result<ChatEntry, ChatError>> + Send + 'static, ApplyError> {
        let ctx = self.session_ctx(session_id).ok_or(ApplyError::NoSuchSession)?;
        let now = self.dialer.wall.unix_ms();
        let (viewer, router) = (self.viewer(session_id), Arc::clone(&self.host_chat));
        Ok(async move {
            match viewer {
                Some(v) => v.chat_send(&text).await,
                None => router.send(&ctx, &text, now).await,
            }
        })
    }

    /// [`chat_send_task`](Self::chat_send_task), awaited.
    pub async fn chat_send(&self, session_id: u64, text: &str) -> Result<ChatEntry, ChatSendError> {
        let task = self.chat_send_task(session_id, text.to_owned()).map_err(|_| ChatSendError::NoSuchSession)?;
        task.await.map_err(ChatSendError::Chat)
    }

    /// The chat panel shows everything the peer sent: the future tells the peer (`ChatRead`).
    pub fn chat_read_task(&self, session_id: u64) -> Result<impl Future<Output = ()> + Send + 'static, ApplyError> {
        let ctx = self.session_ctx(session_id).ok_or(ApplyError::NoSuchSession)?;
        let (viewer, router) = (self.viewer(session_id), Arc::clone(&self.host_chat));
        Ok(async move {
            match viewer {
                Some(v) => v.chat_mark_read().await,
                None => router.mark_read(&ctx).await,
            }
        })
    }

    /// [`chat_read_task`](Self::chat_read_task), awaited.
    pub async fn chat_mark_read(&self, session_id: u64) -> Result<(), ApplyError> {
        self.chat_read_task(session_id)?.await;
        Ok(())
    }

    /// The messages of session `session_id` so far (empty after it ends).
    pub fn chat_entries(&self, session_id: u64) -> Vec<ChatEntry> {
        match self.viewer(session_id) {
            Some(v) => v.chat_entries(),
            None => self.host_chat.session(session_id).map(|h| h.entries()).unwrap_or_default(),
        }
    }

    // ---------- clipboard and quick actions (stage 42) ----------

    /// Gives the core what it needs from the interface for the clipboard and quick actions (the UI hub does it in
    /// `bind_core`). Once: a second call is ignored. Until it is called a viewer session's clipboard has no UI to
    /// serve it and nothing is audited.
    pub fn set_clip_actions(&self, hooks: Arc<dyn ClipActionHooks>) -> bool {
        self.clip_hooks.set(hooks)
    }

    /// "Copiar e colar" of session `session_id` (either role): this side's flag; the peer is told.
    pub fn set_clipboard_sync(&self, session_id: u64, on: bool) -> Result<(), ApplyError> {
        if let Some(v) = self.viewer(session_id) {
            v.set_clipboard_sync(on);
            return Ok(());
        }
        let ctx = self.host.session(session_id).ok_or(ApplyError::NoSuchSession)?;
        self.host_clip.set_enabled(&ctx, on);
        Ok(())
    }

    /// Whether this side's clipboard sync is on for session `session_id` (either role).
    pub fn clipboard_sync_on(&self, session_id: u64) -> bool {
        match self.viewer(session_id) {
            Some(v) => v.clipboard_sync_on(),
            None => self.host_clip.enabled(),
        }
    }

    // ---------- end ----------

    /// Ends every session (`Bye { HOST_SHUTDOWN }`), tells the helpers to shut down, closes the endpoints and stops
    /// the runtime. Call it from a thread that is not on the runtime. Dropping the core does the same.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let Some(runtime) = self.runtime.take() else { return };
        self.host.shutdown();
        for kind in [HelperKind::System, HelperKind::User] {
            if let Some(h) = self.shared.helper(kind) {
                h.stop();
            }
        }
        // The sessions say their `Bye` on their own tasks: give them a moment before the runtime goes.
        let host = self.host.clone();
        runtime.block_on(async move {
            let until = tokio::time::Instant::now() + Duration::from_secs(1);
            while !host.sessions().is_empty() && tokio::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        self.listeners.close_all();
        runtime.shutdown_timeout(Duration::from_secs(2));
    }
}

impl Drop for ServiceCore {
    fn drop(&mut self) {
        self.stop();
    }
}
