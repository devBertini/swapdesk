//! The shared screens over a painter with fixed text metrics: every scenario paints, registers every control the
//! state lists and draws only through the trait, so any shell that implements it gets the same frame.

use std::cell::{Cell, RefCell};

use swapdesk_ui::ControlId;
use swapdesk_ui::mock::Scenario;
use swapdesk_ui::tokens::{Color, TextStyle};

use super::*;

/// Text is 0.55 em per character on one line (or wrapped to `max_w`); it counts what the screens ask for.
#[derive(Default)]
struct Fixed {
    ops: Cell<usize>,
    clips: Cell<i32>,
    clears: Cell<usize>,
    /// Every rectangle filled, in order; the flag says it was a rounded one.
    fills: RefCell<Vec<(Rect, Color, bool)>>,
    /// Every text drawn, in order.
    drawn: RefCell<Vec<String>>,
    /// Where each text was drawn: x, y and its height.
    placed: RefCell<Vec<(f32, f32, f32)>>,
}

impl Fixed {
    fn op(&self) {
        self.ops.set(self.ops.get() + 1);
    }
}

impl Painter for Fixed {
    fn scale(&self) -> f32 {
        1.0
    }
    fn clear(&self, _: Color) {
        self.clears.set(self.clears.get() + 1);
        self.op();
    }
    fn fill_rect(&self, r: Rect, c: Color) {
        self.fills.borrow_mut().push((r, c, false));
        self.op();
    }
    fn fill_rrect(&self, r: Rect, _: f32, c: Color) {
        self.fills.borrow_mut().push((r, c, true));
        self.op();
    }
    fn stroke_rrect(&self, _: Rect, _: f32, _: f32, _: Color) {
        self.op();
    }
    fn stroke_rrect_dashed(&self, _: Rect, _: f32, _: f32, _: Color) {
        self.op();
    }
    fn line(&self, _: (f32, f32), _: (f32, f32), _: f32, _: Color, _: bool) {
        self.op();
    }
    fn fill_circle(&self, _: (f32, f32), _: f32, _: Color) {
        self.op();
    }
    fn stroke_circle(&self, _: (f32, f32), _: f32, _: f32, _: Color) {
        self.op();
    }
    fn arc_stroke(&self, _: (f32, f32), _: f32, _: f32, _: f32, _: f32, _: Color) {
        self.op();
    }
    fn fill_triangle(&self, _: [(f32, f32); 3], _: Color) {
        self.op();
    }
    fn svg(&self, _: &str, _: f32, _: f32, _: f32, _: f32, _: Color) {
        self.op();
    }
    fn shadow(&self, _: Rect, _: f32, _: f32, _: f32, _: Color) {
        self.op();
    }
    fn push_clip(&self, _: Rect) {
        self.clips.set(self.clips.get() + 1);
    }
    fn pop_clip(&self) {
        self.clips.set(self.clips.get() - 1);
    }
    fn content_height(&self, st: TextStyle) -> f32 {
        st.size * 1.2
    }
    fn layout(&self, text: &str, st: TextStyle, max_w: f32, _: Align, ellipsis: bool, _: usize) -> Label {
        let one_line = text.chars().count() as f32 * st.size * 0.55;
        let lines = if ellipsis || one_line <= max_w { 1.0 } else { (one_line / max_w).ceil() };
        Label::new(one_line.min(max_w), lines * st.size * st.line, st.size, text.to_owned())
    }
    fn draw(&self, l: &Label, x: f32, y: f32, _: Color) {
        let text = l.handle::<String>().expect("a label is drawn by the painter that made it");
        self.drawn.borrow_mut().push(text.clone());
        self.placed.borrow_mut().push((x, y, l.h));
        self.op();
    }
}

#[test]
fn every_scenario_paints_through_the_trait_and_places_every_control() {
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        for kind in state.windows() {
            let p = Fixed::default();
            let ix = Interaction::default();
            let out = paint(Frame::new(&p, &state, &ix, design_size(kind), true), kind);
            assert!(p.ops.get() > 10, "{} {kind:?}: drew {} things", sc.name(), p.ops.get());
            assert_eq!(p.clips.get(), 0, "{} {kind:?}: every clip pushed is popped", sc.name());
            let missing: Vec<_> = state
                .controls(kind)
                .into_iter()
                .filter(|c| c.id.focusable() && !out.widgets.iter().any(|w| w.id == c.id))
                .map(|c| format!("{:?}", c.id))
                .collect();
            assert!(missing.is_empty(), "{} {kind:?}: not drawn: {missing:?}", sc.name());
        }
    }
}

