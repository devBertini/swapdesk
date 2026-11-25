//! The Android `Painter`: it draws nothing itself. Every operation of a frame is written into one buffer, in device
//! pixels, which the Java view replays on its canvas (`crate::canvas_ops`), and text is laid out from Android's
//! measures (`crate::text_measure`). The rules are the Mac painter's (`swapdesk-shell-mac` `painter.rs`), which are
//! Direct2D's: fills and clips snap to whole device pixels, strokes stay where they are, a shadow is twelve
//! translucent layers, the first baseline of a label lands on a whole pixel.

use std::cell::RefCell;
use std::rc::Rc;

use swapdesk_paint::svg::{self, Seg};
use swapdesk_paint::{Align, INSTRUMENT_SANS, Label, Painter, Rect, face_metrics, label_height, line_box};
use swapdesk_ui::tokens::{Color, TextStyle};

use crate::access_nodes::Drawn;
use crate::canvas_ops::{FRAME, Op, Writer, argb, flag};
use crate::text_measure::{Measured, Run, TextCache, TextKey};

/// The weight of the semi-bold start of a label (`Painter::label_rich`).
const SEMI_BOLD: u16 = 600;
/// The layers of a soft shadow.
const SHADOW_STEPS: usize = 12;

/// Ascent and descent of the two faces, as fractions of the em.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub ascent: f32,
    pub descent: f32,
    pub mono_ascent: f32,
    pub mono_descent: f32,
}

impl Metrics {
    /// The interface's face is the embedded Instrument Sans, whose metrics are read from the font itself as on
    /// every system; the monospaced face is the phone's, whose metrics Java reads once.
    pub fn new(mono_ascent: f32, mono_descent: f32) -> Self {
        let (ascent, descent) = face_metrics(INSTRUMENT_SANS).unwrap_or((1.0, 0.25));
        Self { ascent, descent, mono_ascent, mono_descent }
    }

    fn of(&self, st: TextStyle) -> (f32, f32) {
        if st.mono { (self.mono_ascent, self.mono_descent) } else { (self.ascent, self.descent) }
    }
}

/// A label as it will be drawn: its lines, each with where it starts (DIPs from the label's left) and its runs.
struct Laid {
    /// The label's whole text, as it was laid out (before a trim with an ellipsis).
    text: String,
    lines: Vec<(f32, Vec<Run>)>,
    /// In DIPs.
    line_h: f32,
    size_px: f32,
    weight: u16,
    tracking: f32,
    flags: u8,
}

/// Records one frame.
pub struct Recorder<'a> {
    scale: f32,
    metrics: Metrics,
    texts: &'a TextCache,
    out: RefCell<Writer>,
    /// The clips in force, in DIPs, each already cut to the one before.
    clips: RefCell<Vec<Rect>>,
    /// The labels drawn, in their order, for the accessibility tree (`crate::access_nodes`).
    drawn: RefCell<Vec<Drawn>>,
}

impl<'a> Recorder<'a> {
    pub fn new(scale: f32, metrics: Metrics, texts: &'a TextCache) -> Self {
        let mut out = Writer::default();
        out.u8(FRAME);
        Self {
            scale,
            metrics,
            texts,
            out: RefCell::new(out),
            clips: RefCell::new(Vec::new()),
            drawn: RefCell::new(Vec::new()),
        }
    }

    /// The frame's buffer.
    pub fn finish(self) -> Vec<u8> {
        self.out.into_inner().0
    }

    /// The labels the frame drew so far, in their order, each with the part of it that shows.
    pub fn take_drawn(&self) -> Vec<Drawn> {
        std::mem::take(&mut *self.drawn.borrow_mut())
    }

    fn push(&self, op: Op) {
        self.out.borrow_mut().push(&op);
    }

    fn px(&self, v: f32) -> f32 {
        v * self.scale
    }

    fn point(&self, p: (f32, f32)) -> [f32; 2] {
        [self.px(p.0), self.px(p.1)]
    }

