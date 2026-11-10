//! Plain DTOs of the UI replies and notifications. They mirror the C6 shapes of `swapdesk-ui` field by field
//! (this crate must not depend on that one), with the string and list maxima of the wire.

use swapdesk_proto::{CodecError, Desktop, Feature, Intent, Os, Preset, Reader, Wire, Writer};

use crate::macros::unknown_tag;
use crate::redact::Text;

type CResult<T> = Result<T, CodecError>;

/// Longest `device_id`, device name, OS version and peer name (C5 maximum).
pub const MAX_NAME: usize = 64;
/// A typed address: an IPv4 literal, optionally followed by `:port` ("255.255.255.255:65535").
pub const MAX_IPV4: usize = 21;
/// Longest address text (IPv6 with scope fits).
pub const MAX_ADDRESS: usize = 45;
/// Longest network label.
pub const MAX_LABEL: usize = 128;
/// Longest pairing code text.
pub const MAX_CODE: usize = 16;
/// Largest device list a reply or notification holds.
pub const MAX_DEVICES: usize = 256;
/// Largest activity page a reply holds.
pub const MAX_ACTIVITY: usize = 1000;
/// Largest display list.
pub const MAX_DISPLAYS: usize = 16;
/// Most arguments of a `Msg`.
pub const MAX_MSG_ARGS: usize = 4;
/// Longest `Msg` argument.
pub const MAX_MSG_ARG: usize = 128;
/// Most notices on a network status.
pub const MAX_NOTICES: usize = 8;
/// Longest chat text in bytes (PROTOCOL.md 5.3 `ChatMessage.text`).
pub const MAX_CHAT_TEXT: usize = 4096;
/// How long a Wake-on-LAN attempt waits for the device to appear (F-WAKE-01), the first `Waking { remaining_s }`.
pub const WAKE_WINDOW_S: u16 = 60;

wire_enum! {
    /// Why a local operation failed; on the wire `UiReply.status` is the tag plus one.
    ApiCode {
        ServiceDown = 0, NotFound = 1, NotPermitted = 2, NeedsAdmin = 3, Busy = 4,
        Timeout = 5, Refused = 6, Locked = 7, Invalid = 8, Internal = 9,
    }
}

wire_enum! {
    /// `Hello.role`.
    IpcRole {
        Ui = 1,
        HelperSystem = 2,
        HelperUser = 3,
        /// The elevated `Swapdesk.exe --elevated` instance that confirms one administrator action.
        Elevator = 4,
    }
}

wire_enum! {
    /// `StartVideo.backend`.
    VideoBackend { Auto = 0, Nvenc = 1, Mf = 2 }
}

wire_enum! {
    /// `AudioMode.mode`.
    AudioModeKind { Off = 0, Loopback = 1, ProcessExclusion = 2 }
}

wire_enum! {
    Ready { ReadyToReceive = 0, ServiceOff = 1, ServiceNeedsApproval = 2, NoNetwork = 3 }
}

wire_enum! {
    NetCategoryView { Public = 0, Private = 1, Domain = 2 }
}

wire_enum! {
    OsPermission { ScreenRecording = 0, Accessibility = 1, InputMonitoring = 2 }
}

wire_enum! {
    Direction { Incoming = 0, Outgoing = 1 }
}

wire_enum! {
    Role { Viewer = 0, Host = 1 }
}

wire_enum! {
    Language { PtBr = 0, En = 1 }
}

wire_enum! {
    /// The theme chosen in Ajustes: `System` follows the OS.
    Theme { System = 0, Light = 1, Dark = 2 }
}

wire_enum! {
    KeyboardMode { Auto = 0, Physical = 1, Text = 2 }
}

wire_enum! {
    /// `AnswerIncoming.decision`. Never `Unattended` or `AdminTerminal` here (D8).
    Decision { Accept = 0, ViewOnly = 1, Decline = 2 }
}

wire_enum! {
    /// `SendKeys.keys`.
    SpecialKeys { CtrlAltDel = 0, AltTab = 1, WinOrCmd = 2, AltF4OrCmdQ = 3, PrintScreen = 4 }
}

