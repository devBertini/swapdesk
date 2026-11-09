//! Offscreen tests: every scenario of the mock backend is drawn by the real drawing code into a bitmap, written as a
//! PNG under `test-results/stage11-shots/` for the comparison with `docs/design`, and checked for the things a picture
//! cannot say by itself (every control of the state has a place on screen, nothing is blank, light and dark differ).

use std::path::PathBuf;

use swapdesk_ui::mock::Scenario;
use swapdesk_ui::tokens::{DARK, LIGHT};
use swapdesk_ui::{ControlId, WindowKind};

use crate::offscreen::{design_size, render};
use crate::view::Interaction;

fn shots_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-results/stage11-shots");
    std::fs::create_dir_all(&dir).expect("shots dir");
    dir
}

fn suffix(kind: WindowKind) -> &'static str {
    match kind {
        WindowKind::Main => "",
        WindowKind::Viewer => "-viewer",
        WindowKind::Banner => "-banner",
    }
}

#[test]
fn every_scenario_renders_to_a_png_and_every_control_has_a_place() {
    let dir = shots_dir();
    let mut written = Vec::new();
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        for kind in state.windows() {
            let r = render(&state, kind, design_size(kind), 1.0, &Interaction::default())
                .unwrap_or_else(|e| panic!("{} {kind:?}: {e}", sc.name()));
            assert!(r.distinct_colors() > 3, "{} {kind:?} looks blank", sc.name());
            let name = format!("{}{}.png", sc.name(), suffix(kind));
            r.save_png(&dir.join(&name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            written.push(name);

            let missing: Vec<_> = state
                .controls(kind)
                .into_iter()
                .filter(|c| c.id.focusable() && !r.output.widgets.iter().any(|w| w.id == c.id))
                .map(|c| format!("{:?} ({})", c.id, c.label))
                .collect();
            assert!(
                missing.is_empty(),
                "{} {kind:?}: controls the state lists but the screen did not draw: {missing:?}",
                sc.name()
            );
            // And the other way round: whatever the screen registered is a control the state knows.
            let known: Vec<_> = state.controls(kind).into_iter().map(|c| c.id).collect();
            let extra: Vec<_> = r.output.widgets.iter().filter(|w| !known.contains(&w.id)).map(|w| w.id).collect();
            assert!(
                extra.is_empty(),
                "{} {kind:?}: the screen registered controls the state does not list: {extra:?}",
                sc.name()
            );
        }
    }
    assert!(written.len() >= 35, "wrote {written:?}");
    std::fs::write(dir.join("INDEX.txt"), written.join("\n")).expect("index");
}

#[test]
fn the_address_field_sits_in_the_pair_card_on_the_home_and_the_sidebar_holds_only_navigation() {
    use swapdesk_ui::EditId;
    use swapdesk_ui::tokens::layout;
    let state = Scenario::Inicio.prepared_state();
    for (w, h) in [(layout::DESIGN_W, layout::DESIGN_H), (layout::MIN_W, layout::MIN_H)] {
        let r = render(&state, WindowKind::Main, (w, h), 1.0, &Interaction::default()).unwrap();
        let slot = r
            .output
            .edits
            .iter()
            .find(|e| e.id == EditId::Address)
            .unwrap_or_else(|| panic!("{w}x{h}: no address field"));
        assert_eq!(slot.placeholder, "192.168.0.21");
        assert!(slot.rect.x >= layout::SIDEBAR_W, "{w}x{h}: the field left the sidebar");
        assert!(slot.rect.right() <= w && slot.rect.bottom() <= h, "{w}x{h}: inside the window");
        let pair = r.output.widgets.iter().find(|x| x.id == ControlId::PairThisComputer).expect("pair button");
        assert!(slot.rect.y > pair.rect.bottom(), "{w}x{h}: the field is under the pair button");
        let all = r.output.widgets.iter().find(|x| x.id == ControlId::HomeActivityAll).expect("Ver tudo");
        assert!(all.rect.bottom() <= h, "{w}x{h}: Ver tudo is on screen");
        // Sidebar: Início, the devices and Ajustes only, all inside its width.
        for x in r.output.widgets.iter().filter(|x| x.rect.x < layout::SIDEBAR_W) {
            assert!(
                matches!(x.id, ControlId::NavHome | ControlId::NavSettings | ControlId::Device(_)),
                "{w}x{h}: {:?} in the sidebar",
                x.id
            );
            assert!(x.rect.right() <= layout::SIDEBAR_W, "{w}x{h}: {:?} overflows the sidebar", x.id);
        }
        // Cards: two columns when wide, one when narrow, never overlapping.
        let n = r.output.widgets.iter().find(|x| x.id == ControlId::HomeActivityAll).unwrap().rect;
        let add = r.output.widgets.iter().find(|x| x.id == ControlId::AddAddress).expect("Adicionar").rect;
        assert!(add.x >= slot.rect.right() && add.right() <= w, "{w}: Adicionar is beside the field");
        if w > 1000.0 {
            assert!(n.x > add.right(), "{w}: two columns");
        } else {
            assert!(n.y > add.bottom(), "{w}: one column");
        }
        // No device page on the home.
        assert!(!r.output.widgets.iter().any(|x| x.id == ControlId::Control));
    }
}

#[test]
fn the_home_drops_its_cards_into_one_column_and_keeps_the_header_clear_at_every_width() {
    use swapdesk_ui::tokens::layout;
    let state = Scenario::Inicio.prepared_state();
    for w in [layout::MIN_W, 1000.0, layout::DESIGN_W, 1600.0] {
        let r = render(&state, WindowKind::Main, (w, 700.0), 1.0, &Interaction::default()).unwrap();
        let get = |id| r.output.widgets.iter().find(|x| x.id == id).map(|x| x.rect).unwrap();
        let (pair, all) = (get(ControlId::PairThisComputer), get(ControlId::HomeActivityAll));
        let content_right = w - 40.0;
        assert!(pair.right() <= content_right && all.right() <= content_right, "{w}: inside the content");
        assert!(all.bottom() <= 700.0 - 32.0 + 0.5, "{w}: Ver tudo above the bottom margin");
    }
}

#[test]
fn dark_and_light_paint_their_own_canvas() {
    let light = Scenario::Dispositivos.prepared_state();
    let dark = Scenario::DispositivosEscuro.prepared_state();
    let (a, b) = (
        render(&light, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default()).unwrap(),
        render(&dark, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default()).unwrap(),
    );
    // The main area's canvas and the sidebar's sunken colour, away from any text.
    assert_eq!(a.rgb_at(600, 745), LIGHT.canvas.hex());
    assert_eq!(b.rgb_at(600, 745), DARK.canvas.hex());
    assert_eq!(a.rgb_at(10, 400), LIGHT.sunken.hex());
    assert_eq!(b.rgb_at(10, 400), DARK.sunken.hex());
}

#[test]
fn the_design_geometry_is_where_the_css_puts_it() {
    let state = Scenario::Dispositivos.prepared_state();
    let r = render(&state, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default()).unwrap();
    let w = |id: ControlId| r.output.widgets.iter().find(|w| w.id == id).map(|w| w.rect).unwrap();
    // The sidebar is 232 px; the footer buttons are 40 px high.
    assert!((w(ControlId::NavSettings).h - 40.0).abs() < 0.01);
    assert!((w(ControlId::NavSettings).w - (232.0 - 24.0 - 1.0)).abs() < 0.01);
    // "Controlar" ends at the main area's right padding (1200 - 40).
    assert!((w(ControlId::Control).right() - 1160.0).abs() < 0.01);
    assert!((w(ControlId::Control).y - 32.0).abs() < 0.01);
    // Header 32 + title block, gap 28, then the tabs row: 40 px high.
    assert!((w(ControlId::Tab(swapdesk_ui::DeviceTab::Overview)).h - 40.0).abs() < 0.01);
    // The selected device row is the first of "Pareados".
    assert!(w(ControlId::Device(0)).w > 200.0);
    // Hit targets of the switches in Permissões are 38 x 22.
    let perm = Scenario::Permissoes.prepared_state();
    let r = render(&perm, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default()).unwrap();
    let sw = r.output.widgets.iter().find(|w| w.id == ControlId::Grant(swapdesk_ui::Permission::View)).unwrap().rect;
    assert_eq!((sw.w, sw.h), (38.0, 22.0));
}

#[test]
fn a_fractional_scale_renders_the_same_screens() {
    let state = Scenario::Permissoes.prepared_state();
    let r = render(&state, WindowKind::Main, design_size(WindowKind::Main), 1.5, &Interaction::default()).unwrap();
    assert_eq!((r.width, r.height), (1800, 1140));
    r.save_png(&shots_dir().join("permissoes@150.png")).unwrap();
    let r = render(
        &Scenario::Dispositivos.prepared_state(),
        WindowKind::Main,
        design_size(WindowKind::Main),
        1.5,
        &Interaction::default(),
    )
    .unwrap();
    r.save_png(&shots_dir().join("dispositivos@150.png")).unwrap();
}

#[test]
fn the_activity_log_draws_only_the_rows_in_view() {
    // 5 000 rows: drawing them all would take seconds; the page-wise draw stays quick and registers no per-row widgets.
    let mut state = Scenario::Atividade.prepared_state();
    let rows: Vec<_> = (0..5000u64)
        .map(|n| swapdesk_ui::AuditView {
            at_ms: swapdesk_ui::mock::NOW_MS - n * 60_000,
            device_name: format!("Dispositivo {n}"),
            what: swapdesk_ui::Msg::new(swapdesk_ui::MsgKey::AuditViewOnly),
            duration_s: Some(120),
            alert: false,
        })
        .collect();
    state.apply(swapdesk_ui::UiEvent::Reply {
        ticket: swapdesk_ui::Ticket(0),
        result: Ok(swapdesk_ui::Reply::Activity(rows)),
    });
    let ix = Interaction { scroll: 100_000.0, ..Interaction::default() };
    let start = std::time::Instant::now();
    let r = render(&state, WindowKind::Main, design_size(WindowKind::Main), 1.0, &ix).unwrap();
    assert!(start.elapsed().as_millis() < 1500, "took {:?}", start.elapsed());
    assert!(r.output.scroll_max > 200_000.0, "{}", r.output.scroll_max);
    assert!(r.output.list_area.is_some());
}

#[test]
fn placeholder_screens_have_the_footprint_of_the_real_ones() {
    // R-UI-1: nothing jumps when the data arrives. The loading screen and the loaded one put their sidebar rows and
    // headings in the same places.
    let loading = render(
        &Scenario::Carregando.prepared_state(),
        WindowKind::Main,
        design_size(WindowKind::Main),
        1.0,
        &Interaction::default(),
    )
    .unwrap();
    assert!(loading.distinct_colors() > 3);
    // The footer buttons (hung from the bottom) are drawn in both.
    let loaded = render(
        &Scenario::Inicio.prepared_state(),
        WindowKind::Main,
        design_size(WindowKind::Main),
        1.0,
        &Interaction::default(),
    )
    .unwrap();
    let y = |r: &crate::offscreen::Rendered, id| r.output.widgets.iter().find(|w| w.id == id).map(|w| w.rect.y);
    assert_eq!(y(&loading, ControlId::NavSettings), y(&loaded, ControlId::NavSettings));
    assert_eq!(y(&loading, ControlId::NavHome), y(&loaded, ControlId::NavHome));
    assert_eq!(y(&loading, ControlId::PairThisComputer), y(&loaded, ControlId::PairThisComputer));
    // Pending (the first 100 ms) draws no placeholder blocks: the main area stays the plain canvas.
    let mut pending = swapdesk_ui::UiState::default();
    pending.startup_commands();
    let r = render(&pending, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default()).unwrap();
    assert_eq!(r.rgb_at(700, 50), LIGHT.canvas.hex());
    assert_eq!(r.rgb_at(330, 45), LIGHT.canvas.hex(), "no skeleton block where the name will be");
    assert_eq!(r.rgb_at(1100, 45), LIGHT.canvas.hex(), "no skeleton chip");
}

#[test]
fn every_screen_has_a_complete_accessibility_tree_with_bounds() {
    // The trees of every window of every scenario, with the bounds of the controls as drawn, written to
    // test-results/stage11-a11y.txt. Every interactive node needs a role and a name; nothing may dangle.
    let mut dump = String::new();
    let mut interactive = 0usize;
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        for kind in state.windows() {
            let r = render(&state, kind, design_size(kind), 1.0, &Interaction::default()).unwrap();
            let bounds = |id: ControlId| {
                r.output.widgets.iter().find(|w| w.id == id).map(|w| {
                    (f64::from(w.rect.x), f64::from(w.rect.y), f64::from(w.rect.right()), f64::from(w.rect.bottom()))
                })
            };
            let tree = swapdesk_ui::a11y::build_tree(&state, kind, &bounds, None);
            let problems = swapdesk_ui::a11y::validate(&tree);
            assert!(problems.is_empty(), "{} {kind:?}: {problems:?}", sc.name());
            // Every focusable control that was drawn has bounds in the tree (a screen reader can find it on screen).
            for c in state.controls(kind).iter().filter(|c| c.id.focusable()) {
                interactive += 1;
                let node = swapdesk_ui::a11y::node_id(c.id);
                let n = tree.nodes.iter().find(|(i, _)| *i == node).map(|(_, n)| n).expect("node");
                assert!(n.bounds().is_some(), "{} {kind:?}: {:?} ({}) has no bounds", sc.name(), c.id, c.label);
                assert!(!c.label.trim().is_empty());
            }
            dump.push_str(&format!(
                "=== {} / {kind:?} ({})
{}
",
                sc.name(),
                sc.board(),
                swapdesk_ui::a11y::dump(&tree)
            ));
        }
    }
    assert!(interactive > 150, "{interactive}");
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-results/stage11-a11y.txt");
    std::fs::write(
        path,
        format!(
            "Interactive nodes checked (name + role + bounds): {interactive}

{dump}"
        ),
    )
    .unwrap();
}

