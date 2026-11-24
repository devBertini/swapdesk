//! The phone's shell over a backend that only records: what a finger does to the state, what goes to the backend,
//! and what is asked of Android.

use std::sync::{Arc, Mutex};

use swapdesk_paint::Rect;
use swapdesk_ui::mock::Scenario;
use swapdesk_ui::{Command, ControlId, EditId, Nav, SelectId, Ticket, UiBackend, UiEvent, UiState};

use crate::canvas_ops::{FRAME, Reader};
use crate::phone_shell::{Ask, PhoneShell, ask, edit_code, edit_of};
use crate::recorder_tests::{METRICS, NEAR};
use crate::text_measure::{Measured, decode_needs, encode_measured};

/// A backend that keeps what it was sent and answers nothing.
#[derive(Default)]
struct Recording {
    sent: Mutex<Vec<Command>>,
}

impl UiBackend for Recording {
    fn send(&self, command: Command) -> Ticket {
        let mut sent = self.sent.lock().expect("the list of commands");
        sent.push(command);
        Ticket(sent.len() as u64)
    }
    fn drain(&self) -> Vec<UiEvent> {
        Vec::new()
    }
}

/// A phone 360 by 740 DIPs at two device pixels each.
const PHONE: (f32, f32, f32) = (720.0, 1480.0, 2.0);

fn shell_of(state: UiState) -> (PhoneShell, Arc<Recording>) {
    let backend = Arc::new(Recording::default());
    let mut shell = PhoneShell::new(state, backend.clone());
    settle(&mut shell);
    (shell, backend)
}

/// Paints until a frame comes, answering each list of text as Android would.
fn settle(shell: &mut PhoneShell) {
    for _ in 0..8 {
        let bytes = shell.paint(METRICS, PHONE.0, PHONE.1, PHONE.2);
        if bytes[0] == FRAME {
            assert!(!shell.wants_frame(), "a frame leaves nothing to paint");
            return;
        }
        let answer: Vec<Measured> =
            decode_needs(&bytes).expect("the list reads").iter().map(|k| NEAR.measure(k)).collect();
        shell.measured(&encode_measured(&answer)).expect("the measures are taken");
    }
    panic!("no frame");
}

fn rect_of(shell: &PhoneShell, id: ControlId) -> Rect {
    let out = shell.output().expect("a frame");
    out.widgets.iter().find(|w| w.id == id).unwrap_or_else(|| panic!("{id:?} is not on the screen")).rect
}

/// The middle of a control, in device pixels.
fn middle(shell: &PhoneShell, id: ControlId) -> (f32, f32) {
    let (x, y) = rect_of(shell, id).center();
    (x * PHONE.2, y * PHONE.2)
}

fn tap(shell: &mut PhoneShell, id: ControlId) {
    let (x, y) = middle(shell, id);
    shell.press(x, y);
    assert_eq!(shell.interaction().pressed, Some(id), "the finger is on {id:?}");
    shell.release(x, y);
    assert_eq!(shell.interaction().pressed, None);
    settle(shell);
}

#[test]
fn a_tap_on_a_computer_opens_its_page_and_back_returns_to_the_first_pane() {
    let (mut shell, backend) = shell_of(Scenario::Inicio.prepared_state());
    assert_eq!(shell.state().nav(), Nav::Home);
    assert!(!shell.takes_back(), "on the first pane the back gesture is the system's");
    assert!(!shell.back() && shell.state().nav() == Nav::Home);

    tap(&mut shell, ControlId::Device(0));
    assert_eq!(shell.state().nav(), Nav::Devices);
    let name = shell.state().selected_device().map(|d| d.name.clone());
    assert_eq!(name, shell.state().devices().first().map(|d| d.name.clone()));
    assert!(shell.takes_back());

    // Back leaves the page, and on the first pane it is the system's again.
    assert!(shell.back());
    settle(&mut shell);
    assert_eq!(shell.state().nav(), Nav::Home);
    assert!(!shell.takes_back() && !shell.back());
    assert!(backend.sent.lock().unwrap().is_empty(), "moving between panes asks the backend nothing");

    // "Controlar" asks the backend to connect, and the page waits; back is then the wait's "Cancelar" (the state
    // drops the connect when the backend answers), and the page stays.
    tap(&mut shell, ControlId::Device(0));
    tap(&mut shell, ControlId::Control);
    let sent = backend.sent.lock().unwrap().clone();
    assert!(matches!(sent.as_slice(), [Command::Connect { .. }]), "{sent:?}");
    let cancel = shell.state().escape_target(swapdesk_ui::WindowKind::Main).expect("a card waits, with its way out");
    assert!(shell.output().unwrap().widgets.iter().any(|w| w.id == cancel), "and the way out is on the screen");
    assert!(shell.back() && shell.wants_frame());
    assert_eq!(shell.state().nav(), Nav::Devices);
}