    /// A rectangle that is filled or clips: its edges land on whole device pixels, so a 1 px border stays crisp.
    fn snapped(&self, r: Rect) -> [f32; 4] {
        let (x1, y1) = (self.px(r.x).round(), self.px(r.y).round());
        let (x2, y2) = (self.px(r.right()).round(), self.px(r.bottom()).round());
        [x1, y1, (x2 - x1).max(0.0), (y2 - y1).max(0.0)]
    }

    /// A rectangle that is stroked: where it is.
    fn free(&self, r: Rect) -> [f32; 4] {
        [self.px(r.x), self.px(r.y), self.px(r.w), self.px(r.h)]
    }

    /// A path in device pixels, stroked with round ends and joins; nothing for an empty one.
    fn stroke(&self, segs: Vec<Seg>, width: f32, c: Color) {
        if !segs.is_empty() {
            self.push(Op::StrokePath(width, argb(c), segs));
        }
    }
}

/// The size a label gets before Android measured it: about half its size per character, on as many lines as that
/// needs, with its whole text on the first. A frame that used it is shown only when the measures never came
/// (`PhoneWindow::paint`).
fn rough(text: &str, st: TextStyle, scale: f32, max_w: f32, ellipsis: bool) -> Measured {
    let one_line = text.chars().count() as f32 * st.size * 0.5;
    let lines = if ellipsis || one_line <= max_w || max_w <= 0.0 { 1 } else { (one_line / max_w).ceil() as usize };
    let width = one_line.min(max_w.max(0.0)) * scale;
    let line = |i: usize| vec![Run { bold: false, width, text: if i == 0 { text.to_owned() } else { String::new() } }];
    Measured { lines: (0..lines.clamp(1, 64)).map(line).collect() }
}

