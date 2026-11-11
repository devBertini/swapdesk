//! The host's user, as the session layer sees it ([`HostUi`]): a prompt or a consent question becomes an
//! `IncomingRequest` on the console session's UI and waits for `AnswerIncoming`. With no UI there the request is
//! declined at once (nobody to ask, F-SESS-01); the 20 s auto-decline stays in `swapdesk-session`.
//!
//! The host events of the session layer (a session started, ended, its grants changed) become the banner's
//! `Session` notifications and the audit entries of F-SESS-04.

use std::sync::Arc;

use swapdesk_ipc::{ApiCode, IncomingRequest, Notify, RequestKind, Role, SessionState};
use swapdesk_proto::{Desktop, Feature, PermissionSet};
use swapdesk_session::{ConsentRequest, HostEvent, HostUi, PromptAnswer, PromptEnd, PromptRequest, StartInfo};
use swapdesk_trust::{AuditEntry, AuditEvent, DeviceId, DeviceRef, Direction, UserRef, device_id_hex};
use tokio::sync::oneshot;

use super::{Conn, Hub, lock};
use crate::service::end_reason;

pub(crate) enum PendingKind {
    Prompt { host_request: u64, tx: oneshot::Sender<PromptAnswer> },
    Consent { device: DeviceId, feature: Feature, tx: oneshot::Sender<bool> },
}

/// A request waiting for the host's user.
pub(crate) struct Pending {
    pub kind: PendingKind,
    pub view: IncomingRequest,
}

pub(crate) struct HubHostUi {
    hub: Arc<Hub>,
}

impl HubHostUi {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
}

impl Hub {
    /// Shows the requests still waiting to a UI that just connected (or became the console's).
    pub fn send_pending(&self, conn: &Arc<Conn>) {
        if !self.console_conns().iter().any(|c| c.id == conn.id) {
            return;
        }
        let views: Vec<IncomingRequest> = lock(&self.incoming).values().map(|p| p.view.clone()).collect();
        for v in views {
            conn.queue.control(swapdesk_ipc::Message::UiNotify(Notify::IncomingRequest(v)));
        }
    }

    /// `AnswerIncoming`: the host's user decided. `grants` are validated by the caller.
    pub fn answer_incoming(
        &self,
        request_id: u32,
        decision: swapdesk_ipc::Decision,
        grants: PermissionSet,
    ) -> Result<(), ApiCode> {
        let pending = lock(&self.incoming).remove(&request_id).ok_or(ApiCode::NotFound)?;
        match pending.kind {
            PendingKind::Prompt { tx, .. } => {
                let answer = match decision {
                    swapdesk_ipc::Decision::Accept => PromptAnswer::Accept(grants),
                    swapdesk_ipc::Decision::ViewOnly => PromptAnswer::Accept(PermissionSet::VIEW),
                    swapdesk_ipc::Decision::Decline => PromptAnswer::Decline,
                };
                let _ = tx.send(answer);
            }
            PendingKind::Consent { tx, .. } => {
                let _ = tx.send(matches!(decision, swapdesk_ipc::Decision::Accept | swapdesk_ipc::Decision::ViewOnly));
            }
        }
        Ok(())
    }

    fn close_incoming(&self, request_id: u32, reason: ApiCode) {
        self.notify_console(Notify::IncomingClosed { request_id, reason });
    }

    fn audit(&self, device: DeviceId, event: AuditEvent, duration_s: Option<u32>) {
        self.p.audit.append(AuditEntry {
            time_ms: self.p.wall.unix_ms(),
            device: DeviceRef::Known(device),
            event,
            duration_s,
        });
    }

    /// Who changed grants from the banner: the console UI's user when there is one.
    pub(super) fn console_user(&self) -> UserRef {
        self.console_conns()
            .first()
            .map(|c| c.info.user.clone())
            .unwrap_or(UserRef { id: String::new(), name: String::new() })
    }
}

fn timeout_s(d: std::time::Duration) -> u16 {
    u16::try_from(d.as_secs()).unwrap_or(u16::MAX)
}

impl HostUi for HubHostUi {
    fn somebody_present(&self) -> bool {
        let hub = &self.hub;
        !hub.console_conns().is_empty() && *lock(&hub.desktop) == Desktop::Default
    }

