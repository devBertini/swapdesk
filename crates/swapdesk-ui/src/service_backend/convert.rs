//! Conversions between the C6 types of [`crate::api`] and the C2.4 DTOs of `swapdesk-ipc` (and the proto enums
//! they embed). Both directions exist so every pair can be round-trip tested; the backend itself needs the
//! service-to-UI direction for replies and notifications and the UI-to-service direction for commands.
//!
//! Decisions the contract leaves open: a `Msg` travels as the index of its [`MsgKey`] in [`MsgKey::ALL`]
//! (declaration order), so the UI and the service must be built from the same `strings.rs`; an index the UI does
//! not know is a [`ConvertError`] and the message that held it is dropped.

use swapdesk_ipc as w;
use swapdesk_proto as p;

use crate::api as a;
use crate::strings::MsgKey;

/// A wire value the C6 types cannot hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertError {
    /// `Msg.key` is past the last [`MsgKey`].
    UnknownMsgKey(u16),
    /// A reply body only the elevation worker reads (ops 35, 36, 38) came to the general reply path.
    ElevationReply,
}

type CResult<T> = Result<T, ConvertError>;

/// A pair of conversion functions between two enums with the same variant names.
macro_rules! pair_enum {
    ($to:ident, $from:ident, $ui:ty, $wire:ty; $($v:ident),+ $(,)?) => {
        /// The wire twin of a UI value.
        pub fn $to(v: $ui) -> $wire {
            match v { $(<$ui>::$v => <$wire>::$v,)+ }
        }
        /// The UI twin of a wire value.
        pub fn $from(v: $wire) -> $ui {
            match v { $(<$wire>::$v => <$ui>::$v,)+ }
        }
    };
}

pair_enum!(os_to_wire, os_from_wire, a::Os, p::Os; Windows, MacOs);
pair_enum!(intent_to_wire, intent_from_wire, a::Intent, p::Intent; View, Control);
pair_enum!(feature_to_wire, feature_from_wire, a::Feature, p::Feature; Terminal, Files);
pair_enum!(quality_to_wire, quality_from_wire, a::Quality, p::Preset; Auto, Text, Motion);

/// The wire twin of a UI value.
pub fn desktop_to_wire(v: a::DesktopKind) -> p::Desktop {
    match v {
        a::DesktopKind::Default => p::Desktop::Default,
        a::DesktopKind::Secure => p::Desktop::Secure,
        a::DesktopKind::Locked => p::Desktop::Locked,
        a::DesktopKind::AccountAway => p::Desktop::AccountAway,
        a::DesktopKind::LoginWindow => p::Desktop::LoginWindow,
    }
}

/// The UI twin of a wire value. `AccountAway` (protocol minor 1) reads as a locked screen until the interface
/// has a state of its own for an account that left the console.
pub fn desktop_from_wire(v: p::Desktop) -> a::DesktopKind {
    match v {
        p::Desktop::Default => a::DesktopKind::Default,
        p::Desktop::Secure => a::DesktopKind::Secure,
        p::Desktop::Locked => a::DesktopKind::Locked,
        p::Desktop::AccountAway => a::DesktopKind::AccountAway,
        p::Desktop::LoginWindow => a::DesktopKind::LoginWindow,
    }
}

pair_enum!(code_to_wire, code_from_wire, a::ApiCode, w::ApiCode;
    ServiceDown, NotFound, NotPermitted, NeedsAdmin, Busy, Timeout, Refused, Locked, Invalid, Internal);
pair_enum!(ready_to_wire, ready_from_wire, a::Ready, w::Ready; ReadyToReceive, ServiceOff, ServiceNeedsApproval, NoNetwork);
pair_enum!(category_to_wire, category_from_wire, a::NetCategoryView, w::NetCategoryView; Public, Private, Domain);
pair_enum!(os_perm_to_wire, os_perm_from_wire, a::OsPermission, w::OsPermission; ScreenRecording, Accessibility, InputMonitoring);
pair_enum!(direction_to_wire, direction_from_wire, a::Direction, w::Direction; Incoming, Outgoing);
pair_enum!(role_to_wire, role_from_wire, a::Role, w::Role; Viewer, Host);
pair_enum!(language_to_wire, language_from_wire, a::Language, w::Language; PtBr, En);
pair_enum!(theme_to_wire, theme_from_wire, a::Theme, w::Theme; System, Light, Dark);
pair_enum!(keyboard_to_wire, keyboard_from_wire, a::KeyboardMode, w::KeyboardMode; Auto, Physical, Text);
pair_enum!(keys_to_wire, keys_from_wire, a::SpecialKeys, w::SpecialKeys; CtrlAltDel, AltTab, WinOrCmd, AltF4OrCmdQ, PrintScreen);
pair_enum!(swap_why_to_wire, swap_why_from_wire, a::SwapWhy, w::SwapWhy; Declined, TimedOut, NobodyPresent, NotPermitted, Busy);
pair_enum!(connect_why_to_wire, connect_why_from_wire, a::ConnectWhy, w::ConnectWhy;
    Declined, TimedOut, NobodyPresent, Busy, Forgotten, NotPermitted, Unreachable, Failed);

// ---------- Msg ----------

/// The wire `Msg` of a UI `Msg` (key = index in [`MsgKey::ALL`]).
pub fn msg_to_wire(m: &a::Msg) -> w::Msg {
    let key = MsgKey::ALL.iter().position(|k| *k == m.key).unwrap_or(0);
    w::Msg { key: u16::try_from(key).unwrap_or(0), args: m.args.iter().cloned().map(w::Text).collect() }
}

pub fn msg_from_wire(m: &w::Msg) -> CResult<a::Msg> {
    let key = *MsgKey::ALL.get(usize::from(m.key)).ok_or(ConvertError::UnknownMsgKey(m.key))?;
    Ok(a::Msg { key, args: m.args.iter().map(|t| t.0.clone()).collect() })
}

// ---------- DTOs ----------

