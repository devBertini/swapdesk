//! The control-stream state machine of one session (PROTOCOL.md 5.2), host and viewer in one loop.
//!
//! Everything the peer, the timer, the connection and the decision task send reaches the session as an [`Ev`] on
//! one channel; the loop handles one at a time, so a session has no shared mutable state beyond what [`SessionCtx`]
//! exposes. What the service layer orders (a `Bye`, a grant change, a desktop switch) does not wait in that queue,
//! which a peer can fill: it is read first, before every event, from the session's [`Control`] state, and the
//! loop is woken when it changes. Nothing in the loop waits on the peer: replies are dropped when the outgoing queue
//! is full and the messages that must arrive (the grant, `PermissionsChanged`, `HostState`) wait in a short list that
//! is flushed as room appears; a queue full for [`SEND_TIMEOUT`] ends the session as a lost connection.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::{Connection, VarInt};
use swapdesk_proto::{
    Bye, Desktop, DisplayList, ErrorCode, ErrorMessage, HelloAck, HostState, Message, PROTO_MINOR, PermissionSet,
    PermissionsChanged, Ping, Pong, SessionGrant, SessionRefused, WakeInfo,
};
use swapdesk_trust::DeviceId;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::caps::CapVerdict;
use crate::ctx::{DesktopState, EndKind, Ev, SessionCtx, SessionEnd};
use crate::host::{Decision, HostEvent, HostInner, now};
use crate::link::{ControlIo, LinkError, SEND_TIMEOUT, SendError};
use crate::liveness::{Liveness, LivenessConfig, Verdict};
use crate::policy::{self, Role};
use crate::registry::Registry;
use crate::router;
use crate::util::{Either, lock, select2};
use crate::viewer::{GrantInfo, OpenError, ViewerEvent};

/// How often the state machine looks at its timers.
const TICK: Duration = Duration::from_millis(250);
/// How long a closing session waits to hand its last message (`Bye`, `Error`) to the peer.
const SAY_LIMIT: Duration = Duration::from_millis(500);
const CLOSE_LIMIT: Duration = Duration::from_secs(1);
/// How long the side that had the last word waits for the peer to close the connection.
const LINGER_LIMIT: Duration = Duration::from_millis(500);
/// Beyond the prompt and the wait for an old connection, how long the decision task may take before the session gives
/// up on it (a task that crashed never answers).
const DECISION_MARGIN: Duration = Duration::from_secs(5);
/// Messages that must arrive and wait for room in the outgoing queue (the latest `PermissionsChanged` and `HostState`
/// replace their older selves, so a few are enough).
const MAX_PENDING: usize = 16;

pub(crate) enum Side {
    Host {
        service: Arc<HostInner>,
        device: DeviceId,
        hello: Option<swapdesk_proto::Hello>,
        deadline: Instant,
    },
    Viewer {
        events: mpsc::Sender<ViewerEvent>,
        opened: Option<oneshot::Sender<Result<GrantInfo, OpenError>>>,
        deadline: Instant,
        peer: Option<HelloAck>,
        on_prompt: Option<Arc<dyn Fn(u16) + Send + Sync>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    // Host.
    AwaitHello,
    AwaitCaps,
    Deciding,
    // Viewer.
    AwaitAck,
    AwaitHostCaps,
    AwaitGrant,
    Active,
}

/// How a session ends and what is said on the way out.
pub(crate) struct Finish {
    end: SessionEnd,
    say: Option<Message>,
    close: ErrorCode,
}

impl Finish {
    fn lost() -> Self {
        Self { end: SessionEnd { kind: EndKind::Lost, code: ErrorCode::NoError }, say: None, close: ErrorCode::NoError }
    }

    fn local_bye(code: ErrorCode) -> Self {
        Self { end: SessionEnd { kind: EndKind::LocalBye, code }, say: Some(Bye { code }.into()), close: code }
    }

    fn peer_bye(code: ErrorCode) -> Self {
        Self { end: SessionEnd { kind: EndKind::PeerBye, code }, say: None, close: code }
    }

