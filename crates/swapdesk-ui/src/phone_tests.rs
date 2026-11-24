//! A phone's state: what it says differently and what it leaves out (docs/ANDROID.md 2 and 5). A phone dials and is
//! never dialled, so nothing of hosting is among its controls or its texts.

use crate::mock::{PHONE_NAME, Scenario};
use crate::{
    Command, ControlId, DeviceTab, EditId, Effect, LocalNetwork, MsgKey, NetworkStatus, NoticeKind, Os, Permission,
    Ready, SelectId, UiEvent, UiState, WindowKind,
};

fn ids(state: &UiState) -> Vec<ControlId> {
    state.controls(WindowKind::Main).into_iter().map(|c| c.id).collect()
}

fn says(state: &UiState, text: &str) -> bool {
    state.controls(WindowKind::Main).iter().any(|c| c.label.contains(text))
}

#[test]
fn a_state_is_a_computers_until_it_is_told_it_is_a_phones() {
    let computer = Scenario::Inicio.prepared_state();
    assert!(!computer.on_phone() && computer.grants_shown());
    assert_eq!(computer.t(MsgKey::SidebarThisComputer), "Este computador");
    let phone = Scenario::Inicio.phone_state();
    assert!(phone.on_phone() && !phone.grants_shown());
    assert_eq!(phone.t(MsgKey::SidebarThisComputer), "Este celular");
    assert_eq!(phone.this_device().map(|t| (t.name.as_str(), t.os)), Some((PHONE_NAME, Os::Android)));
    // A text a phone says the same way.
    assert_eq!(phone.t(MsgKey::Control), computer.t(MsgKey::Control));
}

#[test]
fn the_first_pane_of_a_phone_opens_no_pairing_of_its_own_and_says_who_it_is() {
    let computer = Scenario::Inicio.prepared_state();
    let phone = Scenario::Inicio.phone_state();
    assert!(ids(&computer).contains(&ControlId::PairThisComputer));
    let on_phone = ids(&phone);
    assert!(!on_phone.contains(&ControlId::PairThisComputer), "a phone never waits to be paired with");
    // It still adds a computer by its address, and still lists the computers.
    assert!(on_phone.contains(&ControlId::Edit(EditId::Address)) && on_phone.contains(&ControlId::AddAddress));
    assert!(on_phone.contains(&ControlId::Device(0)));

    let home = phone.home();
    assert_eq!((home.name.as_deref(), home.os), (Some(PHONE_NAME), Some(Os::Android)));
    assert_eq!(home.status.as_deref(), Some("Pronto para controlar"));
    assert!(says(&phone, "toque nele em Na rede") && !says(&phone, "Os dois computadores"));
    assert_eq!(computer.home().status.as_deref(), Some("Pronto para receber"));
}

#[test]
fn a_computers_page_on_a_phone_has_no_grants_for_that_computer() {
    let mut phone = Scenario::Dispositivos.phone_state();
    let overview = ids(&phone);
    assert!(overview.contains(&ControlId::Control) && overview.contains(&ControlId::ViewOnly));
    assert!(!overview.contains(&ControlId::EditGrants), "nothing to edit: nothing connects to a phone");
    assert!(says(&phone, "É este celular que inicia a conexão."));

    let _ = phone.select_tab(DeviceTab::Permissions);
    let tab = ids(&phone);
    assert!(!tab.iter().any(|id| matches!(id, ControlId::Grant(_))), "{tab:?}");
    assert!(phone.permission_rows().is_empty());
    // What the computer lets the phone do is still there, with the way to forget it.
    assert!(tab.contains(&ControlId::Forget) && says(&phone, "Este celular pode, no MacBook do escritório"));

    let mut computer = Scenario::Dispositivos.prepared_state();
    assert!(ids(&computer).contains(&ControlId::EditGrants));
    let _ = computer.select_tab(DeviceTab::Permissions);
    assert!(ids(&computer).contains(&ControlId::Grant(Permission::View)));
}

