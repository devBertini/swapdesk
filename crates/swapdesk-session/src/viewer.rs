//! The viewer side of a session: `Hello`, the wait for the host's decision, and the live session afterwards.

use std::sync::Arc;
use std::time::Duration;

use quinn::{Connection, Endpoint};
use swapdesk_platform::Clock;
use swapdesk_proto::{
    Capabilities, Desktop, ErrorCode, GrantMode, Hello, HelloAck, Intent, Message, Os, PROTO_MINOR, PermissionSet,
    Resume,
};
use swapdesk_trust::{DeviceId, Identity, Spki, connect_session, device_id};
use tokio::sync::{mpsc, oneshot};

use crate::actor::{Actor, Side};
use crate::caps::Caps;
use crate::ctx::{CtxParts, SessionCtx, SessionEnd};
use crate::host::now;
use crate::link::ControlIo;
use crate::liveness::LivenessConfig;
use crate::policy::Role;
use crate::registry::Registry;

#[derive(Clone)]
pub struct ViewerConfig {
    pub name: String,
    pub os: Os,
    pub os_version: String,
    pub app_version: String,
    /// The protocol minor this viewer announces in its `Hello`: this build's own. A test that plays a viewer
    /// of 1.0 sets 0.
    pub proto_minor: u16,
    pub intent: Intent,
    /// What the viewer asks for. The viewer's service sets `UNATTENDED` or `ADMIN_TERMINAL` only for a local
    /// administrator (D10, F-PERM-03).
    pub requested: PermissionSet,
    /// Resume the session after a lost connection (6.4).
    pub resume: Option<Resume>,
    /// Enter a swap (6.3).
    pub swap_token: Option<[u8; 16]>,
    pub capabilities: Capabilities,
    pub liveness: LivenessConfig,
    /// How long to wait for the host's decision: the prompt's 20 s and room for the handshake.
    pub grant_wait: Duration,
    pub caps: Caps,
    pub clock: Arc<dyn Clock>,
    /// The handlers for what the host sends (video, cursor, clipboard, chat, files).
    pub registry: Registry,
    /// Called with the countdown when the host starts a prompt ("Aguardando <host>"), before the session opens.
    pub on_prompt: Option<Arc<dyn Fn(u16) + Send + Sync>>,
    /// What this viewer's account has stored for the host (`grants_here` of the host's record): told to a host of
    /// minor 1 with `StoredGrants` when the grant comes. `None`: not known, nothing is told.
    pub stored_grants: Option<PermissionSet>,
}

impl ViewerConfig {
    pub fn new(
        name: &str,
        os: Os,
        os_version: &str,
        app_version: &str,
        capabilities: Capabilities,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            name: name.into(),
            os,
            os_version: os_version.into(),
            app_version: app_version.into(),
            proto_minor: PROTO_MINOR,
            intent: Intent::Control,
            requested: PermissionSet::CONTROL,
            resume: None,
            swap_token: None,
            capabilities,
            liveness: LivenessConfig::default(),
            grant_wait: Duration::from_secs(25),
            caps: Caps::default(),
            clock,
            registry: Registry::new(),
            on_prompt: None,
            stored_grants: None,
        }
    }

    fn hello(&self) -> Hello {
        Hello {
            proto_minor: self.proto_minor,
            app_version: self.app_version.clone(),
            name: self.name.clone(),
            os: self.os,
            os_version: self.os_version.clone(),
            intent: self.intent,
            requested: self.requested,
            resume: self.resume.clone(),
            swap_token: self.swap_token,
        }
    }
}

/// Why a session did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenError {
    /// The host said no: `DECLINED`, `TIMEOUT`, `NOBODY_PRESENT`, `NOT_PERMITTED`, `BUSY`, `UNKNOWN_DEVICE`.
    Refused(ErrorCode),
    /// A violation of the format, by either side.
    Protocol(ErrorCode),
    /// No decision in time.
    Timeout,
    /// The connection was lost.
    Lost,
    /// The host ended the connection (with this code) before deciding.
    Closed(ErrorCode),
}

/// What the host granted.
#[derive(Clone, PartialEq, Eq)]
pub struct GrantInfo {
    pub session_id: u64,
    pub granted: PermissionSet,
    pub mode: GrantMode,
    /// Keep it to resume this session after a lost connection: `Hello { resume }`.
    pub resume: Resume,
    pub host: HelloAck,
}

