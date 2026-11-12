//! Direct2D drawing helpers and DirectWrite text, in device-independent pixels (DIPs, 96 per inch).
//!
//! The render target carries the window DPI, so callers draw in the design's CSS pixels. Rectangle edges snap to the
//! device pixel grid, which keeps 1 px borders crisp at 125% and 150%. Grown from an early UI spike.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use swapdesk_ui::tokens::{self, Color, TextStyle};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_F, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_BEGIN_HOLLOW,
    D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    D2D1_ROUNDED_RECT, D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE, ID2D1Factory,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::*;
use windows::core::{BOOL, HSTRING, Interface, Result};

pub use swapdesk_paint::INSTRUMENT_SANS;

use swapdesk_paint::Painter;
pub use swapdesk_paint::{Align, Label, Rect};

/// Size, weight, line height, tracking and scale, as bits.
type FormatKey = (u32, u16, u32, u32, u8);

/// What a cached layout depends on: the text, the style (size, weight, line height, tracking), the width rounded to
/// device pixels, the alignment, trimming, bold prefix, the language and the scale.
type LayoutKey = (String, (u32, u16, u32, u32, u8), i32, Align, bool, u32, swapdesk_ui::Language, u32);

/// The layout cache is cleared when it holds more than this many entries.
const LAYOUT_CACHE_MAX: usize = 512;

/// The private Instrument Sans collection, or Segoe UI from the system when loading it fails.
pub struct Fonts {
    factory: IDWriteFactory,
    collection: IDWriteFontCollection,
    family: HSTRING,
    loader: Option<IDWriteInMemoryFontFileLoader>,
    /// Ascent and descent of the face as fractions of the em; CSS centres them in the line box.
    ascent: f32,
    descent: f32,
    /// The system's monospaced face (the terminal grid): Cascadia Mono, else Consolas, and its metrics.
    mono_collection: IDWriteFontCollection,
    mono_family: HSTRING,
    mono_ascent: f32,
    mono_descent: f32,
    formats: RefCell<HashMap<FormatKey, (IDWriteTextFormat, f32)>>,
    /// The ellipsis trimming sign of each text format, made once.
    signs: RefCell<HashMap<FormatKey, IDWriteInlineObject>>,
    /// Laid-out labels by [`LayoutKey`]: painting the same text again creates no `IDWriteTextLayout`.
    layouts: RefCell<HashMap<LayoutKey, Label>>,
    /// How many `IDWriteTextLayout`s were created (the cache's proof in tests).
    layouts_created: Cell<u64>,
    /// The UI language the formats were made for (their DirectWrite locale).
    language: Cell<swapdesk_ui::Language>,
}

/// The DirectWrite locale name of an interface language.
pub fn locale_name(lang: swapdesk_ui::Language) -> &'static str {
    match lang {
        swapdesk_ui::Language::PtBr => "pt-BR",
        swapdesk_ui::Language::En => "en-US",
    }
}

impl Fonts {
    /// Switches the language of the text formats; the cached formats are dropped when it changed, so the next
    /// layout builds them with the new locale. True when it changed.
    pub fn set_language(&self, lang: swapdesk_ui::Language) -> bool {
        if self.language.replace(lang) == lang {
            return false;
        }
        self.formats.borrow_mut().clear();
        self.signs.borrow_mut().clear();
        self.layouts.borrow_mut().clear();
        true
    }

    /// Drops every cached layout (a DPI or font change): the next paint lays the text out again.
    pub fn clear_layouts(&self) {
        self.layouts.borrow_mut().clear();
    }

