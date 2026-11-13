//! Swap, chat and Wake-on-LAN in the hub (stage 41): the UI ops `RequestSwap`, `AnswerSwap`, `ChatSend`,
//! `ChatMarkRead` and `Wake`, and the [`ExtrasHooks`] the core reports swap questions, new swap sessions and chat to.
//!
//! * **Swap.** "Inverter" asks through the core's swap handler; the reply carries a request number at once and the
//!   outcome arrives as `Notify::Swap`. When this machine is asked, the question is a `Swap { Asked }` card on the UI
//!   that owns the viewer session (or the console's UI) with the handler's 20 s; `AnswerSwap` resolves it. The new
//!   viewer session of an accepted swap goes to the console UI like any session opened by `Connect`.
//! * **Chat.** Messages from the peer become `Notify::Chat` on the UI that shows the session; what this UI sends is
//!   answered with the message as stored.
//! * **Wake.** The device's stored MACs and last address make the route (`wake_route`), the socket is bound to it
//!   with TTL 1, and the attempt runs off every thread of the UI path until the device appears, the minute ends, or
//!   the user cancels; on `Appeared` the hub connects as `Connect` would, as the same entry.

use std::io;
use std::net::Ipv4Addr;
use std::sync::{Arc, Weak};

use swapdesk_extras::chat::ChatError;
use swapdesk_extras::wake::{
    UdpWake, WakeConfig, WakeError, WakeOutcome, WakeRoute, WakeSocket, wake_and_wait, wake_route,
};
use swapdesk_ipc::{
    ApiCode, ChatItem, Message as Ipc, Notify, ReplyBody, SessionState, SwapNote, SwapWhy, Text, WAKE_WINDOW_S,
};
use swapdesk_platform::Level;
use swapdesk_proto::{ErrorCode, GrantMode, Intent, Os};
use swapdesk_swap::{AskEnd, SwapOutcome, SwapPrompt};
use swapdesk_trust::{AuditEvent, DeviceId, device_id_hex};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

use super::ops::reply;
use super::views::parse_device_id;
use super::{Conn, Hub, VideoPrefs, Viewer, lock};
use crate::service::{ChatNote, ConnectError, ExtrasHooks, ViewerHandle, ViewerMessages};

/// The entries (a waking attempt, a connect that waits for the host) get ids with this bit set so they never meet a
/// session id the host picked at random in practice.
pub(super) const ENTRY_ID_BIT: u64 = 1 << 63;

/// A swap question waiting for this machine's user.
pub(crate) struct PendingSwap {
    tx: oneshot::Sender<bool>,
    /// The connections the card went to: only they may answer.
    conns: Vec<u64>,
    session_id: u64,
}

/// A connect attempt in flight, from a click on "Controlar" or "Acordar" until its session opens or it ends: the
/// interface shows it as a card and may cancel it.
pub(crate) struct Entry {
    pub(super) abort: AbortHandle,
    pub(super) conn: u64,
    pub(super) device_hex: String,
}

/// How the hub opens the socket of a Wake-on-LAN attempt and how long it runs. The default is the real thing (a UDP
/// socket bound to the route, TTL 1, 60 s); tests give a fake socket and a short window.
pub(crate) struct WakeSetup {
    pub binder: WakeBinder,
    pub cfg: WakeConfig,
}

pub type WakeBinder = Arc<dyn Fn(&WakeRoute) -> io::Result<Box<dyn WakeSocket + Send>> + Send + Sync>;

impl Default for WakeSetup {
    fn default() -> Self {
        Self {
            binder: Arc::new(|route| UdpWake::bind(route).map(|s| Box::new(s) as Box<dyn WakeSocket + Send>)),
            cfg: WakeConfig::default(),
        }
    }
}

struct BoxedWake(Box<dyn WakeSocket + Send>);

impl WakeSocket for BoxedWake {
    fn send_to(&mut self, packet: &[u8], dest: std::net::SocketAddrV4) -> io::Result<()> {
        self.0.send_to(packet, dest)
    }
}

fn swap_why(code: ErrorCode) -> SwapWhy {
    match code {
        ErrorCode::Timeout => SwapWhy::TimedOut,
        ErrorCode::NobodyPresent => SwapWhy::NobodyPresent,
        ErrorCode::NotPermitted => SwapWhy::NotPermitted,
        ErrorCode::Busy => SwapWhy::Busy,
        _ => SwapWhy::Declined,
    }
}

