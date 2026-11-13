//! The in-process UI API of contract C6: commands, replies, events, the DTOs they carry and the backend traits.
//!
//! Typed Rust only: no serialization. The shell's UI thread calls [`UiBackend::send`], which never blocks; results and
//! service notifications come back as [`UiEvent`]s, drained on the UI thread and applied to
//! [`UiState`](crate::state::UiState).

use std::path::PathBuf;

use crate::strings::MsgKey;

// ---------- plumbing ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ticket(pub u64);

pub trait UiBackend: Send + Sync {
    /// Never blocks: queues `command` for the backend thread. Its result arrives as `UiEvent::Reply { ticket, .. }`.
    fn send(&self, command: Command) -> Ticket;
    /// Drains the events queued since the last call (called on the UI thread after a wake-up).
    fn drain(&self) -> Vec<UiEvent>;
}

/// Implemented by each shell: wakes its UI thread from any thread.
pub trait UiWaker: Send + Sync {
    fn wake(&self);
}

// ---------- DTOs ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: ApiCode,
    pub detail: Option<Msg>,
}

impl ApiError {
    pub fn new(code: ApiCode) -> ApiError {
        ApiError { code, detail: None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiCode {
    ServiceDown,
    NotFound,
    NotPermitted,
    NeedsAdmin,
    Busy,
    Timeout,
    Refused,
    Locked,
    Invalid,
    Internal,
    /// The other computer's volume has too little free space for a file (F-FILE-03).
    NoSpace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Windows,
    MacOs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    View,
    Control,
    Audio,
    Terminal,
    AdminTerminal,
    Files,
    Clipboard,
    Unattended,
}

impl Permission {
    pub const ALL: [Permission; 8] = [
        Permission::View,
        Permission::Control,
        Permission::Audio,
        Permission::Terminal,
        Permission::AdminTerminal,
        Permission::Files,
        Permission::Clipboard,
        Permission::Unattended,
    ];

    pub const fn bit(self) -> u16 {
        match self {
            Permission::View => 1 << 0,
            Permission::Control => 1 << 1,
            Permission::Audio => 1 << 2,
            Permission::Terminal => 1 << 3,
            Permission::AdminTerminal => 1 << 4,
            Permission::Files => 1 << 5,
            Permission::Clipboard => 1 << 6,
            Permission::Unattended => 1 << 7,
        }
    }

    /// Needs a local administrator to grant (D7).
    pub const fn needs_admin(self) -> bool {
        matches!(self, Permission::AdminTerminal | Permission::Unattended)
    }
}

/// The PROTOCOL.md 6.1 bit set: Control, Audio and Unattended imply View; AdminTerminal implies Terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Permissions(pub u16);

impl Permissions {
    pub const NONE: Permissions = Permissions(0);

    pub fn has(self, p: Permission) -> bool {
        self.0 & p.bit() != 0
    }

    /// Adds `p` and what it implies.
    pub fn with(self, p: Permission) -> Permissions {
        let mut bits = self.0 | p.bit();
        if matches!(p, Permission::Control | Permission::Audio | Permission::Unattended) {
            bits |= Permission::View.bit();
        }
        if p == Permission::AdminTerminal {
            bits |= Permission::Terminal.bit();
        }
        Permissions(bits)
    }

    /// Removes `p` and everything that depends on it.
    pub fn without(self, p: Permission) -> Permissions {
        let mut bits = self.0 & !p.bit();
        if p == Permission::View {
            bits &= !(Permission::Control.bit() | Permission::Audio.bit() | Permission::Unattended.bit());
        }
        if p == Permission::Terminal {
            bits &= !Permission::AdminTerminal.bit();
        }
        Permissions(bits)
    }

    pub fn toggled(self, p: Permission) -> Permissions {
        if self.has(p) { self.without(p) } else { self.with(p) }
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThisDevice {
    pub name: String,
    pub os: Os,
    pub ready: Ready,
    /// The one-time note under Pareados that pairings made before 1.1 do not carry over (D55).
    pub legacy_note: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ready {
    ReadyToReceive,
    ServiceOff,
    ServiceNeedsApproval,
    NoNetwork,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkStatus {
    pub label: String,
    pub address: Option<String>,
    pub category: Option<NetCategoryView>,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetCategoryView {
    Public,
    Private,
    Domain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    PublicNetwork,
    FirewallBlocks,
    ServiceNeedsApproval,
    MacPermissionsMissing(Vec<OsPermission>),
    /// macOS has not given this Mac access to the receiving folder (F-FILE-03): the folder's display name.
    ReceiveFolderDenied(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsPermission {
    ScreenRecording,
    Accessibility,
    InputMonitoring,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceStatus {
    Online { address: String, rtt_ms: u32 },
    Asleep,
    Offline,
    OnNetworkUnpaired { pairing_open: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub os: Os,
    pub os_version: String,
    pub status: DeviceStatus,
    pub paired: bool,
    pub grants_here: Permissions,
    pub grants_there: Option<Permissions>,
    pub last_session: Option<SessionSummary>,
    pub wakeable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub at_ms: u64,
    pub duration_s: u32,
    pub direction: Direction,
    pub desktop: DesktopKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopKind {
    Default,
    Secure,
    Locked,
    LoginWindow,
    /// The session's account is not at the other computer's console (ACCOUNTS.md 5.1).
    AccountAway,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingState {
    Open { remaining_s: u16 },
    Waiting,
    ShowCode { code: String, peer_name: String },
    Done { device_id: String },
    Failed { reason: ApiCode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Terminal,
    Files,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    Session { intent: Intent },
    Consent { feature: Feature },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingRequest {
    pub request_id: u32,
    pub device: DeviceView,
    pub kind: RequestKind,
    pub preset: Permissions,
    pub timeout_s: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    View,
    Control,
}

/// D8: never `Unattended` or `AdminTerminal` here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Accept { grants: Permissions },
    ViewOnly,
    Decline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevatedAction {
    SetGrants {
        device_id: String,
        grants: Permissions,
    },
    /// "Elevar controle" in a viewer session (ACCOUNTS.md 9).
    ElevateSession {
        session_id: u64,
    },
    OpenAdminTerminal {
        session_id: u64,
    },
}

/// Whether a viewer session can be elevated (ACCOUNTS.md 9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ElevationLevel {
    /// Entered with a prompt, toward a 1.0 host, or a host session.
    #[default]
    Unavailable,
    /// Entered without a prompt: "Elevar controle" is offered.
    Available,
    /// Elevated: it sees and controls the secure desktops.
    Elevated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    Prompted {
        remaining_s: u16,
    },
    Active {
        since_ms: u64,
        role: Role,
        granted: Permissions,
        elevation: ElevationLevel,
    },
    Paused {
        desktop: DesktopKind,
    },
    Reconnecting {
        attempt: u32,
    },
    Waking {
        remaining_s: u16,
    },
    /// `None`: a normal end (either side disconnected).
    Ended {
        reason: Option<ApiCode>,
    },
    /// The last state of a connect entry whose session never opened, with why.
    NotOpened {
        why: ConnectWhy,
    },
}

/// Why a connect did not open a session: who refused, or what did not answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectWhy {
    /// The other computer's user said no.
    Declined,
    /// Nobody answered within the other computer's 20 s.
    TimedOut,
    /// Nobody is at the other computer.
    NobodyPresent,
    /// The other computer is in another session.
    Busy,
    /// The other computer no longer knows this one.
    Forgotten,
    /// The other computer does not allow this connection.
    NotPermitted,
    /// The other computer did not answer over the network.
    Unreachable,
    /// Anything else (a broken exchange, a version it does not speak).
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Host,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayView {
    pub id: u16,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale_milli: u16,
    pub primary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    Fit,
    OneToOne,
    Scroll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Auto,
    Text,
    Motion,
}

impl Quality {
    /// Every preset, in the order the selects list them.
    pub const ALL: [Quality; 3] = [Quality::Auto, Quality::Text, Quality::Motion];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardMode {
    Auto,
    Physical,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialKeys {
    CtrlAltDel,
    AltTab,
    WinOrCmd,
    AltF4OrCmdQ,
    PrintScreen,
}

impl SpecialKeys {
    /// Every chord, in the order the "send keys" menu lists them.
    pub const ALL: [SpecialKeys; 5] = [
        SpecialKeys::CtrlAltDel,
        SpecialKeys::AltTab,
        SpecialKeys::WinOrCmd,
        SpecialKeys::AltF4OrCmdQ,
        SpecialKeys::PrintScreen,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub latency_ms: u32,
    pub fps: u16,
    pub kbps: u32,
    /// The pictures come in HEVC (else H.264), F-VIEW-07.
    pub hevc: bool,
}

/// Why a swap this side asked for did not happen (F-SWAP-02): the card says which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapWhy {
    /// The other user said no.
    Declined,
    /// Nobody answered within 20 s.
    TimedOut,
    /// Nobody is at the other computer.
    NobodyPresent,
    /// The side that would be controlled has not granted `CONTROL` to the other.
    NotPermitted,
    /// A swap is already under way on this session.
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapEvent {
    Asked {
        request_id: u32,
        by: String,
        remaining_s: u16,
    },
    Declined {
        why: SwapWhy,
    },
    /// Close the swap card: the swap is under way, or the question is no longer waiting.
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    Opened { account: String },
    Exited { code: Option<i32> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryView {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub path: String,
    pub entries: Vec<DirEntryView>,
    pub truncated: bool,
}

// ---------- the other computer's volumes and changes (FILES-UI.md slice 2b) ----------

/// What an entry of the other computer is; a link is the entry itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    File,
    Dir,
    Link,
}

/// One entry of a page of the other computer's folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: RemoteKind,
    pub size: u64,
    pub mtime_ms: u64,
    pub hidden: bool,
    pub system: bool,
    pub read_only: bool,
}

/// A page of a folder of the other computer (`Command::FilesListPage`): `path` as the host spells it (a 1.1 path),
/// `next` the start of the following page, `free` the free bytes of its volume when the host told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePage {
    pub path: String,
    pub start: u32,
    pub entries: Vec<RemoteEntry>,
    pub next: Option<u32>,
    pub free: Option<u64>,
}

/// A well-known folder of the other computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceName {
    Desktop,
    Documents,
    Downloads,
    Pictures,
    Music,
    Videos,
    Received,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlace {
    pub kind: PlaceName,
    /// A 1.1 path of the other computer.
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeType {
    System,
    Fixed,
    Removable,
    Network,
    Optical,
    Image,
}

/// A volume the other computer publishes: its paths start with `:<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVolume {
    pub id: u32,
    pub kind: VolumeType,
    pub label: String,
    /// The Windows drive letter.
    pub letter: Option<char>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub has_bin: bool,
}

/// The other computer's home folder (a 1.1 path), places and volumes (`Command::FilesVolumes`, and pushed as
/// `UiEvent::RemoteVolumes` when they change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVolumes {
    pub home: String,
    pub places: Vec<RemotePlace>,
    pub volumes: Vec<RemoteVolume>,
}

/// A change of the other computer's files; needs `FILES` and `CONTROL` there (FILES-UI.md 3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteChange {
    MakeDir { parent: String, name: String },
    Rename { dir: String, name: String, new_name: String },
    Move { from_dir: String, names: Vec<String>, to_dir: String },
    Trash { dir: String, names: Vec<String> },
}

/// What happened to one name of a `RemoteChange`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOutcome {
    Done,
    /// The new name is taken; nothing is ever replaced.
    Exists,
    /// That place has no bin; nothing was deleted.
    NoBin,
    /// The volume is no longer there.
    VolumeGone,
    /// A name the rules refuse, a link on the way, or a folder moved into itself.
    InvalidName,
    /// The system refused, or a protected location.
    NotPermitted,
    NotFound,
    NoSpace,
    Failed,
}

/// A folder of this computer, as the explorer's left pane lists it (`Command::LocalList`): hidden entries left out,
/// What a properties count found inside a path of this computer (slice 2a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProps {
    pub path: PathBuf,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    /// The count stopped at `local_disk::MAX_PROPS_ENTRIES`.
    pub truncated: bool,
    /// The free space of the volume it is on, when the system says.
    pub free_bytes: Option<u64>,
}

/// not sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalListing {
    pub path: PathBuf,
    pub entries: Vec<DirEntryView>,
    pub truncated: bool,
    /// The hidden entries (dot files, the system's hidden flag), apart: the pane shows them when asked (FILES-UI.md
    /// 2.5). The transfer engine's own side files are never listed.
    pub hidden: Vec<DirEntryView>,
}

/// What a place of the explorer's navigation pane is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaceKind {
    Home,
    Desktop,
    Documents,
    Downloads,
    /// A drive (Windows: its letter as `label`) or a volume (macOS: its name).
    Volume,
}

/// One place or volume of this computer (`Command::LocalPlaces`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPlace {
    pub kind: PlaceKind,
    /// The volume's own name or letter; empty for a place (the interface names it in its language).
    pub label: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Running { done: u64, total: u64, bytes_per_s: u64 },
    Paused { done: u64, total: u64, why: ApiCode },
    Done,
    Failed { reason: ApiCode },
}

#[derive(Clone, PartialEq, Eq)]
pub struct ChatView {
    pub msg_id: u32,
    pub mine: bool,
    pub text: String,
    pub sent_at_ms: u64,
    pub read: bool,
}

/// Chat text is never logged (CONTRACTS.md C2.3): `Debug` shows its size only.
impl core::fmt::Debug for ChatView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChatView")
            .field("msg_id", &self.msg_id)
            .field("mine", &self.mine)
            .field("text", &swapdesk_proto::RedactedLen(self.text.len()))
            .field("sent_at_ms", &self.sent_at_ms)
            .field("read", &self.read)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Lock,
    Sleep,
    Restart,
    TaskMonitor,
}

/// Convention for `what`: `args[0]` is the peer's address for the unknown-device alert, the device name or a count for
/// the other lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditView {
    pub at_ms: u64,
    pub device_name: String,
    pub what: Msg,
    pub duration_s: Option<u32>,
    pub alert: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub name: String,
    pub language: Language,
    pub open_at_login: bool,
    pub service_enabled: bool,
    pub lock_on_end: bool,
    pub keep_awake: bool,
    pub quality: Quality,
    pub show_stats: bool,
    pub keyboard_mode: KeyboardMode,
    pub swap_ctrl_cmd: bool,
    pub theme: Theme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    PtBr,
    En,
}

/// The theme chosen in Ajustes; `System` follows the OS (live).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTarget {
    NetworkCategory,
    Accessibility,
    InputMonitoring,
    ScreenRecording,
    LoginItems,
}

/// A string key with its arguments; `strings::text(Language, &Msg)` translates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub key: MsgKey,
    pub args: Vec<String>,
}

impl Msg {
    pub fn new(key: MsgKey) -> Msg {
        Msg { key, args: Vec::new() }
    }

    pub fn with(mut self, arg: impl Into<String>) -> Msg {
        self.args.push(arg.into());
        self
    }
}

impl From<MsgKey> for Msg {
    fn from(key: MsgKey) -> Msg {
        Msg::new(key)
    }
}

// ---------- commands ----------

#[derive(Clone, PartialEq)]
pub enum Command {
    GetThisDevice,
    GetNetwork,
    ListDevices,
    AddByAddress {
        ipv4: String,
    },
    PairOpen {
        device_id: Option<String>,
    },
    PairStart {
        pairing_id: u32,
        device_id: String,
    },
    PairConfirm {
        pairing_id: u32,
        matches: bool,
    },
    PairCancel {
        pairing_id: u32,
    },
    SetGrants {
        device_id: String,
        grants: Permissions,
    },
    ForgetDevice {
        device_id: String,
    },
    /// A local name for a paired device (the alias this machine shows; the peer is not told).
    RenameDevice {
        device_id: String,
        name: String,
    },
    /// Op 39: Permissões of this device opened: the service refreshes what the other side allows (at most once per
    /// 30 s per device).
    RefreshGrants {
        device_id: String,
    },
    /// Op 50: "Elevar controle" in viewer session `session_id`, after the confirmation on this machine.
    ElevateSession {
        session_id: u64,
    },
    Elevate {
        action: ElevatedAction,
        /// What the step shows, in the interface's language: the Mac's system dialog shows it as its extra text.
        line: String,
        /// Which press of "Continuar" this run answers: a result of an earlier run never lands on a later step.
        run: u32,
    },
    Connect {
        device_id: String,
        intent: Intent,
        unattended: bool,
    },
    Disconnect {
        session_id: u64,
    },
    AnswerIncoming {
        request_id: u32,
        decision: Decision,
    },
    ListDisplays {
        session_id: u64,
    },
    SelectDisplay {
        session_id: u64,
        display_id: u16,
    },
    SetScale {
        session_id: u64,
        scale: Scale,
    },
    SetQuality {
        session_id: u64,
        quality: Quality,
    },
    /// F-VIEW-06: the video area of the viewer window in physical pixels (0x0 in 1:1 mode: the host sends native) and
    /// the refresh rate of the monitor it is on. The host encodes at that size; a change is reported once.
    SetViewSize {
        session_id: u64,
        width: u16,
        height: u16,
        max_fps: u16,
    },
    SetAudio {
        session_id: u64,
        on: bool,
    },
    SetKeyboardMode {
        session_id: u64,
        mode: KeyboardMode,
    },
    SetGrab {
        session_id: u64,
        on: bool,
    },
    /// The chord that releases the keyboard (UI-local: the viewer pipeline applies it).
    SetReleaseKey {
        key: crate::shortcuts::ReleaseKey,
    },
    SetViewOnly {
        session_id: u64,
        on: bool,
    },
    SendKeys {
        session_id: u64,
        keys: SpecialKeys,
    },
    RequestSwap {
        session_id: u64,
    },
    AnswerSwap {
        request_id: u32,
        accept: bool,
    },
    CancelSwap {
        request_id: u32,
    },
    TermOpen {
        session_id: u64,
        admin: bool,
        cols: u16,
        rows: u16,
    },
    TermWrite {
        term_id: u32,
        bytes: Vec<u8>,
    },
    TermResize {
        term_id: u32,
        cols: u16,
        rows: u16,
    },
    TermClose {
        term_id: u32,
    },
    RunAction {
        session_id: u64,
        action: ActionKind,
    },
    FilesList {
        session_id: u64,
        path: Option<String>,
    },
    FilesSend {
        session_id: u64,
        local_paths: Vec<PathBuf>,
        remote_dir: Option<String>,
    },
    FilesGet {
        session_id: u64,
        remote_path: String,
        local_dir: PathBuf,
    },
    /// Slice 2b: a page of a folder of the other computer (a 1.1 path, empty for the home folder) from entry `start`.
    /// Answered `RemotePage`; `Invalid` at once when the other computer runs 1.0.
    FilesListPage {
        session_id: u64,
        path: String,
        start: u32,
    },
    /// Slice 2b: a change of the other computer's files. Answered `ChangeResults` (one per name) after the host's
    /// answer; `Invalid` when the other computer runs 1.0.
    FilesChange {
        session_id: u64,
        change: RemoteChange,
    },
    /// Slice 2b: the other computer's home folder, places and volumes. Answered `RemoteVolumes`; `Invalid` when the
    /// other computer runs 1.0, which is how the explorer learns it.
    FilesVolumes {
        session_id: u64,
    },
    TransferPause {
        transfer_id: u32,
    },
    TransferResume {
        transfer_id: u32,
    },
    TransferCancel {
        transfer_id: u32,
    },
    SetClipboardSync {
        session_id: u64,
        on: bool,
    },
    ChatSend {
        session_id: u64,
        text: String,
    },
    ChatMarkRead {
        session_id: u64,
        up_to: u32,
    },
    Wake {
        device_id: String,
    },
    ActivityList {
        before_ms: Option<u64>,
        limit: u32,
    },
    /// The explorer's left pane (FILES-UI.md, slice 1): a folder of this computer, listed by the UI process on a
    /// worker thread (`None`: the default folder, Downloads). Answered `LocalListing`.
    LocalList {
        path: Option<PathBuf>,
    },
    /// This computer's places and drives or volumes for the navigation pane. Answered `LocalPlaces`.
    LocalPlaces,
    /// Which of `paths` are folders (slice 4: files dropped from the system's file manager are stat-ed on a worker
    /// thread, never on the UI thread). Answered `LocalKinds`; a path that cannot be read is left out.
    LocalKinds {
        paths: Vec<PathBuf>,
    },
    /// Creates the folder `path` on this computer (slice 2a). `Refused`: something has that name already.
    LocalMakeDir {
        path: PathBuf,
    },
    /// Renames `path` to `name` in its folder. `Invalid`: a name the systems refuse; `Refused`: the name is taken.
    LocalRename {
        path: PathBuf,
        name: String,
    },
    /// Moves `paths` into the folder `into` (cut and paste, also across volumes). `Invalid`: a folder into itself;
    /// `Refused`: a name is taken there (nothing is replaced).
    LocalMove {
        paths: Vec<PathBuf>,
        into: PathBuf,
    },
    /// Deletes `paths` for good, after the person confirmed (Shift+Delete).
    LocalDeleteForever {
        paths: Vec<PathBuf>,
    },
    /// Sends `paths` to the system's bin (Delete). `Refused`: the volume has no bin and nothing was deleted.
    LocalTrash {
        paths: Vec<PathBuf>,
    },
    /// Counts what is inside `path` (`Reply::LocalProps`); a newer properties request makes the older reply moot.
    LocalProps {
        path: PathBuf,
    },
    /// Writes the activity log to `path` as CSV, in this interface's language and local time (`UiState::activity_export`
    /// fills both). The UI process writes the file; the service only lists the entries.
    ActivityExport {
        path: PathBuf,
        language: Language,
        utc_offset_min: i32,
    },
    /// Blocks the device now behind `ipv4` by its MAC (D65).
    BlockAddress {
        ipv4: String,
    },
    /// Unblocks a MAC (`aa:bb:cc:dd:ee:ff`) of the blocked list.
    UnblockDevice {
        mac: String,
    },
    /// The devices blocked by MAC (`Reply::Blocked`).
    ListBlocked,
    GetSettings,
    SetSettings {
        settings: Settings,
    },
    OpenSystemSettings {
        target: SettingsTarget,
    },
    RequestOsPermissions,
    /// The machine's updater (UPDATES.md 10, ops 51 to 56).
    GetUpdate,
    UpdateCheckNow,
    UpdateInstall {
        version: String,
        /// "Encerrar e atualizar" was pressed (a live session ends).
        end_sessions: bool,
    },
    UpdateCancel,
    /// "Agora não" (or closing the rollback card) for this version.
    UpdateDismiss {
        version: String,
    },
    UpdateSetCheck {
        on: bool,
    },
    /// What this machine's GPU does in hardware, probed by the interface at start (F-VIEW-07): the service offers and
    /// picks HEVC only when it is told so.
    MediaCaps {
        hevc_encode: bool,
        hevc_decode: bool,
    },
}

/// Typed chat text and terminal input are never logged (CONTRACTS.md C2.3): `Debug` shows their size only. Written
/// by hand because a derive would print them.
impl core::fmt::Debug for Command {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        macro_rules! dbg_fields {
            ($name:literal $(, $field:ident)*) => {
                f.debug_struct($name)$(.field(stringify!($field), $field))*.finish()
            };
        }
        match self {
            Self::GetThisDevice => f.write_str("GetThisDevice"),
            Self::GetNetwork => f.write_str("GetNetwork"),
            Self::ListDevices => f.write_str("ListDevices"),
            Self::AddByAddress { ipv4 } => dbg_fields!("AddByAddress", ipv4),
            Self::PairOpen { device_id } => dbg_fields!("PairOpen", device_id),
            Self::PairStart { pairing_id, device_id } => dbg_fields!("PairStart", pairing_id, device_id),
            Self::PairConfirm { pairing_id, matches } => dbg_fields!("PairConfirm", pairing_id, matches),
            Self::PairCancel { pairing_id } => dbg_fields!("PairCancel", pairing_id),
            Self::SetGrants { device_id, grants } => dbg_fields!("SetGrants", device_id, grants),
            Self::ForgetDevice { device_id } => dbg_fields!("ForgetDevice", device_id),
            Self::RenameDevice { device_id, name } => dbg_fields!("RenameDevice", device_id, name),
            Self::RefreshGrants { device_id } => dbg_fields!("RefreshGrants", device_id),
            Self::ElevateSession { session_id } => dbg_fields!("ElevateSession", session_id),
            Self::Elevate { action, run, .. } => dbg_fields!("Elevate", action, run),
            Self::Connect { device_id, intent, unattended } => dbg_fields!("Connect", device_id, intent, unattended),
            Self::Disconnect { session_id } => dbg_fields!("Disconnect", session_id),
            Self::AnswerIncoming { request_id, decision } => dbg_fields!("AnswerIncoming", request_id, decision),
            Self::ListDisplays { session_id } => dbg_fields!("ListDisplays", session_id),
            Self::SelectDisplay { session_id, display_id } => dbg_fields!("SelectDisplay", session_id, display_id),
            Self::SetScale { session_id, scale } => dbg_fields!("SetScale", session_id, scale),
            Self::SetQuality { session_id, quality } => dbg_fields!("SetQuality", session_id, quality),
            Self::SetViewSize { session_id, width, height, max_fps } => {
                dbg_fields!("SetViewSize", session_id, width, height, max_fps)
            }
            Self::SetAudio { session_id, on } => dbg_fields!("SetAudio", session_id, on),
            Self::SetKeyboardMode { session_id, mode } => dbg_fields!("SetKeyboardMode", session_id, mode),
            Self::SetGrab { session_id, on } => dbg_fields!("SetGrab", session_id, on),
            Self::SetReleaseKey { key } => dbg_fields!("SetReleaseKey", key),
            Self::SetViewOnly { session_id, on } => dbg_fields!("SetViewOnly", session_id, on),
            Self::SendKeys { session_id, keys } => dbg_fields!("SendKeys", session_id, keys),
            Self::RequestSwap { session_id } => dbg_fields!("RequestSwap", session_id),
            Self::AnswerSwap { request_id, accept } => dbg_fields!("AnswerSwap", request_id, accept),
            Self::CancelSwap { request_id } => dbg_fields!("CancelSwap", request_id),
            Self::TermOpen { session_id, admin, cols, rows } => dbg_fields!("TermOpen", session_id, admin, cols, rows),
            Self::TermResize { term_id, cols, rows } => dbg_fields!("TermResize", term_id, cols, rows),
            Self::TermClose { term_id } => dbg_fields!("TermClose", term_id),
            Self::RunAction { session_id, action } => dbg_fields!("RunAction", session_id, action),
            Self::FilesList { session_id, path } => dbg_fields!("FilesList", session_id, path),
            Self::FilesSend { session_id, local_paths, remote_dir } => {
                dbg_fields!("FilesSend", session_id, local_paths, remote_dir)
            }
            Self::FilesGet { session_id, remote_path, local_dir } => {
                dbg_fields!("FilesGet", session_id, remote_path, local_dir)
            }
            Self::FilesListPage { session_id, path, start } => dbg_fields!("FilesListPage", session_id, path, start),
            Self::FilesChange { session_id, change } => dbg_fields!("FilesChange", session_id, change),
            Self::FilesVolumes { session_id } => dbg_fields!("FilesVolumes", session_id),
            Self::TransferPause { transfer_id } => dbg_fields!("TransferPause", transfer_id),
            Self::TransferResume { transfer_id } => dbg_fields!("TransferResume", transfer_id),
            Self::TransferCancel { transfer_id } => dbg_fields!("TransferCancel", transfer_id),
            Self::SetClipboardSync { session_id, on } => dbg_fields!("SetClipboardSync", session_id, on),
            Self::ChatMarkRead { session_id, up_to } => dbg_fields!("ChatMarkRead", session_id, up_to),
            Self::Wake { device_id } => dbg_fields!("Wake", device_id),
            Self::ActivityList { before_ms, limit } => dbg_fields!("ActivityList", before_ms, limit),
            Self::LocalList { path } => dbg_fields!("LocalList", path),
            Self::LocalMakeDir { path } => dbg_fields!("LocalMakeDir", path),
            Self::LocalRename { path, name } => dbg_fields!("LocalRename", path, name),
            Self::LocalMove { paths, into } => dbg_fields!("LocalMove", paths, into),
            Self::LocalDeleteForever { paths } => dbg_fields!("LocalDeleteForever", paths),
            Self::LocalTrash { paths } => dbg_fields!("LocalTrash", paths),
            Self::LocalProps { path } => dbg_fields!("LocalProps", path),
            Self::LocalPlaces => f.write_str("LocalPlaces"),
            Self::LocalKinds { paths } => dbg_fields!("LocalKinds", paths),
            Self::ActivityExport { path, language, utc_offset_min } => {
                dbg_fields!("ActivityExport", path, language, utc_offset_min)
            }
            Self::BlockAddress { ipv4 } => dbg_fields!("BlockAddress", ipv4),
            Self::UnblockDevice { mac } => dbg_fields!("UnblockDevice", mac),
            Self::ListBlocked => f.write_str("ListBlocked"),
            Self::GetSettings => f.write_str("GetSettings"),
            Self::SetSettings { settings } => dbg_fields!("SetSettings", settings),
            Self::OpenSystemSettings { target } => dbg_fields!("OpenSystemSettings", target),
            Self::RequestOsPermissions => f.write_str("RequestOsPermissions"),
            Self::GetUpdate => f.write_str("GetUpdate"),
            Self::UpdateCheckNow => f.write_str("UpdateCheckNow"),
            Self::UpdateInstall { version, end_sessions } => dbg_fields!("UpdateInstall", version, end_sessions),
            Self::UpdateCancel => f.write_str("UpdateCancel"),
            Self::UpdateDismiss { version } => dbg_fields!("UpdateDismiss", version),
            Self::UpdateSetCheck { on } => dbg_fields!("UpdateSetCheck", on),
            Self::MediaCaps { hevc_encode, hevc_decode } => dbg_fields!("MediaCaps", hevc_encode, hevc_decode),
            Self::TermWrite { term_id, bytes } => f
                .debug_struct("TermWrite")
                .field("term_id", term_id)
                .field("bytes", &swapdesk_proto::RedactedLen(bytes.len()))
                .finish(),
            Self::ChatSend { session_id, text } => f
                .debug_struct("ChatSend")
                .field("session_id", session_id)
                .field("text", &swapdesk_proto::RedactedLen(text.len()))
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Done,
    ThisDevice(ThisDevice),
    Network(NetworkStatus),
    Devices(Vec<DeviceView>),
    Device(DeviceView),
    PairingId(u32),
    SessionId(u64),
    RequestId(u32),
    TermId(u32),
    Displays(Vec<DisplayView>),
    Listing(Listing),
    TransferIds(Vec<u32>),
    TransferId(u32),
    RemotePage(RemotePage),
    ChangeResults(Vec<(String, ChangeOutcome)>),
    RemoteVolumes(RemoteVolumes),
    Chat(ChatView),
    Activity(Vec<AuditView>),
    Settings(Settings),
    MissingPermissions(Vec<OsPermission>),
    Update(UpdateInfo),
    LocalListing(LocalListing),
    /// What `LocalProps` counted.
    LocalProps(LocalProps),
    LocalPlaces(Vec<LocalPlace>),
    /// What `LocalKinds` found: each readable path with whether it is a folder.
    LocalKinds(Vec<(PathBuf, bool)>),
    Blocked(Vec<BlockedItem>),
}

/// A device blocked by its MAC (D65), as Ajustes lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedItem {
    pub mac: String,
    pub last_ipv4: String,
    pub name: String,
    pub blocked_at_ms: u64,
}

// ---------- updates (UPDATES.md 10) ----------

/// Where the machine's updater is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePhase {
    Idle,
    Checking,
    /// `manual`: this version must be installed by hand.
    Offered {
        version: String,
        manual: bool,
    },
    Downloading {
        version: String,
        done: u64,
        total: u64,
    },
    Verifying {
        version: String,
    },
    /// Verified and handed to the installer: Swapdesk closes and opens again.
    Installing {
        version: String,
    },
    Failed(UpdateFailure),
}

/// Why an install failed (the service's `InstallFailure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateFailure {
    NotConsole,
    NeedsConfirmation,
    NotOffered,
    Busy,
    Download,
    Mismatch,
    Disk,
    Cancelled,
    /// Mac: only an administrator can replace the app where it is.
    Unwritable,
    /// Mac: the app runs from a translocated copy.
    Translocated,
}

/// Why the last "Procurar agora" failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckFailure {
    Network,
    RateLimited,
    Signature,
}

/// The machine's updater as the interface shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// The running version.
    pub running: String,
    pub phase: UpdatePhase,
    pub check_on: bool,
    pub last_check_ms: Option<u64>,
    /// The card is hidden for this version ("Agora não").
    pub dismissed: Option<String>,
    pub last_failure: Option<CheckFailure>,
    /// The version that did not start and was replaced by the running one.
    pub rolled_back: Option<String>,
    /// The running version came by an update at this start ("Swapdesk atualizado para X", once).
    pub updated: bool,
}

// ---------- events ----------

#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    Reply {
        ticket: Ticket,
        result: Result<Reply, ApiError>,
    },
    DevicesChanged(Vec<DeviceView>),
    NetworkChanged(NetworkStatus),
    Pairing {
        pairing_id: u32,
        state: PairingState,
    },
    IncomingRequest(IncomingRequest),
    IncomingClosed {
        request_id: u32,
        reason: ApiCode,
    },
    Session {
        session_id: u64,
        device_id: String,
        state: SessionState,
    },
    Displays {
        session_id: u64,
        displays: Vec<DisplayView>,
    },
    Stats {
        session_id: u64,
        stats: Stats,
    },
    Swap {
        session_id: u64,
        event: SwapEvent,
    },
    Chat {
        session_id: u64,
        message: ChatView,
    },
    ChatRead {
        session_id: u64,
        up_to: u32,
    },
    Term {
        term_id: u32,
        event: TermEvent,
    },
    Transfer {
        transfer_id: u32,
        session_id: u64,
        name: String,
        outgoing: bool,
        state: TransferState,
    },
    /// A received file was saved under `name`, not the offered one: that name was taken and files never overwrite.
    TransferSavedAs {
        transfer_id: u32,
        session_id: u64,
        name: String,
    },
    /// Slice 2b: the other computer's volumes changed (a disk came or went).
    RemoteVolumes {
        session_id: u64,
        volumes: RemoteVolumes,
    },
    /// This computer hosts `session_id` and the viewer changed files here, for the banner's line (FILES-UI.md 6):
    /// `op` is the audit's (0 new folder, 1 rename, 2 move, 3 to the bin), `count` the names done. Never a name.
    HostFilesChanged {
        session_id: u64,
        op: u8,
        count: u32,
    },
    /// This computer hosts `session_id` and the viewer reached a volume outside the home folder (sent whenever a new
    /// kind of volume is reached; the banner says it once per session).
    HostFilesReach {
        session_id: u64,
    },
    ClipboardSkipped {
        session_id: u64,
        reason: ApiCode,
    },
    /// The host is asking its user whether this viewer session may use `feature` (D13), for `remaining_s` more
    /// seconds; the answer arrives as the outcome of the request that caused the question.
    ConsentPending {
        session_id: u64,
        feature: Feature,
        remaining_s: u16,
    },
    ActivityAlert(AuditView),
    SettingsChanged(Settings),
    /// The link to the service broke and is open again (a restarted service, WAITING.md 4 row 23): what the old
    /// service held is gone, and the interface asks for its data again.
    ServiceReconnected,
    ElevationResult {
        action: ElevatedAction,
        /// The `run` of the `Elevate` this answers.
        run: u32,
        result: Result<(), ApiError>,
    },
    ThemeChanged {
        dark: bool,
    },
    LanguageChanged(Language),
    /// The updater changed (UPDATES.md 10).
    UpdateChanged(UpdateInfo),
    /// An update is being installed: the interface saves its window state and closes (UPDATES.md 6.2 step 4).
    UpdateRestarting,
}

