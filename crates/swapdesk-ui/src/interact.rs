//! What the user can reach on each window (the controls, in Tab order) and what activating one does.
//!
//! The shells draw the controls and register where each one landed; this module decides their names, roles and
//! states (for UI Automation and the keyboard) and the commands an activation sends. Nothing here knows about pixels.

use crate::api::*;
use crate::format;
use crate::state::*;
use crate::strings::MsgKey;
use crate::tokens::layout::PAIR_COUNTDOWN_S;

mod files_panel;
pub use files_panel::FILES_PAGE;
pub(crate) use files_panel::FilesPanel;

/// Everything a window can focus or announce. `Static` ones are text the screen reader reads, never focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlId {
    // sidebar
    Device(u16),
    PairThisComputer,
    NavActivity,
    NavSettings,
    // device header, tabs, overview
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
    // settings
    Edit(EditId),
    Select(SelectId),
    OpenAtLogin,
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
    WakeCancel,
    ReconnectDisconnect,
    SwapDeclinedClose,
    ErrorClose,
    // viewer toolbar and drawer
    TbScreen,
    TbFit,
    /// The viewer's quality select (the settings' default quality is `Select(SelectId::Quality)`).
    TbQuality,
    /// The viewer's "Só ver" switch (`ViewOnly` is the device header's "connect view-only" button).
    TbViewOnly,
    TbAudio,
    TbGrab,
    TbKeys,
    TbTerminal,
    TbFiles,
    TbChat,
    TbSwap,
    TbDisconnect,
    DrawerTab(DrawerTab),
    DrawerClose,
    Action(ActionKind),
    /// The terminal's grid: it takes the keyboard focus while a shell runs (the shell draws and handles it).
    TermGrid,
    /// "Abrir terminal de administrador" (F-TERM-02), offered when the session holds `ADMIN_TERMINAL`.
    TermAdmin,
    /// "Abrir de novo" after the shell ended or the host refused.
    TermReopen,
    FilesChange,
    FilesChoose,
    /// The panel goes up one folder of the other computer.
    FilesUp,
    /// Earlier (`false`) or later (`true`) entries of a long folder.
    FilesPage(bool),
    /// The row `n` of the folder's listing: opens a folder, downloads a file.
    FilesRow(u16),
    /// "Salvar em ...": choose the folder files are received into.
    FilesRecvFolder,
    /// "Retomar" on the paused transfer with this number.
    TransferResume(u32),
    /// "Cancelar" on the transfer with this number.
    TransferCancel(u32),
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
        }
    }
}

struct Specs {
    v: Vec<ControlSpec>,
    next_static: u16,
    region: Region,
}

impl Specs {
    fn new(region: Region) -> Specs {
        Specs { v: Vec::new(), next_static: 0, region }
    }
    fn region(&mut self, region: Region) {
        self.region = region;
    }
    fn add(&mut self, id: ControlId, role: UiRole, label: impl Into<String>) -> &mut ControlSpec {
        self.v.push(ControlSpec::new(id, role, self.region, label));
        self.v.last_mut().expect("just pushed")
    }
    fn button(&mut self, id: ControlId, label: impl Into<String>) {
        self.add(id, UiRole::Button, label);
    }
    fn text(&mut self, role: UiRole, label: impl Into<String>) {
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
        let lang = self.lang;
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
        // sidebar
        s.region(Region::Sidebar);
        let (paired, network) = self.device_groups();
        for i in paired.iter().chain(network.iter()) {
            let d = &self.devices[*i];
            let label = format!("{}, {}", d.name, format::status_line(lang, d));
            let selected = self.selected_device().is_some_and(|x| x.id == d.id) && self.nav == Nav::Devices;
            let c = s.add(ControlId::Device(*i as u16), UiRole::Button, label);
            c.selected = selected;
        }
        s.add(ControlId::Edit(EditId::Address), UiRole::TextInput, self.name_of(MsgKey::AddressLabel));
        s.button(ControlId::PairThisComputer, self.name_of(MsgKey::PairThisComputer));
        let c = s.add(ControlId::NavActivity, UiRole::Button, self.name_of(MsgKey::NavActivity));
        c.selected = self.nav == Nav::Activity;
        let c = s.add(ControlId::NavSettings, UiRole::Button, self.name_of(MsgKey::NavSettings));
        c.selected = self.nav == Nav::Settings;
        s.region(Region::Content);
        if let Some(e) = &self.error {
            s.text(UiRole::Text, self.error_text(e));
            s.button(ControlId::ErrorClose, self.name_of(MsgKey::Close));
        }
        match self.nav {
            Nav::Devices => self.devices_controls(&mut s),
            Nav::Activity => self.activity_controls(&mut s),
            Nav::Settings => self.settings_controls(&mut s),
        }
        s.v
    }

