//! The requests of a UI connection and the relay of its viewer sessions.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use swapdesk_ipc::{
    ApiCode, Bytes, ConnectWhy, DisplayView, ElevAction, IpcError, Message as Ipc, Notify, ReplyBody, STATUS_OK,
    SessionState, SettingsUpdate, SpecialKeys, UiOp,
};
use swapdesk_lan::guard::{ManualError, validate_manual};
use swapdesk_platform::Level;
use swapdesk_proto::{
    DisplayList, ErrorCode, InputEvent, Intent, KeyframeRequest, Os, PermissionSet, Special, VideoSettings,
};
use swapdesk_trust::{AuditEntry, AuditEvent, DeviceId, DeviceRef, Direction, device_id_hex};
use tokio::time::Instant;

use super::extras::{ENTRY_ID_BIT, Entry};
use super::store::{store_settings, valid_name};
use super::views::{Manual, parse_device_id};
use super::{Conn, Hub, Viewer, lock};
use crate::service::{
    ApplyError, ConnectError, DEFAULT_SESSION_PORT, TermOut, ViewerHandle, ViewerMessages, ViewerOut, WaitHook,
};

/// Typed addresses kept at once; the oldest goes first.
const MAX_MANUAL: usize = 16;
/// HID usage page of the keyboard.
const KEYBOARD_PAGE: u16 = 0x07;

type OpResult = Result<ReplyBody, ApiCode>;

/// The `UiReply` that answers request `req` with `result`.
pub(super) fn reply_message(req: u32, result: OpResult) -> Ipc {
    let (status, body) = match result {
        Ok(b) => match b.encode() {
            Ok(bytes) => (STATUS_OK, bytes),
            Err(_) => (ApiCode::Internal.status(), Vec::new()),
        },
        Err(code) => (code.status(), Vec::new()),
    };
    Ipc::UiReply { req, status, body: Bytes(body) }
}

pub(super) fn reply(conn: &Conn, req: u32, result: OpResult) {
    conn.queue.control(reply_message(req, result));
}

fn done(r: Result<(), ApiCode>) -> OpResult {
    r.map(|()| ReplyBody::Done)
}

fn apply_code(e: ApplyError) -> ApiCode {
    match e {
        ApplyError::NoSuchSession | ApplyError::Closed => ApiCode::NotFound,
        ApplyError::NotPermitted => ApiCode::NotPermitted,
        // The queue is full: the batch was dropped and the session releases every key behind it.
        ApplyError::Full => ApiCode::Busy,
        ApplyError::NotAViewerRequest => ApiCode::Invalid,
    }
}

/// What the interface is told when a connect did not open: who refused, or what did not answer.
fn connect_why(e: &ConnectError) -> ConnectWhy {
    use swapdesk_session::OpenError;
    match e {
        // The LAN guard turned the address away: nothing to reach.
        ConnectError::Refused(_) => ConnectWhy::Unreachable,
        ConnectError::NotIpv4 | ConnectError::UnknownDevice | ConnectError::Socket(_) => ConnectWhy::Failed,
        ConnectError::Open(OpenError::Refused(c) | OpenError::Closed(c)) => match c {
            ErrorCode::Declined => ConnectWhy::Declined,
            ErrorCode::Timeout => ConnectWhy::TimedOut,
            ErrorCode::NobodyPresent => ConnectWhy::NobodyPresent,
            ErrorCode::Busy => ConnectWhy::Busy,
            ErrorCode::UnknownDevice | ErrorCode::Forgotten => ConnectWhy::Forgotten,
            ErrorCode::UnsupportedVersion => ConnectWhy::Failed,
            _ => ConnectWhy::NotPermitted,
        },
        ConnectError::Open(OpenError::Protocol(_)) => ConnectWhy::Failed,
        ConnectError::Open(OpenError::Timeout | OpenError::Lost) => ConnectWhy::Unreachable,
    }
}

impl Hub {
    /// One message from a UI connection. An `Err` is a protocol error: the connection closes.
    pub fn dispatch(self: &Arc<Self>, conn: &Arc<Conn>, msg: Ipc) -> Result<(), IpcError> {
        match msg {
            Ipc::UiRequest { req, op } => {
                self.request(conn, req, op);
                Ok(())
            }
            // The UI's own sessions only: a session id is not a capability on another connection.
            Ipc::ViewerInput { session_id, events } => {
                if let Some(v) = lock(&conn.viewers).get(&session_id)
                    && !v.view_only
                {
                    let _ = v.handle.send_input(events.0);
                }
                Ok(())
            }
            Ipc::ViewerKeyframe { session_id, request } => {
                if let Some(v) = lock(&conn.viewers).get(&session_id) {
                    let _ = v.handle.request_keyframe(request);
                }
                Ok(())
            }
            Ipc::ViewerReport { session_id, report } => {
                if let Some(v) = lock(&conn.viewers).get(&session_id) {
                    let _ = v.handle.report(report);
                }
                Ok(())
            }
            // The answer to a clipboard call the service made on this UI's pipe.
            reply @ Ipc::ClipReply { .. } => {
                conn.clip.resolve(reply);
                Ok(())
            }
            // The UI's own terminals only, like the sessions above.
            Ipc::TermInput { term_id, bytes } => {
                self.term_input(conn, term_id, bytes.0);
                Ok(())
            }
            // The answer to a disk call this service made to the UI (C2.4 "Files"): it goes to the call that waits.
            Ipc::FsReply { req, result } => {
                conn.fs.pending().complete(req, result);
                Ok(())
            }
            other => Err(IpcError::UnknownType(other.type_code())),
        }
    }

