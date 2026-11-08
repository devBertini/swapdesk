//! Swap (PROTOCOL.md 6.3, decision D1): "Request swap" inverts the roles of two machines through a new connection
//! with the pinned keys. This crate is the state machine of the five `Swap*` messages as a [`ChannelHandler`]; it
//! owns no socket and no UI. What it needs from the machine it runs on is the [`SwapEnv`] trait.
//!
//! Names follow the protocol: A asks, B is asked. The *future host* is the side that is the viewer now (its handler
//! sees `Role::Viewer`), the *future viewer* is the side that is the host now.
//!
//! ```text
//!   A: request()  --SwapRequest-->  B: checks (grant, presence), asks its user (20 s)
//!   A <--SwapAccept{token}--  B     (B future host: B armed the token first)
//!   future host arms the token for the future viewer's device, sends SwapArmed
//!   future viewer, on SwapArmed: Bye{SWAPPED}, then SwapEnv::begin_viewing (Hello{swap_token})
//!   future host: ends with the viewer's Bye (a Bye of its own after 2 s if none came)
//! ```
//!
//! One handler serves a machine: register the same one in the `HostService` registry and in each
//! `ViewerConfig.registry`; per-session state is keyed by the session.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use swapdesk_proto::{ErrorCode, Message, SwapAccept, SwapArmed, SwapCancel, SwapDecline, SwapRequest};
use swapdesk_session::{ChannelHandler, Role, SessionCtx, SessionEnd, random_token};
use swapdesk_trust::DeviceId;
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

/// The message type IDs of the five swap messages (PROTOCOL.md 5.3, 0x0201 to 0x0205).
pub const SWAP_TYPES: &[u16] = &[0x0201, 0x0202, 0x0203, 0x0204, 0x0205];

/// The question shown to the asked user: "pediu para inverter" with a countdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapPrompt {
    pub request_id: u32,
    /// The device that asked.
    pub device: DeviceId,
    /// Whether the asked machine would become the host (it shows what it would give up control of).
    pub asked_becomes_host: bool,
    pub timeout: Duration,
}

/// How a question to the user ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskEnd {
    Answered,
    /// The 20 s ran out: the handler declined for the user.
    TimedOut,
    /// The asking side cancelled, gave up, or the session ended first.
    Cancelled,
}

/// What a machine gives the swap handler. Every method runs on a session task: none may block.
pub trait SwapEnv: Send + Sync + 'static {
    /// "Someone at this machine" (PROTOCOL.md 6.2).
    fn somebody_present(&self) -> bool;

    /// Whether this machine, as host, has stored `CONTROL` for `peer` in its own trust store (6.3 step 1).
    fn control_granted_to(&self, peer: &DeviceId) -> bool;

    /// Shows the request; the answer comes through the channel (`true` accepts). Dropping the sender declines.
    fn ask(&self, prompt: SwapPrompt) -> oneshot::Receiver<bool>;

    /// The question is no longer waiting: close its card.
    fn ask_closed(&self, _request_id: u32, _how: AskEnd) {}

    /// Future host, 6.3 step 4: arm `token` for the future viewer's `device` (`HostService::arm_swap_token`).
    fn arm_token(&self, device: DeviceId, token: [u8; 16]);

    /// Future viewer, 6.3 step 5: open a new session connection to `host` with `Hello { swap_token }`. The old
    /// session is already closing. Starts the work and returns; the result reaches the UI by the normal session path.
    fn begin_viewing(&self, host: DeviceId, token: [u8; 16]);
}

/// How a swap this machine asked for ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapOutcome {
    /// Accepted and armed: the old session is closing and the new connection is being opened (future viewer) or
    /// awaited (future host).
    Swapped,
    /// Refused: by the other side (`DECLINED`, `TIMEOUT`, `NOBODY_PRESENT`, `NOT_PERMITTED`, `BUSY`) or here, before
    /// sending, with `NOT_PERMITTED` (the future host has not granted `CONTROL`) or `BUSY` (one swap at a time);
    /// the UI says why.
    Declined(ErrorCode),
    /// No answer within 22 s: `SwapCancel` was sent.
    GaveUp,
    /// The session ended first.
    SessionEnded,
}

#[derive(Debug, Clone, Copy)]
pub struct SwapTimings {
    /// The asked user's time (20 s).
    pub prompt: Duration,
    /// How long the asker waits for any answer (22 s).
    pub give_up: Duration,
    /// How long a side waits for the partner's `SwapArmed` after an accept (the token's life, 10 s).
    pub armed_wait: Duration,
    /// How long the future host waits for the future viewer's `Bye` before closing itself.
    pub close_grace: Duration,
}

