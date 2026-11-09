//! The view-model mapping against every scenario of the mock backend. Pure: runs on every platform, no WindowServer.

use swapdesk_ui::a11y::build_tree;
use swapdesk_ui::mock::Scenario;
use swapdesk_ui::{
    ApiCode, ApiError, Command, ControlId, DisplayView, EditId, Language, MsgKey, Permissions, Quality, Reply, Role,
    SessionState, Ticket, UiEvent, UiState, WindowKind,
};

use crate::model::*;

fn main_of(state: &UiState) -> MainModel {
    main_model(state)
}

/// A scenario's state as the shell runs it: the viewer toolbar lists the quality select and the view-only switch.
fn shell_state(sc: Scenario) -> UiState {
    let mut state = sc.prepared_state();
    state.set_viewer_extras(true);
    state
}

#[test]
fn every_focusable_control_of_the_main_window_has_a_place_and_the_a11y_name() {
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        let model = main_of(&state);
        let specs = state.controls(WindowKind::Main);
        let tree = build_tree(&state, WindowKind::Main, &|_| None, None);
        let ids = model.control_ids();
        for c in specs.iter().filter(|c| c.id.focusable()) {
            assert!(ids.contains(&c.id), "{}: {:?} ({}) has no place in the model", sc.name(), c.id, c.label);
            let labels = model.control_labels();
            let (_, name) = labels.iter().find(|(i, _)| *i == c.id).expect("label");
            // The AppKit control and swapdesk-ui's accessibility node carry the same name.
            let node = tree.nodes.iter().find(|(n, _)| *n == swapdesk_ui::a11y::node_id(c.id)).expect("a11y node");
            assert_eq!(node.1.label(), Some(name.as_str()), "{}: {:?}", sc.name(), c.id);
            if !matches!(c.id, ControlId::Device(_)) {
                assert_eq!(name, &c.label, "{}: {:?}", sc.name(), c.id);
            }
        }
        // The other way round: nothing offered that the state does not list.
        let known: Vec<ControlId> = specs.iter().map(|c| c.id).collect();
        for id in ids {
            assert!(known.contains(&id), "{}: model offers {id:?} the state does not list", sc.name());
        }
    }
}

#[test]
fn the_devices_screen_has_a_narrow_sidebar_of_navigation_and_lists_and_the_notices_in_the_content() {
    let state = Scenario::Dispositivos.prepared_state();
    let m = main_of(&state);
    let MainLayout::Browse { sidebar, content } = &m.layout else { panic!("browse layout") };
    assert_eq!(sidebar.home.label, "Início");
    assert!(!sidebar.home.selected, "a device page is open");
    assert!(sidebar.groups.iter().map(|g| g.rows.len()).sum::<usize>() >= 2);
    assert_eq!(sidebar.groups[0].title, "Pareados");
    assert_eq!(sidebar.nav.len(), 1, "only Ajustes under the lists");
    assert_eq!(sidebar.nav[0].label, "Ajustes");
    assert!(find_field(content, EditId::Address).is_none(), "the address field lives on the home");
    assert!(!content.is_empty());
    assert!(sidebar.rows_selected_count() <= 1);

    let notice = main_of(&Scenario::EstadoRedePublica.prepared_state());
    let MainLayout::Browse { content, .. } = &notice.layout else { panic!("browse layout") };
    assert!(
        content.iter().any(|b| matches!(b, Block::Button(x) if matches!(x.id, ControlId::Notice(_)))),
        "a notice card with its button"
    );
}

/// The text field `edit` among the blocks, inside cards too.
fn find_field(blocks: &[Block], edit: EditId) -> Option<FieldModel> {
    blocks.iter().find_map(|b| match b {
        Block::Field(f) if f.edit == edit => Some(f.clone()),
        Block::FieldAction { field, .. } if field.edit == edit => Some(field.clone()),
        Block::Cards(cards) => cards.iter().find_map(|c| find_field(c, edit)),
        _ => None,
    })
}