    pub fn new() -> Result<Fonts> {
        // SAFETY: plain factory creation.
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let forced_system = std::env::var("SWAPDESK_FONT").as_deref() == Ok("system");
        let private = if forced_system { Err(windows::core::Error::empty()) } else { load_private(&factory) };
        let (collection, family, loader) = match private {
            Ok((collection, loader)) => (collection, HSTRING::from(tokens::FONT_FAMILY), Some(loader)),
            Err(_) => {
                let mut system = None;
                // SAFETY: `system` receives the shared system collection.
                unsafe { factory.GetSystemFontCollection(&mut system, false)? };
                (system.ok_or_else(windows::core::Error::empty)?, HSTRING::from(tokens::FALLBACK_FONT_FAMILY), None)
            }
        };
        let (ascent, descent) = face_metrics(&collection, &family).unwrap_or((1.0, 0.25));
        let mut system = None;
        // SAFETY: `system` receives the shared system collection.
        unsafe { factory.GetSystemFontCollection(&mut system, false)? };
        let mono_collection = system.ok_or_else(windows::core::Error::empty)?;
        let mono_family = HSTRING::from(
            ["Cascadia Mono", "Consolas"]
                .into_iter()
                .find(|f| has_family(&mono_collection, &HSTRING::from(*f)))
                .unwrap_or("Consolas"),
        );
        let (mono_ascent, mono_descent) = face_metrics(&mono_collection, &mono_family).unwrap_or((0.8, 0.2));
        Ok(Fonts {
            factory,
            collection,
            family,
            loader,
            ascent,
            descent,
            mono_collection,
            mono_family,
            mono_ascent,
            mono_descent,
            formats: RefCell::default(),
            signs: RefCell::default(),
            layouts: RefCell::default(),
            layouts_created: Cell::new(0),
            language: Cell::new(swapdesk_ui::Language::PtBr),
        })
    }

    /// The content area (ascent plus descent) of a style, as an inline box would have it.
    pub fn content_height(&self, st: TextStyle) -> f32 {
        let (ascent, descent) = self.metrics(st);
        (ascent + descent) * st.size
    }

    /// Ascent and descent (fractions of the em) of the face a style uses.
    fn metrics(&self, st: TextStyle) -> (f32, f32) {
        if st.mono { (self.mono_ascent, self.mono_descent) } else { (self.ascent, self.descent) }
    }

    /// A text format with CSS line spacing, and the baseline offset inside the line box in DIPs.
    fn format(&self, st: TextStyle, scale: f32) -> Result<(IDWriteTextFormat, f32)> {
        let key = (st.size.to_bits(), st.weight, st.line.to_bits(), scale.to_bits(), st.face_bits());
        if let Some(f) = self.formats.borrow().get(&key) {
            return Ok(f.clone());
        }
        let locale = HSTRING::from(locale_name(self.language.get()));
        let (family, collection) =
            if st.mono { (&self.mono_family, &self.mono_collection) } else { (&self.family, &self.collection) };
        let style = if st.italic { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL };
        // SAFETY: the collection, family name and locale outlive the call.
        let format = unsafe {
            self.factory.CreateTextFormat(
                family,
                collection,
                DWRITE_FONT_WEIGHT(i32::from(st.weight)),
                style,
                DWRITE_FONT_STRETCH_NORMAL,
                st.size,
                &locale,
            )?
        };
        // CSS puts the content area in the middle of the line box. Browsers lay text out in device pixels and round
        // the font's ascent and descent there, so the half-leading is worked out the same way.
        let (ascent, descent) = self.metrics(st);
        let (line_height, baseline) = swapdesk_paint::line_box(st, scale, ascent, descent);
        // SAFETY: plain setter on a format we own.
        unsafe { format.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, line_height, baseline)? };
        self.formats.borrow_mut().insert(key, (format.clone(), baseline));
        Ok((format, baseline))
    }

    /// The ellipsis trimming sign of a text format, created once per format.
    fn ellipsis_sign(&self, st: TextStyle, scale: f32, format: &IDWriteTextFormat) -> Result<IDWriteInlineObject> {
        let key = (st.size.to_bits(), st.weight, st.line.to_bits(), scale.to_bits(), st.face_bits());
        if let Some(sign) = self.signs.borrow().get(&key) {
            return Ok(sign.clone());
        }
        // SAFETY: `format` is a live text format of this factory.
        let sign = unsafe { self.factory.CreateEllipsisTrimmingSign(format)? };
        self.signs.borrow_mut().insert(key, sign.clone());
        Ok(sign)
    }

    /// Lays text out (cached); `max_w` wraps it (use a large value for one line). `ellipsis` trims one line with "…".
    #[allow(clippy::too_many_arguments)] // drawing helpers take the geometry and colour the CSS gives them
    pub fn layout(
        &self,
        text: &str,
        st: TextStyle,
        scale: f32,
        max_w: f32,
        align: Align,
        ellipsis: bool,
        bold_prefix: u32,
    ) -> Result<Label> {
        let key: LayoutKey = (
            text.to_owned(),
            (st.size.to_bits(), st.weight, st.line.to_bits(), st.tracking.to_bits(), st.face_bits()),
            (max_w * scale).round() as i32,
            align,
            ellipsis,
            bold_prefix,
            self.language.get(),
            scale.to_bits(),
        );
        if let Some(hit) = self.layouts.borrow().get(&key) {
            return Ok(hit.clone());
        }
        let label = self.make_layout(text, st, scale, max_w, align, ellipsis, bold_prefix)?;
        let mut cache = self.layouts.borrow_mut();
        if cache.len() >= LAYOUT_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, label.clone());
        Ok(label)
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
        bold_prefix: u32,
    ) -> Result<Label> {
        let (format, baseline) = self.format(st, scale)?;
        self.layouts_created.set(self.layouts_created.get() + 1);
        let wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: `wide` and `format` outlive the calls; the metrics struct is written by `GetMetrics`.
        unsafe {
            let layout = self.factory.CreateTextLayout(&wide, &format, max_w, 10_000.0)?;
            match align {
                Align::Start => {}
                Align::Center => layout.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?,
            }
            if ellipsis {
                layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                let sign = self.ellipsis_sign(st, scale, &format)?;
                let trimming = DWRITE_TRIMMING {
                    granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                    delimiter: 0,
                    delimiterCount: 0,
                };
                layout.SetTrimming(&trimming, &sign)?;
            }
            if bold_prefix > 0 {
                let range = DWRITE_TEXT_RANGE { startPosition: 0, length: bold_prefix.min(wide.len() as u32) };
                layout.SetFontWeight(DWRITE_FONT_WEIGHT_SEMI_BOLD, range)?;
            }
            if st.tracking != 0.0 {
                let layout1: IDWriteTextLayout1 = layout.cast()?;
                let range = DWRITE_TEXT_RANGE { startPosition: 0, length: wide.len() as u32 };
                layout1.SetCharacterSpacing(0.0, st.tracking * st.size, 0.0, range)?;
            }
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            // Browsers keep layout sizes in 1/64 device pixels, rounded down; sums of them decide where rows land.
            let h = swapdesk_paint::label_height(m.height, scale);
            Ok(Label::new(m.width, h, baseline, layout))
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        if let Some(loader) = &self.loader {
            // SAFETY: the loader was registered on this factory in `load_private`.
            unsafe {
                let _ = self.factory.UnregisterFontFileLoader(loader);
            }
        }
    }
}

