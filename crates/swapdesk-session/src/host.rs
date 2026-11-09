//! The host side of a session (PROTOCOL.md 5.2, 6): the service that owns the live sessions and the resume tokens,
//! the decision (unattended, swap, resume, or a prompt with a 20 s auto-decline), and the callbacks into the host's
//! UI. The per-connection state machine is in `actor`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quinn::{Connection, VarInt};
use swapdesk_platform::Clock;
use swapdesk_proto::{
    Capabilities, Desktop, Display, ErrorCode, Feature, GrantMode, Hello, Intent, Os, PermissionSet, PromptPending,
};
use swapdesk_trust::{DeviceId, TrustHandle};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::actor::{Actor, Side};
use crate::caps::Caps;
use crate::ctx::{ConsentSource, CtxParts, EndKind, SessionCtx, SessionEnd};
use crate::link::ControlIo;
use crate::liveness::LivenessConfig;
use crate::policy::{self, Role};
use crate::registry::Registry;
use crate::resume::{ResumeError, ResumeTable, SwapTokens};
use crate::util::{lock, random_session_id};

/// The time source of the session's timers: tokio's clock, so a test can pause it.
pub(crate) fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    pub os: Os,
    pub os_version: String,
    pub app_version: String,
    pub capabilities: Capabilities,
    /// How long the host's user has to answer a request before it is declined (F-SESS-01: 20 s).
    pub prompt_timeout: Duration,
    /// The same for the D13 consent of another logged-in user.
    pub consent_timeout: Duration,
    /// From the control stream opening to the viewer's `Capabilities`.
    pub handshake_timeout: Duration,
    /// How long after a lost connection a resume token works (PROTOCOL.md 6.4: 60 s).
    pub resume_window: Duration,
    /// How long a resume request waits for the old connection to be seen lost.
    pub resume_wait: Duration,
    /// How long an armed swap token lives (6.3 step 4: 10 s).
    pub swap_token_ttl: Duration,
    /// Sessions that may be live at once; another request is refused `BUSY`.
    pub max_live_sessions: usize,
    pub liveness: LivenessConfig,
    pub caps: Caps,
}

impl HostConfig {
    pub fn new(name: &str, os: Os, os_version: &str, app_version: &str, capabilities: Capabilities) -> Self {
        Self {
            name: name.into(),
            os,
            os_version: os_version.into(),
            app_version: app_version.into(),
            capabilities,
            prompt_timeout: Duration::from_secs(20),
            consent_timeout: Duration::from_secs(20),
            handshake_timeout: Duration::from_secs(10),
            resume_window: Duration::from_secs(60),
            resume_wait: Duration::from_secs(8),
            swap_token_ttl: Duration::from_secs(10),
            max_live_sessions: 1,
            liveness: LivenessConfig::default(),
            caps: Caps::default(),
        }
    }
}