    fn request(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, op: UiOp) {
        // The ops that answer when their work is done (or at once, with the work going on) reply themselves.
        match op {
            UiOp::RequestSwap { session_id } => self.request_swap(conn, req, session_id),
            UiOp::ChatSend { session_id, text } => self.chat_send(conn, req, session_id, text.0),
            UiOp::ChatMarkRead { session_id } => self.chat_read(conn, req, session_id),
            UiOp::Wake { device_id, intent } => self.wake(conn, req, &device_id, intent),
            UiOp::RunAction { session_id, action } => self.run_action(conn, req, session_id, action),
            // The open answers when the shell runs or was refused, which can wait for the host's user.
            UiOp::TermOpen { session_id, cols, rows, admin } => {
                self.term_open(conn, req, session_id, cols, rows, admin)
            }
            UiOp::Files(op) => self.files_op(conn, req, op),
            op => self.request_other(conn, req, op),
        }
    }

    fn request_other(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, op: UiOp) {
        // `unattended` of the op is not read: whether the viewer asks for entry without a prompt follows what it
        // knows of the host and of the account that connects (`open_viewer`), and the host decides the rest.
        if let UiOp::Connect { device_id, intent, .. } = op {
            if let Err(code) = self.require_console(conn, "a connect from a UI that is not the console's") {
                return reply(conn, req, Err(code));
            }
            // Answers with the entry at once; the attempt (up to the host's 20 s) goes on as a task.
            self.connect(conn, req, &device_id, intent);
            return;
        }
        reply(conn, req, self.op(conn, op));
    }

    pub(super) fn is_console(&self, conn: &Conn) -> bool {
        self.console_conns().iter().any(|c| c.id == conn.id)
    }

    /// The ops that change who may reach this machine, or make it reach out (pairing, grants, forgetting a device,
    /// `Connect`, a swap that would make it the host) belong to the person at the console: a UI of another session
    /// (a user parked by fast user switching) gets `NotPermitted`.
    pub(super) fn require_console(&self, conn: &Conn, what: &str) -> Result<(), ApiCode> {
        if self.is_console(conn) { Ok(()) } else { Err(self.api_error(ApiCode::NotPermitted, what)) }
    }