// ----- stage 68: full screen and the viewer shortcuts (D30) -----

fn shots68() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-results/stage68-shots");
    std::fs::create_dir_all(&dir).expect("shots dir");
    dir
}

#[test]
fn full_screen_hides_the_bar_and_the_picture_fills_the_window() {
    use swapdesk_ui::ViewerAction;
    let (w, h) = design_size(WindowKind::Viewer);
    let mut state = Scenario::Visualizador.prepared_state();
    let window = render(&state, WindowKind::Viewer, (w, h), 1.0, &Interaction::default()).unwrap();
    window.save_png(&shots68().join("visualizador-janela.png")).unwrap();
    let id = |r: &crate::offscreen::Rendered, c| r.output.widgets.iter().any(|x| x.id == c);
    assert!(id(&window, ControlId::TbKeys), "the bar shows in a window");
    let video = window.output.video.expect("video rectangle");
    assert!(video.y >= swapdesk_ui::tokens::layout::VIEWER_BAR_H && video.w < w);

    state.run_shortcut(ViewerAction::FullScreen);
    let full = render(&state, WindowKind::Viewer, (w, h), 1.0, &Interaction::default()).unwrap();
    full.save_png(&shots68().join("visualizador-tela-cheia.png")).unwrap();
    assert!(!id(&full, ControlId::TbKeys) && !id(&full, ControlId::TbDisconnect), "the bar is hidden");
    let v = full.output.video.expect("video rectangle");
    assert!((v.x - (w - v.w) / 2.0).abs() < 0.5 && (v.y - (h - v.h) / 2.0).abs() < 0.5, "centred: {v:?}");
    assert!(v.w == w || v.h == h, "the 16:10 picture touches two edges of the window: {v:?}");
    // The viewer's top bar is gone too: its pixel is not the bar's surface colour any more.
    assert_ne!(full.rgb_at(2, 2), window.rgb_at(2, 2));
    let missing: Vec<_> =
        state.controls(WindowKind::Viewer).into_iter().filter(|c| c.id.focusable() && !id(&full, c.id)).collect();
    assert!(missing.is_empty(), "controls listed but not drawn in full screen: {missing:?}");

    // Ctrl+Alt+B brings the session bar back.
    state.run_shortcut(ViewerAction::ToggleBar);
    let shown = render(&state, WindowKind::Viewer, (w, h), 1.0, &Interaction::default()).unwrap();
    shown.save_png(&shots68().join("visualizador-tela-cheia-com-barra.png")).unwrap();
    assert!(id(&shown, ControlId::TbKeys) && id(&shown, ControlId::TbDisconnect));
    let missing: Vec<_> =
        state.controls(WindowKind::Viewer).into_iter().filter(|c| c.id.focusable() && !id(&shown, c.id)).collect();
    assert!(missing.is_empty(), "controls listed but not drawn with the bar shown: {missing:?}");
}