#[test]
fn the_home_is_the_start_page_with_the_header_the_pill_and_two_cards() {
    let state = Scenario::Inicio.prepared_state();
    let m = main_of(&state);
    let MainLayout::Browse { sidebar, content } = &m.layout else { panic!("browse layout") };
    assert!(sidebar.home.selected);
    assert_eq!(sidebar.app_name, "Swapdesk");
    assert!(matches!(&content[0], Block::Heading { text, level: 1 } if text == "Notebook da sala"));
    assert!(matches!(&content[1], Block::Text(t) if t == "Pronto para receber"));
    assert!(
        matches!(&content[2], Block::Pill { text, warn: false } if text == "Só na rede local: Wi-Fi Casa, 192.168.0.12")
    );
    let Some(Block::Cards(cards)) = content.last() else { panic!("cards last") };
    assert_eq!(cards.len(), 2);
    assert!(matches!(&cards[0][0], Block::Heading { text, .. } if text == "Parear"));
    assert!(cards[0].iter().any(
        |b| matches!(b, Block::Button(x) if x.id == ControlId::PairThisComputer && x.style == ButtonStyle::Primary)
    ));
    let field = find_field(content, EditId::Address).expect("address field in the pair card");
    assert_eq!(field.label, "Adicionar por endereço");
    assert_eq!(field.placeholder, "192.168.0.21");
    assert_eq!(field.hint.as_deref(), Some("Se o outro computador não aparece em Na rede, digite o IPv4 dele."));
    assert!(
        cards[0].iter().any(|b| matches!(b, Block::FieldAction { action, .. } if action.id == ControlId::AddAddress))
    );
    assert!(
        matches!(&cards[1][0], Block::HeadingAction { text, level: 2, action } if text == "Atividade recente" && action.id == ControlId::HomeActivityAll)
    );
    assert_eq!(cards[1].iter().filter(|b| matches!(b, Block::Entry { .. })).count(), 5);
    assert!(
        matches!(&cards[1][1], Block::Entry { who, what, .. } if who == "MacBook do escritório" && what.starts_with("Inverteu"))
    );
}

#[test]
fn the_pill_warns_on_a_public_network() {
    let mut s = Scenario::EstadoRedePublica.prepared_state();
    s.navigate(swapdesk_ui::Nav::Home);
    let MainLayout::Browse { content, .. } = main_of(&s).layout else { panic!() };
    assert!(content.iter().any(|b| matches!(b, Block::Pill { warn: true, .. })));
}

impl Sidebar {
    fn rows_selected_count(&self) -> usize {
        self.groups.iter().flat_map(|g| &g.rows).filter(|r| r.selected).count()
    }
}

#[test]
fn pairing_shows_the_six_digit_code_large_and_the_two_answers() {
    let state = Scenario::Pareamento.prepared_state();
    let m = main_of(&state);
    let MainLayout::Card { blocks } = &m.layout else { panic!("pairing is a card") };
    let digits = blocks
        .iter()
        .find_map(|b| if let Block::Code { digits, .. } = b { Some(digits.clone()) } else { None })
        .expect("code block");
    // The service groups the digits ("482 913"): six digits and nothing else but the separator.
    assert_eq!(digits.chars().filter(char::is_ascii_digit).count(), 6, "{digits}");
    assert!(digits.chars().all(|c| c.is_ascii_digit() || c == ' '), "{digits}");
    let ids: Vec<ControlId> = blocks.iter().flat_map(Block::control_ids).collect();
    assert!(ids.contains(&ControlId::PairSame) && ids.contains(&ControlId::PairDifferent));
    // Side by side, as on Windows (one interface on both systems).
    // One row, the negative action at the start and the positive at the end (owner, 09/11).
    assert!(blocks.iter().any(|b| matches!(b, Block::Actions(v)
        if v.len() == 2 && v[0].id == ControlId::PairDifferent
            && v[1].id == ControlId::PairSame && v[1].style == ButtonStyle::Primary)));
}

#[test]
fn modal_requests_are_cards() {
    for sc in [Scenario::PedidoConexao, Scenario::Consentimento, Scenario::Elevacao, Scenario::ParearEsteComputador] {
        let m = main_of(&sc.prepared_state());
        assert!(matches!(m.layout, MainLayout::Card { .. }), "{}", sc.name());
    }
}