fn chat_code(e: ChatError) -> ApiCode {
    match e {
        ChatError::Empty | ChatError::TooLong => ApiCode::Invalid,
        ChatError::Busy => ApiCode::Busy,
        ChatError::Closed => ApiCode::NotFound,
    }
}

impl Hub {
    // ---------- who sees what ----------

    /// The connections that show session `session_id`: the one that owns the viewer, else the console's.
    pub(super) fn conns_for_session(&self, session_id: u64) -> Vec<Arc<Conn>> {
        let owners: Vec<Arc<Conn>> =
            self.conns().into_iter().filter(|c| lock(&c.viewers).contains_key(&session_id)).collect();
        if owners.is_empty() { self.console_conns() } else { owners }
    }

    /// May `conn` act on session `session_id`: its own viewer session, or (console UI) a session this machine hosts.
    pub(super) fn can_use_session(&self, conn: &Conn, session_id: u64) -> bool {
        if lock(&conn.viewers).contains_key(&session_id) {
            return true;
        }
        self.is_console(conn) && self.core().is_some_and(|c| c.host().session(session_id).is_some())
    }

    /// The session (either role) with `device`, 0 when there is none.
    fn session_of_device(&self, device: &DeviceId) -> u64 {
        for c in self.conns() {
            if let Some((id, _)) = lock(&c.viewers).iter().find(|(_, v)| v.device == *device) {
                return *id;
            }
        }
        self.core()
            .and_then(|core| core.host().sessions().into_iter().find(|s| s.device == *device))
            .map_or(0, |s| s.session_id)
    }

    // ---------- swap ----------