fn load_private(factory: &IDWriteFactory) -> Result<(IDWriteFontCollection, IDWriteInMemoryFontFileLoader)> {
    let factory5: IDWriteFactory5 = factory.cast()?;
    // SAFETY: the font bytes are a `'static` slice, so they outlive every face made from them.
    unsafe {
        let loader = factory5.CreateInMemoryFontFileLoader()?;
        factory.RegisterFontFileLoader(&loader)?;
        let built = (|| -> Result<IDWriteFontCollection> {
            let file = loader.CreateInMemoryFontFileReference(
                factory,
                INSTRUMENT_SANS.as_ptr().cast(),
                INSTRUMENT_SANS.len() as u32,
                None,
            )?;
            let builder = factory5.CreateFontSetBuilder()?;
            builder.AddFontFile(&file)?;
            let set = builder.CreateFontSet()?;
            let collection: IDWriteFontCollection = factory5.CreateFontCollectionFromFontSet(&set)?.cast()?;
            let mut index = 0;
            let mut exists = BOOL(0);
            collection.FindFamilyName(&HSTRING::from(tokens::FONT_FAMILY), &mut index, &mut exists)?;
            if exists.as_bool() { Ok(collection) } else { Err(windows::core::Error::empty()) }
        })();
        match built {
            Ok(collection) => Ok((collection, loader)),
            Err(e) => {
                let _ = factory.UnregisterFontFileLoader(&loader);
                Err(e)
            }
        }
    }
}

/// Whether the collection holds a family of that name (`FindFamilyName` answers index 0 for a missing one).
fn has_family(collection: &IDWriteFontCollection, family: &HSTRING) -> bool {
    let mut index = 0;
    let mut exists = BOOL(0);
    // SAFETY: out-pointers are valid locals.
    unsafe { collection.FindFamilyName(family, &mut index, &mut exists) }.is_ok() && exists.as_bool()
}