wire_enum! {
    /// How an elevation ticket ended, in the reply of `ElevationEnd`.
    ElevationOutcome {
        /// The administrator confirmed in the elevated window.
        Confirmed = 0,
        /// The administrator pressed Cancel in the elevated window.
        Declined = 1,
        /// Never confirmed; the ticket is closed by this call.
        Open = 2,
        /// Expired, already ended, or never existed (also what a ticket of another connection answers).
        Unknown = 3,
    }
}

/// What an elevation ticket is for: the action the administrator confirms and the service lets through once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevAction {
    /// Set `grants` (the raw bit set of PROTOCOL.md 6.1, validated by the service) of a paired device.
    SetGrants { device_id: String, grants: u16 },
    // Tags 1 and 2 are unassigned: an unknown tag is a codec error.
}

impl Wire for ElevAction {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::SetGrants { device_id, grants } => {
                w.put_u8(0);
                w.put_str(device_id, MAX_NAME)?;
                w.put_u16(*grants);
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        match r.u8()? {
            0 => Ok(Self::SetGrants { device_id: r.str(MAX_NAME)?, grants: r.u16()? }),
            t => Err(unknown_tag("ElevAction", t)),
        }
    }
}

impl ApiCode {
    /// The `UiReply.status` of an error: the tag plus one (0 is ok).
    pub const fn status(self) -> u16 {
        self as u16 + 1
    }
}

wire_struct! {
    ThisDevice { name: String [MAX_NAME], os: Os, ready: Ready }
}

/// A notice on the network status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    PublicNetwork,
    FirewallBlocks,
    ServiceNeedsApproval,
    MacPermissionsMissing(Vec<OsPermission>),
}

impl Wire for Notice {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::PublicNetwork => w.put_u8(0),
            Self::FirewallBlocks => w.put_u8(1),
            Self::ServiceNeedsApproval => w.put_u8(2),
            Self::MacPermissionsMissing(list) => {
                w.put_u8(3);
                list.put(w, OsPermission::ALL.len())?;
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        match r.u8()? {
            0 => Ok(Self::PublicNetwork),
            1 => Ok(Self::FirewallBlocks),
            2 => Ok(Self::ServiceNeedsApproval),
            3 => Ok(Self::MacPermissionsMissing(<Vec<OsPermission> as Wire>::get(r, OsPermission::ALL.len())?)),
            t => Err(unknown_tag("Notice", t)),
        }
    }
}

wire_struct! {
    NetworkStatus {
        label: String [MAX_LABEL],
        address: Option<String> [MAX_ADDRESS],
        category: Option<NetCategoryView>,
        notices: Vec<Notice> [MAX_NOTICES],
    }
}

/// Reachability of a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceStatus {
    Online { address: String, rtt_ms: u32 },
    Asleep,
    Offline,
    OnNetworkUnpaired { pairing_open: bool },
}

impl Wire for DeviceStatus {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::Online { address, rtt_ms } => {
                w.put_u8(0);
                w.put_str(address, MAX_ADDRESS)?;
                w.put_u32(*rtt_ms);
            }
            Self::Asleep => w.put_u8(1),
            Self::Offline => w.put_u8(2),
            Self::OnNetworkUnpaired { pairing_open } => {
                w.put_u8(3);
                w.put_bool(*pairing_open);
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        match r.u8()? {
            0 => Ok(Self::Online { address: r.str(MAX_ADDRESS)?, rtt_ms: r.u32()? }),
            1 => Ok(Self::Asleep),
            2 => Ok(Self::Offline),
            3 => Ok(Self::OnNetworkUnpaired { pairing_open: r.bool()? }),
            t => Err(unknown_tag("DeviceStatus", t)),
        }
    }
}

wire_struct! {
    SessionSummary { at_ms: u64, duration_s: u32, direction: Direction, desktop: Desktop }
}

wire_struct! {
    /// `grants_*` are the PROTOCOL.md 6.1 bit set, carried as the raw `u16` of C6 `Permissions`.
    DeviceView {
        id: String [MAX_NAME],
        name: String [MAX_NAME],
        os: Os,
        os_version: String [MAX_NAME],
        status: DeviceStatus,
        paired: bool,
        grants_here: u16,
        grants_there: Option<u16>,
        last_session: Option<SessionSummary>,
        wakeable: bool,
    }
}

