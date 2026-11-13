//! The view model of the interface: pure, owned by the UI thread, changed only by [`UiState::apply`] (events from the
//! backend) and [`UiState::activate`] (what the user does). The shells read it and draw it.

use std::collections::HashMap;

use crate::api::*;
use crate::format::Clock;
use crate::interact::ControlId;
use crate::strings::{self, MsgKey};
use crate::tokens::layout::{CONNECT_GIVE_UP_S, PAIR_COUNTDOWN_S, WAKE_COUNTDOWN_S};

/// Which windows must redraw after a change (a bit set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Changed(u8);

impl Changed {
    pub const NONE: Changed = Changed(0);
    pub const MAIN: Changed = Changed(1);
    pub const VIEWER: Changed = Changed(2);
    pub const BANNER: Changed = Changed(4);
    pub const ALL: Changed = Changed(7);

    pub const fn contains(self, other: Changed) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Changed) -> Changed {
        Changed(self.0 | other.0)
    }
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
    /// True when the window of `kind` must redraw.
    pub const fn hits(self, kind: WindowKind) -> bool {
        match kind {
            WindowKind::Main => self.0 & 1 != 0,
            WindowKind::Viewer => self.0 & 2 != 0,
            WindowKind::Banner => self.0 & 4 != 0,
        }
    }
}

/// The windows the shell may show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowKind {
    Main,
    Viewer,
    Banner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Nav {
    /// "Início": the home screen (D35).
    Home,
    Devices,
    Activity,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceTab {
    Overview,
    Permissions,
    History,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DrawerTab {
    Terminal,
    Files,
    Chat,
    Clipboard,
}

/// What the viewer's terminal is doing, for the drawer (F-TERM-01, F-TERM-02). One terminal per viewer window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermPhase {
    /// Nothing was asked yet (the panel opens one the first time it is shown).
    Idle,
    /// `TermOpen` is out: the host may be asking its user first (D13, up to 20 s).
    Opening {
        admin: bool,
    },
    Open {
        admin: bool,
    },
    /// The shell ended by itself (or the host took the permission away): `code` is the shell's.
    Exited {
        code: Option<i32>,
        admin: bool,
    },
    /// The host refused, or the service could not place the request; the message says why.
    Failed {
        why: Msg,
        admin: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    Public,
    Firewall,
    Service,
    MacPerms,
    /// The last run of Swapdesk ended in a panic (D12): shown once, first.
    Crash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectId {
    Language,
    Theme,
    Quality,
    Keyboard,
    /// The chord that releases the keyboard in the viewer (UI-local, D30).
    ReleaseKey,
    /// The monitor full screen goes to (UI-local).
    FullscreenMonitor,
    /// The keys the toolbar's keys button sends to the host (Ctrl+Alt+Del, Alt+Tab or Cmd+Tab, ...): a list of
    /// actions, none of them checked.
    SpecialKeys,
    /// The other computer's displays, the one shown checked (F-VIEW-02): the toolbar's screen button opens it.
    HostDisplay,
}

impl SelectId {
    /// The control the select's list opens under: the select's own field, or the toolbar button that opens it.
    pub fn anchor(self) -> ControlId {
        match self {
            SelectId::SpecialKeys => ControlId::TbKeys,
            SelectId::HostDisplay => ControlId::TbScreen,
            other => ControlId::Select(other),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditId {
    Name,
    Chat,
    /// The inline field that renames a paired device (acts on Enter or "Salvar", never on a timer).
    Rename,
    /// The manual address field (F-FIND-03); acts 150 ms after the last key (R-UI-4).
    Address,
    /// The explorer's inline rename of a row of this computer (slice 2a): Enter commits, Esc cancels.
    ExpRename,
}

/// The data a screen shows that comes from the service and may take a while (R-UI-1, R-UI-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataKind {
    ThisDevice,
    Network,
    Devices,
    Settings,
    Activity,
    Listing,
}

impl DataKind {
    pub const ALL: [DataKind; 6] = [
        DataKind::ThisDevice,
        DataKind::Network,
        DataKind::Devices,
        DataKind::Settings,
        DataKind::Activity,
        DataKind::Listing,
    ];
}

/// Where a piece of data stands. A screen draws placeholder blocks only in `Placeholder`: `Pending` is the first
/// 100 ms, when nothing in between is shown so nothing flashes (R-UI-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fetch {
    Missing,
    Pending,
    Placeholder,
    Ready,
}

/// The delay after which a pending fetch shows placeholders.
pub const PLACEHOLDER_AFTER_MS: u64 = 100;

/// The minute a lost session keeps reconnecting (the service's `ReconnectPolicy` window).
pub const RECONNECT_WINDOW_S: u16 = 60;

/// The seconds the asker waits for the other side's answer to a swap request before giving up.
pub const SWAP_WAIT_S: u16 = 22;

/// The body of the main window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Home,
    Devices,
    Activity,
    Settings,
    Pairing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingView {
    /// 0 until the backend's `PairingId` reply arrives.
    pub id: u32,
    pub device_id: Option<String>,
    pub state: PairingState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingView {
    pub request: IncomingRequest,
    pub remaining_s: u16,
    /// The boxes of "Permitir nesta sessão".
    pub allow: Permissions,
    pub remember_unattended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElevPhase {
    Ask,
    Waiting,
    Failed(ApiCode),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Elevation {
    pub action: ElevatedAction,
    pub phase: ElevPhase,
    /// The run of the last `Elevate` this step sent (0 before the first).
    pub run: u32,
}

/// What the elevated instance's own window asks (ELEVATION.md 3.5): the action as the service holds it, the
/// seconds the ticket still lives and the person's answer once given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElevatedConfirm {
    /// The line that says what is about to be allowed, in the window's language.
    pub line: String,
    pub remaining_s: u16,
    /// `Some(true)` only for "Confirmar"; "Cancelar", Esc and the time running out answer no.
    pub answer: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub id: u64,
    pub device_id: String,
    pub state: SessionState,
    pub stats: Option<Stats>,
    /// The displays of the peer the service listed for this session (empty until it did).
    pub displays: Vec<DisplayView>,
    /// The display being shown: the primary one until the user picks another.
    pub display: Option<u16>,
    /// For a connect entry: what the user asked for, kept so "Tentar de novo" asks the same.
    pub intent: Option<Intent>,
    /// For an entry that waits on a countdown (`Waking`, `Prompted`): the seconds it started from, for the bar.
    pub countdown_total_s: Option<u16>,
    /// Seconds a connect entry has spent in `Connecting` or `Prompted`, for the interface's own give-up.
    pub age_s: u16,
    /// The user pressed "Cancelar" before the service gave the entry its id: the id is cancelled when it arrives.
    pub(crate) cancel_asked: bool,
    /// The role of the session's last `Active` state, kept while it is `Paused` or `Reconnecting`: a hosted session
    /// that reconnects stays the banner's and never opens a viewer window.
    pub role: Option<Role>,
    /// Seconds left of the reconnect minute, counted by the interface from the first `Reconnecting` attempt (the
    /// service sends attempts, not a deadline); `None` outside a reconnect or before its first attempt.
    pub reconnect_left_s: Option<u16>,
    /// Whether the session can be elevated, from its last `Active` state, kept while it is `Paused`.
    pub elevation: ElevationLevel,
}

impl SessionView {
    pub(crate) fn new(id: u64, device_id: String, state: SessionState) -> SessionView {
        let countdown_total_s = countdown_of(&state);
        let mut view = SessionView {
            id,
            device_id,
            state,
            stats: None,
            displays: Vec::new(),
            display: None,
            intent: None,
            countdown_total_s,
            age_s: 0,
            cancel_asked: false,
            role: None,
            reconnect_left_s: None,
            elevation: ElevationLevel::Unavailable,
        };
        view.note_state();
        view
    }

    /// What the current state says about the role and the reconnect minute.
    fn note_state(&mut self) {
        match &self.state {
            SessionState::Active { role, elevation, .. } => {
                self.role = Some(*role);
                self.reconnect_left_s = None;
                self.elevation = *elevation;
            }
            SessionState::Reconnecting { attempt } => {
                if *attempt >= 1 && self.reconnect_left_s.is_none() {
                    self.reconnect_left_s = Some(RECONNECT_WINDOW_S);
                }
            }
            _ => self.reconnect_left_s = None,
        }
    }

    /// Takes the state the service pushed. The bar's total is the first countdown value of a phase.
    fn set_state(&mut self, state: SessionState) {
        if std::mem::discriminant(&self.state) != std::mem::discriminant(&state) {
            self.countdown_total_s = countdown_of(&state);
        }
        self.state = state;
        self.note_state();
    }

    /// Is this session shown by the viewer window: an active viewer, or one that was and is paused or reconnecting.
    fn is_viewer(&self) -> bool {
        match &self.state {
            SessionState::Active { role, .. } => *role == Role::Viewer,
            SessionState::Paused { .. } | SessionState::Reconnecting { .. } => self.role != Some(Role::Host),
            _ => false,
        }
    }

    /// Is this session the one that makes this computer a host (the banner).
    fn is_host(&self) -> bool {
        match &self.state {
            SessionState::Active { role, .. } => *role == Role::Host,
            SessionState::Paused { .. } | SessionState::Reconnecting { .. } => self.role == Some(Role::Host),
            _ => false,
        }
    }

    /// Is this a connect entry still waiting (not a session)?
    pub(crate) fn is_waiting_entry(&self) -> bool {
        matches!(self.state, SessionState::Connecting | SessionState::Prompted { .. } | SessionState::Waking { .. })
    }

    pub fn display_count(&self) -> usize {
        self.displays.len()
    }

    fn set_displays(&mut self, displays: Vec<DisplayView>) {
        if self.display.is_none_or(|c| !displays.iter().any(|d| d.id == c)) {
            self.display = displays.iter().find(|d| d.primary).or(displays.first()).map(|d| d.id);
        }
        self.displays = displays;
    }
}

/// Where the connect flow stands on the main window (the device the user asked to connect to).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPhase {
    /// The session is being set up.
    Connecting,
    /// The other side's prompt is open, with the seconds left.
    Prompted { remaining_s: u16 },
    /// The attempt ended without a session, with why; kept until the user closes it or tries again.
    Ended { why: ConnectWhy },
}

/// The connect flow of one device's page: from "Conectando" to why it did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFlow {
    /// The entry's id (0 until the service answers the click).
    pub entry: u64,
    pub device_id: String,
    pub phase: ConnectPhase,
}

/// A connect attempt that ended without a session, waiting to be closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectEnd {
    pub entry: u64,
    pub device_id: String,
    pub why: ConnectWhy,
    pub intent: Option<Intent>,
}

/// The icon of the wait card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitIcon {
    /// Waking a sleeping computer.
    Power,
    /// Waiting for the other computer.
    Clock,
    /// The attempt did not open.
    ShieldAlert,
}

/// Where the card's phase stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitPhase {
    Waking,
    Connecting,
    Prompted,
    Ended,
}

/// Which side of the card's button row a button sits on (positive actions on the right).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonSide {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitButton {
    pub id: ControlId,
    pub label: String,
    pub side: ButtonSide,
    /// The prominent button of the row.
    pub primary: bool,
}

/// The host is asking its user about a feature of the viewer session (D13): the panel of that feature says so, with
/// the time the question has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConsentWait {
    feature: Feature,
    remaining_s: u16,
    total_s: u16,
}

/// The countdown row of a waiting card: a track filled from the left, with the time beside it when there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitBar {
    /// How much of the track is filled, 0.0 to 1.0.
    pub fraction: f32,
    /// "0:14", or `None` while the wait has no deadline to show (the track is drawn alone).
    pub text: Option<String>,
}

/// What a device's page shows in place of its content while a connect or a wake waits or has just failed.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitCard {
    pub device_id: String,
    pub phase: WaitPhase,
    pub icon: WaitIcon,
    pub title: String,
    pub body: String,
    /// `None` on an ended card.
    pub bar: Option<WaitBar>,
    pub buttons: Vec<WaitButton>,
}

/// What the viewer shows while its own swap request waits for the other side: a card in the place and style of the
/// "swap declined" card, with the countdown row and no button (there is no way to withdraw a request).
#[derive(Debug, Clone, PartialEq)]
pub struct SwapWait {
    /// The session the request was made on.
    pub session_id: u64,
    /// "Esperando o {peer} aceitar a inversão".
    pub title: String,
    /// "Se ninguém responder em 20 segundos, você continua controlando o {peer}."
    pub body: String,
    /// The countdown row, from 22 s.
    pub bar: WaitBar,
}

/// The pairing card after "Os códigos são iguais": this side confirmed and the other has not yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairWait {
    /// Replaces the `PairWarn` block under the code.
    pub body: String,
    /// Replaces the button row: `PairCancel` alone.
    pub buttons: Vec<WaitButton>,
}

