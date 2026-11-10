//! The shared screens (`swapdesk-paint`, D37) painted with CoreGraphics and CoreText: the same embedded Instrument
//! Sans, the same CSS line box and the same pixel snapping as the Windows painter, so a frame lands where it lands on
//! Windows. The context's CTM is flipped (y grows down) and scaled to device pixels before a frame is painted.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;

use swapdesk_paint::svg::{self, Seg};
use swapdesk_paint::{Align, Frame, INSTRUMENT_SANS, Interaction, Label, Output, Painter, Rect};
use swapdesk_ui::tokens::{Color, TextStyle};
use swapdesk_ui::{UiState, WindowKind};

use crate::cg::*;

/// The 'wght' axis tag of a variable font.
const WGHT: i32 = i32::from_be_bytes(*b"wght");
/// The layout cache is cleared when it holds more than this many entries.
const LAYOUT_CACHE_MAX: usize = 512;

/// The text, style bits, width in device pixels, alignment, trimming, bold prefix and scale of a cached layout.
type LayoutKey = (String, (u32, u16, u32, u32), i32, Align, bool, usize, u32);

/// A CTFont shared by the cache and the attributed strings made from it.
type SharedFont = std::rc::Rc<Owned>;

/// The embedded face at each weight and size, and the laid-out labels.
pub struct Fonts {
    base: Option<Owned>,
    ascent: f32,
    descent: f32,
    fonts: RefCell<HashMap<(u32, u16), Option<SharedFont>>>,
    layouts: RefCell<HashMap<LayoutKey, Label>>,
    pub layouts_created: Cell<u64>,
}

impl Fonts {
    pub fn new() -> Fonts {
        // SAFETY: the bytes are 'static and copied by CFDataCreate; the descriptor call reads the data object.
        let base = unsafe {
            Owned::new(CFDataCreate(std::ptr::null(), INSTRUMENT_SANS.as_ptr(), INSTRUMENT_SANS.len() as isize))
                .and_then(|data| Owned::new(CTFontManagerCreateFontDescriptorFromData(data.get())))
        };
        let (ascent, descent) = swapdesk_paint::face_metrics(INSTRUMENT_SANS).unwrap_or((1.0, 0.25));
        Fonts {
            base,
            ascent,
            descent,
            fonts: RefCell::default(),
            layouts: RefCell::default(),
            layouts_created: Cell::new(0),
        }
    }

    /// The face at `size` points and CSS `weight` (the variable font's 'wght' axis).
    pub(crate) fn font(&self, size: f32, weight: u16) -> Option<std::rc::Rc<Owned>> {
        let key = (size.to_bits(), weight);
        if let Some(f) = self.fonts.borrow().get(&key) {
            return f.clone();
        }
        let base = self.base.as_ref()?;
        // SAFETY: plain Create calls on live references; each result is owned once.
        let font = unsafe {
            let axis = Owned::new(CFNumberCreate(std::ptr::null(), kCFNumberSInt32Type, (&WGHT as *const i32).cast()));
            let desc = axis.and_then(|axis| {
                Owned::new(CTFontDescriptorCreateCopyWithVariation(base.get(), axis.get(), f64::from(weight)))
            });
            let desc_ref = desc.as_ref().map_or(base.get(), Owned::get);
            Owned::new(CTFontCreateWithFontDescriptor(desc_ref, f64::from(size), std::ptr::null()))
        }
        .map(std::rc::Rc::new);
        self.fonts.borrow_mut().insert(key, font.clone());
        font
    }

