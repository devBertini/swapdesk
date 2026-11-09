//! The accessibility tree of the sidebar and the address field (F-FIND-03, UI Automation patterns).

use accesskit::{Action, AriaCurrent, Node, Role, TreeUpdate};

use crate::a11y::{build_tree, node_id};
use crate::api::{ApiCode, ApiError, Command, Ticket, UiEvent};
use crate::interact::{ControlId, Region, UiRole};
use crate::mock::Scenario;
use crate::state::{EditId, UiState, WindowKind};

fn node_of(tree: &TreeUpdate, id: ControlId) -> &Node {
    let n = node_id(id);
    &tree.nodes.iter().find(|(i, _)| *i == n).expect("node").1
}

#[test]
fn a_selected_sidebar_item_keeps_the_click_action_and_stays_invocable() {
    let mut state = Scenario::Dispositivos.prepared_state();
    state.activate(ControlId::NavSettings);
    let tree = build_tree(&state, WindowKind::Main, &|_| None, None);
    for id in [ControlId::NavSettings, ControlId::NavHome] {
        let n = node_of(&tree, id);
        assert!(n.supports_action(Action::Click), "{id:?} lost Click");
        // UI Automation drops Invoke from any node with a selected state (accesskit_consumer::is_invocable), and
        // SelectionItem exists for tabs and list items only: a selected button would keep no pattern.
        assert_eq!(n.is_selected(), None, "{id:?} must not carry a selected state");
    }
    assert_eq!(node_of(&tree, ControlId::NavSettings).aria_current(), Some(AriaCurrent::True));
    assert_eq!(node_of(&tree, ControlId::NavHome).aria_current(), None);

    // The selected device row, too; tabs keep their real selected state (SelectionItem).
    let state = Scenario::Dispositivos.prepared_state();
    assert!(state.controls(WindowKind::Main).iter().any(|c| c.selected && c.role == UiRole::Button));
    let tree = build_tree(&state, WindowKind::Main, &|_| None, None);
    assert!(tree.nodes.iter().any(|(_, n)| n.role() == Role::Button
        && n.aria_current() == Some(AriaCurrent::True)
        && n.supports_action(Action::Click)));
    assert!(tree.nodes.iter().all(|(_, n)| n.role() != Role::Button || n.is_selected().is_none()));
    assert!(tree.nodes.iter().any(|(_, n)| n.role() == Role::Tab && n.is_selected() == Some(true)));
}

#[test]
fn the_home_lists_the_pair_card_with_the_address_field_after_the_sidebar() {
    let state = Scenario::Inicio.prepared_state();
    let specs = state.controls(WindowKind::Main);
    let pos = |id| specs.iter().position(|c| c.id == id).expect("control");
    let addr = pos(ControlId::Edit(EditId::Address));
    assert_eq!(specs[addr].role, UiRole::TextInput);
    assert_eq!(specs[addr].label, "Adicionar por endereço");
    assert_eq!(specs[addr].region, Region::Content);
    // Focus order: the sidebar (Início, the devices, Ajustes), then the home (heading, pair card, recent activity).
    let order = [
        ControlId::NavHome,
        ControlId::Device(0),
        ControlId::NavSettings,
        ControlId::PairThisComputer,
        ControlId::Edit(EditId::Address),
        ControlId::HomeActivityAll,
    ];
    let at: Vec<usize> = order.iter().map(|id| pos(*id)).collect();
    assert!(at.windows(2).all(|w| w[0] < w[1]), "Tab order {at:?}");
    for id in [ControlId::NavHome, ControlId::Device(0), ControlId::NavSettings] {
        assert_eq!(specs[pos(id)].region, Region::Sidebar);
    }
    assert!(specs[pos(ControlId::NavHome)].selected);
    assert_eq!(specs[pos(ControlId::NavHome)].label, "Início");
    let tree = build_tree(&state, WindowKind::Main, &|_| None, None);
    let n = node_of(&tree, ControlId::Edit(EditId::Address));
    assert_eq!(n.role(), Role::TextInput);
    assert!(n.supports_action(Action::Focus));
    assert_eq!(node_of(&tree, ControlId::NavHome).aria_current(), Some(AriaCurrent::True));
    // The old sidebar items are gone: no field, no pairing button on a device page.
    let page = Scenario::Dispositivos.prepared_state().controls(WindowKind::Main);
    assert!(!page.iter().any(|c| c.id == ControlId::Edit(EditId::Address) || c.id == ControlId::PairThisComputer));
}

#[test]
fn a_refused_address_shows_why_and_only_a_bare_ipv4_is_sent() {
    let mut s: UiState = Scenario::Inicio.prepared_state();
    for bad in ["", "abc", "192.168.0.", "192.168.0.21:47830", "192.168.0.21/24", "::1"] {
        assert!(s.submit_edit(EditId::Address, bad.into()).commands.is_empty(), "{bad:?}");
    }
    let a = s.submit_edit(EditId::Address, " 8.8.8.8 ".into());
    assert_eq!(a.commands, vec![Command::AddByAddress { ipv4: "8.8.8.8".into() }]);
    let t = Ticket(777);
    s.track(t, &a.commands[0]);
    s.apply(UiEvent::Reply { ticket: t, result: Err(ApiError::new(ApiCode::Refused)) });
    let e = s.error().expect("the refusal is shown").clone();
    let text = s.error_text(&e);
    assert!(text.contains("Recusado") && text.contains("rede local"), "{text}");
}