    fn op(self: &Arc<Self>, conn: &Arc<Conn>, op: UiOp) -> OpResult {
        // Before any argument is looked at, so a refused UI learns nothing about the device list. What only narrows this
        // account's own reach (forgetting a device, taking permissions away) is open to any interactive session of the
        // account's own user, which is every connection that reaches this hub (ACCOUNTS.md 5.6): a user switched away
        // from the console can still cut a device off.
        let own_user = self.account.get().is_some_and(|a| a.eq_ignore_ascii_case(&conn.info.user.id));
        let narrows = own_user
            && match &op {
                UiOp::ForgetDevice { .. } => true,
                UiOp::SetGrants { device_id, grants } => parse_device_id(device_id).is_some_and(|id| {
                    self.p.trust.read(|s| s.find_by_id(&id).is_some_and(|d| grants & !d.grants_here.bits() == 0))
                }),
                _ => false,
            };
        if !narrows
            && matches!(
                op,
                UiOp::PairOpen { .. }
                    | UiOp::PairStart { .. }
                    | UiOp::PairConfirm { .. }
                    | UiOp::PairCancel { .. }
                    | UiOp::SetGrants { .. }
                    | UiOp::ForgetDevice { .. }
                    | UiOp::RenameDevice { .. }
            )
        {
            self.require_console(
                conn,
                "a pairing, grants, rename or forget request from a UI that is not the console's",
            )?;
        }
        match op {
            UiOp::GetThisDevice => Ok(ReplyBody::ThisDevice(self.this_device())),
            UiOp::GetNetwork => Ok(ReplyBody::Network(self.network_status())),
            UiOp::ListDevices => Ok(ReplyBody::Devices(self.device_views())),
            UiOp::AddByAddress { ipv4 } => self.add_by_address(&ipv4).map(ReplyBody::Device),
            UiOp::PairOpen { device_id } => self.pair_open(conn, device_id).map(ReplyBody::PairingId),
            UiOp::PairStart { pairing_id, device_id } => done(self.pair_start(conn, pairing_id, &device_id)),
            UiOp::PairConfirm { pairing_id, matches } => done(self.pair_confirm(conn, pairing_id, matches)),
            UiOp::PairCancel { pairing_id } => done(self.pair_cancel(conn, pairing_id)),
            UiOp::SetGrants { device_id, grants } => done(self.set_grants(conn, &device_id, grants)),
            UiOp::ForgetDevice { device_id } => done(self.forget_device(&device_id)),
            UiOp::RenameDevice { device_id, name } => done(self.rename_device(&device_id, &name)),
            UiOp::RefreshGrants { device_id } => {
                let id = parse_device_id(&device_id).ok_or(ApiCode::Invalid)?;
                done(self.refresh_grants(id))
            }
            UiOp::Connect { .. } => Err(ApiCode::Internal),
            UiOp::Disconnect { session_id } => done(self.disconnect(conn, session_id)),
            UiOp::AnswerIncoming { request_id, decision, grants } => {
                // Validated before use, whatever the decision (PROTOCOL.md 6.1).
                let grants = PermissionSet::from_bits(grants).ok_or(ApiCode::Invalid)?;
                if !self.is_console(conn) {
                    return Err(self.api_error(ApiCode::NotPermitted, "answer from a UI that is not the console's"));
                }
                done(self.answer_incoming(request_id, decision, grants))
            }
            UiOp::SelectDisplay { session_id, display } => {
                done(self.with_viewer(conn, session_id, |v| v.handle.select_display(display).map_err(apply_code)))
            }
            UiOp::ListDisplays { session_id } => {
                self.with_viewer(conn, session_id, |v| Ok(v.displays.clone())).map(ReplyBody::Displays)
            }
            UiOp::SetQuality { session_id, quality } => {
                let settings =
                    VideoSettings { preset: quality, max_fps: 60, max_kbps: 0, view_width: 0, view_height: 0 };
                done(self.with_viewer(conn, session_id, |v| v.handle.set_video_settings(settings).map_err(apply_code)))
            }
            UiOp::SetAudio { session_id, on } => {
                done(self.with_viewer(conn, session_id, |v| v.handle.set_audio(on).map_err(apply_code)))
            }
            UiOp::SetViewOnly { session_id, on } => done(self.set_view_only(conn, session_id, on)),
            UiOp::SendKeys { session_id, keys } => done(self.send_keys(conn, session_id, keys)),
            UiOp::GetSettings => Ok(ReplyBody::Settings(self.settings_view())),
            UiOp::SetSettings(update) => done(self.set_settings(&update)),
            UiOp::ActivityList { before_ms, limit } => {
                Ok(ReplyBody::Activity(self.activity(before_ms, usize::try_from(limit).unwrap_or(usize::MAX))))
            }
            UiOp::AnswerSwap { request_id, accept } => done(self.answer_swap(conn, request_id, accept)),
            UiOp::SetClipboardSync { session_id, on } => done(self.set_clipboard_sync(conn, session_id, on)),
            UiOp::TermResize { term_id, cols, rows } => done(self.term_resize(conn, term_id, cols, rows)),
            UiOp::TermClose { term_id } => done(self.term_close(conn, term_id)),
            // Answered by `request`, which never sends them here.
            UiOp::RequestSwap { .. }
            | UiOp::ChatSend { .. }
            | UiOp::ChatMarkRead { .. }
            | UiOp::Wake { .. }
            | UiOp::RunAction { .. }
            | UiOp::TermOpen { .. }
            | UiOp::Files(_) => Err(ApiCode::Internal),
            UiOp::ElevationBegin { action } => self.elevation_begin(conn, action),
            UiOp::ElevationEnd { ticket } => Ok(ReplyBody::ElevationOutcome(self.elevation_end(conn, &ticket))),
            // Only the restricted loop of an elevated instance (`serve_elevator`) answers these two: a UI
            // connection is never the approver of its own ticket.
            UiOp::ElevationDescribe { .. } | UiOp::ElevationConfirm { .. } => {
                Err(self.api_error(ApiCode::NotPermitted, "an elevation approval on a UI connection"))
            }
        }
    }

    fn with_viewer<T>(
        &self,
        conn: &Conn,
        session_id: u64,
        f: impl FnOnce(&mut Viewer) -> Result<T, ApiCode>,
    ) -> Result<T, ApiCode> {
        let mut viewers = lock(&conn.viewers);
        f(viewers.get_mut(&session_id).ok_or(ApiCode::NotFound)?)
    }

    // ---------- devices ----------

    /// F-FIND-03: an IPv4 literal on-link; off-link, loopback and this machine's own addresses are refused.
    /// `a.b.c.d:port` names a Swapdesk that listens on another session port (its pairing port is the next one); in
    /// development mode that can also be another instance on this PC.
    fn add_by_address(&self, text: &str) -> Result<swapdesk_ipc::DeviceView, ApiCode> {
        let text = text.trim();
        let (ip_text, port) = match text.split_once(':') {
            // Any port of 1 to 65535: an account on a slot other than 0 (ACCOUNTS.md 5.2).
            Some((ip, port)) => (ip, Some(port.parse::<u16>().ok().filter(|p| *p != 0).ok_or(ApiCode::Invalid)?)),
            None => (text, None),
        };
        let ip: Ipv4Addr = ip_text.parse().map_err(|_| ApiCode::Invalid)?;
        let session_port = port.unwrap_or(DEFAULT_SESSION_PORT);
        if self.allow_loopback() && ip.is_loopback() {
            // Development only. An instance still never pairs with its own endpoint.
            if self.core().is_some_and(|c| c.local_addrs().contains(&SocketAddr::from((ip, session_port)))) {
                return Err(ApiCode::Refused);
            }
        } else {
            let list = self.p.interfaces.list().map_err(|_| ApiCode::Internal)?;
            validate_manual(&list, ip_text).map_err(|e| match e {
                ManualError::Syntax => ApiCode::Invalid,
                ManualError::Refused(_) => ApiCode::Refused,
            })?;
        }
        // An address that is a device seen on the network is that device.
        if let Some(p) =
            self.p.presence.peers().iter().find(|p| p.addrs.contains(&ip) && p.session_port == session_port)
        {
            let id = device_id_hex(&p.device_id);
            return self.device_views().into_iter().find(|d| d.id == id).ok_or(ApiCode::NotFound);
        }
        let m = Manual { addr: ip, session_port };
        let view = self.manual_view(&m);
        {
            let mut manual = lock(&self.manual);
            manual.retain(|x| x != &m);
            manual.push(m);
            if manual.len() > MAX_MANUAL {
                manual.remove(0);
            }
        }
        self.devices_changed();
        Ok(view)
    }