/// An incoming request the host's user is asked about (F-SESS-01). `stored` is what the device may do here: the
/// preset of the per-session choices.
#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub request_id: u64,
    pub device: DeviceId,
    pub name: String,
    pub os: Os,
    pub intent: Intent,
    pub requested: PermissionSet,
    pub stored: PermissionSet,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswer {
    /// Accept with these grants for this session (D8). "Só deixar ver" is `Accept(VIEW)`. `UNATTENDED` and
    /// `ADMIN_TERMINAL` are dropped unless stored. An empty set is a refusal.
    Accept(PermissionSet),
    Decline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptEnd {
    Answered,
    /// The 20 s ran out: the host declined for its user.
    TimedOut,
    /// The viewer went away (a `Bye`, a lost connection) before the answer.
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct ConsentRequest {
    pub device: DeviceId,
    pub feature: Feature,
    pub timeout: Duration,
}

/// What the host tells a new session before video starts (5.2 step 4).
#[derive(Debug, Clone)]
pub struct StartInfo {
    pub desktop: Desktop,
    pub displays: Vec<Display>,
    pub wake_macs: Vec<[u8; 6]>,
}

impl Default for StartInfo {
    fn default() -> Self {
        Self { desktop: Desktop::Default, displays: Vec::new(), wake_macs: Vec::new() }
    }
}

/// Things the service layer logs (the audit entries of F-SESS-04) or shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// `mode` Unattended is "Entrou sem perguntar".
    SessionStarted {
        device: DeviceId,
        session_id: u64,
        mode: GrantMode,
        granted: PermissionSet,
    },
    Refused {
        device: DeviceId,
        code: ErrorCode,
    },
    Ended {
        device: DeviceId,
        session_id: u64,
        end: SessionEnd,
        resumable: bool,
    },
    GrantsChanged {
        device: DeviceId,
        session_id: u64,
        from: PermissionSet,
        to: PermissionSet,
    },
    PeerOffline {
        device: DeviceId,
        session_id: u64,
    },
    PeerOnline {
        device: DeviceId,
        session_id: u64,
    },
    ConsentAsked {
        device: DeviceId,
        feature: Feature,
        answer: ErrorCode,
    },
}

/// The host's user and desktop, as the session layer needs them. The service implements it over the session
/// watcher and the UI connection; tests script it.
pub trait HostUi: Send + Sync + 'static {
    /// "Someone at the host" (PROTOCOL.md 6.2): a UI is connected from the console session and it is unlocked.
    fn somebody_present(&self) -> bool;

    /// Shows the request. The answer comes through the channel; dropping the sender is a refusal.
    fn prompt(&self, request: PromptRequest) -> oneshot::Receiver<PromptAnswer>;

    /// The request is no longer waiting: close its card.
    fn prompt_closed(&self, _request_id: u64, _how: PromptEnd) {}

    /// D13: asks the logged-in user whether the viewer may use the terminal or files.
    fn consent(&self, request: ConsentRequest) -> oneshot::Receiver<bool>;

    /// The consent question is no longer waiting: close its card (answered, timed out, or the session or the
    /// permission went away first).
    fn consent_closed(&self, _request: &ConsentRequest, _how: PromptEnd) {}

    /// "Bloquear ao terminar" (F-SESS-03).
    fn lock_on_end(&self) -> bool {
        false
    }

    fn lock_screen(&self) {}

    fn start_info(&self) -> StartInfo {
        StartInfo::default()
    }

    fn event(&self, _event: HostEvent) {}
}

/// One live or resumable session, for the banner and Atividade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_id: u64,
    pub device: DeviceId,
    pub granted: PermissionSet,
}

/// How the grants of a new session are worked out: at the commit, from what is stored then.
pub(crate) enum Basis {
    /// Entry without a prompt: the stored grants intersected with what was requested.
    Unattended,
    /// A swap: the stored grants for the device, never `UNATTENDED`, limited to what was requested.
    Swap,
    /// A prompt: what the host's user picked (D8).
    Prompted(PermissionSet),
}

pub(crate) enum Decision {
    Refuse(ErrorCode),
    Grant { basis: Basis, requested: PermissionSet, slot: Option<PromptSlot> },
    Resume { session_id: u64, token: [u8; 16], requested: PermissionSet },
}

impl Decision {
    fn take_slot(&mut self) -> Option<PromptSlot> {
        match self {
            Decision::Grant { slot, .. } => slot.take(),
            _ => None,
        }
    }
}

/// One pending decision of this host: held from the moment a request is about to prompt until its session is committed
/// or refused, so requests do not pile up behind each other (one per device, and never more than the host can
/// start).
pub(crate) struct PromptSlot {
    inner: Arc<HostInner>,
    device: DeviceId,
}

impl Drop for PromptSlot {
    fn drop(&mut self) {
        lock(&self.inner.deciding).remove(&self.device);
    }
}

/// What a session was granted at commit.
pub(crate) struct Committed {
    pub session_id: u64,
    pub granted: PermissionSet,
    pub mode: GrantMode,
    pub token: [u8; 16],
}

struct Table {
    live: HashMap<u64, SessionCtx>,
    resume: ResumeTable,
    swap: SwapTokens,
}

pub(crate) struct HostInner {
    pub cfg: HostConfig,
    pub trust: Arc<TrustHandle>,
    pub ui: Arc<dyn HostUi>,
    pub registry: Arc<Registry>,
    pub clock: Arc<dyn Clock>,
    table: Mutex<Table>,
    /// The devices whose request is waiting for the host's user (see [`PromptSlot`]).
    deciding: Mutex<HashSet<DeviceId>>,
    /// Signalled whenever a live session ends or is marked lost, for a resume that waits for it.
    changed: Notify,
    next_request: AtomicU64,
}

/// Owns the sessions of this machine as a host. Cheap to clone.
#[derive(Clone)]
pub struct HostService {
    pub(crate) inner: Arc<HostInner>,
}

