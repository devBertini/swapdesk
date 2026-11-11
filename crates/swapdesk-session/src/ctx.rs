//! What a feature crate and the service see of one running session: [`SessionCtx`].

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use quinn::{Connection, RecvStream, SendStream};
use swapdesk_platform::Clock;
use swapdesk_proto::{
    Capabilities, ConsentPending, Desktop, ErrorCode, Feature, Message, PermissionSet, StreamKind, Wire,
};
use swapdesk_trust::DeviceId;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{AbortHandle, JoinHandle};

use crate::caps::{CapTracker, CapVerdict, Caps};
use crate::link::{Outbox, SendError};
use crate::policy::{Role, priority_of, stream_allowed};
use crate::util::lock;

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndKind {
    /// We said `Bye` ("Desconectar", a swap, a forgotten device, a shutdown).
    LocalBye,
    /// The peer said `Bye`.
    PeerBye,
    /// The host refused the session (or, on the viewer, was refused): `code` says why.
    Refused,
    /// The connection was lost (idle timeout, transport error, a silent peer): a resume may follow.
    Lost,
    /// The peer broke the protocol (or we found its message wrong): fatal, `code` says how.
    Violation,
    /// The peer closed the connection without a `Bye`.
    PeerClosed,
    /// The handshake or the decision did not finish in time.
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionEnd {
    pub kind: EndKind,
    pub code: ErrorCode,
}

/// The desktop the host's input and video are on, and whether they are paused (F-SESS-01: without `UNATTENDED`
/// the host pauses video and drops input while the desktop is not `Default`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopState {
    pub desktop: Desktop,
    pub paused: bool,
}

/// Messages to the state machine of a session: what the peer, the timer, the connection and the decision task send
/// it. What the service layer orders (a `Bye`, a grant change, a desktop switch) does not travel here: a peer can
/// fill this queue, so those go through [`Control`], which the state machine looks at first.
pub(crate) enum Ev {
    In(crate::link::Inbound),
    Tick,
    ConnClosed(quinn::ConnectionError),
    Decision(crate::host::Decision),
    /// A stream the router checked: it goes to its handler on the state machine's task, in order with the rest.
    Stream(crate::registry::IncomingStream),
}

/// Orders to a session that must not be lost behind the peer's messages. The state machine reads them before every
/// event; each setter wakes it.
#[derive(Default)]
pub(crate) struct Control {
    /// `Bye` with this code (the first one wins).
    pub bye: Option<ErrorCode>,
    /// A protocol violation found outside the control stream (a bad stream).
    pub violation: Option<ErrorCode>,
}

/// How long a stream open may wait for the peer to allow another stream.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

struct Gated {
    required: PermissionSet,
    handle: AbortHandle,
}

/// What the host asks of its user for a feature (D13), and for how long.
pub(crate) struct ConsentSource {
    pub ui: Arc<dyn crate::host::HostUi>,
    pub timeout: Duration,
    pub device: DeviceId,
}

pub(crate) struct CtxInner {
    pub role: Role,
    pub session_id: AtomicU64,
    pub device: Option<DeviceId>,
    pub out: Outbox,
    pub grants: watch::Sender<PermissionSet>,
    pub state: watch::Sender<DesktopState>,
    pub conn: Option<Connection>,
    tasks: Mutex<Vec<Gated>>,
    pub caps_cfg: Caps,
    pub caps: Mutex<CapTracker>,
    pub active: watch::Sender<bool>,
    pub ended: watch::Sender<Option<SessionEnd>>,
    pub ev: mpsc::Sender<Ev>,
    pub clock: Arc<dyn Clock>,
    pub rtt_us: AtomicU64,
    pub consent: Option<ConsentSource>,
    pub peer_caps: Mutex<Option<Capabilities>>,
    pub control: Mutex<Control>,
    /// Wakes the state machine when `control`, the grants or `desktop_req` changed.
    pub wake: Notify,
    /// The desktop the service last announced (`None` until it did); the state machine applies a change.
    pub desktop_req: watch::Sender<Option<Desktop>>,
    /// What the viewer's `Hello` asked for (host side; bits of a `PermissionSet`).
    requested: AtomicU16,
    /// The protocol minor the peer announced in its `Hello` or `HelloAck` (0 until it did): nothing newer than it
    /// is sent to that peer.
    peer_minor: AtomicU16,
    /// The session entered without a prompt (host side): the grant mode `Unattended`, or a resume of such a session.
    /// It is not the `UNATTENDED` permission, which a viewer of minor 1 holds only after it elevated.
    entered_unattended: AtomicBool,
    /// The stored grants of the device as the session last saw them (host side): a change of what is stored removes
    /// only what it revoked from them.
    pub stored_seen: Mutex<PermissionSet>,
    /// What this side's account has stored for the peer's device (`grants_here` of its record): told to a peer of
    /// minor 1 with `StoredGrants` after the grant and again on every change. `None`: not known, nothing is told.
    pub own_stored: Mutex<Option<PermissionSet>>,
    /// Set when the session starts to close: nothing is started for it from then on.
    closing: AtomicBool,
}

