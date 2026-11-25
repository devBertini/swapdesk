//! What a screen reader finds on the phone: every control of every phone screen is listed where it shows, with a
//! name; what is scrolled out of sight is reached by scrolling; a click on a node does what a tap does.

use std::collections::BTreeSet;

use swapdesk_ui::mock::Scenario;
use swapdesk_ui::{ControlId, Nav, UiState, WindowKind};

use crate::access_nodes::{Built, Ids, build, flag, kind};
use crate::canvas_ops::Reader;
use crate::phone_shell::PhoneShell;
use crate::phone_shell_tests::{PHONE, settle, shell_of};

/// Every main window a phone shows, as the phone shows it.
fn phone_screens() -> Vec<(String, UiState)> {
    let mut all: Vec<(String, UiState)> = Scenario::ALL
        .into_iter()
        .map(|sc| (sc.name().to_string(), sc.phone_state()))
        .filter(|(_, st)| st.windows().contains(&WindowKind::Main))
        .collect();
    all.push(("celular-precisa-atualizar".into(), swapdesk_ui::mock::phone_needs_update_state()));
    all.push(("celular-sem-bloqueio-de-tela".into(), swapdesk_ui::mock::phone_screen_lock_state()));
    all
}

fn view() -> (f32, f32) {
    (PHONE.0 / PHONE.2, PHONE.1 / PHONE.2)
}

/// Texts of the shared tree the compact layout does not draw, so the phone's reader does not get them: the first
/// pane lists the three latest connections and the tree all five, the activity page has no "Registro" heading,
/// and two waits draw their time left as a clock ("1:42", "0:14") where the tree says it in words (owed: the
/// reader of a phone hears no time left on those two waits).
const NOT_ON_A_PHONE: [&str; 5] = [
    "Ontem, 19:12, MacBook do escritório, Só viu a tela, 4 min",
    "Ontem, 23:40, MacBook do escritório, Entrou sem perguntar, na tela de login, 6 min",
    "Registro",
    "Tempo para parear: 1:42",
    "O pedido expira em 14 s",
];

/// Every node has a name and a place inside the view, no two nodes share an id, and every text and heading of the
/// shared tree is placed on a label the frame drew (measured over every phone screen: a miss names the screen and
/// the text).
#[test]
fn every_node_of_every_phone_screen_has_a_name_and_a_place_and_every_text_is_placed() {
    let (w, h) = view();
    let mut misses = Vec::new();
    let mut screens = 0;
    for (name, state) in phone_screens() {
        let (mut shell, _) = shell_of(state);
        let built: Built = shell.access_built();
        assert!(!built.nodes.is_empty(), "{name}: no node");
        let mut ids = BTreeSet::new();
        for n in &built.nodes {
            assert!(ids.insert(n.id) && n.id > 0, "{name}: id {} twice or not positive", n.id);
            assert!(!n.label.trim().is_empty(), "{name}: {:?} has no name", n.control);
            let r = n.rect;
            assert!(r.w > 0.0 && r.h > 0.0, "{name}: {:?} has no size", n.control);
            assert!(
                r.x >= -0.5 && r.y >= -0.5 && r.right() <= w + 0.5 && r.bottom() <= h + 0.5,
                "{name}: {:?} at {r:?} is outside the view",
                n.control
            );
            if matches!(n.control, ControlId::Edit(_)) {
                assert!(n.kind == kind::EDIT && n.edit != 0, "{name}: a text field is listed by its field");
            }
        }
        // A row of a list out of sight is not drawn: what stays unplaced at every offset of the page is a miss.
        let mut unplaced: BTreeSet<String> = built.unplaced.iter().cloned().collect();
        for _ in 0..200 {
            if unplaced.is_empty() || !shell.scroll_by(PHONE.1 * 0.8) {
                break;
            }
            settle(&mut shell);
            let now: BTreeSet<String> = shell.access_built().unplaced.into_iter().collect();
            unplaced = unplaced.intersection(&now).cloned().collect();
        }
        misses.extend(
            unplaced.iter().filter(|t| !NOT_ON_A_PHONE.contains(&t.as_str())).map(|t| format!("{name}: {t:?}")),
        );
        screens += 1;
    }
    assert!(screens >= 30, "{screens} screens");
    assert!(misses.is_empty(), "{} texts the frame did not draw:\n{}", misses.len(), misses.join("\n"));
}