fn app_code(code: ErrorCode) -> VarInt {
    VarInt::from_u32(u32::from(code.tag()))
}

/// Makes sure a session that stops without going through its normal end (a panic) is removed from the table.
struct EndGuard {
    inner: Arc<HostInner>,
    ctx: SessionCtx,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        if self.ctx.ended().is_none() {
            let end = SessionEnd { kind: EndKind::Lost, code: ErrorCode::Internal };
            self.ctx.inner.ended.send_replace(Some(end));
            self.inner.session_ended(&self.ctx, &end);
            self.ctx.abort_all();
        }
    }
}

struct PromptGuard {
    ui: Arc<dyn HostUi>,
    request_id: u64,
    how: PromptEnd,
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.ui.prompt_closed(self.request_id, self.how);
    }
}

impl HostService {
    pub fn new(
        cfg: HostConfig,
        trust: Arc<TrustHandle>,
        ui: Arc<dyn HostUi>,
        registry: Registry,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let table = Table {
            live: HashMap::new(),
            resume: ResumeTable::new(cfg.resume_window),
            swap: SwapTokens::new(cfg.swap_token_ttl),
        };
        Self {
            inner: Arc::new(HostInner {
                cfg,
                trust,
                ui,
                registry: Arc::new(registry),
                clock,
                table: Mutex::new(table),
                deciding: Mutex::new(HashSet::new()),
                changed: Notify::new(),
                next_request: AtomicU64::new(1),
            }),
        }
    }

    /// Runs one session on an accepted, gated connection until it ends.
    pub async fn serve(&self, conn: Connection, device: DeviceId) -> SessionEnd {
        match tokio::time::timeout(self.inner.cfg.handshake_timeout, conn.accept_bi()).await {
            Ok(Ok((send, recv))) => self.serve_io(ControlIo::from_quinn(send, recv), Some(conn), device).await,
            _ => {
                conn.close(app_code(ErrorCode::Timeout), b"");
                SessionEnd { kind: EndKind::Timeout, code: ErrorCode::Timeout }
            }
        }
    }

    /// The same over any control link (the in-memory pair of the tests).
    pub async fn serve_io(&self, io: ControlIo, conn: Option<Connection>, device: DeviceId) -> SessionEnd {
        let (ev_tx, ev_rx) = mpsc::channel(256);
        let inner = &self.inner;
        let ctx = SessionCtx::new(CtxParts {
            role: Role::Host,
            device: Some(device),
            out: io.out.clone(),
            conn: conn.clone(),
            caps_cfg: inner.cfg.caps,
            ev: ev_tx.clone(),
            clock: Arc::clone(&inner.clock),
            consent: Some(ConsentSource { ui: Arc::clone(&inner.ui), timeout: inner.cfg.consent_timeout, device }),
        });
        let side = Side::Host {
            service: Arc::clone(&self.inner),
            device,
            hello: None,
            deadline: now() + inner.cfg.handshake_timeout,
        };
        // A panic in a handler ends only this session (D15): the guard takes it out of the live table as lost, so
        // the host is not left "busy" with a session nobody runs.
        let _guard = EndGuard { inner: Arc::clone(&self.inner), ctx: ctx.clone() };
        Actor::new(side, ctx, io, conn, ev_tx, ev_rx, Arc::clone(&inner.registry), inner.cfg.liveness, now())
            .run()
            .await
    }

    // ---------- what the service layer does to sessions ----------

    pub fn session(&self, session_id: u64) -> Option<SessionCtx> {
        lock(&self.inner.table).live.get(&session_id).cloned()
    }

    pub fn sessions(&self) -> Vec<SessionInfo> {
        let t = lock(&self.inner.table);
        t.live
            .values()
            .map(|c| SessionInfo {
                session_id: c.session_id(),
                device: c.device().unwrap_or([0; 16]),
                granted: c.granted(),
            })
            .collect()
    }

    /// The host's user changed what this session may do (the banner): the new grants apply at once, the viewer is
    /// told (`PermissionsChanged`), the handlers hear what went, and the session's resume token dies (F-PERM-02).
    /// Returns false when there is no such live session.
    pub fn set_session_grants(&self, session_id: u64, new: PermissionSet) -> bool {
        let Some(ctx) = self.session(session_id) else { return false };
        lock(&self.inner.table).resume.invalidate(session_id);
        // `UNATTENDED` and `ADMIN_TERMINAL` need an administrator in Permissões: the banner never adds them.
        ctx.modify_granted(|current| policy::session_change(current, new));
        true
    }