#[test]
fn the_toolbar_tooltip_draws_the_shortcut_list_under_the_bar_and_only_while_hovered() {
    let (w, h) = design_size(WindowKind::Viewer);
    let state = Scenario::Visualizador.prepared_state();
    let plain = render(&state, WindowKind::Viewer, (w, h), 1.0, &Interaction::default()).unwrap();
    let ix = Interaction { hover: Some(ControlId::TbKeys), ..Interaction::default() };
    let hovered = render(&state, WindowKind::Viewer, (w, h), 1.0, &ix).unwrap();
    hovered.save_png(&shots68().join("visualizador-dica-atalhos.png")).unwrap();
    let bar = hovered.output.widgets.iter().find(|x| x.id == ControlId::TbKeys).unwrap().rect;
    // A pixel inside the tooltip card (under the bar, at its right side) is the dark card colour only when it shows.
    let (px, py) = ((w / 2.0 + 150.0) as u32, (bar.bottom() + 60.0) as u32);
    assert_ne!(hovered.rgb_at(px, py), plain.rgb_at(px, py), "the card darkens the picture under it");
    // Away from the card both frames are the same picture.
    assert_eq!(hovered.rgb_at(w as u32 - 40, py), plain.rgb_at(w as u32 - 40, py));
}

#[test]
fn settings_list_the_shortcuts_inside_the_window() {
    let (w, h) = design_size(WindowKind::Main);
    let state = Scenario::Ajustes.prepared_state();
    let r = render(&state, WindowKind::Main, (w, h), 1.0, &Interaction::default()).unwrap();
    r.save_png(&shots68().join("ajustes-atalhos.png")).unwrap();
    let find = |s| r.output.widgets.iter().find(|x| x.id == ControlId::Select(s)).map(|x| x.rect);
    let release = find(swapdesk_ui::SelectId::ReleaseKey).expect("the release key select is drawn");
    assert!(find(swapdesk_ui::SelectId::FullscreenMonitor).is_some(), "the monitor select is drawn");
    assert!(release.bottom() < h, "inside the window at the design size: {release:?}");
}