impl Default for SwapTimings {
    fn default() -> Self {
        Self {
            prompt: Duration::from_secs(20),
            give_up: Duration::from_secs(22),
            armed_wait: Duration::from_secs(10),
            close_grace: Duration::from_secs(2),
        }
    }
}

enum Slot {
    /// This side asked.
    Outgoing { id: u32, token: Option<[u8; 16]>, done: Option<oneshot::Sender<SwapOutcome>> },
    /// The other side asked, the user is being asked.
    Prompting { id: u32, task: Option<AbortHandle> },
    /// The other side asked, the user accepted, this side is the future viewer and waits for `SwapArmed`.
    AwaitArmed { id: u32, token: [u8; 16] },
}

impl Slot {
    fn id(&self) -> u32 {
        match self {
            Slot::Outgoing { id, .. } | Slot::Prompting { id, .. } | Slot::AwaitArmed { id, .. } => *id,
        }
    }
}

struct Inner {
    env: Arc<dyn SwapEnv>,
    timings: SwapTimings,
    next_id: AtomicU32,
    slots: Mutex<HashMap<(u64, Role), Slot>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The swap [`ChannelHandler`] of one machine. Cheap to clone; every clone is the same handler.
#[derive(Clone)]
pub struct SwapHandler {
    inner: Arc<Inner>,
}

impl SwapHandler {
    pub fn new(env: Arc<dyn SwapEnv>) -> Self {
        Self::with_timings(env, SwapTimings::default())
    }

    pub fn with_timings(env: Arc<dyn SwapEnv>, timings: SwapTimings) -> Self {
        Self { inner: Arc::new(Inner { env, timings, next_id: AtomicU32::new(1), slots: Mutex::new(HashMap::new()) }) }
    }

    /// The handler as the registry wants it.
    pub fn handler(&self) -> Arc<dyn ChannelHandler> {
        Arc::new(self.clone())
    }

    /// "Inverter": asks the other side of `ctx` to swap (the viewer's toolbar, the host's banner). Resolves with how
    /// it ended; on `Swapped` the old session is closing. A future host that has not granted `CONTROL` to the peer
    /// gets `Declined(NOT_PERMITTED)` without anything being sent.
    pub async fn request(&self, ctx: &SessionCtx) -> SwapOutcome {
        let i = &self.inner;
        let Some(peer) = ctx.device() else { return SwapOutcome::Declined(ErrorCode::Unavailable) };
        if !ctx.is_active() {
            return SwapOutcome::SessionEnded;
        }
        let future_host = ctx.role() == Role::Viewer;
        if future_host && !i.env.control_granted_to(&peer) {
            return SwapOutcome::Declined(ErrorCode::NotPermitted);
        }
        let key = (ctx.session_id(), ctx.role());
        let id = i.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut slots = lock(&i.slots);
            if slots.contains_key(&key) {
                return SwapOutcome::Declined(ErrorCode::Busy);
            }
            slots.insert(key, Slot::Outgoing { id, token: None, done: Some(tx) });
        }
        if ctx.send(SwapRequest { request_id: id }).await.is_err() {
            lock(&i.slots).remove(&key);
            return SwapOutcome::SessionEnded;
        }
        match tokio::time::timeout(i.timings.give_up, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => SwapOutcome::SessionEnded,
            Err(_) => {
                // The answer may have landed between the timer and the lock.
                let still_waiting = {
                    let mut slots = lock(&i.slots);
                    let waiting = matches!(slots.get(&key), Some(Slot::Outgoing { id: sid, .. }) if *sid == id);
                    if waiting {
                        slots.remove(&key);
                    }
                    waiting
                };
                if still_waiting {
                    let _ = ctx.send(SwapCancel { request_id: id }).await;
                    SwapOutcome::GaveUp
                } else {
                    SwapOutcome::Swapped
                }
            }
        }
    }

    /// Whether a swap is in flight on this session (for the toolbar's disabled state).
    pub fn pending(&self, ctx: &SessionCtx) -> bool {
        lock(&self.inner.slots).contains_key(&(ctx.session_id(), ctx.role()))
    }