    /// The stored grants of `device` changed (Permissões). Every live session of the device keeps at most what is
    /// still stored (a change never widens a running session), and the device's resume tokens die.
    pub fn stored_grants_changed(&self, device: &DeviceId) {
        let stored = self.stored(device).map(|d| d.grants_here).unwrap_or(PermissionSet::EMPTY);
        let live: Vec<SessionCtx> = {
            let mut t = lock(&self.inner.table);
            t.resume.invalidate_device(device);
            t.live.values().filter(|c| c.device().as_ref() == Some(device)).cloned().collect()
        };
        for ctx in live {
            // Only what was revoked goes; what the host's user added for this session alone stays (D8).
            let seen = std::mem::replace(&mut *lock(&ctx.inner.stored_seen), stored);
            ctx.modify_granted(|granted| policy::without_revoked(granted, seen, stored));
        }
    }

    /// "Desconectar" (F-SESS-03): `Bye { NO_ERROR }` and the token dies with it.
    pub fn disconnect(&self, session_id: u64) -> bool {
        match self.session(session_id) {
            Some(ctx) => {
                // The token dies now, whatever the session is doing: it cannot outlive "Desconectar" by a lost message.
                lock(&self.inner.table).resume.invalidate(session_id);
                ctx.bye(ErrorCode::NoError);
                true
            }
            None => false,
        }
    }

    /// Forget (F-PAIR-04): the device's sessions end with `Bye { FORGOTTEN }` and every token of it dies.
    pub fn forget_device(&self, device: &DeviceId) {
        let live: Vec<SessionCtx> = {
            let mut t = lock(&self.inner.table);
            t.resume.invalidate_device(device);
            t.swap.disarm_device(device);
            t.live.values().filter(|c| c.device().as_ref() == Some(device)).cloned().collect()
        };
        for ctx in live {
            ctx.bye(ErrorCode::Forgotten);
        }
    }

    /// A desktop switch on the host: `HostState` goes to every live session, paused where the session lacks
    /// `UNATTENDED`.
    pub fn set_desktop(&self, desktop: Desktop) {
        let live: Vec<SessionCtx> = lock(&self.inner.table).live.values().cloned().collect();
        for ctx in live {
            ctx.set_desktop(desktop);
        }
    }

    /// The host is stopping: every live session ends with `Bye { HOST_SHUTDOWN }` and every token dies (a reboot
    /// loses every token anyway).
    pub fn shutdown(&self) {
        let live: Vec<SessionCtx> = {
            let mut t = lock(&self.inner.table);
            t.resume.clear();
            t.live.values().cloned().collect()
        };
        for ctx in live {
            ctx.bye(ErrorCode::HostShutdown);
        }
    }

    /// 6.3 step 4: the future host arms a swap token for the future viewer's device (10 s, single use).
    pub fn arm_swap_token(&self, device: DeviceId, token: [u8; 16]) {
        lock(&self.inner.table).swap.arm(now(), device, token);
    }

    /// Resume entries that exist (live sessions and sessions waiting to be resumed), for tests and Atividade.
    pub fn resumable_sessions(&self) -> usize {
        let mut t = lock(&self.inner.table);
        t.resume.purge(now());
        t.resume.len()
    }

    fn stored(&self, device: &DeviceId) -> Option<swapdesk_trust::PairedDevice> {
        self.inner.trust.read(|s| s.find_by_id(device).cloned())
    }
}

impl HostInner {
    pub(crate) fn stored(&self, device: &DeviceId) -> Option<swapdesk_trust::PairedDevice> {
        self.trust.read(|s| s.find_by_id(device).cloned())
    }

    fn live_count(&self) -> usize {
        lock(&self.table).live.len()
    }

