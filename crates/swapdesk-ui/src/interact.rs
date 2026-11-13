//! What the user can reach on each window (the controls, in Tab order) and what activating one does.
//!
//! The shells draw the controls and register where each one landed; this module decides their names, roles and
//! states (for UI Automation and the keyboard) and the commands an activation sends. Nothing here knows about pixels.

use crate::api::*;
use crate::format;
use crate::shortcuts::ReleaseKey;
use crate::state::*;
use crate::strings::MsgKey;
use crate::tokens::layout::PAIR_COUNTDOWN_S;

/// The device page's button that starts a connect with this intent.
fn start_button(intent: Intent) -> ControlId {
    match intent {
        Intent::View => ControlId::ViewOnly,
        Intent::Control => ControlId::Control,
    }
}

/// Longest local name of a paired device, in bytes: what the trust record stores.
const MAX_DEVICE_NAME: usize = 64;

mod explorer;
pub mod explorer_drag;
pub mod explorer_drop;
pub mod explorer_manage;
pub mod explorer_menu;
pub mod explorer_remote;
mod transfers;
pub(crate) use explorer::Explorer;
pub use explorer::{
    PaneView, PlaceView, PointerMods, RowView, SORT_MODIFIED, SORT_NAME, SORT_SIZE, SORT_TYPE, natural_cmp, type_of,
};

/// Everything a window can focus or announce. `Static` ones are text the screen reader reads, never focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlId {
    // sidebar
    Device(u16),
    NavHome,
    NavSettings,
    // home
    PairThisComputer,
    /// "Adicionar" beside the address field: sends what the field holds, as Enter does.
    AddAddress,
    /// "Ver tudo" on the home's recent activity card: opens the Atividade screen.
    HomeActivityAll,
    // device header, tabs, overview
    /// "Renomear": turns the header's name into the inline field.
    RenameDevice,
    RenameSave,
    RenameCancel,
    ViewOnly,
    Control,
    /// "Acordar": wake an asleep paired device (F-WAKE-01).
    Wake,
    PairWithDevice,
    Tab(DeviceTab),
    EditGrants,
    // permissions tab
    Grant(Permission),
    Forget,
    // activity
    Export,
    BlockAlert,
    /// "Desbloquear" on row `n` of the blocked devices in Ajustes (D65).
    Unblock(u16),
    // settings
    Edit(EditId),
    Select(SelectId),
    OpenAtLogin,
    /// "Receber conexões" (ACCOUNTS.md 7.2, D57): this account's endpoint and record.
    Receive,
    LockOnEnd,
    KeepAwake,
    ShowStats,
    SwapCtrlCmd,
    // pairing
    PairSame,
    PairDifferent,
    PairCancel,
    PairDone,
    PairRetry,
    PairClose,
    // incoming request
    Allow(Permission),
    RememberUnattended,
    Decline,
    ViewOnlyAccept,
    Accept,
    // modal dialogs (consent, elevation, swap)
    DialogPrimary,
    DialogSecondary,
    // notices
    Notice(NoticeKind),
    /// The "no computer found" card's action: focus the address field (F20).
    NoComputerAddress,
    /// The first-run guide's "Pular" and "Entendi" (B16): both close it for good.
    GuideSkip,
    GuideDone,
    /// A button of the update card or the Ajustes section "Atualizações" (UPDATES.md 10).
    Update(crate::update_card::UpdAction),
    WakeCancel,
    /// "Cancelar" on the connect card: asks the service to drop the entry.
    ConnectCancel,
    /// "Fechar" on the card that says why a connect did not open.
    ConnectClose,
    /// "Tentar de novo" on that card.
    ConnectRetry,
    ReconnectDisconnect,
    SwapDeclinedClose,
    ErrorClose,
    // viewer toolbar and drawer
    TbScreen,
    TbFit,
    /// The viewer's quality select (the settings' default quality is `Select(SelectId::Quality)`).
    TbQuality,
    /// The viewer's "Assistir" switch (`ViewOnly` is the device header's "connect view-only" button).
    TbViewOnly,
    TbAudio,
    TbGrab,
    TbKeys,
    TbTerminal,
    TbFiles,
    TbChat,
    TbSwap,
    /// "Elevar controle" on the session bar (ACCOUNTS.md 9).
    TbElevate,
    /// "Elevar controle" on the card over a paused picture.
    PausedElevate,
    TbDisconnect,
    /// The last button of the session bar: shrinks it to a handle at the top of the picture.
    TbCollapse,
    /// The handle of a collapsed session bar: brings the bar back.
    TbExpand,
    DrawerTab(DrawerTab),
    DrawerClose,
    Action(ActionKind),
    /// The terminal's grid: it takes the keyboard focus while a shell runs (the shell draws and handles it).
    TermGrid,
    /// "Abrir terminal de administrador" (F-TERM-02), offered when the session holds `ADMIN_TERMINAL`.
    TermAdmin,
    /// "Abrir de novo" after the shell ended or the host refused.
    TermReopen,
    /// "Retomar" on the paused transfer with this number.
    TransferResume(u32),
    /// "Cancelar" on the transfer with this number.
    TransferCancel(u32),
    // The files explorer (FILES-UI.md): `true` names the other computer's pane.
    /// "Tela": back from the Arquivos view to the picture.
    ExpTela,
    /// The primary "Copiar para ..." of the command bar.
    ExpCopy,
    /// Row `n` of a pane, in its sorted order.
    ExpRow(bool, u16),
    /// A pane's whole area, registered under its rows: the empty part of the list, and the pane's current folder as a
    /// drop target (slice 3).
    ExpPane(bool),
    /// Segment `n` of a pane's breadcrumb: opens that folder, and is a drop target for it.
    ExpCrumb(bool, u8),
    /// A column header: sorts by it (`SORT_NAME` to `SORT_MODIFIED`), again reverses.
    ExpSort(bool, u8),
    ExpBack(bool),
    ExpForward(bool),
    ExpUp(bool),
    ExpRefresh(bool),
    /// Place or volume `n` of a computer in the navigation pane.
    ExpPlace(bool, u8),
    /// "Mostrar" / "Ocultar" of the transfers bar.
    ExpTransfers,
    /// Slice 2a, the left pane's commands: Nova pasta, Renomear, Excluir, Propriedades, the hidden items switch.
    ExpNewFolder,
    ExpRename,
    ExpDelete,
    ExpProps,
    ExpHidden,
    /// The properties card's "Fechar".
    ExpPropsClose,
    /// The "Excluir para sempre" card's buttons.
    ExpForeverConfirm,
    ExpForeverCancel,
    /// Slice 2b: the right pane's own "Mostrar itens ocultos" switch.
    ExpHiddenThere,
    /// The card "Mandar N itens para a Lixeira do ...?" of the other computer: Cancelar, Excluir (danger).
    ExpThereTrashCancel,
    ExpThereTrashConfirm,
    /// The conflict card (FILES-UI.md 5), from the left: Cancelar, Pular, Substituir (danger), Manter os dois
    /// (primary, the default answer); and its check "Fazer o mesmo com os outros".
    ExpConflictCancel,
    ExpConflictSkip,
    ExpConflictReplace,
    ExpConflictKeepBoth,
    ExpConflictApplyAll,
    ChatSend,
    ClipToggle,
    // host banner
    BannerChat,
    BannerSwap,
    BannerDisconnect,
    // text only
    Static(u16),
}