    /// The question for this machine's user. It goes to the console session's UI only, whichever way the swap turns:
    /// when the machine is the viewer now it would become the host, and only the person at the console may agree to
    /// that (the swap token then skips the host's prompt and presence check). A UI of another session, even the one
    /// that owns the viewer session, never hears it.
    fn swap_ask(&self, prompt: SwapPrompt) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        // Since 1.1 a swap works between Windows and macOS too (F-XSWAP-01); a 1.0 peer still declines one across
        // systems on its own side, and the asker hears that refusal.
        let conns = self.console_conns();
        if conns.is_empty() {
            // Nobody to ask: the sender goes, which declines.
            return rx;
        }
        let session_id = self.session_of_device(&prompt.device);
        let by = self.device_view_of(&prompt.device, "", Os::Windows).name;
        let remaining_s = u16::try_from(prompt.timeout.as_secs()).unwrap_or(u16::MAX);
        let note = SwapNote::Asked { request_id: prompt.request_id, by, remaining_s };
        lock(&self.swaps)
            .insert(prompt.request_id, PendingSwap { tx, conns: conns.iter().map(|c| c.id).collect(), session_id });
        for c in conns {
            c.queue.control(Ipc::UiNotify(Notify::Swap { session_id, note: note.clone() }));
        }
        rx
    }

    /// The question is no longer waiting (timed out, cancelled, the session ended): its card closes.
    fn swap_ask_closed(&self, request_id: u32, how: AskEnd) {
        let Some(p) = lock(&self.swaps).remove(&request_id) else { return };
        if how == AskEnd::Answered {
            return;
        }
        for c in self.conns().into_iter().filter(|c| p.conns.contains(&c.id)) {
            c.queue.control(Ipc::UiNotify(Notify::Swap { session_id: p.session_id, note: SwapNote::Done }));
        }
    }

    /// `AnswerSwap`: the user answered the card; only a connection the card went to may, and only while it is still
    /// the console's (the console can move to another session while the card waits).
    pub fn answer_swap(&self, conn: &Conn, request_id: u32, accept: bool) -> Result<(), ApiCode> {
        self.require_console(conn, "answer to a swap card from a UI that is not the console's")?;
        let mut swaps = lock(&self.swaps);
        let allowed = swaps.get(&request_id).ok_or(ApiCode::NotFound)?.conns.contains(&conn.id);
        if !allowed {
            return Err(self.api_error(ApiCode::NotPermitted, "answer to a swap card another UI holds"));
        }
        let pending = swaps.remove(&request_id).ok_or(ApiCode::NotFound)?;
        drop(swaps);
        let _ = pending.tx.send(accept);
        Ok(())
    }

    /// `RequestSwap`: replies with a request number at once, then reports the outcome as `Notify::Swap`.
    pub fn request_swap(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64) {
        if !self.can_use_session(conn, session_id) {
            return reply(conn, req, Err(ApiCode::NotFound));
        }
        // A swap of a session this connection views makes THIS machine the host, with the swap token skipping the
        // prompt and the presence check: only the console's UI may ask for that. (A hosted session is the console's
        // already, by `can_use_session`.)
        if let Err(code) = self.require_console(conn, "a swap request from a UI that is not the console's") {
            return reply(conn, req, Err(code));
        }
        let (Some(core), Some(rt)) = (self.core(), self.rt()) else {
            return reply(conn, req, Err(ApiCode::ServiceDown));
        };
        let Ok(task) = core.swap_request(session_id) else { return reply(conn, req, Err(ApiCode::NotFound)) };
        drop(core);
        reply(conn, req, Ok(ReplyBody::RequestId(self.fresh_id())));
        let me = Arc::clone(self);
        rt.spawn(async move {
            let note = match task.await {
                SwapOutcome::Swapped | SwapOutcome::SessionEnded => SwapNote::Done,
                SwapOutcome::Declined(code) => SwapNote::Declined { why: swap_why(code) },
                SwapOutcome::GaveUp => SwapNote::Declined { why: SwapWhy::TimedOut },
            };
            for c in me.conns_for_session(session_id) {
                c.queue.control(Ipc::UiNotify(Notify::Swap { session_id, note: note.clone() }));
            }
        });
    }

    /// The new viewer session of an accepted swap: this machine was the host and now views `device`. It goes to the
    /// console's UI and relays like a session `Connect` opened.
    fn swap_viewing(self: &Arc<Self>, device: DeviceId, opened: Result<(ViewerHandle, ViewerMessages), ConnectError>) {
        let (handle, messages) = match opened {
            Ok(v) => v,
            Err(e) => {
                return self.log(
                    Level::Warn,
                    format_args!("the swap with {} did not open its new connection: {e}", device_id_hex(&device)),
                );
            }
        };
        let Some(conn) = self.console_conns().into_iter().next() else {
            self.log(Level::Warn, format_args!("a swap opened a session and nobody is at the console to show it"));
            handle.disconnect();
            return;
        };
        let (Some(rt), session_id) = (self.rt(), handle.session_id()) else { return };
        self.p.audit.append(swapdesk_trust::AuditEntry {
            time_ms: self.p.wall.unix_ms(),
            device: swapdesk_trust::DeviceRef::Known(device),
            event: AuditEvent::Swapped,
            duration_s: None,
        });
        handle.files().bind(conn.fs.clone());
        lock(&conn.viewers).insert(
            session_id,
            Viewer {
                handle: handle.clone(),
                device,
                view_only: false,
                displays: Vec::new(),
                elevated: false,
                video: VideoPrefs::default(),
            },
        );
        let me = Arc::clone(self);
        rt.spawn(
            async move { me.pump_viewer(conn, session_id, device, handle, messages, GrantMode::Swap, None).await },
        );
    }

    // ---------- chat ----------

    fn chat_note(&self, session_id: u64, note: ChatNote) {
        let n = match note {
            ChatNote::Received(e) => Notify::Chat {
                session_id,
                item: ChatItem { msg_id: e.msg_id, text: Text(e.text), sent_at_ms: e.sent_at_ms },
            },
            ChatNote::ReadByPeer { up_to } => Notify::ChatRead { session_id, up_to },
        };
        for c in self.conns_for_session(session_id) {
            c.queue.control(Ipc::UiNotify(n.clone()));
        }
    }

    /// `ChatSend`: answered with the message as stored, or why not.
    pub fn chat_send(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64, text: String) {
        if !self.can_use_session(conn, session_id) {
            return reply(conn, req, Err(ApiCode::NotFound));
        }
        let (Some(core), Some(rt)) = (self.core(), self.rt()) else {
            return reply(conn, req, Err(ApiCode::ServiceDown));
        };
        let Ok(task) = core.chat_send_task(session_id, text) else { return reply(conn, req, Err(ApiCode::NotFound)) };
        drop(core);
        let conn = Arc::clone(conn);
        rt.spawn(async move {
            let result = task
                .await
                .map(|e| ReplyBody::Chat(ChatItem { msg_id: e.msg_id, text: Text(e.text), sent_at_ms: e.sent_at_ms }))
                .map_err(chat_code);
            reply(&conn, req, result);
        });
    }

    /// `ChatMarkRead`: the peer is told the panel shows everything.
    pub fn chat_read(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64) {
        if !self.can_use_session(conn, session_id) {
            return reply(conn, req, Err(ApiCode::NotFound));
        }
        let (Some(core), Some(rt)) = (self.core(), self.rt()) else {
            return reply(conn, req, Err(ApiCode::ServiceDown));
        };
        let Ok(task) = core.chat_read_task(session_id) else { return reply(conn, req, Err(ApiCode::NotFound)) };
        drop(core);
        reply(conn, req, Ok(ReplyBody::Done));
        rt.spawn(task);
    }

    // ---------- wake ----------

    /// The device is on the network now (discovery sees it with an address).
    fn device_appeared(&self, id: &DeviceId) -> bool {
        self.p.presence.peers().iter().any(|p| &p.device_id == id && !p.addrs.is_empty())
    }

    /// `Wake`: replies with the id of the waking entry, then runs the attempt (F-WAKE-01).
    pub fn wake(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, device_id: &str, intent: Intent) {
        // The attempt ends in a `Connect`: the same rule.
        if let Err(code) = self.require_console(conn, "a wake from a UI that is not the console's") {
            return reply(conn, req, Err(code));
        }
        let Some(id) = parse_device_id(device_id) else { return reply(conn, req, Err(ApiCode::Invalid)) };
        let Some(dev) = self.p.trust.read(|s| s.find_by_id(&id).cloned()) else {
            return reply(conn, req, Err(ApiCode::NotFound));
        };
        // The MACs it reported after pinning, and the address it had last: nothing here comes from the network now.
        if dev.wol_macs.is_empty() {
            return reply(
                conn,
                req,
                Err(self.api_error(ApiCode::Invalid, "wake of a device with no stored hardware address")),
            );
        }
        let Some(last) = dev.last_addrs.first().copied() else { return reply(conn, req, Err(ApiCode::NotFound)) };
        let Ok(list) = self.p.interfaces.list() else { return reply(conn, req, Err(ApiCode::Internal)) };
        let Ok(route) = wake_route(&list, Ipv4Addr::from(last)) else {
            return reply(
                conn,
                req,
                Err(self.api_error(ApiCode::Refused, "no approved interface holds the device's last address")),
            );
        };
        let (binder, cfg) = {
            let w = lock(&self.wake);
            (Arc::clone(&w.binder), w.cfg)
        };
        let socket = match binder(&route) {
            Ok(s) => s,
            Err(e) => {
                self.log(Level::Warn, format_args!("the wake socket could not be made: {e}"));
                return reply(conn, req, Err(ApiCode::Internal));
            }
        };
        let Some(rt) = self.rt() else { return reply(conn, req, Err(ApiCode::ServiceDown)) };
        let wake_id = ENTRY_ID_BIT | u64::from(self.fresh_id());
        reply(conn, req, Ok(ReplyBody::SessionId(wake_id)));
        let (me, c, macs) = (Arc::clone(self), Arc::clone(conn), dev.wol_macs.clone());
        // The entry is registered under the lock the task ends with, so a fast end cannot leave a stale one.
        let mut entries = lock(&self.entries);
        let task =
            rt.spawn(async move { me.run_wake(c, wake_id, id, macs, route, BoxedWake(socket), cfg, intent).await });
        entries.insert(wake_id, Entry { abort: task.abort_handle(), conn: conn.id, device_hex: device_id_hex(&id) });
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_wake(
        self: Arc<Self>,
        conn: Arc<Conn>,
        wake_id: u64,
        id: DeviceId,
        macs: Vec<[u8; 6]>,
        route: WakeRoute,
        mut socket: BoxedWake,
        cfg: WakeConfig,
        intent: Intent,
    ) {
        let hex = device_id_hex(&id);
        let state = |state: SessionState| {
            conn.queue.control(Ipc::UiNotify(Notify::Session { session_id: wake_id, device_id: hex.clone(), state }));
        };
        let window = u16::try_from(cfg.give_up.as_secs()).unwrap_or(WAKE_WINDOW_S);
        let appeared = || {
            let me = Arc::clone(&self);
            async move { me.device_appeared(&id) }
        };
        let outcome = wake_and_wait(&mut socket, &route, &macs, cfg, appeared, || {
            state(SessionState::Waking { remaining_s: window })
        })
        .await;
        let reason = match outcome {
            // The same entry goes on as a connect: it ends with the session's start or with why it did not open, and
            // a cancel in any phase drops this task, which tells the host.
            Ok(WakeOutcome::Appeared) => return self.run_connect(conn, wake_id, id, intent).await,
            Ok(WakeOutcome::GaveUp) => Some(ApiCode::Timeout),
            Err(WakeError::NoMacs) => Some(ApiCode::Invalid),
            Err(WakeError::Send(_)) => Some(ApiCode::Internal),
        };
        if lock(&self.entries).remove(&wake_id).is_some() {
            state(SessionState::Ended { reason });
        }
    }

    /// "Cancelar" on a card of the device's page (waking, or a connect waiting for the host): the attempt stops, which
    /// closes the host's question, and its entry ends (a normal end).
    pub fn cancel_entry(&self, conn: &Conn, entry_id: u64) -> bool {
        let mut entries = lock(&self.entries);
        if entries.get(&entry_id).is_none_or(|e| e.conn != conn.id) {
            return false;
        }
        let Some(entry) = entries.remove(&entry_id) else { return false };
        drop(entries);
        entry.abort.abort();
        conn.queue.control(Ipc::UiNotify(Notify::Session {
            session_id: entry_id,
            device_id: entry.device_hex,
            state: SessionState::Ended { reason: None },
        }));
        true
    }

    /// The UI connection is gone: its attempts stop (nobody is left to show them) and none stays in the map.
    pub(super) fn abort_entries_of(&self, conn: &Conn) {
        let mut entries = lock(&self.entries);
        let ids: Vec<u64> = entries.iter().filter(|(_, e)| e.conn == conn.id).map(|(id, _)| *id).collect();
        for id in ids {
            if let Some(entry) = entries.remove(&id) {
                entry.abort.abort();
            }
        }
    }
}