    fn send_later(ctx: &SessionCtx, msgs: Vec<Message>) {
        let c = ctx.clone();
        ctx.spawn(async move {
            for m in msgs {
                if c.send(m).await.is_err() {
                    return;
                }
            }
        });
    }

    fn decline(ctx: &SessionCtx, request_id: u32, code: ErrorCode) {
        Self::send_later(ctx, vec![SwapDecline { request_id, code }.into()]);
    }

    fn on_request(&self, ctx: &SessionCtx, id: u32) {
        let i = &self.inner;
        let Some(peer) = ctx.device() else { return Self::decline(ctx, id, ErrorCode::Unavailable) };
        let future_host = ctx.role() == Role::Viewer;
        let key = (ctx.session_id(), ctx.role());
        let refusal = {
            let mut slots = lock(&i.slots);
            let code = if slots.contains_key(&key) {
                Some(ErrorCode::Busy)
            } else if future_host && !i.env.control_granted_to(&peer) {
                // 6.3 step 1: no prompt.
                Some(ErrorCode::NotPermitted)
            } else if !i.env.somebody_present() {
                Some(ErrorCode::NobodyPresent)
            } else {
                None
            };
            if code.is_none() {
                slots.insert(key, Slot::Prompting { id, task: None });
            }
            code
        };
        if let Some(code) = refusal {
            return Self::decline(ctx, id, code);
        }
        let answer = i.env.ask(SwapPrompt {
            request_id: id,
            device: peer,
            asked_becomes_host: future_host,
            timeout: i.timings.prompt,
        });
        let (inner, c) = (Arc::clone(&self.inner), ctx.clone());
        let task = tokio::spawn(async move {
            let e = &inner.env;
            let verdict = tokio::time::timeout(inner.timings.prompt, answer).await;
            let drop_slot = || {
                let mut slots = lock(&inner.slots);
                if matches!(slots.get(&key), Some(s) if s.id() == id) {
                    slots.remove(&key);
                }
            };
            match verdict {
                Err(_) => {
                    e.ask_closed(id, AskEnd::TimedOut);
                    drop_slot();
                    let _ = c.send(SwapDecline { request_id: id, code: ErrorCode::Timeout }).await;
                }
                Ok(Ok(true)) => {
                    e.ask_closed(id, AskEnd::Answered);
                    // The grant may have been revoked while the card was open.
                    if future_host && !e.control_granted_to(&peer) {
                        drop_slot();
                        let _ = c.send(SwapDecline { request_id: id, code: ErrorCode::NotPermitted }).await;
                        return;
                    }
                    let token = random_token();
                    if future_host {
                        // 6.3 step 4 right here: armed before the accept leaves, so the viewer can connect at once.
                        e.arm_token(peer, token);
                        drop_slot();
                        if c.send(SwapAccept { request_id: id, token }).await.is_ok()
                            && c.send(SwapArmed { request_id: id }).await.is_ok()
                        {
                            close_after(&c, inner.timings.close_grace).await;
                        }
                    } else {
                        {
                            let mut slots = lock(&inner.slots);
                            if !matches!(slots.get(&key), Some(Slot::Prompting { id: sid, .. }) if *sid == id) {
                                return;
                            }
                            slots.insert(key, Slot::AwaitArmed { id, token });
                        }
                        if c.send(SwapAccept { request_id: id, token }).await.is_err() {
                            drop_slot();
                            return;
                        }
                        tokio::time::sleep(inner.timings.armed_wait).await;
                        let mut slots = lock(&inner.slots);
                        if matches!(slots.get(&key), Some(Slot::AwaitArmed { id: sid, .. }) if *sid == id) {
                            slots.remove(&key);
                        }
                    }
                }
                Ok(Ok(false) | Err(_)) => {
                    e.ask_closed(id, AskEnd::Answered);
                    drop_slot();
                    let _ = c.send(SwapDecline { request_id: id, code: ErrorCode::Declined }).await;
                }
            }
        });
        if let Some(Slot::Prompting { task: t, .. }) = lock(&i.slots).get_mut(&key) {
            *t = Some(task.abort_handle());
        }
    }