    /// An attributed string of `text` in the style, its first `bold_utf16` units semi-bold.
    fn attributed(&self, text: &str, st: TextStyle, bold_utf16: isize) -> Option<(Owned, isize)> {
        let s = cf_string(text)?;
        let regular = self.font(st.size, st.weight)?;
        // SAFETY: plain Core Foundation calls on references created here.
        unsafe {
            let a = Owned::new(CFAttributedStringCreateMutable(std::ptr::null(), 0))?;
            CFAttributedStringReplaceString(a.mut_ptr(), CFRange { location: 0, length: 0 }, s.get());
            let len = CFStringGetLength(s.get());
            let all = CFRange { location: 0, length: len };
            CFAttributedStringSetAttribute(a.mut_ptr(), all, kCTFontAttributeName, regular.get());
            CFAttributedStringSetAttribute(
                a.mut_ptr(),
                all,
                kCTForegroundColorFromContextAttributeName,
                kCFBooleanTrue,
            );
            if st.tracking != 0.0 {
                let kern = f64::from(st.tracking * st.size);
                if let Some(n) =
                    Owned::new(CFNumberCreate(std::ptr::null(), kCFNumberFloat64Type, (&kern as *const f64).cast()))
                {
                    CFAttributedStringSetAttribute(a.mut_ptr(), all, kCTKernAttributeName, n.get());
                }
            }
            if bold_utf16 > 0
                && let Some(bold) = self.font(st.size, 600)
            {
                let range = CFRange { location: 0, length: bold_utf16.min(len) };
                CFAttributedStringSetAttribute(a.mut_ptr(), range, kCTFontAttributeName, bold.get());
            }
            Some((a, len))
        }
    }

    #[allow(clippy::too_many_arguments)] // the same geometry and style as the trait's `layout`
    pub fn layout(
        &self,
        text: &str,
        st: TextStyle,
        scale: f32,
        max_w: f32,
        align: Align,
        ellipsis: bool,
        bold_chars: usize,
    ) -> Label {
        let key: LayoutKey = (
            text.to_owned(),
            (st.size.to_bits(), st.weight, st.line.to_bits(), st.tracking.to_bits()),
            (max_w * scale).round() as i32,
            align,
            ellipsis,
            bold_chars,
            scale.to_bits(),
        );
        if let Some(hit) = self.layouts.borrow().get(&key) {
            return hit.clone();
        }
        let label = self.make_layout(text, st, scale, max_w, align, ellipsis, bold_chars);
        let mut cache = self.layouts.borrow_mut();
        if cache.len() >= LAYOUT_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, label.clone());
        label
    }

    #[allow(clippy::too_many_arguments)] // the same geometry and style as `layout`
    fn make_layout(
        &self,
        text: &str,
        st: TextStyle,
        scale: f32,
        max_w: f32,
        align: Align,
        ellipsis: bool,
        bold_chars: usize,
    ) -> Label {
        self.layouts_created.set(self.layouts_created.get() + 1);
        let (line_h, baseline) = swapdesk_paint::line_box(st, scale, self.ascent, self.descent);
        let bold_utf16 = text.chars().take(bold_chars).map(char::len_utf16).sum::<usize>() as isize;
        let mut lines: Vec<Line> = Vec::new();
        if let Some((a, len)) = self.attributed(text, st, bold_utf16) {
            // SAFETY: Core Text calls on the attributed string made above; every line is owned once.
            unsafe {
                if ellipsis {
                    if let Some(line) = Owned::new(CTLineCreateWithAttributedString(a.mut_ptr())) {
                        let line = if Line::measure(&line).0 > f64::from(max_w) {
                            let token = self
                                .attributed("\u{2026}", st, 0)
                                .and_then(|(t, _)| Owned::new(CTLineCreateWithAttributedString(t.mut_ptr())));
                            let token_ref = token.as_ref().map_or(std::ptr::null(), Owned::get);
                            Owned::new(CTLineCreateTruncatedLine(
                                line.get(),
                                f64::from(max_w),
                                kCTLineTruncationEnd,
                                token_ref,
                            ))
                            .unwrap_or(line)
                        } else {
                            line
                        };
                        lines.push(Line::new(line));
                    }
                } else if let Some(ts) = Owned::new(CTTypesetterCreateWithAttributedString(a.mut_ptr())) {
                    let mut start = 0;
                    while start < len {
                        let n = CTTypesetterSuggestLineBreak(ts.get(), start, f64::from(max_w)).max(1);
                        let range = CFRange { location: start, length: n };
                        if let Some(line) = Owned::new(CTTypesetterCreateLine(ts.get(), range)) {
                            lines.push(Line::new(line));
                        }
                        start += n;
                    }
                }
            }
        }
        let w = lines.iter().map(|l| l.width).fold(0.0f32, f32::max);
        for l in &mut lines {
            l.dx = match align {
                Align::Start => 0.0,
                Align::Center => (max_w - l.width) / 2.0,
            };
        }
        let count = lines.len().max(1) as f32;
        let h = swapdesk_paint::label_height(count * line_h, scale);
        Label::new(w, h, baseline, CtLabel { lines, line_h })
    }
}