#[test]
fn ajustes_on_a_phone_is_its_name_its_language_its_theme_and_the_picture() {
    let phone = Scenario::Ajustes.phone_state();
    let settings: Vec<ControlId> = ids(&phone).into_iter().filter(|id| !matches!(id, ControlId::Static(_))).collect();
    let own: Vec<ControlId> = settings
        .iter()
        .copied()
        .filter(|id| !matches!(id, ControlId::NavHome | ControlId::NavSettings | ControlId::Device(_)))
        .collect();
    assert_eq!(
        own,
        vec![
            ControlId::Edit(EditId::Name),
            ControlId::Select(SelectId::Language),
            ControlId::Select(SelectId::Theme),
            ControlId::Select(SelectId::Quality),
            ControlId::ShowStats,
        ]
    );
    assert!(
        says(&phone, "Os computadores pareados veem este nome.") || {
            let name = phone.controls(WindowKind::Main).into_iter().find(|c| c.id == ControlId::Edit(EditId::Name));
            name.and_then(|c| c.description).as_deref() == Some("Os computadores pareados veem este nome.")
        }
    );
    // A computer keeps all of its own.
    let computer = ids(&Scenario::Ajustes.prepared_state());
    for id in [ControlId::OpenAtLogin, ControlId::Receive, ControlId::LockOnEnd, ControlId::SwapCtrlCmd] {
        assert!(computer.contains(&id) && !settings.contains(&id), "{id:?}");
    }
}

/// A device that started with no network says so; when a network comes, or goes, the service is asked again what
/// this device is ready for. Found on the phone (24/11/2026): Android reports its networks after the app's core has
/// started, and the first pane kept "Sem rede" over the chip of the network it was on.
#[test]
fn a_network_that_comes_or_goes_asks_again_what_this_device_is_ready_for() {
    let network = |address: Option<&str>| {
        UiEvent::NetworkChanged(NetworkStatus {
            label: "wlan0".into(),
            address: address.map(str::to_owned),
            category: None,
            notices: Vec::new(),
        })
    };
    let mut s = Scenario::Inicio.phone_state();
    let _ = s.take_followups();
    s.this.as_mut().expect("this device").ready = Ready::NoNetwork;
    let _ = s.apply(network(Some("10.0.2.16")));
    assert_eq!(s.take_followups(), vec![Command::GetThisDevice], "a network came");

    // The service answered: ready. The same network told again asks nothing.
    s.this.as_mut().expect("this device").ready = Ready::ReadyToReceive;
    let _ = s.apply(network(Some("10.0.2.16")));
    assert!(s.take_followups().is_empty());
    let _ = s.apply(network(None));
    assert_eq!(s.take_followups(), vec![Command::GetThisDevice], "the network went");

    // Before the service said anything of this device, its first answer is still on its way: nothing more is asked.
    let mut fresh = UiState::default();
    let _ = fresh.apply(network(Some("10.0.2.16")));
    assert!(fresh.take_followups().is_empty());
}

