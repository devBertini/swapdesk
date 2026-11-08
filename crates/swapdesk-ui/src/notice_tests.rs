//! The crash notice (D12) and the "no computer found" card (F20).

use crate::api::*;
use crate::interact::ControlId;
use crate::mock::Scenario;
use crate::state::*;

fn reply_ok(r: Reply) -> UiEvent {
    UiEvent::Reply { ticket: Ticket(0), result: Ok(r) }
}

#[test]
fn a_crash_marker_shows_the_notice_first_and_once() {
    let mut s = Scenario::EstadoRedePublica.prepared_state();
    assert!(!s.crash_notice());
    s.show_crash_notice();
    assert_eq!(s.notices(), vec![NoticeKind::Crash, NoticeKind::Public], "the crash notice comes first");
    let (title, body, button) = s.notice_texts(NoticeKind::Crash);
    assert_eq!((title.as_str(), button.as_str()), ("O Swapdesk fechou inesperadamente", "Abrir a pasta de logs"));
    assert!(body.starts_with("O registro do que aconteceu"));
    let a = s.activate(ControlId::Notice(NoticeKind::Crash));
    assert_eq!(a.effect, Some(Effect::OpenLogs));
    assert!(a.commands.is_empty(), "the OS call is the shell's, not a backend command");
    assert_eq!(s.notices(), vec![NoticeKind::Public], "used once, gone");
    // Picking another section also ends it.
    let mut s = Scenario::Dispositivos.prepared_state();
    s.show_crash_notice();
    s.navigate(Nav::Activity);
    assert!(s.notices().is_empty());
    let mut en = Scenario::EstadoFechouInesperado.prepared_state();
    assert_eq!(en.notices(), vec![NoticeKind::Crash]);
    en.lang = Language::En;
    assert_eq!(en.notice_texts(NoticeKind::Crash).0, "Swapdesk closed unexpectedly");
}

#[test]
fn the_crash_notice_shows_with_an_empty_device_list_too() {
    let mut s = Scenario::EstadoNenhumComputador.prepared_state();
    s.show_crash_notice();
    let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
    assert!(ids.contains(&ControlId::Notice(NoticeKind::Crash)) && ids.contains(&ControlId::NoComputerAddress));
}

fn empty_answered() -> UiState {
    let mut s = UiState::default();
    s.apply(reply_ok(Reply::Devices(Vec::new())));
    s
}

fn main_texts(s: &UiState) -> Vec<String> {
    s.controls(WindowKind::Main).into_iter().map(|c| c.label).collect()
}

#[test]
fn an_empty_list_shows_the_plain_text_until_discovery_has_run_for_10_seconds() {
    let mut s = empty_answered();
    assert!(!s.no_computer_card());
    for _ in 0..9 {
        s.tick();
    }
    assert!(!s.no_computer_card(), "9 s: still the plain text");
    assert!(main_texts(&s).contains(&"Nenhum computador encontrado ainda.".to_string()));
    assert!(!s.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::NoComputerAddress));
    let changed = s.tick();
    assert!(s.no_computer_card(), "10 s: the card");
    assert_eq!(changed, Changed::MAIN, "the screen is redrawn when the card appears");
    let texts = main_texts(&s);
    assert!(texts.contains(&"Nenhum computador encontrado".to_string()));
    assert!(texts.iter().any(|t| t.starts_with("Confira no outro computador: o Swapdesk está aberto")));
    assert!(!texts.contains(&"Nenhum computador encontrado ainda.".to_string()), "the card replaces the plain text");
}

#[test]
fn the_card_waits_for_the_first_answer_and_a_device_removes_it() {
    let mut s = UiState::default();
    for _ in 0..20 {
        s.tick();
    }
    assert!(!s.no_computer_card(), "nothing answered yet: discovery has not run");
    s.apply(reply_ok(Reply::Devices(Vec::new())));
    for _ in 0..10 {
        s.tick();
    }
    assert!(s.no_computer_card());
    s.apply(UiEvent::DevicesChanged(Scenario::Dispositivos.prepared_state().devices().to_vec()));
    assert!(!s.no_computer_card(), "a device appeared");
    s.apply(UiEvent::DevicesChanged(Vec::new()));
    assert!(!s.no_computer_card(), "the count starts over when the list empties again");
    for _ in 0..10 {
        s.tick();
    }
    assert!(s.no_computer_card());
}

#[test]
fn the_card_action_focuses_the_address_field() {
    let mut s = Scenario::EstadoNenhumComputador.prepared_state();
    assert!(s.no_computer_card());
    let a = s.activate(ControlId::NoComputerAddress);
    assert_eq!(a.effect, Some(Effect::FocusEdit(EditId::Address)));
    assert!(a.commands.is_empty());
}

#[test]
fn the_new_texts_are_exact_in_both_languages() {
    use crate::strings::{MsgKey, text};
    let t = |k: MsgKey, l| text(l, &Msg::new(k));
    assert_eq!(t(MsgKey::NoticeCrashTitle, Language::PtBr), "O Swapdesk fechou inesperadamente");
    assert_eq!(
        t(MsgKey::NoticeCrashBody, Language::PtBr),
        "O registro do que aconteceu ficou na pasta de logs. Para relatar o problema, anexe esse arquivo."
    );
    assert_eq!(t(MsgKey::NoticeCrashButton, Language::PtBr), "Abrir a pasta de logs");
    assert_eq!(t(MsgKey::NoticeCrashTitle, Language::En), "Swapdesk closed unexpectedly");
    assert_eq!(
        t(MsgKey::NoticeCrashBody, Language::En),
        "A record of what happened is in the logs folder. To report the problem, attach that file."
    );
    assert_eq!(t(MsgKey::NoticeCrashButton, Language::En), "Open logs folder");
    assert_eq!(t(MsgKey::NoComputerTitle, Language::PtBr), "Nenhum computador encontrado");
    assert_eq!(t(MsgKey::NoComputerButton, Language::PtBr), "Adicionar por endereço");
    assert!(t(MsgKey::NoComputerBody, Language::PtBr).ends_with("ou os dois estão em redes diferentes."));
    assert_eq!(t(MsgKey::NoComputerTitle, Language::En), "No computer found");
    assert_eq!(t(MsgKey::NoComputerButton, Language::En), "Add by address");
    assert!(t(MsgKey::NoComputerBody, Language::En).ends_with("or the two are on different networks."));
}