impl Default for Fonts {
    fn default() -> Fonts {
        Fonts::new()
    }
}

/// One laid-out line: its width without trailing spaces and its offset inside the layout box.
struct Line {
    line: Owned,
    width: f32,
    dx: f32,
}

impl Line {
    fn new(line: Owned) -> Line {
        let (_, visible) = Line::measure(&line);
        Line { line, width: visible as f32, dx: 0.0 }
    }

    /// (typographic width, width without the trailing whitespace).
    fn measure(line: &Owned) -> (f64, f64) {
        // SAFETY: a live line; null out-pointers are allowed.
        unsafe {
            let w = CTLineGetTypographicBounds(
                line.get(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            (w, w - CTLineGetTrailingWhitespaceWidth(line.get()))
        }
    }
}

/// The handle a Mac label carries.
struct CtLabel {
    lines: Vec<Line>,
    line_h: f32,
}

/// Paints one frame onto a CoreGraphics context whose CTM is flipped and scaled (DIPs in, device pixels out).
pub struct CgPainter<'a> {
    ctx: CGContextRef,
    fonts: &'a Fonts,
    scale: f32,
    size: (f32, f32),
}

impl<'a> CgPainter<'a> {
    /// # Safety
    /// `ctx` must stay a live context for the painter's life, with a y-down CTM in DIPs.
    pub unsafe fn new(ctx: CGContextRef, fonts: &'a Fonts, scale: f32, size: (f32, f32)) -> CgPainter<'a> {
        // SAFETY: the caller's context is live.
        unsafe { CGContextSetShouldSmoothFonts(ctx, false) };
        CgPainter { ctx, fonts, scale, size }
    }

    fn snapped(&self, r: Rect) -> CGRect {
        let (l, t) = (self.snap(r.x), self.snap(r.y));
        let (rt, b) = (self.snap(r.right()), self.snap(r.bottom()));
        CGRect::new(f64::from(l), f64::from(t), f64::from(rt - l), f64::from(b - t))
    }

    fn fill_color(&self, c: Color) {
        let [r, g, b, a] = c.to_f32();
        // SAFETY: the colour is created, set and released here.
        unsafe {
            if let Some(col) = Owned::new(CGColorCreateSRGB(f64::from(r), f64::from(g), f64::from(b), f64::from(a))) {
                CGContextSetFillColorWithColor(self.ctx, col.mut_ptr());
            }
        }
    }

    fn stroke_color(&self, c: Color, width: f32, round: bool) {
        let [r, g, b, a] = c.to_f32();
        // SAFETY: the colour is created, set and released here.
        unsafe {
            if let Some(col) = Owned::new(CGColorCreateSRGB(f64::from(r), f64::from(g), f64::from(b), f64::from(a))) {
                CGContextSetStrokeColorWithColor(self.ctx, col.mut_ptr());
            }
            CGContextSetLineWidth(self.ctx, f64::from(width));
            CGContextSetLineCap(self.ctx, if round { kCGLineCapRound } else { kCGLineCapButt });
            CGContextSetLineJoin(self.ctx, if round { kCGLineJoinRound } else { kCGLineJoinMiter });
            CGContextSetLineDash(self.ctx, 0.0, std::ptr::null(), 0);
        }
    }

    /// Adds a rounded rectangle to the current path; nothing for an empty one. The radius is clamped to half the
    /// shorter side (Core Graphics refuses a larger one; Direct2D clamps it the same way).
    fn add_rrect(&self, r: CGRect, radius: f32) -> bool {
        if r.size.width <= 0.0 || r.size.height <= 0.0 {
            return false;
        }
        let radius = f64::from(radius).clamp(0.0, r.size.width.min(r.size.height) / 2.0);
        // SAFETY: the path is created, added and released here.
        unsafe {
            if let Some(path) = Owned::new(CGPathCreateWithRoundedRect(r, radius, radius, std::ptr::null())) {
                CGContextBeginPath(self.ctx);
                CGContextAddPath(self.ctx, path.get());
                return true;
            }
        }
        false
    }

    fn add_segs(&self, segs: &[Seg]) {
        // SAFETY: path building on the live context.
        unsafe {
            CGContextBeginPath(self.ctx);
            for s in segs {
                match *s {
                    Seg::Move(x, y) => CGContextMoveToPoint(self.ctx, f64::from(x), f64::from(y)),
                    Seg::Line(x, y) => CGContextAddLineToPoint(self.ctx, f64::from(x), f64::from(y)),
                    Seg::Cubic(a, b, c, d, x, y) => CGContextAddCurveToPoint(
                        self.ctx,
                        f64::from(a),
                        f64::from(b),
                        f64::from(c),
                        f64::from(d),
                        f64::from(x),
                        f64::from(y),
                    ),
                    Seg::Close => CGContextClosePath(self.ctx),
                }
            }
        }
    }

    fn stroke_segs(&self, segs: &[Seg], width: f32, c: Color) {
        self.stroke_color(c, width, true);
        self.add_segs(segs);
        // SAFETY: strokes the path just built.
        unsafe { CGContextStrokePath(self.ctx) };
    }
}

fn plain(r: Rect) -> CGRect {
    CGRect::new(f64::from(r.x), f64::from(r.y), f64::from(r.w), f64::from(r.h))
}

impl Painter for CgPainter<'_> {
    fn scale(&self) -> f32 {
        self.scale
    }