/// State of a pairing. The code is the SAS: its `Debug` prints a length only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingState {
    Open { remaining_s: u16 },
    Waiting,
    ShowCode { code: Text, peer_name: String },
    Done { device_id: String },
    Failed { reason: ApiCode },
}

impl Wire for PairingState {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::Open { remaining_s } => {
                w.put_u8(0);
                w.put_u16(*remaining_s);
            }
            Self::Waiting => w.put_u8(1),
            Self::ShowCode { code, peer_name } => {
                w.put_u8(2);
                code.put(w, MAX_CODE)?;
                w.put_str(peer_name, MAX_NAME)?;
            }
            Self::Done { device_id } => {
                w.put_u8(3);
                w.put_str(device_id, MAX_NAME)?;
            }
            Self::Failed { reason } => {
                w.put_u8(4);
                reason.put(w, 0)?;
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        match r.u8()? {
            0 => Ok(Self::Open { remaining_s: r.u16()? }),
            1 => Ok(Self::Waiting),
            2 => Ok(Self::ShowCode { code: Text::get(r, MAX_CODE)?, peer_name: r.str(MAX_NAME)? }),
            3 => Ok(Self::Done { device_id: r.str(MAX_NAME)? }),
            4 => Ok(Self::Failed { reason: ApiCode::get(r, 0)? }),
            t => Err(unknown_tag("PairingState", t)),
        }
    }
}

/// What an incoming request asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Session { intent: Intent },
    Consent { feature: Feature },
}

impl Wire for RequestKind {
    const MIN_SIZE: usize = 2;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::Session { intent } => {
                w.put_u8(0);
                intent.put(w, 0)
            }
            Self::Consent { feature } => {
                w.put_u8(1);
                feature.put(w, 0)
            }
        }
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        match r.u8()? {
            0 => Ok(Self::Session { intent: Intent::get(r, 0)? }),
            1 => Ok(Self::Consent { feature: Feature::get(r, 0)? }),
            t => Err(unknown_tag("RequestKind", t)),
        }
    }
}

wire_struct! {
    IncomingRequest {
        request_id: u32,
        device: DeviceView,
        kind: RequestKind,
        preset: u16,
        timeout_s: u16,
    }
}

/// State of a session as the UI shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    Prompted {
        remaining_s: u16,
    },
    Active {
        since_ms: u64,
        role: Role,
        granted: u16,
    },
    Paused {
        desktop: Desktop,
    },
    Reconnecting {
        attempt: u32,
    },
    Waking {
        remaining_s: u16,
    },
    /// `reason: None` is a normal end (either side disconnected); `Some` names what ended it.
    Ended {
        reason: Option<ApiCode>,
    },
}

impl Wire for SessionState {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::Connecting => w.put_u8(0),
            Self::Prompted { remaining_s } => {
                w.put_u8(1);
                w.put_u16(*remaining_s);
            }
            Self::Active { since_ms, role, granted } => {
                w.put_u8(2);
                w.put_u64(*since_ms);
                role.put(w, 0)?;
                w.put_u16(*granted);
            }
            Self::Paused { desktop } => {
                w.put_u8(3);
                desktop.put(w, 0)?;
            }
            Self::Reconnecting { attempt } => {
                w.put_u8(4);
                w.put_u32(*attempt);
            }
            Self::Waking { remaining_s } => {
                w.put_u8(5);
                w.put_u16(*remaining_s);
            }
            Self::Ended { reason } => {
                w.put_u8(6);
                reason.put(w, 0)?;
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        match r.u8()? {
            0 => Ok(Self::Connecting),
            1 => Ok(Self::Prompted { remaining_s: r.u16()? }),
            2 => Ok(Self::Active { since_ms: r.u64()?, role: Role::get(r, 0)?, granted: r.u16()? }),
            3 => Ok(Self::Paused { desktop: Desktop::get(r, 0)? }),
            4 => Ok(Self::Reconnecting { attempt: r.u32()? }),
            5 => Ok(Self::Waking { remaining_s: r.u16()? }),
            6 => Ok(Self::Ended { reason: Option::<ApiCode>::get(r, 0)? }),
            t => Err(unknown_tag("SessionState", t)),
        }
    }
}