#[test]
fn a_label_hands_back_only_the_handle_type_it_was_made_with() {
    let l = Label::new(10.0, 12.0, 9.0, 7u32);
    assert_eq!(l.handle::<u32>(), Some(&7));
    assert!(l.handle::<String>().is_none());
    let shared = l.clone();
    assert_eq!((shared.w, shared.h, shared.baseline), (10.0, 12.0, 9.0));
}

// ----- input -----

fn widget(id: ControlId, x: f32, y: f32, w: f32, h: f32) -> Widget {
    Widget { id, rect: Rect::new(x, y, w, h), radius: 0.0 }
}

#[test]
fn the_control_registered_last_wins_the_point_and_an_empty_spot_hits_nothing() {
    let (under, over) = (ControlId::Static(1), ControlId::Static(2));
    let widgets = [widget(under, 0.0, 0.0, 100.0, 100.0), widget(over, 40.0, 40.0, 20.0, 20.0)];
    assert_eq!(input::hit_test(&widgets, 50.0, 50.0), Some(over));
    assert_eq!(input::hit_test(&widgets, 10.0, 10.0), Some(under));
    // A box holds its left and top edges, not its right and bottom ones.
    assert_eq!(input::hit_test(&widgets, 40.0, 40.0), Some(over));
    assert_eq!(input::hit_test(&widgets, 60.0, 60.0), Some(under));
    assert_eq!(input::hit_test(&widgets, 100.0, 50.0), None);
    assert_eq!(input::hit_test(&[], 5.0, 5.0), None);
}

#[test]
fn the_tab_order_is_the_drawn_enabled_focusable_controls_in_the_order_of_the_state() {
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        for kind in state.windows() {
            let p = Fixed::default();
            let ix = Interaction::default();
            let out = paint(Frame::new(&p, &state, &ix, design_size(kind), true), kind);
            let controls = state.controls(kind);
            let order = input::tab_order(&state, kind, &out.widgets);
            let expected: Vec<_> = controls
                .iter()
                .filter(|c| !matches!(c.id, ControlId::Static(_)) && !c.disabled)
                .filter(|c| out.widgets.iter().any(|w| w.id == c.id))
                .map(|c| c.id)
                .collect();
            assert_eq!(order, expected, "{} {kind:?}", sc.name());

            // A control that was not drawn takes no focus.
            if let Some(&first) = order.first() {
                let without: Vec<_> = out.widgets.iter().filter(|w| w.id != first).copied().collect();
                let order = input::tab_order(&state, kind, &without);
                assert!(!order.contains(&first), "{} {kind:?}: {first:?} is not drawn", sc.name());
                assert_eq!(order.len(), expected.len() - 1, "{} {kind:?}", sc.name());
            }
        }
    }
}

#[test]
fn a_disabled_control_is_drawn_but_leaves_the_tab_order() {
    // Pairing with the code shown: confirming it disables both answers while the service checks.
    let mut state = Scenario::Pareamento.prepared_state();
    let kind = WindowKind::Main;
    let order_of = |state: &swapdesk_ui::UiState| {
        let p = Fixed::default();
        let ix = Interaction::default();
        let out = paint(Frame::new(&p, state, &ix, design_size(kind), true), kind);
        (input::tab_order(state, kind, &out.widgets), out.widgets)
    };
    let (before, _) = order_of(&state);
    assert!(before.contains(&ControlId::PairSame) && before.contains(&ControlId::PairDifferent), "{before:?}");

    let _ = state.activate(ControlId::PairSame);
    let disabled: Vec<_> = state.controls(kind).into_iter().filter(|c| c.disabled).map(|c| c.id).collect();
    assert_eq!(disabled, [ControlId::PairSame, ControlId::PairDifferent]);
    let (after, widgets) = order_of(&state);
    for id in disabled {
        assert!(widgets.iter().any(|w| w.id == id), "{id:?} is still drawn");
        assert!(!after.contains(&id), "{id:?} is disabled: Tab skips it");
    }
    assert_eq!(after.len(), before.len() - 2);
}