fn face_metrics(collection: &IDWriteFontCollection, family: &HSTRING) -> Result<(f32, f32)> {
    let mut index = 0;
    let mut exists = BOOL(0);
    // SAFETY: out-pointers are valid locals; the metrics struct is written by `GetMetrics`.
    unsafe {
        collection.FindFamilyName(family, &mut index, &mut exists)?;
        let font = collection.GetFontFamily(index)?.GetFirstMatchingFont(
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
        )?;
        let mut m = DWRITE_FONT_METRICS::default();
        font.GetMetrics(&mut m);
        let em = f32::from(m.designUnitsPerEm);
        Ok((f32::from(m.ascent) / em, f32::from(m.descent) / em))
    }
}

/// A Direct2D point. The bindings type it as `windows_numerics::Vector2`, which `windows` does not re-export, so it
/// is built by field through a struct that holds one instead of adding the crate as a direct dependency.
macro_rules! point {
    ($x:expr, $y:expr) => {{
        let mut e = D2D1_ELLIPSE::default();
        e.point.X = $x;
        e.point.Y = $y;
        e.point
    }};
}

pub fn d2d_color(c: Color) -> D2D1_COLOR_F {
    let [r, g, b, a] = c.to_f32();
    D2D1_COLOR_F { r, g, b, a }
}

fn rounded(rect: D2D_RECT_F, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect, radiusX: radius, radiusY: radius }
}

/// Stroke styles every painter shares (made once per window set).
pub struct Strokes {
    pub round: ID2D1StrokeStyle,
    pub dashed: ID2D1StrokeStyle,
}

/// Draws one frame onto a render target whose DPI is already set.
pub struct D2dPainter<'a> {
    rt: &'a ID2D1RenderTarget,
    factory: &'a ID2D1Factory,
    strokes: &'a Strokes,
    brush: ID2D1SolidColorBrush,
    pub fonts: &'a Fonts,
    pub scale: f32,
}

impl<'a> D2dPainter<'a> {
    pub fn new(
        rt: &'a ID2D1RenderTarget,
        factory: &'a ID2D1Factory,
        strokes: &'a Strokes,
        fonts: &'a Fonts,
        scale: f32,
    ) -> Result<D2dPainter<'a>> {
        // SAFETY: plain resource creation on a live render target.
        let brush = unsafe { rt.CreateSolidColorBrush(&D2D1_COLOR_F::default(), None)? };
        Ok(D2dPainter { rt, factory, strokes, brush, fonts, scale })
    }

    fn snapped(&self, r: Rect) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.snap(r.x),
            top: self.snap(r.y),
            right: self.snap(r.right()),
            bottom: self.snap(r.bottom()),
        }
    }

    fn set(&self, c: Color) {
        // SAFETY: plain setter on our brush.
        unsafe { self.brush.SetColor(&d2d_color(c)) };
    }

    /// Builds a geometry in the closure and strokes or fills it as the closure says: (colour, width, fill).
    fn path(
        &self,
        build: impl FnOnce(&windows::Win32::Graphics::Direct2D::ID2D1GeometrySink) -> (Color, f32, bool),
    ) -> Result<()> {
        // SAFETY: the geometry and sink are local; the sink is closed before the geometry is used.
        unsafe {
            let geometry = self.factory.CreatePathGeometry()?;
            let sink = geometry.Open()?;
            let (color, width, fill) = build(&sink);
            sink.Close()?;
            self.set(color);
            if fill {
                self.rt.FillGeometry(&geometry, &self.brush, None);
            } else {
                self.rt.DrawGeometry(&geometry, &self.brush, width, Some(&self.strokes.round));
            }
        }
        Ok(())
    }
}