    fn clear(&self, c: Color) {
        let all = CGRect::new(0.0, 0.0, f64::from(self.size.0), f64::from(self.size.1));
        // SAFETY: plain drawing on the live context.
        unsafe { CGContextClearRect(self.ctx, all) };
        self.fill_color(c);
        // SAFETY: as above.
        unsafe { CGContextFillRect(self.ctx, all) };
    }

    fn fill_rect(&self, r: Rect, c: Color) {
        self.fill_color(c);
        // SAFETY: plain drawing on the live context.
        unsafe { CGContextFillRect(self.ctx, self.snapped(r)) };
    }

    fn fill_rrect(&self, r: Rect, radius: f32, c: Color) {
        self.fill_color(c);
        if self.add_rrect(self.snapped(r), radius) {
            // SAFETY: fills the path just built.
            unsafe { CGContextFillPath(self.ctx) };
        }
    }

    fn stroke_rrect(&self, r: Rect, radius: f32, width: f32, c: Color) {
        self.stroke_color(c, width, false);
        if self.add_rrect(plain(r), radius) {
            // SAFETY: strokes the path just built.
            unsafe { CGContextStrokePath(self.ctx) };
        }
    }

    fn stroke_rrect_dashed(&self, r: Rect, radius: f32, width: f32, c: Color) {
        self.stroke_color(c, width, false);
        // Direct2D's dash style: dashes and gaps two stroke widths long.
        let dash = [f64::from(width) * 2.0, f64::from(width) * 2.0];
        // SAFETY: the lengths outlive the call, which copies them.
        unsafe { CGContextSetLineDash(self.ctx, 0.0, dash.as_ptr(), 2) };
        if self.add_rrect(plain(r), radius) {
            // SAFETY: strokes the path just built.
            unsafe { CGContextStrokePath(self.ctx) };
        }
    }