/// The resume token is a secret: it never reaches a log through `{:?}`.
impl std::fmt::Debug for GrantInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrantInfo")
            .field("session_id", &self.session_id)
            .field("granted", &self.granted)
            .field("mode", &self.mode)
            .field("resume", &format_args!("Resume {{ session_id: {}, token: <redacted> }}", self.resume.session_id))
            .field("host", &self.host)
            .finish()
    }
}

/// What happens in a live session that the viewer's UI hears of. Messages no handler owns arrive as `Message`.
#[derive(Clone, PartialEq, Eq)]
pub enum ViewerEvent {
    PromptPending {
        timeout_s: u16,
    },
    PermissionsChanged(PermissionSet),
    /// What the host's account has stored for this viewer (`StoredGrants`, minor 1): display only.
    PeerGrants(PermissionSet),
    HostState {
        desktop: Desktop,
        paused: bool,
    },
    /// A silent host: no message for the liveness limit.
    PeerOffline,
    PeerOnline,
    Message(Message),
    Ended(SessionEnd),
}

/// A message can carry a chat text, a clipboard item or a token: the log gets its type and nothing of its body.
impl std::fmt::Debug for ViewerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PromptPending { timeout_s } => f.debug_struct("PromptPending").field("timeout_s", timeout_s).finish(),
            Self::PermissionsChanged(g) => f.debug_tuple("PermissionsChanged").field(g).finish(),
            Self::PeerGrants(g) => f.debug_tuple("PeerGrants").field(g).finish(),
            Self::HostState { desktop, paused } => {
                f.debug_struct("HostState").field("desktop", desktop).field("paused", paused).finish()
            }
            Self::PeerOffline => f.write_str("PeerOffline"),
            Self::PeerOnline => f.write_str("PeerOnline"),
            Self::Message(m) => write!(f, "Message(type {:#06x}, body not shown)", m.type_id()),
            Self::Ended(end) => f.debug_tuple("Ended").field(end).finish(),
        }
    }
}

pub struct ViewerSession {
    ctx: SessionCtx,
    events: mpsc::Receiver<ViewerEvent>,
    grant: GrantInfo,
    task: tokio::task::JoinHandle<SessionEnd>,
    /// Set by `abandon`: a vanished network says no `Bye`.
    quiet: bool,
}

/// A session that is dropped ends: its `Bye` goes to the host, and its tasks stop with it.
impl Drop for ViewerSession {
    fn drop(&mut self) {
        if !self.quiet {
            self.ctx.bye(ErrorCode::NoError);
        }
    }
}

/// Armed while `open_io` waits for the host's answer: dropped armed, it says `Bye { NO_ERROR }`.
struct UnansweredGuard(Option<SessionCtx>);

impl Drop for UnansweredGuard {
    fn drop(&mut self) {
        if let Some(ctx) = self.0.take() {
            ctx.bye(ErrorCode::NoError);
        }
    }
}

impl ViewerSession {
    /// Dials `addr`, expecting the device whose key is `expected` (PROTOCOL.md 2.2), and opens the session.
    pub async fn connect(
        endpoint: &Endpoint,
        identity: &Identity,
        addr: std::net::SocketAddr,
        expected: Spki,
        cfg: ViewerConfig,
    ) -> Result<Self, OpenError> {
        Self::connect_within(endpoint, identity, addr, expected, cfg, Duration::from_secs(10)).await
    }

    /// As [`connect`](Self::connect), with the QUIC handshake bounded by `handshake` (the decision after it is not).
    /// A dial that runs while the link is down sends its first packet at once and then waits for QUIC's own
    /// retransmissions, which come after 1 s, 3 s, 7 s: a link that returns in a gap is not noticed until the
    /// next one. A caller that redials (the reconnect of F-NET-01) bounds each attempt and starts a fresh one instead.
    pub async fn connect_within(
        endpoint: &Endpoint,
        identity: &Identity,
        addr: std::net::SocketAddr,
        expected: Spki,
        cfg: ViewerConfig,
        handshake: Duration,
    ) -> Result<Self, OpenError> {
        let conn = tokio::time::timeout(handshake, connect_session(endpoint, identity, addr, expected))
            .await
            .map_err(|_| OpenError::Lost)?
            .map_err(|_| OpenError::Lost)?;
        Self::open(conn, Some(device_id(&expected)), cfg).await
    }