#[test]
fn the_theme_setting_decides_the_palette_and_the_appearance_to_apply() {
    use swapdesk_ui::{SelectId, Theme, UiEvent};
    let mut s = Scenario::Ajustes.prepared_state();
    assert_eq!(appearance_name(s.theme()), None, "Sistema clears the override");
    assert!(!main_of(&s).dark);
    s.choose(SelectId::Theme, 2);
    assert_eq!(appearance_name(s.theme()), Some("NSAppearanceNameDarkAqua"));
    assert!(main_of(&s).dark, "Escuro paints dark on a light OS");
    s.apply(UiEvent::ThemeChanged { dark: true });
    s.choose(SelectId::Theme, 1);
    assert_eq!(appearance_name(s.theme()), Some("NSAppearanceNameAqua"));
    assert!(!main_of(&s).dark, "Claro paints light on a dark OS");
    s.choose(SelectId::Theme, 0);
    assert_eq!(s.theme(), Theme::System);
    assert!(main_of(&s).dark, "Sistema follows the OS again");
    // The select is in Ajustes with the three choices.
    let MainLayout::Browse { content, .. } = main_of(&s).layout else { panic!() };
    let opts = content.iter().find_map(|b| match b {
        Block::Select(m) if m.select == SelectId::Theme => Some(m.options.clone()),
        _ => None,
    });
    assert_eq!(opts.map(|o| o.len()), Some(3));
}

#[test]
fn language_and_theme_follow_the_state() {
    let light = main_of(&Scenario::Dispositivos.prepared_state());
    let dark = main_of(&Scenario::DispositivosEscuro.prepared_state());
    assert!(!light.dark && dark.dark);
    let mut s = Scenario::Dispositivos.prepared_state();
    assert_eq!(main_of(&s).title, "Swapdesk");
    s.set_language(Language::En);
    s.navigate(swapdesk_ui::Nav::Home);
    let MainLayout::Browse { sidebar, content } = main_of(&s).layout else { panic!() };
    let field = find_field(&content, EditId::Address).expect("address field");
    assert_eq!(field.label, s.t(MsgKey::AddressLabel));
    assert_eq!(field.label, "Add by address");
    assert_eq!(sidebar.home.label, "Home");
}

#[test]
fn the_connect_flow_states_show_on_the_main_window() {
    let mut s = Scenario::Dispositivos.prepared_state();
    let dev = s.devices()[0].clone();

    let ev = UiEvent::Session {
        session_id: 7,
        device_id: dev.id.clone(),
        state: SessionState::Prompted { remaining_s: 42 },
    };
    s.apply(ev);
    let strip = main_model(&s).strip.expect("prompted strip");
    assert_eq!(strip.kind, StripKind::Prompted);
    assert_eq!(strip.text, format!("Esperando o {} aceitar", dev.name));
    assert_eq!(strip.detail.as_deref(), Some("0:42"));
    assert_eq!(strip.cancel.as_ref().map(|c| c.1), Some(7));
    s.tick();
    assert_eq!(main_model(&s).strip.unwrap().detail.as_deref(), Some("0:41"), "the countdown counts down");

    let ev = UiEvent::Session { session_id: 7, device_id: dev.id.clone(), state: SessionState::Connecting };
    s.apply(ev);
    assert_eq!(main_model(&s).strip.unwrap().kind, StripKind::Connecting);
    assert!(viewer_model(&s).is_some(), "connecting opens the viewer window");

    let ev = UiEvent::Session {
        session_id: 7,
        device_id: dev.id.clone(),
        state: SessionState::Ended { reason: Some(ApiCode::NotPermitted) },
    };
    s.apply(ev);
    let strip = main_model(&s).strip.expect("ended strip");
    assert_eq!(strip.kind, StripKind::Ended);
    assert_eq!(strip.detail.as_deref(), Some(s.error_text(&ApiError::new(ApiCode::NotPermitted)).as_str()));
    assert!(strip.dismiss.is_some());
    s.dismiss_connect();
    assert!(main_model(&s).strip.is_none());

    // A refusal is already shown by the state's own error line: no second line.
    let ev = UiEvent::Session {
        session_id: 8,
        device_id: dev.id.clone(),
        state: SessionState::Ended { reason: Some(ApiCode::Refused) },
    };
    s.apply(ev);
    assert!(main_model(&s).strip.is_none());
    assert!(s.error().is_some());
    // A normal end says nothing.
    let ev = UiEvent::Session { session_id: 9, device_id: dev.id, state: SessionState::Ended { reason: None } };
    s.apply(ev);
    assert!(main_model(&s).strip.is_none());
}