impl ControlId {
    /// Can it take the keyboard focus?
    pub fn focusable(self) -> bool {
        !matches!(self, ControlId::Static(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiRole {
    Button,
    Tab,
    Switch,
    CheckBox,
    Link,
    ComboBox,
    TextInput,
    Heading,
    Text,
    Progress,
    Table,
    Row,
    Cell,
    ColumnHeader,
}

/// The landmark a control sits under in the accessibility tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Sidebar,
    Content,
    Dialog,
    Toolbar,
    Drawer,
    Banner,
    /// A polite live region: what a screen reader announces when its text changes (the wait card's title and body).
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlSpec {
    pub id: ControlId,
    pub role: UiRole,
    pub label: String,
    pub description: Option<String>,
    pub toggled: Option<bool>,
    pub selected: bool,
    pub value: Option<String>,
    pub region: Region,
    pub disabled: bool,
    /// A heading's level: 1 for the title of the page, 2 for a section or a card in it; 0 for anything else.
    pub level: u8,
}

impl ControlSpec {
    fn new(id: ControlId, role: UiRole, region: Region, label: impl Into<String>) -> ControlSpec {
        ControlSpec {
            id,
            role,
            label: label.into(),
            description: None,
            toggled: None,
            selected: false,
            value: None,
            region,
            disabled: false,
            level: 0,
        }
    }
}

pub(crate) struct Specs {
    v: Vec<ControlSpec>,
    next_static: u16,
    region: Region,
}

impl Specs {
    fn new(region: Region) -> Specs {
        Specs { v: Vec::new(), next_static: 0, region }
    }
    pub(crate) fn region(&mut self, region: Region) {
        self.region = region;
    }
    pub(crate) fn add(&mut self, id: ControlId, role: UiRole, label: impl Into<String>) -> &mut ControlSpec {
        self.v.push(ControlSpec::new(id, role, self.region, label));
        self.v.last_mut().expect("just pushed")
    }
    pub(crate) fn button(&mut self, id: ControlId, label: impl Into<String>) {
        self.add(id, UiRole::Button, label);
    }
    /// A heading of `level` (1: the page's title, 2: a section or a card).
    pub(crate) fn heading(&mut self, level: u8, label: impl Into<String>) {
        let id = ControlId::Static(self.next_static);
        self.next_static += 1;
        self.add(id, UiRole::Heading, label).level = level;
    }
    pub(crate) fn text(&mut self, role: UiRole, label: impl Into<String>) {
        let id = ControlId::Static(self.next_static);
        self.next_static += 1;
        self.add(id, role, label);
    }
    fn switch(&mut self, id: ControlId, label: impl Into<String>, on: bool, hint: Option<String>) {
        let c = self.add(id, UiRole::Switch, label);
        c.toggled = Some(on);
        c.description = hint;
    }
}

/// A device name as an administrator's confirmation shows it: control and direction-override characters removed, so
/// a name a paired device chose cannot break or reorder the line (review of the elevation, finding 8).
pub fn shown_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

/// The line that says what an elevation is for, in `lang`: the step shows it and so does the elevated window.
/// `peer_name` is the device the action concerns (unused by actions that name none).
pub fn elevation_action_text(lang: Language, action: &ElevatedAction, peer_name: &str) -> String {
    let say = |key: MsgKey, args: &[&str]| {
        let mut msg = Msg::new(key);
        for a in args {
            msg = msg.with(*a);
        }
        crate::strings::text(lang, &msg)
    };
    let peer = shown_name(peer_name);
    let peer_name = peer.as_str();
    match action {
        ElevatedAction::SetGrants { grants, .. } => {
            // One line per machine-level permission the result holds, so a change that also adds the administrator
            // terminal never reads as "Entrar sem perguntar" alone (review of the elevation, finding 1).
            let mut lines = Vec::new();
            if grants.has(Permission::Unattended) {
                lines.push(say(MsgKey::ElevActGrantUnattended, &[peer_name]));
            }
            if grants.has(Permission::AdminTerminal) {
                lines.push(say(MsgKey::ElevActGrantAdminTerminal, &[peer_name]));
            }
            if lines.is_empty() {
                lines.push(say(MsgKey::ElevActGrants, &[peer_name]));
            }
            lines.join("\n")
        }
        ElevatedAction::ElevateSession { .. } => say(MsgKey::ElevActElevateSession, &[peer_name]),
        ElevatedAction::OpenAdminTerminal { .. } => say(MsgKey::ElevActAdminTerminalOpen, &[]),
    }
}

impl UiState {
    /// The controls of a window in Tab order, with their names and states.
    pub fn controls(&self, kind: WindowKind) -> Vec<ControlSpec> {
        match kind {
            WindowKind::Main => self.main_controls(),
            WindowKind::Viewer => self.viewer_controls(),
            WindowKind::Banner => self.banner_controls(),
        }
    }

    fn name_of(&self, key: MsgKey) -> String {
        self.t(key)
    }

    /// Indices of the paired devices, then of the others (the sidebar's groups).
    pub fn device_groups(&self) -> (Vec<usize>, Vec<usize>) {
        let paired = (0..self.devices.len()).filter(|&i| self.devices[i].paired).collect();
        let network = (0..self.devices.len()).filter(|&i| !self.devices[i].paired).collect();
        (paired, network)
    }

    fn main_controls(&self) -> Vec<ControlSpec> {
        let mut s = Specs::new(Region::Content);
        if let Some(kind) = self.dialog(WindowKind::Main) {
            self.dialog_controls(&mut s, kind);
            return s.v;
        }
        if self.incoming_session_open() {
            self.incoming_controls(&mut s);
            return s.v;
        }
        if let Some(p) = &self.pairing {
            self.pairing_controls(&mut s, p);
            return s.v;
        }
        // sidebar: Início, the device lists, Ajustes
        s.region(Region::Sidebar);
        let c = s.add(ControlId::NavHome, UiRole::Button, self.name_of(MsgKey::NavHome));
        c.selected = self.nav() == Nav::Home;
        let (paired, network) = self.device_groups();
        for i in paired.iter().chain(network.iter()) {
            let d = &self.devices[*i];
            let label = format!("{}, {}", d.name, self.device_status_line(d));
            let selected = self.selected_device().is_some_and(|x| x.id == d.id) && self.nav() == Nav::Devices;
            let c = s.add(ControlId::Device(*i as u16), UiRole::Button, label);
            c.selected = selected;
        }
        let c = s.add(ControlId::NavSettings, UiRole::Button, self.name_of(MsgKey::NavSettings));
        c.selected = self.nav() == Nav::Settings;
        s.region(Region::Content);
        if let Some(e) = &self.error {
            s.text(UiRole::Text, self.error_text(e));
            s.button(ControlId::ErrorClose, self.name_of(MsgKey::Close));
        }
        match self.nav() {
            Nav::Home => self.home_controls(&mut s),
            Nav::Devices => self.devices_controls(&mut s),
            Nav::Activity => self.activity_controls(&mut s),
            Nav::Settings => self.settings_controls(&mut s),
        }
        s.v
    }

    /// The controls of a wait card: its title and body are a polite live region (announced when the phase changes),
    /// the countdown is a progress node outside it (read when focused, never announced each second), then the buttons.
    fn wait_card_controls(&self, s: &mut Specs, card: &WaitCard) {
        s.region(Region::Status);
        s.heading(2, card.title.clone());
        s.text(UiRole::Text, card.body.clone());
        s.region(Region::Content);
        if card.phase == WaitPhase::Prompted
            && let Some(e) = self.waiting_entry(&card.device_id)
            && let SessionState::Prompted { remaining_s } = e.state
        {
            let label = self.tm(&Msg::new(MsgKey::ConnectExpiresIn).with(remaining_s.to_string()));
            let c = s.add(ControlId::Static(u16::MAX), UiRole::Progress, label);
            c.value = Some(remaining_s.to_string());
        }
        for b in &card.buttons {
            s.button(b.id, b.label.clone());
        }
    }

    /// The crash notice and the "no computer found" card (or the plain empty line): what the home shows when no
    /// device page is open.
    pub(crate) fn crash_and_empty_controls(&self, s: &mut Specs) {
        if self.crash_notice {
            let (title, body, button) = self.notice_texts(NoticeKind::Crash);
            s.heading(2, title);
            s.text(UiRole::Text, body);
            s.button(ControlId::Notice(NoticeKind::Crash), button);
        }
        if self.no_computer_card() {
            s.heading(2, self.name_of(MsgKey::NoComputerTitle));
            s.text(UiRole::Text, self.name_of(MsgKey::NoComputerBody));
            s.button(ControlId::NoComputerAddress, self.name_of(MsgKey::NoComputerButton));
        } else if self.devices.is_empty() && self.fetch(DataKind::Devices) == Fetch::Ready {
            s.text(UiRole::Text, self.name_of(MsgKey::EmptyDevices));
        }
    }

    fn devices_controls(&self, s: &mut Specs) {
        let lang = self.lang;
        let Some(d) = self.selected_device() else {
            self.crash_and_empty_controls(s);
            return;
        };
        match self.renaming().filter(|_| self.rename_available(d)) {
            Some(r) => {
                let c = s.add(ControlId::Edit(EditId::Rename), UiRole::TextInput, self.name_of(MsgKey::RenameLabel));
                c.value = Some(d.name.clone());
                s.button(ControlId::RenameSave, self.name_of(MsgKey::RenameSave));
                s.button(ControlId::RenameCancel, self.name_of(MsgKey::Cancel));
                if let Some(e) = &r.error {
                    s.text(UiRole::Text, self.rename_error_text(e));
                }
            }
            None => {
                s.heading(1, d.name.clone());
                if self.rename_available(d) {
                    s.button(ControlId::RenameDevice, self.name_of(MsgKey::RenameAction));
                }
            }
        }
        s.text(UiRole::Text, format::detail_line(lang, d));
        if let Some(card) = self.wait_card(&d.id) {
            self.wait_card_controls(s, &card);
            return;
        }
        if d.paired && Self::can_wake(d) {
            s.button(ControlId::Wake, self.name_of(MsgKey::Wake));
        } else if d.paired {
            s.button(ControlId::ViewOnly, self.name_of(MsgKey::ViewOnly));
            s.button(ControlId::Control, self.name_of(MsgKey::Control));
        } else {
            s.button(ControlId::PairWithDevice, self.tm(&Msg::new(MsgKey::PairTitleWith).with(d.name.clone())));
        }
        if d.paired {
            for (tab, key) in [
                (DeviceTab::Overview, MsgKey::TabOverview),
                (DeviceTab::Permissions, MsgKey::TabPermissions),
                (DeviceTab::History, MsgKey::TabHistory),
            ] {
                let c = s.add(ControlId::Tab(tab), UiRole::Tab, self.name_of(key));
                c.selected = self.tab == tab;
            }
        }
        let notices = self.notices();
        if !notices.is_empty() {
            for k in notices {
                let (title, body, button) = self.notice_texts(k);
                s.heading(2, title);
                s.text(UiRole::Text, body);
                s.button(ControlId::Notice(k), button);
            }
            return;
        }
        if !d.paired {
            s.text(UiRole::Text, self.name_of(MsgKey::PairNotPairedBody));
            return;
        }
        match self.tab {
            DeviceTab::Overview => {
                s.text(UiRole::Text, self.name_of(MsgKey::SwapNote));
                s.heading(2, self.tm(&Msg::new(MsgKey::PermsPanelTitle).with(d.name.clone())));
                s.text(UiRole::Text, self.permission_summary(d.grants_here));
                s.add(ControlId::EditGrants, UiRole::Link, self.name_of(MsgKey::PermsPanelEdit));
                s.heading(2, self.name_of(MsgKey::LastSessionTitle));
                s.text(UiRole::Text, self.last_session_text(d));
            }
            DeviceTab::Permissions => {
                s.heading(2, self.tm(&Msg::new(MsgKey::PermsHereTitle).with(d.name.clone())));
                for (p, label, hint) in self.permission_rows() {
                    let available = self.grant_available(p);
                    s.switch(ControlId::Grant(p), label, available && d.grants_here.has(p), hint);
                    if (self.grant_waiting(p) || !available)
                        && let Some(c) = s.v.last_mut()
                    {
                        c.disabled = true;
                    }
                }
                s.heading(2, self.tm(&Msg::new(MsgKey::PermsThereTitle).with(d.name.clone())));
                for (label, answer) in self.there_rows(d) {
                    s.text(UiRole::Text, format!("{label}: {answer}"));
                }
                s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::ThereNote).with(d.name.clone())));
                s.button(ControlId::Forget, self.name_of(MsgKey::ForgetDevice));
            }
            DeviceTab::History => {
                s.heading(2, self.tm(&Msg::new(MsgKey::HistoryTitle).with(d.name.clone())));
                s.text(UiRole::Text, self.name_of(MsgKey::HistoryPlaceholder));
            }
        }
    }

    /// An asleep paired device that stored the addresses to wake it with: "Acordar" replaces the connect buttons.
    pub fn can_wake(d: &DeviceView) -> bool {
        d.wakeable && matches!(d.status, DeviceStatus::Asleep)
    }

    /// Title, body and button of a notice card.
    pub fn notice_texts(&self, kind: NoticeKind) -> (String, String, String) {
        let label = self.network.as_ref().map(|n| n.label.clone()).unwrap_or_default();
        match kind {
            NoticeKind::Public => (
                self.name_of(MsgKey::NoticePublicTitle),
                self.tm(&Msg::new(MsgKey::NoticePublicBody).with(label)),
                self.name_of(MsgKey::NoticePublicButton),
            ),
            NoticeKind::Firewall => (
                self.name_of(MsgKey::NoticeFirewallTitle),
                self.name_of(MsgKey::NoticeFirewallBody),
                self.name_of(MsgKey::NoticeFirewallButton),
            ),
            NoticeKind::Service => (
                self.name_of(MsgKey::NoticeServiceTitle),
                self.name_of(MsgKey::NoticeServiceBody),
                self.name_of(MsgKey::NoticeServiceButton),
            ),
            NoticeKind::Crash => (
                self.name_of(MsgKey::NoticeCrashTitle),
                self.name_of(MsgKey::NoticeCrashBody),
                self.name_of(MsgKey::NoticeCrashButton),
            ),
            NoticeKind::ReceiveFolder => {
                let folder = self
                    .network
                    .as_ref()
                    .and_then(|n| {
                        n.notices.iter().find_map(|x| match x {
                            Notice::ReceiveFolderDenied(name) => Some(name.clone()),
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                (
                    self.name_of(MsgKey::RecvFolderDeniedTitle),
                    self.tm(&Msg::new(MsgKey::RecvFolderDeniedBody).with(folder)),
                    self.name_of(MsgKey::RecvFolderDeniedOpen),
                )
            }
            NoticeKind::MacPerms => {
                let missing: Vec<OsPermission> = self
                    .network
                    .as_ref()
                    .and_then(|n| {
                        n.notices.iter().find_map(|x| match x {
                            Notice::MacPermissionsMissing(v) => Some(v.clone()),
                            _ => None,
                        })
                    })
                    .unwrap_or_default();
                let title = if missing.len() == 1 {
                    self.name_of(MsgKey::NoticeMacPermsOne)
                } else {
                    self.tm(&Msg::new(MsgKey::NoticeMacPermsMany).with(missing.len().to_string()))
                };
                let body = missing
                    .iter()
                    .map(|p| {
                        self.name_of(match p {
                            OsPermission::ScreenRecording => MsgKey::OsPermScreen,
                            OsPermission::Accessibility => MsgKey::OsPermAccessibility,
                            OsPermission::InputMonitoring => MsgKey::OsPermInput,
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                (title, body, self.name_of(MsgKey::NoticeMacPermsButton))
            }
        }
    }

    /// "Ver a tela, controlar, ouvir o áudio, terminal, arquivos, área de transferência e acesso sem usuário."
    pub fn permission_summary(&self, g: Permissions) -> String {
        let items: Vec<String> = [
            (Permission::View, MsgKey::SumView),
            (Permission::Control, MsgKey::SumControl),
            (Permission::Audio, MsgKey::SumAudio),
            (Permission::Terminal, MsgKey::SumTerminal),
            (Permission::AdminTerminal, MsgKey::SumAdminTerminal),
            (Permission::Files, MsgKey::SumFiles),
            (Permission::Clipboard, MsgKey::SumClipboard),
            (Permission::Unattended, MsgKey::SumUnattended),
        ]
        .into_iter()
        .filter(|(p, _)| g.has(*p))
        .map(|(_, k)| self.name_of(k))
        .collect();
        if items.is_empty() {
            return self.name_of(MsgKey::PermsNone);
        }
        let mut text = match items.split_last() {
            Some((last, rest)) if !rest.is_empty() => {
                format!("{}{}{}", rest.join(", "), self.name_of(MsgKey::ListAnd), last)
            }
            _ => items.join(""),
        };
        if let Some(first) = text.chars().next() {
            let upper: String = first.to_uppercase().collect();
            text.replace_range(..first.len_utf8(), &upper);
        }
        text.push('.');
        text
    }

    /// The switches of "O X pode, neste computador": permission, name, hint.
    pub fn permission_rows(&self) -> Vec<(Permission, String, Option<String>)> {
        vec![
            (Permission::View, self.name_of(MsgKey::PermView), None),
            (Permission::Control, self.name_of(MsgKey::PermControl), None),
            (Permission::Audio, self.name_of(MsgKey::PermAudio), None),
            (Permission::Terminal, self.name_of(MsgKey::PermTerminal), None),
            (
                Permission::AdminTerminal,
                self.name_of(MsgKey::PermAdminTerminal),
                Some(self.name_of(if self.grant_available(Permission::AdminTerminal) {
                    MsgKey::PermAdminTerminalHint
                } else {
                    MsgKey::PermAdminTerminalMacOff
                })),
            ),
            (Permission::Files, self.name_of(MsgKey::PermFiles), None),
            (Permission::Clipboard, self.name_of(MsgKey::PermClipboard), None),
            (
                Permission::Unattended,
                self.name_of(MsgKey::PermUnattended),
                Some(self.name_of(MsgKey::PermUnattendedHint)),
            ),
        ]
    }

    /// Whether this computer can serve `p` at all. A Mac host serves no administrator terminal in this version
    /// (D32), so on a Mac its switch stays off and inert, with the line that says why (ELEVATION-MAC.md, owner
    /// decision 3).
    pub fn grant_available(&self, p: Permission) -> bool {
        !(p == Permission::AdminTerminal && self.this.as_ref().is_some_and(|t| t.os == Os::MacOs))
    }

    /// The read-only column "Este computador pode, no X": (label, "Sim" / "Não" / "–"), plus whether it is a yes.
    pub fn there_rows(&self, d: &DeviceView) -> Vec<(String, String)> {
        let rows = [
            (Permission::View, MsgKey::PermView),
            (Permission::Control, MsgKey::PermControl),
            (Permission::Audio, MsgKey::PermAudio),
            (Permission::Terminal, MsgKey::PermTerminal),
            (Permission::Files, MsgKey::PermFiles),
            (Permission::Clipboard, MsgKey::PermClipboard),
            (Permission::Unattended, MsgKey::PermUnattended),
        ];
        rows.iter()
            .map(|(p, key)| {
                let answer = match d.grants_there {
                    Some(g) if g.has(*p) => MsgKey::PermYes,
                    Some(_) => MsgKey::PermNo,
                    None => MsgKey::PermUnknown,
                };
                (self.name_of(*key), self.name_of(answer))
            })
            .collect()
    }

    /// "Ontem às 23:40, por 6 minutos, na tela de login deste computador."
    pub fn last_session_text(&self, d: &DeviceView) -> String {
        match &d.last_session {
            Some(s) => self.tm(&Msg::new(MsgKey::LastSessionLine)
                .with(format::when_phrase(self.lang, s.at_ms, self.clock))
                .with(format::minutes_phrase(self.lang, s.duration_s))
                .with(format::where_phrase(self.lang, s.desktop))),
            None => self.name_of(MsgKey::LastSessionNone),
        }
    }

    fn activity_controls(&self, s: &mut Specs) {
        s.heading(1, self.name_of(MsgKey::ActivityTitle));
        s.button(ControlId::Export, self.name_of(MsgKey::ExportLog));
        if let Some(a) = &self.alert {
            let addr = a.what.args.first().cloned().unwrap_or_default();
            let when = format::when_phrase(self.lang, a.at_ms, self.clock).to_lowercase();
            s.text(
                UiRole::Text,
                format!(
                    "{} {}",
                    self.tm(&Msg::new(MsgKey::AlertUnknownPair).with(when)),
                    self.name_of(MsgKey::AlertUnknownRest)
                ),
            );
            let _ = addr;
            s.button(ControlId::BlockAlert, self.name_of(MsgKey::AlertBlockDevice));
        }
        s.add(ControlId::Static(900), UiRole::Table, self.name_of(MsgKey::LogLabel));
        for (n, row) in self.activity.iter().enumerate() {
            let when = format::table_when(self.lang, row.at_ms, self.clock);
            let who = self.activity_who(row);
            let dur = self.duration_cell(row);
            s.add(
                ControlId::Static(901 + n as u16),
                UiRole::Row,
                format!("{when}, {who}, {}, {dur}", self.activity_what(row)),
            );
        }
    }

    /// The device cell of an activity row: "Desconhecido" for an alert, "Dispositivo removido" for a device no longer
    /// paired (the service sends it with no name), else the device's name.
    pub fn activity_who(&self, row: &AuditView) -> String {
        if row.alert {
            self.name_of(MsgKey::UnknownDevice)
        } else if row.device_name.is_empty() {
            self.name_of(MsgKey::RemovedDevice)
        } else {
            row.device_name.clone()
        }
    }

    /// The text of an activity row; a device no longer paired is named "dispositivo removido" inside it.
    pub fn activity_what(&self, row: &AuditView) -> String {
        if row.alert || !row.device_name.is_empty() {
            return self.tm(&row.what);
        }
        let mut what = row.what.clone();
        for a in what.args.iter_mut().filter(|a| a.is_empty()) {
            *a = self.name_of(MsgKey::RemovedDeviceInLine);
        }
        self.tm(&what)
    }

    /// The duration cell of an activity row: "17 min", "agora" or "–".
    pub fn duration_cell(&self, row: &AuditView) -> String {
        match row.duration_s {
            Some(0) => self.name_of(MsgKey::DurNow),
            Some(secs) => format::short_minutes(self.lang, secs),
            None => self.name_of(MsgKey::DurNone),
        }
    }

    fn settings_controls(&self, s: &mut Specs) {
        s.heading(1, self.name_of(MsgKey::SettingsTitle));
        let Some(set) = &self.settings else { return };
        let c = s.add(ControlId::Edit(EditId::Name), UiRole::TextInput, self.name_of(MsgKey::FieldName));
        c.value = Some(set.name.clone());
        c.description = Some(self.name_of(MsgKey::NameNetworkHint));
        let c = s.add(ControlId::Select(SelectId::Language), UiRole::ComboBox, self.name_of(MsgKey::FieldLanguage));
        c.value = Some(self.select_value(SelectId::Language));
        let c = s.add(ControlId::Select(SelectId::Theme), UiRole::ComboBox, self.name_of(MsgKey::FieldTheme));
        c.value = Some(self.select_value(SelectId::Theme));
        s.switch(ControlId::OpenAtLogin, self.name_of(MsgKey::OpenAtLogin), set.open_at_login, None);
        s.switch(
            ControlId::Receive,
            self.name_of(MsgKey::ServiceName),
            set.service_enabled,
            Some(self.name_of(MsgKey::ServiceHint)),
        );
        s.switch(ControlId::LockOnEnd, self.name_of(MsgKey::LockOnEnd), set.lock_on_end, None);
        s.switch(ControlId::KeepAwake, self.name_of(MsgKey::KeepAwake), set.keep_awake, None);
        // UPDATES.md 10.1: "Atualizações", the last group of the left column.
        if let (Some(u), Some(lines)) = (&self.update, self.update_lines()) {
            use crate::update_card::UpdAction;
            s.heading(2, self.name_of(MsgKey::UpdSection));
            s.switch(
                ControlId::Update(UpdAction::Switch),
                self.name_of(MsgKey::UpdSwitch),
                u.check_on,
                Some(self.name_of(MsgKey::UpdSwitchHint)),
            );
            for line in lines {
                s.text(UiRole::Text, line);
            }
            s.button(ControlId::Update(UpdAction::CheckNow), self.name_of(MsgKey::UpdCheckNow));
        }
        let c = s.add(ControlId::Select(SelectId::Quality), UiRole::ComboBox, self.name_of(MsgKey::Quality));
        c.value = Some(self.select_value(SelectId::Quality));
        s.switch(ControlId::ShowStats, self.name_of(MsgKey::ShowStats), set.show_stats, None);
        let c = s.add(ControlId::Select(SelectId::Keyboard), UiRole::ComboBox, self.name_of(MsgKey::SendKeys));
        c.value = Some(self.select_value(SelectId::Keyboard));
        c.description = Some(self.name_of(MsgKey::SendKeysHint));
        s.switch(ControlId::SwapCtrlCmd, self.name_of(MsgKey::SwapCtrlCmd), set.swap_ctrl_cmd, None);
        s.heading(2, self.name_of(MsgKey::GroupBlocked));
        for (label, line, n) in self.blocked_rows() {
            s.text(UiRole::Text, format!("{label}, {line}"));
            s.button(ControlId::Unblock(n), self.name_of(MsgKey::Unblock));
        }
        if self.blocked.is_empty() {
            s.text(UiRole::Text, self.name_of(MsgKey::BlockedNone));
        }
        s.text(UiRole::Text, self.name_of(MsgKey::BlockedNote));
        s.heading(2, self.name_of(MsgKey::GroupShortcuts));
        let c = s.add(ControlId::Select(SelectId::ReleaseKey), UiRole::ComboBox, self.name_of(MsgKey::ReleaseKeyLabel));
        c.value = Some(self.select_value(SelectId::ReleaseKey));
        c.description = Some(self.name_of(MsgKey::ReleaseKeyHint));
        let c = s.add(
            ControlId::Select(SelectId::FullscreenMonitor),
            UiRole::ComboBox,
            self.name_of(MsgKey::FullscreenMonitorLabel),
        );
        c.value = Some(self.select_value(SelectId::FullscreenMonitor));
        for (what, keys) in self.shortcut_rows() {
            s.text(UiRole::Text, format!("{what}: {keys}"));
        }
        s.text(UiRole::Text, self.name_of(MsgKey::ShortcutsKeepHint));
    }

    /// The options of a select with the current one marked.
    pub fn select_options(&self, id: SelectId) -> Vec<(String, bool)> {
        match id {
            SelectId::ReleaseKey => {
                return ReleaseKey::ALL.iter().map(|k| (self.release_key_name(*k), *k == self.release_key)).collect();
            }
            SelectId::FullscreenMonitor => return self.fullscreen_monitor_options(),
            SelectId::SpecialKeys => return self.special_keys().into_iter().map(|(_, name)| (name, false)).collect(),
            SelectId::HostDisplay => return self.host_display_options(),
            _ => {}
        }
        let Some(set) = &self.settings else { return Vec::new() };
        match id {
            SelectId::Language => vec![
                (self.name_of(MsgKey::LangPt), set.language == Language::PtBr),
                (self.name_of(MsgKey::LangEn), set.language == Language::En),
            ],
            SelectId::Theme => vec![
                (self.name_of(MsgKey::ThemeSystem), set.theme == Theme::System),
                (self.name_of(MsgKey::ThemeLight), set.theme == Theme::Light),
                (self.name_of(MsgKey::ThemeDark), set.theme == Theme::Dark),
            ],
            SelectId::Quality => vec![
                (self.name_of(MsgKey::QualityAuto), set.quality == Quality::Auto),
                (self.name_of(MsgKey::QualityText), set.quality == Quality::Text),
                (self.name_of(MsgKey::QualityMotion), set.quality == Quality::Motion),
            ],
            SelectId::Keyboard => vec![
                (self.name_of(MsgKey::KeyboardAuto), set.keyboard_mode == KeyboardMode::Auto),
                (self.name_of(MsgKey::KeyboardPhysical), set.keyboard_mode == KeyboardMode::Physical),
                (self.name_of(MsgKey::KeyboardText), set.keyboard_mode == KeyboardMode::Text),
            ],
            SelectId::ReleaseKey | SelectId::FullscreenMonitor | SelectId::SpecialKeys | SelectId::HostDisplay => {
                Vec::new()
            }
        }
    }

    /// The other computer's displays for the screen button's list: "Tela 1 (principal), 2560 × 1600" and so on, the
    /// one shown checked.
    fn host_display_options(&self) -> Vec<(String, bool)> {
        let Some(s) = self.viewer_session() else { return Vec::new() };
        s.displays
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let name = if d.name.trim().is_empty() {
                    self.tm(&Msg::new(MsgKey::DisplayNumbered).with((i + 1).to_string()))
                } else {
                    d.name.clone()
                };
                let key = if d.primary { MsgKey::DisplayOptionPrimary } else { MsgKey::DisplayOption };
                let line = self.tm(&Msg::new(key).with(name).with(format!("{} × {}", d.width, d.height)));
                (line, s.display.map_or(d.primary, |shown| shown == d.id))
            })
            .collect()
    }

    /// The text shown in a select's field.
    pub fn select_value(&self, id: SelectId) -> String {
        self.select_options(id).into_iter().find(|(_, on)| *on).map(|(t, _)| t).unwrap_or_default()
    }

    fn pairing_controls(&self, s: &mut Specs, p: &PairingView) {
        let name = p.device_id.as_ref().and_then(|id| self.device_by_id(id)).map(|d| d.name.clone());
        match &p.state {
            PairingState::Open { remaining_s } => {
                s.heading(2, self.name_of(MsgKey::PairThisComputer));
                s.text(UiRole::Text, self.name_of(MsgKey::PairOpenBody));
                // The time the body refers to, next to it: the value is what the screen draws large ("1:42").
                let left = format::countdown(*remaining_s);
                s.text(UiRole::Progress, self.tm(&Msg::new(MsgKey::PairTimeLeft).with(left.clone())));
                if let Some(c) = s.v.last_mut() {
                    c.value = Some(left);
                }
                s.text(UiRole::Text, self.name_of(MsgKey::PairSearching));
                s.button(ControlId::PairCancel, self.name_of(MsgKey::PairCancel));
            }
            PairingState::Waiting => {
                s.heading(2, self.name_of(MsgKey::PairWaitingTitle));
                s.text(UiRole::Text, self.name_of(MsgKey::PairWaitingBody));
                s.button(ControlId::PairCancel, self.name_of(MsgKey::PairCancel));
            }
            PairingState::ShowCode { code, peer_name } => {
                s.heading(2, self.tm(&Msg::new(MsgKey::PairTitleWith).with(peer_name.clone())));
                s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::PairCompare).with(peer_name.clone())));
                s.heading(2, self.tm(&Msg::new(MsgKey::PairCodeLabel).with(code.clone())));
                if let Some(wait) = self.pair_wait() {
                    // This side confirmed: the warning gives way to what is waited for, and the only way out is "Cancelar".
                    s.text(UiRole::Text, wait.body);
                    for b in wait.buttons {
                        s.button(b.id, b.label);
                    }
                } else {
                    s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::PairWarn).with(peer_name.clone())));
                    s.add(ControlId::PairSame, UiRole::Button, self.name_of(MsgKey::PairSame));
                    s.add(ControlId::PairDifferent, UiRole::Button, self.name_of(MsgKey::PairDifferent));
                }
            }
            PairingState::Done { device_id } => {
                let who = self.device_by_id(device_id).map(|d| d.name.clone()).or(name).unwrap_or_default();
                s.heading(2, self.tm(&Msg::new(MsgKey::PairDoneTitle).with(who)));
                s.text(UiRole::Text, self.name_of(MsgKey::PairDoneBody));
                s.button(ControlId::PairDone, self.name_of(MsgKey::PairDoneButton));
            }
            PairingState::Failed { reason } => {
                s.heading(2, self.name_of(MsgKey::PairFailedTitle));
                s.text(
                    UiRole::Text,
                    format!("{} {}", self.error_text(&ApiError::new(*reason)), self.name_of(MsgKey::PairFailedBody)),
                );
                s.button(ControlId::PairRetry, self.name_of(MsgKey::PairRetry));
                s.button(ControlId::PairClose, self.name_of(MsgKey::PairClose));
            }
        }
    }

    /// The title of the incoming-session dialog.
    pub fn incoming_title(&self, i: &IncomingView) -> String {
        let key = match i.request.kind {
            RequestKind::Session { intent: Intent::Control } => MsgKey::IncomingTitleControl,
            _ => MsgKey::IncomingTitleView,
        };
        self.tm(&Msg::new(key).with(i.request.device.name.clone()))
    }

    /// The checkboxes of "Permitir nesta sessão" the dialog offers, in design order.
    pub fn incoming_boxes(&self) -> [(Permission, MsgKey); 5] {
        [
            (Permission::Control, MsgKey::AllowControl),
            (Permission::Audio, MsgKey::AllowAudio),
            (Permission::Files, MsgKey::AllowFiles),
            (Permission::Clipboard, MsgKey::AllowClipboard),
            (Permission::Terminal, MsgKey::AllowTerminal),
        ]
    }

    fn incoming_controls(&self, s: &mut Specs) {
        s.region(Region::Dialog);
        let Some(i) = &self.incoming else { return };
        s.heading(2, self.incoming_title(i));
        s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::IncomingSub).with(i.remaining_s.to_string())));
        for (p, key) in self.incoming_boxes() {
            let c = s.add(ControlId::Allow(p), UiRole::CheckBox, self.name_of(key));
            c.toggled = Some(i.allow.has(p));
        }
        let c = s.add(ControlId::RememberUnattended, UiRole::CheckBox, self.name_of(MsgKey::RememberUnattended));
        c.toggled = Some(i.remember_unattended);
        s.button(ControlId::Decline, self.name_of(MsgKey::Decline));
        s.button(ControlId::ViewOnlyAccept, self.name_of(MsgKey::ViewOnlyAccept));
        s.button(ControlId::Accept, self.name_of(MsgKey::Accept));
    }

    /// Texts of a modal dialog: title, body lines, extra line (countdown or status), primary, secondary.
    pub fn dialog_texts(&self, kind: &DialogKind) -> DialogTexts {
        match kind {
            DialogKind::Elevation => {
                let el = self.elevation.as_ref();
                let action = el.map(|e| self.elevation_action_text(&e.action)).unwrap_or_default();
                match el.map(|e| &e.phase) {
                    Some(ElevPhase::Waiting) => DialogTexts {
                        title: self.name_of(MsgKey::ElevTitle),
                        body: vec![action],
                        extra: Some(self.name_of(MsgKey::ElevWaiting)),
                        primary: None,
                        secondary: Some(self.name_of(MsgKey::ElevCancel)),
                    },
                    Some(ElevPhase::Failed(code)) => DialogTexts {
                        title: self.name_of(MsgKey::ElevFailedTitle),
                        body: vec![self.name_of(MsgKey::ElevFailedBody)],
                        extra: self.elevation_failure_line(*code),
                        primary: Some(self.name_of(MsgKey::ElevRetry)),
                        secondary: Some(self.name_of(MsgKey::ElevClose)),
                    },
                    _ => DialogTexts {
                        title: self.name_of(MsgKey::ElevTitle),
                        body: vec![action, self.name_of(self.elevation_body_key())],
                        extra: None,
                        primary: Some(self.name_of(MsgKey::ElevContinue)),
                        secondary: Some(self.name_of(MsgKey::ElevCancel)),
                    },
                }
            }
            DialogKind::ElevatedConfirm => DialogTexts {
                title: self.name_of(MsgKey::ElevTitle),
                body: vec![
                    self.confirm.as_ref().map(|c| c.line.clone()).unwrap_or_default(),
                    self.name_of(MsgKey::ElevConfirmBody),
                ],
                extra: None,
                primary: Some(self.name_of(MsgKey::ElevConfirm)),
                secondary: Some(self.name_of(MsgKey::ElevCancel)),
            },
            DialogKind::Consent => {
                let (name, feature, remaining) = match &self.incoming {
                    Some(i) => (
                        i.request.device.name.clone(),
                        match i.request.kind {
                            RequestKind::Consent { feature } => feature,
                            _ => Feature::Terminal,
                        },
                        i.remaining_s,
                    ),
                    None => (String::new(), Feature::Terminal, 0),
                };
                let key =
                    if feature == Feature::Terminal { MsgKey::ConsentTitleTerminal } else { MsgKey::ConsentTitleFiles };
                DialogTexts {
                    title: self.tm(&Msg::new(key).with(name)),
                    body: vec![self.name_of(MsgKey::ConsentBody)],
                    extra: Some(self.tm(&Msg::new(MsgKey::ConsentCountdown).with(remaining.to_string()))),
                    primary: Some(self.name_of(MsgKey::ConsentAllow)),
                    secondary: Some(self.name_of(MsgKey::ConsentDeny)),
                }
            }
            DialogKind::SwapConfirm => {
                let peer = self.swap_peer_name();
                DialogTexts {
                    title: self.name_of(MsgKey::SwapConfirmTitle),
                    body: vec![self.tm(&Msg::new(MsgKey::SwapConfirmBody).with(peer))],
                    extra: None,
                    primary: Some(self.name_of(MsgKey::SwapAskButton)),
                    secondary: Some(self.name_of(MsgKey::Cancel)),
                }
            }
            DialogKind::SwapAsk => {
                let (by, remaining) = match self.swap.as_ref().map(|s| &s.phase) {
                    Some(SwapPhase::Asked { by, remaining_s, .. }) => (by.clone(), *remaining_s),
                    _ => (String::new(), 0),
                };
                DialogTexts {
                    title: self.tm(&Msg::new(MsgKey::SwapAskTitle).with(by.clone())),
                    body: vec![self.tm(&Msg::new(MsgKey::SwapAskBody).with(by))],
                    extra: Some(self.tm(&Msg::new(MsgKey::SwapAskCountdown).with(remaining.to_string()))),
                    primary: Some(self.name_of(MsgKey::SwapAccept)),
                    secondary: Some(self.name_of(MsgKey::SwapDecline)),
                }
            }
        }
    }

    fn elevation_body_key(&self) -> MsgKey {
        match self.this.as_ref().map(|t| t.os) {
            Some(Os::MacOs) => MsgKey::ElevBodyMac,
            // D56: a standard account is asked for an administrator's name and password.
            _ if !self.admin_account => MsgKey::ElevBodyWindowsCredentials,
            _ => MsgKey::ElevBodyWindows,
        }
    }

    fn elevation_action_text(&self, action: &ElevatedAction) -> String {
        let peer = match action {
            ElevatedAction::SetGrants { device_id, .. } => {
                self.device_by_id(device_id).map(|d| d.name.clone()).unwrap_or_default()
            }
            ElevatedAction::ElevateSession { session_id } => self
                .sessions
                .iter()
                .find(|s| s.id == *session_id)
                .and_then(|s| self.device_by_id(&s.device_id))
                .map(|d| d.name.clone())
                .unwrap_or_default(),
            ElevatedAction::OpenAdminTerminal { .. } => String::new(),
        };
        elevation_action_text(self.lang, action, &peer)
    }

    /// The reason line of a failed elevation, by code (`None`: nothing more to say, the body already does).
    fn elevation_failure_line(&self, code: ApiCode) -> Option<String> {
        let key = match code {
            ApiCode::Refused => MsgKey::ElevDeclined,
            ApiCode::NotPermitted => MsgKey::ElevNotAdmin,
            ApiCode::Timeout => MsgKey::ElevTimedOut,
            ApiCode::NotFound => MsgKey::ErrNotFound,
            ApiCode::Busy => MsgKey::ErrBusy,
            ApiCode::ServiceDown => MsgKey::ErrServiceDown,
            ApiCode::NeedsAdmin => MsgKey::ErrNeedsAdmin,
            ApiCode::Locked => MsgKey::ErrLocked,
            ApiCode::Invalid => MsgKey::ErrInvalid,
            ApiCode::Internal => MsgKey::ErrInternal,
            ApiCode::NoSpace => MsgKey::TransferNoSpace, // never an elevation failure; the text is still true
        };
        Some(self.name_of(key))
    }

    pub(crate) fn swap_peer_name(&self) -> String {
        self.viewer_session().and_then(|s| self.device_by_id(&s.device_id)).map(|d| d.name.clone()).unwrap_or_default()
    }

    fn dialog_controls(&self, s: &mut Specs, kind: DialogKind) {
        s.region(Region::Dialog);
        let t = self.dialog_texts(&kind);
        s.heading(2, t.title);
        for b in t.body {
            s.text(UiRole::Text, b);
        }
        if let Some(e) = t.extra {
            s.text(UiRole::Text, e);
        }
        if let Some(p) = t.primary {
            s.button(ControlId::DialogPrimary, p);
        }
        if let Some(sec) = t.secondary {
            s.button(ControlId::DialogSecondary, sec);
        }
    }

    // ----- viewer and banner -----

    fn viewer_controls(&self) -> Vec<ControlSpec> {
        let mut s = Specs::new(Region::Toolbar);
        let Some(session) = self.viewer_session() else { return s.v };
        let peer = self.device_by_id(&session.device_id).map(|d| d.name.clone()).unwrap_or_default();
        if let Some(kind) = self.dialog(WindowKind::Viewer) {
            self.dialog_controls(&mut s, kind);
            return s.v;
        }
        s.region(Region::Content);
        s.heading(1, peer.clone());
        // What changes over the picture without the person acting (reconnecting, a pause, a swap's answer) is a polite
        // live region: a screen reader says it when it appears or changes, never each second (F-UI-01, R-UI-13).
        if let SessionState::Reconnecting { attempt } = &session.state {
            s.region(Region::Status);
            s.heading(2, self.tm(&Msg::new(MsgKey::ReconnectingTitle).with(peer)));
            s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::ReconnectingBody).with(attempt.to_string())));
            s.region(Region::Content);
            self.bar_progress(&mut s, self.reconnect_wait());
            s.button(ControlId::ReconnectDisconnect, self.name_of(MsgKey::Disconnect));
            return s.v;
        }
        // The card over the picture: as the painter picks it (a swap wait, else a pause, else a declined swap).
        let paused = if self.swap_wait().is_none() { self.paused_card() } else { None };
        if let Some((body, elevate)) = &paused {
            s.region(Region::Status);
            s.text(UiRole::Text, body.clone());
            s.region(Region::Content);
            if *elevate {
                s.button(ControlId::PausedElevate, self.name_of(MsgKey::ElevateButton));
            }
        }
        if let (None, Some(SwapView { phase: SwapPhase::Declined(_), .. })) = (&paused, &self.swap) {
            let (title, body) = self.swap_declined_texts();
            s.region(Region::Status);
            s.heading(2, title);
            s.text(UiRole::Text, body);
            s.region(Region::Content);
            s.button(ControlId::SwapDeclinedClose, self.name_of(MsgKey::Close));
        }
        if let Some(note) = self.screen_note() {
            s.region(Region::Status);
            s.text(UiRole::Text, note);
            s.region(Region::Content);
        }
        if let Some(w) = self.swap_wait() {
            s.region(Region::Status);
            s.heading(2, w.title);
            s.text(UiRole::Text, w.body);
            s.region(Region::Content);
            self.bar_progress(&mut s, Some(w.bar));
        }
        s.region(Region::Toolbar);
        // The session line rides on the toolbar (owner, 12/11): read with it, after the peer's heading.
        s.text(UiRole::Text, self.viewer_session_line(session));
        let screen_label = self.name_of(MsgKey::TbScreen);
        s.button(ControlId::TbScreen, screen_label);
        if self.viewer_extras {
            let c = s.add(ControlId::TbQuality, UiRole::ComboBox, self.name_of(MsgKey::Quality));
            c.value = self.quality_options().into_iter().find(|(_, _, on)| *on).map(|(_, t, _)| t);
        }
        let on = self.fit;
        let c = s.add(ControlId::TbFit, UiRole::Button, self.name_of(MsgKey::TbFit));
        c.toggled = Some(on);
        let on = self.audio_on;
        let c = s.add(ControlId::TbAudio, UiRole::Button, self.name_of(MsgKey::TbAudio));
        c.toggled = Some(on);
        if self.viewer_extras {
            let on = self.view_only;
            let c = s.add(ControlId::TbViewOnly, UiRole::Button, self.name_of(MsgKey::ViewOnly));
            c.toggled = Some(on);
        }
        let on = self.grab_on;
        let c = s.add(ControlId::TbGrab, UiRole::Button, self.name_of(MsgKey::TbGrab));
        c.toggled = Some(on);
        s.add(ControlId::TbKeys, UiRole::Button, self.name_of(MsgKey::TbKeys));
        // Arquivos is not a drawer tab: its button opens the explorer (`explorer_open`).
        for (id, key, tab) in [
            (ControlId::TbTerminal, MsgKey::TbTerminal, Some(DrawerTab::Terminal)),
            (ControlId::TbFiles, MsgKey::TbFiles, None),
            (ControlId::TbChat, MsgKey::TbChat, Some(DrawerTab::Chat)),
        ] {
            let open = if tab.is_none() { self.explorer_open() } else { self.drawer == tab };
            let unread = self.chat_unread();
            let label = if tab == Some(DrawerTab::Chat) && unread > 0 && !open {
                self.tm(&Msg::new(MsgKey::TbChatUnread).with(unread.to_string()))
            } else {
                self.name_of(key)
            };
            let c = s.add(id, UiRole::Button, label);
            c.toggled = Some(open);
        }
        // Swap between Windows and macOS is not in 1.0: the button exists only toward a machine of this OS.
        if self.viewer_session().is_none_or(|v| self.swap_offered(v)) {
            s.button(ControlId::TbSwap, self.name_of(MsgKey::TbSwap));
            // A request is waiting for the other side: one at a time, so the button waits too.
            if self.swap_requested()
                && let Some(c) = s.v.last_mut()
            {
                c.disabled = true;
            }
        }
        if self.can_elevate_session() {
            s.button(ControlId::TbElevate, self.name_of(MsgKey::ElevateButton));
        }
        s.button(ControlId::TbDisconnect, self.name_of(MsgKey::Disconnect));
        s.button(ControlId::TbCollapse, self.name_of(MsgKey::TbCollapse));
        // What each button does (the hint under it while the pointer is on it) is its description, where that says
        // more than its name.
        for c in s.v.iter_mut().filter(|c| c.region == Region::Toolbar) {
            c.description = self.toolbar_hint(c.id).filter(|hint| *hint != c.label);
        }
        if !self.session_bar_visible() || self.explorer_open() {
            // Full screen with the bar hidden, or the Arquivos view (its command bar's "Tela" goes back): the bar's
            // controls are not on screen, so they are not reachable either.
            s.v.retain(|c| c.region != Region::Toolbar);
        }
        if self.bar_handle_visible() && !self.explorer_open() {
            let keys = self.shortcut_keys(crate::shortcuts::ViewerAction::ToggleBar);
            s.region(Region::Content);
            s.button(ControlId::TbExpand, self.tm(&Msg::new(MsgKey::TbExpand).with(keys)));
        }
        if self.explorer_open() {
            s.region(Region::Drawer);
            self.explorer_controls(&mut s);
        }
        if let Some(open) = self.drawer {
            s.region(Region::Drawer);
            for (tab, key) in [
                (DrawerTab::Terminal, MsgKey::DrawerTerminal),
                (DrawerTab::Chat, MsgKey::DrawerChat),
                (DrawerTab::Clipboard, MsgKey::DrawerClipboard),
            ] {
                let c = s.add(ControlId::DrawerTab(tab), UiRole::Tab, self.name_of(key));
                c.selected = open == tab;
            }
            s.button(ControlId::DrawerClose, self.name_of(MsgKey::ClosePanel));
            match open {
                DrawerTab::Terminal => {
                    if matches!(self.term, TermPhase::Open { .. }) {
                        s.add(ControlId::TermGrid, UiRole::TextInput, self.name_of(MsgKey::DrawerTerminal));
                    }
                    if matches!(self.term, TermPhase::Exited { .. } | TermPhase::Failed { .. }) {
                        s.button(ControlId::TermReopen, self.name_of(MsgKey::TermReopen));
                    }
                    if self.admin_terminal_offered() && !self.term_is_admin() {
                        s.button(ControlId::TermAdmin, self.name_of(MsgKey::TermOpenAdmin));
                    }
                    let waiting = self.actions_disabled();
                    for (kind, key) in self.quick_actions() {
                        s.button(ControlId::Action(kind), self.name_of(key));
                        if waiting && let Some(c) = s.v.last_mut() {
                            c.disabled = true;
                        }
                    }
                    if let Some(w) = self.action_wait() {
                        s.text(UiRole::Text, w.line);
                    }
                }
                DrawerTab::Chat => {
                    // The conversation, oldest first; the newest message, when it came from the other side, is a
                    // polite live region, so a screen reader says it as it arrives.
                    let newest_in = self.chat.last().filter(|m| !m.mine).map(|m| m.msg_id);
                    for m in &self.chat {
                        let line = if m.mine {
                            self.tm(&Msg::new(MsgKey::ChatLineMine).with(m.text.clone()))
                        } else {
                            self.tm(&Msg::new(MsgKey::ChatLinePeer).with(peer.clone()).with(m.text.clone()))
                        };
                        let live = newest_in == Some(m.msg_id);
                        if live {
                            s.region(Region::Status);
                        }
                        s.text(UiRole::Text, line);
                        if live {
                            s.region(Region::Drawer);
                        }
                    }
                    s.add(ControlId::Edit(EditId::Chat), UiRole::TextInput, self.name_of(MsgKey::ChatLabel));
                    s.button(ControlId::ChatSend, self.name_of(MsgKey::ChatSendButton));
                }
                DrawerTab::Clipboard => {
                    let on = self.clipboard_on;
                    s.switch(
                        ControlId::ClipToggle,
                        self.name_of(MsgKey::ClipTitle),
                        on,
                        Some(self.name_of(MsgKey::ClipBody)),
                    );
                }
            }
        }
        s.v
    }

    /// The countdown row of a waiting card as the accessibility tree reads it: a progress element whose value is the
    /// time left (nothing when the wait has no deadline to show).
    fn bar_progress(&self, s: &mut Specs, bar: Option<WaitBar>) {
        let Some(WaitBar { text: Some(left), .. }) = bar else { return };
        s.text(UiRole::Progress, left.clone());
        if let Some(c) = s.v.last_mut() {
            c.value = Some(left);
        }
    }

    /// The quick action buttons in design order, with the label that fits the peer's system.
    pub fn quick_actions(&self) -> [(ActionKind, MsgKey); 4] {
        let peer_os = self.viewer_session().and_then(|s| self.device_by_id(&s.device_id)).map(|d| d.os);
        let task = if peer_os == Some(Os::Windows) { MsgKey::ActTaskMonitorWindows } else { MsgKey::ActTaskMonitorMac };
        [
            (ActionKind::Lock, MsgKey::ActLock),
            (ActionKind::TaskMonitor, task),
            (ActionKind::Sleep, MsgKey::ActSleep),
            (ActionKind::Restart, MsgKey::ActRestart),
        ]
    }

    fn banner_controls(&self) -> Vec<ControlSpec> {
        let mut s = Specs::new(Region::Banner);
        let Some(h) = self.host_session() else { return s.v };
        let peer = self.device_by_id(&h.device_id).map(|d| d.name.clone()).unwrap_or_default();
        s.text(UiRole::Text, self.banner_text(&peer, h));
        if let Some(line) = self.banner_files_line() {
            s.text(UiRole::Text, line);
        }
        let unread = self.chat_unread();
        let chat_label = if unread > 0 {
            self.tm(&Msg::new(MsgKey::BannerChatUnread).with(unread.to_string()))
        } else {
            self.name_of(MsgKey::BannerOpenChat)
        };
        s.button(ControlId::BannerChat, chat_label);
        if self.swap_offered(h) {
            s.button(ControlId::BannerSwap, self.name_of(MsgKey::TbSwap));
            if self.swap_requested()
                && let Some(c) = s.v.last_mut()
            {
                c.disabled = true;
            }
        }
        s.button(ControlId::BannerDisconnect, self.name_of(MsgKey::Disconnect));
        s.v
    }

    /// Whether "Inverter" is offered in session `s`: swap between Windows and macOS is not in 1.0, so only when the
    /// peer runs this machine's OS. When either OS is not known yet the control stays (the service refuses a swap
    /// toward another OS on its own).
    /// Since 1.1 a swap is offered toward any peer, Windows or Mac (F-XSWAP-01); a 1.0 peer across systems declines
    /// it on its own side and the decline card says so.
    pub fn swap_offered(&self, _s: &SessionView) -> bool {
        true
    }

    /// "MacBook do escritório controla este computador há 12 min".
    pub fn banner_text(&self, peer: &str, h: &SessionView) -> String {
        let (who, since) = self.banner_parts(peer, h);
        if since.is_empty() { who } else { format!("{who} {since}") }
    }

    /// The banner's two parts: who controls (bold) and since when (soft).
    ///
    /// While the hosted session reconnects, or this side's swap request waits, the first part says that
    /// (`ReconnectingTitle`, `SwapWaitingTitle`) and the second is empty.
    pub fn banner_parts(&self, peer: &str, h: &SessionView) -> (String, String) {
        if matches!(h.state, SessionState::Reconnecting { .. }) {
            return (self.tm(&Msg::new(MsgKey::ReconnectingTitle).with(peer)), String::new());
        }
        if self.swap_requested() && self.swap.as_ref().is_some_and(|w| w.session_id == h.id) {
            return (self.tm(&Msg::new(MsgKey::SwapWaitingTitle).with(peer)), String::new());
        }
        let (controls, since, elevated) = match &h.state {
            SessionState::Active { since_ms, granted, .. } => {
                (granted.has(Permission::Control), *since_ms, granted.has(Permission::Unattended))
            }
            _ => (true, self.clock.now_ms, false),
        };
        let key = if controls { MsgKey::BannerControls } else { MsgKey::BannerViews };
        let who = self.tm(&Msg::new(key).with(peer));
        let secs = (self.clock.now_ms.saturating_sub(since) / 1000) as u32;
        // ACCOUNTS.md 9.2: the host's person sees that the session was elevated (D55: "Controle elevado"), in place
        // of how long ago it started, so the line keeps its length.
        if elevated {
            return (who, format!("· {}", self.t(MsgKey::AuditElevated)));
        }
        (who, self.tm(&Msg::new(MsgKey::BannerSince).with(format::short_minutes(self.lang, secs))))
    }

    /// The rows of the blocked devices in Ajustes: the name (or "Dispositivo sem nome"), the line with the MAC, the
    /// last address and when, and the row's index.
    pub fn blocked_rows(&self) -> Vec<(String, String, u16)> {
        self.blocked
            .iter()
            .enumerate()
            .map(|(n, b)| {
                let name = if b.name.is_empty() { self.name_of(MsgKey::BlockedUnnamed) } else { b.name.clone() };
                let when = format::table_when(self.lang, b.blocked_at_ms, self.clock);
                let line =
                    self.tm(&Msg::new(MsgKey::BlockedLine).with(b.mac.clone()).with(b.last_ipv4.clone()).with(when));
                (name, line, u16::try_from(n).unwrap_or(u16::MAX))
            })
            .collect()
    }

    /// "você controla há 12 min" under the viewer's title.
    pub fn viewer_status_text(&self, s: &SessionView) -> String {
        match &s.state {
            SessionState::Active { since_ms, granted, .. } => {
                let secs = (self.clock.now_ms.saturating_sub(*since_ms) / 1000) as u32;
                let key =
                    if granted.has(Permission::Control) { MsgKey::ViewerControlling } else { MsgKey::ViewerViewing };
                self.tm(&Msg::new(key).with(format::short_minutes(self.lang, secs)))
            }
            _ => self.name_of(MsgKey::ViewerConnecting),
        }
    }

    /// The toolbar's session line: how long this has gone on, and, when Ajustes shows the numbers, the latency, the
    /// frame rate and the codec in use ("HEVC" or "H.264", F-VIEW-07). Drawn on the toolbar and read with it.
    pub fn viewer_session_line(&self, s: &SessionView) -> String {
        let status = self.viewer_status_text(s);
        match (self.settings(), s.stats) {
            (Some(set), Some(stats)) if set.show_stats => {
                let numbers = self.tm(&Msg::new(MsgKey::ViewerStats)
                    .with(stats.latency_ms.to_string())
                    .with(stats.fps.to_string())
                    .with(if stats.hevc { "HEVC" } else { "H.264" }));
                format!("{status} · {numbers}")
            }
            _ => status,
        }
    }

    // ----- activation -----

    pub(crate) fn session_id(&self) -> Option<u64> {
        self.viewer_session().map(|s| s.id)
    }

    /// The user activated a control (click, Enter, Space, or a screen reader's default action).
    pub fn activate(&mut self, id: ControlId) -> Activation {
        let mut a = Activation { changed: Changed::MAIN, ..Activation::default() };
        match id {
            ControlId::Device(i) => {
                if let Some(d) = self.devices.get(i as usize) {
                    let id = d.id.clone();
                    a.changed = self.select_device(&id);
                }
            }
            ControlId::NavHome => a.changed = self.navigate(Nav::Home),
            ControlId::HomeActivityAll => {
                a.changed = self.navigate(Nav::Activity);
                // Cached (R-UI-3): asked once, then kept fresh by events. Built on first open (R-UI-11).
                if self.fetch(DataKind::Activity) == Fetch::Missing {
                    self.begin_fetch(DataKind::Activity);
                    a.commands.push(Command::ActivityList { before_ms: None, limit: 200 });
                }
            }
            ControlId::NavSettings => {
                a.changed = self.navigate(Nav::Settings);
                if self.fetch(DataKind::Settings) == Fetch::Missing {
                    self.begin_fetch(DataKind::Settings);
                    a.commands.push(Command::GetSettings);
                }
                // The blocked devices are asked once; blocking and unblocking answer with the list as it then is.
                if !self.blocked_asked {
                    self.blocked_asked = true;
                    a.commands.push(Command::ListBlocked);
                }
            }
            ControlId::AddAddress => {
                a.effect = Some(Effect::SubmitEdit(EditId::Address));
                a.changed = Changed::NONE;
            }
            ControlId::PairThisComputer => {
                self.pairing = Some(PairingView {
                    id: 0,
                    device_id: None,
                    state: PairingState::Open { remaining_s: PAIR_COUNTDOWN_S },
                });
                a.commands.push(Command::PairOpen { device_id: None });
            }
            ControlId::PairWithDevice => {
                let device_id = self.selected_device().map(|d| d.id.clone());
                self.pairing = Some(PairingView {
                    id: 0,
                    device_id: device_id.clone(),
                    state: PairingState::Open { remaining_s: PAIR_COUNTDOWN_S },
                });
                a.commands.push(Command::PairOpen { device_id });
            }
            ControlId::ViewOnly | ControlId::Control => {
                if let Some(d) = self.selected_device() {
                    let device_id = d.id.clone();
                    let intent = if id == ControlId::Control { Intent::Control } else { Intent::View };
                    self.start_connect(device_id, intent, &mut a);
                }
            }
            ControlId::Wake => {
                if let Some(d) = self.selected_device().filter(|d| Self::can_wake(d)) {
                    a.commands.push(Command::Wake { device_id: d.id.clone() });
                }
            }
            ControlId::Tab(t) => a.changed = self.select_tab(t),
            ControlId::EditGrants => a.changed = self.select_tab(DeviceTab::Permissions),
            ControlId::Grant(p) => {
                if let Some(d) = self.selected_device().cloned().filter(|_| self.grant_available(p)) {
                    let grants = d.grants_here.toggled(p);
                    // R-UI-2: the switch moves at once and goes back if the service refuses; an admin grant waits
                    // for the service (the elevation step may come first).
                    if !p.needs_admin() || !grants.has(p) {
                        self.undo_slot = Some(Undo::Grants(d.id.clone(), d.grants_here));
                        if let Some(x) = self.devices.iter_mut().find(|x| x.id == d.id) {
                            x.grants_here = grants;
                        }
                    }
                    a.commands.push(Command::SetGrants { device_id: d.id, grants });
                }
            }
            ControlId::RenameDevice => {
                if let Some(d) = self.selected_device().filter(|d| d.paired) {
                    self.rename = Some(RenameDraft { device_id: d.id.clone(), error: None });
                    a.changed = Changed::MAIN;
                }
            }
            ControlId::RenameSave => a.effect = Some(Effect::SubmitEdit(EditId::Rename)),
            ControlId::RenameCancel => {
                self.rename = None;
                a.changed = Changed::MAIN;
            }
            ControlId::Forget => {
                if let Some(d) = self.selected_device() {
                    a.commands.push(Command::ForgetDevice { device_id: d.id.clone() });
                }
            }
            ControlId::Export => a.effect = Some(Effect::SaveLog),
            ControlId::BlockAlert => {
                if let Some(alert) = self.alert.take()
                    && let Some(addr) = alert.what.args.first()
                {
                    a.commands.push(Command::BlockAddress { ipv4: addr.clone() });
                }
            }
            ControlId::Unblock(n) => {
                // Optimistic (R-UI-2): the row goes at once; a failure shows as an error and the next visit lists again.
                if usize::from(n) < self.blocked.len() {
                    let item = self.blocked.remove(usize::from(n));
                    a.commands.push(Command::UnblockDevice { mac: item.mac });
                    a.changed = Changed::MAIN;
                }
            }
            ControlId::Edit(e) => a.effect = Some(Effect::FocusEdit(e)),
            ControlId::Select(s) => a.effect = Some(Effect::Menu(s)),
            ControlId::OpenAtLogin => self.toggle_setting(&mut a, |s| s.open_at_login = !s.open_at_login),
            ControlId::Receive => self.toggle_setting(&mut a, |s| s.service_enabled = !s.service_enabled),
            ControlId::LockOnEnd => self.toggle_setting(&mut a, |s| s.lock_on_end = !s.lock_on_end),
            ControlId::KeepAwake => self.toggle_setting(&mut a, |s| s.keep_awake = !s.keep_awake),
            ControlId::ShowStats => self.toggle_setting(&mut a, |s| s.show_stats = !s.show_stats),
            ControlId::SwapCtrlCmd => self.toggle_setting(&mut a, |s| s.swap_ctrl_cmd = !s.swap_ctrl_cmd),
            ControlId::PairSame => {
                // Never optimistic: the buttons wait for the service's answer.
                if let Some(p) = &self.pairing
                    && !self.pair_confirming
                {
                    a.commands.push(Command::PairConfirm { pairing_id: p.id, matches: true });
                    self.pair_confirming = true;
                }
            }
            ControlId::PairDifferent | ControlId::PairCancel => {
                if let Some(p) = self.pairing.take() {
                    if id == ControlId::PairDifferent {
                        a.commands.push(Command::PairConfirm { pairing_id: p.id, matches: false });
                    } else {
                        a.commands.push(Command::PairCancel { pairing_id: p.id });
                    }
                }
            }
            ControlId::PairDone => {
                self.pairing = None;
                if let Some(new) = self.last_new_device.take() {
                    self.select_device(&new);
                }
            }
            ControlId::PairClose => self.pairing = None,
            ControlId::PairRetry => {
                let device_id = self.pairing.as_ref().and_then(|p| p.device_id.clone());
                self.pairing = Some(PairingView {
                    id: 0,
                    device_id: device_id.clone(),
                    state: PairingState::Open { remaining_s: PAIR_COUNTDOWN_S },
                });
                a.commands.push(Command::PairOpen { device_id });
            }
            ControlId::Allow(p) => {
                if let Some(i) = &mut self.incoming {
                    i.allow = i.allow.toggled(p);
                }
            }
            ControlId::RememberUnattended => {
                if let Some(i) = &mut self.incoming {
                    i.remember_unattended = !i.remember_unattended;
                }
            }
            ControlId::Decline | ControlId::ViewOnlyAccept | ControlId::Accept => {
                if let Some(i) = self.incoming.take() {
                    let request_id = i.request.request_id;
                    let decision = match id {
                        ControlId::Decline => Decision::Decline,
                        ControlId::ViewOnlyAccept => Decision::ViewOnly,
                        // D8: nothing from this dialog grants Unattended or AdminTerminal.
                        _ => Decision::Accept {
                            grants: i.allow.without(Permission::Unattended).without(Permission::AdminTerminal),
                        },
                    };
                    a.commands.push(Command::AnswerIncoming { request_id, decision: decision.clone() });
                    if i.remember_unattended && id != ControlId::Decline {
                        // The next-time grant is a permission change: the service asks for an administrator (D7).
                        // An entry without a prompt gets what is stored, so what is stored is what this card
                        // permitted (watching only for "Só deixar ver"), with what the store already held besides
                        // (a device paired again holds nothing: the old store alone gave it View and nothing else).
                        let device = &i.request.device;
                        let permitted = match &decision {
                            Decision::Accept { grants } => *grants,
                            _ => Permissions::NONE.with(Permission::View),
                        };
                        let stored = self.device_by_id(&device.id).map_or(device.grants_here, |d| d.grants_here);
                        let grants = Permissions(stored.0 | permitted.0).with(Permission::Unattended);
                        a.commands.push(Command::SetGrants { device_id: device.id.clone(), grants });
                    }
                }
            }
            ControlId::DialogPrimary | ControlId::DialogSecondary => self.activate_dialog(id, &mut a),
            ControlId::Notice(k) => match k {
                NoticeKind::Public | NoticeKind::Firewall => {
                    // Windows' own page (the two notices come only from a Windows service): Network & internet,
                    // where each network's profile and the firewall's link are.
                    self.web_page = Some(crate::state::WINDOWS_NETWORK_SETTINGS.to_owned());
                    a.effect = Some(Effect::OpenWebPage);
                }
                NoticeKind::Service => {
                    a.commands.push(Command::OpenSystemSettings { target: SettingsTarget::LoginItems })
                }
                NoticeKind::MacPerms => a.commands.push(Command::RequestOsPermissions),
                NoticeKind::ReceiveFolder => {
                    self.web_page = Some(crate::state::MAC_FILES_AND_FOLDERS_SETTINGS.to_owned());
                    a.effect = Some(Effect::OpenWebPage);
                }
                NoticeKind::Crash => {
                    self.crash_notice = false;
                    a.effect = Some(Effect::OpenLogs);
                }
            },
            ControlId::NoComputerAddress => a.effect = Some(Effect::FocusEdit(EditId::Address)),
            ControlId::GuideSkip | ControlId::GuideDone => a.changed = self.close_guide(),
            ControlId::Update(action) => self.activate_update(action, &mut a),
            ControlId::WakeCancel | ControlId::ConnectCancel => {
                let device_id = self.selected_device().map(|d| d.id.clone());
                if let Some(device_id) = device_id
                    && let Some(e) = self.sessions.iter_mut().find(|s| s.device_id == device_id && s.is_waiting_entry())
                {
                    if e.id == 0 {
                        // No id to cancel yet: it is cancelled when the service gives it one.
                        e.cancel_asked = true;
                    } else {
                        a.commands.push(Command::Disconnect { session_id: e.id });
                    }
                    if let Some(intent) = e.intent {
                        a.effect = Some(Effect::Focus(start_button(intent)));
                    }
                }
            }
            ControlId::ConnectClose => {
                let intent = self.connect_end.as_ref().and_then(|e| e.intent);
                if self.dismiss_connect() != Changed::NONE {
                    a.effect = Some(Effect::Focus(start_button(intent.unwrap_or(Intent::Control))));
                }
            }
            ControlId::ConnectRetry => {
                if let Some(end) = self.connect_end.take() {
                    self.start_connect(end.device_id, end.intent.unwrap_or(Intent::Control), &mut a);
                }
            }
            ControlId::ReconnectDisconnect | ControlId::TbDisconnect | ControlId::BannerDisconnect => {
                let session = if id == ControlId::BannerDisconnect {
                    self.host_session().map(|s| s.id)
                } else {
                    self.session_id()
                };
                if let Some(session_id) = session {
                    a.commands.push(Command::Disconnect { session_id });
                }
                a.changed = Changed::ALL;
            }
            ControlId::SwapDeclinedClose => {
                self.swap = None;
                a.changed = Changed::VIEWER;
            }
            ControlId::ErrorClose => {
                self.error = None;
                a.changed = Changed::ALL;
            }
            ControlId::TbScreen => {
                if let Some(session_id) = self.session_id() {
                    // The list is asked again each time (a display may have come or gone); what is known opens now.
                    a.commands.push(Command::ListDisplays { session_id });
                    if self.viewer_session().is_some_and(|s| s.displays.len() > 1) {
                        a.effect = Some(Effect::Menu(SelectId::HostDisplay));
                    }
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::TbCollapse => {
                self.bar_collapsed = true;
                a.changed = Changed::VIEWER;
            }
            ControlId::TbExpand => {
                self.bar_collapsed = false;
                a.changed = Changed::VIEWER;
            }
            ControlId::TbFit => {
                self.undo_slot = Some(Undo::Toggle(Toggle::Fit, self.fit));
                self.fit = !self.fit;
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::SetScale {
                        session_id,
                        scale: if self.fit { Scale::Fit } else { Scale::OneToOne },
                    });
                }
                // Fit encodes at the window's size, 1:1 at the display's own.
                a.commands.extend(self.view_size_command());
                a.changed = Changed::VIEWER;
            }
            ControlId::TbQuality => {
                // The choice comes through `choose_quality`; the shell opens the list.
                a.changed = Changed::NONE;
            }
            ControlId::TbViewOnly => {
                self.undo_slot = Some(Undo::Toggle(Toggle::ViewOnly, self.view_only));
                self.view_only = !self.view_only;
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::SetViewOnly { session_id, on: self.view_only });
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::TbAudio => {
                self.undo_slot = Some(Undo::Toggle(Toggle::Audio, self.audio_on));
                self.audio_on = !self.audio_on;
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::SetAudio { session_id, on: self.audio_on });
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::TbGrab => {
                self.undo_slot = Some(Undo::Toggle(Toggle::Grab, self.grab_on));
                self.grab_on = !self.grab_on;
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::SetGrab { session_id, on: self.grab_on });
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::TbKeys => {
                // The list of keys opens under the button; the one picked goes through `choose`.
                if self.session_id().is_some() {
                    a.effect = Some(Effect::Menu(SelectId::SpecialKeys));
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::TbElevate | ControlId::PausedElevate => {
                if let Some(session_id) = self.viewer_session().filter(|_| self.can_elevate_session()).map(|v| v.id) {
                    a.commands.push(Command::ElevateSession { session_id });
                }
            }
            ControlId::TbTerminal => self.toggle_drawer(DrawerTab::Terminal, &mut a),
            ControlId::TbFiles => self.explorer_toggle(&mut a),
            ControlId::TbChat => self.toggle_drawer(DrawerTab::Chat, &mut a),
            // Not offered toward another OS (never drawn): an activation that still arrives does nothing.
            ControlId::TbSwap if self.viewer_session().is_some_and(|v| !self.swap_offered(v)) => {}
            ControlId::BannerSwap if self.host_session().is_some_and(|h| !self.swap_offered(h)) => {}
            ControlId::TbSwap | ControlId::BannerSwap if self.swap_requested() => a.changed = Changed::NONE,
            ControlId::TbSwap | ControlId::BannerSwap => {
                if id == ControlId::TbSwap {
                    if let Some(session_id) = self.session_id() {
                        self.swap = Some(SwapView { session_id, phase: SwapPhase::Confirm });
                    }
                } else if let Some(h) = self.host_session().map(|h| h.id) {
                    // Waiting for the answer: the banner's button waits too, until the outcome comes back.
                    self.begin_swap_request(h);
                    a.commands.push(Command::RequestSwap { session_id: h });
                }
                a.changed = Changed::ALL;
            }
            ControlId::DrawerTab(t) => {
                self.drawer = None;
                self.toggle_drawer(t, &mut a);
            }
            ControlId::DrawerClose => {
                self.drawer = None;
                a.changed = Changed::VIEWER;
            }
            // One at a time: a press while one waits is not sent (the buttons are disabled and say so).
            ControlId::Action(_) if self.actions_disabled() => a.changed = Changed::NONE,
            ControlId::Action(kind) => {
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::RunAction { session_id, action: kind });
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::TermGrid => a.changed = Changed::NONE,
            ControlId::TermAdmin => {
                self.begin_term(true, &mut a);
                a.changed = Changed::VIEWER;
            }
            ControlId::TermReopen => {
                let admin = self.term_is_admin();
                self.begin_term(admin, &mut a);
                a.changed = Changed::VIEWER;
            }
            // The explorer's transfers list: "Retomar" a paused one, "Cancelar" any that is not done.
            ControlId::TransferResume(transfer_id) => {
                a.commands.push(Command::TransferResume { transfer_id });
                a.changed = Changed::VIEWER;
            }
            ControlId::TransferCancel(transfer_id) => {
                a.commands.push(Command::TransferCancel { transfer_id });
                self.transfers.retain(|t| t.id != transfer_id);
                a.changed = Changed::VIEWER;
            }
            ControlId::ExpTela
            | ControlId::ExpCopy
            | ControlId::ExpRow(..)
            | ControlId::ExpPane(_)
            | ControlId::ExpCrumb(..)
            | ControlId::ExpSort(..)
            | ControlId::ExpBack(_)
            | ControlId::ExpForward(_)
            | ControlId::ExpUp(_)
            | ControlId::ExpRefresh(_)
            | ControlId::ExpPlace(..)
            | ControlId::ExpTransfers
            | ControlId::ExpNewFolder
            | ControlId::ExpRename
            | ControlId::ExpDelete
            | ControlId::ExpProps
            | ControlId::ExpHidden
            | ControlId::ExpPropsClose
            | ControlId::ExpForeverConfirm
            | ControlId::ExpForeverCancel
            | ControlId::ExpHiddenThere
            | ControlId::ExpThereTrashCancel
            | ControlId::ExpThereTrashConfirm
            | ControlId::ExpConflictCancel
            | ControlId::ExpConflictSkip
            | ControlId::ExpConflictReplace
            | ControlId::ExpConflictKeepBoth
            | ControlId::ExpConflictApplyAll => {
                self.explorer_activate(id, &mut a);
            }
            ControlId::BannerChat => a.changed = Changed::NONE,
            ControlId::ChatSend => {
                a.effect = Some(Effect::SubmitEdit(EditId::Chat));
                a.changed = Changed::NONE;
            }
            ControlId::ClipToggle => {
                self.undo_slot = Some(Undo::Toggle(Toggle::Clipboard, self.clipboard_on));
                self.clipboard_on = !self.clipboard_on;
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::SetClipboardSync { session_id, on: self.clipboard_on });
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::Static(_) => a.changed = Changed::NONE,
        }
        a
    }

    fn toggle_drawer(&mut self, tab: DrawerTab, a: &mut Activation) {
        a.changed = Changed::VIEWER;
        if self.drawer == Some(tab) {
            self.drawer = None;
            return;
        }
        self.drawer = Some(tab);
        let Some(session_id) = self.session_id() else { return };
        if tab == DrawerTab::Chat {
            // The panel shows everything now: the peer is told what was read.
            if let Some(up_to) = self.read_all_chat() {
                a.commands.push(Command::ChatMarkRead { session_id, up_to });
            }
        }
        match tab {
            // R-UI-11: the terminal is asked for the first time the panel opens (and again after a refusal).
            DrawerTab::Terminal if matches!(self.term, TermPhase::Idle | TermPhase::Failed { .. }) => {
                self.begin_term(false, a)
            }
            _ => {}
        }
    }

    /// Asks the host for a terminal (80 x 24: the shell sends the real size as soon as its panel has one). An open
    /// terminal is closed first, so one window never holds two.
    fn begin_term(&mut self, admin: bool, a: &mut Activation) {
        let Some(session_id) = self.session_id() else { return };
        if let (TermPhase::Open { .. }, Some(term_id)) = (&self.term, self.term_id) {
            a.commands.push(Command::TermClose { term_id });
        }
        self.term = TermPhase::Opening { admin };
        self.term_id = None;
        self.term_account = None;
        a.commands.push(Command::TermOpen { session_id, admin, cols: 80, rows: 24 });
    }

    /// This side's swap request is on its way: the answer is awaited for [`SWAP_WAIT_S`].
    fn begin_swap_request(&mut self, session_id: u64) {
        self.swap = Some(SwapView { session_id, phase: SwapPhase::Requested { request_id: 0 } });
        self.swap_left_s = SWAP_WAIT_S;
    }

    /// This side asked to swap and the other has not answered yet.
    pub fn swap_requested(&self) -> bool {
        matches!(self.swap.as_ref().map(|s| &s.phase), Some(SwapPhase::Requested { .. }))
    }

    /// Marks the other side's messages read; the highest id that was unread, when there was any.
    fn read_all_chat(&mut self) -> Option<u32> {
        let mut top = None;
        for m in self.chat.iter_mut().filter(|m| !m.mine && !m.read) {
            m.read = true;
            top = top.max(Some(m.msg_id));
        }
        top
    }

    fn toggle_setting(&mut self, a: &mut Activation, f: impl FnOnce(&mut Settings)) {
        let Some(old) = self.settings.clone() else { return };
        let mut new = old.clone();
        f(&mut new);
        self.undo_slot = Some(Undo::Settings(Box::new(old)));
        self.lang = new.language;
        self.theme = new.theme;
        self.settings = Some(new.clone());
        a.changed = Changed::ALL;
        a.commands.push(Command::SetSettings { settings: new });
        self.save_prefs();
    }

    fn activate_dialog(&mut self, id: ControlId, a: &mut Activation) {
        a.changed = Changed::ALL;
        let primary = id == ControlId::DialogPrimary;
        let Some(kind) = self.dialog(WindowKind::Main).or_else(|| self.dialog(WindowKind::Viewer)) else { return };
        match kind {
            DialogKind::Elevation => {
                let Some(el) = &mut self.elevation else { return };
                match (&el.phase, primary) {
                    (ElevPhase::Ask | ElevPhase::Failed(_), true) => {
                        el.phase = ElevPhase::Waiting;
                        self.elev_runs = self.elev_runs.wrapping_add(1);
                        el.run = self.elev_runs;
                        let (action, run) = (el.action.clone(), el.run);
                        let line = self.elevation_action_text(&action);
                        a.commands.push(Command::Elevate { action, line, run });
                    }
                    _ => self.elevation = None,
                }
            }
            DialogKind::ElevatedConfirm => {
                if let Some(c) = &mut self.confirm
                    && c.answer.is_none()
                {
                    c.answer = Some(primary);
                }
            }
            DialogKind::Consent => {
                if let Some(i) = self.incoming.take() {
                    let decision =
                        if primary { Decision::Accept { grants: i.request.preset } } else { Decision::Decline };
                    a.commands.push(Command::AnswerIncoming { request_id: i.request.request_id, decision });
                }
            }
            DialogKind::SwapConfirm => {
                if primary {
                    if let Some(session_id) = self.session_id() {
                        a.commands.push(Command::RequestSwap { session_id });
                        self.begin_swap_request(session_id);
                    } else {
                        self.swap = None;
                    }
                } else {
                    self.swap = None;
                }
            }
            DialogKind::SwapAsk => {
                if let Some(SwapView { phase: SwapPhase::Asked { request_id, .. }, .. }) = self.swap.take() {
                    a.commands.push(Command::AnswerSwap { request_id, accept: primary });
                }
            }
        }
    }

    /// The quality presets of the viewer's select, with the current one marked.
    pub fn quality_options(&self) -> Vec<(Quality, String, bool)> {
        [
            (Quality::Auto, MsgKey::QualityAuto),
            (Quality::Text, MsgKey::QualityText),
            (Quality::Motion, MsgKey::QualityMotion),
        ]
        .into_iter()
        .map(|(q, key)| (q, self.name_of(key), q == self.quality))
        .collect()
    }

    /// F-VIEW-06: the viewer window's video area in physical pixels and the refresh rate of its monitor (0 = unknown),
    /// reported by the shell whenever the layout, the DPI or the monitor changes. Returns the command that tells the
    /// host when what it should encode at changed: in Fit the area, in 1:1 mode 0x0 (native). The same figures again
    /// return nothing.
    pub fn set_view_area(&mut self, width: u32, height: u32, refresh_hz: u16) -> Vec<Command> {
        self.view_area = (width, height);
        self.view_hz = refresh_hz;
        self.view_size_command().into_iter().collect()
    }

    fn view_size_command(&mut self) -> Option<Command> {
        let session_id = self.session_id()?;
        let clamp = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
        let (width, height) = if self.fit { (clamp(self.view_area.0), clamp(self.view_area.1)) } else { (0, 0) };
        let key = (session_id, width, height, self.view_hz);
        if self.view_sent == Some(key) {
            return None;
        }
        self.view_sent = Some(key);
        Some(Command::SetViewSize { session_id, width, height, max_fps: self.view_hz })
    }

    /// The user picked a quality in the viewer's select: it changes at once and reverts if the service refuses.
    pub fn choose_quality(&mut self, quality: Quality) -> Activation {
        let mut a = Activation { changed: Changed::VIEWER, ..Activation::default() };
        if let Some(session_id) = self.session_id() {
            self.undo_slot = Some(Undo::Quality(self.quality));
            self.quality = quality;
            a.commands.push(Command::SetQuality { session_id, quality });
        }
        a
    }

    /// The user picked display `display_id` of the viewer's screen list.
    pub fn choose_display(&mut self, display_id: u16) -> Activation {
        let mut a = Activation { changed: Changed::VIEWER, ..Activation::default() };
        if let Some(session_id) = self.session_id()
            && let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id)
            && s.displays.iter().any(|d| d.id == display_id)
        {
            s.display = Some(display_id);
            a.commands.push(Command::SelectDisplay { session_id, display_id });
        }
        a
    }

    /// The chords of the "send keys" menu with the names that fit the controlled computer's system.
    pub fn special_keys(&self) -> Vec<(SpecialKeys, String)> {
        let mac =
            self.viewer_session().and_then(|s| self.device_by_id(&s.device_id)).is_some_and(|d| d.os == Os::MacOs);
        SpecialKeys::ALL
            .iter()
            .map(|k| {
                let key = match (k, mac) {
                    (SpecialKeys::CtrlAltDel, _) => MsgKey::KeysCtrlAltDel,
                    (SpecialKeys::AltTab, false) => MsgKey::KeysAltTab,
                    (SpecialKeys::AltTab, true) => MsgKey::KeysCmdTab,
                    (SpecialKeys::WinOrCmd, false) => MsgKey::KeysWindows,
                    (SpecialKeys::WinOrCmd, true) => MsgKey::KeysCmd,
                    (SpecialKeys::AltF4OrCmdQ, false) => MsgKey::KeysAltF4,
                    (SpecialKeys::AltF4OrCmdQ, true) => MsgKey::KeysCmdQ,
                    (SpecialKeys::PrintScreen, _) => MsgKey::KeysPrintScreen,
                };
                (*k, self.name_of(key))
            })
            .collect()
    }

    /// The user picked a chord from the "send keys" menu.
    pub fn send_keys(&mut self, keys: SpecialKeys) -> Activation {
        let mut a = Activation { changed: Changed::NONE, ..Activation::default() };
        if let Some(session_id) = self.session_id() {
            a.commands.push(Command::SendKeys { session_id, keys });
        }
        a
    }

    /// The user chose option `index` of a select.
    pub fn choose(&mut self, id: SelectId, index: usize) -> Activation {
        match id {
            SelectId::ReleaseKey => {
                let key = ReleaseKey::ALL[index.min(ReleaseKey::ALL.len() - 1)];
                let command = self.set_release_key(key);
                return Activation { changed: Changed::ALL, commands: vec![command], ..Activation::default() };
            }
            SelectId::FullscreenMonitor => return self.choose_fullscreen_monitor(index),
            SelectId::SpecialKeys => {
                let Some(&keys) = SpecialKeys::ALL.get(index) else { return Activation::default() };
                return self.send_keys(keys);
            }
            SelectId::HostDisplay => {
                let id = self.viewer_session().and_then(|s| s.displays.get(index)).map(|d| d.id);
                return id.map_or_else(Activation::default, |id| self.choose_display(id));
            }
            _ => {}
        }
        let mut a = Activation { changed: Changed::ALL, ..Activation::default() };
        self.toggle_setting(&mut a, |s| match id {
            SelectId::Language => s.language = if index == 0 { Language::PtBr } else { Language::En },
            SelectId::Theme => s.theme = [Theme::System, Theme::Light, Theme::Dark][index.min(2)],
            SelectId::Quality => s.quality = [Quality::Auto, Quality::Text, Quality::Motion][index.min(2)],
            SelectId::Keyboard => {
                s.keyboard_mode = [KeyboardMode::Auto, KeyboardMode::Physical, KeyboardMode::Text][index.min(2)]
            }
            SelectId::ReleaseKey | SelectId::FullscreenMonitor | SelectId::SpecialKeys | SelectId::HostDisplay => {}
        });
        if id == SelectId::Language {
            self.lang = self.settings.as_ref().map_or(self.lang, |s| s.language);
        }
        if id == SelectId::Theme {
            self.theme = self.settings.as_ref().map_or(self.theme, |s| s.theme);
        }
        self.save_prefs();
        a
    }

    /// "Renomear" is offered on the page of a paired device, except while it is being woken (that card replaces the page).
    pub fn rename_available(&self, d: &DeviceView) -> bool {
        d.paired && self.wait_card(&d.id).is_none()
    }

    /// A connect to a device: its card shows at once (id 0 until the service answers) and one `Connect` goes out. A
    /// device with a card waiting sends nothing.
    fn start_connect(&mut self, device_id: String, intent: Intent, a: &mut Activation) {
        if self.sessions.iter().any(|s| s.device_id == device_id && s.is_waiting_entry()) {
            a.changed = Changed::NONE;
            return;
        }
        if self.connect_end.as_ref().is_some_and(|e| e.device_id == device_id) {
            self.connect_end = None;
        }
        let mut entry = SessionView::new(0, device_id.clone(), SessionState::Connecting);
        entry.intent = Some(intent);
        self.sessions.push(entry);
        a.commands.push(Command::Connect { device_id, intent, unattended: false });
        a.effect = Some(Effect::Focus(ControlId::ConnectCancel));
        a.changed = Changed::MAIN;
    }

    /// The line under the rename field: what was wrong with the name, or the service's refusal.
    pub fn rename_error_text(&self, error: &ApiError) -> String {
        match &error.detail {
            Some(m) => self.tm(m),
            None => self.error_text(error),
        }
    }

    /// The text of a native input was committed (Enter, the send button, focus lost).
    pub fn submit_edit(&mut self, id: EditId, text: String) -> Activation {
        let mut a = Activation::default();
        match id {
            EditId::Name => {
                let text = text.trim().to_owned();
                if !text.is_empty() && self.settings.as_ref().is_some_and(|s| s.name != text) {
                    self.toggle_setting(&mut a, |s| s.name = text);
                }
            }
            EditId::ExpRename => self.submit_explorer_rename(text, &mut a),
            EditId::Rename => {
                let Some(device_id) = self.renaming().map(|r| r.device_id.clone()) else { return a };
                let name = text.trim().to_owned();
                let refused = if name.is_empty() {
                    Some(MsgKey::RenameEmpty)
                } else if name.len() > MAX_DEVICE_NAME {
                    Some(MsgKey::RenameTooLong)
                } else if name.chars().any(char::is_control) {
                    Some(MsgKey::RenameControlChars)
                } else {
                    None
                };
                a.changed = Changed::MAIN;
                if let Some(key) = refused {
                    let error = ApiError { code: ApiCode::Invalid, detail: Some(Msg::new(key)) };
                    self.rename = Some(RenameDraft { device_id, error: Some(error) });
                } else if self.device_by_id(&device_id).is_some_and(|d| d.name == name) {
                    // Nothing to change: the field just closes.
                    self.rename = None;
                } else {
                    self.rename = Some(RenameDraft { device_id: device_id.clone(), error: None });
                    a.commands.push(Command::RenameDevice { device_id, name });
                }
            }
            EditId::Address => {
                // The shell calls this 150 ms after the last key (R-UI-4, `Debouncer`); a half-typed address is
                // never sent.
                let text = text.trim().to_owned();
                if text.parse::<std::net::Ipv4Addr>().is_ok() {
                    a.commands.push(Command::AddByAddress { ipv4: text });
                }
            }
            EditId::Chat => {
                // Chat sends on Enter only, never on a timer (R-UI-4).
                let text = text.trim().to_owned();
                if let (false, Some(session_id)) = (text.is_empty(), self.session_id()) {
                    a.commands.push(Command::ChatSend { session_id, text });
                }
            }
        }
        a
    }
}

/// The words of a modal dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogTexts {
    pub title: String,
    pub body: Vec<String>,
    pub extra: Option<String>,
    pub primary: Option<String>,
    pub secondary: Option<String>,
}
