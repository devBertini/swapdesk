//! What the shell takes from the state besides the frame, against the scenarios of the mock backend. Pure: runs on
//! every platform, no WindowServer.

use swapdesk_ui::mock::Scenario;
use swapdesk_ui::{ControlId, Effect, Permissions, Role, SelectId, SessionState, Theme, UiEvent, UiState, WindowKind};

use crate::effects::{Handling, handling};
use crate::model::*;

fn shell_state(sc: Scenario) -> UiState {
    sc.prepared_state()
}

#[test]
fn the_theme_setting_decides_the_appearance_to_apply() {
    let mut s = Scenario::Ajustes.prepared_state();
    assert_eq!(appearance_name(s.theme()), None, "Sistema clears the override");
    assert!(!s.dark());
    s.choose(SelectId::Theme, 2);
    assert_eq!(appearance_name(s.theme()), Some("NSAppearanceNameDarkAqua"));
    assert!(s.dark(), "Escuro paints dark on a light OS");
    s.apply(UiEvent::ThemeChanged { dark: true });
    s.choose(SelectId::Theme, 1);
    assert_eq!(appearance_name(s.theme()), Some("NSAppearanceNameAqua"));
    assert!(!s.dark(), "Claro paints light on a dark OS");
    s.choose(SelectId::Theme, 0);
    assert_eq!(s.theme(), Theme::System);
    assert!(s.dark(), "Sistema follows the OS again");
}

#[test]
fn a_viewer_window_exists_exactly_while_a_viewer_session_does_and_is_titled_after_the_peer() {
    assert_eq!(viewer_title(&shell_state(Scenario::Inicio)), None, "no session, no viewer window");
    let mut state = shell_state(Scenario::Visualizador);
    let session = state.viewer_session().expect("the scenario has a viewer session").clone();
    let peer = state.device_by_id(&session.device_id).expect("the peer").name.clone();
    let title = viewer_title(&state).expect("a viewer window");
    assert!(title.contains(&peer), "{title}");
    state.apply(UiEvent::Session {
        session_id: session.id,
        device_id: session.device_id,
        state: SessionState::Ended { reason: None },
    });
    assert_eq!(viewer_title(&state), None, "the window goes with the session");
}

#[test]
fn the_swap_tool_sits_right_before_disconnect_toward_a_machine_of_this_os() {
    use swapdesk_ui::Permission;
    let mut state = shell_state(Scenario::Visualizador);
    state.apply(UiEvent::Session {
        session_id: 1,
        device_id: swapdesk_ui::mock::DESKTOP.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::View),
        },
    });
    let ids: Vec<ControlId> = state.controls(WindowKind::Viewer).iter().map(|c| c.id).collect();
    let (swap, disconnect) = (
        ids.iter().position(|i| *i == ControlId::TbSwap).expect("swap toward the same OS"),
        ids.iter().position(|i| *i == ControlId::TbDisconnect).expect("disconnect"),
    );
    assert_eq!(swap + 1, disconnect, "swap sits right before disconnect");
}

#[test]
fn the_host_banner_says_who_and_since_when_and_goes_with_the_session() {
    // Defect after 1.0: the Mac host showed nothing while the PC controlled it.
    assert_eq!(banner_model(&shell_state(Scenario::Inicio)), None, "no session, no banner");
    let mut state = shell_state(Scenario::Banner);
    let h = state.host_session().expect("the scenario hosts a session").clone();
    let m = banner_model(&state).expect("banner");
    let peer = state.device_by_id(&h.device_id).unwrap().name.clone();
    assert!(m.who.contains(&peer) && m.who.contains("controla"), "{}", m.who);
    assert!(m.since.contains("12 min"), "{}", m.since);
    assert_eq!(m.label(), format!("{} {}", m.who, m.since));
    assert_eq!(m.title, swapdesk_ui::a11y::window_title(&state, WindowKind::Banner));
    // How to end it is a control of the banner's frame.
    let controls = state.controls(WindowKind::Banner);
    assert!(controls.iter().any(|c| c.label == "Desconectar"), "the banner has Desconectar");

    state.apply(UiEvent::Session {
        session_id: h.id,
        device_id: h.device_id,
        state: SessionState::Ended { reason: None },
    });
    assert_eq!(banner_model(&state), None, "the banner goes with the session");
}

/// One effect of every kind the state can ask the shell for. The match has no catch-all: a new `Effect` variant does
/// not build until it is listed here, and then [`every_effect_has_a_handler_of_its_own`] asks for its handler.
fn every_effect() -> Vec<Effect> {
    use swapdesk_ui::EditId;
    let all = vec![
        Effect::Menu(SelectId::Theme),
        Effect::FocusEdit(EditId::Address),
        Effect::SubmitEdit(EditId::Rename),
        Effect::SaveLog,
        Effect::OpenLogs,
        Effect::PickFiles,
        Effect::PickReceiveFolder,
    ];
    for e in &all {
        match e {
            Effect::Menu(_)
            | Effect::FocusEdit(_)
            | Effect::SubmitEdit(_)
            | Effect::SaveLog
            | Effect::OpenLogs
            | Effect::PickFiles
            | Effect::PickReceiveFolder => {}
        }
    }
    all
}

#[test]
fn every_effect_has_a_handler_of_its_own() {
    let effects = every_effect();
    let handlers: Vec<Handling> = effects.iter().map(|e| handling(e.clone())).collect();
    for (i, h) in handlers.iter().enumerate() {
        assert!(
            !handlers[..i].contains(h),
            "{:?} shares the handler {h:?} with another effect: it has none of its own",
            effects[i]
        );
    }
    // What each one does on the Mac, as on Windows.
    assert_eq!(handling(Effect::PickFiles), Handling::ChooseFiles);
    assert_eq!(handling(Effect::PickReceiveFolder), Handling::ChooseFolder);
    assert_eq!(handling(Effect::OpenLogs), Handling::OpenLogsFolder);
    assert_eq!(handling(Effect::SaveLog), Handling::SaveLogPanel);
    assert_eq!(handling(Effect::Menu(SelectId::Theme)), Handling::Menu(SelectId::Theme));
}