#[test]
fn tab_moves_through_the_order_and_wraps_at_both_ends() {
    let [a, b, c] = [ControlId::Static(1), ControlId::Static(2), ControlId::Static(3)];
    let order = [a, b, c];
    assert_eq!(input::next_focus(&order, None, false), Some(a));
    assert_eq!(input::next_focus(&order, None, true), Some(c));
    assert_eq!(input::next_focus(&order, Some(a), false), Some(b));
    assert_eq!(input::next_focus(&order, Some(c), false), Some(a));
    assert_eq!(input::next_focus(&order, Some(a), true), Some(c));
    assert_eq!(input::next_focus(&order, Some(b), true), Some(a));
    // A focus that left the order (its control is gone or disabled) starts over.
    assert_eq!(input::next_focus(&order, Some(ControlId::Static(9)), false), Some(a));
    assert_eq!(input::next_focus(&order, Some(ControlId::Static(9)), true), Some(c));
    assert_eq!(input::next_focus(&[], Some(a), false), None);
    assert_eq!(input::next_focus(&[a], Some(a), false), Some(a));
}

// ----- the video under the canvas -----

#[test]
fn a_real_picture_gets_the_whole_video_column_and_a_mock_keeps_the_designs_framed_rectangle() {
    let mut viewers = 0;
    for sc in Scenario::ALL {
        let mut state = sc.prepared_state();
        if !state.windows().contains(&WindowKind::Viewer) {
            continue;
        }
        viewers += 1;
        let (name, kind) = (sc.name(), WindowKind::Viewer);
        let ix = Interaction::default();
        let remote = swapdesk_ui::tokens::palette(state.dark()).remote;

        // A mock scenario: the column is filled in one piece and the picture's rectangle sits inside it.
        let mock = Fixed::default();
        let framed = paint(Frame::new(&mock, &state, &ix, design_size(kind), true), kind).video.expect("a video");
        let column = mock.fills.borrow().iter().find(|(_, c, _)| *c == remote).map(|(r, ..)| *r).expect("the column");
        assert!(!state.live_picture(), "{name}: a scenario is not a real run");
        assert!(framed.w <= column.w && framed.h <= column.h, "{name}: {framed:?} inside {column:?}");

        // A real run: the picture's rectangle is the column itself, so nothing is left around it for a margin; the
        // renderer keeps the picture's proportion inside it.
        state.set_live_picture(true);
        for under in [false, true] {
            let live = Fixed::default();
            let mut frame = Frame::new(&live, &state, &ix, design_size(kind), true);
            frame.video_under = under;
            let video = paint(frame, kind).video.expect("a video");
            assert_eq!(video, column, "{name}: the picture gets the whole column (video under the canvas: {under})");
            if under {
                let strips = live.fills.borrow().iter().filter(|(_, c, rounded)| *c == remote && !rounded).count();
                assert_eq!(strips, 0, "{name}: no strip is painted around a picture that fills the column");
            }
        }
    }
    assert!(viewers >= 5, "only {viewers} scenarios open a viewer");
}