impl Painter for D2dPainter<'_> {
    fn scale(&self) -> f32 {
        self.scale
    }

    fn content_height(&self, st: TextStyle) -> f32 {
        self.fonts.content_height(st)
    }

    fn clear(&self, c: Color) {
        // SAFETY: called between BeginDraw and EndDraw by the window.
        unsafe { self.rt.Clear(Some(&d2d_color(c))) };
    }

    fn fill_rect(&self, r: Rect, c: Color) {
        self.set(c);
        // SAFETY: valid rect and brush.
        unsafe { self.rt.FillRectangle(&self.snapped(r), &self.brush) };
    }

    fn fill_rrect(&self, r: Rect, radius: f32, c: Color) {
        self.set(c);
        let rect = rounded(self.snapped(r), radius);
        // SAFETY: valid rounded rect and brush.
        unsafe { self.rt.FillRoundedRectangle(&rect, &self.brush) };
    }

    /// A stroke centred on the rectangle's edge, like an SVG or a CSS outline of the same width.
    fn stroke_rrect(&self, r: Rect, radius: f32, width: f32, c: Color) {
        self.set(c);
        let rect = rounded(D2D_RECT_F { left: r.x, top: r.y, right: r.right(), bottom: r.bottom() }, radius);
        // SAFETY: valid rounded rect and brush.
        unsafe { self.rt.DrawRoundedRectangle(&rect, &self.brush, width, None) };
    }

    /// A dashed rounded border (the file drop zone).
    fn stroke_rrect_dashed(&self, r: Rect, radius: f32, width: f32, c: Color) {
        self.set(c);
        let rect = rounded(D2D_RECT_F { left: r.x, top: r.y, right: r.right(), bottom: r.bottom() }, radius);
        // SAFETY: valid rounded rect, brush and stroke style.
        unsafe { self.rt.DrawRoundedRectangle(&rect, &self.brush, width, Some(&self.strokes.dashed)) };
    }

    fn line(&self, from: (f32, f32), to: (f32, f32), width: f32, c: Color, round: bool) {
        self.set(c);
        let (a, b) = (point!(from.0, from.1), point!(to.0, to.1));
        // SAFETY: valid points, brush and (optional) stroke style.
        unsafe { self.rt.DrawLine(a, b, &self.brush, width, if round { Some(&self.strokes.round) } else { None }) };
    }

    fn fill_circle(&self, center: (f32, f32), radius: f32, c: Color) {
        self.set(c);
        let e = D2D1_ELLIPSE { point: point!(center.0, center.1), radiusX: radius, radiusY: radius };
        // SAFETY: valid ellipse and brush.
        unsafe { self.rt.FillEllipse(&e, &self.brush) };
    }

    fn stroke_circle(&self, center: (f32, f32), radius: f32, width: f32, c: Color) {
        self.set(c);
        let e = D2D1_ELLIPSE { point: point!(center.0, center.1), radiusX: radius, radiusY: radius };
        // SAFETY: valid ellipse and brush.
        unsafe { self.rt.DrawEllipse(&e, &self.brush, width, None) };
    }

    /// A spinner arc: 3/4 of a circle starting at `start` degrees (the "Reconectando" icon).
    fn arc_stroke(&self, center: (f32, f32), radius: f32, start_deg: f32, sweep_deg: f32, width: f32, c: Color) {
        let pt = |deg: f32| {
            let r = deg.to_radians();
            (center.0 + radius * r.cos(), center.1 + radius * r.sin())
        };
        let (a, b) = (pt(start_deg), pt(start_deg + sweep_deg));
        let _ = self.path(|sink| {
            // SAFETY: the sink is open for the duration of the closure.
            unsafe {
                sink.BeginFigure(point!(a.0, a.1), D2D1_FIGURE_BEGIN_HOLLOW);
                let mut seg = D2D1_ARC_SEGMENT::default();
                seg.point.X = b.0;
                seg.point.Y = b.1;
                seg.size = D2D_SIZE_F { width: radius, height: radius };
                seg.sweepDirection = D2D1_SWEEP_DIRECTION_CLOCKWISE;
                seg.arcSize = if sweep_deg > 180.0 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL };
                sink.AddArc(&seg);
                sink.EndFigure(D2D1_FIGURE_END_OPEN);
            }
            (c, width, false)
        });
    }

    /// A filled triangle (the wire's arrowhead).
    fn fill_triangle(&self, p: [(f32, f32); 3], c: Color) {
        let _ = self.path(|sink| {
            // SAFETY: the sink is open for the duration of the closure.
            unsafe {
                sink.BeginFigure(point!(p[0].0, p[0].1), D2D1_FIGURE_BEGIN_FILLED);
                sink.AddLine(point!(p[1].0, p[1].1));
                sink.AddLine(point!(p[2].0, p[2].1));
                sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            }
            (c, 0.0, true)
        });
    }

    /// Draws an SVG path (`d` attribute: M L H V C A Z, absolute and relative) from a 24-unit viewBox at `(x, y)`,
    /// scaled to `size` DIPs, stroked with `width` (in viewBox units).
    fn svg(&self, d: &str, x: f32, y: f32, size: f32, width: f32, c: Color) {
        let s = size / 24.0;
        let _ = self.path(|sink| {
            // SAFETY: the sink is open for the duration of the closure.
            unsafe { svg_into(sink, d, x, y, s) };
            (c, width * s, false)
        });
    }

    /// A soft shadow under a rounded rectangle: stacked translucent rounded rectangles.
    fn shadow(&self, r: Rect, radius: f32, dy: f32, blur: f32, c: Color) {
        let steps = 12;
        for i in 0..steps {
            let t = i as f32 / steps as f32;
            let grow = blur * (1.0 - t) * 0.5;
            let alpha = f32::from(c.a) / 255.0 * (0.9 / steps as f32) * (1.0 + t);
            let rr = Rect::new(r.x - grow, r.y + dy - grow, r.w + 2.0 * grow, r.h + 2.0 * grow);
            self.set(c.with_alpha(alpha));
            let rect =
                rounded(D2D_RECT_F { left: rr.x, top: rr.y, right: rr.right(), bottom: rr.bottom() }, radius + grow);
            // SAFETY: valid rounded rect and brush.
            unsafe { self.rt.FillRoundedRectangle(&rect, &self.brush) };
        }
    }

    fn push_clip(&self, r: Rect) {
        let rect = D2D_RECT_F { left: r.x, top: r.y, right: r.right(), bottom: r.bottom() };
        // SAFETY: balanced with `pop_clip`.
        unsafe { self.rt.PushAxisAlignedClip(&rect, windows::Win32::Graphics::Direct2D::D2D1_ANTIALIAS_MODE_ALIASED) };
    }

    fn pop_clip(&self) {
        // SAFETY: balances `push_clip`.
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    fn layout(&self, text: &str, st: TextStyle, max_w: f32, align: Align, ellipsis: bool, bold_chars: usize) -> Label {
        let bold_utf16 = text.chars().take(bold_chars).map(char::len_utf16).sum::<usize>() as u32;
        // A failed layout draws nothing instead of failing the frame.
        self.fonts.layout(text, st, self.scale, max_w, align, ellipsis, bold_utf16).unwrap_or_else(|_| {
            self.fonts.layout("", st, self.scale, max_w, align, false, 0).expect("an empty layout always succeeds")
        })
    }

    /// Draws a label with its line box starting at `(x, y)`. The baseline lands on a whole device pixel; across the
    /// line the glyphs keep their subpixel positions, as in a browser.
    fn draw(&self, l: &Label, x: f32, y: f32, c: Color) {
        self.set(c);
        let top = self.snap(y + l.baseline) - l.baseline;
        let origin = point!(x, top);
        let Some(layout) = l.handle::<IDWriteTextLayout>() else { return };
        // SAFETY: valid layout and brush.
        unsafe { self.rt.DrawTextLayout(origin, layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE) };
    }
}