pub fn this_device_to_wire(v: &a::ThisDevice) -> w::ThisDevice {
    w::ThisDevice {
        name: v.name.clone(),
        os: os_to_wire(v.os),
        ready: ready_to_wire(v.ready),
        legacy_note: v.legacy_note,
    }
}

pub fn this_device_from_wire(v: w::ThisDevice) -> a::ThisDevice {
    a::ThisDevice { name: v.name, os: os_from_wire(v.os), ready: ready_from_wire(v.ready), legacy_note: v.legacy_note }
}

fn notice_to_wire(n: &a::Notice) -> w::Notice {
    match n {
        a::Notice::PublicNetwork => w::Notice::PublicNetwork,
        a::Notice::FirewallBlocks => w::Notice::FirewallBlocks,
        a::Notice::ServiceNeedsApproval => w::Notice::ServiceNeedsApproval,
        a::Notice::MacPermissionsMissing(l) => {
            w::Notice::MacPermissionsMissing(l.iter().copied().map(os_perm_to_wire).collect())
        }
    }
}

fn notice_from_wire(n: w::Notice) -> a::Notice {
    match n {
        w::Notice::PublicNetwork => a::Notice::PublicNetwork,
        w::Notice::FirewallBlocks => a::Notice::FirewallBlocks,
        w::Notice::ServiceNeedsApproval => a::Notice::ServiceNeedsApproval,
        w::Notice::MacPermissionsMissing(l) => {
            a::Notice::MacPermissionsMissing(l.into_iter().map(os_perm_from_wire).collect())
        }
    }
}

pub fn network_to_wire(v: &a::NetworkStatus) -> w::NetworkStatus {
    w::NetworkStatus {
        label: v.label.clone(),
        address: v.address.clone(),
        category: v.category.map(category_to_wire),
        notices: v.notices.iter().map(notice_to_wire).collect(),
    }
}

pub fn network_from_wire(v: w::NetworkStatus) -> a::NetworkStatus {
    a::NetworkStatus {
        label: v.label,
        address: v.address,
        category: v.category.map(category_from_wire),
        notices: v.notices.into_iter().map(notice_from_wire).collect(),
    }
}

fn status_to_wire(s: &a::DeviceStatus) -> w::DeviceStatus {
    match s {
        a::DeviceStatus::Online { address, rtt_ms } => {
            w::DeviceStatus::Online { address: address.clone(), rtt_ms: *rtt_ms }
        }
        a::DeviceStatus::Asleep => w::DeviceStatus::Asleep,
        a::DeviceStatus::Offline => w::DeviceStatus::Offline,
        a::DeviceStatus::OnNetworkUnpaired { pairing_open } => {
            w::DeviceStatus::OnNetworkUnpaired { pairing_open: *pairing_open }
        }
    }
}

fn status_from_wire(s: w::DeviceStatus) -> a::DeviceStatus {
    match s {
        w::DeviceStatus::Online { address, rtt_ms } => a::DeviceStatus::Online { address, rtt_ms },
        w::DeviceStatus::Asleep => a::DeviceStatus::Asleep,
        w::DeviceStatus::Offline => a::DeviceStatus::Offline,
        w::DeviceStatus::OnNetworkUnpaired { pairing_open } => a::DeviceStatus::OnNetworkUnpaired { pairing_open },
    }
}

pub fn device_to_wire(v: &a::DeviceView) -> w::DeviceView {
    w::DeviceView {
        id: v.id.clone(),
        name: v.name.clone(),
        os: os_to_wire(v.os),
        os_version: v.os_version.clone(),
        status: status_to_wire(&v.status),
        paired: v.paired,
        grants_here: v.grants_here.0,
        grants_there: v.grants_there.map(|g| g.0),
        last_session: v.last_session.as_ref().map(|s| w::SessionSummary {
            at_ms: s.at_ms,
            duration_s: s.duration_s,
            direction: direction_to_wire(s.direction),
            desktop: desktop_to_wire(s.desktop),
        }),
        wakeable: v.wakeable,
    }
}

pub fn device_from_wire(v: w::DeviceView) -> a::DeviceView {
    a::DeviceView {
        id: v.id,
        name: v.name,
        os: os_from_wire(v.os),
        os_version: v.os_version,
        status: status_from_wire(v.status),
        paired: v.paired,
        grants_here: a::Permissions(v.grants_here),
        grants_there: v.grants_there.map(a::Permissions),
        last_session: v.last_session.map(|s| a::SessionSummary {
            at_ms: s.at_ms,
            duration_s: s.duration_s,
            direction: direction_from_wire(s.direction),
            desktop: desktop_from_wire(s.desktop),
        }),
        wakeable: v.wakeable,
    }
}

pub fn devices_from_wire(v: Vec<w::DeviceView>) -> Vec<a::DeviceView> {
    v.into_iter().map(device_from_wire).collect()
}

pub fn pairing_to_wire(s: &a::PairingState) -> w::PairingState {
    match s {
        a::PairingState::Open { remaining_s } => w::PairingState::Open { remaining_s: *remaining_s },
        a::PairingState::Waiting => w::PairingState::Waiting,
        a::PairingState::ShowCode { code, peer_name } => {
            w::PairingState::ShowCode { code: w::Text(code.clone()), peer_name: peer_name.clone() }
        }
        a::PairingState::Done { device_id } => w::PairingState::Done { device_id: device_id.clone() },
        a::PairingState::Failed { reason } => w::PairingState::Failed { reason: code_to_wire(*reason) },
    }
}

pub fn pairing_from_wire(s: w::PairingState) -> a::PairingState {
    match s {
        w::PairingState::Open { remaining_s } => a::PairingState::Open { remaining_s },
        w::PairingState::Waiting => a::PairingState::Waiting,
        w::PairingState::ShowCode { code, peer_name } => a::PairingState::ShowCode { code: code.0, peer_name },
        w::PairingState::Done { device_id } => a::PairingState::Done { device_id },
        w::PairingState::Failed { reason } => a::PairingState::Failed { reason: code_from_wire(reason) },
    }
}