    /// `SetGrants`: `grants` validated, `UNATTENDED` and `ADMIN_TERMINAL` only added from an administrator
    /// connection (C2.3, D7), and so is any bit added while the device has `UNATTENDED` (it would widen entry without
    /// a prompt); taking bits away is open to any console connection (it narrows). A connection that is not elevated
    /// may make that one change after an administrator confirmed exactly it (a pass, `elevation.rs`).
    fn set_grants(self: &Arc<Self>, conn: &Conn, device_id: &str, bits: u16) -> Result<(), ApiCode> {
        let new = PermissionSet::from_bits(bits).ok_or(ApiCode::Invalid)?;
        let id = parse_device_id(device_id).ok_or(ApiCode::Invalid)?;
        let old = self.p.trust.read(|s| s.find_by_id(&id).map(|d| d.grants_here)).ok_or(ApiCode::NotFound)?;
        let admin_only = PermissionSet::BIT_UNATTENDED | PermissionSet::BIT_ADMIN_TERMINAL;
        let added = new.bits() & !old.bits();
        // Adding UNATTENDED or ADMIN_TERMINAL, or adding anything to a device that holds (or is given) UNATTENDED:
        // the added bits are then entry without a prompt.
        let unattended_involved = (old.bits() | new.bits()) & PermissionSet::BIT_UNATTENDED != 0;
        // Short-circuit order matters: a pass is looked at, and used up, only for a change that needs one.
        if (added & admin_only != 0 || (unattended_involved && added != 0))
            && !conn.info.admin
            && !self.take_pass(conn, &ElevAction::SetGrants { device_id: device_id_hex(&id), grants: new.bits() })
        {
            return Err(self.api_error(
                ApiCode::NeedsAdmin,
                "widening unattended access (or granting ADMIN_TERMINAL) from a connection that is not elevated",
            ));
        }
        let user = conn.info.user.clone();
        self.p
            .trust
            .update(|s| {
                if let Some(d) = s.devices.iter_mut().find(|d| d.device_id == id) {
                    d.grants_here = new;
                    // D13: who granted the entry without a prompt.
                    if !new.contains(PermissionSet::UNATTENDED) {
                        d.unattended_granted_by = None;
                    } else if !old.contains(PermissionSet::UNATTENDED) {
                        d.unattended_granted_by = Some(user.clone());
                    }
                }
            })
            .map_err(|e| {
                self.log(Level::Warn, format_args!("the trust store was not written: {e}"));
                ApiCode::Internal
            })?;
        self.p.audit.append(AuditEntry {
            time_ms: self.p.wall.unix_ms(),
            device: DeviceRef::Known(id),
            event: AuditEvent::GrantsChanged { from: old, to: new, by: user },
            duration_s: None,
        });
        if let Some(core) = self.core() {
            core.host().stored_grants_changed(&id);
        }
        // Outside a session the device hears it from a grants sync, 1 s after the last change (GRANTS-SYNC.md 4.4).
        self.sync_after_change(id);
        // This machine's viewer sessions with the device tell it too (GRANTS-SYNC.md 3.2).
        for conn in lock(&self.conns).iter() {
            for v in lock(&conn.viewers).values().filter(|v| v.device == id) {
                v.handle.ctx().set_own_stored(new);
            }
        }
        self.devices_changed();
        Ok(())
    }

    /// F-PAIR-04: the key and grants go, the device's sessions end, its tokens die.
    fn forget_device(&self, device_id: &str) -> Result<(), ApiCode> {
        let id = parse_device_id(device_id).ok_or(ApiCode::Invalid)?;
        let removed = self.p.trust.update(|s| s.forget(&id)).map_err(|_| ApiCode::Internal)?;
        if !removed {
            return Err(ApiCode::NotFound);
        }
        self.p.audit.append(AuditEntry {
            time_ms: self.p.wall.unix_ms(),
            device: DeviceRef::Known(id),
            event: AuditEvent::Forgotten,
            duration_s: None,
        });
        if let Some(core) = self.core() {
            core.host().forget_device(&id);
        }
        // Sessions this machine runs as a viewer to that device end too.
        for c in self.conns_of_device(&id) {
            for v in lock(&c.viewers).values().filter(|v| v.device == id) {
                v.handle.disconnect();
            }
        }
        lock(&self.learned).remove(&id);
        lock(&self.minors).forget(&id);
        lock(&self.peer_writes).forget(&id);
        self.forget_peer_port(&id);
        // The last pairing gone: this account stops listening and announcing.
        self.update_receiving();
        self.devices_changed();
        Ok(())
    }

