//! Clipboard sync and quick actions in the hub (stage 42): the UI ops `SetClipboardSync` and `RunAction`, and the
//! [`ClipActionHooks`] the core reports to.
//!
//! * **Clipboard.** `SetClipboardSync` flips this side's switch of a session (the peer is told); the clipboard of a
//!   viewer session is the one of the UI that owns it, which the core reaches through that connection's `Rpc`
//!   (`ClipChangeCount` / `ClipRead` / `ClipWrite` messages). A skipped item becomes `Notify::ClipboardSkipped` on the
//!   UI(s) that show the session.
//! * **Quick actions.** `RunAction` on a viewer session sends `QuickAction` to the host and replies when its
//!   `QuickActionResult` arrives: `Done` when it ran, else the code of why not. The host's runner and its audit
//!   (`ActionRun`, written before the action runs) are in the core.

use std::sync::{Arc, Weak};

use swapdesk_ipc::{ApiCode, Message as Ipc, Notify};
use swapdesk_proto::{Action, ClipSkip, ErrorCode};
use swapdesk_trust::{AuditEntry, AuditEvent, DeviceId, DeviceRef};

use super::ops::reply;
use super::views::api_of;
use super::{Conn, Hub, lock};
use crate::rpc::Rpc;
use crate::service::{ApplyError, ClipActionHooks};

/// The `ApiCode` a `ClipboardSkipped` notification carries for each reason.
pub(crate) fn skip_code(reason: ClipSkip) -> ApiCode {
    match reason {
        ClipSkip::TooLarge => ApiCode::Invalid,
        ClipSkip::Concealed => ApiCode::NotPermitted,
        ClipSkip::Unsupported => ApiCode::Refused,
    }
}

impl Hub {
    /// `SetClipboardSync`: this side's switch of a session of this connection (its viewer session, or, on the
    /// console's UI, a session this machine hosts).
    pub fn set_clipboard_sync(&self, conn: &Conn, session_id: u64, on: bool) -> Result<(), ApiCode> {
        if !self.can_use_session(conn, session_id) {
            return Err(ApiCode::NotFound);
        }
        let core = self.core().ok_or(ApiCode::ServiceDown)?;
        core.set_clipboard_sync(session_id, on).map_err(|e| match e {
            ApplyError::NotPermitted => ApiCode::NotPermitted,
            _ => ApiCode::NotFound,
        })
    }

    /// `RunAction`: a quick action on the host of a viewer session of this connection. Replies when the host did.
    pub fn run_action(self: &Arc<Self>, conn: &Arc<Conn>, req: u32, session_id: u64, action: Action) {
        let (handle, view_only) = match lock(&conn.viewers).get(&session_id) {
            Some(v) => (v.handle.clone(), v.view_only),
            None => return reply(conn, req, Err(ApiCode::NotFound)),
        };
        // A view-only choice of this UI covers actions too (they are control).
        if view_only {
            return reply(conn, req, Err(ApiCode::NotPermitted));
        }
        let Some(rt) = self.rt() else { return reply(conn, req, Err(ApiCode::ServiceDown)) };
        let conn = Arc::clone(conn);
        rt.spawn(async move {
            let result = match handle.run_action(action).await {
                Ok(ErrorCode::NoError) => Ok(swapdesk_ipc::ReplyBody::Done),
                Ok(code) => Err(api_of(code)),
                Err(ApplyError::NotPermitted) => Err(ApiCode::NotPermitted),
                Err(_) => Err(ApiCode::NotFound),
            };
            reply(&conn, req, result);
        });
    }

    /// The pipe of the UI that owns viewer session `session_id`.
    fn clipboard_rpc(&self, session_id: u64) -> Option<Arc<Rpc>> {
        self.conns().into_iter().find(|c| lock(&c.viewers).contains_key(&session_id)).map(|c| Arc::clone(&c.clip))
    }
}

/// What the core reports, handed to the hub (weakly: the owner's handle on the hub keeps it alive).
pub(crate) struct HubClipActions {
    pub hub: Weak<Hub>,
}

impl ClipActionHooks for HubClipActions {
    fn ui_clipboard(&self, session_id: u64) -> Option<Arc<Rpc>> {
        self.hub.upgrade()?.clipboard_rpc(session_id)
    }

    fn clipboard_skipped(&self, session_id: u64, reason: ClipSkip) {
        let Some(hub) = self.hub.upgrade() else { return };
        let n = Notify::ClipboardSkipped { session_id, reason: skip_code(reason) };
        for c in hub.conns_for_session(session_id) {
            c.queue.control(Ipc::UiNotify(n.clone()));
        }
    }

    fn action_starting(&self, session_id: u64, device: Option<DeviceId>, action: Action) {
        let Some(hub) = self.hub.upgrade() else { return };
        // The session's own peer first: it is known even when the session has left the live table. Only a session
        // that never carried a device (none on a host) is looked up there, and one nobody can name is still recorded.
        let device = device.or_else(|| {
            hub.core()
                .and_then(|c| c.host().sessions().into_iter().find(|s| s.session_id == session_id))
                .map(|s| s.device)
        });
        let device = match device {
            Some(d) => DeviceRef::Known(d),
            None => DeviceRef::Unknown { addr: [0; 4], device_id: None },
        };
        hub.p.audit.append(AuditEntry {
            time_ms: hub.p.wall.unix_ms(),
            device,
            event: AuditEvent::ActionRun(action),
            duration_s: None,
        });
    }
}
