//! The shared screens over a painter with fixed text metrics: every scenario paints, registers every control the
//! state lists and draws only through the trait, so any shell that implements it gets the same frame.

use std::cell::Cell;

use swapdesk_ui::mock::Scenario;
use swapdesk_ui::tokens::{Color, TextStyle};

use super::*;

/// Text is 0.55 em per character on one line (or wrapped to `max_w`); it counts what the screens ask for.
#[derive(Default)]
struct Fixed {
    ops: Cell<usize>,
    clips: Cell<i32>,
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
        self.op();
    }
    fn fill_rect(&self, _: Rect, _: Color) {
        self.op();
    }
    fn fill_rrect(&self, _: Rect, _: f32, _: Color) {
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
        assert!(l.handle::<String>().is_some(), "a label is drawn by the painter that made it");
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