#[test]
fn a_press_shows_and_a_drag_takes_it_back_and_scrolls_inside_the_panes_range() {
    let (mut shell, backend) = shell_of(Scenario::Ajustes.prepared_state());
    let (offset, max) = shell.scroll_range();
    assert!(offset == 0.0 && max > 200.0, "Ajustes is taller than a phone: {max}");
    let switch = ControlId::OpenAtLogin;
    let (x, y) = middle(&shell, switch);
    assert!(shell.scrolls_at(x, y) && !shell.scrolls_at(x, 20.0), "the pane scrolls, the bar does not");

    shell.press(x, y);
    assert!(shell.interaction().pressed == Some(switch) && shell.wants_frame(), "the press is drawn");
    settle(&mut shell);
    // The finger moves: Android calls it a scroll.
    shell.cancel();
    assert!(shell.interaction().pressed.is_none() && shell.wants_frame());
    assert!(shell.scroll_by(300.0));
    settle(&mut shell);
    assert_eq!(shell.scroll_range().0, 300.0);
    assert_eq!(rect_of(&shell, switch).center().1 * PHONE.2, y - 300.0, "the control moved up with the pane");
    assert!(backend.sent.lock().unwrap().is_empty(), "a drag switches nothing");

    // The range holds both ways, and a fling's position is taken as it is given.
    assert!(shell.scroll_by(1.0e6));
    assert_eq!(shell.scroll_range().0, max);
    assert!(!shell.scroll_by(50.0), "already at the end");
    assert!(shell.scroll_to(-40.0) && shell.scroll_range().0 == 0.0);
    assert!(shell.scroll_to(max / 2.0) && shell.scroll_range().0 == max / 2.0);

    // A release away from where the finger went down activates nothing.
    settle(&mut shell);
    let (x, y) = middle(&shell, ControlId::NavHome);
    shell.press(x, y);
    shell.release(x + 400.0, y + 600.0);
    assert_eq!(shell.state().nav(), Nav::Settings);
    // Another pane starts at its top.
    tap(&mut shell, ControlId::NavHome);
    assert_eq!((shell.state().nav(), shell.scroll_range().0), (Nav::Home, 0.0));
}

#[test]
fn a_select_asks_android_for_its_list_and_the_choice_changes_the_setting() {
    let (mut shell, backend) = shell_of(Scenario::Ajustes.prepared_state());
    tap(&mut shell, ControlId::Select(SelectId::Theme));
    let options = shell.state().select_options(SelectId::Theme);
    let selected = options.iter().position(|(_, on)| *on);
    assert!(options.len() >= 3 && selected.is_some());
    let names: Vec<String> = options.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(shell.asks(), [Ask::Menu { items: names.clone(), selected }]);

    // The list as Java reads it.
    let bytes = shell.take_asks();
    let mut r = Reader(&bytes);
    assert_eq!((r.u8().unwrap(), r.u8().unwrap()), (1, ask::MENU));
    assert_eq!(usize::from(r.u16().unwrap()), selected.unwrap());
    let count = usize::from(r.u16().unwrap());
    let read: Vec<String> = (0..count).map(|_| r.str().unwrap()).collect();
    assert_eq!((read, r.0.len()), (names, 0));
    assert_eq!(shell.take_asks(), vec![0], "taken once");

    // Closed without a choice: nothing changes. Then a choice.
    shell.chose(None);
    assert!(backend.sent.lock().unwrap().is_empty());
    tap(&mut shell, ControlId::Select(SelectId::Theme));
    let other = (0..options.len()).find(|i| Some(*i) != selected).unwrap();
    shell.chose(Some(other));
    assert_eq!(shell.state().select_options(SelectId::Theme).iter().position(|(_, on)| *on), Some(other));
    assert!(!backend.sent.lock().unwrap().is_empty() && shell.wants_frame());
    // An answer with no list open is nothing.
    shell.chose(Some(0));
    assert_eq!(shell.state().select_options(SelectId::Theme).iter().position(|(_, on)| *on), Some(other));
}

