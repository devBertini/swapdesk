//! What the hub shows: the device list, the network status, this device, the settings and the activity lines, built
//! from the trust store, the presence, the interface list and the audit log; and the audit sink that raises the
//! "unknown device" alert.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Weak;

use swapdesk_ipc::{
    ApiCode, AuditView, DeviceStatus, DeviceView, Direction as ViewDirection, KeyboardMode, Language, Msg,
    NetCategoryView, NetworkStatus, Notice, Notify, Ready, SessionSummary, Settings, Text, ThisDevice,
};
use swapdesk_lan::guard::approved_addrs;
use swapdesk_platform::{NetCategory, NetInterface};
use swapdesk_proto::{Desktop, ErrorCode, GrantMode, Os, PermissionSet, Preset};
use swapdesk_trust::{AuditEntry, AuditEvent, AuditSink, DeviceId, DeviceRef, PairedDevice, device_id_hex};
use swapdesk_ui::strings::MsgKey;

use super::presence::PresencePeer;
use super::{Hub, lock};

/// How many audit entries the device list reads for "last session".
const LAST_SESSION_SCAN: usize = 2000;
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

    fn paired_view(&self, d: &PairedDevice, presence: &[PresencePeer], recent: &[AuditEntry]) -> DeviceView {
        let here = presence.iter().find(|p| p.device_id == d.device_id);
        let status = match here {
            Some(p) => match p.addrs.first() {
                // The round trip is measured by the session's own pings; discovery has none, so it reads 0 here.
                Some(addr) => DeviceStatus::Online { address: addr.to_string(), rtt_ms: 0 },
                None => DeviceStatus::Offline,
            },
            None if !d.wol_macs.is_empty() => DeviceStatus::Asleep,
            None => DeviceStatus::Offline,
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
    /// addresses.
    pub fn device_views(&self) -> Vec<DeviceView> {
        let presence = self.p.presence.peers();
        let recent = self.p.audit.recent(LAST_SESSION_SCAN);
        let mut paired: Vec<PairedDevice> = self.p.trust.read(|s| s.devices.clone());
        paired.sort_by(|a, b| a.name.cmp(&b.name).then(a.device_id.cmp(&b.device_id)));
        let mut out: Vec<DeviceView> = paired.iter().map(|d| self.paired_view(d, &presence, &recent)).collect();
        let pairing_open = self.mode.is_open();
        let _ = pairing_open;
        for p in presence.iter().filter(|p| !paired.iter().any(|d| d.device_id == p.device_id)) {
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
        for m in lock(&self.manual).iter() {
            // A typed address that turned out to be a known device is already in the list under its own id.
            if presence.iter().any(|p| p.addrs.contains(&m.addr) && p.session_port == m.session_port) {
                continue;
            }
            // So is one paired through it: its address was learned at the end of the pairing.
            let typed = SocketAddrV4::new(m.addr, m.session_port);
            if lock(&self.learned).values().any(|a| *a == typed) {
                continue;
            }
            out.push(self.manual_view(m));
        }
        out.truncate(swapdesk_ipc::MAX_DEVICES);
        out
    }

    pub fn manual_view(&self, m: &Manual) -> DeviceView {
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
        let presence = self.p.presence.peers();
        let recent = self.p.audit.recent(LAST_SESSION_SCAN);
        match self.p.trust.read(|s| s.find_by_id(id).cloned()) {
            Some(d) => self.paired_view(&d, &presence, &recent),
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
        // Entries without a line are skipped, so read more than `limit` and cut after.
        for e in self.p.audit.recent(limit.saturating_mul(4).max(256)) {
            if before_ms.is_some_and(|b| e.time_ms >= b) {
                continue;
            }
            let device_name = match &e.device {
                DeviceRef::Known(id) => {
                    names.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone()).unwrap_or_else(|| device_id_hex(id))
                }
                DeviceRef::Unknown { addr, .. } => Ipv4Addr::from(*addr).to_string(),
            };
            if let Some((what, alert)) = activity_line(&e, &device_name) {
                out.push(AuditView { at_ms: e.time_ms, device_name, what, duration_s: e.duration_s, alert });
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
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
            DeviceRef::Known(id) => {
                hub.p.trust.read(|s| s.find_by_id(id).map(|d| d.name.clone())).unwrap_or_else(|| device_id_hex(id))
            }
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