    /// Opens the session on a connected, gated connection: the control stream is this side's first bidirectional one.
    pub async fn open(conn: Connection, host: Option<DeviceId>, cfg: ViewerConfig) -> Result<Self, OpenError> {
        let opening = tokio::time::timeout(Duration::from_secs(10), conn.open_bi()).await;
        let (send, recv) = match opening {
            Ok(Ok(streams)) => streams,
            Ok(Err(_)) => return Err(OpenError::Lost),
            Err(_) => return Err(OpenError::Timeout),
        };
        Self::open_io(ControlIo::from_quinn(send, recv), Some(conn), host, cfg).await
    }

    /// The same over any control link (the in-memory pair of the tests).
    pub async fn open_io(
        io: ControlIo,
        conn: Option<Connection>,
        host: Option<DeviceId>,
        cfg: ViewerConfig,
    ) -> Result<Self, OpenError> {
        let ViewerConfig {
            liveness, grant_wait, caps, clock, registry, on_prompt, capabilities, stored_grants, ..
        } = cfg.clone();
        io.out.send(cfg.hello()).await.map_err(|_| OpenError::Lost)?;
        io.out.send(capabilities).await.map_err(|_| OpenError::Lost)?;
        let (ev_tx, ev_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (opened_tx, opened_rx) = oneshot::channel();
        let ctx = SessionCtx::new(CtxParts {
            role: Role::Viewer,
            device: host,
            out: io.out.clone(),
            conn: conn.clone(),
            caps_cfg: caps,
            ev: ev_tx.clone(),
            clock,
            consent: None,
        });
        if let Some(grants) = stored_grants {
            ctx.set_own_stored(grants);
        }
        // What this viewer asked for caps what it holds of the host's grant (F2 of the review of 13/11).
        ctx.set_requested(cfg.requested);
        let side = Side::Viewer {
            events: events_tx,
            opened: Some(opened_tx),
            deadline: now() + grant_wait,
            peer: None,
            on_prompt,
            own_minor: cfg.proto_minor,
        };
        let actor = Actor::new(side, ctx.clone(), io, conn, ev_tx, ev_rx, Arc::new(registry), liveness, now());
        let task = tokio::spawn(actor.run());
        // A caller that gives up before the host answers (the future is dropped) ends the attempt with a `Bye`, so the
        // host closes its question instead of leaving it open until its own deadline.
        let mut unanswered = UnansweredGuard(Some(ctx.clone()));
        // The state machine has its own deadline; this one only guards against a task that never answers.
        let waited = tokio::time::timeout(grant_wait + Duration::from_secs(5), opened_rx).await;
        unanswered.0 = None;
        match waited {
            Ok(Ok(Ok(grant))) => Ok(Self { ctx, events: events_rx, grant, task, quiet: false }),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => Err(OpenError::Lost),
            Err(_) => {
                ctx.bye(ErrorCode::Timeout);
                Err(OpenError::Timeout)
            }
        }
    }

    pub fn ctx(&self) -> &SessionCtx {
        &self.ctx
    }

    pub fn grant(&self) -> &GrantInfo {
        &self.grant
    }

    /// What the session may do now (it changes with `PermissionsChanged`).
    pub fn granted(&self) -> PermissionSet {
        self.ctx.granted()
    }

    /// The next event; `None` when the session is over and everything was read.
    pub async fn next_event(&mut self) -> Option<ViewerEvent> {
        self.events.recv().await
    }

    /// An event already waiting, if any.
    pub fn try_event(&mut self) -> Option<ViewerEvent> {
        self.events.try_recv().ok()
    }

    /// Drops the session without a `Bye`, as a vanished network would: the host sees a lost connection (and may
    /// allow a resume). Over QUIC the connection itself is left to time out, so tests that need the host to notice use
    /// a dead link instead; over an in-memory link the host notices at once.
    pub fn abandon(mut self) {
        self.quiet = true;
        self.task.abort();
    }

    /// "Desconectar": `Bye { NO_ERROR }`, then waits for the session to end (at most 3 s).
    pub async fn disconnect(self) -> SessionEnd {
        self.ctx.bye(ErrorCode::NoError);
        tokio::time::timeout(Duration::from_secs(3), self.ctx.wait_ended())
            .await
            .unwrap_or(SessionEnd { kind: crate::ctx::EndKind::Timeout, code: ErrorCode::Timeout })
    }
}