#[test]
fn with_the_video_under_the_canvas_the_viewer_leaves_its_rectangle_unpainted_and_moves_nothing() {
    let mut viewers = 0;
    for sc in Scenario::ALL {
        let state = sc.prepared_state();
        if !state.windows().contains(&WindowKind::Viewer) {
            continue;
        }
        viewers += 1;
        let (name, kind) = (sc.name(), WindowKind::Viewer);
        let ix = Interaction::default();
        let remote = swapdesk_ui::tokens::palette(state.dark()).remote;
        let placeholder = state.t(swapdesk_ui::MsgKey::VideoPlaceholder);

        let over = Fixed::default();
        let out_over = paint(Frame::new(&over, &state, &ix, design_size(kind), true), kind);
        assert_eq!(over.clears.get(), 1, "{name}");
        assert!(over.drawn.borrow().contains(&placeholder), "{name}: the placeholder text is drawn");

        let under = Fixed::default();
        let mut frame = Frame::new(&under, &state, &ix, design_size(kind), true);
        frame.video_under = true;
        let out_under = paint(frame, kind);
        assert_eq!(layout_dump(&out_under), layout_dump(&out_over), "{name}: the layout is the same");
        assert_eq!(under.clears.get(), 0, "{name}: a clear would cover the video");
        assert!(!under.drawn.borrow().contains(&placeholder), "{name}: no placeholder text over the video");

        let video = out_under.video.expect("a viewer has a video rectangle");
        let (cx, cy) = video.center();
        let inside = [
            (cx, cy),
            (video.x + 0.5, video.y + 0.5),
            (video.right() - 0.5, video.y + 0.5),
            (video.x + 0.5, video.bottom() - 0.5),
            (video.right() - 0.5, video.bottom() - 0.5),
        ];
        let fills = under.fills.borrow();
        let strips: Vec<Rect> =
            fills.iter().filter(|(_, c, rounded)| *c == remote && !rounded).map(|(r, ..)| *r).collect();
        for r in &strips {
            assert!(!inside.iter().any(|&(x, y)| r.contains(x, y)), "{name}: {r:?} covers the video {video:?}");
        }
        assert!(
            !fills.iter().any(|(r, ..)| *r == video),
            "{name}: nothing is filled exactly on the video rectangle (the placeholder is gone)"
        );
        // The strips and the video tile the column the other mode fills in one piece.
        let column = over.fills.borrow().iter().find(|(_, c, _)| *c == remote).map(|(r, ..)| *r).expect("the column");
        let around: f32 = strips.iter().map(|r| r.w * r.h).sum();
        let want = column.w * column.h - video.w * video.h;
        assert!((around - want).abs() < 0.5, "{name}: strips cover {around}, the column less the video is {want}");
    }
    assert!(viewers >= 5, "only {viewers} scenarios have a viewer window");
}

// ----- the settings rows -----

/// The rows of a settings panel as (top, bottom) spans: the panel's inner edges and the separators between rows.
fn rows_of(panel: Rect, bw: f32, separators: &[Rect]) -> Vec<(f32, f32)> {
    let mut edges: Vec<f32> = vec![panel.y + bw];
    let mut inside: Vec<f32> = separators
        .iter()
        .filter(|s| s.x >= panel.x && s.right() <= panel.right() && s.y > panel.y && s.bottom() < panel.bottom())
        .map(|s| s.y)
        .collect();
    inside.sort_by(f32::total_cmp);
    edges.extend(inside);
    edges.push(panel.bottom() - bw);
    edges.windows(2).map(|w| (w[0], w[1])).collect()
}

#[test]
fn a_settings_row_keeps_its_padding_around_a_label_and_a_hint_of_any_height() {
    // Defect: a row grew to the height of its text and no more, so a hint of two lines (or the list of shortcuts)
    // touched the lines above and below it. The design gives every row 7 px above and below its content.
    let mut rows_seen = 0;
    let mut tall_rows = 0;
    // The settings screen is the one whose rows grow with their text.
    for sc in [Scenario::Ajustes] {
        let state = sc.prepared_state();
        let pal = swapdesk_ui::tokens::palette(state.dark());
        let p = Fixed::default();
        let kind = WindowKind::Main;
        paint(Frame::new(&p, &state, &Interaction::default(), design_size(kind), true), kind);
        let bw = p.hairline();
        let fills = p.fills.borrow();
        // A panel is a rounded surface inside a rounded border; a separator is a plain hairline of the line colour.
        let panels: Vec<Rect> = fills
            .iter()
            .filter(|(r, c, rounded)| *rounded && *c == pal.line && r.w > 300.0 && r.h > 40.0)
            .map(|(r, ..)| *r)
            .collect();
        let separators: Vec<Rect> = fills
            .iter()
            .filter(|(r, c, rounded)| !*rounded && *c == pal.line && (r.h - bw).abs() < 0.01 && r.w > 300.0)
            .map(|(r, ..)| *r)
            .collect();
        assert!(!panels.is_empty(), "{}: no panel found", sc.name());
        for panel in &panels {
            let rows = rows_of(*panel, bw, &separators);
            for &(x, y, h) in p.placed.borrow().iter() {
                if !(x > panel.x && x < panel.right() && y >= panel.y && y + h <= panel.bottom() + 0.01) {
                    continue;
                }
                let Some(&(top, bottom)) = rows.iter().find(|(t, b)| y + h / 2.0 >= *t && y + h / 2.0 <= *b) else {
                    panic!("{}: a text at y {y} of the panel {panel:?} is in no row", sc.name());
                };
                rows_seen += 1;
                if bottom - top > 60.0 {
                    tall_rows += 1;
                }
                assert!(
                    y - top >= 7.0 - 0.01 && bottom - (y + h) >= 7.0 - 0.01,
                    "{}: a text spans y {y}..{} in the row {top}..{bottom}: it needs 7 px above and below",
                    sc.name(),
                    y + h
                );
            }
        }
    }
    assert!(rows_seen > 20, "only {rows_seen} texts were checked");
    assert!(tall_rows > 0, "no row taller than the minimum was checked: the test would not see the defect");
}

