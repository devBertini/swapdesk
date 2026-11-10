//! The viewer's shortcuts (D30): their names, the release key setting, full screen and the session bar.

use crate::api::*;
use crate::interact::{ControlId, Region};
use crate::mock::Scenario;
use crate::shortcuts::{ReleaseKey, ViewerAction, ViewerOs};
use crate::state::*;

fn viewer() -> UiState {
    Scenario::Visualizador.prepared_state()
}

fn keys_of(s: &UiState) -> Vec<(String, String)> {
    s.shortcut_rows()
}

fn toolbar_controls(s: &UiState) -> usize {
    s.controls(WindowKind::Viewer).iter().filter(|c| c.region == Region::Toolbar).count()
}

#[test]
fn the_windows_list_is_exactly_the_five_chords_of_d30() {
    let s = viewer();
    assert_eq!(
        keys_of(&s),
        vec![
            ("Liberar o teclado".to_owned(), "Ctrl+Alt".to_owned()),
            ("Mostrar a barra da sessão".to_owned(), "Ctrl+Alt+B".to_owned()),
            ("Tela cheia".to_owned(), "F11".to_owned()),
            ("Enviar Ctrl+Alt+Del".to_owned(), "Ctrl+Alt+End".to_owned()),
            ("Desconectar".to_owned(), "Ctrl+Alt+Q".to_owned()),
        ]
    );
}

#[test]
fn the_macos_list_uses_control_option_and_control_command_f() {
    let mut s = viewer();
    s.set_viewer_os(ViewerOs::MacOs);
    let keys: Vec<String> = keys_of(&s).into_iter().map(|(_, k)| k).collect();
    assert_eq!(
        keys,
        ["Control+Option", "Control+Option+B", "Control+Command+F", "Control+Option+End", "Control+Option+Q"]
    );
}

#[test]
fn the_english_names_follow_the_language() {
    let mut s = viewer();
    s.choose(SelectId::Language, 1);
    let rows = keys_of(&s);
    assert_eq!(rows[0], ("Release the keyboard".to_owned(), "Ctrl+Alt".to_owned()));
    assert_eq!(rows[1].0, "Show the session bar");
    assert_eq!(rows[2], ("Full screen".to_owned(), "F11".to_owned()));
    s.choose(SelectId::ReleaseKey, 1);
    assert_eq!(s.shortcut_keys(ViewerAction::ReleaseKeyboard), "Right Ctrl");
}

#[test]
fn each_button_of_the_session_bar_has_a_hint_of_its_own_that_says_what_it_does() {
    // A session toward a machine of this system: its bar has every button, "Inverter" included.
    let mut s = viewer();
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: crate::mock::DESKTOP.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::View),
        },
    });
    let expected = [
        (ControlId::TbScreen, "Escolher qual tela do outro computador aparece"),
        (ControlId::TbFit, "Ajustar a imagem ao tamanho da janela"),
        (ControlId::TbAudio, "Ouvir o som do outro computador"),
        (ControlId::TbGrab, "Enviar os atalhos do sistema ao outro computador"),
        (ControlId::TbKeys, "Enviar Ctrl+Alt+Del e outras teclas"),
        (ControlId::TbTerminal, "Abrir um terminal no outro computador"),
        (ControlId::TbFiles, "Enviar e receber arquivos"),
        (ControlId::TbChat, "Conversar com quem está no outro computador"),
        (ControlId::TbSwap, "Inverter o controle: o outro computador passa a controlar este"),
        (ControlId::TbDisconnect, "Encerrar a sessão (Ctrl+Alt+Q)"),
    ];
    let specs = s.controls(WindowKind::Viewer);
    let bar: Vec<_> = specs.iter().filter(|c| c.region == Region::Toolbar).collect();
    assert_eq!(bar.len(), expected.len(), "every button of the bar is in the table");
    for (id, text) in expected {
        let hint = s.toolbar_hint(id).unwrap_or_else(|| panic!("{id:?} has no hint"));
        assert_eq!(hint, text, "{id:?}");
        // One line about that button: never the shortcut list, which is in the settings.
        assert!(!hint.contains('\n') && !hint.contains("Atalhos"), "{id:?}: {hint:?}");
        // A screen reader hears it as the button's description, where it says more than the button's name.
        let c = bar.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id:?} is not in the bar"));
        assert_eq!(c.description, (hint != c.label).then_some(hint), "{id:?}");
    }
    assert!(s.toolbar_hint(ControlId::DrawerClose).is_none(), "only the bar's buttons have one");
    // The keys in a hint are the viewer's own: a Mac says Control+Option.
    let mut mac = viewer();
    mac.set_viewer_os(ViewerOs::MacOs);
    assert_eq!(mac.toolbar_hint(ControlId::TbDisconnect).as_deref(), Some("Encerrar a sessão (Control+Option+Q)"));
    // And in English.
    let mut en = viewer();
    en.set_language(Language::En);
    assert_eq!(en.toolbar_hint(ControlId::TbAudio).as_deref(), Some("Hear the other computer's sound"));
    assert_eq!(en.toolbar_hint(ControlId::TbDisconnect).as_deref(), Some("End the session (Ctrl+Alt+Q)"));
}

#[test]
fn settings_list_the_shortcuts_and_the_release_key_choice() {
    let mut s = Scenario::Ajustes.prepared_state();
    let texts: Vec<String> = s.controls(WindowKind::Main).into_iter().map(|c| c.label).collect();
    assert!(texts.contains(&"Tecla que libera o teclado".to_owned()), "{texts:?}");
    assert!(texts.contains(&"Tela cheia: F11".to_owned()));
    assert!(texts.contains(&"Mostrar a barra da sessão: Ctrl+Alt+B".to_owned()));
    assert_eq!(s.select_options(SelectId::ReleaseKey).len(), 3, "exactly three chords");
    assert_eq!(s.select_value(SelectId::ReleaseKey), "Ctrl+Alt");
    assert_eq!(s.activate(ControlId::Select(SelectId::ReleaseKey)).effect, Some(Effect::Menu(SelectId::ReleaseKey)));
}