    /// The local alias of a paired device: only `PairedDevice.name` of the trust record changes (the peer is not told
    /// and nothing else of the record moves). The name is trimmed; it must not be empty, longer than the 64 bytes the
    /// record stores (which also bounds it at 64 characters), or hold a control character.
    fn rename_device(&self, device_id: &str, name: &str) -> Result<(), ApiCode> {
        let id = parse_device_id(device_id).ok_or(ApiCode::Invalid)?;
        let name = name.trim();
        if name.is_empty() || name.len() > swapdesk_ipc::MAX_NAME || name.chars().any(char::is_control) {
            return Err(ApiCode::Invalid);
        }
        let renamed = self
            .p
            .trust
            .update(|s| match s.devices.iter_mut().find(|d| d.device_id == id) {
                Some(d) => {
                    d.name = name.to_owned();
                    true
                }
                None => false,
            })
            .map_err(|e| {
                self.log(Level::Warn, format_args!("the trust store was not written: {e}"));
                ApiCode::Internal
            })?;
        if !renamed {
            return Err(ApiCode::NotFound);
        }
        self.devices_changed();
        Ok(())
    }

    pub(super) fn conns_of_device(&self, id: &DeviceId) -> Vec<Arc<Conn>> {
        self.conns().into_iter().filter(|c| lock(&c.viewers).values().any(|v| &v.device == id)).collect()
    }

    // ---------- sessions ----------

    /// `Disconnect`: a viewer session of this connection, or (console UI only) a session of this machine as host.
    fn disconnect(&self, conn: &Conn, session_id: u64) -> Result<(), ApiCode> {
        // An entry (a waking or connecting card) is not a session: cancelling it stops the attempt.
        if self.cancel_entry(conn, session_id) {
            return Ok(());
        }
        if let Some(v) = lock(&conn.viewers).get(&session_id) {
            v.handle.disconnect();
            return Ok(());
        }
        if !self.is_console(conn) {
            return Err(ApiCode::NotFound);
        }
        match self.core() {
            Some(core) if core.host().disconnect(session_id) => Ok(()),
            _ => Err(ApiCode::NotFound),
        }
    }

    /// `SetViewOnly`: on a viewer session the UI stops forwarding input (nothing is told to the host); on a hosted
    /// session (console UI) the grants lose or regain `CONTROL`.
    fn set_view_only(&self, conn: &Conn, session_id: u64, on: bool) -> Result<(), ApiCode> {
        if let Some(v) = lock(&conn.viewers).get_mut(&session_id) {
            v.view_only = on;
            return Ok(());
        }
        if !self.is_console(conn) {
            return Err(ApiCode::NotFound);
        }
        let core = self.core().ok_or(ApiCode::ServiceDown)?;
        let ctx = core.host().session(session_id).ok_or(ApiCode::NotFound)?;
        let bits = ctx.granted().bits();
        let bits = if on { bits & !PermissionSet::BIT_CONTROL } else { bits | PermissionSet::BIT_CONTROL };
        if core.host().set_session_grants(session_id, PermissionSet::closure(bits)) {
            Ok(())
        } else {
            Err(ApiCode::NotFound)
        }
    }

    /// `SendKeys`: Ctrl+Alt+Del is the secure attention sequence and only an `UNATTENDED` session may send it (the
    /// host's service sends it); the other keys are ordinary input and need `CONTROL`.
    fn send_keys(&self, conn: &Conn, session_id: u64, keys: SpecialKeys) -> Result<(), ApiCode> {
        let viewers = lock(&conn.viewers);
        let v = viewers.get(&session_id).ok_or(ApiCode::NotFound)?;
        let granted = v.handle.granted();
        if keys == SpecialKeys::CtrlAltDel {
            if !granted.contains(PermissionSet::UNATTENDED) {
                return Err(self.api_error(ApiCode::NotPermitted, "SAS without an UNATTENDED session"));
            }
            return v
                .handle
                .ctx()
                .try_send(swapdesk_proto::SendSpecial { key: Special::SecureAttention })
                .map_err(|_| ApiCode::NotFound);
        }
        if v.view_only {
            return Err(ApiCode::NotPermitted);
        }
        let host_is_mac = self.p.trust.read(|s| s.find_by_id(&v.device).map(|d| d.os)) == Some(Os::MacOs);
        let events = key_events(keys, host_is_mac);
        v.handle.send_input(events).map_err(apply_code)
    }