pub fn incoming_to_wire(v: &a::IncomingRequest) -> w::IncomingRequest {
    w::IncomingRequest {
        request_id: v.request_id,
        device: device_to_wire(&v.device),
        kind: match v.kind {
            a::RequestKind::Session { intent } => w::RequestKind::Session { intent: intent_to_wire(intent) },
            a::RequestKind::Consent { feature } => w::RequestKind::Consent { feature: feature_to_wire(feature) },
        },
        preset: v.preset.0,
        timeout_s: v.timeout_s,
    }
}

pub fn incoming_from_wire(v: w::IncomingRequest) -> a::IncomingRequest {
    a::IncomingRequest {
        request_id: v.request_id,
        device: device_from_wire(v.device),
        kind: match v.kind {
            w::RequestKind::Session { intent } => a::RequestKind::Session { intent: intent_from_wire(intent) },
            w::RequestKind::Consent { feature } => a::RequestKind::Consent { feature: feature_from_wire(feature) },
        },
        preset: a::Permissions(v.preset),
        timeout_s: v.timeout_s,
    }
}

pub fn session_state_to_wire(s: &a::SessionState) -> w::SessionState {
    match s {
        a::SessionState::Connecting => w::SessionState::Connecting,
        a::SessionState::Prompted { remaining_s } => w::SessionState::Prompted { remaining_s: *remaining_s },
        a::SessionState::Active { since_ms, role, granted, elevation } => w::SessionState::Active {
            since_ms: *since_ms,
            role: role_to_wire(*role),
            granted: granted.0,
            elevation: match elevation {
                a::ElevationLevel::Unavailable => 0,
                a::ElevationLevel::Available => 1,
                a::ElevationLevel::Elevated => 2,
            },
        },
        a::SessionState::Paused { desktop } => w::SessionState::Paused { desktop: desktop_to_wire(*desktop) },
        a::SessionState::Reconnecting { attempt } => w::SessionState::Reconnecting { attempt: *attempt },
        a::SessionState::Waking { remaining_s } => w::SessionState::Waking { remaining_s: *remaining_s },
        a::SessionState::Ended { reason } => w::SessionState::Ended { reason: reason.map(code_to_wire) },
        a::SessionState::NotOpened { why } => w::SessionState::NotOpened { why: connect_why_to_wire(*why) },
    }
}

pub fn session_state_from_wire(s: w::SessionState) -> a::SessionState {
    match s {
        w::SessionState::Connecting => a::SessionState::Connecting,
        w::SessionState::Prompted { remaining_s } => a::SessionState::Prompted { remaining_s },
        w::SessionState::Active { since_ms, role, granted, elevation } => a::SessionState::Active {
            since_ms,
            role: role_from_wire(role),
            granted: a::Permissions(granted),
            elevation: match elevation {
                1 => a::ElevationLevel::Available,
                2 => a::ElevationLevel::Elevated,
                _ => a::ElevationLevel::Unavailable,
            },
        },
        w::SessionState::Paused { desktop } => a::SessionState::Paused { desktop: desktop_from_wire(desktop) },
        w::SessionState::Reconnecting { attempt } => a::SessionState::Reconnecting { attempt },
        w::SessionState::Waking { remaining_s } => a::SessionState::Waking { remaining_s },
        w::SessionState::Ended { reason } => a::SessionState::Ended { reason: reason.map(code_from_wire) },
        w::SessionState::NotOpened { why } => a::SessionState::NotOpened { why: connect_why_from_wire(why) },
    }
}

pub fn display_to_wire(v: &a::DisplayView) -> w::DisplayView {
    w::DisplayView {
        id: v.id,
        name: v.name.clone(),
        width: v.width,
        height: v.height,
        scale_milli: v.scale_milli,
        primary: v.primary,
    }
}

pub fn display_from_wire(v: w::DisplayView) -> a::DisplayView {
    a::DisplayView {
        id: v.id,
        name: v.name,
        width: v.width,
        height: v.height,
        scale_milli: v.scale_milli,
        primary: v.primary,
    }
}

pub fn audit_to_wire(v: &a::AuditView) -> w::AuditView {
    w::AuditView {
        at_ms: v.at_ms,
        device_name: v.device_name.clone(),
        what: msg_to_wire(&v.what),
        duration_s: v.duration_s,
        alert: v.alert,
    }
}

pub fn audit_from_wire(v: w::AuditView) -> CResult<a::AuditView> {
    Ok(a::AuditView {
        at_ms: v.at_ms,
        device_name: v.device_name,
        what: msg_from_wire(&v.what)?,
        duration_s: v.duration_s,
        alert: v.alert,
    })
}

pub fn settings_to_wire(v: &a::Settings) -> w::Settings {
    w::Settings {
        name: v.name.clone(),
        language: language_to_wire(v.language),
        open_at_login: v.open_at_login,
        service_enabled: v.service_enabled,
        lock_on_end: v.lock_on_end,
        keep_awake: v.keep_awake,
        quality: quality_to_wire(v.quality),
        show_stats: v.show_stats,
        keyboard_mode: keyboard_to_wire(v.keyboard_mode),
        swap_ctrl_cmd: v.swap_ctrl_cmd,
        theme: theme_to_wire(v.theme),
    }
}

pub fn settings_from_wire(v: w::Settings) -> a::Settings {
    a::Settings {
        name: v.name,
        language: language_from_wire(v.language),
        open_at_login: v.open_at_login,
        service_enabled: v.service_enabled,
        lock_on_end: v.lock_on_end,
        keep_awake: v.keep_awake,
        quality: quality_from_wire(v.quality),
        show_stats: v.show_stats,
        keyboard_mode: keyboard_from_wire(v.keyboard_mode),
        swap_ctrl_cmd: v.swap_ctrl_cmd,
        theme: theme_from_wire(v.theme),
    }
}

pub fn swap_note_to_wire(e: &a::SwapEvent) -> w::SwapNote {
    match e {
        a::SwapEvent::Asked { request_id, by, remaining_s } => {
            w::SwapNote::Asked { request_id: *request_id, by: by.clone(), remaining_s: *remaining_s }
        }
        a::SwapEvent::Declined { why } => w::SwapNote::Declined { why: swap_why_to_wire(*why) },
        a::SwapEvent::Done => w::SwapNote::Done,
    }
}

