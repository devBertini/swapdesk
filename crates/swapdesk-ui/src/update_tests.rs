//! UPDATES.md 14 U7: the update card of Início, the Ajustes section and their commands (D58 texts).

use crate::api::*;
use crate::interact::{ControlId, Region, UiRole};
use crate::mock::{Scenario, update_idle, update_with};
use crate::state::*;
use crate::update_card::{UpdAction, UpdIcon};

fn ids(s: &UiState) -> Vec<ControlId> {
    s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect()
}

fn offered(v: &str) -> UpdateInfo {
    update_with(UpdatePhase::Offered { version: v.into(), manual: false })
}

#[test]
fn the_update_card_comes_first_when_a_version_is_offered() {
    let s = Scenario::AtualizacaoOferecida.prepared_state();
    let card = s.update_card().expect("a version on offer shows its card");
    assert_eq!(card.title, "Nova versão: 1.1.1");
    assert_eq!(card.icon, UpdIcon::New);
    let labels: Vec<&str> = card.buttons.iter().map(|(_, l, _)| l.as_str()).collect();
    // Negative left, positive right; the primary one last.
    assert_eq!(labels, ["Agora não", "O que mudou", "Atualizar agora"]);
    assert!(card.buttons.last().is_some_and(|b| b.2));
    // In the reading order the card's buttons come before the page's own cards.
    let order = ids(&s);
    let install = order.iter().position(|c| *c == ControlId::Update(UpdAction::Install)).unwrap();
    let pair = order.iter().position(|c| *c == ControlId::PairThisComputer).unwrap();
    assert!(install < pair, "the update card comes first");
    // Nothing on offer: no card.
    let s = Scenario::Inicio.prepared_state();
    assert!(s.update_card().is_none());
}

#[test]
fn each_failure_has_its_reason_and_a_way_out() {
    for (failure, text) in [
        (UpdateFailure::Download, "Não foi possível falar com o GitHub"),
        (UpdateFailure::Mismatch, "O arquivo baixado não confere"),
        (UpdateFailure::Disk, "Falta espaço em disco"),
    ] {
        let mut s = Scenario::Inicio.prepared_state();
        s.apply(UiEvent::UpdateChanged(update_with(UpdatePhase::Failed(failure))));
        let card = s.update_card().unwrap_or_else(|| panic!("{failure:?} has a card"));
        assert!(card.title.starts_with(text), "{failure:?}: {}", card.title);
        assert_eq!(card.buttons.iter().map(|b| b.0).collect::<Vec<_>>(), [ControlId::Update(UpdAction::Retry)]);
        let a = s.activate(ControlId::Update(UpdAction::Retry));
        assert_eq!(a.commands, [Command::UpdateCheckNow], "{failure:?}: the way out checks again");
    }
    // On the Mac, the app's location: an administrator's folder offers the release page, a translocated copy asks
    // to be moved.
    let mut s = Scenario::Inicio.prepared_state();
    s.apply(UiEvent::UpdateChanged(update_with(UpdatePhase::Failed(UpdateFailure::Unwritable))));
    let card = s.update_card().unwrap();
    assert!(card.title.starts_with("Só um administrador deste Mac"));
    assert_eq!(card.buttons[0].1, "Abrir a página da versão");
    s.apply(UiEvent::UpdateChanged(update_with(UpdatePhase::Failed(UpdateFailure::Translocated))));
    assert!(s.update_card().unwrap().title.starts_with("Mova o Swapdesk para a pasta Aplicativos"));
    // A rollback says which version did not start and offers the release page and a new try.
    let s = Scenario::AtualizacaoRevertida.prepared_state();
    let card = s.update_card().unwrap();
    assert_eq!(card.title, "A versão 1.1.1 não iniciou. A 1.1.0 foi restaurada e continua funcionando.");
    assert_eq!(card.icon, UpdIcon::Warn);
    // A check whose manifest was not signed says so in Ajustes.
    let s = Scenario::AtualizacaoFalhaAssinatura.prepared_state();
    let lines = s.update_lines().unwrap();
    assert!(lines.iter().any(|l| l.starts_with("O que foi baixado não tem a assinatura")), "{lines:?}");
    // "O que mudou" opens the release page through the shell.
    let mut s = Scenario::AtualizacaoOferecida.prepared_state();
    let a = s.activate(ControlId::Update(UpdAction::Notes));
    assert_eq!(a.effect, Some(Effect::OpenWebPage));
    assert_eq!(s.take_web_page().as_deref(), Some("https://github.com/devBertini/swapdesk/releases/tag/v1.1.1"));
}