/// F-AND-01: from Android 17 the local network is a permission. While the system's question is on the screen the
/// first pane says why it is asked and offers nothing; refused, it says what stops working and offers the app's
/// page in the system's settings; allowed, it says nothing. The notice stands before every other.
#[test]
fn a_phone_without_the_local_network_says_so_first_and_offers_the_systems_settings() {
    let mut phone = Scenario::Inicio.phone_state();
    assert_eq!(phone.local_network(), LocalNetwork::Allowed);
    assert!(!phone.notices().contains(&NoticeKind::LocalNetwork));
    assert!(phone.set_local_network(LocalNetwork::Allowed).is_none(), "told again, nothing changes");

    assert!(!phone.set_local_network(LocalNetwork::Asking).is_none());
    assert_eq!(phone.notices().first(), Some(&NoticeKind::LocalNetwork));
    let (title, body, button) = phone.notice_texts(NoticeKind::LocalNetwork);
    assert_eq!(title, "O Swapdesk funciona só na rede local");
    assert!(body.contains("o Android pede a sua permissão") && button.is_empty());
    assert!(!ids(&phone).contains(&ControlId::Notice(NoticeKind::LocalNetwork)), "a question of the system, no button");
    // The phone is ready for nothing meanwhile, and says so where it said it was ready.
    assert_eq!(
        (phone.home().status.as_deref(), phone.home().ready),
        (Some("Esperando a permissão de rede local"), false)
    );
    assert!(says(&phone, "O Swapdesk funciona só na rede local"));

    let _ = phone.set_local_network(LocalNetwork::Refused);
    let (title, body, button) = phone.notice_texts(NoticeKind::LocalNetwork);
    assert_eq!((title.as_str(), button.as_str()), ("Rede local não permitida", "Abrir os ajustes do app"));
    assert!(body.contains("não encontra nem alcança nenhum computador"));
    assert!(ids(&phone).contains(&ControlId::Notice(NoticeKind::LocalNetwork)));
    assert_eq!((phone.home().status.as_deref(), phone.home().ready), (Some("Sem acesso à rede local"), false));
    // Before the first-run guide and everything else of the pane.
    let order: Vec<String> = phone.controls(WindowKind::Main).into_iter().map(|c| c.label).collect();
    let at = |text: &str| order.iter().position(|l| l == text).unwrap_or_else(|| panic!("{text} is not said"));
    assert!(at("Rede local não permitida") < at("Parear"), "{order:?}");
    let act = phone.activate(ControlId::Notice(NoticeKind::LocalNetwork));
    assert_eq!(act.effect, Some(Effect::OpenWebPage));
    let page = phone.take_web_page().expect("a page of the system");
    assert_eq!(page, crate::state::ANDROID_APP_SETTINGS);
    assert!(crate::state::openable_page(&page));
    // The notice stays until Android says the answer changed.
    assert_eq!(phone.notices().first(), Some(&NoticeKind::LocalNetwork));
    let _ = phone.set_local_network(LocalNetwork::Allowed);
    assert!(!phone.notices().contains(&NoticeKind::LocalNetwork));
    assert_eq!((phone.home().status.as_deref(), phone.home().ready), (Some("Pronto para controlar"), true));

    // On a computer's page too: nothing there works without the network.
    let mut page = Scenario::Dispositivos.phone_state();
    let _ = page.set_local_network(LocalNetwork::Refused);
    let on_page = ids(&page);
    assert!(on_page.contains(&ControlId::Notice(NoticeKind::LocalNetwork)));
    assert!(!on_page.contains(&ControlId::EditGrants), "the notice takes the page's content");
}

/// docs/ANDROID.md 5: a computer whose Swapdesk is older than a phone can talk to is listed where it is, says
/// "needs an update" in place of its status, and its page offers nothing that would dial it: what to do about it
/// instead. An activation that still arrives sends nothing.
#[test]
fn a_computer_that_needs_an_update_says_so_and_offers_nothing_that_dials_it() {
    let mut phone = crate::mock::phone_needs_update_state();
    let d = phone.selected_device().cloned().expect("the computer's page");
    assert!(d.needs_update && !d.paired);
    assert_eq!(phone.device_status_line(&d), "Precisa de atualização");
    assert_eq!(crate::format::detail_line(phone.language(), &d), "O Swapdesk deste computador precisa de atualização");
    let on_page = ids(&phone);
    for dial in [ControlId::PairWithDevice, ControlId::Control, ControlId::ViewOnly, ControlId::Wake] {
        assert!(!on_page.contains(&dial), "{dial:?} on the page of a computer that needs an update");
    }
    assert!(says(&phone, "Este computador precisa de atualização"));
    assert!(says(&phone, "Atualize o Swapdesk no computador"));
    assert!(!says(&phone, "ainda não está pareado"), "not the text of a computer that can be paired");
    for dial in [ControlId::PairWithDevice, ControlId::Control, ControlId::ViewOnly] {
        let act = phone.activate(dial);
        assert!(act.commands.is_empty(), "{dial:?} sent {:?}", act.commands);
    }
    assert!(phone.pairing().is_none());

    // The same computer once paired (it went back to an older build): no connect either, and the page says why.
    let mut paired = crate::mock::phone_needs_update_state();
    for d in paired.devices.iter_mut().filter(|d| d.needs_update) {
        d.paired = true;
        d.status = crate::DeviceStatus::Online { address: "192.168.0.30".into(), rtt_ms: 2 };
    }
    let on_page = ids(&paired);
    assert!(!on_page.contains(&ControlId::Control) && !on_page.contains(&ControlId::ViewOnly), "{on_page:?}");
    assert!(says(&paired, "O Swapdesk deste computador precisa de atualização"));
    assert!(paired.activate(ControlId::Control).commands.is_empty());

    // A computer that does not need one keeps its button.
    let fine = Scenario::EstadoNaoPareado.phone_state();
    assert!(ids(&fine).contains(&ControlId::PairWithDevice));
}

