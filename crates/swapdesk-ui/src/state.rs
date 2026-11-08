//! The view model of the interface: pure, owned by the UI thread, changed only by [`UiState::apply`] (events from the
//! backend) and [`UiState::activate`] (what the user does). The shells read it and draw it.

use std::collections::HashMap;

use crate::api::*;
use crate::format::Clock;
use crate::strings::{self, MsgKey};
use crate::tokens::layout::{PAIR_COUNTDOWN_S, WAKE_COUNTDOWN_S};

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
    Quality,
    Keyboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditId {
    Name,
    Chat,
    /// The manual address field (F-FIND-03); acts 150 ms after the last key (R-UI-4).
    Address,
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

/// The body of the main window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
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
}

impl SessionView {
    fn new(id: u64, device_id: String, state: SessionState) -> SessionView {
        SessionView { id, device_id, state, stats: None, displays: Vec::new(), display: None }
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
    /// The session ended with a reason, kept until the user dismisses it. `in_error_line` is true when the state's own
    /// error strip already says it (a refusal, a silence, a busy device).
    Ended { reason: ApiCode, in_error_line: bool },
}

/// The connect flow: one line on the main window, from "Conectando" to "A sessão terminou".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFlow {
    pub session_id: u64,
    pub device_id: String,
    pub phase: ConnectPhase,
}