    /// `Connect`: answers with the id of an entry at once, then runs the attempt as a task. The entry reports
    /// `Connecting` and `Prompted` while the host decides; it ends with the session's first `Active` (then the entry's
    /// `Ended`) or with `NotOpened` and why. Only what is wrong with the request itself is a reply's error.
    pub(super) fn connect(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, device_id: &str, intent: Intent) {
        let Some(id) = parse_device_id(device_id) else { return reply(conn, req, Err(ApiCode::Invalid)) };
        // The user acted on the device: a key it refused may be pinned again.
        self.sync_user_acted(&id);
        if self.p.trust.read(|s| s.find_by_id(&id).is_none()) {
            return reply(conn, req, Err(ApiCode::NotFound));
        }
        if self.core().is_none() {
            return reply(conn, req, Err(ApiCode::ServiceDown));
        }
        let Some(rt) = self.rt() else { return reply(conn, req, Err(ApiCode::ServiceDown)) };
        let entry_id = ENTRY_ID_BIT | u64::from(self.fresh_id());
        reply(conn, req, Ok(ReplyBody::SessionId(entry_id)));
        let (me, c) = (Arc::clone(self), Arc::clone(conn));
        // The entry is registered under the lock the task ends with, so a fast end cannot leave a stale one.
        let mut entries = lock(&self.entries);
        let task = rt.spawn(async move { me.run_connect(c, entry_id, id, intent).await });
        entries.insert(entry_id, Entry { abort: task.abort_handle(), conn: conn.id, device_hex: device_id_hex(&id) });
    }

    /// One connect attempt under the entry `entry_id` (a connect's own, or the one of a wake): the session goes to its
    /// relay, which ends the entry after the session's first `Active`; or the entry ends here with why it did not open.
    pub(super) async fn run_connect(self: Arc<Self>, conn: Arc<Conn>, entry_id: u64, id: DeviceId, intent: Intent) {
        let Err(why) = self.open_viewer(&conn, entry_id, id, intent).await else { return };
        // A cancel took the entry first: its end was told.
        if lock(&self.entries).remove(&entry_id).is_some() {
            conn.queue.control(Ipc::UiNotify(Notify::Session {
                session_id: entry_id,
                device_id: device_id_hex(&id),
                state: SessionState::NotOpened { why },
            }));
        }
    }

    /// Opens the viewer session of an entry and starts relaying it.
    async fn open_viewer(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        entry_id: u64,
        id: DeviceId,
        intent: Intent,
    ) -> Result<(), ConnectWhy> {
        let device_hex = device_id_hex(&id);
        let addr = self.address_of(&id).ok_or(ConnectWhy::Unreachable)?;
        let core = self.core().ok_or(ConnectWhy::Failed)?;
        let rt = self.rt().ok_or(ConnectWhy::Failed)?.clone();
        // Entry without a prompt is asked for whenever the host is known to be of minor 1 or newer: such a host
        // lets a device that holds `UNATTENDED` in without a question and without the machine-level bits, and
        // prompts every other. A 1.0 host is never asked: it would let the device in at once with machine-level
        // input, which its own interface never asked for.
        //
        // Only for an administrator account of this machine, for now: the other machine gave its grant to this
        // machine's one key, which every local account dials with, so without this any account here would walk
        // into a host that trusted one person (D10). It goes when each account has its own key (D44). The Mac's app
        // connection says `can_elevate` for every account: there each account already runs its own service with its
        // own key (D31), so the grant was made to that account alone.
        let hint = self.minor_hint(&id);
        let unattended = (conn.info.admin || conn.info.can_elevate) && self.known_minor(&id, hint) >= 1;
        // An unattended entry grants only what was asked for: a viewer that may use the terminal asks for it (D13 then decides
        // at the host), and an administrator connection also asks for the administrator terminal (D10).
        let mut extra = if unattended { PermissionSet::TERMINAL } else { PermissionSet::EMPTY };
        if conn.info.admin {
            extra = extra.union(PermissionSet::ADMIN_TERMINAL);
        }
        // What the attempt reports while the host decides. A state that comes after the entry ended (a cancel, the host's
        // last message in flight) is not sent: the interface has forgotten the entry.
        let waiting: WaitHook = {
            let (hub, queue, hex) = (Arc::downgrade(self), conn.queue.clone(), device_hex.clone());
            Arc::new(move |state| {
                if hub.upgrade().is_some_and(|h| lock(&h.entries).contains_key(&entry_id)) {
                    queue.control(Ipc::UiNotify(Notify::Session {
                        session_id: entry_id,
                        device_id: hex.clone(),
                        state,
                    }));
                }
            })
        };
        let (handle, messages) = core
            .connect_asking_async(SocketAddr::V4(addr), id, intent, unattended, extra, Some(waiting))
            .await
            .map_err(|e| {
                self.log(Level::Info, format_args!("connect to {device_hex}: {e}"));
                connect_why(&e)
            })?;
        drop(core);
        if conn.queue.is_closed() {
            // The UI went away while the host was deciding: nobody is left to decode this session.
            handle.disconnect();
            return Err(ConnectWhy::Failed);
        }
        let session_id = handle.session_id();
        // What the host said it speaks, over the connection its pinned key authenticated.
        let said = handle.ctx().peer_minor();
        if lock(&self.minors).said(id, said, hint) {
            self.log(
                Level::Info,
                format_args!(
                    "{device_hex} answered protocol minor {said} under a record of minor {hint:?}: it counts as {said}"
                ),
            );
        }
        let mode = handle.mode();
        handle.files().bind(conn.fs.clone());
        lock(&conn.viewers)
            .insert(session_id, Viewer { handle: handle.clone(), device: id, view_only: false, displays: Vec::new() });
        let (me, c) = (Arc::clone(self), Arc::clone(conn));
        rt.spawn(async move { me.pump_viewer(c, session_id, id, handle, messages, mode, Some(entry_id)).await });
        Ok(())
    }

