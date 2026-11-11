//! What the hub shows: the device list, the network status, this device, the settings and the activity lines, built
//! from the trust store, the presence, the interface list and the audit log; and the audit sink that raises the
//! "unknown device" alert.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4};
use std::sync::Weak;

use swapdesk_ipc::{
    ApiCode, AuditView, DeviceStatus, DeviceView, Direction as ViewDirection, KeyboardMode, Language, Msg,
    NetCategoryView, NetworkStatus, Notice, Notify, Ready, SessionSummary, Settings, Text, Theme, ThisDevice,
};
use swapdesk_lan::guard::approved_addrs;
use swapdesk_platform::{NetCategory, NetInterface};
use swapdesk_proto::{Desktop, ErrorCode, GrantMode, Os, PermissionSet, Preset};
use swapdesk_session::SessionCtx;
use swapdesk_trust::{AuditEntry, AuditEvent, AuditSink, DeviceId, DeviceRef, Direction, PairedDevice, device_id_hex};
use swapdesk_ui::strings::MsgKey;

use super::presence::PresencePeer;
use super::{Hub, lock};

/// How many audit entries the device list reads for "last session".
const LAST_SESSION_SCAN: usize = 2000;

/// A paired device with a live session (either role): where the connection goes, and the session's round trip.
struct LiveLink {
    device: DeviceId,
    addr: Option<Ipv4Addr>,
    rtt_ms: u32,
}

/// What one device list is built from besides the trust store (see `Hub::reach`).
struct Reach {
    presence: Vec<PresencePeer>,
    live: Vec<LiveLink>,
    /// Typed addresses that belong to a paired device: listed as that device, not on their own.
    typed: Vec<(DeviceId, Ipv4Addr)>,
    /// Typed addresses that are no known device's: listed as entries of their own.
    unmatched: Vec<Manual>,
}

/// The device a typed address belongs to, by an identity bound to that address (see `Hub::reach`), or `None`.
fn typed_owner(
    m: &Manual,
    paired: &[PairedDevice],
    presence: &[PresencePeer],
    live: &[LiveLink],
    learned: &HashMap<DeviceId, SocketAddrV4>,
) -> Option<DeviceId> {
    let at = SocketAddrV4::new(m.addr, m.session_port);
    presence
        .iter()
        .find(|p| p.addrs.contains(&m.addr) && p.session_port == m.session_port)
        .map(|p| p.device_id)
        .or_else(|| learned.iter().find(|(_, a)| **a == at).map(|(id, _)| *id))
        .or_else(|| live.iter().find(|l| l.addr == Some(m.addr)).map(|l| l.device))
        .or_else(|| {
            // The address the pairing or a session stored, unless DNS-SD places that device somewhere else now.
            if m.session_port != crate::service::DEFAULT_SESSION_PORT {
                return None;
            }
            paired
                .iter()
                .find(|d| {
                    d.last_addrs.contains(&m.addr.octets()) && !presence.iter().any(|p| p.device_id == d.device_id)
                })
                .map(|d| d.device_id)
        })
}

/// The id prefix of a device that is only an address the user typed.
pub(crate) const MANUAL_PREFIX: &str = "addr:";

/// An address the user typed: a placeholder for a device not paired yet (its identity is learned in the pairing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manual {
    pub addr: Ipv4Addr,
    /// The peer's session port: [`crate::service::DEFAULT_SESSION_PORT`], or the one typed (development mode only).
    pub session_port: u16,
}

impl Manual {
    pub fn id(&self) -> String {
        if self.session_port == crate::service::DEFAULT_SESSION_PORT {
            format!("{MANUAL_PREFIX}{}", self.addr)
        } else {
            format!("{MANUAL_PREFIX}{}:{}", self.addr, self.session_port)
        }
    }

    /// The pairing endpoint is the next port of the session endpoint (PROTOCOL.md 1: 47820, 47821).
    pub fn pair_port(&self) -> u16 {
        self.session_port.wrapping_add(1)
    }
}

