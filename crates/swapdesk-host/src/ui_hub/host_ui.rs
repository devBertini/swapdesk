//! The host's user, as the session layer sees it ([`HostUi`]): a prompt or a consent question becomes an
//! `IncomingRequest` on the console session's UI and waits for `AnswerIncoming`. With no UI there the request is
//! declined at once (nobody to ask, F-SESS-01); the 20 s auto-decline stays in `swapdesk-session`.
//!
//! The host events of the session layer (a session started, ended, its grants changed) become the banner's
//! `Session` notifications and the audit entries of F-SESS-04.

use std::sync::Arc;
use std::sync::atomic::Ordering;

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

/// A hosted session whose link was lost and that the core keeps resumable (what its end said, for the end that
/// follows when the window does).
pub(crate) struct LostSession {
    pub number: u64,
    pub device_id: String,
    pub reason: Option<ApiCode>,
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
    /// A hosted session lost its link and the core keeps it resumable for its window: the console's banner shows it
    /// reconnecting until the viewer is back (`SessionStarted` again) or the window ends, never ended at once.
    fn session_lost(self: &Arc<Self>, session_id: u64, device_id: String, reason: Option<ApiCode>) {
        let number = self.lost_gen.fetch_add(1, Ordering::SeqCst) + 1;
        lock(&self.lost).insert(session_id, LostSession { number, device_id: device_id.clone(), reason });
        self.notify_console(Notify::Session {
            session_id,
            device_id,
            state: SessionState::Reconnecting { attempt: 0 },
        });
        let window = *lock(&self.resume_window);
        match self.rt() {
            Some(rt) => {
                let hub = Arc::downgrade(self);
                rt.spawn(async move {
                    tokio::time::sleep(window).await;
                    if let Some(hub) = hub.upgrade() {
                        hub.end_lost(session_id, Some(number));
                    }
                });
            }
            // No runtime to count the window on: nothing would ever end it.
            None => {
                self.end_lost(session_id, None);
            }
        }
    }

    /// The end of a lost hosted session the core kept resumable: its window ran out (`number` is the loss the
    /// window belongs to, so an older window never ends a later loss), or the host's user or a forget ended it.
    /// Nothing happens when it resumed or ended meanwhile.
    pub(super) fn end_lost(&self, session_id: u64, number: Option<u64>) -> bool {
        let lost = {
            let mut table = lock(&self.lost);
            if number.is_some_and(|n| table.get(&session_id).is_none_or(|l| l.number != n)) {
                return false;
            }
            let Some(lost) = table.remove(&session_id) else { return false };
            lost
        };
        lock(&self.started).remove(&session_id);
        lock(&self.hosted).remove(&session_id);
        // A window that ran out reads as the silence it was; the user's own end carries no reason.
        let reason = if number.is_some() { lost.reason } else { None };
        self.notify_console(Notify::Session {
            session_id,
            device_id: lost.device_id,
            state: SessionState::Ended { reason },
        });
        true
    }

    /// Whether the core keeps this hosted session resumable after a lost link.
    pub(super) fn is_lost(&self, session_id: u64) -> bool {
        lock(&self.lost).contains_key(&session_id)
    }

    /// A forgotten device's lost sessions end with its tokens.
    pub(super) fn end_lost_of(&self, device_id: &str) {
        let ids: Vec<u64> =
            lock(&self.lost).iter().filter(|(_, l)| l.device_id == device_id).map(|(id, _)| *id).collect();
        for id in ids {
            self.end_lost(id, None);
        }
    }

    /// Shows the requests still waiting to a UI that just connected (or became the console's).
    pub fn send_pending(&self, conn: &Arc<Conn>) {
        if !self.console_conns().iter().any(|c| c.id == conn.id) {
            return;
        }
        let views: Vec<IncomingRequest> = lock(&self.incoming).values().map(|p| p.view.clone()).collect();
        for v in views {
            conn.queue.control(swapdesk_ipc::Message::UiNotify(Notify::IncomingRequest(v)));
        }
        // The hosted sessions that run now: a window that starts while one runs (F-SESS-03: the service starts it when
        // the user quit it) shows its banner at once, as it would have had it been there at the start.
        for (session_id, device_id, state) in self.hosted_states() {
            conn.queue.control(swapdesk_ipc::Message::UiNotify(Notify::Session { session_id, device_id, state }));
        }
    }