    fn console_owner(&self) -> swapdesk_session::ConsoleOwner {
        self.hub.console_owner()
    }

    fn prompt(&self, request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
        let hub = &self.hub;
        let (tx, rx) = oneshot::channel();
        let conns = hub.console_conns();
        if conns.is_empty() {
            let _ = tx.send(PromptAnswer::Decline);
            return rx;
        }
        let request_id = hub.fresh_id();
        let view = IncomingRequest {
            request_id,
            device: hub.device_view_of(&request.device, &request.name, request.os),
            kind: RequestKind::Session { intent: request.intent },
            // The stored grants, or, for a device that has none yet (just paired), what its user asked for:
            // ticking every box by hand before "Aceitar" is a dead end. Never UNATTENDED or ADMIN_TERMINAL (D8), and
            // never the terminal: a viewer asks for it with every connect to a 1.1 host (an entry without a prompt
            // grants only what was asked), so it says nothing of what its user wants, and one "Aceitar" must not
            // open a shell.
            // Only the bits go: the sets UNATTENDED and ADMIN_TERMINAL also hold VIEW and TERMINAL, and masking
            // with them left CONTROL without VIEW, a set the answer's own check refuses as invalid.
            preset: if request.stored.is_empty() {
                PermissionSet::closure(
                    request.requested.bits()
                        & !(PermissionSet::BIT_UNATTENDED
                            | PermissionSet::BIT_ADMIN_TERMINAL
                            | PermissionSet::BIT_TERMINAL),
                )
                .bits()
            } else {
                request.stored.bits()
            },
            timeout_s: timeout_s(request.timeout),
        };
        lock(&hub.incoming).insert(
            request_id,
            Pending { kind: PendingKind::Prompt { host_request: request.request_id, tx }, view: view.clone() },
        );
        for c in conns {
            c.queue.control(swapdesk_ipc::Message::UiNotify(Notify::IncomingRequest(view.clone())));
        }
        rx
    }

    fn prompt_closed(&self, request_id: u64, how: PromptEnd) {
        let hub = &self.hub;
        let found = {
            let mut map = lock(&hub.incoming);
            let key = map
                .iter()
                .find(
                    |(_, p)| matches!(&p.kind, PendingKind::Prompt { host_request, .. } if *host_request == request_id),
                )
                .map(|(k, _)| *k);
            key.and_then(|k| map.remove(&k).map(|_| k))
        };
        if let Some(id) = found {
            match how {
                PromptEnd::Answered => {}
                PromptEnd::TimedOut => hub.close_incoming(id, ApiCode::Timeout),
                PromptEnd::Cancelled => hub.close_incoming(id, ApiCode::Refused),
            }
        }
    }

    fn consent(&self, request: ConsentRequest) -> oneshot::Receiver<bool> {
        let hub = &self.hub;
        let (tx, rx) = oneshot::channel();
        let conns = hub.console_conns();
        if conns.is_empty() {
            let _ = tx.send(false);
            return rx;
        }
        let request_id = hub.fresh_id();
        let view = IncomingRequest {
            request_id,
            device: hub.device_view_of(&request.device, "", swapdesk_proto::Os::Windows),
            kind: RequestKind::Consent { feature: request.feature },
            preset: 0,
            timeout_s: timeout_s(request.timeout),
        };
        lock(&hub.incoming).insert(
            request_id,
            Pending {
                kind: PendingKind::Consent { device: request.device, feature: request.feature, tx },
                view: view.clone(),
            },
        );
        for c in conns {
            c.queue.control(swapdesk_ipc::Message::UiNotify(Notify::IncomingRequest(view.clone())));
        }
        rx
    }

    fn consent_closed(&self, request: &ConsentRequest, how: PromptEnd) {
        let hub = &self.hub;
        let found = {
            let mut map = lock(&hub.incoming);
            let key = map.iter().find(|(_, p)| matches!(&p.kind, PendingKind::Consent { device, feature, .. } if *device == request.device && *feature == request.feature)).map(|(k, _)| *k);
            key.and_then(|k| map.remove(&k).map(|_| k))
        };
        if let Some(id) = found {
            match how {
                PromptEnd::Answered => {}
                PromptEnd::TimedOut => hub.close_incoming(id, ApiCode::Timeout),
                PromptEnd::Cancelled => hub.close_incoming(id, ApiCode::Refused),
            }
        }
    }