/// What the core reports, handed to the hub (weakly: the owner's handle on the hub keeps it alive).
pub(crate) struct HubExtras {
    pub hub: Weak<Hub>,
}

impl ExtrasHooks for HubExtras {
    fn swap_ask(&self, prompt: SwapPrompt) -> oneshot::Receiver<bool> {
        match self.hub.upgrade() {
            Some(h) => h.swap_ask(prompt),
            None => oneshot::channel().1,
        }
    }

    fn swap_ask_closed(&self, request_id: u32, how: AskEnd) {
        if let Some(h) = self.hub.upgrade() {
            h.swap_ask_closed(request_id, how);
        }
    }

    fn address_of(&self, device: &DeviceId) -> Option<std::net::SocketAddr> {
        self.hub.upgrade()?.address_of(device).map(std::net::SocketAddr::V4)
    }

    fn swap_viewing(&self, device: DeviceId, opened: Result<(ViewerHandle, ViewerMessages), ConnectError>) {
        if let Some(h) = self.hub.upgrade() {
            h.swap_viewing(device, opened);
        }
    }

    fn chat(&self, session_id: u64, note: ChatNote) {
        if let Some(h) = self.hub.upgrade() {
            h.chat_note(session_id, note);
        }
    }

    fn terminal_opened(&self, device: DeviceId, admin: bool) {
        if let Some(h) = self.hub.upgrade() {
            h.terminal_opened(device, admin);
        }
    }

    fn files(&self, session_id: u64, event: swapdesk_files::Event) {
        if let Some(h) = self.hub.upgrade() {
            h.files_event(session_id, event);
        }
    }

    fn console_user(&self) -> Option<String> {
        let id = self.hub.upgrade()?.console_user().id;
        (!id.is_empty()).then_some(id)
    }
}
