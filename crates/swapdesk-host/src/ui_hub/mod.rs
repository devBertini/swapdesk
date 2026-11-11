//! The UI hub (`docs/CONTRACTS.md` C2.4, stage 18d-2): what the service answers on a UI pipe, over any `Read` /
//! `Write` pair so it needs no OS. One [`UiHub`] serves the whole machine; every interactive session's UI connects
//! to it ([`UiHub::serve`], one blocking call per connection, after the server handshake).
//!
//! * Requests: every stage-18 `UiOp` (1 to 21) is dispatched here, with the checks of C2.3 (`grants` validated as
//!   PROTOCOL.md 6.1 says before any use; granting `UNATTENDED` or `ADMIN_TERMINAL`
//!   only on an administrator connection, or after an administrator confirmed exactly that change, see
//!   `elevation.rs`; no request carries a path).
//! * Notifications: devices, network, pairing, incoming requests, sessions, displays, stats, alerts and settings go
//!   to the connections that should see them (host-side items to the console session's UI only).
//! * Media: a viewer session opened here keeps its `ViewerHandle` on the connection; its video, audio and cursor
//!   go out as `ViewerVideo`, `ViewerAudio` and `ViewerCursor*`, its input, keyframe requests and reports come in.
//! * [`UiHub::host_ui`] is the [`swapdesk_session::HostUi`] of the core: a prompt becomes `IncomingRequest` on the
//!   console session's UI and waits for `AnswerIncoming`; with no UI there it is declined at once.
//!
//! Wiring order: build the hub, give `hub.host_ui()` to `ServiceConfig`, start the core, then
//! [`UiHub::bind_core`]. The hub holds the core weakly, so the owner's `Arc<ServiceCore>` alone keeps it alive.

mod clip_actions;
mod elevation;
mod extras;
mod files;
mod grants_sync;
mod host_ui;
mod ops;
mod pairing;
mod peer_minor;
pub mod presence;
mod queue;
pub mod store;
mod sync_dial;
mod terminal;
mod views;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};

use swapdesk_files::{Pending, PipeCaller};
use swapdesk_ipc::{ApiCode, FrameReader, FrameWriter, IpcError, Message as Ipc, Notify};
use swapdesk_platform::{Clock, Level, NetInterfaces, SecretStore, TraceSink};
use swapdesk_proto::{Desktop, Os};
use swapdesk_session::HostUi;
use swapdesk_trust::{DeviceId, Identity, PairingMode, TrustHandle, UserRef, WallClock};
use tokio::runtime::Handle;

use crate::service::{ServiceCore, ViewerHandle};
use extras::{Entry, HubExtras, PendingSwap, WakeSetup};
use host_ui::HubHostUi;
pub use presence::{DnsSdPresence, FakePresence, NoPresence, Presence, PresencePeer};
use queue::UiQueue;
pub use store::{AuditLog, FileAuditLog, MachineSettings, load_settings, store_settings};

/// What the hub asks of the machine beyond the C3 traits. Every method has a do-nothing default.
pub trait SystemHooks: Send + Sync {
    /// A session is live (or none is) and the "keep awake" setting is on: hold off sleep.
    fn keep_awake(&self, _on: bool) {}
    /// "Bloquear ao terminar" (F-SESS-03): lock the workstation.
    fn lock_screen(&self) {}
    /// The "service" switch of Ajustes changed.
    fn service_enabled(&self, _on: bool) {}
}

/// Hooks that do nothing.
pub struct NoHooks;

impl SystemHooks for NoHooks {}

/// Development and test options (C2.4 "Development mode"): loopback is accepted as a peer address and the pairing
/// endpoint also listens on these addresses.
#[derive(Debug, Clone, Default)]
pub struct DevOptions {
    /// Extra pairing listen addresses (the dev counterpart of `ServiceConfig::listen_extra`).
    pub pair_listen: Vec<SocketAddrV4>,
}