// ----- rename a paired device, swap only toward the same OS -----

#[test]
fn the_rename_action_and_the_rename_field_fit_the_header_at_every_size() {
    use swapdesk_ui::tokens::layout;
    use swapdesk_ui::{EditId, UiState};
    let overlap = |a: &crate::gfx::Rect, b: &crate::gfx::Rect| {
        a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    };
    let at = |state: &UiState, size: (f32, f32)| {
        render(state, WindowKind::Main, size, 1.0, &Interaction::default()).unwrap()
    };
    for size in [(layout::DESIGN_W, layout::DESIGN_H), (layout::MIN_W, layout::MIN_H)] {
        // Not renaming: "Renomear" sits beside the name and clear of the connect buttons.
        let mut state = Scenario::Dispositivos.prepared_state();
        let r = at(&state, size);
        let widget = |r: &crate::offscreen::Rendered, id: ControlId| {
            r.output.widgets.iter().find(|w| w.id == id).map(|w| w.rect).unwrap_or_else(|| panic!("{id:?} not drawn"))
        };
        let (rename, control, view) =
            (widget(&r, ControlId::RenameDevice), widget(&r, ControlId::Control), widget(&r, ControlId::ViewOnly));
        assert!(!overlap(&rename, &control) && !overlap(&rename, &view), "{size:?}: rename clear of the buttons");
        assert!(rename.right() <= control.x, "{size:?}: inside the title block");

        // Renaming: the field is prefilled, "Cancelar" then "Salvar" follow it (negative first, owner 09/11), nothing
        // overlaps the connect buttons.
        state.activate(ControlId::RenameDevice);
        let r = at(&state, size);
        let slot = r.output.edits.iter().find(|e| e.id == EditId::Rename).expect("the rename field");
        assert_eq!(slot.value, "MacBook do escritório");
        let (save, cancel) = (widget(&r, ControlId::RenameSave), widget(&r, ControlId::RenameCancel));
        let (control, view) = (widget(&r, ControlId::Control), widget(&r, ControlId::ViewOnly));
        assert!(
            slot.rect.right() <= cancel.x && cancel.right() <= save.x,
            "{size:?}: field, Cancelar, Salvar in order"
        );
        for part in [&save, &cancel, &widget(&r, ControlId::Edit(EditId::Rename))] {
            assert!(!overlap(part, &control) && !overlap(part, &view), "{size:?}: clear of the connect buttons");
        }
        assert!(r.output.widgets.iter().all(|w| w.id != ControlId::RenameDevice), "the action gives way to the field");

        // A refused name says why, under the field.
        state.submit_edit(EditId::Rename, "   ".into());
        let r = at(&state, size);
        assert!(r.distinct_colors() > 3);
        assert!(r.output.widgets.iter().any(|w| w.id == ControlId::RenameSave));
    }
}