    /// The decision for a `Hello` (5.2 step 2). Runs as a task of the session; the state machine commits the result.
    pub(crate) async fn decide(self: Arc<Self>, ctx: SessionCtx, hello: Hello, device: DeviceId) -> Decision {
        let Some(stored) = self.stored(&device) else { return Decision::Refuse(ErrorCode::UnknownDevice) };
        let stored_grants = stored.grants_here;

        // A resume: the token is checked, and a token whose old connection is not yet known lost waits for it.
        // (The host being busy with another session is decided at the commit, after the old connection is gone.)
        if let Some(r) = &hello.resume {
            let deadline = tokio::time::Instant::now() + self.cfg.resume_wait;
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let verdict = {
                    let mut t = lock(&self.table);
                    t.resume.purge(now());
                    t.resume.check(now(), &device, r.session_id, &r.token)
                };
                match verdict {
                    Ok(()) => {
                        return Decision::Resume {
                            session_id: r.session_id,
                            token: r.token,
                            requested: hello.requested,
                        };
                    }
                    Err(ResumeError::NotLost) => {
                        if tokio::time::timeout_at(deadline, notified).await.is_err() {
                            break;
                        }
                    }
                    // An unknown, expired, used or foreign token is not an error: the session goes the normal way
                    // (an unattended device reconnects through UNATTENDED, any other gets a fresh prompt).
                    Err(_) => break,
                }
            }
        }

        // Another session is live: refused before anything is consumed or asked.
        if self.live_count() >= self.cfg.max_live_sessions {
            return Decision::Refuse(ErrorCode::Busy);
        }

        // A swap: the future host armed the token for this device.
        if let Some(token) = hello.swap_token {
            let ok = lock(&self.table).swap.take(now(), &device, &token);
            if ok {
                return Decision::Grant { basis: Basis::Swap, requested: hello.requested, slot: None };
            }
        }

        if policy::unattended_grant(stored_grants, hello.requested).is_some() {
            return Decision::Grant { basis: Basis::Unattended, requested: hello.requested, slot: None };
        }