pub fn swap_note_from_wire(n: w::SwapNote) -> a::SwapEvent {
    match n {
        w::SwapNote::Asked { request_id, by, remaining_s } => a::SwapEvent::Asked { request_id, by, remaining_s },
        w::SwapNote::Declined { why } => a::SwapEvent::Declined { why: swap_why_from_wire(why) },
        w::SwapNote::Done => a::SwapEvent::Done,
    }
}

/// A message of the other side, not read yet.
pub fn chat_from_wire(item: w::ChatItem, mine: bool) -> a::ChatView {
    a::ChatView { msg_id: item.msg_id, mine, text: item.text.0, sent_at_ms: item.sent_at_ms, read: false }
}

fn chat_to_wire(m: &a::ChatView) -> w::ChatItem {
    w::ChatItem { msg_id: m.msg_id, text: w::Text(m.text.clone()), sent_at_ms: m.sent_at_ms }
}

// ---------- replies and notifications ----------

/// The updater's wire view as the interface keeps it.
pub fn update_from_wire(v: w::UpdateView) -> a::UpdateInfo {
    use a::UpdateFailure as F;
    let phase = match v.phase {
        w::UpdatePhase::Idle => a::UpdatePhase::Idle,
        w::UpdatePhase::Checking => a::UpdatePhase::Checking,
        w::UpdatePhase::Offered { version, manual } => a::UpdatePhase::Offered { version, manual },
        w::UpdatePhase::Downloading { version, done, total } => a::UpdatePhase::Downloading { version, done, total },
        w::UpdatePhase::Verifying { version } => a::UpdatePhase::Verifying { version },
        w::UpdatePhase::Ready { version } => a::UpdatePhase::Installing { version },
        w::UpdatePhase::Failed { reason } => a::UpdatePhase::Failed(match reason {
            0 => F::NotConsole,
            1 => F::NeedsConfirmation,
            2 => F::NotOffered,
            3 => F::Busy,
            4 => F::Download,
            5 => F::Mismatch,
            6 => F::Disk,
            8 => F::Unwritable,
            9 => F::Translocated,
            _ => F::Cancelled,
        }),
    };
    a::UpdateInfo {
        running: v.running,
        phase,
        check_on: v.check_on,
        last_check_ms: v.last_check_ms,
        dismissed: v.dismissed,
        last_failure: v.last_failure.map(|f| match f {
            0 => a::CheckFailure::Network,
            1 => a::CheckFailure::RateLimited,
            _ => a::CheckFailure::Signature,
        }),
        rolled_back: v.rolled_back,
        updated: v.updated,
    }
}

/// The C6 `Reply` of a decoded reply body.
pub fn reply_from_wire(body: w::ReplyBody) -> CResult<a::Reply> {
    Ok(match body {
        w::ReplyBody::Done => a::Reply::Done,
        // The backend's own elevation worker reads these bodies; none of them is a reply of the interface.
        // The elevation worker reads these through its own reply table (`elevation.rs`); C6 has no reply for them.
        w::ReplyBody::ElevationTicket { .. }
        | w::ReplyBody::ElevationAction { .. }
        | w::ReplyBody::ElevationOutcome(_) => return Err(ConvertError::ElevationReply),
        w::ReplyBody::Update(v) => a::Reply::Update(update_from_wire(v)),
        w::ReplyBody::ThisDevice(v) => a::Reply::ThisDevice(this_device_from_wire(v)),
        w::ReplyBody::Network(v) => a::Reply::Network(network_from_wire(v)),
        w::ReplyBody::Devices(v) => a::Reply::Devices(devices_from_wire(v)),
        w::ReplyBody::Device(v) => a::Reply::Device(device_from_wire(v)),
        w::ReplyBody::PairingId(v) => a::Reply::PairingId(v),
        w::ReplyBody::SessionId(v) => a::Reply::SessionId(v),
        w::ReplyBody::RequestId(v) => a::Reply::RequestId(v),
        // The reply to `ChatSend` is this side's own message.
        w::ReplyBody::Chat(v) => a::Reply::Chat(chat_from_wire(v, true)),
        w::ReplyBody::Settings(v) => a::Reply::Settings(settings_from_wire(v)),
        w::ReplyBody::Activity(v) => a::Reply::Activity(v.into_iter().map(audit_from_wire).collect::<CResult<_>>()?),
        w::ReplyBody::Displays(v) => a::Reply::Displays(v.into_iter().map(display_from_wire).collect()),
        // The backend splits an open that was refused into an error before it gets here; this is the id of one that ran.
        w::ReplyBody::TermOpened { term_id, .. } => a::Reply::TermId(term_id),
        w::ReplyBody::Listing(v) => a::Reply::Listing(super::files::listing_from_wire(v)),
        w::ReplyBody::TransferIds(v) => a::Reply::TransferIds(v),
        w::ReplyBody::TransferId(v) => a::Reply::TransferId(v),
        w::ReplyBody::Blocked(v) => a::Reply::Blocked(
            v.into_iter()
                .map(|b| a::BlockedItem {
                    mac: b.mac,
                    last_ipv4: b.last_ipv4,
                    name: b.name,
                    blocked_at_ms: b.blocked_at_ms,
                })
                .collect(),
        ),
    })
}

/// Why a terminal did not open, as the error of the `TermOpen` command: the code the UI already knows, and the
/// message the panel shows in place (`ApiError::detail`).
pub fn term_outcome_error(outcome: w::TermOutcome) -> a::ApiError {
    use w::TermOutcome as O;
    let (code, key) = match outcome {
        O::Opened => (a::ApiCode::Internal, MsgKey::TermFailed),
        O::NotPermitted => (a::ApiCode::NotPermitted, MsgKey::TermFailNotPermitted),
        O::NobodyPresent => (a::ApiCode::Refused, MsgKey::TermFailNobody),
        O::Declined => (a::ApiCode::Refused, MsgKey::TermFailDeclined),
        O::TimedOut => (a::ApiCode::Timeout, MsgKey::TermFailTimedOut),
        O::TooMany => (a::ApiCode::Busy, MsgKey::TermFailTooMany),
        O::Unavailable => (a::ApiCode::NotFound, MsgKey::TermFailUnavailable),
        O::Failed => (a::ApiCode::Internal, MsgKey::TermFailed),
    };
    a::ApiError { code, detail: Some(a::Msg::new(key)) }
}

