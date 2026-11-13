//! What a shell's graphics must offer the screens: rectangles, rounded boxes, lines, circles, arcs, SVG icon paths,
//! shadows, clips and laid-out text, in device-independent pixels (96 per inch) with the window's scale known.

use std::any::Any;
use std::rc::Rc;

use swapdesk_ui::tokens::{Color, TextStyle};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Start,
    Center,
}

/// Laid-out text, made by a [`Painter`] and drawn with [`Painter::draw`]. The handle is the shell's own layout object;
/// cloning shares it.
#[derive(Clone)]
pub struct Label {
    pub w: f32,
    pub h: f32,
    /// Distance from the top of the line box to the first baseline.
    pub baseline: f32,
    handle: Rc<dyn Any>,
}

impl Label {
    pub fn new(w: f32, h: f32, baseline: f32, handle: impl Any) -> Label {
        Label { w, h, baseline, handle: Rc::new(handle) }
    }

    /// The shell's layout object, when it is of type `T`.
    pub fn handle<T: Any>(&self) -> Option<&T> {
        self.handle.downcast_ref()
    }
}

/// The drawing operations of one frame, in DIPs. Rectangles a shell fills snap their edges to the device pixel grid,
/// so 1 px borders stay crisp at 125% and 150%.
pub trait Painter {
    /// Device pixels per DIP (1.0 is 100%, 1.5 is 150%).
    fn scale(&self) -> f32;

    /// One device pixel, in DIPs: the width a CSS 1 px border gets (browsers floor it to whole device pixels).
    fn hairline(&self) -> f32 {
        self.scale().floor().max(1.0) / self.scale()
    }

    fn snap(&self, v: f32) -> f32 {
        (v * self.scale()).round() / self.scale()
    }

    fn clear(&self, c: Color);
    fn fill_rect(&self, r: Rect, c: Color);
    fn fill_rrect(&self, r: Rect, radius: f32, c: Color);

    /// A box with a 1 px border: border colour outside, fill inside.
    fn bordered(&self, r: Rect, radius: f32, border: Color, fill: Color) {
        self.fill_rrect(r, radius, border);
        let bw = self.hairline();
        self.fill_rrect(r.inset(bw), (radius - bw).max(0.0), fill);
    }

    /// A stroke centred on the rectangle's edge, like an SVG or a CSS outline of the same width.
    fn stroke_rrect(&self, r: Rect, radius: f32, width: f32, c: Color);
    /// A dashed rounded border (the file drop zone).
    fn stroke_rrect_dashed(&self, r: Rect, radius: f32, width: f32, c: Color);
    fn line(&self, from: (f32, f32), to: (f32, f32), width: f32, c: Color, round: bool);
    fn fill_circle(&self, center: (f32, f32), radius: f32, c: Color);
    fn stroke_circle(&self, center: (f32, f32), radius: f32, width: f32, c: Color);
    /// An arc of `sweep_deg` degrees clockwise from `start_deg` (0 is the positive x axis, y grows down), round caps.
    fn arc_stroke(&self, center: (f32, f32), radius: f32, start_deg: f32, sweep_deg: f32, width: f32, c: Color);
    fn fill_triangle(&self, p: [(f32, f32); 3], c: Color);
    /// Draws an SVG path (`d` attribute: M L H V C A Z, absolute and relative) from a 24-unit viewBox at `(x, y)`,
    /// scaled to `size` DIPs, stroked with `width` (in viewBox units), round caps and joins.
    fn svg(&self, d: &str, x: f32, y: f32, size: f32, width: f32, c: Color);
    /// A soft shadow under a rounded rectangle.
    fn shadow(&self, r: Rect, radius: f32, dy: f32, blur: f32, c: Color);
    fn push_clip(&self, r: Rect);
    fn pop_clip(&self);

    /// The content area (ascent plus descent) of a style, as an inline box would have it.
    fn content_height(&self, st: TextStyle) -> f32;

    /// Lays text out in `max_w` (wrapped, or one line trimmed with "…" when `ellipsis`); the first `bold_chars`
    /// characters are semi-bold. A failed layout gives an empty label rather than failing the frame.
    fn layout(&self, text: &str, st: TextStyle, max_w: f32, align: Align, ellipsis: bool, bold_chars: usize) -> Label;

    /// Draws a label with its line box starting at `(x, y)`; the baseline lands on a whole device pixel.
    fn draw(&self, l: &Label, x: f32, y: f32, c: Color);

    fn label(&self, text: &str, st: TextStyle) -> Label {
        self.label_in(text, st, 100_000.0, Align::Start)
    }

    fn label_in(&self, text: &str, st: TextStyle, max_w: f32, align: Align) -> Label {
        self.layout(text, st, max_w, align, false, 0)
    }

    /// One line, trimmed with an ellipsis at `max_w`.
    fn label_ellipsis(&self, text: &str, st: TextStyle, max_w: f32) -> Label {
        self.layout(text, st, max_w, Align::Start, true, 0)
    }

    /// Wrapped text whose first `bold_chars` characters are semi-bold (a sentence in bold followed by plain text).
    fn label_rich(&self, text: &str, st: TextStyle, max_w: f32, bold_chars: usize) -> Label {
        self.layout(text, st, max_w, Align::Start, false, bold_chars)
    }
}

/// The SVG path of a rounded rectangle, for icons that are drawn from `<rect>` in the design.
pub fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> String {
    format!(
        "M{} {}h{}a{r} {r} 0 0 1 {r} {r}v{}a{r} {r} 0 0 1 -{r} {r}h-{}a{r} {r} 0 0 1 -{r} -{r}v-{}a{r} {r} 0 0 1 {r} -{r}z",
        x + r,
        y,
        w - 2.0 * r,
        h - 2.0 * r,
        w - 2.0 * r,
        h - 2.0 * r
    )
}