#[test]
fn the_install_button_is_disabled_while_an_install_runs() {
    let mut s = Scenario::AtualizacaoBaixando.prepared_state();
    assert!(!ids(&s).contains(&ControlId::Update(UpdAction::Install)), "no install button while downloading");
    // A late click on an install button that is gone sends nothing.
    let a = s.activate(ControlId::Update(UpdAction::Install));
    assert!(a.commands.is_empty());
    for phase in
        [UpdatePhase::Verifying { version: "1.1.1".into() }, UpdatePhase::Installing { version: "1.1.1".into() }]
    {
        s.apply(UiEvent::UpdateChanged(update_with(phase)));
        let card = s.update_card().unwrap();
        assert!(card.buttons.is_empty(), "past the point of no return there is no button");
    }
    // "Procurar agora" waits too.
    assert!(!s.update_check_enabled());
    // A live session first asks for the confirmation, and only its button installs with the sessions ended.
    let mut s = Scenario::AtualizacaoSessaoAtiva.prepared_state();
    let card = s.update_card().unwrap();
    assert!(card.body.unwrap().contains("Há uma sessão ativa com MacBook do escritório"));
    assert_eq!(card.buttons.iter().map(|b| b.1.as_str()).collect::<Vec<_>>(), ["Cancelar", "Encerrar e atualizar"]);
    let a = s.activate(ControlId::Update(UpdAction::Confirm));
    assert_eq!(a.commands, [Command::UpdateInstall { version: "1.1.1".into(), end_sessions: true }]);
}

#[test]
fn not_now_hides_the_card_until_a_newer_version() {
    let mut s = Scenario::AtualizacaoOferecida.prepared_state();
    let a = s.activate(ControlId::Update(UpdAction::Later));
    assert_eq!(a.commands, [Command::UpdateDismiss { version: "1.1.1".into() }]);
    assert!(s.update_card().is_none(), "hidden at once");
    // The service keeps the dismissal: the same version stays hidden after a new view...
    s.apply(UiEvent::UpdateChanged(UpdateInfo { dismissed: Some("1.1.1".into()), ..offered("1.1.1") }));
    assert!(s.update_card().is_none());
    // ... and a newer one shows.
    s.apply(UiEvent::UpdateChanged(UpdateInfo { dismissed: Some("1.1.1".into()), ..offered("1.1.2") }));
    assert_eq!(s.update_card().unwrap().title, "Nova versão: 1.1.2");
}

#[test]
fn the_settings_switch_sends_update_set_check() {
    let mut s = Scenario::Ajustes.prepared_state();
    let specs = s.controls(WindowKind::Main);
    let switch = specs.iter().find(|c| c.id == ControlId::Update(UpdAction::Switch)).expect("the switch is listed");
    assert_eq!((switch.label.as_str(), switch.toggled), ("Procurar novas versões no GitHub", Some(true)));
    assert!(specs.iter().any(|c| c.id == ControlId::Update(UpdAction::CheckNow)));
    let a = s.activate(ControlId::Update(UpdAction::Switch));
    assert_eq!(a.commands, [Command::UpdateSetCheck { on: false }]);
    assert_eq!(s.update_info().map(|u| u.check_on), Some(false), "the switch moves at once");
    let a = s.activate(ControlId::Update(UpdAction::CheckNow));
    assert_eq!(a.commands, [Command::UpdateCheckNow]);
    // A service without an updater (a development instance) answers NotFound: no section, no error strip.
    let mut s = UiState::default();
    s.track(Ticket(7), &Command::GetUpdate);
    s.apply(UiEvent::Reply { ticket: Ticket(7), result: Err(ApiError::new(ApiCode::NotFound)) });
    assert!(s.update_info().is_none() && s.error().is_none());
    let _ = update_idle();
}

#[test]
fn the_wait_states_are_a_polite_live_region() {
    let s = Scenario::AtualizacaoBaixando.prepared_state();
    let specs = s.controls(WindowKind::Main);
    let title = specs.iter().find(|c| c.label == "Nova versão: 1.1.1").expect("the title is listed");
    assert_eq!(title.region, Region::Status, "announced when the phase changes");
    let bar = specs.iter().find(|c| c.role == UiRole::Progress).expect("the bar is a progress node");
    assert_eq!(bar.region, Region::Content, "the bar is read when focused, never announced each second");
    assert!(bar.label.contains(':'), "the time left: {}", bar.label);
    // A version on offer is not a wait.
    let s = Scenario::AtualizacaoOferecida.prepared_state();
    let specs = s.controls(WindowKind::Main);
    assert!(specs.iter().filter(|c| c.label.starts_with("Nova versão")).all(|c| c.region != Region::Status));
}

#[test]
fn update_restarting_asks_the_shell_to_close_once() {
    let mut s = Scenario::Inicio.prepared_state();
    assert!(!s.take_quit_request());
    s.apply(UiEvent::UpdateRestarting);
    assert!(s.take_quit_request());
    assert!(!s.take_quit_request(), "once");
}