#[test]
fn the_banner_has_no_swap_toward_another_os_and_closes_the_gap() {
    use swapdesk_ui::api::{Permission, Permissions, Role, SessionState, UiEvent};
    use swapdesk_ui::mock::MAC;
    let widget =
        |r: &crate::offscreen::Rendered, id: ControlId| r.output.widgets.iter().find(|w| w.id == id).map(|w| w.rect);
    let size = design_size(WindowKind::Banner);
    // A Windows peer: chat, swap and disconnect, in that order, 14 apart.
    let state = Scenario::Banner.prepared_state();
    let r = render(&state, WindowKind::Banner, size, 1.0, &Interaction::default()).unwrap();
    let (chat, swap, disc) = (
        widget(&r, ControlId::BannerChat).unwrap(),
        widget(&r, ControlId::BannerSwap).expect("swap toward the same OS"),
        widget(&r, ControlId::BannerDisconnect).unwrap(),
    );
    assert!(chat.right() < swap.x && swap.right() < disc.x);
    // A Mac peer: no swap, and chat takes its place next to disconnect.
    let mut state = Scenario::Banner.prepared_state();
    state.apply(UiEvent::Session {
        session_id: 2,
        device_id: MAC.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Host,
            granted: Permissions::NONE.with(Permission::View),
        },
    });
    let r = render(&state, WindowKind::Banner, size, 1.0, &Interaction::default()).unwrap();
    assert!(widget(&r, ControlId::BannerSwap).is_none(), "no swap toward another OS");
    let (chat2, disc2) = (widget(&r, ControlId::BannerChat).unwrap(), widget(&r, ControlId::BannerDisconnect).unwrap());
    assert_eq!(disc2.x, disc.x, "disconnect keeps its place");
    assert!((disc2.x - chat2.right() - 14.0).abs() < 0.6, "no gap where swap was: {} {}", chat2.right(), disc2.x);
}