#[test]
fn text_fields_are_told_to_java_and_their_text_comes_back_through_the_state() {
    let (mut shell, backend) = shell_of(Scenario::Inicio.prepared_state());
    // The address field of the pairing card: its slot, in device pixels, inside the pane.
    let bytes = shell.edits();
    let mut r = Reader(&bytes);
    let view: [f32; 4] = r.floats().unwrap();
    assert_eq!(view, [0.0, 112.0, 720.0, 1480.0 - 112.0], "the pane under the bar");
    assert_eq!(r.f32().unwrap(), 28.0, "14 px text at two pixels each");
    let (ink, hint) = (r.u32().unwrap(), r.u32().unwrap());
    assert!(ink >> 24 == 0xFF && hint >> 24 == 0xFF && ink != hint);
    assert_eq!((r.u8().unwrap(), r.u8().unwrap()), (1, edit_code(EditId::Address)));
    let slot: [f32; 4] = r.floats().unwrap();
    let boxed = rect_of(&shell, ControlId::Edit(EditId::Address));
    assert!(slot[0] > boxed.x * 2.0 && slot[0] + slot[2] < boxed.right() * 2.0, "{slot:?} inside {boxed:?}");
    assert_eq!(r.str().unwrap(), "");
    assert!(!r.str().unwrap().is_empty() && r.0.is_empty(), "the placeholder, and nothing after it");

    // The card is below the fold: there the field takes no finger. Scrolled into sight, a tap on its box puts the
    // keyboard in it, and "Adicionar" reads it.
    let (x, y) = middle(&shell, ControlId::Edit(EditId::Address));
    assert!(y > PHONE.1, "the field starts out of sight, at {y}");
    shell.press(x, y);
    assert_eq!(shell.interaction().pressed, None);
    shell.cancel();
    assert!(shell.scroll_by(y - PHONE.1 / 2.0));
    settle(&mut shell);
    tap(&mut shell, ControlId::Edit(EditId::Address));
    assert_eq!(shell.asks(), [Ask::FocusEdit(EditId::Address)]);
    tap(&mut shell, ControlId::AddAddress);
    assert_eq!(shell.asks()[1], Ask::SubmitEdit { edit: EditId::Address, clear: false });
    let bytes = shell.take_asks();
    assert_eq!(bytes, vec![2, ask::FOCUS_EDIT, 4, ask::SUBMIT_EDIT, 4, 0]);

    // Half an address is not sent; a whole one is.
    shell.submit_edit(EditId::Address, "192.168.0".into());
    assert!(backend.sent.lock().unwrap().is_empty());
    shell.submit_edit(EditId::Address, " 192.168.0.50 ".into());
    assert_eq!(*backend.sent.lock().unwrap(), vec![Command::AddByAddress { ipv4: "192.168.0.50".into() }]);

    for edit in [EditId::Name, EditId::Chat, EditId::Rename, EditId::Address, EditId::ExpRename] {
        assert_eq!(edit_of(edit_code(edit)), Some(edit));
    }
    assert_eq!((edit_of(0), edit_of(99)), (None, None));
}

#[test]
fn back_drops_a_name_being_typed_and_answers_only_the_questions_that_have_a_way_out() {
    // Renaming: back is "Cancelar".
    let (mut shell, _) = shell_of(Scenario::Dispositivos.prepared_state());
    tap(&mut shell, ControlId::RenameDevice);
    assert!(shell.state().renaming().is_some());
    assert!(shell.back() && shell.state().renaming().is_none());
    assert_eq!(shell.state().nav(), Nav::Devices, "the page stays");

    // Comparing codes takes a chosen answer: back stays on it, and the app does not leave either.
    let (mut shell, backend) = shell_of(Scenario::Pareamento.prepared_state());
    assert!(shell.takes_back() && shell.back());
    assert!(shell.state().pairing().is_some() && backend.sent.lock().unwrap().is_empty());

    // A request to connect too.
    let (mut shell, backend) = shell_of(Scenario::PedidoConexao.prepared_state());
    assert!(shell.back() && shell.state().incoming_session_open());
    assert!(backend.sent.lock().unwrap().is_empty());

    // A dialog with a lesser button: back is that button.
    let (mut shell, _) = shell_of(Scenario::Consentimento.prepared_state());
    let before = shell.state().dialog(swapdesk_ui::WindowKind::Main).is_some();
    assert!(before && shell.takes_back() && shell.back());
    settle(&mut shell);
    assert!(shell.state().dialog(swapdesk_ui::WindowKind::Main).is_none(), "the dialog went with its lesser answer");
}

#[test]
fn what_the_phone_does_not_do_yet_is_said_not_swallowed() {
    let (mut shell, _) = shell_of(Scenario::Atividade.prepared_state());
    tap(&mut shell, ControlId::Export);
    let [Ask::Note(text)] = shell.asks() else { panic!("{:?}", shell.asks()) };
    assert!(text.starts_with("not built yet: A7"), "{text}");
    let bytes = shell.take_asks();
    let mut r = Reader(&bytes);
    assert_eq!((r.u8().unwrap(), r.u8().unwrap()), (1, ask::NOTE));
    assert!(r.str().unwrap().contains("A7") && r.0.is_empty());
}
