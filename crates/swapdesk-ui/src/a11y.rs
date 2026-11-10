//! The accessibility tree of each window, as an `accesskit` update (C6).
//!
//! The Windows shell hands it to `accesskit_windows` (UI Automation); the macOS shell maps the same roles and names
//! onto its AppKit controls. Names come from the same strings the screen shows; bounds are supplied by the shell,
//! since only it knows where it drew each control.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use accesskit::{Action, AriaCurrent, Live, Node, NodeId, Rect, Role, Toggled, TreeId, TreeInfo, TreeUpdate};

use crate::interact::{ControlId, ControlSpec, Region, UiRole};
use crate::state::{UiState, WindowKind};
use crate::strings::MsgKey;

/// The window's root node.
pub const ROOT: NodeId = NodeId(1);
const REGION_BASE: u64 = 2;
const TABLIST_BASE: u64 = 100;
const CONTROL_BIT: u64 = 1 << 63;

/// A stable node id for a control (the same control keeps its id across updates).
pub fn node_id(id: ControlId) -> NodeId {
    let mut h = DefaultHasher::new();
    id.hash(&mut h);
    NodeId(CONTROL_BIT | (h.finish() & !CONTROL_BIT))
}

/// The control a node id belongs to, among the controls the window has now.
pub fn control_of(state: &UiState, kind: WindowKind, node: NodeId) -> Option<ControlId> {
    state.controls(kind).into_iter().map(|c| c.id).find(|c| node_id(*c) == node)
}

fn role_of(r: UiRole) -> Role {
    match r {
        UiRole::Button => Role::Button,
        UiRole::Tab => Role::Tab,
        UiRole::Switch => Role::Switch,
        UiRole::CheckBox => Role::CheckBox,
        UiRole::Link => Role::Link,
        UiRole::ComboBox => Role::ComboBox,
        UiRole::TextInput => Role::TextInput,
        UiRole::Heading => Role::Heading,
        UiRole::Text => Role::Label,
        UiRole::Progress => Role::ProgressIndicator,
        UiRole::Table => Role::Table,
        UiRole::Row => Role::Row,
        UiRole::Cell => Role::Cell,
        UiRole::ColumnHeader => Role::ColumnHeader,
    }
}

fn region_id(r: Region) -> NodeId {
    NodeId(
        REGION_BASE
            + match r {
                Region::Sidebar => 0,
                Region::Content => 1,
                Region::Dialog => 2,
                Region::Toolbar => 3,
                Region::Drawer => 4,
                Region::Banner => 5,
                Region::Status => 6,
            },
    )
}

fn region_node(state: &UiState, r: Region) -> Node {
    let (role, name) = match r {
        Region::Sidebar => (Role::Navigation, state.t(MsgKey::SidebarNavLabel)),
        Region::Content => (Role::Main, state.t(MsgKey::AppName)),
        Region::Dialog => (Role::Dialog, state.t(MsgKey::RequestDialogLabel)),
        Region::Toolbar => (Role::Toolbar, state.t(MsgKey::ToolbarLabel)),
        Region::Drawer => (Role::Complementary, state.t(MsgKey::DrawerLabel)),
        Region::Banner => (Role::Status, state.t(MsgKey::BannerLabel)),
        // The wait card's title and body: its text is what is announced, so the region itself has no name.
        Region::Status => (Role::Status, String::new()),
    };
    let mut n = Node::new(role);
    if !name.is_empty() {
        n.set_label(name);
    }
    if r == Region::Status {
        n.set_live(Live::Polite);
    }
    if r == Region::Dialog {
        n.set_modal();
    }
    n
}