/// Everything the hub is built from.
pub struct HubParams {
    pub os: Os,
    pub os_version: String,
    pub identity: Arc<Identity>,
    pub trust: Arc<TrustHandle>,
    /// Where `settings.bin` lives.
    pub secrets: Arc<dyn SecretStore>,
    /// The machine settings as loaded ([`load_settings`]); the hub keeps and persists changes.
    pub settings: MachineSettings,
    pub interfaces: Arc<dyn NetInterfaces>,
    pub presence: Arc<dyn Presence>,
    pub audit: Arc<dyn AuditLog>,
    pub clock: Arc<dyn Clock>,
    pub wall: Arc<dyn WallClock>,
    pub trace: Arc<dyn TraceSink>,
    pub hooks: Arc<dyn SystemHooks>,
    /// `Some` in development mode and in tests.
    pub dev: Option<DevOptions>,
}

/// Who is on the other end of a UI pipe, as the pipe checks established them (C2.1, C2.3).
#[derive(Debug, Clone)]
pub struct ConnInfo {
    /// The Windows session the client runs in.
    pub session_id: u32,
    /// The session's user (SID and account name); recorded as the grantor of `UNATTENDED`.
    pub user: UserRef,
    /// The impersonation token is elevated with Administrators enabled (C2.3).
    pub admin: bool,
    /// The connection's user is an administrator account (its token holds the Administrators group, enabled or
    /// deny-only): it may ask for an administrator confirmation (`ElevationBegin`) without being elevated itself.
    pub can_elevate: bool,
}

/// One connected UI.
pub(crate) struct Conn {
    pub id: u64,
    pub info: ConnInfo,
    pub queue: UiQueue,
    /// Blocking requests to this UI (the clipboard of the viewer role); its answers come in `dispatch`.
    pub clip: Arc<crate::rpc::Rpc>,
    /// The viewer sessions this connection opened, by session id.
    pub viewers: Mutex<HashMap<u64, Viewer>>,
    /// The terminals this connection opened: the connection-wide id and the viewer session each runs in.
    pub terms: Mutex<HashMap<u32, u64>>,
    /// The disk of this UI (C2.4 "Files"): the service's calls to it wait for the `FsReply` the reader routes back.
    pub fs: Arc<PipeCaller>,
    /// What an administrator confirmed for this connection and it has not used yet (see `elevation.rs`).
    pub passes: Mutex<Vec<elevation::Pass>>,
}

/// A viewer session on a connection.
pub(crate) struct Viewer {
    pub handle: ViewerHandle,
    pub device: DeviceId,
    /// The UI asked for view-only: input is not forwarded (a local choice, the host is not told).
    pub view_only: bool,
    /// The host's display list as last relayed (`Notify::Displays`); what `ListDisplays` answers.
    pub displays: Vec<swapdesk_ipc::DisplayView>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The state every connection, task and host callback shares.
pub(crate) struct Hub {
    pub p: HubParams,
    core: Mutex<Weak<ServiceCore>>,
    rt: OnceLock<Handle>,
    /// The SID of the account this hub serves, on a machine of several (ACCOUNTS.md 7.1); unset for the only one.
    account: OnceLock<String>,
    /// The SID of the user at the console (`None`: the sign-in screen), for the console rule of ACCOUNTS.md 5.1.
    console_user: Mutex<Option<String>>,
    settings: Mutex<MachineSettings>,
    conns: Mutex<Vec<Arc<Conn>>>,
    next_conn: AtomicU64,
    console: Mutex<Option<u32>>,
    desktop: Mutex<Desktop>,
    pub mode: Arc<PairingMode>,
    pairing: pairing::PairState,
    incoming: Mutex<HashMap<u32, host_ui::Pending>>,
    next_id: AtomicU32,
    /// Addresses typed by the user (`AddByAddress`): placeholders for devices not paired yet.
    manual: Mutex<Vec<views::Manual>>,
    /// Where the devices paired through a typed address answer (session endpoint), remembered while we run.
    learned: Mutex<HashMap<DeviceId, SocketAddrV4>>,
    /// The protocol minor each paired device said it speaks, beside the hint of its discovery record.
    minors: Mutex<peer_minor::PeerMinors>,
    /// The writes of what each paired device reported it allows here (`grants_there`), at most one a second.
    peer_writes: Mutex<grants_sync::PeerWrites>,
    /// The grants sync outside a session, per paired device (GRANTS-SYNC.md 4.3).
    sync: Mutex<sync_dial::SyncState>,
    /// The session port each paired device was last seen on (`peers.bin`, ACCOUNTS.md 5.2), and when it was written.
    peer_ports: Mutex<(store::PeerPorts, HashMap<DeviceId, std::time::Instant>)>,
    /// Live sessions in either role, for "keep awake".
    live_sessions: AtomicUsize,
    /// Swap questions waiting for this machine's user, by the swap handler's request number.
    swaps: Mutex<HashMap<u32, PendingSwap>>,
    /// Connect attempts in flight (a wake, a connect waiting for the host), by the id of their entry.
    entries: Mutex<HashMap<u64, Entry>>,
    /// File transfers and listings in flight, for the files ops and notifications.
    files: Mutex<files::FilesTable>,
    wake: Mutex<WakeSetup>,
    /// The open administrator confirmations (tickets) and the lifetimes they run on.
    elevation: Mutex<elevation::Elevation>,
    /// Host sessions: when they started (Unix ms) and what they hold, for the durations of the audit log and the
    /// `Active` states.
    started: Mutex<HashMap<u64, (u64, swapdesk_proto::PermissionSet)>>,
    shutting: AtomicBool,
}

impl Hub {
    /// Who owns the console, seen from this account (ACCOUNTS.md 5.1).
    pub fn console_owner(&self) -> swapdesk_session::ConsoleOwner {
        use swapdesk_session::ConsoleOwner;
        let Some(account) = self.account.get() else { return ConsoleOwner::This };
        match lock(&self.console_user).as_deref() {
            None => ConsoleOwner::Nobody,
            Some(u) if u.eq_ignore_ascii_case(account) => ConsoleOwner::This,
            Some(_) => ConsoleOwner::Other,
        }
    }