    /// What the banner shows of each hosted session now: active, or reconnecting while its link is lost.
    fn hosted_states(&self) -> Vec<(u64, String, SessionState)> {
        let hosted = lock(&self.hosted).clone();
        let started = lock(&self.started);
        let lost = lock(&self.lost);
        let mut states: Vec<(u64, String, SessionState)> = hosted
            .into_iter()
            .filter_map(|(id, device)| {
                let (since_ms, granted) = *started.get(&id)?;
                let state = if lost.contains_key(&id) {
                    SessionState::Reconnecting { attempt: 0 }
                } else {
                    SessionState::Active { since_ms, role: Role::Host, granted: granted.bits(), elevation: 0 }
                };
                Some((id, device, state))
            })
            .collect();
        states.sort_by_key(|(id, ..)| *id);
        states
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
    /// D24: the Mac cannot see or control its lock screen or login window; Windows can (the SYSTEM helper).
    fn serves_other_desktops(&self) -> bool {
        self.hub.p.os != swapdesk_proto::Os::MacOs
    }

    fn somebody_present(&self) -> bool {
        let hub = &self.hub;
        !hub.console_conns().is_empty() && *lock(&hub.desktop) == Desktop::Default
    }

    fn console_owner(&self) -> swapdesk_session::ConsoleOwner {
        self.hub.console_owner()
    }

    fn banner_ready(&self) -> bool {
        self.hub.banner_ready()
    }

    fn start_banner_window(&self) {
        self.hub.start_window();
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
        use swapdesk_session::ConsoleOwner;
        let hub = &self.hub;
        // Only the account at the console has a screen to lock: with another account at it (or the sign-in screen
        // up) the console is not this account's to touch.
        let session = *lock(&hub.console);
        match (hub.console_owner(), session) {
            (ConsoleOwner::This, Some(session)) => hub.p.hooks.lock_screen(session),
            (owner, session) => hub.log(
                swapdesk_platform::Level::Info,
                format_args!("lock on end: nothing of this account to lock (console {owner:?}, session {session:?})"),
            ),
        }
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
                // A resume starts the same session again: it keeps the time it began, not the time it came back.
                lock(&hub.lost).remove(&session_id);
                let began = match mode {
                    swapdesk_proto::GrantMode::Resumed => lock(&hub.started).get(&session_id).map_or(now, |(t, _)| *t),
                    _ => now,
                };
                lock(&hub.started).insert(session_id, (began, granted));
                lock(&hub.hosted).insert(session_id, device_id_hex(&device));
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
                    state: SessionState::Active {
                        since_ms: began,
                        role: Role::Host,
                        granted: granted.bits(),
                        elevation: 0,
                    },
                });
            }
            HostEvent::Refused { device, code } => {
                hub.log(
                    swapdesk_platform::Level::Info,
                    format_args!("a request from {} was refused: {code:?}", device_id_hex(&device)),
                );
            }
            HostEvent::Ended { device, session_id, end, resumable } => {
                // A lost link the core keeps resumable stays the host's session: its time and grants wait for the
                // resume, and the end the UI shows comes when the window does.
                let since = {
                    let mut started = lock(&hub.started);
                    if resumable {
                        started.get(&session_id).map(|(t, _)| *t)
                    } else {
                        lock(&hub.hosted).remove(&session_id);
                        started.remove(&session_id).map(|(t, _)| t)
                    }
                }
                .unwrap_or(now);
                let duration = u32::try_from(now.saturating_sub(since) / 1000).unwrap_or(u32::MAX);
                hub.audit(device, AuditEvent::SessionEnded(end.code), Some(duration));
                hub.session_ended();
                if resumable {
                    hub.session_lost(session_id, device_id_hex(&device), end_reason(&end));
                } else {
                    lock(&hub.lost).remove(&session_id);
                    hub.notify_console(Notify::Session {
                        session_id,
                        device_id: device_id_hex(&device),
                        state: SessionState::Ended { reason: end_reason(&end) },
                    });
                }
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
                    state: SessionState::Active { since_ms: since, role: Role::Host, granted: to.bits(), elevation: 0 },
                });
            }
            HostEvent::Elevated { device, session_id, granted } => {
                let since = lock(&hub.started).get_mut(&session_id).map_or(now, |e| {
                    e.1 = granted;
                    e.0
                });
                hub.audit(device, AuditEvent::Elevated, None);
                hub.notify_console(Notify::Session {
                    session_id,
                    device_id: device_id_hex(&device),
                    state: SessionState::Active {
                        since_ms: since,
                        role: Role::Host,
                        granted: granted.bits(),
                        elevation: 0,
                    },
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
                    state: SessionState::Active { since_ms: since, role: Role::Host, granted, elevation: 0 },
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