    fn violation(code: ErrorCode) -> Self {
        let say = ErrorMessage { code, fatal: true, about_type: 0, about_id: 0 };
        Self { end: SessionEnd { kind: EndKind::Violation, code }, say: Some(say.into()), close: code }
    }

    fn timeout() -> Self {
        Self {
            end: SessionEnd { kind: EndKind::Timeout, code: ErrorCode::Timeout },
            say: None,
            close: ErrorCode::Timeout,
        }
    }

    /// The host gave up on a decision that never came: the viewer is told, like any other refusal.
    fn decision_timeout() -> Self {
        Self {
            end: SessionEnd { kind: EndKind::Timeout, code: ErrorCode::Timeout },
            say: Some(SessionRefused { code: ErrorCode::Timeout }.into()),
            close: ErrorCode::Timeout,
        }
    }

    /// The host refuses: the viewer is told, then the connection closes normally.
    fn refused_by_host(code: ErrorCode) -> Self {
        Self {
            end: SessionEnd { kind: EndKind::Refused, code },
            say: Some(SessionRefused { code }.into()),
            close: ErrorCode::NoError,
        }
    }

    fn refused_for_viewer(code: ErrorCode) -> Self {
        Self { end: SessionEnd { kind: EndKind::Refused, code }, say: None, close: ErrorCode::NoError }
    }