/// The line under the quick actions while one of them waits for the other computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionWait {
    pub session_id: u64,
    /// "Esperando o {peer} executar a ação…"
    pub line: String,
}

/// The seconds a state counts down from, if it counts.
fn countdown_of(state: &SessionState) -> Option<u16> {
    match state {
        // A wake always lasts its minute, also when the first news of it arrives with the minute under way; a
        // request counts from the first value the other computer reports.
        SessionState::Waking { remaining_s } => Some((*remaining_s).max(WAKE_COUNTDOWN_S)),
        SessionState::Prompted { remaining_s } => Some(*remaining_s),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapPhase {
    /// The viewer's own "Inverter o controle?" card.
    Confirm,
    /// Asked, waiting for the other side.
    Requested { request_id: u32 },
    /// The other side asks this one.
    Asked { request_id: u32, by: String, remaining_s: u16 },
    /// This side's request did not happen: the card says why.
    Declined(SwapWhy),
}

/// A paired device's name being edited on its page: which device, and why the last save did not go through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameDraft {
    pub device_id: String,
    pub error: Option<ApiError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapView {
    pub session_id: u64,
    pub phase: SwapPhase,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferView {
    pub id: u32,
    pub session_id: u64,
    pub name: String,
    pub outgoing: bool,
    pub state: TransferState,
    /// The name a received file was saved under when it is not `name`: that one was taken and files never overwrite.
    pub saved_as: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogKind {
    Elevation,
    /// The elevated instance's confirmation: the only thing its window shows.
    ElevatedConfirm,
    Consent,
    SwapConfirm,
    SwapAsk,
}

/// What `activate` hands back: windows to redraw, commands for the backend and an effect only the shell can do.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Activation {
    pub changed: Changed,
    pub commands: Vec<Command>,
    pub effect: Option<Effect>,
}

/// Things the shell does itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Open the choice list of a select; the answer comes back through [`UiState::choose`].
    Menu(SelectId),
    /// Put the keyboard focus in the native text input.
    FocusEdit(EditId),
    /// Read the native text input and call [`UiState::submit_edit`] with it.
    SubmitEdit(EditId),
    /// Ask where to save the activity log, then send `ActivityExport`.
    SaveLog,
    /// Open the folder of the logs with the system shell (the crash notice's action).
    OpenLogs,
    /// Ask which files to send, then call [`UiState::send_files`] with them (the file panel's "Escolher arquivos").
    PickFiles,
    /// Ask which folder to receive files into, then call [`UiState::set_receive_dir`] with it.
    PickReceiveFolder,
    /// Move the keyboard focus to this control of the main window (the one that was activated has gone).
    Focus(ControlId),
    /// Open the web page of [`UiState::take_web_page`] in the system's browser (the release page of an update,
    /// UPDATES.md 10).
    OpenWebPage,
    /// Open the local file of [`UiState::take_local_target`] with the system's app for it (the explorer's left pane).
    OpenLocal,
    /// Show the context menu of [`UiState::context_items`] at the pointer (a right click, Control+click on the Mac,
    /// the Menu key or Shift+F10); the answer comes back through [`UiState::context_chosen`], or
    /// [`UiState::context_closed`] when it was dismissed.
    ContextMenu,
    /// Show the local item of [`UiState::take_local_target`] in the system's file manager (Explorador de Arquivos,
    /// Finder).
    RevealLocal,
    /// Slice 5: start the system's drag of [`UiState::take_system_drag`] (the pointer left the window mid-drag).
    SystemDrag,
}

/// What to restore when an optimistic change is refused.
#[derive(Debug, Clone)]
pub(crate) enum Undo {
    Grants(String, Permissions),
    Settings(Box<Settings>),
    Toggle(Toggle, bool),
    Quality(Quality),
}

/// The viewer's on-screen switches that change at once and revert on refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toggle {
    Audio,
    Clipboard,
    Grab,
    Fit,
    ViewOnly,
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub command: Command,
    pub undo: Option<Undo>,
    /// Seconds waited so far; the reply is a failure at `limit_s` (R-UI-7).
    pub age_s: u16,
    pub limit_s: u16,
}

pub struct UiState {
    /// Commands an event calls for (a pairing id that starts the initiator), sent by the shell after `apply`.
    pub(crate) followups: Vec<Command>,
    /// An activity refresh is out: the next `DevicesChanged` asks for none.
    activity_refresh: bool,
    pub(crate) lang: Language,
    /// What the OS says (`ThemeChanged`); used while `theme` is `System`.
    pub(crate) dark: bool,
    /// What the system asks for beyond light and dark (a contrast theme, "Increase contrast").
    pub(crate) contrast: crate::tokens::Contrast,
    /// The theme chosen in Ajustes (per user, like the language: the UI's copy wins over the service's).
    pub(crate) theme: Theme,
    /// The per-user record of C5.1 once a store is attached (`load_prefs`): the UI's own copy of the per-user
    /// settings, which wins over what the service answers and is written on each change.
    pub(crate) ui: Option<crate::prefs::UserPrefs>,
    prefs_store: Option<Box<dyn crate::prefs::PrefsStore>>,
    /// The first-run guide shows on Início (B16): no per-user record existed when the interface started, and the
    /// person has not closed the guide yet. Closing it writes the record, so it never comes back.
    pub(crate) guide_open: bool,
    pub(crate) clock: Clock,
    pub(crate) this: Option<ThisDevice>,
    pub(crate) network: Option<NetworkStatus>,
    pub(crate) devices: Vec<DeviceView>,
    pub(crate) selected: Option<String>,
    pub(crate) nav: Nav,
    pub(crate) tab: DeviceTab,
    pub(crate) pairing: Option<PairingView>,
    pub(crate) incoming: Option<IncomingView>,
    pub(crate) elevation: Option<Elevation>,
    /// The runs of `Elevate` sent so far, for `Elevation::run`.
    pub(crate) elev_runs: u32,
    pub(crate) confirm: Option<ElevatedConfirm>,
    /// This interface's account is an administrator account (D56: the confirmation's body); true until told.
    pub(crate) admin_account: bool,
    pub(crate) sessions: Vec<SessionView>,
    pub(crate) swap: Option<SwapView>,
    pub(crate) rename: Option<RenameDraft>,
    pub(crate) activity: Vec<AuditView>,
    pub(crate) alert: Option<AuditView>,
    /// The devices blocked by MAC (D65), as the service last listed them.
    pub(crate) blocked: Vec<crate::api::BlockedItem>,
    /// The list was asked for once (R-UI-3); the replies to blocking and unblocking keep it fresh.
    pub(crate) blocked_asked: bool,
    pub(crate) settings: Option<Settings>,
    pub(crate) drawer: Option<DrawerTab>,
    pub(crate) chat: Vec<ChatView>,
    pub(crate) transfers: Vec<TransferView>,
    pub(crate) listing: Option<Listing>,
    pub(crate) files: crate::interact::FilesPanel,
    /// The files explorer of the session window (FILES-UI.md).
    pub(crate) explorer: crate::interact::Explorer,
    /// The local file `Effect::OpenLocal` opens.
    pub(crate) local_target: Option<std::path::PathBuf>,
    pub(crate) term_account: Option<String>,
    pub(crate) term: TermPhase,
    /// The connection-wide id of the current terminal, once the service gave it.
    pub(crate) term_id: Option<u32>,
    /// The host is asking its user about the terminal or the files of the viewer session.
    pub(crate) consent_wait: Option<ConsentWait>,
    pub(crate) clipboard_on: bool,
    pub(crate) audio_on: bool,
    pub(crate) grab_on: bool,
    pub(crate) fit: bool,
    /// F-VIEW-06: the video area the shell last reported (physical pixels) and its monitor's refresh rate, and what
    /// the host was last told for which viewer session.
    pub(crate) view_area: (u32, u32),
    pub(crate) view_hz: u16,
    pub(crate) view_sent: Option<(u64, u16, u16, u16)>,
    /// The viewer's own "Assistir" switch and quality preset (per viewer session; reset when it ends).
    pub(crate) view_only: bool,
    pub(crate) quality: Quality,
    /// The shell's viewer toolbar offers the quality select and the view-only switch (the Windows toolbar does not).
    pub(crate) viewer_extras: bool,
    /// The viewer's shortcuts (D30): the viewer's OS, the release chord, full screen, the session bar shown in full
    /// screen, the monitor full screen goes to and how many monitors there are.
    pub(crate) viewer_os: crate::shortcuts::ViewerOs,
    pub(crate) release_key: crate::shortcuts::ReleaseKey,
    pub(crate) fullscreen: bool,
    pub(crate) bar_shown: bool,
    /// The session bar was made small (a handle at the top of the picture) for this session (B22).
    pub(crate) bar_collapsed: bool,
    pub(crate) fullscreen_monitor: Option<u8>,
    pub(crate) local_monitors: u8,
    pub(crate) connect_end: Option<ConnectEnd>,
    /// The control the shell should focus after the last events (the end card's "Fechar"), taken once.
    focus_request: Option<ControlId>,
    /// The viewer session whose display list was asked for by [`UiState::take_display_request`].
    pub(crate) displays_asked: Option<u64>,
    /// The sessions whose first `Active` state already asked the shell to minimize the main window.
    minimize_asked: Vec<u64>,
    /// Of those, the ones in which this computer controls and that have not ended yet: when the last of them ends,
    /// the main window comes back (never on the computer that was controlled, D60).
    restore_after: Vec<u64>,
    /// The shell composes a real remote picture in the viewer window (a real run, not a mock scenario or a capture).
    live_picture: bool,
    pub(crate) error: Option<ApiError>,
    pub(crate) pending: HashMap<Ticket, Pending>,
    pub(crate) undo_slot: Option<Undo>,
    pub(crate) last_new_device: Option<String>,
    pub(crate) fetch: HashMap<DataKind, Fetch>,
    /// "Os códigos são iguais" was sent and the service has not answered: the buttons wait (never optimistic).
    pub(crate) pair_confirming: bool,
    /// Seconds left of the 22 s the asker waits for an answer to its swap request.
    pub(crate) swap_left_s: u16,
    /// The crash notice is showing (D12).
    pub(crate) crash_notice: bool,
    /// Seconds the device list has been answered and empty (F20): the "no computer found" card shows at 10.
    pub(crate) empty_s: u16,
    /// The machine's updater (UPDATES.md 10); `None` while unknown or when the service has none (a development
    /// instance).
    pub(crate) update: Option<crate::api::UpdateInfo>,
    /// "Atualizar agora" met a live session: the confirmation (10.3) shows.
    pub(crate) update_confirm: bool,
    /// The download's rate (bytes a second, measured over the last second) for the time left, and the bytes at the
    /// last tick.
    pub(crate) update_rate: u64,
    pub(crate) update_prev_done: Option<u64>,
    /// The size of the package last seen downloading (the disk failure says how much it needs).
    pub(crate) update_size: u64,
    /// An update is being installed: the shell saves its window state and closes (Windows; on the Mac the app's own
    /// updater quits the app once the swap is done).
    pub(crate) quit_request: bool,
    /// The page [`Effect::OpenWebPage`] opens: a release page (`https://`), or one of [`SYSTEM_PAGES`].
    pub(crate) web_page: Option<String>,
}

/// Windows Settings > Network & internet ("ms-settings:network-status", Microsoft's ms-settings reference, read
/// 2026-11-12): where a network's profile is set to Private and the firewall is reached.
pub const WINDOWS_NETWORK_SETTINGS: &str = "ms-settings:network-status";

/// The system pages [`Effect::OpenWebPage`] may open besides an `https://` page. Nothing else is handed to the system.
pub const SYSTEM_PAGES: &[&str] = &[WINDOWS_NETWORK_SETTINGS];

/// Whether a shell may hand `page` to the system: an `https://` page or one of [`SYSTEM_PAGES`].
pub fn openable_page(page: &str) -> bool {
    page.starts_with("https://") || SYSTEM_PAGES.contains(&page)
}

impl Default for UiState {
    fn default() -> Self {
        UiState::new(Language::PtBr, false)
    }
}

impl UiState {
    pub fn new(lang: Language, dark: bool) -> UiState {
        UiState {
            followups: Vec::new(),
            activity_refresh: false,
            lang,
            dark,
            contrast: crate::tokens::Contrast::Standard,
            theme: Theme::System,
            ui: None,
            prefs_store: None,
            guide_open: false,
            clock: Clock::default(),
            this: None,
            network: None,
            devices: Vec::new(),
            selected: None,
            nav: Nav::Home,
            tab: DeviceTab::Overview,
            pairing: None,
            incoming: None,
            elevation: None,
            elev_runs: 0,
            confirm: None,
            admin_account: true,
            sessions: Vec::new(),
            swap: None,
            activity: Vec::new(),
            alert: None,
            blocked: Vec::new(),
            blocked_asked: false,
            settings: None,
            drawer: None,
            chat: Vec::new(),
            transfers: Vec::new(),
            listing: None,
            files: crate::interact::FilesPanel::default(),
            explorer: crate::interact::Explorer::default(),
            local_target: None,
            term_account: None,
            term: TermPhase::Idle,
            term_id: None,
            consent_wait: None,
            clipboard_on: true,
            audio_on: true,
            grab_on: true,
            fit: true,
            view_area: (0, 0),
            view_hz: 0,
            view_sent: None,
            view_only: false,
            quality: Quality::Auto,
            viewer_extras: false,
            viewer_os: crate::shortcuts::ViewerOs::Windows,
            release_key: crate::shortcuts::ReleaseKey::CtrlAlt,
            fullscreen: false,
            bar_shown: false,
            bar_collapsed: false,
            fullscreen_monitor: None,
            local_monitors: 1,
            connect_end: None,
            focus_request: None,
            displays_asked: None,
            minimize_asked: Vec::new(),
            restore_after: Vec::new(),
            live_picture: false,
            error: None,
            pending: HashMap::new(),
            undo_slot: None,
            last_new_device: None,
            fetch: HashMap::new(),
            pair_confirming: false,
            swap_left_s: 0,
            crash_notice: false,
            update: None,
            update_confirm: false,
            update_rate: 0,
            update_prev_done: None,
            update_size: 0,
            quit_request: false,
            web_page: None,
            empty_s: 0,
            rename: None,
        }
    }

    // ----- reads -----

    /// The command that writes the activity log to `path` (the shell's save panel picked it), in this interface's
    /// language and local time.
    pub fn activity_export(&self, path: std::path::PathBuf) -> Command {
        Command::ActivityExport { path, language: self.language(), utc_offset_min: self.clock.utc_offset_min }
    }

    pub fn language(&self) -> Language {
        self.lang
    }
    /// Whether the interface is drawn dark: the choice of Ajustes, or the OS's while it is `Sistema`.
    pub fn dark(&self) -> bool {
        match self.theme {
            Theme::System => self.dark,
            Theme::Light => false,
            Theme::Dark => true,
        }
    }
    pub fn theme(&self) -> Theme {
        self.theme
    }
    pub fn clock(&self) -> Clock {
        self.clock
    }
    pub fn this_device(&self) -> Option<&ThisDevice> {
        self.this.as_ref()
    }
    pub fn network(&self) -> Option<&NetworkStatus> {
        self.network.as_ref()
    }
    pub fn devices(&self) -> &[DeviceView] {
        &self.devices
    }
    /// The page shown. A device page with no device to show (the last one was forgotten) is the home.
    pub fn nav(&self) -> Nav {
        if self.nav == Nav::Devices && self.selected_device().is_none() { Nav::Home } else { self.nav }
    }
    pub fn tab(&self) -> DeviceTab {
        self.tab
    }
    pub fn pairing(&self) -> Option<&PairingView> {
        self.pairing.as_ref()
    }
    pub fn incoming(&self) -> Option<&IncomingView> {
        self.incoming.as_ref()
    }
    pub fn elevation(&self) -> Option<&Elevation> {
        self.elevation.as_ref()
    }
    pub fn activity(&self) -> &[AuditView] {
        &self.activity
    }
    pub fn alert(&self) -> Option<&AuditView> {
        self.alert.as_ref()
    }
    pub fn settings(&self) -> Option<&Settings> {
        self.settings.as_ref()
    }
    pub fn drawer(&self) -> Option<DrawerTab> {
        self.drawer
    }
    pub fn chat(&self) -> &[ChatView] {
        &self.chat
    }

    /// Messages from the other side this user has not seen (the badge on the chat button).
    pub fn chat_unread(&self) -> usize {
        self.chat.iter().filter(|m| !m.mine && !m.read).count()
    }

    /// The peer's name for session `id` (either role), empty when unknown.
    pub(crate) fn session_peer_name(&self, id: u64) -> String {
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .or_else(|| self.host_session())
            .and_then(|s| self.device_by_id(&s.device_id))
            .map(|d| d.name.clone())
            .unwrap_or_default()
    }

    /// The error strip for a swap this side asked as the host that did not happen.
    fn swap_why_error(&self, why: SwapWhy, peer: &str) -> ApiError {
        let detail = |key: MsgKey| Some(Msg::new(key).with(peer));
        match why {
            SwapWhy::Declined => ApiError { code: ApiCode::Refused, detail: detail(MsgKey::SwapWhyDeclined) },
            SwapWhy::TimedOut => ApiError::new(ApiCode::Timeout),
            SwapWhy::NobodyPresent => ApiError { code: ApiCode::Refused, detail: detail(MsgKey::SwapWhyNobody) },
            SwapWhy::NotPermitted => {
                ApiError { code: ApiCode::NotPermitted, detail: detail(MsgKey::SwapWhyNotPermittedThere) }
            }
            SwapWhy::Busy => ApiError::new(ApiCode::Busy),
        }
    }

    /// Title and body of the viewer's "swap declined" card.
    pub fn swap_declined_texts(&self) -> (String, String) {
        let peer = self.swap_peer_name();
        let why = match self.swap.as_ref().map(|s| &s.phase) {
            Some(SwapPhase::Declined(why)) => *why,
            _ => SwapWhy::TimedOut,
        };
        let title = match why {
            SwapWhy::Declined | SwapWhy::TimedOut | SwapWhy::NobodyPresent => MsgKey::SwapDeclinedTitle,
            SwapWhy::NotPermitted | SwapWhy::Busy => MsgKey::SwapNotNowTitle,
        };
        let stays = |line: MsgKey| {
            format!(
                "{} {}",
                self.tm(&Msg::new(line).with(peer.clone())),
                self.tm(&Msg::new(MsgKey::SwapStays).with(peer.clone()))
            )
        };
        let body = match why {
            SwapWhy::TimedOut => self.tm(&Msg::new(MsgKey::SwapDeclinedBody).with(peer.clone())),
            SwapWhy::Declined => stays(MsgKey::SwapWhyDeclined),
            SwapWhy::NobodyPresent => stays(MsgKey::SwapWhyNobody),
            SwapWhy::NotPermitted => self.tm(&Msg::new(MsgKey::SwapWhyNotPermittedHere).with(peer.clone())),
            SwapWhy::Busy => self.t(MsgKey::SwapWhyBusy),
        };
        (self.tm(&Msg::new(title).with(peer)), body)
    }
    pub fn transfers(&self) -> &[TransferView] {
        &self.transfers
    }
    pub fn listing(&self) -> Option<&Listing> {
        self.listing.as_ref()
    }
    pub fn term_account(&self) -> Option<&str> {
        self.term_account.as_deref()
    }
    pub fn term_phase(&self) -> &TermPhase {
        &self.term
    }
    /// The id the shell routes terminal bytes by; `None` until the service answered `TermOpen`.
    pub fn term_id(&self) -> Option<u32> {
        self.term_id
    }
    /// The terminal that is open or opening is the administrator's (the panel labels it so).
    pub fn term_is_admin(&self) -> bool {
        matches!(
            self.term,
            TermPhase::Opening { admin: true }
                | TermPhase::Open { admin: true }
                | TermPhase::Exited { admin: true, .. }
                | TermPhase::Failed { admin: true, .. }
        )
    }
    /// The session can open an administrator terminal (`ADMIN_TERMINAL` granted): the panel offers the button.
    pub fn admin_terminal_offered(&self) -> bool {
        self.viewer_session().is_some_and(
            |s| matches!(&s.state, SessionState::Active { granted, .. } if granted.has(Permission::AdminTerminal)),
        )
    }
    pub fn clipboard_on(&self) -> bool {
        self.clipboard_on
    }
    pub fn audio_on(&self) -> bool {
        self.audio_on
    }
    pub fn grab_on(&self) -> bool {
        self.grab_on
    }
    /// "Ajustar à janela": true when the video is scaled to fit the window.
    pub fn fit(&self) -> bool {
        self.fit
    }
    /// The viewer's "Assistir" switch (the peer may also deny control: see the session's granted permissions).
    pub fn view_only(&self) -> bool {
        self.view_only
    }
    /// The viewer's quality preset.
    pub fn quality(&self) -> Quality {
        self.quality
    }
    /// True when the shell's viewer toolbar has the quality select and the view-only switch, so the control list
    /// carries them.
    pub fn viewer_extras(&self) -> bool {
        self.viewer_extras
    }
    pub fn set_viewer_extras(&mut self, on: bool) {
        self.viewer_extras = on;
    }
    pub fn error(&self) -> Option<&ApiError> {
        self.error.as_ref()
    }
    pub fn swap(&self) -> Option<&SwapView> {
        self.swap.as_ref()
    }
    pub fn sessions(&self) -> &[SessionView] {
        &self.sessions
    }

    /// The text of a key in the current language.
    pub fn t(&self, key: MsgKey) -> String {
        strings::plain(self.lang, key)
    }

    /// The text of a message in the current language.
    pub fn tm(&self, msg: &Msg) -> String {
        strings::text(self.lang, msg)
    }

    /// The selected device (the first paired one when nothing is selected).
    /// The rename in progress for the device on screen (the field replaces its name in the header).
    pub fn renaming(&self) -> Option<&RenameDraft> {
        let d = self.selected_device()?;
        self.rename.as_ref().filter(|r| r.device_id == d.id)
    }

    pub fn selected_device(&self) -> Option<&DeviceView> {
        self.selected
            .as_ref()
            .and_then(|id| self.devices.iter().find(|d| &d.id == id))
            .or_else(|| self.devices.iter().find(|d| d.paired))
            .or_else(|| self.devices.first())
    }

    pub fn device_by_id(&self, id: &str) -> Option<&DeviceView> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// The session shown in the viewer window, if any.
    pub fn viewer_session(&self) -> Option<&SessionView> {
        self.sessions.iter().find(|s| s.is_viewer())
    }

    /// The card over the paused picture of the viewer's session (ACCOUNTS.md 5.1 and 9, D55): its text and whether it
    /// offers "Elevar controle". `None`: not paused, or paused with nothing to offer (the 1.0 pause).
    pub fn paused_card(&self) -> Option<(String, bool)> {
        let v = self.viewer_session()?;
        let SessionState::Paused { desktop } = v.state else { return None };
        let peer = self.device_by_id(&v.device_id).map(|d| d.name.clone()).unwrap_or_default();
        match desktop {
            DesktopKind::AccountAway => Some((self.tm(&Msg::new(MsgKey::PausedAwayBody).with(peer)), false)),
            _ if v.elevation == ElevationLevel::Available => {
                Some((self.tm(&Msg::new(MsgKey::PausedElevateBody).with(peer)), true))
            }
            // D24: a Mac's lock screen cannot be seen or controlled from here, which offers no elevation either.
            DesktopKind::Locked | DesktopKind::LoginWindow
                if self.device_by_id(&v.device_id).is_some_and(|d| d.os == Os::MacOs) =>
            {
                Some((self.tm(&Msg::new(MsgKey::PausedMacLocked).with(peer)), false))
            }
            _ => None,
        }
    }

    /// Whether the viewer's session offers "Elevar controle" (ACCOUNTS.md 9.3).
    pub fn can_elevate_session(&self) -> bool {
        self.viewer_session().is_some_and(|v| {
            v.elevation == ElevationLevel::Available
                && !matches!(v.state, SessionState::Paused { desktop: DesktopKind::AccountAway })
        })
    }

    /// The session that makes this computer a host (the banner).
    pub fn host_session(&self) -> Option<&SessionView> {
        self.sessions.iter().find(|s| s.is_host())
    }

    /// The host is asking its user whether the viewer session may use `feature` (D13): the countdown row of the
    /// question, from the seconds it was told to the end. `None` when nobody is being asked about it.
    pub fn consent_wait(&self, feature: Feature) -> Option<WaitBar> {
        let w = self.consent_wait.filter(|w| w.feature == feature)?;
        let total = w.total_s.max(1);
        let left = w.remaining_s.min(total);
        Some(WaitBar {
            fraction: f32::from(total - left) / f32::from(total),
            text: Some(crate::format::countdown(left)),
        })
    }

    /// The answer to the question about `feature` came (the feature opened, or the request failed).
    pub(crate) fn consent_answered(&mut self, feature: Feature) {
        if self.consent_wait.is_some_and(|w| w.feature == feature) {
            self.consent_wait = None;
        }
    }

    /// A session waking its device (shown in the main area, in place of the device's content).
    pub fn waking_session(&self) -> Option<&SessionView> {
        self.sessions.iter().find(|s| matches!(s.state, SessionState::Waking { .. }))
    }

    /// The countdown row of the viewer's "Reconectando" card: from [`RECONNECT_WINDOW_S`], counted by the interface
    /// from the first attempt; the track alone until then. `None` when the viewer session is not reconnecting.
    pub fn reconnect_wait(&self) -> Option<WaitBar> {
        let s = self.viewer_session()?;
        let SessionState::Reconnecting { .. } = s.state else { return None };
        Some(match s.reconnect_left_s {
            Some(left) => {
                let left = left.min(RECONNECT_WINDOW_S);
                WaitBar {
                    fraction: f32::from(RECONNECT_WINDOW_S - left) / f32::from(RECONNECT_WINDOW_S),
                    text: Some(crate::format::countdown(left)),
                }
            }
            None => WaitBar { fraction: 0.0, text: None },
        })
    }

    /// The viewer's swap card while its own request waits for the other side; `None` otherwise (also on the host,
    /// whose banner says it in [`UiState::banner_parts`]).
    pub fn swap_wait(&self) -> Option<SwapWait> {
        let SwapView { session_id, phase: SwapPhase::Requested { .. } } = self.swap.as_ref()? else { return None };
        if self.viewer_session()?.id != *session_id {
            return None;
        }
        let peer = self.swap_peer_name();
        let left = self.swap_left_s.min(SWAP_WAIT_S);
        Some(SwapWait {
            session_id: *session_id,
            title: self.tm(&Msg::new(MsgKey::SwapWaitingTitle).with(peer.clone())),
            body: self.tm(&Msg::new(MsgKey::SwapWaitingBody).with(peer)),
            bar: WaitBar {
                fraction: f32::from(SWAP_WAIT_S - left) / f32::from(SWAP_WAIT_S),
                text: Some(crate::format::countdown(left)),
            },
        })
    }

    /// The pairing card's switched content while this side's confirmation waits for the other side's; `None` when
    /// the card is not in that step.
    pub fn pair_wait(&self) -> Option<PairWait> {
        if !self.pair_confirming || !matches!(self.pairing.as_ref()?.state, PairingState::ShowCode { .. }) {
            return None;
        }
        Some(PairWait {
            body: self.t(MsgKey::PairWaitingBody),
            buttons: vec![WaitButton {
                id: ControlId::PairCancel,
                label: self.t(MsgKey::PairCancel),
                side: ButtonSide::Left,
                primary: false,
            }],
        })
    }

    /// A quick action of this session waits for the other computer.
    fn action_pending(&self, session_id: u64) -> bool {
        self.pending
            .values()
            .any(|p| matches!(p.command, Command::RunAction { session_id: id, .. } if id == session_id))
    }

    /// While a quick action waits, every quick-action button is disabled (one at a time).
    pub fn actions_disabled(&self) -> bool {
        self.viewer_session().is_some_and(|s| self.action_pending(s.id))
    }

    /// The line under the quick actions while one waits; `None` when none does.
    pub fn action_wait(&self) -> Option<ActionWait> {
        let s = self.viewer_session()?;
        if !self.action_pending(s.id) {
            return None;
        }
        let peer = self.device_by_id(&s.device_id).map(|d| d.name.clone()).unwrap_or_default();
        Some(ActionWait { session_id: s.id, line: self.tm(&Msg::new(MsgKey::ActionWaiting).with(peer)) })
    }

    /// The entry of a device that is waiting (connecting, prompted or waking).
    pub(crate) fn waiting_entry(&self, device_id: &str) -> Option<&SessionView> {
        self.sessions.iter().find(|s| s.device_id == device_id && s.is_waiting_entry())
    }

    /// The connect flow of a device's page: the attempt being set up (`Connecting`, or `Prompted` with its seconds),
    /// else why the last one did not open, kept until [`UiState::dismiss_connect`]. A wake's own wait is
    /// [`UiState::waking_session`].
    pub fn connect_flow(&self, device_id: &str) -> Option<ConnectFlow> {
        if let Some(s) = self.sessions.iter().find(|s| s.device_id == device_id) {
            let phase = match s.state {
                SessionState::Connecting => Some(ConnectPhase::Connecting),
                SessionState::Prompted { remaining_s } => Some(ConnectPhase::Prompted { remaining_s }),
                _ => None,
            };
            if let Some(phase) = phase {
                return Some(ConnectFlow { entry: s.id, device_id: s.device_id.clone(), phase });
            }
        }
        self.connect_end.as_ref().filter(|e| e.device_id == device_id).map(|e| ConnectFlow {
            entry: e.entry,
            device_id: e.device_id.clone(),
            phase: ConnectPhase::Ended { why: e.why },
        })
    }

    /// What a device's page shows in place of its content while a connect or a wake waits or has just failed, for
    /// both cases: the painter draws this one card. `None` when the page is the normal page.
    pub fn wait_card(&self, device_id: &str) -> Option<WaitCard> {
        let name = self.device_by_id(device_id)?.name.clone();
        let titled = |key: MsgKey| self.tm(&Msg::new(key).with(name.clone()));
        let cancel = |id: ControlId| {
            vec![WaitButton { id, label: self.t(MsgKey::Cancel), side: ButtonSide::Left, primary: false }]
        };
        if let Some(e) = self.waiting_entry(device_id) {
            let bar = |remaining_s: u16| {
                let total = e.countdown_total_s.unwrap_or(remaining_s).max(1);
                let elapsed = total.saturating_sub(remaining_s);
                WaitBar {
                    fraction: (f32::from(elapsed) / f32::from(total)).clamp(0.0, 1.0),
                    text: Some(crate::format::countdown(remaining_s)),
                }
            };
            return Some(match e.state {
                SessionState::Waking { remaining_s } => WaitCard {
                    device_id: device_id.to_owned(),
                    phase: WaitPhase::Waking,
                    icon: WaitIcon::Power,
                    title: titled(MsgKey::WakingTitle),
                    body: self.t(MsgKey::WakingBody),
                    bar: Some(bar(remaining_s)),
                    buttons: cancel(ControlId::WakeCancel),
                },
                SessionState::Prompted { remaining_s } => WaitCard {
                    device_id: device_id.to_owned(),
                    phase: WaitPhase::Prompted,
                    icon: WaitIcon::Clock,
                    title: titled(MsgKey::PromptedTitle),
                    body: titled(MsgKey::PromptedBody),
                    bar: Some(bar(remaining_s)),
                    buttons: cancel(ControlId::ConnectCancel),
                },
                _ => WaitCard {
                    device_id: device_id.to_owned(),
                    phase: WaitPhase::Connecting,
                    icon: WaitIcon::Clock,
                    title: titled(MsgKey::ConnectingTitle),
                    body: self.t(MsgKey::ConnectingBody),
                    // The track alone: there is no deadline to show until the other computer asks its user.
                    bar: Some(WaitBar { fraction: 0.0, text: None }),
                    buttons: cancel(ControlId::ConnectCancel),
                },
            });
        }
        let end = self.connect_end.as_ref().filter(|e| e.device_id == device_id)?;
        let body = match end.why {
            ConnectWhy::Declined => titled(MsgKey::SwapWhyDeclined),
            ConnectWhy::TimedOut => titled(MsgKey::ConnectWhyTimedOut),
            ConnectWhy::NobodyPresent => titled(MsgKey::SwapWhyNobody),
            ConnectWhy::Busy => titled(MsgKey::ConnectWhyBusy),
            ConnectWhy::Forgotten => titled(MsgKey::ConnectWhyForgotten),
            ConnectWhy::NotPermitted => titled(MsgKey::ConnectWhyNotPermitted),
            ConnectWhy::Unreachable => titled(MsgKey::ConnectWhyUnreachable),
            ConnectWhy::Failed => self.t(MsgKey::ErrInternal),
        };
        Some(WaitCard {
            device_id: device_id.to_owned(),
            phase: WaitPhase::Ended,
            icon: WaitIcon::ShieldAlert,
            title: titled(MsgKey::ConnectEndedTitle),
            body,
            bar: None,
            buttons: vec![
                WaitButton {
                    id: ControlId::ConnectClose,
                    label: self.t(MsgKey::Close),
                    side: ButtonSide::Left,
                    primary: false,
                },
                WaitButton {
                    id: ControlId::ConnectRetry,
                    label: self.t(MsgKey::PairRetry),
                    side: ButtonSide::Right,
                    primary: true,
                },
            ],
        })
    }

    /// The wait card of the device whose page is open, if any.
    pub fn selected_wait_card(&self) -> Option<WaitCard> {
        self.wait_card(&self.selected_device()?.id)
    }

    /// The second line of a sidebar row: the wait says so while the device has a connect or a wake under way,
    /// otherwise [`crate::format::status_line`].
    pub fn device_status_line(&self, d: &DeviceView) -> String {
        let key = self.waiting_entry(&d.id).map(|e| match e.state {
            SessionState::Waking { .. } => MsgKey::SidebarWaking,
            SessionState::Prompted { .. } => MsgKey::SidebarWaitingAccept,
            _ => MsgKey::SidebarConnecting,
        });
        match key {
            Some(key) => self.t(key),
            None => crate::format::status_line(self.lang, d),
        }
    }

    /// The control Esc activates in a window: "Cancelar" while a card waits, "Fechar" on an ended card. Only the
    /// main window's page, and not while a dialog or another screen is over it.
    pub fn escape_target(&self, kind: WindowKind) -> Option<ControlId> {
        // In the elevated instance's window Esc is "Cancelar".
        if kind == WindowKind::Main && self.confirm.is_some() {
            return Some(ControlId::DialogSecondary);
        }
        if kind != WindowKind::Main
            || self.dialog(kind).is_some()
            || self.incoming_session_open()
            || self.pairing.is_some()
            || self.nav() != Nav::Devices
        {
            return None;
        }
        self.selected_wait_card()?.buttons.first().map(|b| b.id)
    }

    /// The control the shell should focus now, once (after a connect ended and its card says why).
    pub fn take_focus_request(&mut self) -> Option<ControlId> {
        self.focus_request.take()
    }

    /// The user closed the end card.
    pub fn dismiss_connect(&mut self) -> Changed {
        if self.connect_end.take().is_some() { Changed::MAIN } else { Changed::NONE }
    }

    /// The displays the service listed for the viewer session.
    pub fn viewer_displays(&self) -> &[DisplayView] {
        self.viewer_session().map_or(&[], |s| &s.displays)
    }

    /// The display shown in the viewer.
    pub fn viewer_display(&self) -> Option<u16> {
        self.viewer_session().and_then(|s| s.display)
    }

    /// The display listing to ask for, once per viewer session, when the session is up. The shell sends it with its
    /// other commands; the answer lands in the session's `displays`.
    pub fn take_display_request(&mut self) -> Option<Command> {
        let s = self.viewer_session()?;
        if !matches!(s.state, SessionState::Active { .. }) || self.displays_asked == Some(s.id) {
            return None;
        }
        let session_id = s.id;
        self.displays_asked = Some(session_id);
        Some(Command::ListDisplays { session_id })
    }

    /// Whether the viewer window shows a real remote picture. The viewer screen then gives the picture its whole area,
    /// with no margin, and the renderer keeps the picture's own proportion inside it; a mock scenario and a capture
    /// keep the design's framed 16:10 rectangle.
    pub fn live_picture(&self) -> bool {
        self.live_picture
    }

    /// The shell says, once at its start, that it composes real pictures (see [`UiState::live_picture`]).
    pub fn set_live_picture(&mut self, on: bool) {
        self.live_picture = on;
    }

    /// True once per session id, when a session of this computer (viewer or host) is first seen `Active`: the shell
    /// then minimizes the main window, which does not need to stay on screen. A later state of the same session
    /// (paused, reconnecting, active again, other grants) asks nothing; a new session asks again. A shell that
    /// never shows a real session (a mock run) does not call it.
    pub fn take_minimize_request(&mut self) -> bool {
        let mut asked = false;
        for s in &self.sessions {
            if matches!(s.state, SessionState::Active { .. }) && !self.minimize_asked.contains(&s.id) {
                self.minimize_asked.push(s.id);
                if matches!(s.state, SessionState::Active { role: Role::Viewer, .. }) {
                    self.restore_after.push(s.id);
                }
                asked = true;
            }
        }
        asked
    }

    /// True once when the last session in which this computer controlled (and that minimized the main window) has
    /// ended and no other session of this computer is still going (owner, 11/11: the window comes back on the
    /// computer that controlled; on the one that was controlled it stays minimized). The shell
    /// then restores the main window if it is still minimized; a window the user already brought back is left as it
    /// is. A shell that never shows a real session (a mock run) does not call it.
    pub fn take_restore_request(&mut self) -> bool {
        let ended = |s: &SessionState| matches!(s, SessionState::Ended { .. } | SessionState::NotOpened { .. });
        let live = |id: u64, sessions: &[SessionView]| sessions.iter().any(|s| s.id == id && !ended(&s.state));
        let before = self.restore_after.len();
        let sessions = &self.sessions;
        self.restore_after.retain(|id| live(*id, sessions));
        before > self.restore_after.len()
            && self.restore_after.is_empty()
            && !self.sessions.iter().any(|s| {
                matches!(
                    s.state,
                    SessionState::Active { .. } | SessionState::Paused { .. } | SessionState::Reconnecting { .. }
                )
            })
    }

    /// Which windows exist now.
    pub fn windows(&self) -> Vec<WindowKind> {
        let mut v = vec![WindowKind::Main];
        if self.viewer_session().is_some() {
            v.push(WindowKind::Viewer);
        }
        if self.host_session().is_some() {
            v.push(WindowKind::Banner);
        }
        v
    }

    /// The body of the main window.
    pub fn screen(&self) -> Screen {
        if self.pairing.is_some() {
            return Screen::Pairing;
        }
        match self.nav() {
            Nav::Home => Screen::Home,
            Nav::Devices => Screen::Devices,
            Nav::Activity => Screen::Activity,
            Nav::Settings => Screen::Settings,
        }
    }

    /// Which notices to show in place of the selected device's content.
    pub fn notices(&self) -> Vec<NoticeKind> {
        let crash = self.crash_notice.then_some(NoticeKind::Crash);
        let Some(net) = &self.network else { return crash.into_iter().collect() };
        crash
            .into_iter()
            .chain(net.notices.iter().map(|n| match n {
                Notice::PublicNetwork => NoticeKind::Public,
                Notice::FirewallBlocks => NoticeKind::Firewall,
                Notice::ServiceNeedsApproval => NoticeKind::Service,
                Notice::MacPermissionsMissing(_) => NoticeKind::MacPerms,
            }))
            .collect()
    }

    /// The last run crashed: the app calls this once at launch, after it renamed the marker file (D12). The notice
    /// goes when its action is used, another device is picked or another section opens.
    /// Whether this interface's account is an administrator account (Windows reads it from its own token): a
    /// standard account's confirmation says Windows will ask for an administrator's name and password (D56).
    pub fn set_admin_account(&mut self, admin: bool) {
        self.admin_account = admin;
    }

    pub fn show_crash_notice(&mut self) {
        self.crash_notice = true;
    }

    pub fn crash_notice(&self) -> bool {
        self.crash_notice
    }

    /// The first-run guide (B16): its title and body while it shows.
    pub fn guide_card(&self) -> Option<(String, String)> {
        self.guide_open.then(|| (self.t(MsgKey::GuideTitle), self.t(MsgKey::GuideBody)))
    }

    /// The person closed the guide: the record is written, so the guide never shows again.
    pub(crate) fn close_guide(&mut self) -> Changed {
        self.guide_open = false;
        self.write_prefs();
        Changed::ALL
    }

    /// Seconds of an answered, empty device list before the "no computer found" card replaces the plain text (F20).
    pub const NO_COMPUTER_AFTER_S: u16 = 10;

    /// The main screen shows the "Nenhum computador encontrado" card: the list was answered, is empty, and discovery
    /// has found nothing for [`Self::NO_COMPUTER_AFTER_S`] seconds.
    pub fn no_computer_card(&self) -> bool {
        self.devices.is_empty()
            && self.fetch(DataKind::Devices) == Fetch::Ready
            && self.empty_s >= Self::NO_COMPUTER_AFTER_S
    }

    /// The modal dialog of a window, if any.
    pub fn dialog(&self, kind: WindowKind) -> Option<DialogKind> {
        match kind {
            WindowKind::Main => {
                if self.confirm.is_some() {
                    Some(DialogKind::ElevatedConfirm)
                } else if self.elevation.is_some() {
                    Some(DialogKind::Elevation)
                } else if matches!(self.incoming.as_ref().map(|i| &i.request.kind), Some(RequestKind::Consent { .. })) {
                    Some(DialogKind::Consent)
                } else if matches!(self.swap.as_ref().map(|s| &s.phase), Some(SwapPhase::Asked { .. })) {
                    Some(DialogKind::SwapAsk)
                } else {
                    None
                }
            }
            WindowKind::Viewer => match self.swap.as_ref().map(|s| &s.phase) {
                Some(SwapPhase::Confirm) => Some(DialogKind::SwapConfirm),
                _ => None,
            },
            WindowKind::Banner => None,
        }
    }

    /// True while the incoming-session dialog covers the main window.
    pub fn incoming_session_open(&self) -> bool {
        self.elevation.is_none()
            && matches!(self.incoming.as_ref().map(|i| &i.request.kind), Some(RequestKind::Session { .. }))
    }

    /// Milliseconds from `now_ms` to the next whole minute of the host banner's duration ("há 12 min"), or `None`
    /// while no session is being hosted. The shell arms one timer for it, re-arms it after each firing and stops it
    /// when the banner closes: the banner's text changes only on these minutes, so it needs no steadier tick.
    pub fn banner_minute_delay_ms(&self, now_ms: u64) -> Option<u64> {
        let SessionState::Active { since_ms, .. } = self.host_session()?.state else { return None };
        const MINUTE_MS: u64 = 60_000;
        let elapsed = now_ms.saturating_sub(since_ms);
        Some(MINUTE_MS - elapsed % MINUTE_MS)
    }

    /// True while something counts seconds down (the shell then runs a one-second timer, otherwise none).
    /// The state of the elevated instance's window (ELEVATION.md 3.5): nothing but the step that says what an
    /// administrator is about to allow, with "Cancelar" and "Confirmar". `line` is the action as the service
    /// described it; `remaining_s` is what is left of the ticket.
    pub fn for_elevated_confirm(lang: Language, dark: bool, line: String, remaining_s: u16) -> UiState {
        let mut state = UiState::new(lang, dark);
        state.confirm = Some(ElevatedConfirm { line, remaining_s, answer: None });
        state
    }

    pub fn elevated_confirm(&self) -> Option<&ElevatedConfirm> {
        self.confirm.as_ref()
    }

    /// The answer given in the elevated instance's window, once there is one. The window closes on it.
    pub fn confirm_answer(&self) -> Option<bool> {
        self.confirm.as_ref().and_then(|c| c.answer)
    }

    pub fn needs_tick(&self) -> bool {
        matches!(self.pairing.as_ref().map(|p| &p.state), Some(PairingState::Open { .. }))
            || self.confirm.as_ref().is_some_and(|c| c.answer.is_none())
            || self.incoming.is_some()
            || matches!(self.swap.as_ref().map(|s| &s.phase), Some(SwapPhase::Asked { .. }))
            || self.swap_requested()
            || self.sessions.iter().any(|s| {
                matches!(
                    s.state,
                    SessionState::Waking { .. }
                        | SessionState::Prompted { .. }
                        | SessionState::Connecting
                        | SessionState::Reconnecting { .. }
                )
            })
            || !self.pending.is_empty()
    }

    // ----- local changes -----

    /// Takes the commands the last events called for; the shell sends them like any other command.
    pub fn take_followups(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.followups)
    }

    pub fn set_clock(&mut self, clock: Clock) {
        self.clock = clock;
    }

    /// The OS's appearance changed (or was read): it shows only while the theme is `Sistema`.
    pub fn set_dark(&mut self, dark: bool) -> Changed {
        let before = self.dark();
        self.dark = dark;
        if self.dark() == before { Changed::NONE } else { Changed::ALL }
    }

    /// The system's contrast setting changed (or was read): a Windows contrast theme or macOS "Increase contrast".
    pub fn set_contrast(&mut self, contrast: crate::tokens::Contrast) -> Changed {
        if self.contrast == contrast {
            return Changed::NONE;
        }
        self.contrast = contrast;
        Changed::ALL
    }

    /// The colours to draw with now: a Windows contrast theme's own colours whatever the theme; with "Increase
    /// contrast", the stronger light or dark palette; else the design's (F-UI-01).
    pub fn palette(&self) -> crate::tokens::Palette {
        use crate::tokens::{Contrast, DARK_MORE_CONTRAST, LIGHT_MORE_CONTRAST, contrast_palette, palette};
        match self.contrast {
            Contrast::Theme(colors) => contrast_palette(&colors),
            Contrast::More if self.dark() => DARK_MORE_CONTRAST,
            Contrast::More => LIGHT_MORE_CONTRAST,
            Contrast::Standard => *palette(self.dark()),
        }
    }

    /// The colours that do not follow light and dark: the design's, or a Windows contrast theme's own.
    pub fn fixed_colors(&self) -> crate::tokens::FixedColors {
        match self.contrast {
            crate::tokens::Contrast::Theme(colors) => crate::tokens::contrast_fixed(&colors),
            _ => crate::tokens::FIXED,
        }
    }

    /// Attaches the per-user record (C5.1) and applies it: language, theme, release key and the rest of the
    /// per-user settings. Call before the first paint. A record that cannot be used reads as the defaults (a trace
    /// line, never a failure).
    pub fn load_prefs(&mut self, store: Box<dyn crate::prefs::PrefsStore>) {
        // No record yet (first start): the language the window started with, the system's, stays, and the guide shows.
        self.guide_open = matches!(store.read(), Ok(None));
        let p = crate::prefs::load_or(&*store, crate::prefs::UserPrefs { language: self.lang, ..Default::default() });
        self.lang = p.language;
        self.theme = p.theme;
        self.release_key = p.release_key;
        if let Some(s) = self.settings.take() {
            self.settings = Some(p.overlay(s));
        }
        self.ui = Some(p);
        self.prefs_store = Some(store);
    }

    /// The per-user settings in force (what `ui.bin` holds), for the shells that start the viewer pipeline with them.
    pub fn user_prefs(&self) -> crate::prefs::UserPrefs {
        let mut p = match &self.settings {
            Some(s) => crate::prefs::UserPrefs::from_settings(s, self.release_key),
            None => self.ui.unwrap_or_default(),
        };
        p.language = self.lang;
        p.theme = self.theme;
        p.release_key = self.release_key;
        p
    }

    /// Writes `ui.bin` when a store is attached and a per-user field changed since the last write.
    pub(crate) fn save_prefs(&mut self) {
        if self.prefs_store.is_none() || self.ui == Some(self.user_prefs()) {
            return;
        }
        self.write_prefs();
    }

    /// Takes the per-user settings in force as the record and writes it, changed or not.
    fn write_prefs(&mut self) {
        let Some(store) = &self.prefs_store else { return };
        let p = self.user_prefs();
        self.ui = Some(p);
        crate::prefs::save(&**store, &p);
    }

    /// What the system itself says about opening at login (macOS: the login item is registered or not). The switch
    /// shows it and the per-user record keeps it, like a change made in the window; the service's copy never wins
    /// over it.
    pub fn set_open_at_login(&mut self, on: bool) {
        if let Some(p) = &mut self.ui {
            p.open_at_login = on;
        }
        if let Some(s) = &mut self.settings {
            s.open_at_login = on;
        }
        self.write_prefs();
    }

    pub fn set_theme(&mut self, theme: Theme) -> Changed {
        let before = self.dark();
        self.theme = theme;
        if let Some(set) = &mut self.settings {
            set.theme = theme;
        }
        self.save_prefs();
        if self.dark() == before { Changed::NONE } else { Changed::ALL }
    }

    pub fn set_language(&mut self, lang: Language) -> Changed {
        if self.lang == lang {
            return Changed::NONE;
        }
        self.lang = lang;
        self.save_prefs();
        Changed::ALL
    }

    pub fn navigate(&mut self, nav: Nav) -> Changed {
        self.nav = nav;
        self.rename = None;
        self.crash_notice = false;
        Changed::MAIN
    }

    pub fn select_device(&mut self, id: &str) -> Changed {
        if self.devices.iter().any(|d| d.id == id) {
            self.selected = Some(id.to_owned());
            self.rename = None;
            if self.connect_end.as_ref().is_some_and(|e| e.device_id != id) {
                self.connect_end = None;
            }
            self.crash_notice = false;
            self.nav = Nav::Devices;
            self.tab = DeviceTab::Overview;
        }
        Changed::MAIN
    }

    pub fn select_tab(&mut self, tab: DeviceTab) -> Changed {
        // Opening Permissões of a paired device asks the service for what the other side allows now (op 39).
        if tab == DeviceTab::Permissions
            && self.tab != DeviceTab::Permissions
            && let Some(d) = self.selected_device().filter(|d| d.paired)
        {
            self.followups.push(Command::RefreshGrants { device_id: d.id.clone() });
        }
        self.tab = tab;
        self.rename = None;
        Changed::MAIN
    }

    pub fn open_drawer(&mut self, tab: Option<DrawerTab>) -> Changed {
        self.drawer = tab;
        Changed::VIEWER
    }

    /// Remembers which command a ticket belongs to, so its reply can be understood (errors, undo).
    pub fn track(&mut self, ticket: Ticket, command: &Command) {
        let undo = if matches!(
            command,
            Command::SetGrants { .. }
                | Command::SetSettings { .. }
                | Command::SetAudio { .. }
                | Command::SetClipboardSync { .. }
                | Command::SetGrab { .. }
                | Command::SetScale { .. }
                | Command::SetQuality { .. }
                | Command::SetViewOnly { .. }
                | Command::SetReleaseKey { .. }
        ) {
            self.undo_slot.take()
        } else {
            None
        };
        self.undo_slot = None;
        self.pending.insert(ticket, Pending { command: command.clone(), undo, age_s: 0, limit_s: command.timeout_s() });
    }

    // ----- fetch states (R-UI-1, R-UI-3, R-UI-6) -----

    /// Where a kind of data stands.
    pub fn fetch(&self, kind: DataKind) -> Fetch {
        self.fetch.get(&kind).copied().unwrap_or(Fetch::Missing)
    }

    /// True when placeholder blocks must be drawn for `kind` (only after the 100 ms of silence).
    pub fn placeholder(&self, kind: DataKind) -> bool {
        self.fetch(kind) == Fetch::Placeholder
    }

    /// A request for `kind` goes out. Ready data stays on screen while it refreshes; missing data waits for the
    /// placeholder timer.
    pub fn begin_fetch(&mut self, kind: DataKind) {
        if self.fetch(kind) == Fetch::Missing {
            self.fetch.insert(kind, Fetch::Pending);
        }
    }

    pub(crate) fn finish_fetch(&mut self, kind: DataKind) {
        self.fetch.insert(kind, Fetch::Ready);
    }

    pub(crate) fn fail_fetch(&mut self, kind: DataKind) {
        if kind == DataKind::Activity {
            self.activity_refresh = false;
        }
        if self.fetch(kind) != Fetch::Ready {
            self.fetch.insert(kind, Fetch::Missing);
        }
    }

    /// True while some data waits for its first answer: the shell then keeps a 100 ms one-shot timer.
    pub fn has_pending_fetch(&self) -> bool {
        self.fetch.values().any(|f| *f == Fetch::Pending)
    }

    /// 100 ms passed since the requests went out: whatever still waits shows placeholders.
    pub fn arm_placeholders(&mut self) -> Changed {
        let mut changed = Changed::NONE;
        for f in self.fetch.values_mut() {
            if *f == Fetch::Pending {
                *f = Fetch::Placeholder;
                changed = Changed::ALL;
            }
        }
        changed
    }

    /// The commands the UI asks for at launch, all at once (R-UI-6): identity, network, device list, settings with
    /// the blocked devices, and activity. The window is drawn before any answer arrives.
    pub fn startup_commands(&mut self) -> Vec<Command> {
        let commands = vec![
            Command::GetThisDevice,
            Command::GetNetwork,
            Command::ListDevices,
            Command::GetSettings,
            Command::ListBlocked,
            Command::ActivityList { before_ms: None, limit: 200 },
            Command::GetUpdate,
        ];
        self.blocked_asked = true;
        for c in &commands {
            if let Some(k) = fetch_kind(c) {
                self.begin_fetch(k);
            }
        }
        commands
    }

    /// One second passed: counts every countdown down by one and fails the commands that waited too long (R-UI-7).
    pub fn tick(&mut self) -> Changed {
        // The download's rate over the last second, for its time left.
        if let Some(crate::api::UpdateInfo { phase: crate::api::UpdatePhase::Downloading { done, .. }, .. }) =
            &self.update
        {
            if let Some(prev) = self.update_prev_done {
                let now = done.saturating_sub(prev);
                // A light smoothing: the time left does not jump with each second.
                self.update_rate = if self.update_rate == 0 { now } else { (self.update_rate * 3 + now) / 4 };
            }
            self.update_prev_done = Some(*done);
        }
        let mut changed = Changed::NONE;
        if self.devices.is_empty()
            && self.fetch(DataKind::Devices) == Fetch::Ready
            && self.empty_s < Self::NO_COMPUTER_AFTER_S
        {
            self.empty_s += 1;
            if self.empty_s == Self::NO_COMPUTER_AFTER_S {
                changed = changed.union(Changed::MAIN);
            }
        }
        let mut expired: Vec<Ticket> = Vec::new();
        for (t, p) in &mut self.pending {
            p.age_s += 1;
            if p.age_s >= p.limit_s {
                expired.push(*t);
            }
        }
        for t in expired {
            changed = changed.union(self.apply_reply(t, Err(ApiError::new(ApiCode::Timeout))));
        }
        if let Some(c) = &mut self.confirm
            && c.answer.is_none()
        {
            c.remaining_s = c.remaining_s.saturating_sub(1);
            if c.remaining_s == 0 {
                // The ticket ran out: the window closes as declined.
                c.answer = Some(false);
            }
            changed = changed.union(Changed::MAIN);
        }
        if let Some(p) = &mut self.pairing
            && let PairingState::Open { remaining_s } = &mut p.state
            && *remaining_s > 0
        {
            *remaining_s -= 1;
            changed = changed.union(Changed::MAIN);
        }
        if let Some(i) = &mut self.incoming
            && i.remaining_s > 0
        {
            i.remaining_s -= 1;
            changed = changed.union(Changed::MAIN);
        }
        if let Some(SwapView { phase: SwapPhase::Asked { remaining_s, .. }, .. }) = &mut self.swap
            && *remaining_s > 0
        {
            *remaining_s -= 1;
            changed = changed.union(Changed::MAIN);
        }
        if self.swap_requested() && self.swap_left_s > 0 {
            self.swap_left_s -= 1;
            changed = changed.union(Changed::ALL);
        }
        if let Some(w) = &mut self.consent_wait
            && w.remaining_s > 0
        {
            w.remaining_s -= 1;
            changed = changed.union(Changed::VIEWER);
        }
        let mut gave_up: Vec<(u64, String)> = Vec::new();
        for s in &mut self.sessions {
            if let Some(left) = &mut s.reconnect_left_s
                && *left > 0
            {
                *left -= 1;
                changed = changed.union(Changed::ALL);
            }
            match &mut s.state {
                SessionState::Waking { remaining_s } | SessionState::Prompted { remaining_s } if *remaining_s > 0 => {
                    *remaining_s -= 1;
                    changed = changed.union(Changed::MAIN);
                }
                _ => {}
            }
            // News that never comes (a service that died) must not leave the card up for ever.
            if matches!(s.state, SessionState::Connecting | SessionState::Prompted { .. }) {
                s.age_s += 1;
                if s.age_s >= CONNECT_GIVE_UP_S {
                    gave_up.push((s.id, s.device_id.clone()));
                }
            }
        }
        for (id, device_id) in gave_up {
            self.sessions.retain(|s| !(s.id == id && s.device_id == device_id));
            if id != 0 {
                self.followups.push(Command::Disconnect { session_id: id });
            }
            self.end_connect(id, device_id, ConnectWhy::Failed, None);
            changed = changed.union(Changed::ALL);
        }
        changed
    }

    // ----- events -----

    pub fn apply(&mut self, event: UiEvent) -> Changed {
        match event {
            UiEvent::Reply { ticket, result } => self.apply_reply(ticket, result),
            UiEvent::DevicesChanged(devices) => {
                // The service writes a session's start and end, and a pairing, to the log before it pushes the device
                // list: the list's push is when Início and Atividade read the log again.
                if self.fetch(DataKind::Activity) == Fetch::Ready && !self.activity_refresh {
                    self.activity_refresh = true;
                    self.followups.push(Command::ActivityList { before_ms: None, limit: 200 });
                }
                self.set_devices(devices)
            }
            UiEvent::NetworkChanged(n) => {
                self.network = Some(n);
                self.finish_fetch(DataKind::Network);
                Changed::MAIN
            }
            UiEvent::Pairing { pairing_id, state } => {
                let device_id = self.pairing.as_ref().and_then(|p| p.device_id.clone());
                if let PairingState::Done { device_id: new } = &state {
                    self.last_new_device = Some(new.clone());
                }
                self.pairing = Some(PairingView { id: pairing_id, device_id, state });
                self.pair_confirming = false;
                Changed::MAIN
            }
            UiEvent::IncomingRequest(request) => {
                let allow = request.preset;
                let remaining_s = request.timeout_s;
                self.incoming = Some(IncomingView { request, remaining_s, allow, remember_unattended: false });
                Changed::MAIN
            }
            UiEvent::IncomingClosed { request_id, .. } => {
                if self.incoming.as_ref().is_some_and(|i| i.request.request_id == request_id) {
                    self.incoming = None;
                    Changed::MAIN
                } else {
                    Changed::NONE
                }
            }
            UiEvent::Session { session_id, device_id, state } => self.apply_session(session_id, device_id, state),
            UiEvent::Displays { session_id, displays } => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.set_displays(displays);
                    Changed::VIEWER
                } else {
                    Changed::NONE
                }
            }
            UiEvent::Stats { session_id, stats } => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.stats = Some(stats);
                    if self.settings.as_ref().is_some_and(|s| s.show_stats) {
                        return Changed::VIEWER;
                    }
                }
                Changed::NONE
            }
            UiEvent::Swap { session_id, event } => {
                match event {
                    SwapEvent::Asked { request_id, by, remaining_s } => {
                        self.swap =
                            Some(SwapView { session_id, phase: SwapPhase::Asked { request_id, by, remaining_s } });
                    }
                    SwapEvent::Declined { why } => {
                        if self.viewer_session().is_some() {
                            self.swap = Some(SwapView { session_id, phase: SwapPhase::Declined(why) });
                        } else {
                            // The asker is the host: there is no viewer window to carry the card, so the strip says why.
                            self.swap = None;
                            let peer = self.session_peer_name(session_id);
                            self.error = Some(self.swap_why_error(why, &peer));
                        }
                    }
                    SwapEvent::Done => self.swap = None,
                }
                Changed::ALL
            }
            UiEvent::Chat { session_id, mut message } => {
                // Each side numbers its own messages from 1: the same number can be mine and theirs.
                let theirs = !message.mine;
                let id = message.msg_id;
                // With the panel open the message is read as it arrives: the peer is told.
                if theirs && self.drawer == Some(DrawerTab::Chat) && self.viewer_session().is_some() {
                    message.read = true;
                    self.followups.push(Command::ChatMarkRead { session_id, up_to: id });
                }
                if let Some(m) = self.chat.iter_mut().find(|m| m.msg_id == id && m.mine == message.mine) {
                    *m = message;
                } else {
                    self.chat.push(message);
                }
                Changed::ALL
            }
            UiEvent::ChatRead { up_to, .. } => {
                for m in self.chat.iter_mut().filter(|m| m.mine && m.msg_id <= up_to) {
                    m.read = true;
                }
                Changed::VIEWER
            }
            UiEvent::Term { term_id, event } => {
                match event {
                    TermEvent::Opened { account } => {
                        self.consent_answered(Feature::Terminal);
                        let admin = matches!(self.term, TermPhase::Opening { admin: true });
                        self.term_account = Some(account);
                        self.term_id = Some(term_id);
                        self.term = TermPhase::Open { admin };
                    }
                    TermEvent::Exited { code } => {
                        // An end that names an older terminal (one that was reopened since) changes nothing.
                        if self.term_id.is_some_and(|id| id != term_id) {
                            return Changed::NONE;
                        }
                        let admin = self.term_is_admin();
                        self.term_account = None;
                        self.term = TermPhase::Exited { code, admin };
                    }
                }
                Changed::VIEWER
            }
            UiEvent::Transfer { transfer_id, session_id, name, outgoing, state } => {
                self.system_drag_transfer(transfer_id, session_id, &name, outgoing, &state);
                // A transfer that moves or ends was answered (a refusal fails it).
                if !matches!(state, TransferState::Queued) {
                    self.consent_answered(Feature::Files);
                }
                if let Some(t) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
                    t.state = state;
                } else {
                    self.transfers.push(TransferView {
                        id: transfer_id,
                        session_id,
                        name,
                        outgoing,
                        state,
                        saved_as: None,
                    });
                }
                Changed::VIEWER
            }
            UiEvent::TransferSavedAs { transfer_id, name, .. } => {
                self.system_drag_saved_as(transfer_id, &name);
                if let Some(t) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
                    t.saved_as = Some(name);
                }
                Changed::VIEWER
            }
            UiEvent::RemoteVolumes { session_id, volumes } => self.there_volumes_event(session_id, volumes),
            UiEvent::ServiceReconnected => {
                // The sessions of the old service ended with it.
                let ended: Vec<(u64, String)> = self.sessions.iter().map(|s| (s.id, s.device_id.clone())).collect();
                for (session_id, device_id) in ended {
                    self.apply(UiEvent::Session {
                        session_id,
                        device_id,
                        state: SessionState::Ended { reason: Some(ApiCode::ServiceDown) },
                    });
                }
                let again = self.startup_commands();
                self.followups.extend(again);
                Changed::ALL
            }
            UiEvent::ConsentPending { session_id, feature, remaining_s } => {
                if self.viewer_session().is_none_or(|s| s.id != session_id) {
                    return Changed::NONE;
                }
                self.consent_wait = Some(ConsentWait { feature, remaining_s, total_s: remaining_s });
                Changed::VIEWER
            }
            UiEvent::ClipboardSkipped { reason, .. } => {
                // The code carries why (C2.4 notify 14): too large, marked private, a format not carried.
                let key = match reason {
                    ApiCode::Invalid => MsgKey::ClipSkippedTooLarge,
                    ApiCode::NotPermitted => MsgKey::ClipSkippedConcealed,
                    _ => MsgKey::ClipSkippedUnsupported,
                };
                self.error = Some(ApiError { code: reason, detail: Some(Msg::new(key)) });
                Changed::ALL
            }
            UiEvent::ActivityAlert(a) => {
                self.activity.insert(0, a.clone());
                self.alert = Some(a);
                Changed::MAIN
            }
            UiEvent::SettingsChanged(s) => {
                self.set_settings(s);
                Changed::ALL
            }
            UiEvent::ElevationResult { action, run, result } => self.apply_elevation_result(action, run, result),
            UiEvent::ThemeChanged { dark } => self.set_dark(dark),
            UiEvent::LanguageChanged(l) => self.set_language(l),
            UiEvent::UpdateChanged(u) => self.set_update(u),
            UiEvent::UpdateRestarting => {
                self.quit_request = true;
                Changed::ALL
            }
        }
    }

    /// The updater's new view (UPDATES.md 10.2).
    pub(crate) fn set_update(&mut self, u: crate::api::UpdateInfo) -> Changed {
        use crate::api::UpdatePhase as P;
        match &u.phase {
            P::Downloading { total, .. } => self.update_size = *total,
            _ => {
                self.update_prev_done = None;
                self.update_rate = 0;
            }
        }
        // The confirmation goes once the install runs or the offer is gone.
        if !matches!(u.phase, P::Offered { .. }) {
            self.update_confirm = false;
        }
        self.update = Some(u);
        Changed::ALL
    }

    /// The page an [`Effect::OpenWebPage`] opens (once).
    pub fn take_web_page(&mut self) -> Option<String> {
        self.web_page.take()
    }

    /// The shell asks after each batch of events whether to close for an update (once).
    pub fn take_quit_request(&mut self) -> bool {
        std::mem::take(&mut self.quit_request)
    }

    fn set_settings(&mut self, s: Settings) {
        // The language is per user (C5.1): the service does not keep it and always answers its default, so the UI's
        // own choice wins over the service's copy.
        let s = match &self.ui {
            Some(p) => p.overlay(s),
            None => Settings { language: self.lang, theme: self.theme, ..s },
        };
        self.settings = Some(s);
        self.finish_fetch(DataKind::Settings);
    }

    fn set_devices(&mut self, devices: Vec<DeviceView>) -> Changed {
        self.devices = devices;
        if !self.devices.is_empty() {
            self.empty_s = 0;
        }
        self.finish_fetch(DataKind::Devices);
        if self.selected.as_ref().is_none_or(|id| !self.devices.iter().any(|d| &d.id == id)) {
            self.selected = self.devices.iter().find(|d| d.paired).or(self.devices.first()).map(|d| d.id.clone());
        }
        Changed::MAIN
    }

    /// A connect attempt ended without a session: its card says why, and the focus goes to "Fechar" when that card
    /// is the page on screen.
    fn end_connect(&mut self, entry: u64, device_id: String, why: ConnectWhy, intent: Option<Intent>) {
        if self.selected_device().is_some_and(|d| d.id == device_id) {
            self.focus_request = Some(ControlId::ConnectClose);
        }
        self.connect_end = Some(ConnectEnd { entry, device_id, why, intent });
    }

    /// Takes the entry a click made, which has no id until the service answers.
    fn take_unanswered_entry(&mut self, device_id: &str) -> Option<SessionView> {
        let at = self.sessions.iter().position(|s| s.id == 0 && s.device_id == device_id && s.is_waiting_entry())?;
        Some(self.sessions.remove(at))
    }

    fn apply_session(&mut self, id: u64, device_id: String, state: SessionState) -> Changed {
        if let SessionState::NotOpened { why } = state {
            let entry = match self.sessions.iter().position(|s| s.id == id) {
                Some(at) => Some(self.sessions.remove(at)),
                None => self.take_unanswered_entry(&device_id),
            };
            let device_id = entry.as_ref().map_or(device_id, |e| e.device_id.clone());
            self.end_connect(id, device_id, why, entry.and_then(|e| e.intent));
            return Changed::ALL;
        }
        if let SessionState::Ended { reason } = &state {
            let before = self.sessions.iter().find(|s| s.id == id).map(|s| (s.state.clone(), s.device_id.clone()));
            let entry = self.sessions.iter().find(|s| s.id == id && s.is_waiting_entry()).cloned();
            self.sessions.retain(|s| s.id != id);
            // An entry that ends with a code (not a normal end) did not open: the card says it did not.
            if let Some(e) = &entry
                && reason.is_some()
                && !matches!(e.state, SessionState::Waking { .. })
            {
                self.end_connect(id, e.device_id.clone(), ConnectWhy::Failed, e.intent);
            }
            if self.viewer_session().is_none() {
                self.view_only = false;
                self.fullscreen = false;
                self.bar_shown = false;
                self.bar_collapsed = false;
                self.quality = Quality::Auto;
                self.drawer = None;
                self.chat.clear();
                self.transfers.clear();
                self.listing = None;
                self.files.waiting_download = None;
                self.fetch.remove(&DataKind::Listing);
                self.term_account = None;
                self.term_id = None;
                self.term = TermPhase::Idle;
                self.consent_wait = None;
                if matches!(
                    self.swap.as_ref().map(|s| &s.phase),
                    Some(SwapPhase::Confirm | SwapPhase::Requested { .. })
                ) {
                    self.swap = None;
                }
            }
            // Only a refusal or a silence of a session is worth a line (an entry's card says its own); any other end
            // is the user's own.
            if entry.is_none()
                && let Some(reason @ (ApiCode::Refused | ApiCode::Timeout | ApiCode::Busy)) = reason
            {
                self.error = Some(ApiError::new(*reason));
            }
            // The minute of reconnecting or of waking ran out: say which.
            if let (Some(reason), Some((prev, device))) = (reason, before) {
                let key = match prev {
                    SessionState::Reconnecting { .. } => Some(MsgKey::ReconnectGaveUp),
                    SessionState::Waking { .. } => Some(MsgKey::WakeGaveUp),
                    _ => None,
                };
                if let Some(key) = key {
                    let name = self.device_by_id(&device).map(|d| d.name.clone()).unwrap_or_default();
                    self.error = Some(ApiError { code: *reason, detail: Some(Msg::new(key).with(name)) });
                }
            }
            return Changed::ALL;
        }
        let waiting =
            matches!(state, SessionState::Connecting | SessionState::Prompted { .. } | SessionState::Waking { .. });
        if waiting && self.connect_end.as_ref().is_some_and(|e| e.device_id == device_id) {
            self.connect_end = None;
        }
        // The service's first word about the entry a click made: it takes the id.
        if waiting
            && !self.sessions.iter().any(|s| s.id == id)
            && let Some(at) =
                self.sessions.iter().position(|s| s.id == 0 && s.device_id == device_id && s.is_waiting_entry())
        {
            self.sessions[at].id = id;
            if self.sessions[at].cancel_asked {
                self.followups.push(Command::Disconnect { session_id: id });
            }
        }
        let was_active = self.sessions.iter().any(|s| s.id == id && matches!(s.state, SessionState::Active { .. }));
        if !was_active && matches!(state, SessionState::Active { role: Role::Viewer, .. }) {
            // The host captures sound only once it is told to (D16): the toolbar's switch is sent as the session
            // comes up, or the button shows on while nothing plays.
            self.followups.push(Command::SetAudio { session_id: id, on: self.audio_on });
        }
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            s.set_state(state);
            s.device_id = device_id;
        } else {
            self.sessions.push(SessionView::new(id, device_id, state));
        }
        Changed::ALL
    }

    fn apply_elevation_result(&mut self, action: ElevatedAction, run: u32, result: Result<(), ApiError>) -> Changed {
        // Only the run this step waits for: a worker of a step that was cancelled, or of an earlier press, may end
        // later and must not decide a newer step.
        if self.elevation.as_ref().is_none_or(|e| e.action != action || e.run != run || e.phase != ElevPhase::Waiting) {
            return Changed::NONE;
        }
        match result {
            Ok(()) => {
                self.elevation = None;
                // The command that needed the administrator goes again: the service now lets it through once.
                match action {
                    ElevatedAction::SetGrants { device_id, grants } => {
                        self.followups.push(Command::SetGrants { device_id, grants });
                    }
                    ElevatedAction::ElevateSession { session_id } => {
                        self.followups.push(Command::ElevateSession { session_id });
                    }
                    // The administrator terminal of that session, if its window is still this one's.
                    ElevatedAction::OpenAdminTerminal { session_id } => {
                        if self.session_id() == Some(session_id) {
                            if let (TermPhase::Open { .. }, Some(term_id)) = (&self.term, self.term_id) {
                                self.followups.push(Command::TermClose { term_id });
                            }
                            self.term = TermPhase::Opening { admin: true };
                            self.term_id = None;
                            self.term_account = None;
                            self.followups.push(Command::TermOpen { session_id, admin: true, cols: 80, rows: 24 });
                        }
                    }
                }
            }
            Err(e) => {
                if let Some(el) = &mut self.elevation {
                    el.phase = ElevPhase::Failed(e.code);
                }
            }
        }
        Changed::ALL
    }

    fn apply_reply(&mut self, ticket: Ticket, result: Result<Reply, ApiError>) -> Changed {
        let pending = self.pending.remove(&ticket);
        match result {
            Ok(reply) => self.apply_ok(pending, reply),
            Err(error) => self.apply_err(pending, error),
        }
    }

    fn apply_ok(&mut self, pending: Option<Pending>, reply: Reply) -> Changed {
        match reply {
            Reply::Done => match pending.map(|p| p.command) {
                Some(Command::ForgetDevice { device_id }) => {
                    self.devices.retain(|d| d.id != device_id);
                    self.selected = self.devices.iter().find(|d| d.paired).map(|d| d.id.clone());
                    self.tab = DeviceTab::Overview;
                    Changed::MAIN
                }
                Some(Command::Disconnect { session_id }) => {
                    self.sessions.retain(|s| s.id != session_id);
                    Changed::ALL
                }
                // The service stored the trimmed name; the pushed device list says so too, this just does not wait.
                Some(
                    c @ (Command::LocalMakeDir { .. }
                    | Command::LocalRename { .. }
                    | Command::LocalMove { .. }
                    | Command::LocalTrash { .. }
                    | Command::LocalDeleteForever { .. }),
                ) => self.manage_done(&c),
                Some(Command::RenameDevice { device_id, name }) => {
                    if let Some(d) = self.devices.iter_mut().find(|d| d.id == device_id) {
                        d.name = name;
                    }
                    if self.rename.as_ref().is_some_and(|r| r.device_id == device_id) {
                        self.rename = None;
                    }
                    Changed::ALL
                }
                _ => Changed::NONE,
            },
            Reply::ThisDevice(t) => {
                self.this = Some(t);
                self.finish_fetch(DataKind::ThisDevice);
                Changed::ALL
            }
            Reply::Network(n) => {
                self.network = Some(n);
                self.finish_fetch(DataKind::Network);
                Changed::ALL
            }
            Reply::Devices(d) => self.set_devices(d),
            Reply::Device(d) => {
                if let Some(slot) = self.devices.iter_mut().find(|x| x.id == d.id) {
                    *slot = d;
                } else {
                    self.devices.push(d);
                }
                Changed::MAIN
            }
            Reply::PairingId(id) => {
                if let Some(p) = &mut self.pairing {
                    p.id = id;
                    // "Parear com <device>": this side is the initiator, so the ceremony starts as soon as the
                    // service has opened pairing mode (D12 still needs the other user to have opened theirs).
                    if let Some(device_id) = p.device_id.clone() {
                        self.followups.push(Command::PairStart { pairing_id: id, device_id });
                    }
                } else {
                    self.pairing = Some(PairingView {
                        id,
                        device_id: None,
                        state: PairingState::Open { remaining_s: PAIR_COUNTDOWN_S },
                    });
                }
                Changed::MAIN
            }
            Reply::SessionId(id) => match pending.map(|p| p.command) {
                // The click made its entry already: the reply gives it the id.
                Some(Command::Connect { device_id, .. }) => {
                    if self.sessions.iter().any(|s| s.id == id) {
                        return Changed::NONE;
                    }
                    match self.sessions.iter_mut().find(|s| s.id == 0 && s.device_id == device_id) {
                        Some(entry) => {
                            entry.id = id;
                            if entry.cancel_asked {
                                self.followups.push(Command::Disconnect { session_id: id });
                            }
                        }
                        // The card ended before the answer came (the interface gave up): stop what the service began.
                        None => self.followups.push(Command::Disconnect { session_id: id }),
                    }
                    Changed::MAIN
                }
                // "Acordar": the entry that shows "Acordando" until the device appears or the minute ends.
                Some(Command::Wake { device_id }) if !self.sessions.iter().any(|s| s.id == id) => {
                    if self.connect_end.as_ref().is_some_and(|e| e.device_id == device_id) {
                        self.connect_end = None;
                    }
                    let state = SessionState::Waking { remaining_s: WAKE_COUNTDOWN_S };
                    self.sessions.push(SessionView::new(id, device_id, state));
                    Changed::ALL
                }
                _ => Changed::NONE,
            },
            Reply::RequestId(request_id) => {
                if let Some(s) = &mut self.swap
                    && matches!(s.phase, SwapPhase::Requested { .. })
                {
                    s.phase = SwapPhase::Requested { request_id };
                }
                Changed::ALL
            }
            Reply::LocalListing(l) => {
                let asked = pending.map(|p| p.command);
                match self.walk_listed(asked.as_ref(), l) {
                    Ok(changed) => changed,
                    Err(l) => self.here_listed(l),
                }
            }
            Reply::LocalProps(p) => self.props_counted(p),
            Reply::LocalPlaces(p) => self.places_listed(p),
            Reply::LocalKinds(k) => self.drop_resolved(k),
            Reply::Listing(l) => {
                self.consent_answered(Feature::Files);
                // A 1.1 peer lists through pages (slice 2b); a late FilesList answer is not the pane's any more.
                if self.there_v11() {
                    return Changed::NONE;
                }
                self.listing = Some(l);
                self.files.page = 0;
                self.finish_fetch(DataKind::Listing);
                Changed::ALL
            }
            Reply::Chat(m) => self.apply(UiEvent::Chat { session_id: 0, message: m }),
            Reply::Activity(a) => {
                // A refresh leaves the alert as it is: a new one arrives as `ActivityAlert`, and one the user closed
                // stays closed.
                if self.fetch(DataKind::Activity) != Fetch::Ready {
                    self.alert = a.iter().find(|x| x.alert).cloned();
                }
                self.activity_refresh = false;
                self.activity = a;
                self.finish_fetch(DataKind::Activity);
                Changed::ALL
            }
            Reply::Settings(s) => {
                self.set_settings(s);
                Changed::ALL
            }
            Reply::Blocked(list) => {
                self.blocked = list;
                self.blocked_asked = true;
                Changed::MAIN
            }
            Reply::Update(u) => self.set_update(u),
            Reply::MissingPermissions(missing) => {
                if let Some(n) = &mut self.network {
                    n.notices.retain(|x| !matches!(x, Notice::MacPermissionsMissing(_)));
                    if !missing.is_empty() {
                        n.notices.push(Notice::MacPermissionsMissing(missing));
                    }
                }
                Changed::MAIN
            }
            Reply::Displays(d) => {
                // The listing belongs to the session its command named (the reply carries no id of its own).
                let Some(Command::ListDisplays { session_id }) = pending.map(|p| p.command) else {
                    return Changed::NONE;
                };
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.set_displays(d);
                    return Changed::VIEWER;
                }
                Changed::NONE
            }
            Reply::TermId(id) => {
                self.term_id = Some(id);
                Changed::VIEWER
            }
            Reply::TransferIds(_) | Reply::TransferId(_) => Changed::NONE,
            Reply::RemotePage(page) => {
                let asked = match pending.map(|p| p.command) {
                    Some(Command::FilesListPage { path, start, .. }) => Some((path, start)),
                    _ => None,
                };
                self.there_page(asked, page)
            }
            Reply::ChangeResults(results) => self.there_change_results(pending.map(|p| p.command), results),
            Reply::RemoteVolumes(v) => self.there_volumes(v),
        }
    }

    fn apply_err(&mut self, pending: Option<Pending>, error: ApiError) -> Changed {
        let Some(pending) = pending else {
            self.error = Some(error);
            return Changed::ALL;
        };
        // The updater's refusals have their own place (UPDATES.md 10): a service without an updater (a development
        // instance) answers NotFound and the interface shows nothing; a live session asks for the confirmation.
        match &pending.command {
            Command::UpdateInstall { end_sessions: false, .. } if error.code == ApiCode::Busy => {
                self.update_confirm = true;
                return Changed::ALL;
            }
            Command::GetUpdate
            | Command::UpdateCheckNow
            | Command::UpdateInstall { .. }
            | Command::UpdateCancel
            | Command::UpdateDismiss { .. }
            | Command::UpdateSetCheck { .. } => return Changed::NONE,
            _ => {}
        }
        // The explorer's own disk: the pane shows the failure in place (FILES-UI.md 2.9), no strip.
        match &pending.command {
            Command::LocalList { path } => {
                if self.walk_list_failed(path.as_ref()) {
                    return Changed::VIEWER;
                }
                return self.here_failed(error.code);
            }
            Command::FilesVolumes { .. } => return self.there_volumes_failed(error.code),
            c @ Command::FilesListPage { .. } => return self.there_page_failed(c, error.code),
            c @ Command::FilesChange { .. } => return self.there_change_failed(c, error.code),
            Command::LocalPlaces => return Changed::NONE,
            Command::LocalKinds { .. } => return self.drop_failed(),
            c @ (Command::LocalMakeDir { .. }
            | Command::LocalRename { .. }
            | Command::LocalMove { .. }
            | Command::LocalTrash { .. }
            | Command::LocalDeleteForever { .. }
            | Command::LocalProps { .. }) => return self.manage_failed(c, error.code),
            _ => {}
        }
        // The step tells how the confirmation failed (a timeout of its own 120 s too): no strip over the content.
        if let Command::Elevate { action, .. } = &pending.command {
            return match &mut self.elevation {
                Some(el) if el.action == *action => {
                    el.phase = ElevPhase::Failed(error.code);
                    Changed::ALL
                }
                _ => Changed::NONE,
            };
        }
        // The click's card goes; a refusal that is not the elevation step ends it with its why.
        if let Command::Connect { device_id, unattended, .. } = &pending.command {
            let entry = self.take_unanswered_entry(device_id);
            if !(*unattended && error.code == ApiCode::NeedsAdmin) {
                let why = if error.code == ApiCode::NotFound { ConnectWhy::Unreachable } else { ConnectWhy::Failed };
                self.end_connect(0, device_id.clone(), why, entry.and_then(|e| e.intent));
                return Changed::ALL;
            }
        }
        match pending.undo {
            Some(Undo::Grants(device_id, grants)) => {
                if let Some(d) = self.devices.iter_mut().find(|d| d.id == device_id) {
                    d.grants_here = grants;
                }
            }
            Some(Undo::Settings(old)) => {
                self.lang = old.language;
                self.theme = old.theme;
                if self.ui.is_some() {
                    self.ui = Some(crate::prefs::UserPrefs::from_settings(&old, self.release_key));
                }
                self.set_settings(*old);
                self.write_prefs();
            }
            Some(Undo::Quality(old)) => self.quality = old,
            Some(Undo::Toggle(which, old)) => match which {
                Toggle::Audio => self.audio_on = old,
                Toggle::Clipboard => self.clipboard_on = old,
                Toggle::Grab => self.grab_on = old,
                Toggle::Fit => self.fit = old,
                Toggle::ViewOnly => self.view_only = old,
            },
            None => {}
        }
        if let Some(kind) = fetch_kind(&pending.command) {
            self.fail_fetch(kind);
        }
        // A refused or lost request ends the question about it.
        match pending.command {
            Command::TermOpen { .. } => self.consent_answered(Feature::Terminal),
            Command::FilesList { .. } | Command::FilesSend { .. } => self.consent_answered(Feature::Files),
            _ => {}
        }
        if matches!(pending.command, Command::PairConfirm { .. }) {
            self.pair_confirming = false;
        }
        // A swap request the service refused or lost never gets its outcome: the buttons must not wait for it.
        if matches!(pending.command, Command::RequestSwap { .. }) && self.swap_requested() {
            self.swap = None;
        }
        if let Command::TermOpen { admin, .. } = pending.command
            && error.code != ApiCode::NeedsAdmin
        {
            // The panel says why, in place: no error strip over the viewer.
            self.term = TermPhase::Failed { why: term_failure(&error), admin };
            self.term_id = None;
            return Changed::VIEWER;
        }
        if error.code == ApiCode::NeedsAdmin {
            if matches!(pending.command, Command::TermOpen { .. }) {
                self.term = TermPhase::Idle;
            }
            let action = match &pending.command {
                Command::SetGrants { device_id, grants } => {
                    Some(ElevatedAction::SetGrants { device_id: device_id.clone(), grants: *grants })
                }
                Command::ElevateSession { session_id } => {
                    Some(ElevatedAction::ElevateSession { session_id: *session_id })
                }
                Command::TermOpen { session_id, admin: true, .. } => {
                    Some(ElevatedAction::OpenAdminTerminal { session_id: *session_id })
                }
                _ => None,
            };
            if let Some(action) = action {
                self.elevation = Some(Elevation { action, phase: ElevPhase::Ask, run: 0 });
                return Changed::ALL;
            }
        }
        // A refused rename is told inline, under the field that is still open.
        if let Command::RenameDevice { device_id, .. } = &pending.command
            && let Some(r) = self.rename.as_mut().filter(|r| &r.device_id == device_id)
        {
            r.error = Some(error);
            return Changed::MAIN;
        }
        if let Command::PairOpen { .. } | Command::PairStart { .. } = pending.command {
            self.pairing =
                Some(PairingView { id: 0, device_id: None, state: PairingState::Failed { reason: error.code } });
            return Changed::MAIN;
        }
        let error = match (&pending.command, error.code) {
            // The service answers with the code alone; the field says why (F-FIND-03).
            (Command::AddByAddress { .. }, ApiCode::Refused) => {
                ApiError { detail: Some(Msg::new(MsgKey::AddressRefusedWhy)), ..error }
            }
            // A quick action the host did not run (not granted, nobody there, the system refused): say where.
            (Command::RunAction { session_id, .. }, _) => ApiError {
                detail: Some(Msg::new(MsgKey::ActionFailed).with(self.session_peer_name(*session_id))),
                ..error
            },
            _ => error,
        };
        self.error = Some(error);
        Changed::ALL
    }

    /// Is a grant change for the selected device waiting for the service (admin grants never change on screen first)?
    pub fn grant_waiting(&self, p: Permission) -> bool {
        let Some(d) = self.selected_device() else { return false };
        self.pending.values().any(|x| matches!(&x.command, Command::SetGrants { device_id, grants } if *device_id == d.id && grants.has(p) != d.grants_here.has(p)))
    }

    /// The text for an error line (the strip over the content).
    pub fn error_text(&self, error: &ApiError) -> String {
        let key = match error.code {
            ApiCode::ServiceDown => MsgKey::ErrServiceDown,
            ApiCode::NotFound => MsgKey::ErrNotFound,
            ApiCode::NotPermitted => MsgKey::ErrNotPermitted,
            ApiCode::NeedsAdmin => MsgKey::ErrNeedsAdmin,
            ApiCode::Busy => MsgKey::ErrBusy,
            ApiCode::Timeout => MsgKey::ErrTimeout,
            ApiCode::Refused => MsgKey::ErrRefused,
            ApiCode::Locked => MsgKey::ErrLocked,
            ApiCode::Invalid => MsgKey::ErrInvalid,
            ApiCode::Internal => MsgKey::ErrInternal,
        };
        match &error.detail {
            // These say everything themselves: the generic line before them would read as a second problem.
            Some(m)
                if matches!(
                    m.key,
                    MsgKey::ClipSkippedTooLarge | MsgKey::ClipSkippedConcealed | MsgKey::ClipSkippedUnsupported
                ) =>
            {
                self.tm(m)
            }
            Some(m) => format!("{} {}", self.t(key), self.tm(m)),
            None => self.t(key),
        }
    }
}

/// The line the terminal panel shows for a refused `TermOpen`: the detail the service's outcome carried, else one by code.
fn term_failure(error: &ApiError) -> Msg {
    if let Some(m) = &error.detail {
        return m.clone();
    }
    Msg::new(match error.code {
        ApiCode::NotPermitted => MsgKey::TermFailNotPermitted,
        ApiCode::Timeout => MsgKey::TermFailTimedOut,
        ApiCode::Busy => MsgKey::TermFailTooMany,
        ApiCode::ServiceDown => MsgKey::TermFailUnavailable,
        _ => MsgKey::TermFailed,
    })
}

/// The data a command's reply fills, if it is a read.
pub fn fetch_kind(command: &Command) -> Option<DataKind> {
    match command {
        Command::GetThisDevice => Some(DataKind::ThisDevice),
        Command::GetNetwork => Some(DataKind::Network),
        Command::ListDevices => Some(DataKind::Devices),
        Command::GetSettings => Some(DataKind::Settings),
        Command::ActivityList { .. } => Some(DataKind::Activity),
        Command::FilesList { .. } => Some(DataKind::Listing),
        _ => None,
    }
}
