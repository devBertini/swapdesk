//! The compact layout of the main window over the painter with fixed text metrics: at the widths of small and large
//! phones, upright and sideways, every scenario keeps its controls, its text and its boxes inside the window and
//! inside the column, a finger reaches every control, and what scrolled under the bar cannot be touched.

use swapdesk_ui::mock::Scenario;
use swapdesk_ui::tokens::layout;
use swapdesk_ui::{ControlId, LocalNetwork, Nav, NoticeKind, Screen, UiState, WindowKind};

use crate::card;
use crate::input::{hit_test, hit_touch};
use crate::painter::Rect;
use crate::screens_compact::{BAR_H, COLUMN_MAX, MARGIN};
use crate::tests::Fixed;
use crate::view::{Frame, Interaction, Kind, Output, TOUCH_MIN};
use crate::{is_compact, layout_dump, paint};

/// A small phone, a common one, a large one, and a phone held sideways.
const PHONES: [(f32, f32); 4] = [(320.0, 640.0), (360.0, 740.0), (411.0, 891.0), (800.0, 360.0)];

/// No text is laid out narrower than this: a narrower box means a width that went wrong (on Android a box of nothing
/// breaks a label at every character).
const NARROWEST: f32 = 30.0;

fn touched(scroll: f32, key: u8) -> Interaction {
    Interaction { touch: true, scroll, scroll_key: key, ..Interaction::default() }
}

fn frame(state: &UiState, size: (f32, f32), ix: &Interaction) -> (Fixed, Output) {
    let p = Fixed::default();
    let out = paint(Frame::new(&p, state, ix, size, true), WindowKind::Main);
    (p, out)
}

/// Every scenario with a main window, as a phone's own state (this device is the phone, with a phone's wording and
/// only what a phone has) and as the mock's computer state at a phone's size (a host's screens, which a phone that
/// is controlled will show).
fn panes() -> impl Iterator<Item = (Scenario, UiState)> {
    Scenario::ALL
        .into_iter()
        .flat_map(|sc| [(sc, sc.phone_state()), (sc, sc.prepared_state())])
        .filter(|(_, st)| st.windows().contains(&WindowKind::Main))
}

/// A control of the desktop's sidebar: the compact layout shows it on one pane only.
fn of_the_sidebar(id: ControlId) -> bool {
    matches!(id, ControlId::NavHome | ControlId::NavSettings | ControlId::Device(_))
}

/// The first pane is on the screen: Início, or the device page with no device.
fn first_pane(state: &UiState) -> bool {
    state.nav() == Nav::Home || (state.nav() == Nav::Devices && state.selected_device().is_none())
}

#[test]
fn a_phone_is_compact_both_ways_and_no_desktop_window_ever_is() {
    for size in PHONES {
        assert!(is_compact(size), "{size:?}");
    }
    assert!(!is_compact((layout::MIN_W, layout::MIN_H)), "the smallest desktop window");
    assert!(!is_compact((layout::DESIGN_W, layout::DESIGN_H)));
    assert!(!is_compact((1280.0, 800.0)), "a tablet takes the desktop layout");

    // A window that is pointed at keeps the desktop layout at any size, and a touched one of a desktop's size too.
    let state = Scenario::Dispositivos.prepared_state();
    let (_, small_mouse) = frame(&state, (411.0, 891.0), &Interaction::default());
    assert!(small_mouse.page_view.is_none() && small_mouse.widgets.iter().any(|w| w.id == ControlId::NavSettings));
    let size = (layout::DESIGN_W, layout::DESIGN_H);
    let (_, mouse) = frame(&state, size, &Interaction::default());
    let (_, finger) = frame(&state, size, &touched(0.0, 0));
    assert_eq!(layout_dump(&mouse), layout_dump(&finger), "the same layout");
    assert!(finger.page_view.is_none());
    // Only the fingers' targets differ: at least 48 on each side there, the control's own box here.
    assert!(mouse.widgets.iter().all(|w| w.touch.w == w.rect.w && w.touch.h == w.rect.h));
    assert!(finger.widgets.iter().all(|w| w.touch.w >= TOUCH_MIN && w.touch.h >= TOUCH_MIN));
}