#[test]
fn the_viewer_toolbar_has_every_tool_and_no_swap_toward_another_os() {
    let state = shell_state(Scenario::Visualizador);
    let m = viewer_model(&state).expect("viewer");
    let ids = m.control_ids();
    // The mock machine is Windows and the scenario's peer a Mac: swap is not in the bar (not in 1.0).
    assert!(!ids.contains(&ControlId::TbSwap));
    for id in [
        ControlId::TbScreen,
        ControlId::TbFit,
        ControlId::TbAudio,
        ControlId::TbGrab,
        ControlId::TbKeys,
        ControlId::TbDisconnect,
        ControlId::TbQuality,
        ControlId::TbViewOnly,
    ] {
        assert!(ids.contains(&id), "{id:?}");
    }
    let tool = |f: &dyn Fn(&Tool) -> bool| m.tools.iter().any(f);
    assert!(tool(&|t| matches!(t, Tool::Quality { options, .. } if options.len() == 3)));
    assert!(tool(&|t| matches!(t, Tool::Keys { options, .. } if options.len() == 5)));
    // The state's own toolbar names match the toolbar's.
    let specs = state.controls(WindowKind::Viewer);
    let label_of = |id| specs.iter().find(|c| c.id == id).map(|c| c.label.clone());
    assert!(m.tools.iter().any(|t| matches!(t, Tool::Switch { tool: SwitchTool::Audio, label, .. } if Some(label) == label_of(ControlId::TbAudio).as_ref())));
    assert!(
        m.tools.iter().any(
            |t| matches!(t, Tool::Disconnect { label } if Some(label) == label_of(ControlId::TbDisconnect).as_ref())
        )
    );
    assert!(m.title.contains("Swapdesk"));
    assert!(m.overlay.is_none(), "an active session has no card over the video");
}

#[test]
fn every_viewer_scenario_maps_and_toolbar_controls_exist_in_the_state() {
    for sc in Scenario::ALL {
        let state = shell_state(sc);
        let Some(m) = viewer_model(&state) else { continue };
        let known: Vec<ControlId> = state.controls(WindowKind::Viewer).iter().map(|c| c.id).collect();
        for id in m.control_ids() {
            // While a dialog or the reconnecting card covers the window the state lists only the card.
            if state.dialog(WindowKind::Viewer).is_some() || known.contains(&ControlId::ReconnectDisconnect) {
                continue;
            }
            assert!(known.contains(&id), "{}: {id:?}", sc.name());
        }
    }
}

#[test]
fn reconnecting_and_swap_show_as_a_card_over_the_video() {
    let m = viewer_model(&Scenario::EstadoReconectando.prepared_state()).expect("viewer");
    let card = m.overlay.expect("reconnecting card");
    assert!(
        card.iter().any(|b| matches!(b, Block::Actions(v) if v.iter().any(|x| x.id == ControlId::ReconnectDisconnect)))
    );

    let m = viewer_model(&Scenario::Inverter.prepared_state()).expect("viewer");
    let card = m.overlay.expect("swap card");
    // The primary at the end of the row (owner, 09/11).
    assert!(
        card.iter()
            .any(|b| matches!(b, Block::Actions(v) if v.last().is_some_and(|x| x.id == ControlId::DialogPrimary)))
    );
}

#[test]
fn a_viewer_without_control_is_view_only_and_cannot_switch_it_off() {
    let mut state = shell_state(Scenario::Visualizador);
    let s = state.viewer_session().expect("session").clone();
    state.apply(UiEvent::Session {
        session_id: s.id,
        device_id: s.device_id,
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(swapdesk_ui::Permission::View),
        },
    });
    let m = viewer_model(&state).unwrap();
    assert!(
        m.tools.iter().any(|t| matches!(t, Tool::Switch { tool: SwitchTool::ViewOnly, on: true, disabled: true, .. }))
    );
}