/// A pairing by a typed address that ended at a computer too old to read the phone: the dialog names the cause
/// and what to do, and offers only its way out (trying again changes nothing until the computer is updated).
#[test]
fn a_pairing_that_met_an_old_computer_says_it_needs_an_update_and_only_closes() {
    let phone = crate::mock::phone_pairing_needs_update_state();
    let on_page = ids(&phone);
    assert!(on_page.contains(&ControlId::PairClose) && !on_page.contains(&ControlId::PairRetry), "{on_page:?}");
    assert!(says(&phone, "Este computador precisa de atualização"));
    assert!(says(&phone, "Nada foi liberado.") && says(&phone, "pareie de novo"));
    assert!(!says(&phone, "Não foi possível parear") && !says(&phone, "Erro interno"));
    // Any other failure keeps its two buttons.
    let mut other = crate::mock::phone_pairing_needs_update_state();
    if let Some(p) = &mut other.pairing {
        p.state = crate::PairingState::Failed { reason: crate::ApiCode::Timeout };
    }
    let on_page = ids(&other);
    assert!(on_page.contains(&ControlId::PairRetry) && on_page.contains(&ControlId::PairClose));
    // In English too.
    let mut en = crate::mock::phone_pairing_needs_update_state();
    en.set_language(crate::Language::En);
    assert!(says(&en, "This computer needs an update") && says(&en, "pair again"));
}