/// A session that ended with a reason, waiting to be dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConnectEnd {
    pub session_id: u64,
    pub device_id: String,
    pub reason: ApiCode,
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
    pub(crate) lang: Language,
    pub(crate) dark: bool,
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
    pub(crate) sessions: Vec<SessionView>,
    pub(crate) swap: Option<SwapView>,
    pub(crate) activity: Vec<AuditView>,
    pub(crate) alert: Option<AuditView>,
    pub(crate) settings: Option<Settings>,
    pub(crate) drawer: Option<DrawerTab>,
    pub(crate) chat: Vec<ChatView>,
    pub(crate) transfers: Vec<TransferView>,
    pub(crate) listing: Option<Listing>,
    pub(crate) files: crate::interact::FilesPanel,
    pub(crate) term_account: Option<String>,
    pub(crate) term: TermPhase,
    /// The connection-wide id of the current terminal, once the service gave it.
    pub(crate) term_id: Option<u32>,
    pub(crate) clipboard_on: bool,
    pub(crate) audio_on: bool,
    pub(crate) grab_on: bool,
    pub(crate) fit: bool,
    /// The viewer's own "Só ver" switch and quality preset (per viewer session; reset when it ends).
    pub(crate) view_only: bool,
    pub(crate) quality: Quality,
    /// The shell's viewer toolbar offers the quality select and the view-only switch (the Windows toolbar does not).
    pub(crate) viewer_extras: bool,
    pub(crate) connect_end: Option<ConnectEnd>,
    /// The viewer session whose display list was asked for by [`UiState::take_display_request`].
    pub(crate) displays_asked: Option<u64>,
    pub(crate) error: Option<ApiError>,
    pub(crate) pending: HashMap<Ticket, Pending>,
    pub(crate) undo_slot: Option<Undo>,
    pub(crate) last_new_device: Option<String>,
    pub(crate) fetch: HashMap<DataKind, Fetch>,
    /// "Os códigos são iguais" was sent and the service has not answered: the buttons wait (never optimistic).
    pub(crate) pair_confirming: bool,
    /// The crash notice is showing (D12).
    pub(crate) crash_notice: bool,
    /// Seconds the device list has been answered and empty (F20): the "no computer found" card shows at 10.
    pub(crate) empty_s: u16,
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
            lang,
            dark,
            clock: Clock::default(),
            this: None,
            network: None,
            devices: Vec::new(),
            selected: None,
            nav: Nav::Devices,
            tab: DeviceTab::Overview,
            pairing: None,
            incoming: None,
            elevation: None,
            sessions: Vec::new(),
            swap: None,
            activity: Vec::new(),
            alert: None,
            settings: None,
            drawer: None,
            chat: Vec::new(),
            transfers: Vec::new(),
            listing: None,
            files: crate::interact::FilesPanel::default(),
            term_account: None,
            term: TermPhase::Idle,
            term_id: None,
            clipboard_on: true,
            audio_on: true,
            grab_on: true,
            fit: true,
            view_only: false,
            quality: Quality::Auto,
            viewer_extras: false,
            connect_end: None,
            displays_asked: None,
            error: None,
            pending: HashMap::new(),
            undo_slot: None,
            last_new_device: None,
            fetch: HashMap::new(),
            pair_confirming: false,
            crash_notice: false,
            empty_s: 0,
        }
    }

    // ----- reads -----

    pub fn language(&self) -> Language {
        self.lang
    }
    pub fn dark(&self) -> bool {
        self.dark
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
    pub fn nav(&self) -> Nav {
        self.nav
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
    /// The viewer's "Só ver" switch (the peer may also deny control: see the session's granted permissions).
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
        self.sessions.iter().find(|s| match &s.state {
            SessionState::Active { role, .. } => *role == Role::Viewer,
            SessionState::Connecting | SessionState::Paused { .. } | SessionState::Reconnecting { .. } => true,
            _ => false,
        })
    }

    /// The session that makes this computer a host (the banner).
    pub fn host_session(&self) -> Option<&SessionView> {
        self.sessions.iter().find(|s| matches!(s.state, SessionState::Active { role: Role::Host, .. }))
    }

    /// A session waking its device (shown in the main area, in place of the device's content).
    pub fn waking_session(&self) -> Option<&SessionView> {
        self.sessions.iter().find(|s| matches!(s.state, SessionState::Waking { .. }))
    }

    /// The connect flow's line for the main window: the session being set up (Connecting, or Prompted with its
    /// countdown), else the last session that ended with a reason, kept until [`UiState::dismiss_connect`].
    pub fn connect_flow(&self) -> Option<ConnectFlow> {
        for s in &self.sessions {
            let phase = match s.state {
                SessionState::Prompted { remaining_s } => ConnectPhase::Prompted { remaining_s },
                SessionState::Connecting => ConnectPhase::Connecting,
                _ => continue,
            };
            return Some(ConnectFlow { session_id: s.id, device_id: s.device_id.clone(), phase });
        }
        self.connect_end.as_ref().map(|e| ConnectFlow {
            session_id: e.session_id,
            device_id: e.device_id.clone(),
            phase: ConnectPhase::Ended {
                reason: e.reason,
                in_error_line: matches!(e.reason, ApiCode::Refused | ApiCode::Timeout | ApiCode::Busy),
            },
        })
    }

    /// The title and the detail line (the countdown, or why it ended) of a connect flow.
    pub fn connect_texts(&self, flow: &ConnectFlow) -> (String, Option<String>) {
        let peer = self.device_by_id(&flow.device_id).map(|d| d.name.clone()).unwrap_or_default();
        let (key, detail) = match &flow.phase {
            ConnectPhase::Connecting => (MsgKey::ConnectingTitle, None),
            ConnectPhase::Prompted { remaining_s } => {
                (MsgKey::PromptedTitle, Some(crate::format::countdown(*remaining_s)))
            }
            ConnectPhase::Ended { reason, .. } => (MsgKey::EndedTitle, Some(self.error_text(&ApiError::new(*reason)))),
        };
        (self.tm(&Msg::new(key).with(peer)), detail)
    }

    /// The user closed the "session ended" line.
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
        match self.nav {
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
    pub fn show_crash_notice(&mut self) {
        self.crash_notice = true;
    }

    pub fn crash_notice(&self) -> bool {
        self.crash_notice
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
                if self.elevation.is_some() {
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

    /// True while something counts seconds down (the shell then runs a one-second timer, otherwise none).
    pub fn needs_tick(&self) -> bool {
        matches!(self.pairing.as_ref().map(|p| &p.state), Some(PairingState::Open { .. }))
            || self.incoming.is_some()
            || matches!(self.swap.as_ref().map(|s| &s.phase), Some(SwapPhase::Asked { .. }))
            || self
                .sessions
                .iter()
                .any(|s| matches!(s.state, SessionState::Waking { .. } | SessionState::Prompted { .. }))
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

    pub fn set_dark(&mut self, dark: bool) -> Changed {
        if self.dark == dark {
            return Changed::NONE;
        }
        self.dark = dark;
        Changed::ALL
    }

    pub fn set_language(&mut self, lang: Language) -> Changed {
        if self.lang == lang {
            return Changed::NONE;
        }
        self.lang = lang;
        Changed::ALL
    }

    pub fn navigate(&mut self, nav: Nav) -> Changed {
        self.nav = nav;
        self.crash_notice = false;
        Changed::MAIN
    }

    pub fn select_device(&mut self, id: &str) -> Changed {
        if self.devices.iter().any(|d| d.id == id) {
            self.selected = Some(id.to_owned());
            self.crash_notice = false;
            self.nav = Nav::Devices;
            self.tab = DeviceTab::Overview;
        }
        Changed::MAIN
    }

    pub fn select_tab(&mut self, tab: DeviceTab) -> Changed {
        self.tab = tab;
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

    fn finish_fetch(&mut self, kind: DataKind) {
        self.fetch.insert(kind, Fetch::Ready);
    }

    fn fail_fetch(&mut self, kind: DataKind) {
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

    /// The commands the UI asks for at launch, all at once (R-UI-6): identity, network, device list, settings and
    /// activity. The window is drawn before any answer arrives.
    pub fn startup_commands(&mut self) -> Vec<Command> {
        let commands = vec![
            Command::GetThisDevice,
            Command::GetNetwork,
            Command::ListDevices,
            Command::GetSettings,
            Command::ActivityList { before_ms: None, limit: 200 },
        ];
        for c in &commands {
            if let Some(k) = fetch_kind(c) {
                self.begin_fetch(k);
            }
        }
        commands
    }

    /// One second passed: counts every countdown down by one and fails the commands that waited too long (R-UI-7).
    pub fn tick(&mut self) -> Changed {
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
        for s in &mut self.sessions {
            match &mut s.state {
                SessionState::Waking { remaining_s } | SessionState::Prompted { remaining_s } if *remaining_s > 0 => {
                    *remaining_s -= 1;
                    changed = changed.union(Changed::MAIN);
                }
                _ => {}
            }
        }
        changed
    }

    // ----- events -----

    pub fn apply(&mut self, event: UiEvent) -> Changed {
        match event {
            UiEvent::Reply { ticket, result } => self.apply_reply(ticket, result),
            UiEvent::DevicesChanged(devices) => self.set_devices(devices),
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
                if let Some(t) = self.transfers.iter_mut().find(|t| t.id == transfer_id) {
                    t.saved_as = Some(name);
                }
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
            UiEvent::ElevationResult { action, result } => self.apply_elevation_result(action, result),
            UiEvent::ThemeChanged { dark } => self.set_dark(dark),
            UiEvent::LanguageChanged(l) => self.set_language(l),
        }
    }

    fn set_settings(&mut self, s: Settings) {
        // The language is per user (C5.1): the service does not keep it and always answers its default, so the UI's
        // own choice wins over the service's copy.
        self.settings = Some(Settings { language: self.lang, ..s });
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

    fn apply_session(&mut self, id: u64, device_id: String, state: SessionState) -> Changed {
        if let SessionState::Ended { reason } = &state {
            let before = self.sessions.iter().find(|s| s.id == id).map(|s| (s.state.clone(), s.device_id.clone()));
            self.sessions.retain(|s| s.id != id);
            // A session that ended with a reason stays as a line on the main window until the user closes it (the
            // host side's own ends are not a connect attempt of this user).
            let was_host = matches!(before, Some((SessionState::Active { role: Role::Host, .. }, _)));
            if let (Some(reason), false) = (reason, was_host) {
                self.connect_end = Some(ConnectEnd { session_id: id, device_id, reason: *reason });
            }
            if self.viewer_session().is_none() {
                self.view_only = false;
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
                if matches!(
                    self.swap.as_ref().map(|s| &s.phase),
                    Some(SwapPhase::Confirm | SwapPhase::Requested { .. })
                ) {
                    self.swap = None;
                }
            }
            // Only a refusal or a silence is worth a line; any other end is the user's own.
            if let Some(reason @ (ApiCode::Refused | ApiCode::Timeout | ApiCode::Busy)) = reason {
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
        if self.connect_end.as_ref().is_some_and(|e| e.device_id == device_id) {
            self.connect_end = None;
        }
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            s.state = state;
            s.device_id = device_id;
        } else {
            self.sessions.push(SessionView::new(id, device_id, state));
        }
        Changed::ALL
    }

    fn apply_elevation_result(&mut self, action: ElevatedAction, result: Result<(), ApiError>) -> Changed {
        if self.elevation.as_ref().is_none_or(|e| e.action != action) {
            return Changed::NONE;
        }
        match result {
            Ok(()) => self.elevation = None,
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
            Reply::SessionId(id) => {
                let state = match pending.map(|p| p.command) {
                    Some(Command::Connect { device_id, .. }) => Some((device_id, SessionState::Connecting)),
                    // "Acordar": the entry that shows "Acordando" until the device appears or the minute ends.
                    Some(Command::Wake { device_id }) => {
                        Some((device_id, SessionState::Waking { remaining_s: WAKE_COUNTDOWN_S }))
                    }
                    _ => None,
                };
                if let Some((device_id, state)) = state
                    && !self.sessions.iter().any(|s| s.id == id)
                {
                    if self.connect_end.as_ref().is_some_and(|e| e.device_id == device_id) {
                        self.connect_end = None;
                    }
                    self.sessions.push(SessionView::new(id, device_id, state));
                    return Changed::ALL;
                }
                Changed::NONE
            }
            Reply::RequestId(request_id) => {
                if let Some(s) = &mut self.swap
                    && matches!(s.phase, SwapPhase::Requested { .. })
                {
                    s.phase = SwapPhase::Requested { request_id };
                }
                Changed::ALL
            }
            Reply::Listing(l) => {
                self.listing = Some(l);
                self.files.page = 0;
                self.finish_fetch(DataKind::Listing);
                Changed::ALL
            }
            Reply::Chat(m) => self.apply(UiEvent::Chat { session_id: 0, message: m }),
            Reply::Activity(a) => {
                self.alert = a.iter().find(|x| x.alert).cloned();
                self.activity = a;
                self.finish_fetch(DataKind::Activity);
                Changed::ALL
            }
            Reply::Settings(s) => {
                self.set_settings(s);
                Changed::ALL
            }
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
        }
    }

    fn apply_err(&mut self, pending: Option<Pending>, error: ApiError) -> Changed {
        let Some(pending) = pending else {
            self.error = Some(error);
            return Changed::ALL;
        };
        match pending.undo {
            Some(Undo::Grants(device_id, grants)) => {
                if let Some(d) = self.devices.iter_mut().find(|d| d.id == device_id) {
                    d.grants_here = grants;
                }
            }
            Some(Undo::Settings(old)) => {
                self.lang = old.language;
                self.set_settings(*old);
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
                Command::Connect { device_id, intent, unattended: true } => {
                    Some(ElevatedAction::ConnectUnattended { device_id: device_id.clone(), intent: *intent })
                }
                Command::TermOpen { session_id, admin: true, .. } => {
                    Some(ElevatedAction::OpenAdminTerminal { session_id: *session_id })
                }
                _ => None,
            };
            if let Some(action) = action {
                self.elevation = Some(Elevation { action, phase: ElevPhase::Ask });
                return Changed::ALL;
            }
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