    fn line(&self, from: (f32, f32), to: (f32, f32), width: f32, c: Color, round: bool) {
        self.stroke_color(c, width, round);
        self.add_segs(&[Seg::Move(from.0, from.1), Seg::Line(to.0, to.1)]);
        // SAFETY: strokes the path just built.
        unsafe { CGContextStrokePath(self.ctx) };
    }

    fn fill_circle(&self, center: (f32, f32), radius: f32, c: Color) {
        self.fill_color(c);
        let r = Rect::new(center.0 - radius, center.1 - radius, 2.0 * radius, 2.0 * radius);
        // SAFETY: plain drawing on the live context.
        unsafe { CGContextFillEllipseInRect(self.ctx, plain(r)) };
    }

    fn stroke_circle(&self, center: (f32, f32), radius: f32, width: f32, c: Color) {
        self.stroke_color(c, width, false);
        let r = Rect::new(center.0 - radius, center.1 - radius, 2.0 * radius, 2.0 * radius);
        // SAFETY: plain drawing on the live context.
        unsafe { CGContextStrokeEllipseInRect(self.ctx, plain(r)) };
    }

    fn arc_stroke(&self, center: (f32, f32), radius: f32, start_deg: f32, sweep_deg: f32, width: f32, c: Color) {
        self.stroke_segs(&svg::arc(center, radius, start_deg, sweep_deg), width, c);
    }

    fn fill_triangle(&self, p: [(f32, f32); 3], c: Color) {
        self.fill_color(c);
        self.add_segs(&[Seg::Move(p[0].0, p[0].1), Seg::Line(p[1].0, p[1].1), Seg::Line(p[2].0, p[2].1), Seg::Close]);
        // SAFETY: fills the path just built.
        unsafe { CGContextFillPath(self.ctx) };
    }

    fn svg(&self, d: &str, x: f32, y: f32, size: f32, width: f32, c: Color) {
        let s = size / 24.0;
        self.stroke_segs(&svg::parse(d, x, y, s), width * s, c);
    }

    /// The Windows painter's soft shadow: stacked translucent rounded rectangles.
    fn shadow(&self, r: Rect, radius: f32, dy: f32, blur: f32, c: Color) {
        let steps = 12;
        for i in 0..steps {
            let t = i as f32 / steps as f32;
            let grow = blur * (1.0 - t) * 0.5;
            let alpha = f32::from(c.a) / 255.0 * (0.9 / steps as f32) * (1.0 + t);
            let rr = Rect::new(r.x - grow, r.y + dy - grow, r.w + 2.0 * grow, r.h + 2.0 * grow);
            self.fill_color(c.with_alpha(alpha));
            if self.add_rrect(plain(rr), radius + grow) {
                // SAFETY: fills the path just built.
                unsafe { CGContextFillPath(self.ctx) };
            }
        }
    }

    /// An aliased clip, as Direct2D's: the rectangle snapped to whole device pixels.
    fn push_clip(&self, r: Rect) {
        // SAFETY: balanced with `pop_clip`.
        unsafe {
            CGContextSaveGState(self.ctx);
            CGContextClipToRect(self.ctx, self.snapped(r));
        }
    }

    fn pop_clip(&self) {
        // SAFETY: balances `push_clip`.
        unsafe { CGContextRestoreGState(self.ctx) };
    }

    fn content_height(&self, st: TextStyle) -> f32 {
        (self.fonts.ascent + self.fonts.descent) * st.size
    }

    fn layout(&self, text: &str, st: TextStyle, max_w: f32, align: Align, ellipsis: bool, bold_chars: usize) -> Label {
        self.fonts.layout(text, st, self.scale, max_w, align, ellipsis, bold_chars)
    }