        // A prompt: nobody at the host refuses at once; otherwise the user has the prompt timeout.
        if !self.ui.somebody_present() {
            return Decision::Refuse(ErrorCode::NobodyPresent);
        }
        // One question at a time per device, and no more than the host could start: the rest is `BUSY`, so a device
        // cannot pile up cards in front of the host's user.
        let slot = {
            let mut deciding = lock(&self.deciding);
            if deciding.contains(&device) || self.live_count() + deciding.len() >= self.cfg.max_live_sessions {
                return Decision::Refuse(ErrorCode::Busy);
            }
            deciding.insert(device);
            PromptSlot { inner: Arc::clone(&self), device }
        };
        let secs = u16::try_from(self.prompt_timeout_secs()).unwrap_or(u16::MAX);
        let _ = ctx.inner.out.send(PromptPending { timeout_s: secs }).await;
        let request_id = self.next_request.fetch_add(1, Ordering::SeqCst);
        let mut guard = PromptGuard { ui: Arc::clone(&self.ui), request_id, how: PromptEnd::Cancelled };
        let answer = self.ui.prompt(PromptRequest {
            request_id,
            device,
            name: hello.name.clone(),
            os: hello.os,
            intent: hello.intent,
            requested: hello.requested,
            stored: stored_grants,
            timeout: self.cfg.prompt_timeout,
        });
        match tokio::time::timeout(self.cfg.prompt_timeout, answer).await {
            Ok(Ok(PromptAnswer::Accept(picked))) => {
                guard.how = PromptEnd::Answered;
                // What is stored may change while the card is open: the commit works the grants out from what is
                // stored then. An empty pick is a refusal.
                if picked.is_empty() {
                    Decision::Refuse(ErrorCode::Declined)
                } else {
                    Decision::Grant { basis: Basis::Prompted(picked), requested: hello.requested, slot: Some(slot) }
                }
            }
            Ok(Ok(PromptAnswer::Decline)) | Ok(Err(_)) => {
                guard.how = PromptEnd::Answered;
                Decision::Refuse(ErrorCode::Declined)
            }
            Err(_) => {
                guard.how = PromptEnd::TimedOut;
                Decision::Refuse(ErrorCode::Timeout)
            }
        }
    }

    fn prompt_timeout_secs(&self) -> u64 {
        self.cfg.prompt_timeout.as_secs()
    }

    /// Records the session as live: single use of a resume token, a new token for the session. Fails with `BUSY`
    /// when another session took the host meanwhile, or the resume was used by a competing connection. The grants
    /// are worked out here, from what is stored now and under the table lock, so a change of the stored grants
    /// either lands before (and bounds this session) or after (and finds it in the live table).
    pub(crate) fn commit(
        &self,
        ctx: &SessionCtx,
        device: &DeviceId,
        mut decision: Decision,
    ) -> Result<Committed, ErrorCode> {
        // The pending-decision slot is released after the table lock.
        let slot = decision.take_slot();
        let result = self.commit_locked(ctx, device, decision);
        drop(slot);
        result
    }

    fn commit_locked(&self, ctx: &SessionCtx, device: &DeviceId, decision: Decision) -> Result<Committed, ErrorCode> {
        let mut t = lock(&self.table);
        if t.live.len() >= self.cfg.max_live_sessions {
            return Err(ErrorCode::Busy);
        }
        let Some(stored_now) = self.trust.read(|s| s.find_by_id(device).map(|d| d.grants_here)) else {
            return Err(ErrorCode::UnknownDevice);
        };
        let committed = match decision {
            Decision::Refuse(code) => return Err(code),
            Decision::Grant { basis, requested, .. } => {
                let (granted, mode) = match basis {
                    // Entry without a prompt needs the stored `UNATTENDED` still: a revocation that raced the
                    // decision refuses it, and the viewer's next request is asked about.
                    Basis::Unattended => match policy::unattended_grant(stored_now, requested) {
                        Some(g) => (g, GrantMode::Unattended),
                        None => return Err(ErrorCode::NotPermitted),
                    },
                    Basis::Swap => (policy::swap_grant(stored_now, requested), GrantMode::Swap),
                    Basis::Prompted(picked) => {
                        let g = policy::prompted_grant(stored_now, picked, requested);
                        if g.is_empty() {
                            return Err(ErrorCode::Declined);
                        }
                        (g, GrantMode::Prompted)
                    }
                };
                ctx.set_requested(requested);
                let session_id = random_session_id();
                let token = t.resume.insert(session_id, *device, granted);
                Committed { session_id, granted, mode, token }
            }
            Decision::Resume { session_id, token, requested } => {
                match t.resume.resume(now(), device, session_id, &token, stored_now) {
                    Ok((granted, token)) => {
                        ctx.set_requested(requested);
                        Committed { session_id, granted, mode: GrantMode::Resumed, token }
                    }
                    Err(_) => return Err(ErrorCode::Busy),
                }
            }
        };
        // The session id and the grants are the session's before it is visible: whoever finds it in the live table
        // (a stored-grant change, the end guard) sees them.
        ctx.inner.session_id.store(committed.session_id, Ordering::SeqCst);
        ctx.set_granted(committed.granted);
        *lock(&ctx.inner.stored_seen) = stored_now;
        t.live.insert(committed.session_id, ctx.clone());
        Ok(committed)
    }

    /// A session of this host ended (called by its state machine). Decides whether it can be resumed: only a lost
    /// connection, with "Bloquear ao terminar" off; anything else kills the token.
    pub(crate) fn session_ended(&self, ctx: &SessionCtx, end: &SessionEnd) -> bool {
        let id = ctx.session_id();
        if id == 0 {
            return false;
        }
        let lock_on_end = self.ui.lock_on_end();
        let mut resumable = end.kind == EndKind::Lost && !lock_on_end;
        {
            let mut t = lock(&self.table);
            t.live.remove(&id);
            if resumable {
                t.resume.mark_lost(id, now());
            } else {
                t.resume.invalidate(id);
            }
            t.resume.purge(now());
            // A grant change killed the token while the session was live: nothing to resume.
            resumable = resumable && t.resume.contains(id);
        }
        self.changed.notify_waiters();
        if lock_on_end {
            self.ui.lock_screen();
        }
        resumable
    }
}

#[cfg(test)]
mod tests {
    //! Stage 37, findings 4 and 13: they need `HostInner::commit` and the end guard themselves.

    use super::*;
    use crate::link::memory_pair;
    use swapdesk_fakes::{FakeClock, MemorySecretStore};
    use swapdesk_platform::MonoMicros;
    use swapdesk_proto::{KbKind, KeyboardInfo};
    use swapdesk_trust::{Identity, PairedDevice, device_id};

    struct NoUi;

    impl HostUi for NoUi {
        fn somebody_present(&self) -> bool {
            true
        }

        fn prompt(&self, _request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
            oneshot::channel().1
        }

        fn consent(&self, _request: ConsentRequest) -> oneshot::Receiver<bool> {
            oneshot::channel().1
        }
    }

    fn caps() -> Capabilities {
        Capabilities {
            video_decode: vec![],
            audio: vec![],
            max_datagram: 1200,
            keyboard: KeyboardInfo { kind: KbKind::Ansi, layout: "us".into() },
            features: 0,
        }
    }