#[test]
fn the_display_picker_fills_from_the_listing_and_resets_with_the_session() {
    let mut state = shell_state(Scenario::Visualizador);
    let session = state.viewer_session().unwrap().clone();
    let displays = vec![
        DisplayView { id: 3, name: "Principal".into(), width: 1920, height: 1080, scale_milli: 1000, primary: false },
        DisplayView { id: 9, name: "Lateral".into(), width: 1280, height: 720, scale_milli: 1000, primary: true },
    ];
    // The request is asked for once per session.
    let cmd = state.take_display_request().expect("asks once");
    assert_eq!(cmd, Command::ListDisplays { session_id: session.id });
    assert!(state.take_display_request().is_none());
    state.track(Ticket(5), &cmd);
    state.apply(UiEvent::Reply { ticket: Ticket(5), result: Ok(Reply::Displays(displays.clone())) });
    assert_eq!(state.viewer_display(), Some(9), "the primary display is the current one");
    let m = viewer_model(&state).unwrap();
    let Some(Tool::Screen { options, current, .. }) = m.tools.iter().find(|t| matches!(t, Tool::Screen { .. })) else {
        panic!()
    };
    assert_eq!(options.len(), 2);
    assert_eq!(current, "Tela 2");
    assert!(options.iter().any(|(id, _, on)| *id == 9 && *on));

    // Picking one goes through the state and tells the service.
    let act = state.choose_display(3);
    assert_eq!(act.commands, vec![Command::SelectDisplay { session_id: session.id, display_id: 3 }]);
    assert_eq!(
        viewer_model(&state).unwrap().tools.iter().find_map(|t| if let Tool::Screen { current, .. } = t {
            Some(current.clone())
        } else {
            None
        }),
        Some("Tela 1".into())
    );

    // The end of the session clears the quality (the displays go with the session).
    let act = state.choose_quality(Quality::Text);
    assert_eq!(act.commands, vec![Command::SetQuality { session_id: session.id, quality: Quality::Text }]);
    state.apply(UiEvent::Session {
        session_id: session.id,
        device_id: session.device_id,
        state: SessionState::Ended { reason: None },
    });
    assert!(state.viewer_displays().is_empty() && state.quality() == Quality::Auto);
}

#[test]
fn the_quality_and_view_only_switches_go_through_the_state_and_revert_on_refusal() {
    let mut state = shell_state(Scenario::Visualizador);
    let id = state.viewer_session().unwrap().id;
    let act = state.activate(ControlId::TbViewOnly);
    assert_eq!(act.commands, vec![Command::SetViewOnly { session_id: id, on: true }]);
    state.track(Ticket(1), &act.commands[0]);
    assert!(state.view_only());
    state.apply(UiEvent::Reply { ticket: Ticket(1), result: Err(ApiError::new(ApiCode::NotPermitted)) });
    assert!(!state.view_only(), "a refusal puts it back");

    let act = state.activate(ControlId::TbFit);
    state.track(Ticket(2), &act.commands[0]);
    assert!(!state.fit());
    state.apply(UiEvent::Reply { ticket: Ticket(2), result: Err(ApiError::new(ApiCode::Internal)) });
    assert!(state.fit(), "a refused scale does not desync the toolbar");
    let m = viewer_model(&state).unwrap();
    assert!(m.tools.iter().any(|t| matches!(t, Tool::Switch { tool: SwitchTool::Fit, on: true, .. })));
}

#[test]
fn loading_shows_placeholders_only_after_the_state_arms_them() {
    let m = main_of(&Scenario::Carregando.prepared_state());
    let MainLayout::Browse { sidebar, .. } = &m.layout else { panic!() };
    assert!(sidebar.loading);
    let m = main_of(&Scenario::Dispositivos.prepared_state());
    let MainLayout::Browse { sidebar, .. } = &m.layout else { panic!() };
    assert!(!sidebar.loading);
    let m = main_of(&Scenario::AtividadeCarregando.prepared_state());
    let MainLayout::Browse { content, .. } = &m.layout else { panic!() };
    assert!(content.iter().any(|b| matches!(b, Block::Skeleton(_))));
}