/// Feeds an SVG path into a geometry sink. Unknown commands end the path.
///
/// # Safety
/// `sink` must be open.
unsafe fn svg_into(sink: &windows::Win32::Graphics::Direct2D::ID2D1GeometrySink, d: &str, ox: f32, oy: f32, s: f32) {
    let mut nums: Vec<f32> = Vec::new();
    let mut cmd = b'M';
    let (mut cx, mut cy) = (0.0f32, 0.0f32);
    let (mut sx, mut sy) = (0.0f32, 0.0f32);
    let mut open = false;
    let pt = |x: f32, y: f32| point!(ox + x * s, oy + y * s);

    // Tokenize into commands and numbers.
    let bytes = d.as_bytes();
    let mut i = 0;
    let mut tokens: Vec<(Option<u8>, f32)> = Vec::new();
    while i < bytes.len() {
        let ch = bytes[i];
        if ch.is_ascii_alphabetic() {
            tokens.push((Some(ch), 0.0));
            i += 1;
        } else if ch == b'-' || ch == b'.' || ch.is_ascii_digit() {
            let start = i;
            i += 1;
            let mut seen_dot = ch == b'.';
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_digit() {
                    i += 1;
                } else if c == b'.' && !seen_dot {
                    seen_dot = true;
                    i += 1;
                } else {
                    break;
                }
            }
            tokens.push((None, d[start..i].parse().unwrap_or(0.0)));
        } else {
            i += 1;
        }
    }

    let arity = |c: u8| match c.to_ascii_lowercase() {
        b'm' | b'l' => 2,
        b'h' | b'v' => 1,
        b'c' => 6,
        b'a' => 7,
        _ => 0,
    };
    let mut idx = 0;
    while idx < tokens.len() {
        if let (Some(c), _) = tokens[idx] {
            cmd = c;
            idx += 1;
            if matches!(c, b'z' | b'Z') {
                if open {
                    // SAFETY: the figure was begun.
                    unsafe { sink.EndFigure(D2D1_FIGURE_END_CLOSED) };
                    open = false;
                }
                cx = sx;
                cy = sy;
                continue;
            }
        }
        let n = arity(cmd);
        if n == 0 {
            break;
        }
        nums.clear();
        while nums.len() < n && idx < tokens.len() && tokens[idx].0.is_none() {
            nums.push(tokens[idx].1);
            idx += 1;
        }
        if nums.len() < n {
            break;
        }
        let rel = cmd.is_ascii_lowercase();
        let (bx, by) = if rel { (cx, cy) } else { (0.0, 0.0) };
        // SAFETY: figures are begun before segments are added and ended before the next begins.
        unsafe {
            match cmd.to_ascii_lowercase() {
                b'm' => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    cx = bx + nums[0];
                    cy = by + nums[1];
                    sx = cx;
                    sy = cy;
                    sink.BeginFigure(pt(cx, cy), D2D1_FIGURE_BEGIN_HOLLOW);
                    open = true;
                    // Further pairs after a move are implicit line-tos.
                    cmd = if rel { b'l' } else { b'L' };
                }
                b'l' => {
                    cx = bx + nums[0];
                    cy = by + nums[1];
                    sink.AddLine(pt(cx, cy));
                }
                b'h' => {
                    cx = bx + nums[0];
                    sink.AddLine(pt(cx, cy));
                }
                b'v' => {
                    cy = by + nums[0];
                    sink.AddLine(pt(cx, cy));
                }
                b'c' => {
                    let mut seg = D2D1_BEZIER_SEGMENT::default();
                    seg.point1.X = ox + (bx + nums[0]) * s;
                    seg.point1.Y = oy + (by + nums[1]) * s;
                    seg.point2.X = ox + (bx + nums[2]) * s;
                    seg.point2.Y = oy + (by + nums[3]) * s;
                    cx = bx + nums[4];
                    cy = by + nums[5];
                    seg.point3.X = ox + cx * s;
                    seg.point3.Y = oy + cy * s;
                    sink.AddBezier(&seg);
                }
                b'a' => {
                    let mut seg = D2D1_ARC_SEGMENT::default();
                    cx = bx + nums[5];
                    cy = by + nums[6];
                    seg.point.X = ox + cx * s;
                    seg.point.Y = oy + cy * s;
                    seg.size = D2D_SIZE_F { width: nums[0] * s, height: nums[1] * s };
                    seg.rotationAngle = nums[2];
                    seg.arcSize = if nums[3] != 0.0 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL };
                    seg.sweepDirection = if nums[4] != 0.0 {
                        D2D1_SWEEP_DIRECTION_CLOCKWISE
                    } else {
                        D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                    };
                    sink.AddArc(&seg);
                }
                _ => {}
            }
        }
    }
    if open {
        // SAFETY: the figure was begun.
        unsafe { sink.EndFigure(D2D1_FIGURE_END_OPEN) };
    }
}