fn control_node(c: &ControlSpec, bounds: Option<(f64, f64, f64, f64)>) -> Node {
    let mut n = Node::new(role_of(c.role));
    n.set_label(c.label.clone());
    if let Some(d) = &c.description {
        n.set_description(d.clone());
    }
    if let Some(v) = &c.value {
        n.set_value(v.clone());
    }
    if let Some(on) = c.toggled {
        n.set_toggled(Toggled::from(on));
    }
    if c.selected {
        if c.role == UiRole::Tab {
            n.set_selected(true);
        } else {
            // UI Automation drops Invoke from any node that has a selected state, and exposes SelectionItem only
            // for tabs, list items and the like: a selected button (sidebar item) would keep no pattern at all.
            // "Current" keeps it a plain invocable button that still says which one is open.
            n.set_aria_current(AriaCurrent::True);
        }
    }
    if c.disabled {
        n.set_disabled();
    }
    if c.role == UiRole::Heading {
        n.set_level(1);
    }
    if c.id.focusable() {
        n.add_action(Action::Focus);
        if !matches!(c.role, UiRole::TextInput) {
            n.add_action(Action::Click);
        }
    }
    if let Some((x0, y0, x1, y1)) = bounds {
        n.set_bounds(Rect::new(x0, y0, x1, y1));
    }
    n
}

/// The full tree of one window. `bounds` gives a control's rectangle in client pixels, `focus` the focused control.
pub fn build_tree(
    state: &UiState,
    kind: WindowKind,
    bounds: &dyn Fn(ControlId) -> Option<(f64, f64, f64, f64)>,
    focus: Option<ControlId>,
) -> TreeUpdate {
    let specs = state.controls(kind);
    let mut nodes: Vec<(NodeId, Node)> = Vec::new();
    let mut root = Node::new(Role::Window);
    root.set_label(window_title(state, kind));
    let mut root_children: Vec<NodeId> = Vec::new();

    let mut tablists = 0u64;
    for region in [
        Region::Sidebar,
        Region::Status,
        Region::Content,
        Region::Dialog,
        Region::Toolbar,
        Region::Drawer,
        Region::Banner,
    ] {
        let in_region: Vec<&ControlSpec> = specs.iter().filter(|c| c.region == region).collect();
        if in_region.is_empty() {
            continue;
        }
        let mut children: Vec<NodeId> = Vec::new();
        let mut i = 0;
        while i < in_region.len() {
            let c = in_region[i];
            if c.role == UiRole::Tab {
                // Consecutive tabs share a tab list.
                let list_id = NodeId(TABLIST_BASE + tablists);
                tablists += 1;
                let mut list = Node::new(Role::TabList);
                list.set_label(state.t(MsgKey::TabsLabel));
                let mut tabs = Vec::new();
                while i < in_region.len() && in_region[i].role == UiRole::Tab {
                    let t = in_region[i];
                    nodes.push((node_id(t.id), control_node(t, bounds(t.id))));
                    tabs.push(node_id(t.id));
                    i += 1;
                }
                list.set_children(tabs);
                nodes.push((list_id, list));
                children.push(list_id);
                continue;
            }
            nodes.push((node_id(c.id), control_node(c, bounds(c.id))));
            children.push(node_id(c.id));
            i += 1;
        }
        let mut rn = region_node(state, region);
        rn.set_children(children);
        nodes.push((region_id(region), rn));
        root_children.push(region_id(region));
    }
    root.set_children(root_children);
    nodes.push((ROOT, root));

    let focus_node = focus.filter(|f| specs.iter().any(|c| c.id == *f)).map_or(ROOT, node_id);
    TreeUpdate { nodes, tree: Some(TreeInfo::new(ROOT)), tree_id: TreeId::ROOT, focus: focus_node }
}

/// The accessible title of a window.
pub fn window_title(state: &UiState, kind: WindowKind) -> String {
    match kind {
        WindowKind::Main => state.t(MsgKey::AppName),
        WindowKind::Viewer => {
            let peer = state
                .viewer_session()
                .and_then(|s| state.device_by_id(&s.device_id))
                .map(|d| d.name.clone())
                .unwrap_or_default();
            state.tm(&crate::api::Msg::new(MsgKey::ViewerTitle).with(peer))
        }
        WindowKind::Banner => state.t(MsgKey::BannerLabel),
    }
}