    /// Tells this account's sessions the desktop they are on: the console's, or `AccountAway` while another account
    /// owns it.
    pub fn push_desktop(&self) {
        let desktop = if self.console_owner() == swapdesk_session::ConsoleOwner::Other {
            Desktop::AccountAway
        } else {
            *lock(&self.desktop)
        };
        if let Some(core) = self.core() {
            core.host().set_desktop(desktop);
        }
    }

    /// Whether this account receives connections: with a pairing, or while its pairing mode is open (ACCOUNTS.md 6).
    /// Its endpoint listens and its record is announced exactly then.
    pub fn wants_to_receive(&self) -> bool {
        self.mode.is_open() || self.p.trust.read(|s| !s.devices.is_empty())
    }

    /// Applies [`Hub::wants_to_receive`] to the core and the announcement. Called when the core is bound, after a
    /// pairing, a forget, and when pairing mode opens or closes.
    pub fn update_receiving(&self) {
        let on = self.wants_to_receive();
        if let Some(core) = self.core() {
            core.set_receiving(on);
        }
        self.p.presence.set_announced(on);
    }

    pub fn log(&self, level: Level, what: core::fmt::Arguments<'_>) {
        self.p.trace.log(level, "ui-hub", what);
    }

    pub fn core(&self) -> Option<Arc<ServiceCore>> {
        lock(&self.core).upgrade()
    }

    pub fn rt(&self) -> Option<&Handle> {
        self.rt.get()
    }

    pub fn allow_loopback(&self) -> bool {
        self.p.dev.is_some()
    }

    pub fn settings(&self) -> MachineSettings {
        lock(&self.settings).clone()
    }

    pub fn fresh_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn conns(&self) -> Vec<Arc<Conn>> {
        lock(&self.conns).clone()
    }

    /// The connections of the console session (the host's user: banner, prompts).
    pub fn console_conns(&self) -> Vec<Arc<Conn>> {
        let Some(session) = *lock(&self.console) else { return Vec::new() };
        self.conns().into_iter().filter(|c| c.info.session_id == session).collect()
    }

    /// To every connection.
    pub fn notify(&self, n: Notify) {
        for c in self.conns() {
            c.queue.control(Ipc::UiNotify(n.clone()));
        }
    }