impl Command {
    /// Reads and "set to this value" commands: running one twice has the effect of running it once, so a backend may
    /// merge identical ones that are in flight (R-UI-3). Text sent to a peer, terminal input and transfers are not.
    pub fn is_idempotent(&self) -> bool {
        matches!(
            self,
            Command::GetThisDevice
                | Command::GetNetwork
                | Command::ListDevices
                | Command::GetSettings
                | Command::ListBlocked
                | Command::GetUpdate
                | Command::UpdateSetCheck { .. }
                | Command::ActivityList { .. }
                | Command::ListDisplays { .. }
                | Command::FilesList { .. }
                | Command::FilesListPage { .. }
                | Command::FilesVolumes { .. }
                | Command::LocalList { .. }
                | Command::LocalProps { .. }
                | Command::LocalPlaces
                | Command::LocalKinds { .. }
                | Command::SetGrants { .. }
                | Command::SetSettings { .. }
                | Command::SelectDisplay { .. }
                | Command::SetScale { .. }
                | Command::SetQuality { .. }
                | Command::SetViewSize { .. }
                | Command::SetAudio { .. }
                | Command::SetKeyboardMode { .. }
                | Command::SetGrab { .. }
                | Command::SetReleaseKey { .. }
                | Command::SetViewOnly { .. }
                | Command::SetClipboardSync { .. }
                | Command::OpenSystemSettings { .. }
        )
    }

