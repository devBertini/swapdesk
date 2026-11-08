//! The view models: `UiState` turned into exactly what the AppKit screens build, as plain data. Pure Rust, no AppKit,
//! so the mapping is tested on every platform; the AppKit side only walks these values and creates controls.
//!
//! A screen is a list of [`Block`]s derived from the control specs `swapdesk-ui` already lists for the window (the same
//! specs its accessibility tree uses, so names and states agree with it), plus the structure the specs do not carry
//! (device rows, the viewer toolbar). Models compare with `==`: the shell rebuilds a window only when its model
//! changed, so an idle window does no work (R-UI-1, R-UI-8).

use swapdesk_ui::format;
use swapdesk_ui::state::PairingView;
use swapdesk_ui::{
    ConnectPhase, ControlId, ControlSpec, DataKind, EditId, Language, Msg, MsgKey, Nav, Quality, Ready, Region,
    SelectId, SessionState, SpecialKeys, UiRole, UiState, WindowKind,
};

// ---------- blocks ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Plain,
    Danger,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonModel {
    pub id: ControlId,
    pub label: String,
    pub description: Option<String>,
    pub style: ButtonStyle,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToggleModel {
    pub id: ControlId,
    pub label: String,
    pub description: Option<String>,
    pub on: bool,
    pub disabled: bool,
    /// A check box (incoming dialog) rather than a switch.
    pub check_box: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldModel {
    pub id: ControlId,
    pub edit: EditId,
    pub label: String,
    pub value: String,
    pub placeholder: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectModel {
    pub id: ControlId,
    pub select: SelectId,
    pub label: String,
    pub description: Option<String>,
    pub options: Vec<(String, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabModel {
    pub id: ControlId,
    pub label: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// `level` 1 is the screen's title, 2 a section.
    Heading {
        text: String,
        level: u8,
    },
    Text(String),
    /// The 6-digit pairing code, drawn large.
    Code {
        digits: String,
        label: String,
    },
    Button(ButtonModel),
    Toggle(ToggleModel),
    Field(FieldModel),
    Select(SelectModel),
    Tabs(Vec<TabModel>),
    /// A row of the activity log.
    Row(String),
    /// Placeholder lines while data loads (R-UI-1).
    Skeleton(u8),
}

impl Block {
    /// The controls this block carries with the name a screen reader reads for each.
    pub fn labels(&self) -> Vec<(ControlId, String)> {
        match self {
            Block::Button(b) => vec![(b.id, b.label.clone())],
            Block::Toggle(t) => vec![(t.id, t.label.clone())],
            Block::Field(f) => vec![(f.id, f.label.clone())],
            Block::Select(s) => vec![(s.id, s.label.clone())],
            Block::Tabs(t) => t.iter().map(|x| (x.id, x.label.clone())).collect(),
            _ => Vec::new(),
        }
    }

    /// The controls this block carries.
    pub fn control_ids(&self) -> Vec<ControlId> {
        match self {
            Block::Button(b) => vec![b.id],
            Block::Toggle(t) => vec![t.id],
            Block::Field(f) => vec![f.id],
            Block::Select(s) => vec![s.id],
            Block::Tabs(t) => t.iter().map(|x| x.id).collect(),
            _ => Vec::new(),
        }
    }
}

fn style_of(id: ControlId, role: UiRole) -> ButtonStyle {
    match id {
        ControlId::Control
        | ControlId::Accept
        | ControlId::PairSame
        | ControlId::PairDone
        | ControlId::PairRetry
        | ControlId::DialogPrimary
        | ControlId::PairWithDevice => ButtonStyle::Primary,
        ControlId::Decline | ControlId::PairDifferent | ControlId::Forget | ControlId::BlockAlert => {
            ButtonStyle::Danger
        }
        _ if role == UiRole::Link => ButtonStyle::Link,
        _ => ButtonStyle::Plain,
    }
}

/// The blocks of a list of specs, in order. `state` supplies what a spec lacks (a select's options, a text input's
/// placeholder). The first heading is level 1.
pub fn blocks_from(state: &UiState, specs: &[ControlSpec]) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut seen_heading = false;
    for c in specs {
        match c.role {
            UiRole::Heading => {
                let level = if seen_heading { 2 } else { 1 };
                seen_heading = true;
                out.push(Block::Heading { text: c.label.clone(), level });
            }
            UiRole::Text => out.push(Block::Text(c.label.clone())),
            UiRole::Row => out.push(Block::Row(c.label.clone())),
            UiRole::Table | UiRole::Cell | UiRole::ColumnHeader | UiRole::Progress => {}
            UiRole::Tab => {
                let tab = TabModel { id: c.id, label: c.label.clone(), selected: c.selected };
                if let Some(Block::Tabs(v)) = out.last_mut() {
                    v.push(tab);
                } else {
                    out.push(Block::Tabs(vec![tab]));
                }
            }
            UiRole::Button | UiRole::Link => out.push(Block::Button(ButtonModel {
                id: c.id,
                label: c.label.clone(),
                description: c.description.clone(),
                style: style_of(c.id, c.role),
                disabled: c.disabled,
            })),
            UiRole::Switch | UiRole::CheckBox => out.push(Block::Toggle(ToggleModel {
                id: c.id,
                label: c.label.clone(),
                description: c.description.clone(),
                on: c.toggled.unwrap_or(false),
                disabled: c.disabled,
                check_box: c.role == UiRole::CheckBox,
            })),
            UiRole::TextInput => {
                if let ControlId::Edit(edit) = c.id {
                    let placeholder =
                        if edit == EditId::Address { state.t(MsgKey::AddressPlaceholder) } else { String::new() };
                    let placeholder = if edit == EditId::Chat { state.t(MsgKey::ChatPlaceholder) } else { placeholder };
                    out.push(Block::Field(FieldModel {
                        id: c.id,
                        edit,
                        label: c.label.clone(),
                        value: c.value.clone().unwrap_or_default(),
                        placeholder,
                    }));
                }
            }
            UiRole::ComboBox => {
                if let ControlId::Select(select) = c.id {
                    out.push(Block::Select(SelectModel {
                        id: c.id,
                        select,
                        label: c.label.clone(),
                        description: c.description.clone(),
                        options: state.select_options(select),
                    }));
                }
            }
        }
    }
    out
}

/// In the pairing card the heading that carries the code becomes the large code itself.
fn code_blocks(state: &UiState, blocks: Vec<Block>) -> Vec<Block> {
    let Some(PairingView { state: swapdesk_ui::PairingState::ShowCode { code, .. }, .. }) = state.pairing() else {
        return blocks;
    };
    let heading = state.tm(&Msg::new(MsgKey::PairCodeLabel).with(code.clone()));
    blocks
        .into_iter()
        .map(|b| match b {
            Block::Heading { text, .. } if text == heading => {
                Block::Code { digits: code.clone(), label: heading.clone() }
            }
            other => other,
        })
        .collect()
}

// ---------- the main window ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: ControlId,
    pub name: String,
    pub status: String,
    pub selected: bool,
    /// What a screen reader says: the same label `swapdesk-ui`'s accessibility tree gives the row.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceGroup {
    pub title: String,
    pub rows: Vec<DeviceRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavItem {
    pub id: ControlId,
    pub label: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidebar {
    pub this_name: String,
    pub this_status: String,
    pub network_line: String,
    pub groups: Vec<DeviceGroup>,
    /// Placeholder rows instead of the list (R-UI-1).
    pub loading: bool,
    pub address: FieldModel,
    pub pair: ButtonModel,
    pub nav: Vec<NavItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripKind {
    Connecting,
    /// The other side's prompt is open, with a countdown.
    Prompted,
    Ended,
}

/// The connect flow's line on the main window: Connecting, Prompted with its countdown, Ended with its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strip {
    pub kind: StripKind,
    pub text: String,
    pub detail: Option<String>,
    /// "Cancelar" and the session it ends.
    pub cancel: Option<(String, u64)>,
    /// "Fechar" for an ended line.
    pub dismiss: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MainLayout {
    Browse {
        sidebar: Box<Sidebar>,
        content: Vec<Block>,
    },
    /// Pairing, an incoming request, a consent or elevation dialog: one centered card.
    Card {
        blocks: Vec<Block>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainModel {
    pub dark: bool,
    pub lang: Language,
    pub title: String,
    pub layout: MainLayout,
    pub strip: Option<Strip>,
}

impl MainModel {
    /// Every control the window offers with its accessible name, for the "every control has a place" test.
    pub fn control_labels(&self) -> Vec<(ControlId, String)> {
        let mut out = Vec::new();
        match &self.layout {
            MainLayout::Browse { sidebar, content } => {
                for g in &sidebar.groups {
                    out.extend(g.rows.iter().map(|r| (r.id, r.label.clone())));
                }
                out.push((sidebar.address.id, sidebar.address.label.clone()));
                out.push((sidebar.pair.id, sidebar.pair.label.clone()));
                out.extend(sidebar.nav.iter().map(|n| (n.id, n.label.clone())));
                out.extend(content.iter().flat_map(Block::labels));
            }
            MainLayout::Card { blocks } => out.extend(blocks.iter().flat_map(Block::labels)),
        }
        out
    }

    /// Every control the window offers, for the "every control has a place" test.
    pub fn control_ids(&self) -> Vec<ControlId> {
        let mut ids = Vec::new();
        match &self.layout {
            MainLayout::Browse { sidebar, content } => {
                for g in &sidebar.groups {
                    ids.extend(g.rows.iter().map(|r| r.id));
                }
                ids.push(sidebar.address.id);
                ids.push(sidebar.pair.id);
                ids.extend(sidebar.nav.iter().map(|n| n.id));
                for b in content {
                    ids.extend(b.control_ids());
                }
            }
            MainLayout::Card { blocks } => {
                for b in blocks {
                    ids.extend(b.control_ids());
                }
            }
        }
        ids
    }
}

fn strip_of(state: &UiState) -> Option<Strip> {
    let flow = state.connect_flow()?;
    let (text, detail) = state.connect_texts(&flow);
    Some(match flow.phase {
        ConnectPhase::Prompted { .. } => Strip {
            kind: StripKind::Prompted,
            text,
            detail,
            cancel: Some((state.t(MsgKey::Cancel), flow.session_id)),
            dismiss: None,
        },
        ConnectPhase::Connecting => Strip {
            kind: StripKind::Connecting,
            text,
            detail,
            cancel: Some((state.t(MsgKey::Cancel), flow.session_id)),
            dismiss: None,
        },
        // A refusal, a silence or a busy device is already in the state's own error line: no second line.
        ConnectPhase::Ended { in_error_line: true, .. } => return None,
        ConnectPhase::Ended { .. } => {
            Strip { kind: StripKind::Ended, text, detail, cancel: None, dismiss: Some(state.t(MsgKey::Close)) }
        }
    })
}

fn ready_text(state: &UiState, ready: Ready) -> String {
    state.t(match ready {
        Ready::ReadyToReceive => MsgKey::ReadyToReceive,
        Ready::ServiceOff => MsgKey::ReadyServiceOff,
        Ready::ServiceNeedsApproval => MsgKey::ReadyServiceNeedsApproval,
        Ready::NoNetwork => MsgKey::ReadyNoNetwork,
    })
}

fn sidebar_of(state: &UiState, specs: &[ControlSpec]) -> Sidebar {
    let (paired, network) = state.device_groups();
    let group = |title: MsgKey, indices: &[usize]| DeviceGroup {
        title: state.t(title),
        rows: indices
            .iter()
            .map(|&i| {
                let d = &state.devices()[i];
                let id = ControlId::Device(i as u16);
                let spec = specs.iter().find(|c| c.id == id);
                DeviceRow {
                    id,
                    name: d.name.clone(),
                    status: format::status_line(state.language(), d),
                    selected: spec.is_some_and(|c| c.selected),
                    label: spec.map(|c| c.label.clone()).unwrap_or_else(|| d.name.clone()),
                }
            })
            .collect(),
    };
    let mut groups = Vec::new();
    if !paired.is_empty() {
        groups.push(group(MsgKey::SidebarPaired, &paired));
    }
    if !network.is_empty() {
        groups.push(group(MsgKey::SidebarOnNetwork, &network));
    }
    let (this_name, this_status) =
        state.this_device().map(|t| (t.name.clone(), ready_text(state, t.ready))).unwrap_or_default();
    let network_line = match state.network() {
        Some(n) => match &n.address {
            Some(a) => state.tm(&Msg::new(MsgKey::NetworkLine).with(n.label.clone()).with(a.clone())),
            None => state.tm(&Msg::new(MsgKey::NetworkLineNoAddress).with(n.label.clone())),
        },
        None => String::new(),
    };
    let spec_of = |id: ControlId| specs.iter().find(|c| c.id == id);
    let address = FieldModel {
        id: ControlId::Edit(EditId::Address),
        edit: EditId::Address,
        label: state.t(MsgKey::AddressLabel),
        value: String::new(),
        placeholder: state.t(MsgKey::AddressPlaceholder),
    };
    let pair = ButtonModel {
        id: ControlId::PairThisComputer,
        label: spec_of(ControlId::PairThisComputer)
            .map(|c| c.label.clone())
            .unwrap_or_else(|| state.t(MsgKey::PairThisComputer)),
        description: None,
        style: ButtonStyle::Plain,
        disabled: false,
    };
    let nav = [
        (ControlId::NavActivity, MsgKey::NavActivity, Nav::Activity),
        (ControlId::NavSettings, MsgKey::NavSettings, Nav::Settings),
    ]
    .into_iter()
    .map(|(id, key, nav)| NavItem { id, label: state.t(key), selected: state.nav() == nav })
    .collect();
    Sidebar {
        this_name,
        this_status,
        network_line,
        groups,
        loading: state.placeholder(DataKind::Devices),
        address,
        pair,
        nav,
    }
}

/// The main window's model.
pub fn main_model(state: &UiState) -> MainModel {
    let specs = state.controls(WindowKind::Main);
    let card = state.dialog(WindowKind::Main).is_some() || state.incoming_session_open() || state.pairing().is_some();
    let layout = if card {
        MainLayout::Card { blocks: code_blocks(state, blocks_from(state, &specs)) }
    } else {
        let sidebar_specs: Vec<ControlSpec> = specs.iter().filter(|c| c.region == Region::Sidebar).cloned().collect();
        let content_specs: Vec<ControlSpec> = specs.iter().filter(|c| c.region != Region::Sidebar).cloned().collect();
        let mut content = blocks_from(state, &content_specs);
        match state.nav() {
            Nav::Devices => {
                if state.selected_device().is_none() && state.placeholder(DataKind::Devices) {
                    content.push(Block::Skeleton(3));
                }
            }
            Nav::Settings => {
                if state.settings().is_none() && state.placeholder(DataKind::Settings) {
                    content.push(Block::Skeleton(4));
                }
            }
            Nav::Activity => {
                if state.activity().is_empty() && state.placeholder(DataKind::Activity) {
                    content.push(Block::Skeleton(5));
                }
            }
        }
        MainLayout::Browse { sidebar: Box::new(sidebar_of(state, &sidebar_specs)), content }
    };
    MainModel {
        dark: state.dark(),
        lang: state.language(),
        title: state.t(MsgKey::AppName),
        layout,
        strip: strip_of(state),
    }
}

// ---------- the viewer window ----------

/// The video rectangle the render lane draws into: points, top-left origin, in the window's content view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoArea {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `backingScaleFactor` of the window's screen.
    pub scale: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchTool {
    Fit,
    Audio,
    Grab,
    ViewOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    Screen {
        label: String,
        current: String,
        options: Vec<(u16, String, bool)>,
    },
    Quality {
        label: String,
        options: Vec<(Quality, String, bool)>,
    },
    Switch {
        tool: SwitchTool,
        label: String,
        on: bool,
        disabled: bool,
    },
    Keys {
        label: String,
        options: Vec<(SpecialKeys, String)>,
    },
    /// The slot for "Inverter": always laid out, drawn only when the shell is told to show it.
    Swap {
        label: String,
    },
    Disconnect {
        label: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerModel {
    pub dark: bool,
    pub lang: Language,
    pub session_id: u64,
    pub title: String,
    pub status: String,
    pub stats: Option<String>,
    pub tools: Vec<Tool>,
    /// A card over the video: connecting, reconnecting, swap confirmation or refusal.
    pub overlay: Option<Vec<Block>>,
}

impl ViewerModel {
    pub fn control_ids(&self) -> Vec<ControlId> {
        let mut ids = Vec::new();
        for t in &self.tools {
            ids.push(match t {
                Tool::Screen { .. } => ControlId::TbScreen,
                Tool::Quality { .. } => ControlId::TbQuality,
                Tool::Switch { tool: SwitchTool::Fit, .. } => ControlId::TbFit,
                Tool::Switch { tool: SwitchTool::Audio, .. } => ControlId::TbAudio,
                Tool::Switch { tool: SwitchTool::Grab, .. } => ControlId::TbGrab,
                Tool::Switch { tool: SwitchTool::ViewOnly, .. } => ControlId::TbViewOnly,
                Tool::Keys { .. } => ControlId::TbKeys,
                Tool::Swap { .. } => ControlId::TbSwap,
                Tool::Disconnect { .. } => ControlId::TbDisconnect,
            });
        }
        if let Some(o) = &self.overlay {
            for b in o {
                ids.extend(b.control_ids());
            }
        }
        ids
    }
}

/// The viewer window's model, or `None` while no viewer session exists.
pub fn viewer_model(state: &UiState) -> Option<ViewerModel> {
    let session = state.viewer_session()?;
    let peer_dev = state.device_by_id(&session.device_id);
    let peer = peer_dev.map(|d| d.name.clone()).unwrap_or_default();
    let controllable = match &session.state {
        SessionState::Active { granted, .. } => granted.has(swapdesk_ui::Permission::Control),
        _ => true,
    };

    let displays = state.viewer_displays();
    let current =
        state.viewer_display().and_then(|c| displays.iter().position(|d| d.id == c)).map(|i| i + 1).unwrap_or(1);
    let screen_label = state.t(MsgKey::TbScreen);
    let tools = vec![
        Tool::Screen {
            label: screen_label,
            current: state.tm(&Msg::new(MsgKey::TbScreenN).with(current.to_string())),
            options: displays
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let name = if d.name.is_empty() {
                        state.tm(&Msg::new(MsgKey::TbScreenN).with((i + 1).to_string()))
                    } else {
                        d.name.clone()
                    };
                    (d.id, name, state.viewer_display() == Some(d.id))
                })
                .collect(),
        },
        Tool::Quality { label: state.t(MsgKey::Quality), options: state.quality_options() },
        Tool::Switch { tool: SwitchTool::Fit, label: state.t(MsgKey::TbFit), on: state.fit(), disabled: false },
        Tool::Switch {
            tool: SwitchTool::Audio,
            label: state.t(MsgKey::TbAudio),
            on: state.audio_on(),
            disabled: false,
        },
        Tool::Switch {
            tool: SwitchTool::ViewOnly,
            label: state.t(MsgKey::ViewOnly),
            on: state.view_only() || !controllable,
            disabled: !controllable,
        },
        Tool::Switch {
            tool: SwitchTool::Grab,
            label: state.t(MsgKey::TbGrab),
            on: state.grab_on(),
            disabled: !controllable,
        },
        Tool::Keys { label: state.t(MsgKey::TbKeys), options: state.special_keys() },
        Tool::Swap { label: state.t(MsgKey::TbSwap) },
        Tool::Disconnect { label: state.t(MsgKey::Disconnect) },
    ];

    let specs = state.controls(WindowKind::Viewer);
    let mut overlay_specs: Vec<ControlSpec> =
        specs.iter().filter(|c| matches!(c.region, Region::Dialog | Region::Content)).cloned().collect();
    // The first content heading is the peer's name (the window title already says it).
    if overlay_specs
        .first()
        .is_some_and(|c| c.region == Region::Content && c.role == UiRole::Heading && c.label == peer)
    {
        overlay_specs.remove(0);
    }
    let mut overlay = if overlay_specs.is_empty() { None } else { Some(blocks_from(state, &overlay_specs)) };
    if overlay.is_none() && !matches!(session.state, SessionState::Active { .. }) {
        overlay = Some(vec![
            Block::Heading { text: state.tm(&Msg::new(MsgKey::ConnectingTitle).with(peer.clone())), level: 1 },
            Block::Text(state.viewer_status_text(session)),
        ]);
    }

    let stats = match (session.stats, state.settings().is_some_and(|s| s.show_stats)) {
        (Some(s), true) => {
            Some(state.tm(&Msg::new(MsgKey::ViewerStats).with(s.latency_ms.to_string()).with(s.fps.to_string())))
        }
        _ => None,
    };

    Some(ViewerModel {
        dark: state.dark(),
        lang: state.language(),
        session_id: session.id,
        title: state.tm(&Msg::new(MsgKey::ViewerTitle).with(peer)),
        status: state.viewer_status_text(session),
        stats,
        tools,
        overlay,
    })
}