impl Painter for Recorder<'_> {
    fn scale(&self) -> f32 {
        self.scale
    }

    fn clear(&self, c: Color) {
        self.push(Op::Clear(argb(c)));
    }

    fn fill_rect(&self, r: Rect, c: Color) {
        self.push(Op::FillRect(self.snapped(r), argb(c)));
    }

    fn fill_rrect(&self, r: Rect, radius: f32, c: Color) {
        self.push(Op::FillRrect(self.snapped(r), self.px(radius), argb(c)));
    }

    fn stroke_rrect(&self, r: Rect, radius: f32, width: f32, c: Color) {
        self.push(Op::StrokeRrect(self.free(r), self.px(radius), self.px(width), argb(c)));
    }

    fn stroke_rrect_dashed(&self, r: Rect, radius: f32, width: f32, c: Color) {
        self.push(Op::StrokeRrectDashed(self.free(r), self.px(radius), self.px(width), argb(c)));
    }

    fn line(&self, from: (f32, f32), to: (f32, f32), width: f32, c: Color, round: bool) {
        self.push(Op::Line(self.point(from), self.point(to), self.px(width), argb(c), round));
    }

    fn fill_circle(&self, center: (f32, f32), radius: f32, c: Color) {
        self.push(Op::FillCircle(self.point(center), self.px(radius), argb(c)));
    }

    fn stroke_circle(&self, center: (f32, f32), radius: f32, width: f32, c: Color) {
        self.push(Op::StrokeCircle(self.point(center), self.px(radius), self.px(width), argb(c)));
    }

    fn arc_stroke(&self, center: (f32, f32), radius: f32, start_deg: f32, sweep_deg: f32, width: f32, c: Color) {
        let [cx, cy] = self.point(center);
        self.stroke(svg::arc((cx, cy), self.px(radius), start_deg, sweep_deg), self.px(width), c);
    }

    fn fill_triangle(&self, p: [(f32, f32); 3], c: Color) {
        let points = [p[0].0, p[0].1, p[1].0, p[1].1, p[2].0, p[2].1].map(|v| self.px(v));
        self.push(Op::Triangle(points, argb(c)));
    }

    fn svg(&self, d: &str, x: f32, y: f32, size: f32, width: f32, c: Color) {
        // One unit of the 24-unit viewBox, in device pixels; the stroke's width is in those units.
        let unit = self.px(size / 24.0);
        self.stroke(svg::parse(d, self.px(x), self.px(y), unit), width * unit, c);
    }

    fn shadow(&self, r: Rect, radius: f32, dy: f32, blur: f32, c: Color) {
        for i in 0..SHADOW_STEPS {
            let t = i as f32 / SHADOW_STEPS as f32;
            let grow = blur * (1.0 - t) * 0.5;
            let alpha = f32::from(c.a) / 255.0 * (0.9 / SHADOW_STEPS as f32) * (1.0 + t);
            let layer = Rect::new(r.x - grow, r.y + dy - grow, r.w + 2.0 * grow, r.h + 2.0 * grow);
            self.push(Op::ShadowLayer(self.free(layer), self.px(radius + grow), argb(c) & 0x00FF_FFFF, alpha));
        }
    }

    fn push_clip(&self, r: Rect) {
        let mut clips = self.clips.borrow_mut();
        let cut = clips.last().map_or(r, |top| top.intersect(r));
        clips.push(cut);
        self.push(Op::PushClip(self.snapped(r)));
    }

    fn pop_clip(&self) {
        self.clips.borrow_mut().pop();
        self.push(Op::PopClip);
    }

    fn content_height(&self, st: TextStyle) -> f32 {
        let (ascent, descent) = self.metrics.of(st);
        (ascent + descent) * st.size
    }

    fn layout(&self, text: &str, st: TextStyle, max_w: f32, align: Align, ellipsis: bool, bold_chars: usize) -> Label {
        let scale = self.scale;
        let key = TextKey {
            text: text.to_owned(),
            size_px: (st.size * scale).to_bits(),
            weight: st.weight,
            tracking: st.tracking.to_bits(),
            mono: st.mono,
            italic: st.italic,
            max_w_px: (max_w * scale).round().clamp(0.0, 16_000_000.0) as i32,
            ellipsis,
            bold_chars: bold_chars.min(text.chars().count()).min(usize::from(u16::MAX)) as u16,
        };
        let measured = self.texts.get(&key).unwrap_or_else(|| Rc::new(rough(text, st, scale, max_w, ellipsis)));
        let (ascent, descent) = self.metrics.of(st);
        let (line_h, baseline) = line_box(st, scale, ascent, descent);
        let lines: Vec<(f32, Vec<Run>)> = measured
            .lines
            .iter()
            .map(|line| {
                let width = Measured::line_width(line) / scale;
                let dx = match align {
                    Align::Start => 0.0,
                    Align::Center => (max_w - width) / 2.0,
                };
                (dx, line.clone())
            })
            .collect();
        let count = lines.len().max(1) as f32;
        let laid = Laid {
            text: text.to_owned(),
            lines,
            line_h,
            size_px: st.size * scale,
            weight: st.weight,
            tracking: st.tracking,
            flags: flag::of(st.mono, st.italic, false),
        };
        Label::new(measured.width() / scale, label_height(count * line_h, scale), baseline, laid)
    }

    /// The first baseline lands on a whole device pixel; along the line the runs keep their measured widths.
    fn draw(&self, l: &Label, x: f32, y: f32, c: Color) {
        let Some(laid) = l.handle::<Laid>() else { return };
        let left = laid.lines.iter().map(|(dx, _)| *dx).fold(f32::INFINITY, f32::min);
        let at = Rect::new(x + if left.is_finite() { left } else { 0.0 }, y, l.w, l.h);
        let shows = self.clips.borrow().last().map_or(at, |clip| clip.intersect(at));
        self.drawn.borrow_mut().push(Drawn { text: laid.text.clone(), rect: shows });
        let first = self.px(y + l.baseline).round();
        for (i, (dx, runs)) in laid.lines.iter().enumerate() {
            let mut pen = self.px(x + dx);
            let base = first + self.px(i as f32 * laid.line_h);
            for run in runs {
                if !run.text.is_empty() {
                    self.push(Op::Text {
                        at: [pen, base],
                        color: argb(c),
                        size: laid.size_px,
                        weight: if run.bold { laid.weight.max(SEMI_BOLD) } else { laid.weight },
                        tracking: laid.tracking,
                        flags: laid.flags,
                        text: run.text.clone(),
                    });
                }
                pen += run.width;
            }
        }
    }
}
