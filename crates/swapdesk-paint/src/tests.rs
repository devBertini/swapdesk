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
    fn draw(&self, l: &Label, _: f32, _: f32, _: Color) {
        let text = l.handle::<String>().expect("a label is drawn by the painter that made it");
        self.drawn.borrow_mut().push(text.clone());
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