#[test]
fn the_viewer_toolbar_has_no_swap_toward_another_os() {
    let widget =
        |r: &crate::offscreen::Rendered, id: ControlId| r.output.widgets.iter().find(|w| w.id == id).map(|w| w.rect);
    let size = design_size(WindowKind::Viewer);
    // The Mac peer of the Visualizador scenario: no swap, the separator moves to "Desconectar".
    let state = Scenario::Visualizador.prepared_state();
    let r = render(&state, WindowKind::Viewer, size, 1.0, &Interaction::default()).unwrap();
    assert!(widget(&r, ControlId::TbSwap).is_none());
    let (chat, disc) = (widget(&r, ControlId::TbChat).unwrap(), widget(&r, ControlId::TbDisconnect).unwrap());
    assert!(
        disc.x - chat.right() > 12.0 && disc.x - chat.right() < 24.0,
        "one separator, no gap: {}",
        disc.x - chat.right()
    );
    // A Windows peer keeps the swap button, between the chat button and "Desconectar".
    use swapdesk_ui::api::{Permission, Permissions, Role, SessionState, UiEvent};
    let mut state = Scenario::Visualizador.prepared_state();
    state.apply(UiEvent::Session {
        session_id: 1,
        device_id: swapdesk_ui::mock::DESKTOP.into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::View),
        },
    });
    let r = render(&state, WindowKind::Viewer, size, 1.0, &Interaction::default()).unwrap();
    let (chat, swap, disc) = (
        widget(&r, ControlId::TbChat).unwrap(),
        widget(&r, ControlId::TbSwap).expect("swap toward the same OS"),
        widget(&r, ControlId::TbDisconnect).unwrap(),
    );
    assert!(chat.right() < swap.x && swap.right() < disc.x);
}