/// One session, seen from a handler or from the service. Cheap to clone; every clone is the same session.
#[derive(Clone)]
pub struct SessionCtx {
    pub(crate) inner: Arc<CtxInner>,
}

pub(crate) struct CtxParts {
    pub role: Role,
    pub device: Option<DeviceId>,
    pub out: Outbox,
    pub conn: Option<Connection>,
    pub caps_cfg: Caps,
    pub ev: mpsc::Sender<Ev>,
    pub clock: Arc<dyn Clock>,
    pub consent: Option<ConsentSource>,
}

impl SessionCtx {
    pub(crate) fn new(p: CtxParts) -> Self {
        let (grants, _) = watch::channel(PermissionSet::EMPTY);
        let (state, _) = watch::channel(DesktopState { desktop: Desktop::Default, paused: false });
        let (ended, _) = watch::channel(None);
        let (active, _) = watch::channel(false);
        let (desktop_req, _) = watch::channel(None);
        Self {
            inner: Arc::new(CtxInner {
                role: p.role,
                session_id: AtomicU64::new(0),
                device: p.device,
                out: p.out,
                grants,
                state,
                conn: p.conn,
                tasks: Mutex::new(Vec::new()),
                caps_cfg: p.caps_cfg,
                caps: Mutex::new(CapTracker::default()),
                active,
                ended,
                ev: p.ev,
                clock: p.clock,
                rtt_us: AtomicU64::new(0),
                consent: p.consent,
                peer_caps: Mutex::new(None),
                control: Mutex::new(Control::default()),
                wake: Notify::new(),
                desktop_req,
                requested: AtomicU16::new(0),
                peer_minor: AtomicU16::new(0),
                entered_unattended: AtomicBool::new(false),
                stored_seen: Mutex::new(PermissionSet::EMPTY),
                own_stored: Mutex::new(None),
                closing: AtomicBool::new(false),
            }),
        }
    }

    pub fn role(&self) -> Role {
        self.inner.role
    }

    /// The session's id; 0 until the grant.
    pub fn session_id(&self) -> u64 {
        self.inner.session_id.load(Ordering::SeqCst)
    }

    /// The peer's device (known on the host from the gate; on the viewer it is the device it dialled).
    pub fn device(&self) -> Option<DeviceId> {
        self.inner.device
    }

    /// True from the grant until the session ends.
    pub fn is_active(&self) -> bool {
        *self.inner.active.borrow() && self.inner.ended.borrow().is_none()
    }