    /// How long the UI waits for the reply before it shows a failure (R-UI-7).
    pub fn timeout_s(&self) -> u16 {
        match self {
            // A UAC or password prompt waits for a person.
            Command::Elevate { .. } => 120,
            // Both answer at once with the entry's id; the wait itself is told by notifications, and the interface
            // ends an entry that hears nothing for `CONNECT_GIVE_UP_S`.
            // The host's runner may wait on the machine (a suspend); the service gives up at 25 s.
            Command::RunAction { .. } => 30,
            // The service waits 10 s for the host's answer.
            Command::ElevateSession { .. } => 15,
            // The host's user may be asked first (D13: 20 s), and the shell then starts.
            // The same for the first listing of the files (D13), which then lists the folder.
            Command::TermOpen { .. }
            | Command::FilesList { .. }
            | Command::FilesListPage { .. }
            | Command::FilesVolumes { .. } => 25,
            // The service waits 30 s for the host's answer (the host's user may be asked first, D13).
            Command::FilesChange { .. } => 35,
            Command::ActivityList { .. } | Command::ActivityExport { .. } => 10,
            // A slow volume (a network share, a sleeping disk) fails visibly instead of freezing the pane (R-UI-7).
            Command::LocalList { .. }
            | Command::LocalPlaces
            | Command::LocalKinds { .. }
            | Command::LocalMakeDir { .. }
            | Command::LocalRename { .. }
            | Command::LocalMove { .. }
            | Command::LocalDeleteForever { .. }
            | Command::LocalTrash { .. }
            | Command::LocalProps { .. } => 8,
            _ => 5,
        }
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_of_a_command_shows_the_size_of_typed_text_and_terminal_input_and_never_the_content() {
        let chat = format!("{:?}", Command::ChatSend { session_id: 2, text: "segredo-do-chat".into() });
        assert!(!chat.contains("segredo") && chat.contains("<redacted 15 bytes>") && chat.contains("session_id: 2"));
        let term = format!("{:?}", Command::TermWrite { term_id: 1, bytes: b"senha-no-shell".to_vec() });
        assert!(!term.contains("senha") && term.contains("<redacted 14 bytes>"), "{term}");
        let bytes = format!("{:?}", Command::TermWrite { term_id: 1, bytes: vec![233; 4] });
        assert!(!bytes.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "{bytes}");
        // The other commands print as before.
        assert_eq!(format!("{:?}", Command::GetThisDevice), "GetThisDevice");
        assert!(format!("{:?}", Command::Disconnect { session_id: 9 }).contains("session_id: 9"));
    }

    #[test]
    fn debug_of_a_chat_view_shows_its_size_and_never_the_text() {
        let view = ChatView { msg_id: 1, mine: false, text: "segredo-do-chat".into(), sent_at_ms: 2, read: true };
        let shown = format!("{view:?}");
        assert!(!shown.contains("segredo"), "chat text leaked: {shown}");
        assert!(shown.contains("<redacted 15 bytes>"), "{shown}");
    }
}