/// The done-when of the compact layout, for every scenario at every phone size.
#[test]
fn every_scenario_fits_a_phone_with_its_controls_its_text_and_its_boxes() {
    for (w, h) in PHONES {
        let column = (w - 2.0 * MARGIN).min(COLUMN_MAX);
        let (left, right) = ((w - column) / 2.0, (w + column) / 2.0);
        for (sc, state) in panes() {
            let what = format!("{}{} at {w}x{h}", sc.name(), if state.on_phone() { " on a phone" } else { "" });
            let (p, out) = frame(&state, (w, h), &touched(0.0, 0));
            let overlay = state.incoming_session_open() || state.dialog(WindowKind::Main).is_some();
            let pane = state.screen() != Screen::Pairing && !overlay;

            // Controls: every one the state lists is placed, but those of the desktop's sidebar, which are the
            // first pane's rows and Ajustes there, and the way back on every other pane.
            let placed = |id: ControlId| out.widgets.iter().any(|w| w.id == id);
            let missing: Vec<String> = state
                .controls(WindowKind::Main)
                .into_iter()
                .filter(|c| c.id.focusable() && !(pane && of_the_sidebar(c.id)) && !placed(c.id))
                .map(|c| format!("{:?}", c.id))
                .collect();
            assert!(missing.is_empty(), "{what}: not drawn: {missing:?}");
            if pane && first_pane(&state) {
                assert!(placed(ControlId::NavSettings) && !placed(ControlId::NavHome), "{what}: the first pane's bar");
                for n in 0..state.devices().len() {
                    assert!(placed(ControlId::Device(n as u16)), "{what}: computer {n} has its row");
                }
            } else if pane {
                assert!(placed(ControlId::NavHome) && !placed(ControlId::NavSettings), "{what}: the way back");
                assert!(!out.widgets.iter().any(|w| matches!(w.id, ControlId::Device(_))), "{what}: no list here");
            }

            // Boxes: no control has a negative side, and each lies inside the window's width; on a pane, inside the
            // column (the bar's buttons reach into the margin by the 10 px around their icon, "Renomear" by 16).
            for wd in &out.widgets {
                let r = wd.rect;
                assert!(r.w > 0.0 && r.h > 0.0, "{what}: {:?} has no size: {r:?}", wd.id);
                assert!(r.x >= -0.01 && r.right() <= w + 0.01, "{what}: {:?} leaves the window: {r:?}", wd.id);
                if pane && r.y >= BAR_H {
                    assert!(
                        r.x >= left - 0.01 && r.right() <= right + 0.01,
                        "{what}: {:?} leaves the column: {r:?}",
                        wd.id
                    );
                }
            }
            for (r, ..) in p.fills.borrow().iter() {
                assert!(r.w >= 0.0 && r.h >= 0.0, "{what}: a fill with a negative side: {r:?}");
            }

            // Text: nothing is laid out in a box of almost nothing, and every run drawn stays inside the window.
            if let Some((narrow, text)) = p.narrowest.borrow().as_ref() {
                assert!(*narrow >= NARROWEST, "{what}: {text:?} was laid out {narrow} wide");
            }
            for (x, end, text) in p.spans.borrow().iter() {
                assert!(*x >= -0.5 && *end <= w + 0.5, "{what}: {text:?} is drawn from {x} to {end}");
            }
            assert_eq!(p.clips.get(), 0, "{what}: every clip pushed is popped");

            // Fingers: a touch in the middle of a control that is in sight takes that control.
            let view = out.page_view;
            assert_eq!(view.is_some(), state.screen() != Screen::Pairing, "{what}: a pane scrolls under its bar");
            for wd in &out.widgets {
                let (cx, cy) = wd.rect.center();
                let in_sight = cy >= 0.0 && cy < h && view.is_none_or(|v| cy < BAR_H || v.contains(cx, cy));
                if in_sight && hit_test(&out.widgets, cx, cy) == Some(wd.id) {
                    assert_eq!(hit_touch(&out.widgets, cx, cy), Some(wd.id), "{what}: a touch on {:?}", wd.id);
                    assert!(wd.touch.w > 0.0 && wd.touch.h > 0.0, "{what}: {:?} takes no finger", wd.id);
                }
            }
        }
    }
}

