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