/// docs/ANDROID.md 5 (a lost phone): whoever holds a paired phone opens the computers it paired with, so a phone
/// without a secure screen lock starts no pairing. "Parear" opens the dialog that says what to set and leads to the
/// system's security settings; nothing is sent. With a lock, the same tap starts the pairing. A computer's state
/// never reads the flag.
#[test]
fn a_phone_without_a_screen_lock_starts_no_pairing_and_says_what_to_set() {
    let mut phone = Scenario::EstadoNaoPareado.phone_state();
    assert!(phone.device_secure(), "secure until the shell says otherwise: a mock and a computer pair");
    let _ = phone.set_device_secure(false);
    let act = phone.activate(ControlId::PairWithDevice);
    assert!(act.commands.is_empty(), "{:?}", act.commands);
    let lock = ControlId::Notice(NoticeKind::ScreenLock);
    assert!(matches!(
        phone.pairing().map(|p| &p.state),
        Some(crate::PairingState::Failed { reason: crate::ApiCode::NeedsScreenLock })
    ));
    let on_page = ids(&phone);
    assert!(on_page.contains(&lock) && on_page.contains(&ControlId::PairClose), "{on_page:?}");
    assert!(!on_page.contains(&ControlId::PairRetry), "trying again without a lock changes nothing");
    assert!(says(&phone, "Defina um bloqueio de tela para parear"));
    assert!(says(&phone, "só você faz isso") && says(&phone, "Abrir os ajustes de segurança"));
    assert!(!says(&phone, "Não foi possível parear"));

    // The button leads to the system's security settings and closes the dialog.
    let act = phone.activate(lock);
    assert_eq!(act.effect, Some(Effect::OpenWebPage));
    assert_eq!(phone.take_web_page().as_deref(), Some(crate::state::ANDROID_SECURITY_SETTINGS));
    assert!(crate::state::openable_page(crate::state::ANDROID_SECURITY_SETTINGS));
    assert!(phone.pairing().is_none());

    // Back with a lock set: "Parear" starts the pairing.
    let _ = phone.set_device_secure(true);
    let act = phone.activate(ControlId::PairWithDevice);
    assert!(matches!(act.commands.as_slice(), [Command::PairOpen { device_id: Some(_) }]), "{:?}", act.commands);

    // "Tentar de novo" on a pairing that failed goes through the same start.
    let mut again = Scenario::EstadoNaoPareado.phone_state();
    let _ = again.activate(ControlId::PairWithDevice);
    if let Some(p) = &mut again.pairing {
        p.state = crate::PairingState::Failed { reason: crate::ApiCode::Timeout };
    }
    let _ = again.set_device_secure(false);
    let act = again.activate(ControlId::PairRetry);
    assert!(act.commands.is_empty(), "{:?}", act.commands);
    assert!(ids(&again).contains(&lock));

    // What the service refuses by itself reads the same on the screen (a reply that says so).
    assert_eq!(
        phone.error_text(&crate::ApiError::new(crate::ApiCode::NeedsScreenLock)),
        "Defina um bloqueio de tela neste celular para parear."
    );

    // A computer pairs, whatever the flag says.
    let mut computer = Scenario::EstadoNaoPareado.prepared_state();
    let _ = computer.set_device_secure(false);
    let act = computer.activate(ControlId::PairWithDevice);
    assert!(matches!(act.commands.as_slice(), [Command::PairOpen { .. }]));
}

/// A phone whose core did not start shows one card and nothing else of the app: no pane, no navigation. Data that
/// does not open names no cause, says nothing was erased and what clearing costs, and leads to the system's page
/// of the app; a start that failed this time has no button.
#[test]
fn a_stopped_phone_shows_one_card_and_nothing_else() {
    use crate::Stopped;
    let mut phone = crate::mock::phone_stopped_state(Stopped::DataUnreadable);
    assert_eq!(phone.screen(), crate::Screen::Stopped);
    assert_eq!(phone.notices(), vec![NoticeKind::DataUnreadable]);
    let button = ControlId::Notice(NoticeKind::DataUnreadable);
    assert_eq!(ids(&phone).into_iter().filter(|c| c.focusable()).collect::<Vec<_>>(), vec![button]);
    assert!(says(&phone, "Os dados do Swapdesk neste celular não abrem mais"));
    assert!(says(&phone, "Nada foi apagado.") && says(&phone, "apaga todos os pareamentos deste celular"));
    assert!(says(&phone, "em cada computador, esqueça este celular"));
    let act = phone.activate(button);
    assert_eq!(act.effect, Some(Effect::OpenWebPage));
    assert_eq!(phone.take_web_page().as_deref(), Some(crate::state::ANDROID_APP_SETTINGS));
    assert!(act.commands.is_empty(), "the app erases nothing and has no service to ask");
    assert_eq!(phone.screen(), crate::Screen::Stopped, "the card stays");

    let later = crate::mock::phone_stopped_state(Stopped::NotStarted);
    assert!(ids(&later).iter().all(|c| !c.focusable()), "nothing to press");
    assert!(says(&later, "O Swapdesk não conseguiu iniciar agora") && says(&later, "Nada foi apagado."));

    let mut en = crate::mock::phone_stopped_state(Stopped::DataUnreadable);
    en.set_language(crate::Language::En);
    assert!(says(&en, "no longer opens") && says(&en, "Nothing was erased."));
    // Taken away, the app's own screens are back.
    let _ = en.set_stopped(None);
    assert_eq!(en.screen(), crate::Screen::Home);
}