/// The `UiEvent` a notification becomes.
pub fn notify_to_event(n: w::Notify) -> CResult<a::UiEvent> {
    use a::UiEvent as E;
    Ok(match n {
        w::Notify::UpdateChanged(v) => E::UpdateChanged(update_from_wire(v)),
        w::Notify::UpdateRestarting => E::UpdateRestarting,
        w::Notify::DevicesChanged(v) => E::DevicesChanged(devices_from_wire(v)),
        w::Notify::NetworkChanged(v) => E::NetworkChanged(network_from_wire(v)),
        w::Notify::Pairing { pairing_id, state } => E::Pairing { pairing_id, state: pairing_from_wire(state) },
        w::Notify::IncomingRequest(v) => E::IncomingRequest(incoming_from_wire(v)),
        w::Notify::IncomingClosed { request_id, reason } => {
            E::IncomingClosed { request_id, reason: code_from_wire(reason) }
        }
        w::Notify::Session { session_id, device_id, state } => {
            E::Session { session_id, device_id, state: session_state_from_wire(state) }
        }
        w::Notify::Displays { session_id, list } => {
            E::Displays { session_id, displays: list.into_iter().map(display_from_wire).collect() }
        }
        w::Notify::Stats { session_id, latency_ms, fps, kbps, hevc } => {
            E::Stats { session_id, stats: a::Stats { latency_ms, fps, kbps, hevc } }
        }
        w::Notify::ActivityAlert(v) => E::ActivityAlert(audit_from_wire(v)?),
        w::Notify::SettingsChanged(v) => E::SettingsChanged(settings_from_wire(v)),
        w::Notify::Swap { session_id, note } => E::Swap { session_id, event: swap_note_from_wire(note) },
        w::Notify::Chat { session_id, item } => E::Chat { session_id, message: chat_from_wire(item, false) },
        w::Notify::ChatRead { session_id, up_to } => E::ChatRead { session_id, up_to },
        w::Notify::ClipboardSkipped { session_id, reason } => {
            E::ClipboardSkipped { session_id, reason: code_from_wire(reason) }
        }
        w::Notify::TermExited { term_id, exit_code } => {
            E::Term { term_id, event: a::TermEvent::Exited { code: exit_code } }
        }
        w::Notify::ConsentPending { session_id, feature, remaining_s } => {
            E::ConsentPending { session_id, feature: feature_from_wire(feature), remaining_s }
        }
        w::Notify::Files(n) => super::files::note_to_event(n),
    })
}

/// The notification a service-originated `UiEvent` travels as; `None` for events that never cross the pipe.
pub fn event_to_notify(e: &a::UiEvent) -> Option<w::Notify> {
    use a::UiEvent as E;
    Some(match e {
        E::DevicesChanged(v) => w::Notify::DevicesChanged(v.iter().map(device_to_wire).collect()),
        E::NetworkChanged(v) => w::Notify::NetworkChanged(network_to_wire(v)),
        E::Pairing { pairing_id, state } => {
            w::Notify::Pairing { pairing_id: *pairing_id, state: pairing_to_wire(state) }
        }
        E::IncomingRequest(v) => w::Notify::IncomingRequest(incoming_to_wire(v)),
        E::IncomingClosed { request_id, reason } => {
            w::Notify::IncomingClosed { request_id: *request_id, reason: code_to_wire(*reason) }
        }
        E::Session { session_id, device_id, state } => w::Notify::Session {
            session_id: *session_id,
            device_id: device_id.clone(),
            state: session_state_to_wire(state),
        },
        E::Displays { session_id, displays } => {
            w::Notify::Displays { session_id: *session_id, list: displays.iter().map(display_to_wire).collect() }
        }
        E::Stats { session_id, stats } => w::Notify::Stats {
            session_id: *session_id,
            latency_ms: stats.latency_ms,
            fps: stats.fps,
            kbps: stats.kbps,
            hevc: stats.hevc,
        },
        E::ActivityAlert(v) => w::Notify::ActivityAlert(audit_to_wire(v)),
        E::SettingsChanged(v) => w::Notify::SettingsChanged(settings_to_wire(v)),
        E::Swap { session_id, event } => w::Notify::Swap { session_id: *session_id, note: swap_note_to_wire(event) },
        // Only the other side's messages travel as notifications; this side's own come back as the reply.
        E::Chat { session_id, message } if !message.mine => {
            w::Notify::Chat { session_id: *session_id, item: chat_to_wire(message) }
        }
        E::ChatRead { session_id, up_to } => w::Notify::ChatRead { session_id: *session_id, up_to: *up_to },
        E::ClipboardSkipped { session_id, reason } => {
            w::Notify::ClipboardSkipped { session_id: *session_id, reason: code_to_wire(*reason) }
        }
        E::Term { term_id, event: a::TermEvent::Exited { code } } => {
            w::Notify::TermExited { term_id: *term_id, exit_code: *code }
        }
        E::ConsentPending { session_id, feature, remaining_s } => w::Notify::ConsentPending {
            session_id: *session_id,
            feature: feature_to_wire(*feature),
            remaining_s: *remaining_s,
        },
        E::Transfer { .. } | E::TransferSavedAs { .. } => w::Notify::Files(super::files::event_to_note(e)?),
        _ => return None,
    })
}

/// The quick action on the wire.
pub fn action_to_wire(a: a::ActionKind) -> p::Action {
    match a {
        a::ActionKind::Lock => p::Action::Lock,
        a::ActionKind::Sleep => p::Action::Sleep,
        a::ActionKind::Restart => p::Action::Restart,
        a::ActionKind::TaskMonitor => p::Action::TaskMonitor,
    }
}