    fn hello(requested: PermissionSet) -> Hello {
        Hello {
            proto_minor: 0,
            app_version: "t".into(),
            name: "v".into(),
            os: Os::MacOs,
            os_version: "15".into(),
            intent: Intent::Control,
            requested,
            resume: None,
            swap_token: None,
        }
    }

    struct Fx {
        service: HostService,
        trust: Arc<TrustHandle>,
        device: DeviceId,
    }

    fn fixture(stored: PermissionSet) -> Fx {
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let identity = Identity::generate(0).unwrap();
        let device = device_id(identity.spki());
        trust
            .update(|s| {
                let mut d =
                    PairedDevice::new_paired(*identity.spki(), "viewer".into(), Os::MacOs, "15".into(), 1, None);
                d.grants_here = stored;
                s.devices.push(d);
            })
            .unwrap();
        let cfg = HostConfig::new("host", Os::Windows, "11", "0.1.0", caps());
        let service = HostService::new(
            cfg,
            trust.clone(),
            Arc::new(NoUi),
            Registry::new(),
            Arc::new(FakeClock::new(MonoMicros(1))),
        );
        Fx { service, trust, device }
    }

    fn ctx_for(fx: &Fx) -> SessionCtx {
        let (io, _peer) = memory_pair();
        let (ev_tx, _ev_rx) = mpsc::channel(8);
        SessionCtx::new(CtxParts {
            role: Role::Host,
            device: Some(fx.device),
            out: io.out,
            conn: None,
            caps_cfg: Caps::default(),
            ev: ev_tx,
            clock: Arc::new(FakeClock::new(MonoMicros(1))),
            consent: None,
        })
    }

    fn set_stored(fx: &Fx, grants: PermissionSet) {
        let id = fx.device;
        fx.trust
            .update(|s| s.devices.iter_mut().filter(|d| d.device_id == id).for_each(|d| d.grants_here = grants))
            .unwrap();
    }

    fn prompted(picked: PermissionSet) -> Decision {
        Decision::Grant { basis: Basis::Prompted(picked), requested: picked, slot: None }
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// Finding 4: a revocation that lands between the decision and its commit used to be lost, because the session
    /// was not in the live table yet for `stored_grants_changed` to narrow.
    #[test]
    fn f4_a_revocation_between_the_decision_and_the_commit_is_not_lost() {
        block_on(async {
            let fx = fixture(PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES);
            let ctx = ctx_for(&fx);
            let decision = Arc::clone(&fx.service.inner)
                .decide(
                    ctx.clone(),
                    hello(PermissionSet::UNATTENDED | PermissionSet::CONTROL | PermissionSet::FILES),
                    fx.device,
                )
                .await;
            assert!(matches!(decision, Decision::Grant { basis: Basis::Unattended, .. }));
            // The owner takes UNATTENDED and FILES away while the decision is on its way to the state machine.
            set_stored(&fx, PermissionSet::CONTROL);
            fx.service.stored_grants_changed(&fx.device);
            match fx.service.inner.commit(&ctx, &fx.device, decision) {
                Ok(c) => assert_eq!(c.granted, PermissionSet::CONTROL, "the session started with what was revoked"),
                Err(code) => assert_ne!(code, ErrorCode::Busy),
            }
        });
    }

    /// Finding 13: the session id and the grants belong to the commit, so a panic right after it still lets the guard
    /// free the host.
    #[test]
    fn f13_a_panic_right_after_the_commit_does_not_leave_the_host_busy() {
        block_on(async {
            let fx = fixture(PermissionSet::CONTROL);
            let ctx = ctx_for(&fx);
            let decision = prompted(PermissionSet::CONTROL);
            let committed = fx.service.inner.commit(&ctx, &fx.device, decision).unwrap();
            assert_eq!(ctx.session_id(), committed.session_id, "the commit stores the session id");
            assert_eq!(fx.service.sessions().len(), 1);
            // The state machine panics before it stored anything: the guard of `serve_io` runs.
            drop(EndGuard { inner: Arc::clone(&fx.service.inner), ctx: ctx.clone() });
            assert!(fx.service.sessions().is_empty(), "the host is still holding a session nobody runs");
            let again = fx.service.inner.commit(&ctx_for(&fx), &fx.device, prompted(PermissionSet::CONTROL));
            assert!(again.is_ok(), "the next session is refused BUSY forever");
        });
    }
}
