//! The accessibility tree of the sidebar and the address field (F-FIND-03, UI Automation patterns).

use accesskit::{Action, AriaCurrent, Live, Node, Role, TreeUpdate};

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

#[test]
fn the_wait_card_is_a_polite_live_region_and_the_countdown_is_not() {
    let live_nodes = |tree: &TreeUpdate| -> Vec<accesskit::NodeId> {
        tree.nodes.iter().filter(|(_, n)| n.live().is_some_and(|l| l != Live::Off)).map(|(i, _)| *i).collect()
    };
    let state = Scenario::EstadoAguardandoAceite.prepared_state();
    let tree = build_tree(&state, WindowKind::Main, &|_| None, Some(ControlId::ConnectCancel));
    assert!(crate::a11y::validate(&tree).is_empty(), "{}", crate::a11y::dump(&tree));
    let card = state.wait_card(crate::mock::MAC).unwrap();

    // One polite region: the card's title and body, nothing else.
    let live = live_nodes(&tree);
    assert_eq!(live.len(), 1, "{}", crate::a11y::dump(&tree));
    let region = &tree.nodes.iter().find(|(i, _)| *i == live[0]).unwrap().1;
    assert_eq!((region.role(), region.live()), (Role::Status, Some(Live::Polite)));
    let texts: Vec<(Role, String)> = region
        .children()
        .iter()
        .map(|c| {
            let n = &tree.nodes.iter().find(|(i, _)| i == c).unwrap().1;
            (n.role(), n.label().unwrap_or_default().to_owned())
        })
        .collect();
    assert_eq!(texts, vec![(Role::Heading, card.title.clone()), (Role::Label, card.body.clone())]);

    // The countdown is a progress node outside the region, with the seconds as its value.
    let progress: Vec<_> = tree.nodes.iter().filter(|(_, n)| n.role() == Role::ProgressIndicator).collect();
    assert_eq!(progress.len(), 1);
    let (pid, p) = progress[0];
    assert_eq!(p.label(), Some("O pedido expira em 14 s"));
    assert_eq!(p.value(), Some("14"));
    assert!(!region.children().contains(pid), "the countdown is never announced each second");

    // The cancel button takes the focus and can be clicked.
    assert_eq!(tree.focus, node_id(ControlId::ConnectCancel));
    let cancel = node_of(&tree, ControlId::ConnectCancel);
    assert_eq!((cancel.role(), cancel.label()), (Role::Button, Some("Cancelar")));
    assert!(cancel.supports_action(Action::Click));

    // Connecting has the region too, and no progress node (no deadline to read yet); ended says why in the region.
    let tree = build_tree(&Scenario::EstadoConectando.prepared_state(), WindowKind::Main, &|_| None, None);
    assert_eq!(live_nodes(&tree).len(), 1);
    assert!(!tree.nodes.iter().any(|(_, n)| n.role() == Role::ProgressIndicator));
    let ended = Scenario::EstadoConexaoRecusada.prepared_state();
    let tree = build_tree(&ended, WindowKind::Main, &|_| None, None);
    assert_eq!(live_nodes(&tree).len(), 1);
    let dump = crate::a11y::dump(&tree);
    assert!(dump.contains("recusou.") && dump.contains("Não deu para conectar"), "{dump}");
    for id in [ControlId::ConnectClose, ControlId::ConnectRetry] {
        assert!(node_of(&tree, id).supports_action(Action::Click));
    }

    // Everything else is quiet: no live region on the normal page.
    let tree = build_tree(&Scenario::Dispositivos.prepared_state(), WindowKind::Main, &|_| None, None);
    assert!(live_nodes(&tree).is_empty());
}