#[test]
fn choosing_the_release_key_sends_one_ui_local_command_and_changes_the_list() {
    let mut s = Scenario::Ajustes.prepared_state();
    for (index, key, name) in [
        (1, ReleaseKey::RightCtrl, "Ctrl direito"),
        (2, ReleaseKey::CtrlAltShift, "Ctrl+Alt+Shift"),
        (0, ReleaseKey::CtrlAlt, "Ctrl+Alt"),
    ] {
        let a = s.choose(SelectId::ReleaseKey, index);
        assert_eq!(a.commands, vec![Command::SetReleaseKey { key }]);
        assert_eq!(s.release_key(), key);
        assert_eq!(s.select_value(SelectId::ReleaseKey), name);
        assert_eq!(s.shortcut_keys(ViewerAction::ReleaseKeyboard), name);
    }
    // An index past the end is the last choice, never a panic.
    s.choose(SelectId::ReleaseKey, 99);
    assert_eq!(s.release_key(), ReleaseKey::CtrlAltShift);
}

#[test]
fn full_screen_hides_the_session_bar_until_ctrl_alt_b_shows_it() {
    let mut s = viewer();
    assert!(!s.fullscreen() && s.session_bar_visible());
    assert!(toolbar_controls(&s) > 0);

    let a = s.run_shortcut(ViewerAction::FullScreen);
    assert!(a.changed.hits(WindowKind::Viewer));
    assert!(s.fullscreen() && !s.session_bar_visible(), "the bar is hidden in full screen");
    assert_eq!(toolbar_controls(&s), 0, "hidden controls are not reachable either");

    s.run_shortcut(ViewerAction::ToggleBar);
    assert!(s.session_bar_visible() && toolbar_controls(&s) > 0);
    s.run_shortcut(ViewerAction::ToggleBar);
    assert!(!s.session_bar_visible());

    // Leaving full screen brings the bar back and forgets the toggle.
    s.run_shortcut(ViewerAction::ToggleBar);
    s.run_shortcut(ViewerAction::FullScreen);
    assert!(!s.fullscreen() && s.session_bar_visible());
    s.run_shortcut(ViewerAction::FullScreen);
    assert!(!s.session_bar_visible(), "the next full screen starts with the bar hidden");
}

#[test]
fn the_bar_toggle_does_nothing_in_a_window() {
    let mut s = viewer();
    s.run_shortcut(ViewerAction::ToggleBar);
    assert!(s.session_bar_visible() && !s.fullscreen());
    s.run_shortcut(ViewerAction::FullScreen);
    assert!(!s.session_bar_visible(), "an earlier toggle in the window did not leave the bar on");
}

#[test]
fn ctrl_alt_end_and_ctrl_alt_q_are_the_toolbar_actions() {
    let mut s = viewer();
    let a = s.run_shortcut(ViewerAction::CtrlAltDel);
    assert!(matches!(a.commands[0], Command::SendKeys { session_id: 1, keys: SpecialKeys::CtrlAltDel }));
    let a = s.run_shortcut(ViewerAction::Disconnect);
    assert!(matches!(a.commands[0], Command::Disconnect { session_id: 1 }));
    assert!(s.run_shortcut(ViewerAction::ReleaseKeyboard).commands.is_empty(), "the keyboard is the pipeline's");
}

#[test]
fn without_a_viewer_session_no_shortcut_acts_and_the_end_of_a_session_leaves_full_screen() {
    let mut s = Scenario::Dispositivos.prepared_state();
    for a in ViewerAction::ALL {
        let act = s.run_shortcut(a);
        assert!(act.commands.is_empty() && act.changed.is_none(), "{a:?}");
    }
    assert!(!s.fullscreen());

    let mut s = viewer();
    s.run_shortcut(ViewerAction::FullScreen);
    s.apply(UiEvent::Session {
        session_id: 1,
        device_id: s.viewer_session().unwrap().device_id.clone(),
        state: SessionState::Ended { reason: None },
    });
    assert!(s.viewer_session().is_none() && !s.fullscreen());
    // A new viewer session does not start in full screen.
    let s = viewer();
    assert!(!s.fullscreen());
}

#[test]
fn the_full_screen_monitor_is_a_choice_among_the_monitors_the_shell_found() {
    let mut s = Scenario::Ajustes.prepared_state();
    assert_eq!(s.select_options(SelectId::FullscreenMonitor), vec![("Onde a janela está".to_owned(), true)]);
    s.set_local_monitors(3);
    let names: Vec<String> = s.select_options(SelectId::FullscreenMonitor).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["Onde a janela está", "Monitor 1", "Monitor 2", "Monitor 3"]);
    s.choose(SelectId::FullscreenMonitor, 2);
    assert_eq!(s.fullscreen_monitor(), Some(1));
    assert_eq!(s.select_value(SelectId::FullscreenMonitor), "Monitor 2");
    s.choose(SelectId::FullscreenMonitor, 9);
    assert_eq!(s.fullscreen_monitor(), None, "a monitor that is not there is the window's own");
    s.choose(SelectId::FullscreenMonitor, 3);
    s.set_local_monitors(2);
    assert_eq!(s.fullscreen_monitor(), None, "a monitor that went away is forgotten");
}