/// A readable dump of a tree: one line per node, indented by depth.
pub fn dump(update: &TreeUpdate) -> String {
    use std::collections::HashMap;
    let by_id: HashMap<NodeId, &Node> = update.nodes.iter().map(|(i, n)| (*i, n)).collect();
    let mut out = String::new();
    fn walk(by_id: &std::collections::HashMap<NodeId, &Node>, id: NodeId, depth: usize, out: &mut String) {
        let Some(n) = by_id.get(&id) else { return };
        let mut line = format!("{}{:?} \"{}\"", "  ".repeat(depth), n.role(), n.label().unwrap_or(""));
        if let Some(t) = n.toggled() {
            line.push_str(&format!(" toggled={t:?}"));
        }
        if n.is_selected() == Some(true) || n.aria_current() == Some(AriaCurrent::True) {
            line.push_str(" selected");
        }
        if let Some(v) = n.value() {
            line.push_str(&format!(" value=\"{v}\""));
        }
        if let Some(d) = n.description() {
            line.push_str(&format!(" description=\"{d}\""));
        }
        if n.supports_action(Action::Click) {
            line.push_str(" [click]");
        }
        if let Some(b) = n.bounds() {
            line.push_str(&format!(" @({:.0},{:.0} {:.0}x{:.0})", b.x0, b.y0, b.x1 - b.x0, b.y1 - b.y0));
        }
        out.push_str(&line);
        out.push('\n');
        for c in n.children() {
            walk(by_id, *c, depth + 1, out);
        }
    }
    walk(&by_id, update.tree.as_ref().map_or(ROOT, |t| t.root), 0, &mut out);
    out
}

/// Problems found in a tree: every interactive node needs a name and a role, every child must exist.
pub fn validate(update: &TreeUpdate) -> Vec<String> {
    let mut problems = Vec::new();
    let ids: std::collections::HashSet<NodeId> = update.nodes.iter().map(|(i, _)| *i).collect();
    for (id, n) in &update.nodes {
        let interactive = n.supports_action(Action::Click) || n.supports_action(Action::Focus);
        if interactive {
            if n.role() == Role::Unknown {
                problems.push(format!("node {id:?} has no role"));
            }
            if n.label().is_none_or(|l| l.trim().is_empty()) {
                problems.push(format!("interactive node {id:?} ({:?}) has no name", n.role()));
            }
        }
        for c in n.children() {
            if !ids.contains(c) {
                problems.push(format!("node {id:?} lists a missing child {c:?}"));
            }
        }
    }
    if !ids.contains(&update.focus) {
        problems.push("the focus node does not exist".to_owned());
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::Scenario;

    #[test]
    fn every_scenario_has_a_complete_tree_in_each_window() {
        for sc in Scenario::ALL {
            let state = sc.prepared_state();
            for kind in state.windows() {
                let tree = build_tree(&state, kind, &|_| None, None);
                let problems = validate(&tree);
                assert!(problems.is_empty(), "{} {kind:?}: {problems:?}\n{}", sc.name(), dump(&tree));
                assert!(tree.nodes.len() > 1, "{} {kind:?} has an empty tree", sc.name());
            }
        }
    }

    #[test]
    fn node_ids_are_stable_and_distinct() {
        assert_eq!(node_id(ControlId::Control), node_id(ControlId::Control));
        assert_ne!(node_id(ControlId::Control), node_id(ControlId::ViewOnly));
        assert_ne!(node_id(ControlId::Device(0)), node_id(ControlId::Device(1)));
    }

    #[test]
    fn switches_report_their_state() {
        let state = Scenario::Permissoes.prepared_state();
        let tree = build_tree(&state, WindowKind::Main, &|_| None, None);
        let text = dump(&tree);
        assert!(text.contains("Switch \"Ver a tela\" toggled=True"), "{text}");
        assert!(text.contains("Switch \"Terminal de administrador\" toggled=False"), "{text}");
    }

    #[test]
    fn focus_points_at_the_focused_control() {
        let state = Scenario::Dispositivos.prepared_state();
        let tree = build_tree(&state, WindowKind::Main, &|_| None, Some(ControlId::Control));
        assert_eq!(tree.focus, node_id(ControlId::Control));
        let tree = build_tree(&state, WindowKind::Main, &|_| None, Some(ControlId::Accept));
        assert_eq!(tree.focus, ROOT);
    }
}