#[cfg(test)]
mod tests {
    use swapdesk_ui::Language;

    use super::*;

    /// The Mac reads the face's metrics from the font file; they must be the ones DirectWrite uses here.
    #[test]
    fn the_shared_face_metrics_are_directwrites() {
        let fonts = Fonts::new().expect("fonts");
        let (a, d) = swapdesk_paint::face_metrics(INSTRUMENT_SANS).expect("metrics");
        assert_eq!((fonts.ascent, fonts.descent), (a, d));
    }

    #[test]
    fn locale_follows_the_interface_language() {
        assert_eq!(locale_name(Language::PtBr), "pt-BR");
        assert_eq!(locale_name(Language::En), "en-US");
    }

    /// Two identical paints of a mock scenario: the second creates no text layout and draws the same pixels.
    #[test]
    fn a_second_identical_paint_creates_no_text_layout() {
        use crate::offscreen::{design_size, render_with};
        use swapdesk_paint::Interaction;
        use swapdesk_ui::WindowKind;
        use swapdesk_ui::mock::Scenario;
        // SAFETY: COM initialisation of the test thread (WIC needs it); a repeated call is harmless.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        let fonts = Fonts::new().expect("fonts");
        let state = Scenario::Dispositivos.prepared_state();
        let ix = Interaction::default();
        let size = design_size(WindowKind::Main);
        let first = render_with(&fonts, &state, WindowKind::Main, size, 1.0, &ix).expect("first paint");
        let created = fonts.layouts_created.get();
        assert!(created > 10, "the first paint lays text out ({created})");
        let second = render_with(&fonts, &state, WindowKind::Main, size, 1.0, &ix).expect("second paint");
        assert_eq!(
            fonts.layouts_created.get(),
            created,
            "the second paint created {} new layouts",
            fonts.layouts_created.get() - created
        );
        assert_eq!(first.pixels, second.pixels, "cached layouts draw the same picture");
        // A DPI change is another key (and clearing drops them all).
        render_with(&fonts, &state, WindowKind::Main, size, 1.5, &ix).expect("scaled paint");
        assert!(fonts.layouts_created.get() > created);
        fonts.clear_layouts();
        assert!(fonts.layouts.borrow().is_empty());
    }