wire_struct! {
    DisplayView {
        id: u16,
        name: String [MAX_NAME],
        width: u32,
        height: u32,
        scale_milli: u16,
        primary: bool,
    }
}

wire_struct! {
    Stats { latency_ms: u32, fps: u16, kbps: u32 }
}

/// A translatable message: the key of a string of `swapdesk-ui` with its arguments (which may hold anything,
/// so `Debug` prints only their lengths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub key: u16,
    pub args: Vec<Text>,
}

impl Wire for Msg {
    const MIN_SIZE: usize = 3;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        if self.args.len() > MAX_MSG_ARGS {
            return Err(CodecError::OverLimit { what: "vector count", limit: MAX_MSG_ARGS as u64 });
        }
        w.put_u16(self.key);
        w.put_varint(self.args.len() as u64)?;
        for a in &self.args {
            a.put(w, MAX_MSG_ARG)?;
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        let key = r.u16()?;
        let count = r.count(MAX_MSG_ARGS, 1)?;
        let mut args = Vec::with_capacity(count);
        for _ in 0..count {
            args.push(Text::get(r, MAX_MSG_ARG)?);
        }
        Ok(Self { key, args })
    }
}

wire_struct! {
    AuditView {
        at_ms: u64,
        device_name: String [MAX_NAME],
        what: Msg,
        duration_s: Option<u32>,
        alert: bool,
    }
}

wire_struct! {
    /// The settings of C6. `SetSettings` carries only the machine fields (`SettingsUpdate`); the rest stay in the UI.
    Settings {
        name: String [MAX_NAME],
        language: Language,
        open_at_login: bool,
        service_enabled: bool,
        lock_on_end: bool,
        keep_awake: bool,
        quality: Preset,
        show_stats: bool,
        keyboard_mode: KeyboardMode,
        swap_ctrl_cmd: bool,
        /// Appended after the first version of the struct (UI and service come from one build; tags never renumber).
        theme: Theme,
    }
}

wire_struct! {
    /// The machine fields of `SetSettings`.
    SettingsUpdate {
        name: String [MAX_NAME],
        service_enabled: bool,
        lock_on_end: bool,
        keep_awake: bool,
    }
}

wire_enum! {
    /// Why a swap this machine asked for did not happen (`SwapNote::Declined`).
    SwapWhy { Declined = 0, TimedOut = 1, NobodyPresent = 2, NotPermitted = 3, Busy = 4 }
}

/// What the swap card of a session shows (`Notify::Swap`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapNote {
    /// The other side asks this one: `by` is its name, `remaining_s` the countdown to the automatic decline.
    Asked { request_id: u32, by: String, remaining_s: u16 },
    /// This machine's own request did not happen.
    Declined { why: SwapWhy },
    /// Close the swap card: the swap is under way, or the question is no longer waiting.
    Done,
}

impl Wire for SwapNote {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        match self {
            Self::Asked { request_id, by, remaining_s } => {
                w.put_u8(0);
                w.put_u32(*request_id);
                w.put_str(by, MAX_NAME)?;
                w.put_u16(*remaining_s);
            }
            Self::Declined { why } => {
                w.put_u8(1);
                why.put(w, 0)?;
            }
            Self::Done => w.put_u8(2),
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        match r.u8()? {
            0 => Ok(Self::Asked { request_id: r.u32()?, by: r.str(MAX_NAME)?, remaining_s: r.u16()? }),
            1 => Ok(Self::Declined { why: SwapWhy::get(r, 0)? }),
            2 => Ok(Self::Done),
            t => Err(unknown_tag("SwapNote", t)),
        }
    }
}

wire_struct! {
    /// A chat message as the UI shows it. The text is as private as input: its `Debug` prints a length only.
    ChatItem { msg_id: u32, text: Text [MAX_CHAT_TEXT], sent_at_ms: u64 }
}