/// Parses the 32 hexadecimal characters of a device id.
pub(crate) fn parse_device_id(text: &str) -> Option<DeviceId> {
    if text.len() != 32 || !text.is_ascii() {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(id)
}

/// The `ApiCode` of a protocol error code.
pub(crate) fn api_of(code: ErrorCode) -> ApiCode {
    match code {
        ErrorCode::NotPermitted | ErrorCode::NobodyPresent => ApiCode::NotPermitted,
        ErrorCode::Busy => ApiCode::Busy,
        ErrorCode::Timeout => ApiCode::Timeout,
        ErrorCode::PairingLocked => ApiCode::Locked,
        ErrorCode::UnknownDevice | ErrorCode::NotFound | ErrorCode::Forgotten => ApiCode::NotFound,
        ErrorCode::HostShutdown => ApiCode::ServiceDown,
        ErrorCode::Declined
        | ErrorCode::SasRejected
        | ErrorCode::CommitMismatch
        | ErrorCode::AlreadyPaired
        | ErrorCode::Unavailable => ApiCode::Refused,
        ErrorCode::InvalidName => ApiCode::Invalid,
        ErrorCode::NoError => ApiCode::Internal,
        _ => ApiCode::Internal,
    }
}

fn msg(key: MsgKey, args: &[String]) -> Msg {
    let index = MsgKey::ALL.iter().position(|k| *k == key).unwrap_or(0);
    Msg { key: u16::try_from(index).unwrap_or(0), args: args.iter().cloned().map(Text).collect() }
}

/// The activity line of an audit entry, and whether it is an alert; `None` for the events the UI has no text for yet
/// (the Activity screen skips them: C6 `AuditView.what` is a `Msg` of `swapdesk-ui`, and its table has no key for them).
fn activity_line(entry: &AuditEntry, device_name: &str) -> Option<(Msg, bool)> {
    let unknown = matches!(entry.device, DeviceRef::Unknown { .. });
    Some(match &entry.event {
        AuditEvent::Paired => (msg(MsgKey::AuditPaired, &[]), false),
        AuditEvent::UnknownDeviceRejected => (msg(MsgKey::AuditUnknownPair, &[]), true),
        AuditEvent::PairFailed(ErrorCode::SasRejected | ErrorCode::Timeout | ErrorCode::CommitMismatch)
        | AuditEvent::PairRefused(ErrorCode::Timeout)
        | AuditEvent::PairingLocked => (msg(MsgKey::AuditUnknownPair, &[]), unknown),
        AuditEvent::SessionStarted { desktop: Desktop::LoginWindow, mode: GrantMode::Unattended, .. } => {
            (msg(MsgKey::AuditUnattendedLogin, &[]), false)
        }
        AuditEvent::SessionStarted { granted, .. } if !granted.contains(PermissionSet::CONTROL) => {
            (msg(MsgKey::AuditViewOnly, &[]), false)
        }
        AuditEvent::SessionStarted { direction: Direction::Incoming, .. } => (msg(MsgKey::AuditControlled, &[]), false),
        AuditEvent::SessionStarted { direction: Direction::Outgoing, .. } => {
            (msg(MsgKey::AuditControlledFromHere, &[]), false)
        }
        AuditEvent::Swapped => (msg(MsgKey::AuditSwap, &[device_name.to_string()]), false),
        AuditEvent::FilesSent { count, .. } => (msg(MsgKey::AuditControlFiles, &[count.to_string()]), false),
        _ => return None,
    })
}

/// The `Ready` of this device from the interface list.
fn ready_of(list: &[NetInterface]) -> Ready {
    if approved_addrs(list).is_empty() { Ready::NoNetwork } else { Ready::ReadyToReceive }
}

impl Hub {
    fn interfaces(&self) -> Vec<NetInterface> {
        self.p.interfaces.list().unwrap_or_default()
    }

    pub fn this_device(&self) -> ThisDevice {
        ThisDevice { name: self.settings().name, os: self.p.os, ready: ready_of(&self.interfaces()) }
    }

    /// The first approved interface that is up, its address and category; the notices the UI shows as banners.
    pub fn network_status(&self) -> NetworkStatus {
        let list = self.interfaces();
        let approved = approved_addrs(&list);
        let Some(first) = approved.first() else {
            return NetworkStatus { label: String::new(), address: None, category: None, notices: Vec::new() };
        };
        let iface = list.iter().find(|i| i.index == first.if_index);
        let category = iface.and_then(|i| i.category);
        let mut notices = Vec::new();
        if category == Some(NetCategory::Public) {
            notices.push(Notice::PublicNetwork);
        }
        if iface.is_some_and(|i| i.inbound_blocked == Some(true)) {
            notices.push(Notice::FirewallBlocks);
        }
        NetworkStatus {
            label: first.if_name.clone(),
            address: Some(first.addr.to_string()),
            category: category.map(|c| match c {
                NetCategory::Public => NetCategoryView::Public,
                NetCategory::Private => NetCategoryView::Private,
                NetCategory::Domain => NetCategoryView::Domain,
            }),
            notices,
        }
    }

    /// The settings of C6: the machine fields from `settings.bin`, the per-user fields at their defaults (the UI
    /// overlays its own from `ui.bin`, C5.1).
    pub fn settings_view(&self) -> Settings {
        let s = self.settings();
        Settings {
            name: s.name,
            language: Language::PtBr,
            open_at_login: false,
            service_enabled: s.service_enabled,
            lock_on_end: s.lock_on_end,
            keep_awake: s.keep_awake,
            quality: Preset::Auto,
            show_stats: false,
            keyboard_mode: KeyboardMode::Auto,
            swap_ctrl_cmd: false,
            theme: Theme::System,
        }
    }

    /// The last session with `id` read from the audit log: when it started, how long it ran, which way, on which
    /// desktop.
    fn last_session(recent: &[AuditEntry], id: &DeviceId) -> Option<SessionSummary> {
        // `recent` is newest first: the first end seen, then the start before it.
        let mut ended: Option<(u64, u32)> = None;
        for e in recent.iter().filter(|e| e.device == DeviceRef::Known(*id)) {
            match &e.event {
                AuditEvent::SessionEnded(_) if ended.is_none() => ended = Some((e.time_ms, e.duration_s.unwrap_or(0))),
                AuditEvent::SessionStarted { direction, desktop, .. } => {
                    let (at_ms, duration_s) = match ended {
                        Some((end_ms, d)) if end_ms >= e.time_ms => (e.time_ms, d),
                        _ => (e.time_ms, 0),
                    };
                    let direction = match direction {
                        swapdesk_trust::Direction::Incoming => ViewDirection::Incoming,
                        swapdesk_trust::Direction::Outgoing => ViewDirection::Outgoing,
                    };
                    return Some(SessionSummary { at_ms, duration_s, direction, desktop: *desktop });
                }
                _ => {}
            }
        }
        None
    }

    /// The paired devices with a live session to or from this machine, each with where its connection goes and the
    /// session's own round trip. Such a device is reachable by definition, whatever DNS-SD says: its record can be
    /// late, lost, or held back by the peer (a Mac that never noticed its address changed).
    fn live_links(&self) -> Vec<LiveLink> {
        let mut ctxs: Vec<SessionCtx> = Vec::new();
        if let Some(core) = self.core() {
            let host = core.host();
            ctxs.extend(host.sessions().iter().filter_map(|s| host.session(s.session_id)));
        }
        for c in self.conns() {
            ctxs.extend(lock(&c.viewers).values().map(|v| v.handle.ctx()));
        }
        ctxs.iter()
            .filter(|c| c.is_active())
            .filter_map(|c| {
                Some(LiveLink {
                    device: c.device()?,
                    addr: c.connection().and_then(|q| match q.remote_address().ip() {
                        IpAddr::V4(v4) => Some(v4),
                        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
                    }),
                    rtt_ms: c.rtt().map_or(0, |d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)),
                })
            })
            .collect()
    }

    /// What says, for one device list, which paired device is on the network: DNS-SD, the live sessions, and the
    /// typed addresses. Every match is by device id, and an address counts only through an identity bound to it by
    /// an authenticated step: the fingerprint DNS-SD announced there, the pairing that ran through it, the session
    /// connected there, or the address the pairing or a session recorded in the trust store. Names never match (a
    /// device can be renamed on either side).
    fn reach(&self, paired: &[PairedDevice]) -> Reach {
        let presence = self.p.presence.peers();
        let live = self.live_links();
        let learned = lock(&self.learned).clone();
        let mut typed = Vec::new();
        let mut unmatched = Vec::new();
        for m in lock(&self.manual).iter() {
            match typed_owner(m, paired, &presence, &live, &learned) {
                Some(id) => {
                    if paired.iter().any(|d| d.device_id == id) {
                        typed.push((id, m.addr));
                    }
                }
                None => unmatched.push(m.clone()),
            }
        }
        Reach { presence, live, typed, unmatched }
    }

    fn paired_view(&self, d: &PairedDevice, reach: &Reach, recent: &[AuditEntry]) -> DeviceView {
        let seen = reach.presence.iter().find(|p| p.device_id == d.device_id).and_then(|p| p.addrs.first());
        let typed = reach.typed.iter().find(|(id, _)| *id == d.device_id).map(|(_, a)| a);
        let status = match (reach.live.iter().find(|l| l.device == d.device_id), seen.or(typed)) {
            // A live session is the best proof of reach, and its address is the one that works now.
            (Some(l), other) => {
                let addr = l.addr.or(other.copied()).or_else(|| d.last_addrs.first().map(|a| Ipv4Addr::from(*a)));
                DeviceStatus::Online { address: addr.map(|a| a.to_string()).unwrap_or_default(), rtt_ms: l.rtt_ms }
            }
            // Seen by DNS-SD, or typed by the user at an address that is this device's: one entry, on the network.
            // The round trip is measured by the session's own pings; there is none here, so it reads 0.
            (None, Some(addr)) => DeviceStatus::Online { address: addr.to_string(), rtt_ms: 0 },
            (None, None) if !d.wol_macs.is_empty() => DeviceStatus::Asleep,
            (None, None) => DeviceStatus::Offline,
        };
        DeviceView {
            id: device_id_hex(&d.device_id),
            name: d.name.clone(),
            os: d.os,
            os_version: d.os_version.clone(),
            status,
            paired: true,
            grants_here: d.grants_here.bits(),
            grants_there: d.grants_there.map(PermissionSet::bits),
            last_session: Self::last_session(recent, &d.device_id),
            wakeable: !d.wol_macs.is_empty(),
        }
    }

    /// The device list: paired devices (by name), then the unpaired ones seen on the network, then the typed
    /// addresses that are no known device's. A paired device is listed once, whatever else knows of it.
    pub fn device_views(&self) -> Vec<DeviceView> {
        let recent = self.p.audit.recent(LAST_SESSION_SCAN);
        let mut paired: Vec<PairedDevice> = self.p.trust.read(|s| s.devices.clone());
        paired.sort_by(|a, b| a.name.cmp(&b.name).then(a.device_id.cmp(&b.device_id)));
        let reach = self.reach(&paired);
        let mut out: Vec<DeviceView> = paired.iter().map(|d| self.paired_view(d, &reach, &recent)).collect();
        for p in reach.presence.iter().filter(|p| !paired.iter().any(|d| d.device_id == p.device_id)) {
            out.push(DeviceView {
                id: device_id_hex(&p.device_id),
                name: p.name.clone(),
                os: p.os,
                os_version: String::new(),
                status: DeviceStatus::OnNetworkUnpaired { pairing_open: p.pairing_open },
                paired: false,
                grants_here: 0,
                grants_there: None,
                last_session: None,
                wakeable: false,
            });
        }
        out.extend(reach.unmatched.iter().map(Self::typed_entry));
        out.truncate(swapdesk_ipc::MAX_DEVICES);
        out
    }

    /// The view of a typed address: the paired device it belongs to (see `Hub::reach`), else an entry of its own.
    pub fn manual_view(&self, m: &Manual) -> DeviceView {
        let paired: Vec<PairedDevice> = self.p.trust.read(|s| s.devices.clone());
        let mut reach = self.reach(&paired);
        let owner = typed_owner(m, &paired, &reach.presence, &reach.live, &lock(&self.learned).clone());
        match owner.and_then(|id| paired.iter().find(|d| d.device_id == id)) {
            Some(d) => {
                reach.typed.insert(0, (d.device_id, m.addr));
                self.paired_view(d, &reach, &self.p.audit.recent(LAST_SESSION_SCAN))
            }
            None => Self::typed_entry(m),
        }
    }

    /// A typed address that is no known device's.
    fn typed_entry(m: &Manual) -> DeviceView {
        DeviceView {
            id: m.id(),
            name: m.addr.to_string(),
            // A typed address says nothing about the machine behind it: the pairing reveals it. Windows is the
            // placeholder the contract's `Os` forces.
            os: Os::Windows,
            os_version: String::new(),
            status: DeviceStatus::OnNetworkUnpaired { pairing_open: false },
            paired: false,
            grants_here: 0,
            grants_there: None,
            last_session: None,
            wakeable: false,
        }
    }

    /// One paired device as a view (for an incoming request), or a bare view when it is gone.
    pub fn device_view_of(&self, id: &DeviceId, fallback_name: &str, os: Os) -> DeviceView {
        let recent = self.p.audit.recent(LAST_SESSION_SCAN);
        match self.p.trust.read(|s| s.find_by_id(id).cloned()) {
            Some(d) => self.paired_view(&d, &self.reach(std::slice::from_ref(&d)), &recent),
            None => DeviceView {
                id: device_id_hex(id),
                name: fallback_name.into(),
                os,
                os_version: String::new(),
                status: DeviceStatus::Offline,
                paired: false,
                grants_here: 0,
                grants_there: None,
                last_session: None,
                wakeable: false,
            },
        }
    }

    /// The activity lines, newest first, `before_ms` exclusive, at most `limit`.
    pub fn activity(&self, before_ms: Option<u64>, limit: usize) -> Vec<AuditView> {
        let limit = limit.min(swapdesk_ipc::MAX_ACTIVITY);
        let names: Vec<(DeviceId, String)> =
            self.p.trust.read(|s| s.devices.iter().map(|d| (d.device_id, d.name.clone())).collect());
        let mut out = Vec::new();
        // Newest first, so a session's end comes before its start: the end's duration waits here for the start's line.
        let mut ends: Vec<(DeviceRef, u32)> = Vec::new();
        // Entries without a line are skipped, so read more than `limit` and cut after.
        for e in self.p.audit.recent(limit.saturating_mul(4).max(256)) {
            if let AuditEvent::SessionEnded(_) = e.event {
                ends.push((e.device, e.duration_s.unwrap_or(0)));
                continue;
            }
            let mut duration_s = e.duration_s;
            if let AuditEvent::SessionStarted { .. } = e.event
                && let Some(at) = ends.iter().position(|(d, _)| *d == e.device)
            {
                duration_s = Some(ends.remove(at).1);
            }
            if before_ms.is_some_and(|b| e.time_ms >= b) {
                continue;
            }
            let device_name = match &e.device {
                DeviceRef::Known(id) => {
                    // A device no longer paired has no name here: the interface writes "removed device" in its language.
                    names.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone()).unwrap_or_default()
                }
                DeviceRef::Unknown { addr, .. } => Ipv4Addr::from(*addr).to_string(),
            };
            if let Some((what, alert)) = activity_line(&e, &device_name) {
                out.push(AuditView { at_ms: e.time_ms, device_name, what, duration_s, alert });
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// The protocol minor of `id`'s discovery record, when the device is on the network now.
    pub fn minor_hint(&self, id: &DeviceId) -> Option<u16> {
        self.p.presence.peers().iter().find(|p| &p.device_id == id).map(|p| p.proto_minor)
    }

    /// The protocol minor to count on for `id`, whose record now carries `hint` ([`Hub::minor_hint`]): what it said
    /// in a session, else what its record says, else 0 (PROTOCOL.md 1). A viewer asks for anything of a later minor only toward a device this says knows it.
    pub fn known_minor(&self, id: &DeviceId, hint: Option<u16>) -> u16 {
        lock(&self.minors).known(id, hint)
    }

    /// An address the viewer path can dial for `id`: the device seen on the network now, one learned from a typed
    /// address while we run, else the last address stored with the device and the default port.
    pub fn address_of(&self, id: &DeviceId) -> Option<SocketAddrV4> {
        if let Some(p) = self.p.presence.peers().iter().find(|p| &p.device_id == id)
            && let Some(ip) = p.addrs.first()
        {
            return Some(SocketAddrV4::new(*ip, p.session_port));
        }
        if let Some(a) = lock(&self.learned).get(id) {
            return Some(*a);
        }
        self.p
            .trust
            .read(|s| s.find_by_id(id).and_then(|d| d.last_addrs.first().copied()))
            .map(|a| SocketAddrV4::new(Ipv4Addr::from(a), crate::service::DEFAULT_SESSION_PORT))
    }
}

/// The audit sink of the hub: writes the log, and raises `ActivityAlert` for what is an alert.
pub(crate) struct HubAudit {
    pub hub: Weak<Hub>,
}

impl AuditSink for HubAudit {
    fn append(&self, entry: AuditEntry) {
        let Some(hub) = self.hub.upgrade() else { return };
        hub.p.audit.append(entry.clone());
        let name = match &entry.device {
            DeviceRef::Known(id) => hub.p.trust.read(|s| s.find_by_id(id).map(|d| d.name.clone())).unwrap_or_default(),
            DeviceRef::Unknown { addr, .. } => Ipv4Addr::from(*addr).to_string(),
        };
        if let Some((what, true)) = activity_line(&entry, &name) {
            hub.notify(Notify::ActivityAlert(AuditView {
                at_ms: entry.time_ms,
                device_name: name,
                what,
                duration_s: entry.duration_s,
                alert: true,
            }));
        }
    }
}