/// Where the settings title was drawn and the bottom of the lowest panel, for a frame of `size` painted with `ix`.
fn settings_frame(size: (f32, f32), ix: &Interaction) -> (f32, f32, Output) {
    let state = Scenario::Ajustes.prepared_state();
    let pal = swapdesk_ui::tokens::palette(state.dark());
    let p = Fixed::default();
    let out = paint(Frame::new(&p, &state, ix, size, true), WindowKind::Main);
    let title = state.t(swapdesk_ui::MsgKey::SettingsTitle);
    // The page title, not the sidebar item of the same name.
    let drawn = p.drawn.borrow();
    let placed = p.placed.borrow();
    let title_y = drawn
        .iter()
        .zip(placed.iter())
        .find(|(t, (x, ..))| **t == title && *x > 240.0)
        .map(|(_, (_, y, _))| *y)
        .expect("the title is drawn");
    let lowest = p
        .fills
        .borrow()
        .iter()
        .filter(|(r, c, rounded)| *rounded && *c == pal.line && r.w > 300.0 && r.h > 40.0)
        .map(|(r, ..)| r.bottom())
        .fold(0.0, f32::max);
    (title_y, lowest, out)
}

#[test]
fn the_settings_page_scrolls_when_it_is_taller_than_the_window_and_starts_at_its_top() {
    // With the rows padded the page is taller than a short window: what does not fit is reached by scrolling.
    let size = (1200.0, 520.0);
    let (title_y, lowest, out) = settings_frame(size, &Interaction::default());
    assert!((title_y - 32.0).abs() < 0.01, "the title starts at the top margin, not {title_y}");
    assert!(lowest > size.1, "the test needs a page taller than the window: lowest panel ends at {lowest}");
    assert!((out.scroll_max - (lowest + 32.0 - size.1)).abs() < 0.01, "scroll_max {} for {lowest}", out.scroll_max);
    assert_ne!(out.scroll_key, 0, "the page names its scrolling content");

    // Scrolled to the end, the last panel sits one margin above the bottom edge.
    let end = Interaction { scroll: out.scroll_max, scroll_key: out.scroll_key, ..Interaction::default() };
    let (title_end, lowest_end, out_end) = settings_frame(size, &end);
    assert!((lowest_end - (size.1 - 32.0)).abs() < 0.01, "the last panel ends at {lowest_end}");
    assert!((title_end - (32.0 - out.scroll_max)).abs() < 0.01, "the title moved to {title_end}");
    assert!((out_end.scroll_max - out.scroll_max).abs() < 0.01);
    // Every control moved with the page.
    let (first, moved) = (out.widgets.first().expect("controls"), out_end.widgets.first().expect("controls"));
    assert_eq!(first.id, moved.id);

    // An offset kept from another scrolling screen (the activity list) does not carry over: the page starts at its top.
    let other = Interaction { scroll: out.scroll_max, scroll_key: out.scroll_key + 1, ..Interaction::default() };
    let (title_other, ..) = settings_frame(size, &other);
    assert!((title_other - 32.0).abs() < 0.01, "an offset of another screen moved the title to {title_other}");

    // A window tall enough for the whole page does not scroll.
    let (_, lowest_tall, out_tall) = settings_frame((1200.0, lowest + 200.0), &Interaction::default());
    assert_eq!(out_tall.scroll_max, 0.0, "the page ends at {lowest_tall}");
}