    fn devices_controls(&self, s: &mut Specs) {
        let lang = self.lang;
        let Some(d) = self.selected_device() else {
            if self.crash_notice {
                let (title, body, button) = self.notice_texts(NoticeKind::Crash);
                s.text(UiRole::Heading, title);
                s.text(UiRole::Text, body);
                s.button(ControlId::Notice(NoticeKind::Crash), button);
            }
            if self.no_computer_card() {
                s.text(UiRole::Heading, self.name_of(MsgKey::NoComputerTitle));
                s.text(UiRole::Text, self.name_of(MsgKey::NoComputerBody));
                s.button(ControlId::NoComputerAddress, self.name_of(MsgKey::NoComputerButton));
            } else {
                s.text(UiRole::Text, self.name_of(MsgKey::EmptyDevices));
            }
            return;
        };
        s.text(UiRole::Heading, d.name.clone());
        s.text(UiRole::Text, format::detail_line(lang, d));
        if let Some(w) = self.waking_session().filter(|w| w.device_id == d.id) {
            let _ = w;
            s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::WakingTitle).with(d.name.clone())));
            s.text(UiRole::Text, self.name_of(MsgKey::WakingBody));
            s.button(ControlId::WakeCancel, self.name_of(MsgKey::Cancel));
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
                s.text(UiRole::Heading, title);
                s.text(UiRole::Text, body);
                s.button(ControlId::Notice(k), button);
            }
            return;
        }
        if !d.paired {
            s.text(UiRole::Text, self.name_of(MsgKey::PairOpenBody));
            return;
        }
        match self.tab {
            DeviceTab::Overview => {
                s.text(UiRole::Text, self.name_of(MsgKey::SwapNote));
                s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::PermsPanelTitle).with(d.name.clone())));
                s.text(UiRole::Text, self.permission_summary(d.grants_here));
                s.add(ControlId::EditGrants, UiRole::Link, self.name_of(MsgKey::PermsPanelEdit));
                s.text(UiRole::Heading, self.name_of(MsgKey::LastSessionTitle));
                s.text(UiRole::Text, self.last_session_text(d));
            }
            DeviceTab::Permissions => {
                s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::PermsHereTitle).with(d.name.clone())));
                for (p, label, hint) in self.permission_rows() {
                    s.switch(ControlId::Grant(p), label, d.grants_here.has(p), hint);
                    if self.grant_waiting(p)
                        && let Some(c) = s.v.last_mut()
                    {
                        c.disabled = true;
                    }
                }
                s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::PermsThereTitle).with(d.name.clone())));
                for (label, answer) in self.there_rows(d) {
                    s.text(UiRole::Text, format!("{label}: {answer}"));
                }
                s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::ThereNote).with(d.name.clone())));
                s.button(ControlId::Forget, self.name_of(MsgKey::ForgetDevice));
            }
            DeviceTab::History => {
                s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::HistoryTitle).with(d.name.clone())));
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
                Some(self.name_of(MsgKey::PermAdminTerminalHint)),
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
        s.text(UiRole::Heading, self.name_of(MsgKey::ActivityTitle));
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
            s.button(ControlId::BlockAlert, self.tm(&Msg::new(MsgKey::BlockAddress).with(addr)));
        }
        s.add(ControlId::Static(900), UiRole::Table, self.name_of(MsgKey::LogLabel));
        for (n, row) in self.activity.iter().enumerate() {
            let when = format::table_when(self.lang, row.at_ms, self.clock);
            let who = if row.alert { self.name_of(MsgKey::UnknownDevice) } else { row.device_name.clone() };
            let dur = self.duration_cell(row);
            s.add(
                ControlId::Static(901 + n as u16),
                UiRole::Row,
                format!("{when}, {who}, {}, {dur}", self.tm(&row.what)),
            );
        }
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
        s.text(UiRole::Heading, self.name_of(MsgKey::SettingsTitle));
        let Some(set) = &self.settings else { return };
        let c = s.add(ControlId::Edit(EditId::Name), UiRole::TextInput, self.name_of(MsgKey::FieldName));
        c.value = Some(set.name.clone());
        let c = s.add(ControlId::Select(SelectId::Language), UiRole::ComboBox, self.name_of(MsgKey::FieldLanguage));
        c.value = Some(self.select_value(SelectId::Language));
        s.switch(ControlId::OpenAtLogin, self.name_of(MsgKey::OpenAtLogin), set.open_at_login, None);
        s.text(
            UiRole::Text,
            format!(
                "{}: {}",
                self.name_of(MsgKey::ServiceName),
                self.name_of(if set.service_enabled { MsgKey::ServiceActive } else { MsgKey::ServiceInactive })
            ),
        );
        s.switch(ControlId::LockOnEnd, self.name_of(MsgKey::LockOnEnd), set.lock_on_end, None);
        s.switch(ControlId::KeepAwake, self.name_of(MsgKey::KeepAwake), set.keep_awake, None);
        let c = s.add(ControlId::Select(SelectId::Quality), UiRole::ComboBox, self.name_of(MsgKey::Quality));
        c.value = Some(self.select_value(SelectId::Quality));
        s.switch(ControlId::ShowStats, self.name_of(MsgKey::ShowStats), set.show_stats, None);
        let c = s.add(ControlId::Select(SelectId::Keyboard), UiRole::ComboBox, self.name_of(MsgKey::SendKeys));
        c.value = Some(self.select_value(SelectId::Keyboard));
        c.description = Some(self.name_of(MsgKey::SendKeysHint));
        s.switch(ControlId::SwapCtrlCmd, self.name_of(MsgKey::SwapCtrlCmd), set.swap_ctrl_cmd, None);
    }

    /// The options of a select with the current one marked.
    pub fn select_options(&self, id: SelectId) -> Vec<(String, bool)> {
        let Some(set) = &self.settings else { return Vec::new() };
        match id {
            SelectId::Language => vec![
                (self.name_of(MsgKey::LangPt), set.language == Language::PtBr),
                (self.name_of(MsgKey::LangEn), set.language == Language::En),
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
        }
    }

    /// The text shown in a select's field.
    pub fn select_value(&self, id: SelectId) -> String {
        self.select_options(id).into_iter().find(|(_, on)| *on).map(|(t, _)| t).unwrap_or_default()
    }

    fn pairing_controls(&self, s: &mut Specs, p: &PairingView) {
        let name = p.device_id.as_ref().and_then(|id| self.device_by_id(id)).map(|d| d.name.clone());
        match &p.state {
            PairingState::Open { .. } => {
                s.text(UiRole::Heading, self.name_of(MsgKey::PairThisComputer));
                s.text(UiRole::Text, self.name_of(MsgKey::PairOpenBody));
                s.button(ControlId::PairCancel, self.name_of(MsgKey::PairCancel));
            }
            PairingState::Waiting => {
                s.text(UiRole::Heading, self.name_of(MsgKey::PairWaitingTitle));
                s.text(UiRole::Text, self.name_of(MsgKey::PairWaitingBody));
                s.button(ControlId::PairCancel, self.name_of(MsgKey::PairCancel));
            }
            PairingState::ShowCode { code, peer_name } => {
                s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::PairTitleWith).with(peer_name.clone())));
                s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::PairCompare).with(peer_name.clone())));
                s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::PairCodeLabel).with(code.clone())));
                s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::PairWarn).with(peer_name.clone())));
                let waiting = self.pair_confirming;
                s.add(ControlId::PairSame, UiRole::Button, self.name_of(MsgKey::PairSame)).disabled = waiting;
                s.add(ControlId::PairDifferent, UiRole::Button, self.name_of(MsgKey::PairDifferent)).disabled = waiting;
            }
            PairingState::Done { device_id } => {
                let who = self.device_by_id(device_id).map(|d| d.name.clone()).or(name).unwrap_or_default();
                s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::PairDoneTitle).with(who)));
                s.text(UiRole::Text, self.name_of(MsgKey::PairDoneBody));
                s.button(ControlId::PairDone, self.name_of(MsgKey::PairDoneButton));
            }
            PairingState::Failed { reason } => {
                s.text(UiRole::Heading, self.name_of(MsgKey::PairFailedTitle));
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
        s.text(UiRole::Heading, self.incoming_title(i));
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
                    Some(ElevPhase::Failed(_)) => DialogTexts {
                        title: self.name_of(MsgKey::ElevFailedTitle),
                        body: vec![self.name_of(MsgKey::ElevFailedBody)],
                        extra: None,
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
            _ => MsgKey::ElevBodyWindows,
        }
    }

    fn elevation_action_text(&self, action: &ElevatedAction) -> String {
        let device_name = |id: &str| self.device_by_id(id).map(|d| d.name.clone()).unwrap_or_default();
        match action {
            ElevatedAction::SetGrants { device_id, grants } => {
                let name = device_name(device_id);
                let key = if grants.has(Permission::Unattended) {
                    MsgKey::ElevActGrantUnattended
                } else if grants.has(Permission::AdminTerminal) {
                    MsgKey::ElevActGrantAdminTerminal
                } else {
                    MsgKey::ElevActGrants
                };
                self.tm(&Msg::new(key).with(name))
            }
            ElevatedAction::ConnectUnattended { device_id, .. } => {
                self.tm(&Msg::new(MsgKey::ElevActConnectUnattended).with(device_name(device_id)))
            }
            ElevatedAction::OpenAdminTerminal { .. } => self.name_of(MsgKey::ElevActAdminTerminalOpen),
        }
    }

    pub(crate) fn swap_peer_name(&self) -> String {
        self.viewer_session().and_then(|s| self.device_by_id(&s.device_id)).map(|d| d.name.clone()).unwrap_or_default()
    }

    fn dialog_controls(&self, s: &mut Specs, kind: DialogKind) {
        s.region(Region::Dialog);
        let t = self.dialog_texts(&kind);
        s.text(UiRole::Heading, t.title);
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
        s.text(UiRole::Heading, peer.clone());
        if let SessionState::Reconnecting { attempt } = &session.state {
            s.text(UiRole::Heading, self.tm(&Msg::new(MsgKey::ReconnectingTitle).with(peer)));
            s.text(UiRole::Text, self.tm(&Msg::new(MsgKey::ReconnectingBody).with(attempt.to_string())));
            s.button(ControlId::ReconnectDisconnect, self.name_of(MsgKey::Disconnect));
            return s.v;
        }
        if let Some(SwapView { phase: SwapPhase::Declined(_), .. }) = &self.swap {
            let (title, body) = self.swap_declined_texts();
            s.text(UiRole::Heading, title);
            s.text(UiRole::Text, body);
            s.button(ControlId::SwapDeclinedClose, self.name_of(MsgKey::Close));
        }
        s.region(Region::Toolbar);
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
        s.button(ControlId::TbKeys, self.name_of(MsgKey::TbKeys));
        for (id, key, tab) in [
            (ControlId::TbTerminal, MsgKey::TbTerminal, DrawerTab::Terminal),
            (ControlId::TbFiles, MsgKey::TbFiles, DrawerTab::Files),
            (ControlId::TbChat, MsgKey::TbChat, DrawerTab::Chat),
        ] {
            let open = self.drawer == Some(tab);
            let unread = self.chat_unread();
            let label = if tab == DrawerTab::Chat && unread > 0 && !open {
                self.tm(&Msg::new(MsgKey::TbChatUnread).with(unread.to_string()))
            } else {
                self.name_of(key)
            };
            let c = s.add(id, UiRole::Button, label);
            c.toggled = Some(open);
        }
        s.button(ControlId::TbSwap, self.name_of(MsgKey::TbSwap));
        // A request is waiting for the other side: one at a time, so the button waits too.
        if self.swap_requested()
            && let Some(c) = s.v.last_mut()
        {
            c.disabled = true;
        }
        s.button(ControlId::TbDisconnect, self.name_of(MsgKey::Disconnect));
        if let Some(open) = self.drawer {
            s.region(Region::Drawer);
            for (tab, key) in [
                (DrawerTab::Terminal, MsgKey::DrawerTerminal),
                (DrawerTab::Files, MsgKey::DrawerFiles),
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
                    for (kind, key) in self.quick_actions() {
                        s.button(ControlId::Action(kind), self.name_of(key));
                    }
                }
                DrawerTab::Files => self.files_controls(&mut s),
                DrawerTab::Chat => {
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
        let unread = self.chat_unread();
        let chat_label = if unread > 0 {
            self.tm(&Msg::new(MsgKey::BannerChatUnread).with(unread.to_string()))
        } else {
            self.name_of(MsgKey::BannerOpenChat)
        };
        s.button(ControlId::BannerChat, chat_label);
        s.button(ControlId::BannerSwap, self.name_of(MsgKey::TbSwap));
        if self.swap_requested()
            && let Some(c) = s.v.last_mut()
        {
            c.disabled = true;
        }
        s.button(ControlId::BannerDisconnect, self.name_of(MsgKey::Disconnect));
        s.v
    }

    /// "MacBook do escritório controla este computador há 12 min".
    pub fn banner_text(&self, peer: &str, h: &SessionView) -> String {
        let (who, since) = self.banner_parts(peer, h);
        format!("{who} {since}")
    }

    /// The banner's two parts: who controls (bold) and since when (soft).
    pub fn banner_parts(&self, peer: &str, h: &SessionView) -> (String, String) {
        let (controls, since) = match &h.state {
            SessionState::Active { since_ms, granted, .. } => (granted.has(Permission::Control), *since_ms),
            _ => (true, self.clock.now_ms),
        };
        let key = if controls { MsgKey::BannerControls } else { MsgKey::BannerViews };
        let who = self.tm(&Msg::new(key).with(peer));
        let secs = (self.clock.now_ms.saturating_sub(since) / 1000) as u32;
        (who, self.tm(&Msg::new(MsgKey::BannerSince).with(format::short_minutes(self.lang, secs))))
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

    // ----- activation -----

    fn session_id(&self) -> Option<u64> {
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
            ControlId::NavActivity => {
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
                    let intent = if id == ControlId::Control { Intent::Control } else { Intent::View };
                    a.commands.push(Command::Connect { device_id: d.id.clone(), intent, unattended: false });
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
                if let Some(d) = self.selected_device().cloned() {
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
            ControlId::Edit(e) => a.effect = Some(Effect::FocusEdit(e)),
            ControlId::Select(s) => a.effect = Some(Effect::Menu(s)),
            ControlId::OpenAtLogin => self.toggle_setting(&mut a, |s| s.open_at_login = !s.open_at_login),
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
                    a.commands.push(Command::AnswerIncoming { request_id, decision });
                    if i.remember_unattended && id != ControlId::Decline {
                        // The next-time grant is a permission change: the service asks for an administrator (D7).
                        let device = &i.request.device;
                        let grants = self
                            .device_by_id(&device.id)
                            .map_or(device.grants_here, |d| d.grants_here)
                            .with(Permission::Unattended);
                        a.commands.push(Command::SetGrants { device_id: device.id.clone(), grants });
                    }
                }
            }
            ControlId::DialogPrimary | ControlId::DialogSecondary => self.activate_dialog(id, &mut a),
            ControlId::Notice(k) => match k {
                NoticeKind::Public | NoticeKind::Firewall => {
                    a.commands.push(Command::OpenSystemSettings { target: SettingsTarget::NetworkCategory });
                }
                NoticeKind::Service => {
                    a.commands.push(Command::OpenSystemSettings { target: SettingsTarget::LoginItems })
                }
                NoticeKind::MacPerms => a.commands.push(Command::RequestOsPermissions),
                NoticeKind::Crash => {
                    self.crash_notice = false;
                    a.effect = Some(Effect::OpenLogs);
                }
            },
            ControlId::NoComputerAddress => a.effect = Some(Effect::FocusEdit(EditId::Address)),
            ControlId::WakeCancel => {
                if let Some(s) = self.waking_session() {
                    a.commands.push(Command::Disconnect { session_id: s.id });
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
                // The connect line that the strip already said goes with it.
                if self
                    .connect_end
                    .as_ref()
                    .is_some_and(|e| matches!(e.reason, ApiCode::Refused | ApiCode::Timeout | ApiCode::Busy))
                {
                    self.connect_end = None;
                }
                a.changed = Changed::ALL;
            }
            ControlId::TbScreen => {
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::ListDisplays { session_id });
                }
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
                if let Some(session_id) = self.session_id() {
                    a.commands.push(Command::SendKeys { session_id, keys: SpecialKeys::CtrlAltDel });
                }
                a.changed = Changed::VIEWER;
            }
            ControlId::TbTerminal => self.toggle_drawer(DrawerTab::Terminal, &mut a),
            ControlId::TbFiles => self.toggle_drawer(DrawerTab::Files, &mut a),
            ControlId::TbChat => self.toggle_drawer(DrawerTab::Chat, &mut a),
            ControlId::TbSwap | ControlId::BannerSwap if self.swap_requested() => a.changed = Changed::NONE,
            ControlId::TbSwap | ControlId::BannerSwap => {
                if id == ControlId::TbSwap {
                    if let Some(session_id) = self.session_id() {
                        self.swap = Some(SwapView { session_id, phase: SwapPhase::Confirm });
                    }
                } else if let Some(h) = self.host_session().map(|h| h.id) {
                    // Waiting for the answer: the banner's button waits too, until the outcome comes back.
                    self.swap = Some(SwapView { session_id: h, phase: SwapPhase::Requested { request_id: 0 } });
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
            ControlId::FilesChange
            | ControlId::FilesUp
            | ControlId::FilesPage(_)
            | ControlId::FilesRow(_)
            | ControlId::FilesChoose
            | ControlId::FilesRecvFolder
            | ControlId::TransferResume(_)
            | ControlId::TransferCancel(_) => {
                self.files_activate(id, &mut a);
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
            // R-UI-11: the file browser's state is built the first time the panel opens.
            DrawerTab::Files if self.listing.is_none() && self.fetch(DataKind::Listing) == Fetch::Missing => {
                self.begin_fetch(DataKind::Listing);
                a.commands.push(Command::FilesList { session_id, path: None });
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
        self.settings = Some(new.clone());
        a.changed = Changed::ALL;
        a.commands.push(Command::SetSettings { settings: new });
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
                        a.commands.push(Command::Elevate { action: el.action.clone() });
                    }
                    _ => self.elevation = None,
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
                        self.swap = Some(SwapView { session_id, phase: SwapPhase::Requested { request_id: 0 } });
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
        let mut a = Activation { changed: Changed::ALL, ..Activation::default() };
        self.toggle_setting(&mut a, |s| match id {
            SelectId::Language => s.language = if index == 0 { Language::PtBr } else { Language::En },
            SelectId::Quality => s.quality = [Quality::Auto, Quality::Text, Quality::Motion][index.min(2)],
            SelectId::Keyboard => {
                s.keyboard_mode = [KeyboardMode::Auto, KeyboardMode::Physical, KeyboardMode::Text][index.min(2)]
            }
        });
        if id == SelectId::Language {
            self.lang = self.settings.as_ref().map_or(self.lang, |s| s.language);
        }
        a
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