    /// Relays one viewer session to its UI: media out, states and displays as notifications, a `Stats` each second.
    /// `entry` is the connect entry that asked for the session: after the session's first `Active` reached the UI, the
    /// entry ends with a normal `Ended`, so the viewer exists in the interface before the card goes. An entry that was
    /// cancelled meanwhile ends the session instead of showing it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn pump_viewer(
        self: Arc<Self>,
        conn: Arc<Conn>,
        session_id: u64,
        device: DeviceId,
        handle: ViewerHandle,
        mut messages: ViewerMessages,
        mode: swapdesk_proto::GrantMode,
        mut entry: Option<u64>,
    ) {
        let device_hex = device_id_hex(&device);
        let mut cancelled = false;
        let started = self.p.wall.unix_ms();
        let (mut frames, mut bytes) = (0u32, 0u64);
        let mut next_tick = Instant::now() + Duration::from_secs(1);
        let mut began = false;
        let mut reconnecting = false;
        let (mut skipping_video, mut last_good) = (false, 0u32);
        let mut end_code = ErrorCode::NoError;
        loop {
            let item = match tokio::time::timeout_at(next_tick, messages.recv()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    let latency_ms = handle.ctx().rtt().map_or(0, |d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
                    let kbps = u32::try_from(bytes * 8 / 1000).unwrap_or(u32::MAX);
                    conn.queue.control(Ipc::UiNotify(Notify::Stats {
                        session_id,
                        latency_ms,
                        fps: u16::try_from(frames).unwrap_or(u16::MAX),
                        kbps,
                    }));
                    (frames, bytes) = (0, 0);
                    next_tick += Duration::from_secs(1);
                    continue;
                }
            };
            match item {
                ViewerOut::Message(m) => match &m {
                    Ipc::ViewerVideo { header, payload, .. } => {
                        let (key, display, id) =
                            (header.keyframe || header.discontinuity, header.display_id, header.frame_id);
                        let len = payload.0.len() as u64;
                        // The UI's pipe is behind: video is dropped, not waited for (a wait would hold back the audio
                        // and the terminal behind it), up to the next keyframe, which is asked for.
                        if skipping_video && !key {
                            let _ = handle.request_keyframe(KeyframeRequest { display, last_good_frame: last_good });
                            continue;
                        }
                        match conn.queue.try_video(m) {
                            None => {
                                skipping_video = false;
                                last_good = id;
                                frames = frames.saturating_add(1);
                                bytes = bytes.saturating_add(len);
                            }
                            Some(_) if conn.queue.is_closed() => break,
                            Some(_) => {
                                skipping_video = true;
                                let _ =
                                    handle.request_keyframe(KeyframeRequest { display, last_good_frame: last_good });
                            }
                        }
                    }
                    // Realtime: a UI that falls behind loses audio and cursor moves.
                    _ => {
                        conn.queue.realtime(m);
                    }
                },
                ViewerOut::PeerGrants(grants) => self.apply_peer_grants(device, grants),
                // The host asks its user: the panel of the feature says so, with the time left (WAITING.md 4, rows
                // 11 and 12).
                ViewerOut::Consent { feature, timeout_s } => {
                    conn.queue.control(Ipc::UiNotify(Notify::ConsentPending {
                        session_id,
                        feature,
                        remaining_s: timeout_s,
                    }));
                }
                ViewerOut::Displays { session_id: sid, list } => {
                    let views = display_views(&list);
                    // Kept for `ListDisplays`, which a UI sends once per session and may send after this arrived.
                    if let Some(v) = lock(&conn.viewers).get_mut(&sid) {
                        v.displays = views.clone();
                    }
                    conn.queue.control(Ipc::UiNotify(Notify::Displays { session_id: sid, list: views }));
                }
                // Terminal bytes wait for room (a slow UI slows the shell); the end follows the last bytes.
                ViewerOut::Term(TermOut::Output { term_id, bytes }) => {
                    if !conn.queue.term(Ipc::TermOutput { term_id, bytes }).await {
                        break;
                    }
                }
                ViewerOut::Term(TermOut::Exited { term_id, code }) => {
                    if !self.term_exited(&conn, term_id, code).await {
                        break;
                    }
                }
                ViewerOut::State(change) => {
                    let ended = matches!(change.state, SessionState::Ended { .. });
                    // The first `Active` ends the connect entry, after the session itself was told to the UI (below).
                    // An entry the UI cancelled (it is gone from the map) wanted no session: this one ends unseen.
                    let mut ends_entry = None;
                    if matches!(change.state, SessionState::Active { .. })
                        && let Some(e) = entry.take()
                    {
                        if lock(&self.entries).remove(&e).is_some() {
                            ends_entry = Some(e);
                        } else {
                            cancelled = true;
                            handle.disconnect();
                            break;
                        }
                    }
                    match change.state {
                        SessionState::Reconnecting { .. } => reconnecting = true,
                        // The connection is back: what was in flight is offered again and continues (F-FILE-01).
                        SessionState::Active { .. } if reconnecting => {
                            reconnecting = false;
                            self.files_reissue(session_id, &handle);
                        }
                        _ => {}
                    }
                    if matches!(change.state, SessionState::Active { .. }) && !began {
                        began = true;
                        let granted = handle.granted();
                        self.p.audit.append(AuditEntry {
                            time_ms: started,
                            device: DeviceRef::Known(device),
                            event: AuditEvent::SessionStarted {
                                direction: Direction::Outgoing,
                                mode,
                                granted,
                                desktop: swapdesk_proto::Desktop::Default,
                            },
                            duration_s: None,
                        });
                        self.session_began();
                    }
                    conn.queue.control(Ipc::UiNotify(Notify::Session {
                        session_id: change.session_id,
                        device_id: device_hex.clone(),
                        state: change.state,
                    }));
                    if let Some(e) = ends_entry {
                        conn.queue.control(Ipc::UiNotify(Notify::Session {
                            session_id: e,
                            device_id: device_hex.clone(),
                            state: SessionState::Ended { reason: None },
                        }));
                    }
                    if ended {
                        end_code = change.end.map_or(ErrorCode::NoError, |e| e.code);
                        break;
                    }
                }
            }
        }
        self.files_session_ended(session_id);
        if lock(&conn.viewers).remove(&session_id).is_some() && !began && !cancelled {
            // Ended without ever going active (the connection broke at once).
            conn.queue.control(Ipc::UiNotify(Notify::Session {
                session_id,
                device_id: device_hex.clone(),
                state: SessionState::Ended { reason: Some(ApiCode::Timeout) },
            }));
            // Its entry, if it still waits, ends with it.
            if let Some(e) = entry.filter(|e| lock(&self.entries).remove(e).is_some()) {
                conn.queue.control(Ipc::UiNotify(Notify::Session {
                    session_id: e,
                    device_id: device_hex.clone(),
                    state: SessionState::NotOpened { why: ConnectWhy::Unreachable },
                }));
            }
        }
        if began {
            let now = self.p.wall.unix_ms();
            let duration = u32::try_from(now.saturating_sub(started) / 1000).unwrap_or(u32::MAX);
            self.p.audit.append(AuditEntry {
                time_ms: now,
                device: DeviceRef::Known(device),
                event: AuditEvent::SessionEnded(end_code),
                duration_s: Some(duration),
            });
            self.session_ended();
            self.devices_changed();
        }
    }

    fn set_settings(&self, update: &SettingsUpdate) -> Result<(), ApiCode> {
        if !valid_name(&update.name) {
            return Err(ApiCode::Invalid);
        }
        let (before, after) = {
            let mut s = lock(&self.settings);
            let before = s.clone();
            s.name = update.name.clone();
            s.service_enabled = update.service_enabled;
            s.lock_on_end = update.lock_on_end;
            s.keep_awake = update.keep_awake;
            if let Err(e) = store_settings(&*self.p.secrets, &s) {
                *s = before;
                self.log(Level::Warn, format_args!("settings were not written: {e}"));
                return Err(ApiCode::Internal);
            }
            (before, s.clone())
        };
        if before.service_enabled != after.service_enabled {
            self.p.hooks.service_enabled(after.service_enabled);
        }
        if before.keep_awake != after.keep_awake {
            self.p
                .hooks
                .keep_awake(after.keep_awake && self.live_sessions.load(std::sync::atomic::Ordering::SeqCst) > 0);
        }
        self.notify(Notify::SettingsChanged(self.settings_view()));
        Ok(())
    }
}