#[test]
fn an_unchanged_state_gives_an_equal_model_so_the_window_is_not_rebuilt() {
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        assert_eq!(main_of(&state), main_of(&state), "{}", sc.name());
        assert_eq!(viewer_model(&state), viewer_model(&state), "{}", sc.name());
    }
}

#[test]
fn the_address_edit_is_a_field_with_no_value() {
    let state = Scenario::Inicio.prepared_state();
    let MainLayout::Browse { content, .. } = main_of(&state).layout else { panic!() };
    let field = find_field(&content, EditId::Address).expect("address field");
    assert_eq!(field.edit, EditId::Address);
    assert!(field.value.is_empty());
}

#[test]
fn the_swap_tool_is_in_the_bar_only_toward_a_machine_of_this_os() {
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
    let m = viewer_model(&state).expect("viewer");
    let ids = m.control_ids();
    let (swap, disconnect) = (
        ids.iter().position(|i| *i == ControlId::TbSwap).expect("swap toward the same OS"),
        ids.iter().position(|i| *i == ControlId::TbDisconnect).expect("disconnect"),
    );
    assert_eq!(swap + 1, disconnect, "swap sits right before disconnect");
}

#[test]
fn renaming_puts_the_action_beside_the_name_and_save_beside_cancel() {
    let mut state = Scenario::Dispositivos.prepared_state();
    let content = |state: &UiState| match main_model(state).layout {
        MainLayout::Browse { content, .. } => content,
        MainLayout::Card { blocks } => blocks,
    };
    let blocks = content(&state);
    assert!(
        matches!(&blocks[0], Block::HeadingAction { text, action, .. } if text == "MacBook do escritório" && action.id == ControlId::RenameDevice),
        "{:?}",
        blocks[0]
    );
    assert!(!main_model(&state).has_field(EditId::Rename));

    state.activate(ControlId::RenameDevice);
    let model = main_model(&state);
    assert!(model.has_field(EditId::Rename));
    let blocks = content(&state);
    let field = blocks.iter().find_map(|b| match b {
        Block::Field(f) if f.edit == EditId::Rename => Some(f.clone()),
        _ => None,
    });
    assert_eq!(field.expect("the rename field").value, "MacBook do escritório", "prefilled");
    let row = blocks.iter().find_map(|b| match b {
        Block::ButtonRow(v) => Some(v.iter().map(|x| x.id).collect::<Vec<_>>()),
        _ => None,
    });
    assert_eq!(row, Some(vec![ControlId::RenameCancel, ControlId::RenameSave]), "negative first (owner, 09/11)");
    assert!(blocks.iter().all(|b| !matches!(b, Block::HeadingAction { .. })), "the action gave way to the field");

    state.submit_edit(EditId::Rename, "  ".into());
    let texts: Vec<String> =
        content(&state).iter().filter_map(|b| if let Block::Text(t) = b { Some(t.clone()) } else { None }).collect();
    assert!(texts.contains(&state.t(MsgKey::RenameEmpty)), "{texts:?}");
}

#[test]
fn pair_this_computer_shows_the_time_its_text_refers_to() {
    // Owner, 09/11: the screen said "dentro desse tempo" and showed no time (the Mac drew only the title and the text).
    let state = shell_state(Scenario::ParearEsteComputador);
    let MainLayout::Card { blocks } = main_of(&state).layout else { panic!("the pairing card") };
    let at_text = blocks.iter().position(|b| matches!(b, Block::Text(t) if t.contains("tempo abaixo"))).expect("body");
    match &blocks[at_text + 1] {
        Block::Countdown { time, left_milli, label } => {
            assert!(time.contains(':'), "{time}");
            assert!(*left_milli > 0 && *left_milli <= 1000);
            assert!(label.contains(time.as_str()), "{label}");
        }
        other => panic!("the time comes right after the text that refers to it, got {other:?}"),
    }
    assert!(blocks.iter().any(|b| matches!(b, Block::Text(t) if t.starts_with("Procurando"))), "{blocks:?}");
}