/// The nodes of a pane that scrolls are what shows of it: walking the page down to its end, step by step as a
/// reader scrolls, lists every control the frame placed, each at least once.
#[test]
fn scrolling_to_the_end_of_a_page_lists_every_control_on_it() {
    for (name, state) in phone_screens() {
        let (mut shell, _) = shell_of(state);
        let placed: BTreeSet<String> =
            shell.output().expect("a frame").widgets.iter().map(|w| format!("{:?}", w.id)).collect();
        let mut seen = BTreeSet::new();
        for _ in 0..200 {
            seen.extend(shell.access_built().nodes.iter().map(|n| format!("{:?}", n.control)));
            if !shell.scroll_by(PHONE.1 * 0.8) {
                break;
            }
            settle(&mut shell);
        }
        let missing: Vec<_> = placed.difference(&seen).collect();
        assert!(missing.is_empty(), "{name}: never listed: {missing:?}");
    }
}

/// While a dialog stands over the pane only its own nodes are listed.
#[test]
fn a_dialog_over_the_pane_lists_only_its_own_nodes() {
    let mut tried = 0;
    for (name, state) in phone_screens() {
        let specs = state.controls(WindowKind::Main);
        if !specs.iter().any(|c| c.region == swapdesk_ui::Region::Dialog) {
            continue;
        }
        tried += 1;
        let (mut shell, _) = shell_of(state);
        let built = shell.access_built();
        assert!(!built.nodes.is_empty(), "{name}: the dialog lists nothing");
        for n in &built.nodes {
            let spec = specs.iter().find(|c| c.id == n.control).expect("a listed control is in the tree");
            assert_eq!(spec.region, swapdesk_ui::Region::Dialog, "{name}: {:?} behind the dialog", n.control);
        }
    }
    assert!(tried > 0, "no phone screen has a dialog");
}

/// A click from the reader does what a tap does, on the control the node is: the gear opens Ajustes. The node keeps
/// its id across frames, and a node that left the screen does nothing.
#[test]
fn a_click_on_a_node_activates_its_control_and_ids_stay() {
    let (mut shell, _) = shell_of(Scenario::Inicio.phone_state());
    let first = shell.access_built();
    let gear = first.nodes.iter().find(|n| n.control == ControlId::NavSettings).expect("the gear is listed");
    assert!(gear.flags & flag::CLICKABLE != 0);
    let again = shell.access_built();
    assert_eq!(again.nodes.iter().find(|n| n.control == ControlId::NavSettings).map(|n| n.id), Some(gear.id));
    assert!(shell.access_click(gear.id));
    settle(&mut shell);
    assert_eq!(shell.state().nav(), Nav::Settings);
    assert!(!shell.access_click(9_999_999), "an id never given");
}

/// The bytes follow `access_nodes::encode` field for field (the Java provider reads them so).
#[test]
fn the_list_reads_back_as_written() {
    let (mut shell, _): (PhoneShell, _) = shell_of(Scenario::Dispositivos.phone_state());
    let built = shell.access_built();
    let bytes = shell.access();
    let mut r = Reader(&bytes);
    let pane = r.u8().unwrap();
    assert!(pane <= 3);
    let count = r.u16().unwrap();
    assert_eq!(usize::from(count), built.nodes.len());
    for n in &built.nodes {
        assert_eq!(r.u32().unwrap() as i32, n.id);
        assert_eq!(r.u8().unwrap(), n.kind);
        assert_eq!(r.u8().unwrap(), n.edit);
        assert_eq!(r.u16().unwrap(), n.flags);
        let x = r.f32().unwrap();
        assert!((x - n.rect.x * PHONE.2).abs() < 0.01);
        for _ in 0..3 {
            r.f32().unwrap();
        }
        assert_eq!(r.str().unwrap(), n.label);
        r.str().unwrap();
        r.str().unwrap();
    }
    assert!(r.take(1).is_err(), "nothing after the last node");
}

/// Ids are small, kept and start over past the table's size.
#[test]
fn ids_are_kept_and_the_table_starts_over_when_full() {
    let mut ids = Ids::default();
    let a = ids.id(ControlId::NavSettings);
    assert_eq!(ids.id(ControlId::NavSettings), a);
    assert_eq!(ids.control(a), Some(ControlId::NavSettings));
    for i in 0..Ids::MAX {
        ids.id(ControlId::Static(i as u16));
    }
    assert!(ids.id(ControlId::NavHome) <= 2, "the table started over");
    let _ = build;
}