fn display_views(list: &DisplayList) -> Vec<DisplayView> {
    list.displays
        .iter()
        .take(swapdesk_ipc::MAX_DISPLAYS)
        .map(|d| DisplayView {
            id: d.id,
            name: d.name.clone(),
            width: d.width,
            height: d.height,
            scale_milli: d.scale_milli,
            primary: d.primary,
        })
        .collect()
}

/// The input records of a special key: each key goes down and up, with the HID modifier byte after each event.
pub(super) fn key_events(keys: SpecialKeys, host_is_mac: bool) -> Vec<InputEvent> {
    const L_ALT: (u16, u8) = (0xE2, 1 << 2);
    const L_GUI: (u16, u8) = (0xE3, 1 << 3);
    let key = |usage: u16, down: bool, modifiers: u8| InputEvent::Key {
        usage_page: KEYBOARD_PAGE,
        usage,
        down,
        modifiers,
        locks: 0,
    };
    // `held`: a modifier kept down around the tap of `usage`.
    let chord = |held: Option<(u16, u8)>, usage: u16| {
        let mods = held.map_or(0, |h| h.1);
        let mut v = Vec::new();
        if let Some((u, m)) = held {
            v.push(key(u, true, m));
        }
        v.push(key(usage, true, mods));
        v.push(key(usage, false, mods));
        if let Some((u, _)) = held {
            v.push(key(u, false, 0));
        }
        v
    };
    match keys {
        // Not input: the caller sent the secure attention sequence instead.
        SpecialKeys::CtrlAltDel => Vec::new(),
        SpecialKeys::AltTab => chord(Some(L_ALT), 0x2B),
        SpecialKeys::WinOrCmd => chord(None, L_GUI.0),
        SpecialKeys::AltF4OrCmdQ if host_is_mac => chord(Some(L_GUI), 0x14),
        SpecialKeys::AltF4OrCmdQ => chord(Some(L_ALT), 0x3D),
        SpecialKeys::PrintScreen => chord(None, 0x46),
    }
}