    fn peer_closed(code: ErrorCode) -> Self {
        Self { end: SessionEnd { kind: EndKind::PeerClosed, code }, say: None, close: ErrorCode::NoError }
    }
}

/// A session task that is dropped or aborted takes its helpers and its tasks with it.
impl Drop for Actor {
    fn drop(&mut self) {
        for h in &self.helpers {
            h.abort();
        }
        self.ctx.begin_close();
        self.ctx.abort_all();
    }
}

fn app_code(code: ErrorCode) -> VarInt {
    VarInt::from_u32(u32::from(code.tag()))
}

pub(crate) struct Actor {
    side: Side,
    ctx: SessionCtx,
    rx: mpsc::Receiver<Ev>,
    registry: Arc<Registry>,
    liveness: Liveness,
    live_cfg: LivenessConfig,
    phase: Phase,
    helpers: Vec<AbortHandle>,
    ping_nonce: u32,
    /// The grants the viewer was last told (host side): a difference from `ctx.granted()` is a change to report.
    reported: PermissionSet,
    /// The desktop the state machine last applied (host side).
    applied_desktop: Option<Desktop>,
    /// Messages that must arrive, waiting for room in the outgoing queue.
    pending: VecDeque<Message>,
    /// Since when the outgoing queue has been full without a message getting through.
    stalled_since: Option<Instant>,
}

impl Actor {
    /// Starts the helper tasks (reader, timer, connection watcher, stream acceptors) and returns the machine, ready
    /// to `run`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        side: Side,
        ctx: SessionCtx,
        io: ControlIo,
        conn: Option<Connection>,
        ev_tx: mpsc::Sender<Ev>,
        ev_rx: mpsc::Receiver<Ev>,
        registry: Arc<Registry>,
        live_cfg: LivenessConfig,
        at: Instant,
    ) -> Self {
        let mut helpers = Vec::new();
        // The reader: control messages into the event channel; the end of the stream is a lost connection.
        {
            let (tx, mut rx) = (ev_tx.clone(), io.rx);
            helpers.push(
                tokio::spawn(async move {
                    while let Some(item) = rx.recv().await {
                        let stop = item.is_err();
                        if tx.send(Ev::In(item)).await.is_err() || stop {
                            return;
                        }
                    }
                    let _ = tx.send(Ev::In(Err(LinkError::Lost))).await;
                })
                .abort_handle(),
            );
        }
        // The timer.
        {
            let tx = ev_tx.clone();
            helpers.push(
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(TICK);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        interval.tick().await;
                        if tx.try_send(Ev::Tick).is_err() && tx.is_closed() {
                            return;
                        }
                    }
                })
                .abort_handle(),
            );
        }
        // The connection: its closing is an event.
        if let Some(conn) = conn {
            let tx = ev_tx;
            helpers.push(
                tokio::spawn(async move {
                    let why = conn.closed().await;
                    let _ = tx.send(Ev::ConnClosed(why)).await;
                })
                .abort_handle(),
            );
        }
        helpers.extend(router::spawn_acceptors(&ctx));
        let phase = match side {
            Side::Host { .. } => Phase::AwaitHello,
            Side::Viewer { .. } => Phase::AwaitAck,
        };
        Self {
            side,
            ctx,
            rx: ev_rx,
            registry,
            liveness: Liveness::new(live_cfg, at),
            live_cfg,
            phase,
            helpers,
            ping_nonce: 0,
            reported: PermissionSet::EMPTY,
            applied_desktop: None,
            pending: VecDeque::new(),
            stalled_since: None,
        }
    }

    pub(crate) async fn run(mut self) -> SessionEnd {
        let finish = loop {
            if let Some(f) = self.poll_control() {
                break f;
            }
            let inner = Arc::clone(&self.ctx.inner);
            match select2(inner.wake.notified(), self.rx.recv()).await {
                Either::Left(()) => {}
                Either::Right(Some(ev)) => {
                    if let Some(f) = self.on_event(ev) {
                        break f;
                    }
                }
                Either::Right(None) => break Finish::lost(),
            }
        };
        self.finish(finish).await
    }

    fn role(&self) -> Role {
        self.ctx.role()
    }

    /// What ends the session when the link is gone: a connection this side closed itself is a local end (nothing to
    /// resume), anything else is a lost connection.
    fn lost(&self) -> Finish {
        match self.ctx.inner.conn.as_ref().and_then(|c| c.close_reason()) {
            Some(quinn::ConnectionError::LocallyClosed) => Finish::local_bye(ErrorCode::NoError),
            _ => Finish::lost(),
        }
    }

    // ---------- sending: nothing here waits for the peer ----------

    fn note_sent(&mut self, ok: bool) {
        if ok {
            self.stalled_since = None;
        } else {
            self.stalled_since.get_or_insert_with(now);
        }
    }

    fn stalled(&self) -> bool {
        self.stalled_since.is_some_and(|t| now().saturating_duration_since(t) >= SEND_TIMEOUT)
    }

    /// A reply to something the peer asked, or a keepalive: dropped when the queue is full (the peer is not
    /// reading); a queue that stays full ends the session.
    fn reply(&mut self, msg: impl Into<Message>) -> Option<Finish> {
        match self.ctx.inner.out.try_send(msg) {
            Ok(()) => self.note_sent(true),
            Err(SendError::Closed) => return Some(self.lost()),
            Err(_) => self.note_sent(false),
        }
        self.stalled().then(Finish::lost)
    }

    /// A message that must arrive (the grant, `PermissionsChanged`, `HostState`): sent now, or kept in order and sent
    /// as room appears. `latest` makes it replace an older message of its type that still waits.
    fn emit(&mut self, msg: impl Into<Message>, latest: bool) -> Option<Finish> {
        let msg = msg.into();
        if self.pending.is_empty() {
            match self.ctx.inner.out.try_send(msg.clone()) {
                Ok(()) => {
                    self.note_sent(true);
                    return None;
                }
                Err(SendError::Closed) => return Some(self.lost()),
                Err(_) => self.note_sent(false),
            }
        }
        if latest && let Some(slot) = self.pending.iter_mut().find(|m| m.type_id() == msg.type_id()) {
            *slot = msg;
        } else if self.pending.len() >= MAX_PENDING {
            return Some(Finish::lost());
        } else {
            self.pending.push_back(msg);
        }
        self.stalled().then(Finish::lost)
    }

    fn flush_pending(&mut self) -> Option<Finish> {
        while let Some(front) = self.pending.front() {
            match self.ctx.inner.out.try_send(front.clone()) {
                Ok(()) => {
                    self.pending.pop_front();
                    self.note_sent(true);
                }
                Err(SendError::Closed) => return Some(self.lost()),
                Err(_) => {
                    self.note_sent(false);
                    break;
                }
            }
        }
        self.stalled().then(Finish::lost)
    }

    // ---------- what the service layer ordered ----------

    /// Reads the orders that must not wait behind the peer's messages: `Bye`, a violation found off the control
    /// stream, a change of grants, a desktop switch.
    fn poll_control(&mut self) -> Option<Finish> {
        let (bye, violation) = {
            let mut c = lock(&self.ctx.inner.control);
            (c.bye.take(), c.violation.take())
        };
        if let Some(code) = bye {
            return Some(Finish::local_bye(code));
        }
        if let Some(code) = violation {
            return Some(Finish::violation(code));
        }
        if !self.pending.is_empty()
            && let Some(f) = self.flush_pending()
        {
            return Some(f);
        }
        if self.phase == Phase::Active && self.role() == Role::Host {
            let granted = self.ctx.granted();
            if granted != self.reported {
                let old = std::mem::replace(&mut self.reported, granted);
                if let Some(f) = self.on_grants_changed(old, granted) {
                    return Some(f);
                }
            }
            let want = *self.ctx.inner.desktop_req.borrow();
            if let Some(desktop) = want
                && Some(desktop) != self.applied_desktop
            {
                self.applied_desktop = Some(desktop);
                if let Some(f) = self.on_desktop(desktop) {
                    return Some(f);
                }
            }
        }
        None
    }

    fn on_event(&mut self, ev: Ev) -> Option<Finish> {
        match ev {
            Ev::In(Ok(msg)) => {
                if self.liveness.heard(now()) && self.phase == Phase::Active {
                    self.peer_online();
                }
                self.on_message(msg)
            }
            Ev::In(Err(LinkError::Violation(code))) => Some(Finish::violation(code)),
            Ev::In(Err(LinkError::Lost)) => Some(self.lost()),
            Ev::ConnClosed(why) => Some(match why {
                // The peer closed on purpose without a `Bye`.
                quinn::ConnectionError::ApplicationClosed(c) => Finish::peer_closed(
                    ErrorCode::from_tag(u16::try_from(c.error_code.into_inner()).unwrap_or(u16::MAX))
                        .unwrap_or(ErrorCode::NoError),
                ),
                // This side closed it on purpose.
                quinn::ConnectionError::LocallyClosed => Finish::local_bye(ErrorCode::NoError),
                // An idle timeout, a reset or a transport error: the connection is lost.
                _ => Finish::lost(),
            }),
            Ev::Tick => self.on_tick(),
            Ev::Decision(d) => self.on_decision(d),
            Ev::Stream(stream) => {
                if self.phase == Phase::Active {
                    router::dispatch(&self.ctx, &self.registry, stream);
                }
                None
            }
        }
    }

    // ---------- timers ----------

    fn on_tick(&mut self) -> Option<Finish> {
        let at = now();
        if let Some(f) = self.flush_pending() {
            return Some(f);
        }
        match self.phase {
            Phase::Active => {
                let tick = self.liveness.tick(at);
                if tick.send_ping {
                    self.ping_nonce = self.ping_nonce.wrapping_add(1);
                    let nonce = self.ping_nonce;
                    // A full queue means the peer is not reading; the liveness verdict below handles the silence.
                    if let Some(f) = self.reply(Ping { nonce, t_send_us: self.ctx.inner.clock.now().0 }) {
                        return Some(f);
                    }
                }
                match tick.verdict {
                    Verdict::Nothing => None,
                    Verdict::WentOffline => {
                        self.peer_offline();
                        None
                    }
                    Verdict::Dead => Some(Finish::lost()),
                }
            }
            _ => {
                let deadline = match &self.side {
                    Side::Host { deadline, .. } | Side::Viewer { deadline, .. } => *deadline,
                };
                (at >= deadline)
                    .then(|| if self.phase == Phase::Deciding { Finish::decision_timeout() } else { Finish::timeout() })
            }
        }
    }

    fn peer_offline(&mut self) {
        match &self.side {
            Side::Host { service, device, .. } => {
                service.ui.event(HostEvent::PeerOffline { device: *device, session_id: self.ctx.session_id() })
            }
            Side::Viewer { events, .. } => {
                let _ = events.try_send(ViewerEvent::PeerOffline);
            }
        }
    }

    fn peer_online(&mut self) {
        match &self.side {
            Side::Host { service, device, .. } => {
                service.ui.event(HostEvent::PeerOnline { device: *device, session_id: self.ctx.session_id() })
            }
            Side::Viewer { events, .. } => {
                let _ = events.try_send(ViewerEvent::PeerOnline);
            }
        }
    }

    // ---------- messages ----------

    fn on_message(&mut self, msg: Message) -> Option<Finish> {
        let role = self.role();
        if self.phase != Phase::Active && !policy::allowed_before_grant(role, &msg) {
            // Nothing but the session and keepalive messages flows before the grant.
            return Some(Finish::violation(ErrorCode::ProtocolViolation));
        }
        if !policy::may_receive(role, &msg) {
            return Some(Finish::violation(ErrorCode::ProtocolViolation));
        }
        match msg {
            Message::Ping(p) => {
                let pong = Pong { nonce: p.nonce, t_send_us: p.t_send_us, t_peer_us: self.ctx.inner.clock.now().0 };
                self.reply(pong)
            }
            Message::Pong(p) => {
                let rtt = self.ctx.inner.clock.now().0.saturating_sub(p.t_send_us).max(1);
                self.ctx.inner.rtt_us.store(rtt, std::sync::atomic::Ordering::SeqCst);
                None
            }
            Message::Bye(b) => Some(Finish::peer_bye(b.code)),
            Message::Error(e) if e.fatal => {
                Some(Finish { end: SessionEnd { kind: EndKind::Violation, code: e.code }, say: None, close: e.code })
            }
            // A non-fatal `Error` answers a request of ours (`NOT_PERMITTED`, `BUSY`): the host has nothing to do
            // about it, and the viewer's UI hears it once the session is live.
            Message::Error(_) if role == Role::Host || self.phase != Phase::Active => None,
            other => match role {
                Role::Host => self.host_message(other),
                Role::Viewer => self.viewer_message(other),
            },
        }
    }

    // ---------- host ----------

    fn host_message(&mut self, msg: Message) -> Option<Finish> {
        match (self.phase, msg) {
            (Phase::AwaitHello, Message::Hello(hello)) => {
                let Side::Host { service, hello: slot, .. } = &mut self.side else { return None };
                let cfg = &service.cfg;
                let ack = HelloAck {
                    proto_minor: PROTO_MINOR,
                    app_version: cfg.app_version.clone(),
                    name: cfg.name.clone(),
                    os: cfg.os,
                    os_version: cfg.os_version.clone(),
                };
                let caps = cfg.capabilities.clone();
                self.ctx.set_peer_minor(hello.proto_minor);
                *slot = Some(hello);
                if let Some(f) = self.emit(ack, false) {
                    return Some(f);
                }
                self.phase = Phase::AwaitCaps;
                self.emit(caps, false)
            }
            (Phase::AwaitCaps, Message::Capabilities(caps)) => {
                *lock(&self.ctx.inner.peer_caps) = Some(caps);
                let Side::Host { service, device, hello, deadline } = &mut self.side else { return None };
                let Some(hello) = hello.take() else { return Some(Finish::violation(ErrorCode::Internal)) };
                // The decision task has its own timers (the prompt, the wait for an old connection); this is the net
                // under it, for a task that crashed and will never answer.
                *deadline = now() + service.cfg.prompt_timeout + service.cfg.resume_wait + DECISION_MARGIN;
                let (service, device, ctx, ev) =
                    (Arc::clone(service), *device, self.ctx.clone(), self.ctx.inner.ev.clone());
                let task = tokio::spawn(async move {
                    let decision = service.decide(ctx, hello, device).await;
                    let _ = ev.send(Ev::Decision(decision)).await;
                });
                self.helpers.push(task.abort_handle());
                self.phase = Phase::Deciding;
                None
            }
            (Phase::Active, msg) => self.host_active(msg),
            // A second `Hello`, a `Capabilities` out of turn, anything else before the grant.
            _ => Some(Finish::violation(ErrorCode::ProtocolViolation)),
        }
    }

    fn on_decision(&mut self, decision: Decision) -> Option<Finish> {
        if self.phase != Phase::Deciding {
            return None;
        }
        let Side::Host { service, device, .. } = &self.side else { return None };
        let (service, device) = (Arc::clone(service), *device);
        // The commit stores the session id and the grants before the session is visible to the service layer.
        let committed = match service.commit(&self.ctx, &device, decision) {
            Ok(c) => c,
            Err(code) => {
                service.ui.event(HostEvent::Refused { device, code });
                return Some(Finish::refused_by_host(code));
            }
        };
        let info = service.ui.start_info();
        let paused = info.desktop != Desktop::Default && !committed.granted.contains(PermissionSet::UNATTENDED);
        self.ctx.inner.state.send_replace(DesktopState { desktop: info.desktop, paused });
        // What the viewer is about to be told; a change after the commit is reported right behind it.
        self.reported = committed.granted;
        self.applied_desktop = Some(info.desktop);
        // The grant is live before the viewer hears of it: its first stream may arrive right behind the message.
        self.ctx.inner.active.send_replace(true);
        self.phase = Phase::Active;
        self.liveness = Liveness::new(self.live_cfg, now());
        let grant = SessionGrant {
            session_id: committed.session_id,
            granted: committed.granted,
            mode: committed.mode,
            resume_token: committed.token,
        };
        if let Some(f) = self.emit(grant, false) {
            return Some(f);
        }
        if let Some(f) = self.emit(HostState { desktop: info.desktop, paused }, true) {
            return Some(f);
        }
        if committed.granted.contains(PermissionSet::VIEW)
            && let Some(f) = self.emit(DisplayList { displays: info.displays }, true)
        {
            return Some(f);
        }
        if !info.wake_macs.is_empty()
            && let Some(f) = self.emit(WakeInfo { macs: info.wake_macs }, true)
        {
            return Some(f);
        }
        service.ui.event(HostEvent::SessionStarted {
            device,
            session_id: committed.session_id,
            mode: committed.mode,
            granted: committed.granted,
        });
        for h in self.registry.handlers() {
            h.on_start(&self.ctx);
        }
        None
    }

    /// A request in a live session: direction was checked; now the permission, the handler and the caps.
    /// The messages of protocol minor 1 that a session takes after the grant, on either side. `None`: not one of them.
    fn minor_1_message(&mut self, msg: &Message) -> Option<Option<Finish>> {
        if !matches!(msg, Message::StoredGrants(_) | Message::Elevate(_)) {
            return None;
        }
        // A peer that announced minor 0 cannot have sent it (PROTOCOL.md 5.3).
        if self.ctx.peer_minor() < Message::type_min_minor(msg.type_id()) {
            return Some(Some(Finish::violation(ErrorCode::UnsupportedVersion)));
        }
        Some(match msg {
            // What the other side stored for this device: for display, kept by the grants sync when it is built.
            // Until then it is taken and dropped, so a later build that sends it is not cut off.
            Message::StoredGrants(_) => None,
            // The elevation step is not built: the answer is the one of a session that may not elevate.
            _ => self.reply(ErrorMessage {
                code: ErrorCode::NotPermitted,
                fatal: false,
                about_type: msg.type_id(),
                about_id: 0,
            }),
        })
    }

    fn host_active(&mut self, msg: Message) -> Option<Finish> {
        if let Some(done) = self.minor_1_message(&msg) {
            return done;
        }
        let type_id = msg.type_id();
        // The handshake does not repeat: a second `Hello` or `Capabilities` is a violation, not a request.
        if policy::is_core_type(type_id) {
            return Some(Finish::violation(ErrorCode::ProtocolViolation));
        }
        let about = policy::about_id(&msg);
        let refuse = |code| ErrorMessage { code, fatal: false, about_type: type_id, about_id: about };
        if !policy::permitted(self.ctx.granted(), self.ctx.requested(), &msg) {
            // Not granted: refused, nothing done (F-PERM-02). The session goes on.
            return self.reply(refuse(ErrorCode::NotPermitted));
        }
        // No feature installed for it: nothing is counted for a request nobody will run.
        let Some(handler) = self.registry.for_message(type_id).cloned() else {
            return self.reply(refuse(ErrorCode::Unavailable));
        };
        if lock(&self.ctx.inner.caps).on_incoming(&self.ctx.inner.caps_cfg, &msg) == CapVerdict::Busy {
            return self.reply(refuse(ErrorCode::Busy));
        }
        handler.on_message(&self.ctx, msg);
        None
    }

    fn on_grants_changed(&mut self, old: PermissionSet, new: PermissionSet) -> Option<Finish> {
        let desktop = self.ctx.desktop_state().desktop;
        let paused = desktop != Desktop::Default && !new.contains(PermissionSet::UNATTENDED);
        if let Some(f) = self.emit(PermissionsChanged { granted: new }, true) {
            return Some(f);
        }
        if paused != self.ctx.is_paused() {
            self.ctx.inner.state.send_replace(DesktopState { desktop, paused });
            if let Some(f) = self.emit(HostState { desktop, paused }, true) {
                return Some(f);
            }
        }
        if let Side::Host { service, device, .. } = &self.side {
            service.ui.event(HostEvent::GrantsChanged {
                device: *device,
                session_id: self.ctx.session_id(),
                from: old,
                to: new,
            });
        }
        for h in self.registry.handlers() {
            h.on_permissions(&self.ctx, old, new);
        }
        None
    }

    fn on_desktop(&mut self, desktop: Desktop) -> Option<Finish> {
        let paused = desktop != Desktop::Default && !self.ctx.granted().contains(PermissionSet::UNATTENDED);
        self.ctx.inner.state.send_replace(DesktopState { desktop, paused });
        if let Some(f) = self.emit(HostState { desktop, paused }, true) {
            return Some(f);
        }
        for h in self.registry.handlers() {
            h.on_state(&self.ctx, desktop, paused);
        }
        None
    }

    // ---------- viewer ----------

    fn viewer_message(&mut self, msg: Message) -> Option<Finish> {
        match (self.phase, msg) {
            (Phase::AwaitAck, Message::HelloAck(ack)) => {
                self.ctx.set_peer_minor(ack.proto_minor);
                if let Side::Viewer { peer, .. } = &mut self.side {
                    *peer = Some(ack);
                }
                self.phase = Phase::AwaitHostCaps;
                None
            }
            (Phase::AwaitHostCaps, Message::Capabilities(caps)) => {
                *lock(&self.ctx.inner.peer_caps) = Some(caps);
                self.phase = Phase::AwaitGrant;
                None
            }
            (Phase::AwaitGrant, Message::PromptPending(p)) => {
                if let Side::Viewer { events, on_prompt, .. } = &self.side {
                    let _ = events.try_send(ViewerEvent::PromptPending { timeout_s: p.timeout_s });
                    if let Some(hook) = on_prompt {
                        hook(p.timeout_s);
                    }
                }
                None
            }
            (Phase::AwaitGrant, Message::SessionRefused(r)) => {
                if let Side::Viewer { opened, .. } = &mut self.side
                    && let Some(tx) = opened.take()
                {
                    let _ = tx.send(Err(OpenError::Refused(r.code)));
                }
                Some(Finish::refused_for_viewer(r.code))
            }
            (Phase::AwaitGrant, Message::SessionGrant(g)) => self.viewer_granted(g),
            (Phase::Active, msg) => self.viewer_active(msg),
            _ => Some(Finish::violation(ErrorCode::ProtocolViolation)),
        }
    }

    fn viewer_granted(&mut self, g: SessionGrant) -> Option<Finish> {
        let Side::Viewer { opened, peer, .. } = &mut self.side else { return None };
        let Some(peer) = peer.clone() else { return Some(Finish::violation(ErrorCode::Internal)) };
        self.ctx.inner.session_id.store(g.session_id, std::sync::atomic::Ordering::SeqCst);
        self.ctx.set_granted(g.granted);
        self.ctx.inner.active.send_replace(true);
        self.phase = Phase::Active;
        self.liveness = Liveness::new(self.live_cfg, now());
        let info = GrantInfo {
            session_id: g.session_id,
            granted: g.granted,
            mode: g.mode,
            resume: swapdesk_proto::Resume { session_id: g.session_id, token: g.resume_token },
            host: peer,
        };
        if let Some(tx) = opened.take() {
            let _ = tx.send(Ok(info));
        }
        for h in self.registry.handlers() {
            h.on_start(&self.ctx);
        }
        None
    }

    fn viewer_active(&mut self, msg: Message) -> Option<Finish> {
        if let Some(done) = self.minor_1_message(&msg) {
            return done;
        }
        let Side::Viewer { events, .. } = &self.side else { return None };
        let events = events.clone();
        match msg {
            Message::PermissionsChanged(p) => {
                let old = self.ctx.granted();
                self.ctx.set_granted(p.granted);
                let _ = events.try_send(ViewerEvent::PermissionsChanged(p.granted));
                for h in self.registry.handlers() {
                    h.on_permissions(&self.ctx, old, p.granted);
                }
                None
            }
            Message::HostState(h) => {
                self.ctx.inner.state.send_replace(DesktopState { desktop: h.desktop, paused: h.paused });
                let _ = events.try_send(ViewerEvent::HostState { desktop: h.desktop, paused: h.paused });
                for handler in self.registry.handlers() {
                    handler.on_state(&self.ctx, h.desktop, h.paused);
                }
                None
            }
            Message::HelloAck(_)
            | Message::Capabilities(_)
            | Message::PromptPending(_)
            | Message::SessionGrant(_)
            | Message::SessionRefused(_) => Some(Finish::violation(ErrorCode::ProtocolViolation)),
            other => {
                let handler = self.registry.for_message(other.type_id()).cloned();
                if lock(&self.ctx.inner.caps).on_incoming(&self.ctx.inner.caps_cfg, &other) == CapVerdict::Busy {
                    // A peer past a cap: dropped; the host side answers `BUSY`, the viewer just ignores the excess.
                    return None;
                }
                match handler {
                    Some(h) => h.on_message(&self.ctx, other),
                    None => {
                        let _ = events.try_send(ViewerEvent::Message(other));
                    }
                }
                None
            }
        }
    }

    // ---------- the end ----------

    async fn finish(mut self, f: Finish) -> SessionEnd {
        let end = f.end;
        // From here nothing is started for this session, and what runs for it stops.
        self.ctx.begin_close();
        for h in &self.helpers {
            h.abort();
        }
        self.ctx.abort_all();
        // The host is free for another session as soon as this one is over, not after its last word has been handed
        // over: a viewer that sees the end may reconnect at once.
        if let Side::Host { service, device, .. } = &self.side {
            let id = self.ctx.session_id();
            if id != 0 {
                let resumable = service.session_ended(&self.ctx, &end);
                service.ui.event(HostEvent::Ended { device: *device, session_id: id, end, resumable });
            }
        }
        // Hand the peer the last word (`Bye`, `Error`, `SessionRefused`) before the connection goes.
        let said = f.say.is_some();
        if let Some(msg) = f.say {
            let _ = tokio::time::timeout(SAY_LIMIT, self.ctx.inner.out.send(msg)).await;
            self.ctx.inner.out.close(CLOSE_LIMIT).await;
        }
        self.ctx.inner.ended.send_replace(Some(end));
        if let Side::Viewer { events, .. } = &self.side {
            let _ = events.try_send(ViewerEvent::Ended(end));
        }
        for h in self.registry.handlers() {
            h.on_end(&self.ctx, &end);
        }
        if let Side::Viewer { opened, .. } = &mut self.side
            && let Some(tx) = opened.take()
        {
            let err = match end.kind {
                EndKind::Violation => OpenError::Protocol(end.code),
                EndKind::Timeout => OpenError::Timeout,
                EndKind::PeerBye | EndKind::PeerClosed => OpenError::Closed(end.code),
                EndKind::Refused => OpenError::Refused(end.code),
                EndKind::Lost | EndKind::LocalBye => OpenError::Lost,
            };
            let _ = tx.send(Err(err));
        }
        if let Some(conn) = &self.ctx.inner.conn {
            // After a last word, let the peer read it and close first: a CONNECTION_CLOSE sent now can overtake
            // the message on the peer, which then sees a bare close with no reason (found under load: a
            // `SessionRefused { NobodyPresent }` read as `Closed(NoError)`). The peer closes when it reads it.
            if said {
                let _ = tokio::time::timeout(LINGER_LIMIT, conn.closed()).await;
            }
            conn.close(app_code(f.close), b"");
        }
        end
    }
}