    fn lock_on_end(&self) -> bool {
        self.hub.settings().lock_on_end
    }

    fn lock_screen(&self) {
        self.hub.p.hooks.lock_screen();
    }

    fn start_info(&self) -> StartInfo {
        let hub = &self.hub;
        let list = hub.p.interfaces.list().unwrap_or_default();
        let approved = swapdesk_lan::guard::approved_indexes(&list);
        // Where a viewer may send a Wake-on-LAN packet later: the hardware address of each approved interface.
        let wake_macs = list.iter().filter(|i| approved.contains(&i.index)).filter_map(|i| i.mac).collect();
        StartInfo { desktop: hub.effective_desktop(), displays: Vec::new(), wake_macs }
    }

    fn current_desktop(&self) -> Option<Desktop> {
        Some(self.hub.effective_desktop())
    }

    fn event(&self, event: HostEvent) {
        let hub = &self.hub;
        let now = hub.p.wall.unix_ms();
        match event {
            HostEvent::SessionStarted { device, session_id, mode, granted } => {
                lock(&hub.started).insert(session_id, (now, granted));
                hub.audit(
                    device,
                    AuditEvent::SessionStarted {
                        direction: Direction::Incoming,
                        mode,
                        granted,
                        desktop: *lock(&hub.desktop),
                    },
                    None,
                );
                // `session_began` also pushes the device list, which now shows the viewer on the network.
                hub.session_began();
                hub.notify_console(Notify::Session {
                    session_id,
                    device_id: device_id_hex(&device),
                    state: SessionState::Active { since_ms: now, role: Role::Host, granted: granted.bits() },
                });
            }
            HostEvent::Refused { device, code } => {
                hub.log(
                    swapdesk_platform::Level::Info,
                    format_args!("a request from {} was refused: {code:?}", device_id_hex(&device)),
                );
            }
            HostEvent::Ended { device, session_id, end, .. } => {
                let since = lock(&hub.started).remove(&session_id).map_or(now, |(t, _)| t);
                let duration = u32::try_from(now.saturating_sub(since) / 1000).unwrap_or(u32::MAX);
                hub.audit(device, AuditEvent::SessionEnded(end.code), Some(duration));
                hub.session_ended();
                hub.notify_console(Notify::Session {
                    session_id,
                    device_id: device_id_hex(&device),
                    state: SessionState::Ended { reason: end_reason(&end) },
                });
                hub.devices_changed();
            }
            HostEvent::GrantsChanged { device, session_id, from, to } => {
                let since = lock(&hub.started).get_mut(&session_id).map_or(now, |e| {
                    e.1 = to;
                    e.0
                });
                hub.audit(device, AuditEvent::GrantsChanged { from, to, by: hub.console_user() }, None);
                hub.notify_console(Notify::Session {
                    session_id,
                    device_id: device_id_hex(&device),
                    state: SessionState::Active { since_ms: since, role: Role::Host, granted: to.bits() },
                });
            }
            HostEvent::PeerOffline { device, session_id } => {
                hub.notify_console(Notify::Session {
                    session_id,
                    device_id: device_id_hex(&device),
                    state: SessionState::Reconnecting { attempt: 0 },
                });
            }
            HostEvent::PeerOnline { device, session_id } => {
                let (since, granted) = lock(&hub.started).get(&session_id).map_or((now, 0), |(t, g)| (*t, g.bits()));
                hub.notify_console(Notify::Session {
                    session_id,
                    device_id: device_id_hex(&device),
                    state: SessionState::Active { since_ms: since, role: Role::Host, granted },
                });
            }
            HostEvent::ConsentAsked { device, feature, answer } => {
                hub.audit(device, AuditEvent::ConsentAsked { feature, answer }, None);
            }
            HostEvent::PeerGrants { device, grants } => hub.apply_peer_grants(device, grants),
            // What a device said over a connection its pinned key authenticated wins over its record's hint.
            HostEvent::PeerMinor { device, minor } => {
                let hint = hub.minor_hint(&device);
                lock(&hub.minors).said(device, minor, hint);
            }
        }
    }
}