    /// To the console session's connections.
    pub fn notify_console(&self, n: Notify) {
        for c in self.console_conns() {
            c.queue.control(Ipc::UiNotify(n.clone()));
        }
    }

    /// `DevicesChanged` with the list as it is now.
    pub fn devices_changed(&self) {
        let list = self.device_views();
        self.notify(Notify::DevicesChanged(list));
    }

    /// A session went live in either role: apply "keep awake", and the device list shows the peer on the network
    /// (a device with a live session is reachable whatever DNS-SD says).
    pub fn session_began(&self) {
        let n = self.live_sessions.fetch_add(1, Ordering::SeqCst) + 1;
        self.p.hooks.keep_awake(n > 0 && self.settings().keep_awake);
        self.devices_changed();
    }

    /// A session ended: apply "keep awake".
    pub fn session_ended(&self) {
        let before = self
            .live_sessions
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(1)))
            .unwrap_or(0);
        self.p.hooks.keep_awake(before > 1 && self.settings().keep_awake);
    }

    pub fn api_error(&self, code: ApiCode, what: &str) -> ApiCode {
        self.log(Level::Debug, format_args!("refused: {what} ({code:?})"));
        code
    }
}

/// The UI hub of the service. Cheap to clone.
#[derive(Clone)]
pub struct UiHub {
    hub: Arc<Hub>,
}