    /// Waits up to `limit` for the grant: a stream of the other side can overtake the control message that
    /// announces the grant, so a stream that arrives first waits for it instead of being called early.
    pub(crate) async fn wait_active(&self, limit: Duration) -> bool {
        let mut rx = self.inner.active.subscribe();
        let wait = async {
            loop {
                if *rx.borrow_and_update() {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        };
        tokio::time::timeout(limit, wait).await.unwrap_or(false) && self.inner.ended.borrow().is_none()
    }

    /// What the viewer's `Hello` asked for (host side): an administrator terminal needs it as well as the grant (D10).
    pub fn requested(&self) -> PermissionSet {
        PermissionSet::closure(self.inner.requested.load(Ordering::SeqCst))
    }

    pub(crate) fn set_requested(&self, requested: PermissionSet) {
        self.inner.requested.store(requested.bits(), Ordering::SeqCst);
    }

    /// The protocol minor the peer announced (`Hello.proto_minor` on the host, `HelloAck.proto_minor` on the
    /// viewer); 0 before that message, which is also what a 1.0 peer says.
    pub fn peer_minor(&self) -> u16 {
        self.inner.peer_minor.load(Ordering::SeqCst)
    }

    pub(crate) fn set_peer_minor(&self, minor: u16) {
        self.inner.peer_minor.store(minor, Ordering::SeqCst);
        self.inner.out.set_peer_minor(minor);
    }

    /// True for a host session that entered without a prompt (`GrantMode::Unattended`) and for a resume of one.
    /// What D13 asks about: whether somebody at the host accepted this session.
    pub fn entered_unattended(&self) -> bool {
        self.inner.entered_unattended.load(Ordering::SeqCst)
    }

    pub(crate) fn set_entered_unattended(&self, unattended: bool) {
        self.inner.entered_unattended.store(unattended, Ordering::SeqCst);
    }

    /// What this session may do now.
    /// What this side has stored for the peer's device changed (or is known now): a live session with a peer of
    /// minor 1 tells it (`StoredGrants`, GRANTS-SYNC.md 3.2). It is about the stores, never about this session.
    pub fn set_own_stored(&self, grants: PermissionSet) {
        *lock(&self.inner.own_stored) = Some(grants);
        self.inner.wake.notify_one();
    }

    pub(crate) fn own_stored(&self) -> Option<PermissionSet> {
        *lock(&self.inner.own_stored)
    }

    pub fn granted(&self) -> PermissionSet {
        *self.inner.grants.borrow()
    }

    /// A watch on the grants: a task that must stop when a permission goes watches this.
    pub fn watch_granted(&self) -> watch::Receiver<PermissionSet> {
        self.inner.grants.subscribe()
    }

    pub fn desktop_state(&self) -> DesktopState {
        *self.inner.state.borrow()
    }

    /// True while the host is paused (a locked, secure or login desktop in a session without `UNATTENDED`): the
    /// video sender sends nothing and input is dropped.
    pub fn is_paused(&self) -> bool {
        self.desktop_state().paused
    }

    pub fn watch_state(&self) -> watch::Receiver<DesktopState> {
        self.inner.state.subscribe()
    }

    /// Whether input may be injected now: `CONTROL` granted and the host not paused (D11, F-SESS-01).
    pub fn can_inject(&self) -> bool {
        self.is_active() && self.granted().contains(PermissionSet::CONTROL) && !self.is_paused()
    }

    /// Smoothed round-trip time from the session's own `Ping`/`Pong`, if one completed.
    pub fn rtt(&self) -> Option<Duration> {
        match self.inner.rtt_us.load(Ordering::SeqCst) {
            0 => None,
            us => Some(Duration::from_micros(us)),
        }
    }

    /// The peer's `Capabilities`, once received.
    pub fn peer_capabilities(&self) -> Option<Capabilities> {
        lock(&self.inner.peer_caps).clone()
    }

    /// The QUIC connection (to open the streams a feature uses). `None` on a test link.
    pub fn connection(&self) -> Option<&Connection> {
        self.inner.conn.as_ref()
    }

    /// Opens a unidirectional stream of `kind` to the peer: the kind is written first and the quinn priority of
    /// PROTOCOL.md 4 is set (input 4, cursor 2, video 1, clipboard and file data 0). Refuses a kind the peer may not
    /// receive (`INTERNAL`: a bug of the caller), a session that is not live (`UNAVAILABLE`) and a peer that allows
    /// no more streams for 5 s (`TIMEOUT`).
    pub async fn open_uni(&self, kind: StreamKind) -> Result<SendStream, ErrorCode> {
        self.check_open(kind, false)?;
        let conn = self.inner.conn.as_ref().ok_or(ErrorCode::Unavailable)?;
        let mut send = match tokio::time::timeout(OPEN_TIMEOUT, conn.open_uni()).await {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => return Err(ErrorCode::Unavailable),
            Err(_) => return Err(ErrorCode::Timeout),
        };
        let _ = send.set_priority(priority_of(kind));
        let prefix = kind.encode().map_err(|_| ErrorCode::Internal)?;
        send.write_all(&prefix).await.map_err(|_| ErrorCode::Unavailable)?;
        Ok(send)
    }

    /// Opens a bidirectional stream of `kind` (the terminal): as `open_uni`, with the kind written first.
    pub async fn open_bi(&self, kind: StreamKind) -> Result<(SendStream, RecvStream), ErrorCode> {
        self.check_open(kind, true)?;
        let conn = self.inner.conn.as_ref().ok_or(ErrorCode::Unavailable)?;
        let (mut send, recv) = match tokio::time::timeout(OPEN_TIMEOUT, conn.open_bi()).await {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => return Err(ErrorCode::Unavailable),
            Err(_) => return Err(ErrorCode::Timeout),
        };
        let _ = send.set_priority(priority_of(kind));
        let prefix = kind.encode().map_err(|_| ErrorCode::Internal)?;
        send.write_all(&prefix).await.map_err(|_| ErrorCode::Unavailable)?;
        Ok((send, recv))
    }

    fn check_open(&self, kind: StreamKind, bidi: bool) -> Result<(), ErrorCode> {
        let peer = match self.role() {
            Role::Host => Role::Viewer,
            Role::Viewer => Role::Host,
        };
        if !stream_allowed(peer, kind, bidi) {
            return Err(ErrorCode::Internal);
        }
        if !self.is_active() {
            return Err(ErrorCode::Unavailable);
        }
        Ok(())
    }

    /// Sends a control message. Chat past its cap is `Busy`.
    pub async fn send(&self, msg: impl Into<Message>) -> Result<(), SendError> {
        let msg = msg.into();
        if lock(&self.inner.caps).on_outgoing(&self.inner.caps_cfg, &msg) == CapVerdict::Busy {
            return Err(SendError::Busy);
        }
        self.inner.out.send(msg).await
    }

    /// Like `send`, without waiting for room.
    pub fn try_send(&self, msg: impl Into<Message>) -> Result<(), SendError> {
        let msg = msg.into();
        if lock(&self.inner.caps).on_outgoing(&self.inner.caps_cfg, &msg) == CapVerdict::Busy {
            return Err(SendError::Busy);
        }
        self.inner.out.try_send(msg)
    }

    /// A handler refused a `TermOpen` that passed the session's caps (host side): the terminal's slot is free again.
    pub fn release_terminal(&self, term_id: u32) {
        lock(&self.inner.caps).release_terminal(term_id);
    }

    /// Ends the session with a `Bye` carrying `code` (`NO_ERROR` is "Desconectar"). It cannot be lost behind the
    /// peer's messages: the state machine reads it before its next event.
    pub fn bye(&self, code: ErrorCode) {
        lock(&self.inner.control).bye.get_or_insert(code);
        self.inner.wake.notify_one();
    }

    /// The host's desktop changed (host side): `HostState` follows, and the pause flag with it.
    pub(crate) fn set_desktop(&self, desktop: Desktop) {
        self.inner.desktop_req.send_replace(Some(desktop));
        self.inner.wake.notify_one();
    }

    /// The session is closing: nothing may be started for it any more.
    pub(crate) fn begin_close(&self) {
        self.inner.closing.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.inner.closing.load(Ordering::SeqCst)
    }

    /// Runs a task for this session; it is aborted when the session ends.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_gated(PermissionSet::EMPTY, fut);
    }

    /// Runs a task that needs `required`: it is aborted the moment the session no longer holds all of it
    /// (F-PERM-02: revocation stops a feature, whether or not the handler cooperates), and not started at all when
    /// the session does not hold it now. Returns whether it was started.
    pub fn spawn_gated<F>(&self, required: PermissionSet, fut: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.is_closing() || self.inner.ended.borrow().is_some() || !self.granted().contains(required) {
            return false;
        }
        let handle: JoinHandle<()> = tokio::spawn(fut);
        let mut tasks = lock(&self.inner.tasks);
        tasks.retain(|g| !g.handle.is_finished());
        tasks.push(Gated { required, handle: handle.abort_handle() });
        drop(tasks);
        // A revocation, or the end of the session, that landed between the check and the push must not leave the
        // task running: the session's `abort_all` may already have gone past it.
        if self.is_closing() {
            self.abort_all();
            return false;
        }
        self.abort_revoked();
        true
    }

    pub(crate) fn abort_revoked(&self) {
        let now = self.granted();
        lock(&self.inner.tasks).retain(|g| {
            if g.handle.is_finished() {
                false
            } else if now.contains(g.required) {
                true
            } else {
                g.handle.abort();
                false
            }
        });
    }

    pub(crate) fn abort_all(&self) {
        for g in lock(&self.inner.tasks).drain(..) {
            g.handle.abort();
        }
    }

    pub(crate) fn set_granted(&self, new: PermissionSet) {
        self.inner.grants.send_replace(new);
        self.abort_revoked();
        self.inner.wake.notify_one();
    }

    /// Changes the grants in one step, so two changes at once (the banner and Permissões) cannot overwrite each
    /// other: `f` sees the current set and returns the new one. What went is stopped now; the state machine tells
    /// the viewer and the handlers.
    pub(crate) fn modify_granted(&self, f: impl FnOnce(PermissionSet) -> PermissionSet) {
        self.inner.grants.send_modify(|g| *g = f(*g));
        self.abort_revoked();
        self.inner.wake.notify_one();
    }

    pub(crate) fn violation(&self, code: ErrorCode) {
        lock(&self.inner.control).violation.get_or_insert(code);
        self.inner.wake.notify_one();
    }

    /// Resolves with how the session ended.
    pub async fn wait_ended(&self) -> SessionEnd {
        let mut rx = self.inner.ended.subscribe();
        loop {
            if let Some(end) = *rx.borrow_and_update() {
                return end;
            }
            if rx.changed().await.is_err() {
                return SessionEnd { kind: EndKind::Lost, code: ErrorCode::NoError };
            }
        }
    }

    /// The end, if the session already ended.
    pub fn ended(&self) -> Option<SessionEnd> {
        *self.inner.ended.borrow()
    }

    /// Host only (D13): asks the host's user to allow `feature` for this session. Tells the viewer
    /// (`ConsentPending`), shows the question, and answers `Ok` when the user agrees, else the refusal code:
    /// `NOBODY_PRESENT` at once when nobody is at the host, `DECLINED`, or `TIMEOUT` after the consent timeout
    /// (20 s by default).
    pub async fn request_consent(&self, feature: Feature) -> Result<(), ErrorCode> {
        let Some(src) = &self.inner.consent else { return Err(ErrorCode::Unavailable) };
        if !src.ui.somebody_present() {
            src.ui.event(crate::host::HostEvent::ConsentAsked {
                device: src.device,
                feature,
                answer: ErrorCode::NobodyPresent,
            });
            return Err(ErrorCode::NobodyPresent);
        }
        let secs = u16::try_from(src.timeout.as_secs()).unwrap_or(u16::MAX);
        let _ = self.inner.out.send(ConsentPending { feature, timeout_s: secs }).await;
        let request = crate::host::ConsentRequest { device: src.device, feature, timeout: src.timeout };
        // The card is closed however this future ends: answered, timed out, or dropped with its task.
        let mut card = ConsentCard { ui: Arc::clone(&src.ui), request, how: crate::host::PromptEnd::Cancelled };
        let answer = src.ui.consent(request);
        let code = match tokio::time::timeout(src.timeout, answer).await {
            Ok(Ok(true)) => ErrorCode::NoError,
            Ok(Ok(false)) | Ok(Err(_)) => ErrorCode::Declined,
            Err(_) => ErrorCode::Timeout,
        };
        card.how = if code == ErrorCode::Timeout {
            crate::host::PromptEnd::TimedOut
        } else {
            crate::host::PromptEnd::Answered
        };
        src.ui.event(crate::host::HostEvent::ConsentAsked { device: src.device, feature, answer: code });
        if code == ErrorCode::NoError { Ok(()) } else { Err(code) }
    }
}

/// Closes the consent card of the host's UI when the question ends, whichever way.
struct ConsentCard {
    ui: Arc<dyn crate::host::HostUi>,
    request: crate::host::ConsentRequest,
    how: crate::host::PromptEnd,
}

impl Drop for ConsentCard {
    fn drop(&mut self) {
        self.ui.consent_closed(&self.request, self.how);
    }
}