    #[test]
    fn the_layout_cache_is_bounded_and_trimming_signs_are_made_once() {
        let fonts = Fonts::new().expect("fonts");
        let st = swapdesk_ui::tokens::TextStyle::new(14.0, 400).line(1.4);
        for i in 0..(LAYOUT_CACHE_MAX + 40) {
            fonts.layout(&format!("row {i}"), st, 1.0, 100.0, Align::Start, true, 0).expect("layout");
        }
        assert!(fonts.layouts.borrow().len() <= LAYOUT_CACHE_MAX, "bounded: {}", fonts.layouts.borrow().len());
        assert_eq!(fonts.signs.borrow().len(), 1, "one ellipsis sign for one format, not one per label");
    }

    #[test]
    fn the_monospaced_face_gives_every_glyph_one_advance_and_has_formats_of_its_own() {
        let fonts = Fonts::new().expect("fonts");
        let mono = swapdesk_ui::tokens::text::TERM;
        let ui = swapdesk_ui::tokens::TextStyle::new(13.0, 400).line(1.3);
        let width = |t: &str, st| fonts.layout(t, st, 1.0, 1000.0, Align::Start, false, 0).expect("layout").w;
        let (i, m) = (width("iiii", mono), width("mmmm", mono));
        assert!(i > 0.0 && (i - m).abs() < 0.01, "monospaced: {i} against {m}");
        assert!(width("mmmm", ui) > width("iiii", ui) + 1.0, "the interface's face is proportional");
        fonts.layout("x", mono.italic(), 1.0, 1000.0, Align::Start, false, 0).expect("italic layout");
        assert_eq!(fonts.formats.borrow().len(), 3, "the interface's, the monospaced and its italic");
        assert!(fonts.content_height(mono) > 0.0);
        assert!(
            ["Cascadia Mono", "Consolas"].contains(&fonts.mono_family.to_string().as_str()),
            "{}",
            fonts.mono_family
        );
    }

    #[test]
    fn changing_language_drops_the_cached_formats() {
        let fonts = Fonts::new().expect("fonts");
        let st = swapdesk_ui::tokens::TextStyle::new(14.0, 400).line(1.4);
        fonts.layout("a", st, 1.0, 100.0, Align::Start, false, 0).expect("layout");
        assert_eq!(fonts.formats.borrow().len(), 1);
        assert!(!fonts.set_language(Language::PtBr), "same language keeps the cache");
        assert_eq!(fonts.formats.borrow().len(), 1);
        assert!(fonts.set_language(Language::En));
        assert!(fonts.formats.borrow().is_empty());
        assert!(fonts.layouts.borrow().is_empty() && fonts.signs.borrow().is_empty(), "layouts follow the language");
        let (format, _) = fonts.format(st, 1.0).expect("format");
        // SAFETY: reads the locale name into a buffer that outlives the call.
        let mut buf = [0u16; 32];
        unsafe { format.GetLocaleName(&mut buf).expect("locale") };
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        // DirectWrite hands the name back lower-cased.
        assert!(String::from_utf16_lossy(&buf[..n]).eq_ignore_ascii_case("en-US"));
    }
}