/// A pane taller than the window scrolls as a whole: at its end the last thing drawn stands above the bottom margin,
/// and what went under the bar is neither drawn over it nor touched through it.
#[test]
fn a_pane_scrolls_under_its_bar_and_the_bar_takes_no_touch_for_it() {
    let size = (360.0, 640.0);
    for sc in [Scenario::Ajustes, Scenario::Inicio, Scenario::Permissoes, Scenario::Atividade] {
        let state = sc.prepared_state();
        let (_, top) = frame(&state, size, &touched(0.0, 0));
        let what = sc.name();
        assert!(top.scroll_max > 100.0 && top.scroll_key >= 3, "{what} is taller than a phone: {}", top.scroll_max);
        let view = top.page_view.expect("a pane");
        assert_eq!((view.y, view.bottom()), (BAR_H, size.1));

        // Another pane's offset is not this one's: it starts at its top.
        let (_, other) = frame(&state, size, &touched(top.scroll_max, top.scroll_key + 1));
        assert_eq!(layout_dump(&other), layout_dump(&top), "{what}");

        let (p, end) = frame(&state, size, &touched(top.scroll_max + 500.0, top.scroll_key));
        assert!((end.scroll_max - top.scroll_max).abs() < 0.01, "{what}: the height does not depend on the offset");
        // The lowest box of the pane stands on the bottom margin.
        let lowest = end.widgets.iter().filter(|w| w.rect.y >= BAR_H).map(|w| w.rect.bottom()).fold(0.0, f32::max);
        assert!(lowest <= size.1 - MARGIN + 0.5, "{what}: the last control ends at {lowest}");
        let fills = p.fills.borrow();
        let deepest = fills.iter().map(|(r, ..)| r.bottom()).fold(0.0, f32::max);
        assert!((deepest - (size.1 - MARGIN)).abs() < 1.0 || deepest <= size.1, "{what}: content ends at {deepest}");

        // Controls that scrolled under the bar take no finger; a touch on the bar reaches only the bar's own.
        let hidden: Vec<_> = end.widgets.iter().filter(|w| w.rect.bottom() <= BAR_H && w.rect.y < 0.0).collect();
        assert!(!hidden.is_empty(), "{what}: something scrolled away");
        assert!(hidden.iter().all(|w| w.touch.w == 0.0 || w.touch.h == 0.0), "{what}: {hidden:?}");
        let mut x = 2.0;
        while x < size.0 {
            for y in [4.0, BAR_H / 2.0, BAR_H - 2.0] {
                let hit = hit_touch(&end.widgets, x, y);
                assert!(
                    matches!(hit, None | Some(ControlId::NavHome | ControlId::NavSettings | ControlId::RenameDevice)),
                    "{what}: ({x},{y}) took {hit:?}"
                );
            }
            x += 6.0;
        }
        // And every control of the pane keeps its finger's worth inside the pane.
        for w in end.widgets.iter().filter(|w| w.rect.y >= BAR_H && w.rect.bottom() <= size.1) {
            assert!(w.touch.w >= TOUCH_MIN && w.touch.h >= TOUCH_MIN.min(w.rect.bottom() - BAR_H), "{what}: {w:?}");
            assert!(w.touch.y >= BAR_H, "{what}: {:?} is touched through the bar: {:?}", w.id, w.touch);
        }
    }
}

/// The activity log lays out only the rows in sight, wherever the pane is scrolled to.
#[test]
fn the_activity_log_lays_out_the_rows_in_sight_only() {
    let state = Scenario::Atividade.prepared_state();
    let rows = state.activity().len();
    assert!(rows >= 6, "the mock's log has {rows} rows");
    let size = (360.0, 400.0);
    let (p, top) = frame(&state, size, &touched(0.0, 0));
    let drawn = |p: &Fixed| {
        let texts = p.drawn.borrow();
        state.activity().iter().filter(|r| texts.iter().any(|t| *t == state.activity_what(r))).count()
    };
    let at_top = drawn(&p);
    assert!(at_top < rows, "{at_top} of {rows} rows at the top of a short window");
    let (p, _) = frame(&state, size, &touched(top.scroll_max, top.scroll_key));
    let at_end = drawn(&p);
    assert!(at_end >= 2 && at_end < rows, "{at_end} of {rows} rows at the end");
    let last = state.activity().last().map(|r| state.activity_what(r)).unwrap_or_default();
    assert!(p.drawn.borrow().contains(&last), "the last entry is in sight at the end");
}