    /// The baseline lands on a whole device pixel; across the line the glyphs keep their subpixel positions.
    fn draw(&self, l: &Label, x: f32, y: f32, c: Color) {
        let Some(ct) = l.handle::<CtLabel>() else { return };
        self.fill_color(c);
        let top = self.snap(y + l.baseline) - l.baseline;
        // SAFETY: drawing live lines on the live context; the text matrix flips glyphs back upright in the y-down CTM.
        unsafe {
            CGContextSetTextMatrix(self.ctx, CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: -1.0, tx: 0.0, ty: 0.0 });
            for (i, line) in ct.lines.iter().enumerate() {
                let base = top + l.baseline + i as f32 * ct.line_h;
                CGContextSetTextPosition(self.ctx, f64::from(x + line.dx), f64::from(base));
                CTLineDraw(line.line.get(), self.ctx);
            }
        }
    }
}

/// A frame painted offscreen: BGRA pixels (premultiplied) and what the frame registered.
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub output: Output,
    image: Option<Owned>,
}

impl Rendered {
    /// The number of distinct colours in the picture (a blank or failed frame has one or two).
    pub fn distinct_colors(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for px in self.pixels.as_chunks::<4>().0 {
            seen.insert([px[0], px[1], px[2]]);
            if seen.len() > 64 {
                break;
            }
        }
        seen.len()
    }

    /// Writes the picture as a PNG.
    pub fn save_png(&self, path: &Path) -> Result<(), String> {
        let image = self.image.as_ref().ok_or("no image")?;
        let bytes = path.as_os_str().as_encoded_bytes();
        // SAFETY: plain ImageIO calls on references created here; each is released once.
        unsafe {
            let url = Owned::new(CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as isize,
                0,
            ))
            .ok_or("url")?;
            let kind = cf_string("public.png").ok_or("type")?;
            let dest = Owned::new(CGImageDestinationCreateWithURL(url.get(), kind.get(), 1, std::ptr::null()))
                .ok_or_else(|| format!("cannot write {}", path.display()))?;
            CGImageDestinationAddImage(dest.mut_ptr(), image.mut_ptr(), std::ptr::null());
            if !CGImageDestinationFinalize(dest.mut_ptr()) {
                return Err(format!("could not finish {}", path.display()));
            }
        }
        Ok(())
    }
}

/// Paints the window `kind` of `state` at `size` DIPs and `scale` into a bitmap, through the shared screens.
pub fn render(
    fonts: &Fonts,
    state: &UiState,
    kind: WindowKind,
    size: (f32, f32),
    scale: f32,
    ix: &Interaction,
) -> Result<Rendered, String> {
    let (w, h) = ((size.0 * scale).round() as usize, (size.1 * scale).round() as usize);
    // SAFETY: a bitmap context made here, drawn on and read while it lives; every reference is released once.
    unsafe {
        let space = Owned::new(CGColorSpaceCreateWithName(kCGColorSpaceSRGB)).ok_or("colour space")?;
        let ctx = Owned::new(CGBitmapContextCreate(
            std::ptr::null_mut(),
            w,
            h,
            8,
            w * 4,
            space.mut_ptr(),
            BITMAP_BGRA_PREMULTIPLIED,
        ))
        .ok_or("bitmap context")?;
        let c = ctx.mut_ptr();
        // y down, DIPs to device pixels.
        CGContextTranslateCTM(c, 0.0, h as f64);
        CGContextScaleCTM(c, f64::from(scale), -f64::from(scale));
        let painter = CgPainter::new(c, fonts, scale, size);
        let frame = Frame::new(&painter, state, ix, size, true);
        let output = swapdesk_paint::paint(frame, kind);
        let row = CGBitmapContextGetBytesPerRow(c);
        let data = CGBitmapContextGetData(c).cast::<u8>();
        let mut pixels = vec![0u8; w * h * 4];
        for y in 0..h {
            std::ptr::copy_nonoverlapping(data.add(y * row), pixels.as_mut_ptr().add(y * w * 4), w * 4);
        }
        let image = Owned::new(CGBitmapContextCreateImage(c));
        Ok(Rendered { width: w as u32, height: h as u32, pixels, output, image })
    }
}
