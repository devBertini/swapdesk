//! The shared screens over a painter with fixed text metrics: every scenario paints, registers every control the
//! state lists and draws only through the trait, so any shell that implements it gets the same frame.

use std::cell::{Cell, RefCell};

use swapdesk_ui::ControlId;
use swapdesk_ui::mock::Scenario;
use swapdesk_ui::tokens::{Color, TextStyle};

use super::*;

/// Text is 0.55 em per character on one line (or wrapped to `max_w`); it counts what the screens ask for.
#[derive(Default)]
pub(crate) struct Fixed {
    pub(crate) ops: Cell<usize>,
    pub(crate) clips: Cell<i32>,
    pub(crate) clears: Cell<usize>,
    /// Every rectangle filled, in order; the flag says it was a rounded one.
    pub(crate) fills: RefCell<Vec<(Rect, Color, bool)>>,
    /// Every text drawn, in order.
    pub(crate) drawn: RefCell<Vec<String>>,
    /// Where each text was drawn: x, y and its height.
    pub(crate) placed: RefCell<Vec<(f32, f32, f32)>>,
    /// Every text laid out centred: the text, the width of its box and its size.
    pub(crate) centred_made: RefCell<Vec<(String, f32, f32)>>,
    /// Every centred text drawn: the text, the middle of its box and its size.
    pub(crate) centred: RefCell<Vec<(String, f32, f32)>>,
    /// Every text laid out, with its style.
    pub(crate) styled: RefCell<Vec<(String, TextStyle)>>,
    /// The ink of every text drawn, in the order of `drawn`.
    pub(crate) inks: RefCell<Vec<Color>>,
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
    fn layout(&self, text: &str, st: TextStyle, max_w: f32, align: Align, ellipsis: bool, _: usize) -> Label {
        if align == Align::Center {
            self.centred_made.borrow_mut().push((text.to_owned(), max_w, st.size));
        }
        self.styled.borrow_mut().push((text.to_owned(), st));
        let one_line = text.chars().count() as f32 * st.size * 0.55;
        let lines = if ellipsis || one_line <= max_w { 1.0 } else { (one_line / max_w).ceil() };
        Label::new(one_line.min(max_w), lines * st.size * st.line, st.size, text.to_owned())
    }
    fn draw(&self, l: &Label, x: f32, y: f32, ink: Color) {
        let text = l.handle::<String>().expect("a label is drawn by the painter that made it");
        self.drawn.borrow_mut().push(text.clone());
        self.inks.borrow_mut().push(ink);
        self.placed.borrow_mut().push((x, y, l.h));
        if let Some((_, box_w, size)) = self.centred_made.borrow().iter().find(|(t, ..)| t == text) {
            self.centred.borrow_mut().push((text.clone(), x + box_w / 2.0, *size));
        }
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
fn the_banner_grows_by_one_line_when_it_has_a_files_line_and_draws_it_under_the_row() {
    use swapdesk_ui::WindowKind::Banner;
    let plain = Scenario::Banner.prepared_state();
    assert_eq!(banner_size_for(&plain), banner_size(), "no files line, the size it always had");
    let files = Scenario::BannerArquivos.prepared_state();
    let (w, h) = banner_size_for(&files);
    let (bw, bh) = banner_size();
    assert_eq!(w, bw);
    assert!(h > bh, "one line taller: {h} against {bh}");

    let ix = Interaction::default();
    let drawn = |st: &swapdesk_ui::UiState, size: (f32, f32)| {
        let p = Fixed::default();
        let out = paint(Frame::new(&p, st, &ix, size, true), Banner);
        let drawn = p.drawn.borrow().clone();
        let placed = p.placed.borrow().clone();
        (out, drawn, placed)
    };
    let (out, texts, placed) = drawn(&files, (w, h));
    let at = texts.iter().position(|t| t == "Copiando arquivos: 2 de 5").expect("the files line is drawn");
    let (_, y, line_h) = placed[at];
    assert!(y >= bh - 4.0 && y + line_h <= h, "the line sits under the row, inside the window: y {y}, h {h}");
    for id in [ControlId::BannerDisconnect, ControlId::BannerChat] {
        let r = out.widgets.iter().find(|x| x.id == id).expect("a banner button").rect;
        assert!(r.bottom() <= bh, "{id:?} stays in the main row: {r:?}");
    }
    assert!(!texts.iter().any(|t| t.contains("relatorio")), "never a file name: {texts:?}");
    let (_, texts, _) = drawn(&plain, banner_size());
    assert!(!texts.iter().any(|t| t.contains("arquivos")), "no files, no line: {texts:?}");
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
    // The terminal panel's quick actions: while one waits for the other computer, all four are disabled.
    let mut state = Scenario::Terminal.prepared_state();
    let kind = WindowKind::Viewer;
    let is_action = |id: &ControlId| matches!(id, ControlId::Action(_));
    let order_of = |state: &swapdesk_ui::UiState| {
        let p = Fixed::default();
        let ix = Interaction::default();
        let out = paint(Frame::new(&p, state, &ix, design_size(kind), true), kind);
        (input::tab_order(state, kind, &out.widgets), out.widgets)
    };
    let (before, _) = order_of(&state);
    assert_eq!(before.iter().filter(|id| is_action(id)).count(), 4, "{before:?}");

    let sent = state.activate(ControlId::Action(swapdesk_ui::ActionKind::Lock));
    state.track(swapdesk_ui::Ticket(1), &sent.commands[0]);
    let disabled: Vec<_> =
        state.controls(kind).into_iter().filter(|c| c.disabled && is_action(&c.id)).map(|c| c.id).collect();
    assert_eq!(disabled.len(), 4);
    let (after, widgets) = order_of(&state);
    for id in disabled {
        assert!(widgets.iter().any(|w| w.id == id), "{id:?} is still drawn");
        assert!(!after.contains(&id), "{id:?} is disabled: Tab skips it");
    }
    assert_eq!(after.iter().filter(|id| is_action(id)).count(), 0);
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
        // The Arquivos view shows no picture (FILES-UI.md 2.1).
        if !state.windows().contains(&WindowKind::Viewer) || state.explorer_open() {
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
        // The Arquivos view shows no picture (FILES-UI.md 2.1).
        if !state.windows().contains(&WindowKind::Viewer) || state.explorer_open() {
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

// ----- what stands alone is centred (D51) -----

/// A card, a state, an empty page or a dialog that stands alone in its area has its text and its buttons on the
/// middle of that area: the page under the header, the window, the video. The buttons are one group, 12 apart, in
/// the order of the rule (negative on the left, positive on the right).
#[test]
fn what_stands_alone_is_centred_with_its_text_and_its_buttons_on_one_axis() {
    use WindowKind::{Main, Viewer};
    use swapdesk_ui::NoticeKind;
    let (w, _) = design_size(Main);
    let page = (swapdesk_ui::tokens::layout::SIDEBAR_W + w) / 2.0;
    let window = w / 2.0;
    let wait = |sc: Scenario| -> Vec<ControlId> {
        let s = sc.prepared_state();
        let id = s.selected_device().expect("a device is selected").id.clone();
        let mut buttons = s.wait_card(&id).expect("a wait card").buttons;
        buttons.sort_by_key(|b| b.primary);
        buttons.iter().map(|b| b.id).collect()
    };
    let dialog = vec![ControlId::DialogSecondary, ControlId::DialogPrimary];
    let cases: Vec<(Scenario, WindowKind, f32, Vec<ControlId>)> = vec![
        (Scenario::EstadoAguardandoAceite, Main, page, wait(Scenario::EstadoAguardandoAceite)),
        (Scenario::EstadoConexaoRecusada, Main, page, wait(Scenario::EstadoConexaoRecusada)),
        (Scenario::EstadoAcordando, Main, page, wait(Scenario::EstadoAcordando)),
        (Scenario::EstadoRedePublica, Main, page, vec![ControlId::Notice(NoticeKind::Public)]),
        (Scenario::EstadoServicoMac, Main, page, vec![ControlId::Notice(NoticeKind::Service)]),
        (Scenario::EstadoPermissoesMac, Main, page, vec![ControlId::Notice(NoticeKind::MacPerms)]),
        (Scenario::EstadoPastaRecebimentoMac, Main, page, vec![ControlId::Notice(NoticeKind::ReceiveFolder)]),
        (Scenario::EstadoFechouInesperado, Main, page, vec![ControlId::Notice(NoticeKind::Crash)]),
        (Scenario::EstadoNenhumComputador, Main, page, vec![ControlId::NoComputerAddress]),
        (Scenario::EstadoNaoPareado, Main, page, vec![]),
        (Scenario::Historico, Main, page, vec![]),
        (Scenario::AtividadeVazia, Main, page, vec![]),
        (Scenario::Pareamento, Main, window, vec![ControlId::PairDifferent, ControlId::PairSame]),
        (Scenario::ParearEsteComputador, Main, window, vec![ControlId::PairCancel]),
        (Scenario::PedidoConexao, Main, window, vec![ControlId::Decline, ControlId::ViewOnlyAccept, ControlId::Accept]),
        (Scenario::Consentimento, Main, window, dialog.clone()),
        (Scenario::Elevacao, Main, window, dialog.clone()),
        (Scenario::EstadoReconectando, Viewer, window, vec![ControlId::ReconnectDisconnect]),
        (Scenario::EstadoInversaoRecusada, Viewer, window, vec![ControlId::SwapDeclinedClose]),
        (Scenario::InverterAguardando, Viewer, window, vec![]),
        (Scenario::Inverter, Viewer, window, dialog),
    ];
    for (sc, kind, middle, ids) in cases {
        let name = sc.name();
        let state = sc.prepared_state();
        let p = Fixed::default();
        let ix = Interaction::default();
        let out = paint(Frame::new(&p, &state, &ix, design_size(kind), true), kind);

        // The text: something is laid out centred around the middle of the area.
        let centred = p.centred.borrow();
        assert!(
            centred.iter().any(|(_, at, _)| (at - middle).abs() < 0.51),
            "{name}: no text is centred on {middle}: {centred:?}"
        );

        // The buttons: one group in the order given, 12 apart, on one line, around the same middle.
        if ids.is_empty() {
            continue;
        }
        let rects: Vec<Rect> = ids
            .iter()
            .map(|id| {
                out.widgets.iter().find(|w| w.id == *id).unwrap_or_else(|| panic!("{name}: {id:?} is not drawn")).rect
            })
            .collect();
        for pair in rects.windows(2) {
            assert_eq!(pair[0].y, pair[1].y, "{name}: the buttons share a line");
            let gap = pair[1].x - pair[0].right();
            assert!((gap - 12.0).abs() < 0.01, "{name}: {gap} between two buttons of the group, in the order given");
        }
        let group = (rects[0].x + rects[rects.len() - 1].right()) / 2.0;
        assert!((group - middle).abs() < 0.51, "{name}: the buttons are centred on {group}, the area on {middle}");
    }
}

/// The card itself sits in the middle of its area, and at the top of an area too short for it.
#[test]
fn a_card_that_stands_alone_sits_in_the_middle_of_its_area() {
    let state = Scenario::EstadoAguardandoAceite.prepared_state();
    let p = Fixed::default();
    let ix = Interaction::default();
    let mut f = Frame::new(&p, &state, &ix, (1200.0, 760.0), true);
    let buttons = [(ControlId::PairCancel, "Cancelar".to_owned(), Kind::Default)];
    let card = card::Card {
        look: card::Look::Alone,
        icon: Some(card::Icon::Spinner),
        title: Some("Esperando"),
        body: Some("Quem estiver nele precisa aceitar."),
        bar: None,
        buttons: &buttons,
    };
    let area = Rect::new(300.0, 100.0, 800.0, 600.0);
    let r = card.paint_in(&mut f, area, card::W);
    assert_eq!(r.w, card::W);
    assert!((r.center().0 - area.center().0).abs() < 0.51, "{r:?} in {area:?}");
    assert!((r.center().1 - area.center().1).abs() < 0.51, "{r:?} in {area:?}");
    assert_eq!(r.h, card.height(&f, card::W));

    // Narrower than the card: the card takes the width. Shorter: it starts at the top and is not cut.
    let small = Rect::new(300.0, 100.0, 360.0, 80.0);
    let r = card.paint_in(&mut f, small, card::W);
    assert_eq!((r.x, r.y, r.w), (small.x, small.y, small.w));
    assert!(r.h > small.h);
}

/// D55: the note that pairings made before 1.1 do not carry over sits under an empty Pareados, and only there.
#[test]
fn the_note_after_the_update_from_1_0_shows_under_an_empty_paired_list_only() {
    use swapdesk_ui::MsgKey;
    let drawn = |sc: Scenario| -> Vec<String> {
        let state = sc.prepared_state();
        let p = Fixed::default();
        let ix = Interaction::default();
        let _ = paint(Frame::new(&p, &state, &ix, design_size(WindowKind::Main), true), WindowKind::Main);
        p.drawn.take()
    };
    let st = Scenario::PareadosAntesDa11.prepared_state();
    let note = st.t(MsgKey::SidebarLegacyNote);
    let with = drawn(Scenario::PareadosAntesDa11);
    let at = with.iter().position(|t| *t == note).expect("the note is drawn");
    let paired = st.t(MsgKey::SidebarPaired);
    assert!(with[..at].contains(&paired), "under the Pareados caption");
    assert!(!drawn(Scenario::Inicio).contains(&note), "not with paired devices");
}

/// The terminal grid over the trait: one cell box for every face (0.6 em by one line, snapped), backgrounds in runs of
/// one colour, each glyph centred in its box in the monospaced face (bold, italic), a wide glyph over two columns with
/// its second column not drawn, underline and strike as rectangles in the glyph's colour.
#[test]
fn a_terminal_grid_paints_backgrounds_glyphs_lines_and_wide_cells() {
    use crate::grid::{GridCell, grid_metrics, paint_grid};
    use swapdesk_ui::tokens::text::TERM;
    let p = Fixed::default();
    let ink = Color { r: 220, g: 220, b: 220, a: 255 };
    let red = Color { r: 200, g: 0, b: 0, a: 255 };
    let cell = |t: &str| GridCell { text: t.into(), ..GridCell::blank(ink) };
    let rows = vec![
        vec![
            GridCell { bg: Some(red), ..cell("a") },
            GridCell { bg: Some(red), bold: true, ..cell("b") },
            cell(" "),
            GridCell { wide: true, ..cell("漢") },
            GridCell::blank(ink),
            GridCell { italic: true, underline: true, strike: true, ..cell("c") },
        ],
        vec![cell("$")],
    ];
    let m = paint_grid(&p, (10.0, 20.0), &rows, TERM);
    const { assert!(TERM.mono, "the grid's style is the monospaced face") };
    assert_eq!(m, grid_metrics(&p, TERM));
    assert_eq!((m.cell_w, m.cell_h), (8.0, 17.0), "13 px: 0.6 em and 1.3 lines, snapped");
    assert_eq!(m.fit(Rect::new(0.0, 0.0, 81.0, 40.0)), (10, 2));

    // One background run for the two red cells, nothing for cells without their own background.
    let fills = p.fills.borrow();
    let backgrounds: Vec<_> = fills.iter().filter(|(_, c, _)| *c == red).collect();
    assert_eq!(backgrounds.len(), 1);
    assert_eq!(backgrounds[0].0, Rect::new(10.0, 20.0, 16.0, 17.0));

    // Glyphs: a space and the wide cell's second column draw nothing; the second row starts one line lower.
    assert_eq!(*p.drawn.borrow(), vec!["a", "b", "漢", "c", "$"]);
    let placed = p.placed.borrow();
    let styled = p.styled.borrow();
    let label_w = |t: &str| t.chars().count() as f32 * TERM.size * 0.55;
    let wide_x = 10.0 + 3.0 * 8.0;
    assert_eq!(placed[2].0, (wide_x + (16.0 - label_w("漢")) / 2.0).round(), "centred over two columns");
    assert_eq!(
        placed[3].0,
        (10.0 + 5.0 * 8.0 + (8.0 - label_w("c")) / 2.0).round(),
        "after the wide cell's two columns"
    );
    assert!(placed[4].1 >= 20.0 + 17.0, "the second row");
    let style_of = |t: &str| styled.iter().find(|(s, _)| s == t).map(|(_, st)| *st).unwrap();
    assert!(style_of("a").mono && style_of("a").weight == 400 && !style_of("a").italic);
    assert_eq!(style_of("b").weight, 700, "bold");
    assert!(style_of("c").italic && style_of("c").mono, "italic in the monospaced face");

    // Underline at the bottom of the cell and strike across its middle, both one device pixel and in the glyph's ink.
    let lines: Vec<Rect> = fills.iter().filter(|(_, c, _)| *c == ink).map(|(r, ..)| *r).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let c_x = 10.0 + 5.0 * 8.0;
    assert_eq!(lines[0], Rect::new(c_x, 20.0 + 17.0 - 2.0, 8.0, 1.0), "underline");
    assert_eq!(lines[1], Rect::new(c_x, (20.0f32 + 17.0 * 0.55).round(), 8.0, 1.0), "strike");
}

/// Owner, 12/11: the viewer has no top bar; the peer's name and "você controla há …" ride on the session toolbar,
/// and a real picture takes the window's whole height.
#[test]
fn the_viewer_has_no_top_bar_and_its_session_line_rides_on_the_toolbar() {
    let mut state = Scenario::Visualizador.prepared_state();
    let session = state.viewer_session().expect("a session").clone();
    let peer = state.device_by_id(&session.device_id).expect("the peer").name.clone();
    let status = state.viewer_status_text(&session);
    state.set_live_picture(true);
    let (kind, size) = (WindowKind::Viewer, design_size(WindowKind::Viewer));
    let painter = Fixed::default();
    let out = paint(Frame::new(&painter, &state, &Interaction::default(), size, true), kind);
    let video = out.video.expect("a video");
    assert_eq!((video.y, video.h), (0.0, size.1), "the picture takes the whole height: {video:?}");
    let drawn = painter.drawn.borrow();
    assert!(drawn.contains(&peer), "the peer's name is on the toolbar: {drawn:?}");
    assert!(drawn.iter().any(|t| t.starts_with(&status)), "the session line is on the toolbar: {drawn:?}");
    let screen = out.widgets.iter().find(|w| w.id == ControlId::TbScreen).expect("the toolbar");
    assert!(screen.rect.y > 0.0 && screen.rect.y < 80.0, "the toolbar floats at the top of the picture");
}

/// B22 (owner, 12/11): a collapsed session bar is a small handle at the top edge of the picture, centred, and its
/// buttons are not drawn; the handle is where a click brings the bar back.
#[test]
fn a_collapsed_bar_is_a_small_handle_at_the_top_of_the_picture() {
    let mut state = Scenario::Visualizador.prepared_state();
    state.activate(ControlId::TbCollapse);
    let (kind, size) = (WindowKind::Viewer, design_size(WindowKind::Viewer));
    let painter = Fixed::default();
    let out = paint(Frame::new(&painter, &state, &Interaction::default(), size, true), kind);
    assert!(!out.widgets.iter().any(|w| w.id == ControlId::TbScreen), "the bar's buttons are not drawn");
    let handle = out.widgets.iter().find(|w| w.id == ControlId::TbExpand).expect("the handle").rect;
    assert!(handle.w <= 80.0 && handle.h <= 30.0, "small: {handle:?}");
    let (cx, _) = handle.center();
    assert!((cx - size.0 / 2.0).abs() < 2.0, "centred on the picture: {handle:?}");
    let video = out.video.expect("a video");
    assert!((handle.y - video.y).abs() < 1.0 || handle.y <= 60.0, "at the top edge: {handle:?} over {video:?}");
}

/// T1 (13/11): with a side panel open the picture's column is narrower than the full bar; the bar stays inside that
/// column, every button kept (the collapse one too), "Inverter" down to its icon and the session segment shortened or
/// left out.
#[test]
fn the_session_bar_stays_inside_the_pictures_column_when_a_side_panel_is_open() {
    let state = Scenario::Terminal.prepared_state();
    let (kind, size) = (WindowKind::Viewer, design_size(WindowKind::Viewer));
    let painter = Fixed::default();
    let out = paint(Frame::new(&painter, &state, &Interaction::default(), size, true), kind);
    let panel_x = out
        .widgets
        .iter()
        .filter(|w| matches!(w.id, ControlId::DrawerTab(_)))
        .map(|w| w.rect.x)
        .fold(f32::INFINITY, f32::min);
    assert!(panel_x < size.0, "the side panel is open");
    let bar: Vec<_> = out.widgets.iter().filter(|w| format!("{:?}", w.id).starts_with("Tb")).collect();
    for id in [ControlId::TbScreen, ControlId::TbSwap, ControlId::TbDisconnect, ControlId::TbCollapse] {
        assert!(bar.iter().any(|w| w.id == id), "{id:?} is on the bar");
    }
    for w in &bar {
        assert!(
            w.rect.x >= 0.0 && w.rect.right() <= panel_x,
            "{:?} at {:?} leaves the column (panel at {panel_x})",
            w.id,
            w.rect
        );
    }
    let drawn = painter.drawn.borrow();
    assert!(!drawn.iter().any(|t| t == "Inverter"), "Inverter keeps only its icon: {drawn:?}");
}

#[test]
fn the_explorer_draws_the_files_question_and_its_countdown_in_the_right_pane_only() {
    let state = Scenario::ExploradorAguardandoConsentimento.prepared_state();
    let (text, bar) = state.explorer_pane(true).wait.expect("the question is pending");
    assert_eq!(bar.text.as_deref(), Some("0:14"));
    let p = Fixed::default();
    let ix = Interaction::default();
    let out = paint(Frame::new(&p, &state, &ix, design_size(WindowKind::Viewer), true), WindowKind::Viewer);
    let drawn = p.drawn.borrow().clone();
    let placed = p.placed.borrow().clone();
    let at = drawn.iter().position(|t| *t == text).expect("the sentence with the other computer's name is drawn");
    let time = drawn.iter().position(|t| t == "0:14").expect("the countdown row's time is drawn");
    let pal = state.palette();
    assert_eq!(p.inks.borrow()[time], pal.soft, "the time is read in soft ink, not in a background colour");
    let pane = out.widgets.iter().find(|w| w.id == ControlId::ExpPane(true)).expect("the right pane").rect;
    let (x, y, _) = placed[at];
    assert!(x >= pane.x && x < pane.right() && y > pane.y && y < pane.bottom(), "inside the right pane: {x},{y}");
    let list = state.t(swapdesk_ui::MsgKey::ExplorerEmpty);
    assert!(!drawn.contains(&list), "in place of the empty list");
}

#[test]
fn every_breadcrumb_segment_is_a_control_of_its_own_inside_its_pane() {
    let state = Scenario::Explorador.prepared_state();
    let p = Fixed::default();
    let ix = Interaction::default();
    let out = paint(Frame::new(&p, &state, &ix, design_size(WindowKind::Viewer), true), WindowKind::Viewer);
    for there in [false, true] {
        let n = state.explorer_pane(there).crumbs.len();
        assert!(n >= 1, "the pane has a breadcrumb");
        let pane = out.widgets.iter().find(|w| w.id == ControlId::ExpPane(there)).unwrap().rect;
        let mut rects: Vec<Rect> = (0..n)
            .map(|i| {
                let id = ControlId::ExpCrumb(there, i as u8);
                out.widgets.iter().rev().find(|w| w.id == id).unwrap_or_else(|| panic!("{id:?} not placed")).rect
            })
            .collect();
        assert!(rects.iter().all(|r| r.x >= pane.x && r.right() <= pane.right() && r.w > 0.0), "{rects:?}");
        let sorted = rects.clone();
        rects.sort_by(|a, b| a.x.total_cmp(&b.x));
        assert_eq!(rects, sorted, "in reading order");
        assert!(rects.windows(2).all(|w| w[0].right() <= w[1].x), "no overlap: {rects:?}");
    }
}

#[test]
fn the_picture_shows_the_line_of_a_folders_only_drop_over_its_bottom_and_nothing_without_it() {
    use swapdesk_ui::{Command, Reply, Ticket, UiEvent};
    let mut state = Scenario::TelaSoltando.prepared_state();
    let paint_texts = |state: &swapdesk_ui::UiState| {
        let p = Fixed::default();
        let ix = Interaction::default();
        let out = paint(Frame::new(&p, state, &ix, design_size(WindowKind::Viewer), true), WindowKind::Viewer);
        let drawn = p.drawn.borrow().clone();
        let placed = p.placed.borrow().clone();
        (out, drawn, placed)
    };
    let r = state.system_drop(None, (420.0, 300.0));
    let ask = r.activation.commands.into_iter().find(|c| matches!(c, Command::LocalKinds { .. })).expect("kinds asked");
    let Command::LocalKinds { paths } = &ask else { unreachable!() };
    let kinds = paths.iter().map(|p| (p.clone(), true)).collect();
    state.track(Ticket(9_001), &ask);
    state.apply(UiEvent::Reply { ticket: Ticket(9_001), result: Ok(Reply::LocalKinds(kinds)) });
    let note = state.screen_note().expect("the reason is pending");
    let (out, drawn, placed) = paint_texts(&state);
    let at = drawn.iter().position(|t| *t == note).expect("the line is drawn over the picture");
    let video = out.video.expect("the picture");
    let (x, y, _) = placed[at];
    assert!(x >= video.x && x < video.right() && y > video.center().1 && y < video.bottom(), "bottom of the picture");
    for _ in 0..6 {
        state.tick();
    }
    let (_, drawn, _) = paint_texts(&state);
    assert!(!drawn.contains(&note), "gone with its time");
}

/// F-UI-04: the first-run guide card on Início puts "Pular" left of "Entendi": the confirming action trails.
#[test]
fn the_first_run_guide_has_skip_left_of_done() {
    struct Empty;
    impl swapdesk_ui::prefs::PrefsStore for Empty {
        fn read(&self) -> std::io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn write(&self, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut state = Scenario::Inicio.prepared_state();
    state.load_prefs(Box::new(Empty));
    let (kind, size) = (WindowKind::Main, design_size(WindowKind::Main));
    let painter = Fixed::default();
    let out = paint(Frame::new(&painter, &state, &Interaction::default(), size, true), kind);
    let skip = out.widgets.iter().find(|w| w.id == ControlId::GuideSkip).expect("Pular").rect;
    let done = out.widgets.iter().find(|w| w.id == ControlId::GuideDone).expect("Entendi").rect;
    assert!(skip.right() <= done.x, "Pular {skip:?} is left of Entendi {done:?}");
    assert!((skip.y - done.y).abs() < 1.0, "same row");
    let drawn = painter.drawn.borrow();
    assert!(drawn.iter().any(|t| t == "Bem-vindo ao Swapdesk"), "{drawn:?}");
}

/// F-SEC-02: Ajustes draws the blocked list with its note about private or rotating Wi-Fi addresses, with the list
/// empty and with a device in it.
#[test]
fn ajustes_draws_the_blocked_note_with_the_blocked_list() {
    use swapdesk_ui::{Reply, UiEvent};
    let note_drawn = |state: &swapdesk_ui::UiState| {
        let (kind, size) = (WindowKind::Main, design_size(WindowKind::Main));
        let painter = Fixed::default();
        let _ = paint(Frame::new(&painter, state, &Interaction::default(), size, true), kind);
        let drawn = painter.drawn.borrow().join("\n");
        (drawn.contains("endereço Wi-Fi privado ou rotativo"), drawn.contains("Nenhum dispositivo bloqueado."), drawn)
    };
    let mut state = Scenario::Ajustes.prepared_state();
    let (note, none, drawn) = note_drawn(&state);
    assert!(none && note, "empty list: {drawn:?}");

    state.apply(UiEvent::Reply {
        ticket: swapdesk_ui::Ticket(0),
        result: Ok(Reply::Blocked(vec![swapdesk_ui::BlockedItem {
            mac: "02:00:00:00:00:01".into(),
            last_ipv4: "192.168.0.57".into(),
            name: String::new(),
            blocked_at_ms: 0,
        }])),
    });
    let (note, none, drawn) = note_drawn(&state);
    assert!(note && !none, "one blocked device: {drawn:?}");
    assert!(drawn.contains("Dispositivo sem nome"));
}

/// F-VIEW-07: the toolbar draws the codec in its session line, HEVC or H.264.
#[test]
fn the_toolbar_draws_the_codec_in_its_session_line() {
    let drawn_for = |hevc: bool| {
        let mut state = Scenario::Visualizador.prepared_state();
        state.apply(swapdesk_ui::UiEvent::Stats {
            session_id: 1,
            stats: swapdesk_ui::Stats { latency_ms: 18, fps: 60, kbps: 9000, hevc },
        });
        let (kind, size) = (WindowKind::Viewer, design_size(WindowKind::Viewer));
        let painter = Fixed::default();
        let _ = paint(Frame::new(&painter, &state, &Interaction::default(), size, true), kind);
        painter.drawn.borrow().clone()
    };
    let on = drawn_for(true);
    assert!(on.iter().any(|t| t.ends_with("18 ms, 60 qps, HEVC")), "{on:?}");
    let off = drawn_for(false);
    assert!(off.iter().any(|t| t.ends_with("18 ms, 60 qps, H.264")), "{off:?}");
}

#[test]
fn the_explorers_transfer_row_says_no_space_for_that_failure_and_did_not_finish_for_the_rest() {
    use swapdesk_ui::{ApiCode, TransferState, UiEvent};
    let mut state = Scenario::Explorador.prepared_state();
    let session_id = state.viewer_session().expect("a viewer session").id;
    state.activate(ControlId::ExpTransfers);
    assert!(state.explorer_transfers_open());
    for (id, name, reason) in [(1, "filme.mkv", ApiCode::NoSpace), (2, "foto.jpg", ApiCode::Internal)] {
        state.apply(UiEvent::Transfer {
            transfer_id: id,
            session_id,
            name: name.into(),
            outgoing: true,
            state: TransferState::Failed { reason },
        });
    }
    let p = Fixed::default();
    let ix = Interaction::default();
    let _ = paint(Frame::new(&p, &state, &ix, design_size(WindowKind::Viewer), true), WindowKind::Viewer);
    let drawn = p.drawn.borrow().clone();
    assert_eq!(drawn.iter().filter(|t| *t == "Sem espaço livre no outro computador").count(), 1, "{drawn:?}");
    assert_eq!(drawn.iter().filter(|t| *t == "Não terminou").count(), 1, "{drawn:?}");
}