/// What stands side by side on a desktop is stacked on a phone: the two actions of a computer's page share the
/// width under its name with the positive one on the right, a card's buttons that do not fit go one above the
/// other with the positive one on top, and a setting's list goes under its label.
#[test]
fn what_stands_side_by_side_on_a_desktop_is_stacked_on_a_phone() {
    let size = (360.0, 740.0);
    let (left, column) = (MARGIN, size.0 - 2.0 * MARGIN);
    let rect = |out: &Output, id: ControlId| out.widgets.iter().find(|w| w.id == id).map(|w| w.rect).expect("placed");

    let (_, page) = frame(&Scenario::Dispositivos.prepared_state(), size, &touched(0.0, 0));
    let (view, control) = (rect(&page, ControlId::ViewOnly), rect(&page, ControlId::Control));
    assert_eq!((view.x, control.right(), view.y), (left, left + column, control.y), "{view:?} {control:?}");
    assert!((view.w - control.w).abs() < 0.01 && view.right() < control.x, "equal halves, the positive one right");
    assert!(view.y > BAR_H + 60.0, "under the name and its line");
    let rename = rect(&page, ControlId::RenameDevice);
    assert!(rename.bottom() <= BAR_H && rename.right() <= size.0, "Renomear is in the bar: {rename:?}");

    // A group of buttons that does not fit side by side: each across the card, the positive one on top and the way
    // out at the bottom. The same group on a desktop stays in its row, whatever its width.
    let (p, state, ix) = (Fixed::default(), Scenario::PedidoConexao.prepared_state(), touched(0.0, 0));
    let list = [
        (ControlId::Decline, "Recusar o pedido".to_owned(), Kind::Default),
        (ControlId::ViewOnlyAccept, "Deixar apenas assistir".to_owned(), Kind::Default),
        (ControlId::Accept, "Aceitar e deixar controlar".to_owned(), Kind::Primary),
    ];
    let mut f = Frame::new(&p, &state, &ix, size, true);
    assert_eq!(card::actions_height(&f, 240.0, &list), 40.0, "a desktop's row");
    f.compact = true;
    assert_eq!(card::actions_height(&f, 240.0, &list), 3.0 * 40.0 + 2.0 * 10.0);
    assert_eq!(card::actions_height(&f, 240.0, &list[..1]), 40.0, "one button is a row");
    assert_eq!(card::actions_height(&f, 900.0, &list), 40.0, "and so are three that fit");
    card::actions(&mut f, 20.0, 100.0, 240.0, &list, false);
    let placed: Vec<(ControlId, Rect)> = f.out.widgets.iter().map(|w| (w.id, w.rect)).collect();
    assert_eq!(
        placed,
        vec![
            (ControlId::Accept, Rect::new(20.0, 100.0, 240.0, 40.0)),
            (ControlId::ViewOnlyAccept, Rect::new(20.0, 150.0, 240.0, 40.0)),
            (ControlId::Decline, Rect::new(20.0, 200.0, 240.0, 40.0)),
        ]
    );

    let (_, settings) = frame(&Scenario::Ajustes.prepared_state(), size, &touched(0.0, 0));
    let language = rect(&settings, ControlId::Select(swapdesk_ui::SelectId::Language));
    let theme = rect(&settings, ControlId::Select(swapdesk_ui::SelectId::Theme));
    assert!(language.w > column - 60.0 && language.x == theme.x && theme.y > language.bottom() + 20.0, "{language:?}");
}

/// F-AND-01: a phone that may not reach the local network says so at the top of its first pane, in a card across
/// the column, with its button when there is one; every phone size holds it.
#[test]
fn the_local_network_notice_is_the_first_card_of_a_phones_first_pane() {
    for (w, h) in PHONES {
        for (now, button) in [(LocalNetwork::Refused, true), (LocalNetwork::Asking, false)] {
            let mut state = Scenario::Inicio.phone_state();
            let _ = state.set_local_network(now);
            let (p, out) = frame(&state, (w, h), &touched(0.0, 0));
            let what = format!("{now:?} at {w}x{h}");
            let notice = out.widgets.iter().find(|wd| wd.id == ControlId::Notice(NoticeKind::LocalNetwork));
            assert_eq!(notice.is_some(), button, "{what}");
            let (title, ..) = state.notice_texts(NoticeKind::LocalNetwork);
            let texts = p.drawn.borrow();
            let said = texts.iter().position(|t| *t == title).unwrap_or_else(|| panic!("{what}: the notice is drawn"));
            // Before the lists and the cards under it.
            let paired = texts.iter().position(|t| t == "Pareados").expect("the list of paired computers");
            assert!(said < paired, "{what}: the notice comes first");
            if let Some(wd) = notice {
                assert!(wd.rect.x >= MARGIN - 0.01 && wd.rect.right() <= w - MARGIN + 0.01, "{what}: {:?}", wd.rect);
            }
            for (x, end, text) in p.spans.borrow().iter() {
                assert!(*x >= -0.5 && *end <= w + 0.5, "{what}: {text:?} is drawn from {x} to {end}");
            }
        }
    }
}