/// Writes the home and the device page, light and dark, at 100 % and 150 %, plus the narrowest window, into the folder
/// named by `SWAPDESK_RENDERS` (a review aid; without the variable it does nothing).
#[test]
fn home_renders_for_review() {
    use swapdesk_ui::tokens::layout;
    let Some(dir) = std::env::var_os("SWAPDESK_RENDERS").map(PathBuf::from) else { return };
    std::fs::create_dir_all(&dir).expect("renders dir");
    for (sc, name) in [
        (Scenario::Inicio, "inicio-claro"),
        (Scenario::InicioEscuro, "inicio-escuro"),
        (Scenario::Dispositivos, "dispositivo-claro"),
        (Scenario::DispositivosEscuro, "dispositivo-escuro"),
        (Scenario::EstadoRedePublica, "dispositivo-rede-publica"),
        (Scenario::Carregando, "inicio-carregando"),
    ] {
        let state = sc.prepared_state();
        for scale in [1.0f32, 1.5] {
            let r = render(&state, WindowKind::Main, design_size(WindowKind::Main), scale, &Interaction::default())
                .unwrap();
            r.save_png(&dir.join(format!("{name}@{}x.png", scale))).unwrap();
        }
    }
    let mut warn = Scenario::EstadoRedePublica.prepared_state();
    warn.navigate(swapdesk_ui::Nav::Home);
    let r = render(&warn, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default()).unwrap();
    r.save_png(&dir.join("inicio-rede-publica@1x.png")).unwrap();
    let state = Scenario::Inicio.prepared_state();
    let r = render(&state, WindowKind::Main, (layout::MIN_W, layout::MIN_H), 1.0, &Interaction::default()).unwrap();
    r.save_png(&dir.join("inicio-janela-minima@1x.png")).unwrap();
    // Ajustes with the "Tema" select, in each choice (the OS is light for the first and third, dark for the second).
    for (sc, theme, name) in [
        (Scenario::Ajustes, swapdesk_ui::Theme::Light, "ajustes-claro"),
        (Scenario::Ajustes, swapdesk_ui::Theme::Dark, "ajustes-escuro"),
        (Scenario::Ajustes, swapdesk_ui::Theme::System, "ajustes-sistema"),
    ] {
        let mut state = sc.prepared_state();
        state.set_theme(theme);
        let r = render(&state, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default()).unwrap();
        r.save_png(&dir.join(format!("{name}@1x.png"))).unwrap();
    }
}

#[test]
fn the_palette_follows_the_theme_setting_and_sistema_follows_the_os() {
    use swapdesk_ui::{SelectId, Theme, UiEvent};
    let canvas = |state: &swapdesk_ui::UiState| {
        render(state, WindowKind::Main, design_size(WindowKind::Main), 1.0, &Interaction::default())
            .unwrap()
            .rgb_at(600, 745)
    };
    let mut s = Scenario::Ajustes.prepared_state();
    assert_eq!(s.theme(), Theme::System);
    assert_eq!(canvas(&s), LIGHT.canvas.hex(), "Sistema with a light OS");
    // Choosing "Escuro" in the select (index 2) turns every window dark while the OS stays light.
    s.choose(SelectId::Theme, 2);
    assert_eq!(s.theme(), Theme::Dark);
    assert_eq!(canvas(&s), DARK.canvas.hex());
    // The OS changing underneath does not matter now; "Claro" wins over a dark OS.
    s.apply(UiEvent::ThemeChanged { dark: true });
    s.choose(SelectId::Theme, 1);
    assert_eq!(canvas(&s), LIGHT.canvas.hex());
    // "Sistema" again: the dark OS shows, and a later OS change is followed live.
    s.choose(SelectId::Theme, 0);
    assert_eq!(canvas(&s), DARK.canvas.hex());
    s.apply(UiEvent::ThemeChanged { dark: false });
    assert_eq!(canvas(&s), LIGHT.canvas.hex());
}