// ---------- commands ----------

/// What a [`a::Command`] becomes in the backend.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Sent to the service as this request.
    Op(w::UiOp),
    /// UI-local (`SetScale`, `SetKeyboardMode`, `SetGrab`): answered `Done` at once, the viewer pipeline applies it.
    LocalDone,
    /// No op: answered `Internal` at once.
    Unsupported,
    /// Written to the pipe as these messages (terminal input, cut into chunks), answered `Done` at once.
    Messages(Vec<w::Message>),
    /// The command's arguments cannot be sent (a picked file with no name): answered `Invalid` at once.
    Invalid,
    /// `Elevate`: a worker thread asks for a ticket, runs the platform's elevator, ends the ticket and reports.
    Elevate(a::ElevatedAction, String, u32),
    /// `ActivityExport`: a worker thread lists the activity log and writes the file (the path, the language, the
    /// offset of local time).
    Export(std::path::PathBuf, a::Language, i32),
    /// A command on this computer's disk (`LocalList`, `LocalPlaces` and the slice 2a changes): run by a worker
    /// thread of this process (`local_disk::answer`); nothing about this disk reaches the service (C2.3).
    Local(crate::api::Command),
}

/// Routes a command: the op it maps to, a UI-local `Done`, or "no op yet".
pub fn route(c: &a::Command) -> Route {
    use a::Command as C;
    use w::UiOp as O;
    Route::Op(match c {
        C::GetThisDevice => O::GetThisDevice,
        C::GetNetwork => O::GetNetwork,
        C::ListDevices => O::ListDevices,
        C::AddByAddress { ipv4 } => O::AddByAddress { ipv4: ipv4.clone() },
        C::PairOpen { device_id } => O::PairOpen { device_id: device_id.clone() },
        C::PairStart { pairing_id, device_id } => O::PairStart { pairing_id: *pairing_id, device_id: device_id.clone() },
        C::PairConfirm { pairing_id, matches } => O::PairConfirm { pairing_id: *pairing_id, matches: *matches },
        C::PairCancel { pairing_id } => O::PairCancel { pairing_id: *pairing_id },
        C::SetGrants { device_id, grants } => O::SetGrants { device_id: device_id.clone(), grants: grants.0 },
        C::ForgetDevice { device_id } => O::ForgetDevice { device_id: device_id.clone() },
        C::RenameDevice { device_id, name } => O::RenameDevice { device_id: device_id.clone(), name: name.clone() },
        C::RefreshGrants { device_id } => O::RefreshGrants { device_id: device_id.clone() },
        C::ElevateSession { session_id } => O::ElevateSession { session_id: *session_id },
        C::Connect { device_id, intent, unattended } => {
            O::Connect { device_id: device_id.clone(), intent: intent_to_wire(*intent), unattended: *unattended }
        }
        C::Disconnect { session_id } => O::Disconnect { session_id: *session_id },
        C::AnswerIncoming { request_id, decision } => {
            let (decision, grants) = match decision {
                a::Decision::Accept { grants } => (w::Decision::Accept, grants.0),
                a::Decision::ViewOnly => (w::Decision::ViewOnly, 0),
                a::Decision::Decline => (w::Decision::Decline, 0),
            };
            O::AnswerIncoming { request_id: *request_id, decision, grants }
        }
        C::SelectDisplay { session_id, display_id } => O::SelectDisplay { session_id: *session_id, display: *display_id },
        C::SetQuality { session_id, quality } => O::SetQuality { session_id: *session_id, quality: quality_to_wire(*quality) },
        C::SetAudio { session_id, on } => O::SetAudio { session_id: *session_id, on: *on },
        C::SetViewOnly { session_id, on } => O::SetViewOnly { session_id: *session_id, on: *on },
        C::SendKeys { session_id, keys } => O::SendKeys { session_id: *session_id, keys: keys_to_wire(*keys) },
        C::GetSettings => O::GetSettings,
        C::SetSettings { settings } => O::SetSettings(w::SettingsUpdate {
            name: settings.name.clone(),
            service_enabled: settings.service_enabled,
            lock_on_end: settings.lock_on_end,
            keep_awake: settings.keep_awake,
        }),
        C::ActivityList { before_ms, limit } => O::ActivityList { before_ms: *before_ms, limit: *limit },
        C::RequestSwap { session_id } => O::RequestSwap { session_id: *session_id },
        C::AnswerSwap { request_id, accept } => O::AnswerSwap { request_id: *request_id, accept: *accept },
        C::ChatSend { session_id, text } => O::ChatSend { session_id: *session_id, text: w::Text(text.clone()) },
        // The service marks everything the peer sent as read: `up_to` is only for the UI's own state.
        C::ChatMarkRead { session_id, .. } => O::ChatMarkRead { session_id: *session_id },
        // The UI has no choice of intent for "Acordar": it wakes the device to control it, like "Controlar".
        C::Wake { device_id } => O::Wake { device_id: device_id.clone(), intent: intent_to_wire(a::Intent::Control) },
        C::ListDisplays { session_id } => O::ListDisplays { session_id: *session_id },
        C::SetClipboardSync { session_id, on } => O::SetClipboardSync { session_id: *session_id, on: *on },
        C::RunAction { session_id, action } => O::RunAction { session_id: *session_id, action: action_to_wire(*action) },
        C::TermOpen { session_id, admin, cols, rows } => O::TermOpen { session_id: *session_id, cols: *cols, rows: *rows, admin: *admin },
        C::TermResize { term_id, cols, rows } => O::TermResize { term_id: *term_id, cols: *cols, rows: *rows },
        C::TermClose { term_id } => O::TermClose { term_id: *term_id },
        C::TermWrite { term_id, bytes } => {
            let chunks = bytes.chunks(w::MAX_TERM_CHUNK).map(|c| w::Message::TermInput { term_id: *term_id, bytes: w::Bytes(c.to_vec()) }).collect();
            return Route::Messages(chunks);
        }
        C::FilesList { .. } | C::TransferPause { .. } | C::TransferResume { .. } | C::TransferCancel { .. } => match super::files::op_of(c) {
            Some(op) => op,
            None => return Route::Unsupported,
        },
        C::SetScale { .. } | C::SetKeyboardMode { .. } | C::SetGrab { .. } | C::SetReleaseKey { .. } => {
            return Route::LocalDone;
        }
        C::Elevate { action, line, run } => return Route::Elevate(action.clone(), line.clone(), *run),
        C::ActivityExport { path, language, utc_offset_min } => {
            return Route::Export(path.clone(), *language, *utc_offset_min);
        }
        C::LocalList { .. }
        | C::LocalPlaces
        | C::LocalMakeDir { .. }
        | C::LocalRename { .. }
        | C::LocalMove { .. }
        | C::LocalDeleteForever { .. }
        | C::LocalTrash { .. }
        | C::LocalProps { .. } => return Route::Local(c.clone()),
        C::CancelSwap { .. }
        // `FilesSend` and `FilesGet` need the backend's tokens: `files::route_files` makes their ops.
        | C::FilesSend { .. }
        | C::FilesGet { .. }
        | C::OpenSystemSettings { .. }
        | C::RequestOsPermissions => return Route::Unsupported,
        C::BlockAddress { ipv4 } => O::BlockDevice { ipv4: ipv4.clone(), name: String::new() },
        C::UnblockDevice { mac } => O::UnblockDevice { mac: mac.clone() },
        C::ListBlocked => O::ListBlocked,
        C::GetUpdate => O::UpdateGet,
        C::UpdateCheckNow => O::UpdateCheckNow,
        C::UpdateInstall { version, end_sessions } => {
            O::UpdateInstall { version: version.clone(), end_sessions: *end_sessions }
        }
        C::UpdateCancel => O::UpdateCancel,
        C::UpdateDismiss { version } => O::UpdateDismiss { version: version.clone() },
        C::UpdateSetCheck { on } => O::UpdateSetCheck { on: *on },
        C::MediaCaps { hevc_encode, hevc_decode } => {
            O::MediaCaps { hevc_encode: *hevc_encode, hevc_decode: *hevc_decode }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> a::DeviceView {
        a::DeviceView {
            id: "mac-1".into(),
            name: "MacBook".into(),
            os: a::Os::MacOs,
            os_version: "14.5".into(),
            status: a::DeviceStatus::Online { address: "192.168.0.7".into(), rtt_ms: 4 },
            paired: true,
            grants_here: a::Permissions(0b11),
            grants_there: Some(a::Permissions(1)),
            last_session: Some(a::SessionSummary {
                at_ms: 9,
                duration_s: 60,
                direction: a::Direction::Outgoing,
                desktop: a::DesktopKind::Locked,
            }),
            wakeable: true,
        }
    }

    fn settings() -> a::Settings {
        a::Settings {
            name: "PC".into(),
            language: a::Language::En,
            open_at_login: true,
            service_enabled: true,
            lock_on_end: false,
            keep_awake: true,
            quality: a::Quality::Motion,
            show_stats: true,
            keyboard_mode: a::KeyboardMode::Text,
            swap_ctrl_cmd: true,
            theme: a::Theme::Dark,
        }
    }

    #[test]
    fn dtos_round_trip() {
        let d = device();
        assert_eq!(device_from_wire(device_to_wire(&d)), d);
        for s in [
            a::DeviceStatus::Asleep,
            a::DeviceStatus::Offline,
            a::DeviceStatus::OnNetworkUnpaired { pairing_open: true },
        ] {
            assert_eq!(status_from_wire(status_to_wire(&s)), s);
        }
        let n = a::NetworkStatus {
            label: "Casa".into(),
            address: Some("192.168.0.2".into()),
            category: Some(a::NetCategoryView::Domain),
            notices: vec![
                a::Notice::PublicNetwork,
                a::Notice::MacPermissionsMissing(vec![a::OsPermission::Accessibility]),
            ],
        };
        assert_eq!(network_from_wire(network_to_wire(&n)), n);
        let t = a::ThisDevice { name: "PC".into(), os: a::Os::Windows, ready: a::Ready::NoNetwork, legacy_note: true };
        assert_eq!(this_device_from_wire(this_device_to_wire(&t)), t);
        let s = settings();
        assert_eq!(settings_from_wire(settings_to_wire(&s)), s);
        let v = a::DisplayView { id: 2, name: "D".into(), width: 1920, height: 1080, scale_milli: 1500, primary: true };
        assert_eq!(display_from_wire(display_to_wire(&v)), v);
        let audit = a::AuditView {
            at_ms: 5,
            device_name: "X".into(),
            what: a::Msg::new(MsgKey::ALL[3]).with("1.2.3.4"),
            duration_s: Some(3),
            alert: true,
        };
        assert_eq!(audit_from_wire(audit_to_wire(&audit)).unwrap(), audit);
    }

    #[test]
    fn session_pairing_and_incoming_round_trip() {
        for s in [
            a::SessionState::Connecting,
            a::SessionState::Prompted { remaining_s: 9 },
            a::SessionState::Active {
                since_ms: 1,
                role: a::Role::Host,
                granted: a::Permissions(3),
                elevation: Default::default(),
            },
            a::SessionState::Paused { desktop: a::DesktopKind::Secure },
            a::SessionState::Reconnecting { attempt: 2 },
            a::SessionState::Waking { remaining_s: 30 },
            a::SessionState::Ended { reason: Some(a::ApiCode::Busy) },
            a::SessionState::Ended { reason: None },
        ] {
            assert_eq!(session_state_from_wire(session_state_to_wire(&s)), s);
        }
        for s in [
            a::PairingState::Open { remaining_s: 120 },
            a::PairingState::Waiting,
            a::PairingState::ShowCode { code: "123456".into(), peer_name: "Mac".into() },
            a::PairingState::Done { device_id: "m".into() },
            a::PairingState::Failed { reason: a::ApiCode::Refused },
        ] {
            assert_eq!(pairing_from_wire(pairing_to_wire(&s)), s);
        }
        for kind in [
            a::RequestKind::Session { intent: a::Intent::Control },
            a::RequestKind::Consent { feature: a::Feature::Files },
        ] {
            let r =
                a::IncomingRequest { request_id: 7, device: device(), kind, preset: a::Permissions(1), timeout_s: 30 };
            assert_eq!(incoming_from_wire(incoming_to_wire(&r)), r);
        }
    }

    #[test]
    fn every_api_code_and_status_round_trip() {
        for c in w::ApiCode::ALL {
            let ui = code_from_wire(*c);
            assert_eq!(code_to_wire(ui), *c);
            assert_eq!(w::status_code(c.status()).unwrap(), Some(*c));
        }
    }

    #[test]
    fn notifications_round_trip_through_events() {
        let events = vec![
            a::UiEvent::DevicesChanged(vec![device()]),
            a::UiEvent::Stats { session_id: 4, stats: a::Stats { latency_ms: 5, fps: 60, kbps: 9000, hevc: false } },
            a::UiEvent::Session { session_id: 4, device_id: "mac-1".into(), state: a::SessionState::Connecting },
            a::UiEvent::SettingsChanged(settings()),
            a::UiEvent::IncomingClosed { request_id: 2, reason: a::ApiCode::Timeout },
            a::UiEvent::Swap {
                session_id: 4,
                event: a::SwapEvent::Asked { request_id: 3, by: "MacBook".into(), remaining_s: 20 },
            },
            a::UiEvent::Swap { session_id: 4, event: a::SwapEvent::Declined { why: a::SwapWhy::NobodyPresent } },
            a::UiEvent::Swap { session_id: 4, event: a::SwapEvent::Done },
            a::UiEvent::Chat {
                session_id: 4,
                message: a::ChatView { msg_id: 7, mine: false, text: "oi".into(), sent_at_ms: 9, read: false },
            },
            a::UiEvent::ChatRead { session_id: 4, up_to: 7 },
            a::UiEvent::ClipboardSkipped { session_id: 4, reason: a::ApiCode::Invalid },
        ];
        for e in events {
            let n = event_to_notify(&e).expect("crosses the pipe");
            assert_eq!(notify_to_event(n).unwrap(), e);
        }
        assert!(event_to_notify(&a::UiEvent::ThemeChanged { dark: true }).is_none());
        let mine = a::ChatView { msg_id: 1, mine: true, text: "x".into(), sent_at_ms: 1, read: false };
        assert!(
            event_to_notify(&a::UiEvent::Chat { session_id: 1, message: mine }).is_none(),
            "an own message is a reply, not a notification"
        );
    }

    #[test]
    fn a_not_opened_entry_round_trips_for_every_why() {
        for why in w::ConnectWhy::ALL {
            let wire = w::SessionState::NotOpened { why: *why };
            let ui = session_state_from_wire(wire);
            assert_eq!(ui, a::SessionState::NotOpened { why: connect_why_from_wire(*why) });
            assert_eq!(session_state_to_wire(&ui), wire);
        }
    }

    #[test]
    fn unknown_msg_key_is_an_error() {
        let bad = w::Msg { key: u16::MAX, args: vec![] };
        assert_eq!(msg_from_wire(&bad), Err(ConvertError::UnknownMsgKey(u16::MAX)));
    }

    #[test]
    fn commands_route_to_their_ops() {
        use a::Command as C;
        assert_eq!(route(&C::ListDevices), Route::Op(w::UiOp::ListDevices));
        assert_eq!(
            route(&C::SetGrants { device_id: "d".into(), grants: a::Permissions(5) }),
            Route::Op(w::UiOp::SetGrants { device_id: "d".into(), grants: 5 })
        );
        assert_eq!(
            route(&C::AnswerIncoming { request_id: 3, decision: a::Decision::Accept { grants: a::Permissions(3) } }),
            Route::Op(w::UiOp::AnswerIncoming { request_id: 3, decision: w::Decision::Accept, grants: 3 })
        );
        assert_eq!(
            route(&C::SetQuality { session_id: 1, quality: a::Quality::Text }),
            Route::Op(w::UiOp::SetQuality { session_id: 1, quality: p::Preset::Text })
        );
        assert_eq!(route(&C::SetScale { session_id: 1, scale: a::Scale::Fit }), Route::LocalDone);
        assert_eq!(
            route(&C::Wake { device_id: "d".into() }),
            Route::Op(w::UiOp::Wake { device_id: "d".into(), intent: p::Intent::Control })
        );
        assert_eq!(route(&C::RequestSwap { session_id: 4 }), Route::Op(w::UiOp::RequestSwap { session_id: 4 }));
        assert_eq!(
            route(&C::AnswerSwap { request_id: 2, accept: true }),
            Route::Op(w::UiOp::AnswerSwap { request_id: 2, accept: true })
        );
        assert_eq!(
            route(&C::ChatSend { session_id: 4, text: "oi".into() }),
            Route::Op(w::UiOp::ChatSend { session_id: 4, text: w::Text("oi".into()) })
        );
        assert_eq!(
            route(&C::ChatMarkRead { session_id: 4, up_to: 9 }),
            Route::Op(w::UiOp::ChatMarkRead { session_id: 4 })
        );
        assert_eq!(
            route(&C::CancelSwap { request_id: 1 }),
            Route::Unsupported,
            "the swap library has no cancel and the UI no control for it"
        );
        assert_eq!(route(&C::RequestOsPermissions), Route::Unsupported);
        assert_eq!(route(&C::ListDisplays { session_id: 4 }), Route::Op(w::UiOp::ListDisplays { session_id: 4 }));
        assert_eq!(
            route(&C::SetClipboardSync { session_id: 4, on: false }),
            Route::Op(w::UiOp::SetClipboardSync { session_id: 4, on: false })
        );
        assert_eq!(
            route(&C::RunAction { session_id: 4, action: a::ActionKind::Restart }),
            Route::Op(w::UiOp::RunAction { session_id: 4, action: p::Action::Restart })
        );
    }
}