/// docs/ANDROID.md 5: the page of a computer that needs an update and the dialog of a pairing that met one, at
/// every phone size: nothing is drawn outside the screen, the page offers no button that dials, the dialog only
/// its way out.
#[test]
fn a_computer_that_needs_an_update_fits_every_phone_and_offers_no_dial() {
    for (w, h) in PHONES {
        let page = swapdesk_ui::mock::phone_needs_update_state();
        let dialog = swapdesk_ui::mock::phone_pairing_needs_update_state();
        for (name, state) in [("page", &page), ("dialog", &dialog)] {
            let (p, out) = frame(state, (w, h), &touched(0.0, 0));
            let what = format!("{name} at {w}x{h}");
            let placed: Vec<ControlId> = out.widgets.iter().map(|wd| wd.id).collect();
            for dial in [ControlId::PairWithDevice, ControlId::Control, ControlId::ViewOnly, ControlId::PairRetry] {
                assert!(!placed.contains(&dial), "{what}: {dial:?} is offered");
            }
            assert_eq!(placed.contains(&ControlId::PairClose), name == "dialog", "{what}: {placed:?}");
            assert!(p.drawn.borrow().iter().any(|t| t == "Este computador precisa de atualização"), "{what}");
            for (x, end, text) in p.spans.borrow().iter() {
                assert!(*x >= -0.5 && *end <= w + 0.5, "{what}: {text:?} is drawn from {x} to {end}");
            }
            for wd in &out.widgets {
                assert!(wd.rect.x >= -0.01 && wd.rect.right() <= w + 0.01, "{what}: {:?} at {:?}", wd.id, wd.rect);
            }
        }
    }
}

/// The dialog of a phone without a screen lock and the two cards of a phone whose core did not start, at every
/// phone size: nothing is drawn outside the screen, every control takes a finger, and the stopped screens show
/// nothing of the app's panes.
#[test]
fn the_screen_lock_dialog_and_the_stopped_cards_fit_every_phone() {
    use swapdesk_ui::Stopped;
    for (w, h) in PHONES {
        let states = [
            ("lock", swapdesk_ui::mock::phone_screen_lock_state(), 2),
            ("unreadable", swapdesk_ui::mock::phone_stopped_state(Stopped::DataUnreadable), 1),
            ("not started", swapdesk_ui::mock::phone_stopped_state(Stopped::NotStarted), 0),
        ];
        for (name, state, buttons) in &states {
            let (p, out) = frame(state, (w, h), &touched(0.0, 0));
            let what = format!("{name} at {w}x{h}");
            assert_eq!(
                out.widgets.len(),
                *buttons,
                "{what}: {:?}",
                out.widgets.iter().map(|x| x.id).collect::<Vec<_>>()
            );
            // The dialog stands over nothing that scrolls; a stopped card is its own pane, which scrolls when
            // the screen is too short for it (a phone held sideways).
            assert_eq!(out.page_view.is_some(), *name != "lock", "{what}");
            let (title, ..) = state.notice_texts(state.notices().first().copied().unwrap_or(NoticeKind::ScreenLock));
            assert!(p.drawn.borrow().contains(&title), "{what}: {title:?} is not drawn");
            assert!(!p.drawn.borrow().iter().any(|t| t == "Pareados"), "{what}: a pane shows behind");
            for (x, end, text) in p.spans.borrow().iter() {
                assert!(*x >= -0.5 && *end <= w + 0.5, "{what}: {text:?} is drawn from {x} to {end}");
            }
            // Everything is reached: in sight, or by scrolling the pane to its end.
            let below = out.widgets.iter().map(|wd| wd.rect.bottom() - h).fold(0.0, f32::max);
            assert!(
                below <= out.scroll_max + 0.01,
                "{what}: {below} below the screen, the pane scrolls {}",
                out.scroll_max
            );
            let (_, end) = frame(state, (w, h), &touched(out.scroll_max, out.scroll_key));
            for wd in &end.widgets {
                assert!(wd.rect.x >= -0.01 && wd.rect.right() <= w + 0.01, "{what}: {:?} at {:?}", wd.id, wd.rect);
                assert!(wd.rect.bottom() <= h + 0.01, "{what}: {:?} at {:?} with the pane at its end", wd.id, wd.rect);
                let (cx, cy) = wd.rect.center();
                assert_eq!(hit_touch(&end.widgets, cx, cy), Some(wd.id), "{what}: a touch on {:?}", wd.id);
                assert!(wd.touch.h >= 48.0 - 0.01 && wd.touch.w >= 48.0 - 0.01, "{what}: {:?} is too small", wd.id);
            }
        }
    }
}