impl UiHub {
    pub fn new(params: HubParams) -> Self {
        let params_secrets = Arc::clone(&params.secrets);
        let settings = params.settings.clone();
        let mode = Arc::new(PairingMode::new(Arc::clone(&params.clock)));
        let hub = Arc::new(Hub {
            p: params,
            core: Mutex::new(Weak::new()),
            rt: OnceLock::new(),
            account: OnceLock::new(),
            console_user: Mutex::new(None),
            settings: Mutex::new(settings),
            conns: Mutex::new(Vec::new()),
            next_conn: AtomicU64::new(1),
            console: Mutex::new(None),
            desktop: Mutex::new(Desktop::Default),
            mode,
            pairing: pairing::PairState::default(),
            incoming: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            manual: Mutex::new(Vec::new()),
            learned: Mutex::new(HashMap::new()),
            minors: Mutex::new(peer_minor::PeerMinors::default()),
            peer_writes: Mutex::new(grants_sync::PeerWrites::default()),
            sync: Mutex::new(sync_dial::SyncState::default()),
            peer_ports: Mutex::new((store::load_peer_ports(&*params_secrets), HashMap::new())),
            live_sessions: AtomicUsize::new(0),
            swaps: Mutex::new(HashMap::new()),
            entries: Mutex::new(HashMap::new()),
            files: Mutex::new(files::FilesTable::default()),
            wake: Mutex::new(WakeSetup::default()),
            elevation: Mutex::new(elevation::Elevation::default()),
            started: Mutex::new(HashMap::new()),
            shutting: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&hub);
        hub.p.presence.set_listener(Arc::new(move || {
            if let Some(h) = weak.upgrade() {
                h.devices_changed();
                h.sync_presence_changed();
            }
        }));
        Self { hub }
    }

    /// The `HostUi` for `ServiceConfig::ui`.
    pub fn host_ui(&self) -> Arc<dyn HostUi> {
        Arc::new(HubHostUi::new(Arc::clone(&self.hub)))
    }

    /// The audit sink the pairing code and the host events write to (the same log `ActivityList` reads).
    pub fn audit_sink(&self) -> Arc<dyn swapdesk_trust::AuditSink> {
        Arc::new(views::HubAudit { hub: Arc::downgrade(&self.hub) })
    }

    /// The account this hub serves (its user's SID), on a machine of several accounts: a connection of that user from
    /// a session that is not the console's may still narrow the account's reach (ACCOUNTS.md 5.6).
    pub fn set_account(&self, sid: &str) {
        let _ = self.hub.account.set(sid.to_string());
    }

    /// Hands the started core to the hub (weakly) and takes its runtime. Call it once, right after
    /// `ServiceCore::start`.
    pub fn bind_core(&self, core: &Arc<ServiceCore>) {
        *lock(&self.hub.core) = Arc::downgrade(core);
        let _ = self.hub.rt.set(core.runtime().clone());
        // Swap questions, the new session of a swap and chat reach the UI through the hub.
        core.set_extras(Arc::new(HubExtras { hub: Arc::downgrade(&self.hub) }));
        // The viewer role's clipboard, the clipboard notices and the audit of quick actions.
        core.set_clip_actions(Arc::new(clip_actions::HubClipActions { hub: Arc::downgrade(&self.hub) }));
        // The core is up: what is on the network now is synced (GRANTS-SYNC.md 4.4).
        self.hub.sync_presence_changed();
        self.hub.update_receiving();
    }

    /// Wake-on-LAN: how the socket of an attempt is made and how long it runs. The default is a UDP socket bound to
    /// the route with TTL 1 and the 60 s of F-WAKE-01; tests give a recording socket and a short window.
    pub fn set_wake(&self, binder: extras::WakeBinder, cfg: swapdesk_extras::wake::WakeConfig) {
        *lock(&self.hub.wake) = WakeSetup { binder, cfg };
    }

    /// The machine settings as they are now.
    pub fn settings(&self) -> MachineSettings {
        self.hub.settings()
    }

    /// The session whose UI is the host's user (the console session); `None` when nobody is at the machine.
    /// Prompts and the host banner go to that session's UI only.
    pub fn set_console_session(&self, session: Option<u32>) {
        let changed = std::mem::replace(&mut *lock(&self.hub.console), session) != session;
        if changed {
            // A UI that connected before the console was known gets what is waiting for it.
            for c in self.hub.console_conns() {
                self.hub.send_pending(&c);
            }
        }
    }

    /// The host's desktop state (a lock or a UAC prompt is not "somebody present", PROTOCOL.md 6.2).
    /// The desktop of the console changed: kept for the interface, and told to this account's sessions, which see
    /// `AccountAway` instead while another account owns the console (ACCOUNTS.md 5.1).
    pub fn set_desktop(&self, desktop: Desktop) {
        *lock(&self.hub.desktop) = desktop;
        self.hub.push_desktop();
    }

    /// Whether this hub serves the machine's only account (no account SID was set: a machine of one account, the Mac).
    pub fn single_account(&self) -> bool {
        self.hub.account.get().is_none()
    }

    /// The user at the console changed (`None`: the sign-in screen). Sessions of this account pause while another
    /// account owns the console and end after a minute; they go on when it comes back.
    pub fn set_console_user(&self, sid: Option<String>) {
        *lock(&self.hub.console_user) = sid;
        self.hub.push_desktop();
    }

    /// The network changed: the session endpoint follows the approved addresses, the presence re-reads the
    /// interfaces, and the UIs get the new status and list.
    pub fn network_changed(&self) {
        if let Some(core) = self.hub.core() {
            core.network_changed();
        }
        self.hub.p.presence.refresh();
        self.hub.refresh_pair_nets();
        let status = self.hub.network_status();
        self.hub.notify(Notify::NetworkChanged(status));
        self.hub.devices_changed();
    }

    /// The addresses the pairing endpoints listen on right now (empty while pairing mode is closed).
    pub fn pair_local_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.hub.pair_local_addrs()
    }

    /// Connected UIs.
    pub fn connections(&self) -> usize {
        lock(&self.hub.conns).len()
    }

    /// Ends the pairing mode and refuses new work; running connections end when their pipe does.
    pub fn shutdown(&self) {
        self.hub.shutting.store(true, Ordering::SeqCst);
        self.hub.cancel_pairing();
        for c in self.hub.conns() {
            c.queue.control(Ipc::Bye { code: swapdesk_proto::ErrorCode::HostShutdown });
            c.queue.finish();
        }
    }

    /// Serves one UI connection until it ends. The caller did the server handshake (`Handshake::server`, with
    /// `Hello.role` checked to be the UI) and the pipe checks of C2.1 and C2.3 (`info`). Blocks the calling thread:
    /// give each connection its own.
    pub fn serve<R, W>(&self, reader: FrameReader<R>, writer: FrameWriter<W>, info: ConnInfo) -> Result<(), IpcError>
    where
        R: std::io::Read,
        W: std::io::Write + Send + 'static,
    {
        self.serve_with(reader, writer, info, None)
    }