    fn on_accept(&self, ctx: &SessionCtx, id: u32, token: [u8; 16]) {
        let i = &self.inner;
        let Some(peer) = ctx.device() else { return };
        let key = (ctx.session_id(), ctx.role());
        let future_host = ctx.role() == Role::Viewer;
        let mut slots = lock(&i.slots);
        let Some(Slot::Outgoing { id: sid, token: stored, done }) = slots.get_mut(&key) else { return };
        if *sid != id || stored.is_some() {
            return;
        }
        if future_host {
            // A is the future host: 6.3 step 4.
            let done = done.take();
            slots.remove(&key);
            drop(slots);
            i.env.arm_token(peer, token);
            let (c, grace) = (ctx.clone(), i.timings.close_grace);
            ctx.spawn(async move {
                if c.send(SwapArmed { request_id: id }).await.is_ok() {
                    close_after(&c, grace).await;
                }
            });
            if let Some(d) = done {
                let _ = d.send(SwapOutcome::Swapped);
            }
        } else {
            // A is the future viewer: B arms and says so with `SwapArmed`.
            *stored = Some(token);
        }
    }

    fn on_armed(&self, ctx: &SessionCtx, id: u32) {
        let i = &self.inner;
        let Some(peer) = ctx.device() else { return };
        let key = (ctx.session_id(), ctx.role());
        if ctx.role() != Role::Host {
            // Only the future viewer (the host now) receives `SwapArmed`.
            return;
        }
        let mut slots = lock(&i.slots);
        let (token, done) = match slots.get_mut(&key) {
            Some(Slot::Outgoing { id: sid, token: Some(t), done }) if *sid == id => (*t, done.take()),
            Some(Slot::AwaitArmed { id: sid, token }) if *sid == id => (*token, None),
            _ => return,
        };
        slots.remove(&key);
        drop(slots);
        // 6.3 step 5: close the old connection, open the new one with the token.
        ctx.bye(ErrorCode::Swapped);
        i.env.begin_viewing(peer, token);
        if let Some(d) = done {
            let _ = d.send(SwapOutcome::Swapped);
        }
    }

    fn on_decline(&self, ctx: &SessionCtx, id: u32, code: ErrorCode) {
        let key = (ctx.session_id(), ctx.role());
        let mut slots = lock(&self.inner.slots);
        if let Some(Slot::Outgoing { id: sid, done, .. }) = slots.get_mut(&key)
            && *sid == id
        {
            let done = done.take();
            slots.remove(&key);
            drop(slots);
            if let Some(d) = done {
                let _ = d.send(SwapOutcome::Declined(code));
            }
        }
    }

    fn on_cancel(&self, ctx: &SessionCtx, id: u32) {
        let key = (ctx.session_id(), ctx.role());
        let mut slots = lock(&self.inner.slots);
        match slots.get(&key) {
            Some(Slot::Prompting { id: sid, task }) if *sid == id => {
                let task = task.clone();
                slots.remove(&key);
                drop(slots);
                if let Some(t) = task {
                    t.abort();
                }
                self.inner.env.ask_closed(id, AskEnd::Cancelled);
            }
            Some(Slot::AwaitArmed { id: sid, .. }) if *sid == id => {
                slots.remove(&key);
            }
            _ => {}
        }
    }
}

/// The future host's session ends with the future viewer's `Bye`; this is the fallback when none comes.
async fn close_after(ctx: &SessionCtx, grace: Duration) {
    tokio::time::sleep(grace).await;
    ctx.bye(ErrorCode::Swapped);
}

impl ChannelHandler for SwapHandler {
    fn name(&self) -> &'static str {
        "swap"
    }

    fn messages(&self) -> &'static [u16] {
        SWAP_TYPES
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        match msg {
            Message::SwapRequest(m) => self.on_request(ctx, m.request_id),
            Message::SwapAccept(m) => self.on_accept(ctx, m.request_id, m.token),
            Message::SwapDecline(m) => self.on_decline(ctx, m.request_id, m.code),
            Message::SwapCancel(m) => self.on_cancel(ctx, m.request_id),
            Message::SwapArmed(m) => self.on_armed(ctx, m.request_id),
            _ => {}
        }
    }

    fn on_end(&self, ctx: &SessionCtx, _end: &SessionEnd) {
        let key = (ctx.session_id(), ctx.role());
        let Some(slot) = lock(&self.inner.slots).remove(&key) else { return };
        match slot {
            Slot::Outgoing { done, .. } => {
                if let Some(d) = done {
                    let _ = d.send(SwapOutcome::SessionEnded);
                }
            }
            Slot::Prompting { id, task } => {
                if let Some(t) = task {
                    t.abort();
                }
                self.inner.env.ask_closed(id, AskEnd::Cancelled);
            }
            Slot::AwaitArmed { .. } => {}
        }
    }
}