    /// [`serve`](Self::serve) for a pipe the owner can cancel: when the UI stops reading its messages and the connection
    /// is cut (see `queue.rs`), `on_cut` runs and must cancel the pipe's pending I/O. The blocked read then returns,
    /// `serve` ends and the owner drops the pipe, so the UI sees its end instead of silence.
    pub fn serve_cancellable<R, W>(
        &self,
        reader: FrameReader<R>,
        writer: FrameWriter<W>,
        info: ConnInfo,
        on_cut: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), IpcError>
    where
        R: std::io::Read,
        W: std::io::Write + Send + 'static,
    {
        self.serve_with(reader, writer, info, Some(on_cut))
    }

    fn serve_with<R, W>(
        &self,
        mut reader: FrameReader<R>,
        writer: FrameWriter<W>,
        info: ConnInfo,
        on_cut: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<(), IpcError>
    where
        R: std::io::Read,
        W: std::io::Write + Send + 'static,
    {
        let hub = &self.hub;
        let queue = UiQueue::new();
        if let Some(on_cut) = on_cut {
            queue.set_on_cut(on_cut);
        }
        let writer_thread = queue.spawn_writer(writer)?;
        let id = hub.next_conn.fetch_add(1, Ordering::Relaxed);
        let clip = {
            let q = queue.clone();
            crate::rpc::Rpc::new(move |m| q.control(m))
        };
        let to_ui = queue.clone();
        let fs = Arc::new(PipeCaller::new(id, Arc::new(Pending::new()), move |msg| to_ui.control(msg)));
        let conn = Arc::new(Conn {
            id,
            info,
            queue: queue.clone(),
            clip,
            viewers: Mutex::new(HashMap::new()),
            terms: Mutex::new(HashMap::new()),
            fs,
            passes: Mutex::new(Vec::new()),
        });
        lock(&hub.conns).push(Arc::clone(&conn));
        hub.log(
            Level::Info,
            format_args!("UI connected (session {}, admin {})", conn.info.session_id, conn.info.admin),
        );
        hub.send_pending(&conn);
        let end = loop {
            if hub.shutting.load(Ordering::SeqCst) {
                break Ok(());
            }
            // The queue closed under us: the pipe broke, or the UI stopped reading and was cut (see `queue.rs`). The
            // next request it sends finds the connection over.
            if queue.is_closed() {
                break Ok(());
            }
            match reader.read_message() {
                Ok(Some(Ipc::Bye { .. })) | Ok(None) => break Ok(()),
                Ok(Some(msg)) => {
                    if let Err(e) = hub.dispatch(&conn, msg) {
                        queue.control(Ipc::Bye { code: swapdesk_proto::ErrorCode::ProtocolViolation });
                        break Err(e);
                    }
                }
                Err(e) => break Err(e),
            }
        };
        lock(&hub.conns).retain(|c| c.id != conn.id);
        // Its open ticket and the passes it still held die with it.
        hub.drop_elevation_of(&conn);
        // Whoever waits for this UI's clipboard stops waiting.
        conn.clip.close();
        // The calls waiting for this UI's disk fail now (its read handles died with it).
        conn.fs.pending().close();
        // The UI is gone: its connect attempts stop and its viewer sessions go with it (nobody is left to decode them).
        hub.abort_entries_of(&conn);
        let viewers: Vec<Viewer> = lock(&conn.viewers).drain().map(|(_, v)| v).collect();
        for v in &viewers {
            v.handle.disconnect();
        }
        drop(viewers);
        queue.finish();
        let _ = writer_thread.join();
        queue.close();
        hub.log(
            Level::Info,
            format_args!(
                "UI disconnected (session {}, {} realtime messages dropped{})",
                conn.info.session_id,
                queue.dropped(),
                if queue.cut() > 0 { ", cut: it stopped reading its messages" } else { "" }
            ),
        );
        end
    }
}
