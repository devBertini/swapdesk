//! The terminal panel's drawing (stage 20): a cell grid on a Direct2D render target with DirectWrite, in the
//! viewer's DIPs and per-monitor scale, like the rest of the shell.
//!
//! It draws any [`CellGrid`]; [`TermGrid`] is the `swapdesk-termview` `Screen` as one. [`TerminalPane`] is the panel of
//! the viewer's drawer (stage 43): the model of the open shell, its output router, selection, clipboard and the keys.
//! The panel needs a monospace face the
//! shell's Instrument Sans collection does not have, so it owns a small DirectWrite factory with Cascadia Mono (Consolas
//! when absent) in the four weight/slant combinations, made once. Backgrounds go as merged spans, text as one layout
//! per run of equal style, and a damage list (`rows`) limits a frame to the rows that changed (R-UI: nothing when idle).

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use swapdesk_termview::{Key, Mods, OUTPUT_CHANNEL_CHUNKS, OutputTx, TermView, encode_key, encode_paste};
use swapdesk_ui::tokens::{Color, fixed};
use swapdesk_ui::{Command, ControlId, TermPhase, UiWaker, WindowKind};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_ELLIPSE,
    ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_LINE_SPACING_METHOD_DEFAULT, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::WindowsAndMessaging::{
    WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP,
};
use windows::core::{BOOL, HSTRING, Result, w};

use crate::app::Shared;
use crate::gfx::Rect;
use crate::system::{get_x, get_y, hiword};
use crate::window::Window;

/// Font size of the grid in DIPs (the design's MONO style).
const FONT_SIZE: f32 = 13.0;
/// Space between the panel's edge and the grid.
pub const PADDING: f32 = 8.0;
/// Largest grid the panel asks for (the protocol caps a terminal at 1000 x 1000).
const MAX_CELLS: usize = 1000;
/// Selected cells: the design's link blue, dark.
const SELECTION: Color = Color::rgb(0x2E3C86);

/// A colour as the terminal program set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Paint {
    #[default]
    Default,
    /// 0 to 15 the ANSI colours, 16 to 231 the 6x6x6 cube, 232 to 255 the grey ramp.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// One cell as the panel draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridCell {
    pub ch: char,
    pub fg: Paint,
    pub bg: Paint,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// 1, 2 for the left half of a wide glyph, 0 for the spacer after it (not drawn).
    pub width: u8,
}

impl GridCell {
    #[cfg(test)]
    pub const BLANK: GridCell = GridCell {
        ch: ' ',
        fg: Paint::Default,
        bg: Paint::Default,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        strike: false,
        inverse: false,
        hidden: false,
        width: 1,
    };
}

/// What the panel reads from the terminal model.
pub trait CellGrid {
    fn cols(&self) -> usize;
    fn rows(&self) -> usize;
    /// The cell at a viewport row and column (a blank outside the grid).
    fn cell(&self, row: usize, col: usize) -> GridCell;
    /// The cursor's `(row, col)` when it is shown.
    fn cursor(&self) -> Option<(usize, usize)>;
    fn selected(&self, row: usize, col: usize) -> bool;
}

const ANSI: [u32; 16] = [
    0x2B303B, 0xE06C75, 0x98C379, 0xE5C07B, 0x61AFEF, 0xC678DD, 0x56B6C2, 0xD6DAE0, 0x5C6370, 0xFF7A6E, 0x4FC28E,
    0xF0D08A, 0x8C9BFF, 0xD99BEB, 0x7FD3DE, 0xFFFFFF,
];

/// The colour of a paint; `fg` says which default applies.
pub fn paint_color(p: Paint, fg: bool) -> Color {
    match p {
        Paint::Default if fg => fixed::TERMINAL_INK,
        Paint::Default => fixed::TERMINAL_BG,
        Paint::Rgb(r, g, b) => Color { r, g, b, a: 255 },
        Paint::Indexed(i) if i < 16 => Color::rgb(ANSI[usize::from(i)]),
        Paint::Indexed(i) if i < 232 => {
            let v = i - 16;
            let level = |c: u8| if c == 0 { 0 } else { 55 + 40 * c };
            Color { r: level(v / 36), g: level((v / 6) % 6), b: level(v % 6), a: 255 }
        }
        Paint::Indexed(i) => {
            let g = 8 + 10 * (i - 232);
            Color { r: g, g, b: g, a: 255 }
        }
    }
}

/// Mixes `a` toward `b` by `t` (0 = a).
fn mix(a: Color, b: Color, t: f32) -> Color {
    let m = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    Color { r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b), a: 255 }
}

/// A cell after inverse, dim, selection and cursor are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resolved {
    ch: char,
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    width: u8,
}

fn resolve(c: &GridCell, selected: bool, block_cursor: bool) -> Resolved {
    let (mut fg, mut bg) = (paint_color(c.fg, true), paint_color(c.bg, false));
    if c.inverse {
        core::mem::swap(&mut fg, &mut bg);
    }
    if c.dim {
        fg = mix(fg, bg, 0.45);
    }
    if selected {
        bg = SELECTION;
    }
    if block_cursor {
        (fg, bg) = (fixed::TERMINAL_BG, fixed::TERMINAL_INK);
    }
    if c.hidden {
        fg = bg;
    }
    Resolved {
        ch: c.ch,
        fg,
        bg,
        bold: c.bold,
        italic: c.italic,
        underline: c.underline,
        strike: c.strike,
        width: c.width,
    }
}

fn d2d(c: Color) -> D2D1_COLOR_F {
    let [r, g, b, a] = c.to_f32();
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

/// A Direct2D point; see `gfx.rs` for why it is built through a struct that holds one.
macro_rules! point {
    ($x:expr, $y:expr) => {{
        let mut e = D2D1_ELLIPSE::default();
        e.point.X = $x;
        e.point.Y = $y;
        e.point
    }};
}

pub struct TerminalRenderer {
    factory: IDWriteFactory,
    /// Index: bit 0 bold, bit 1 italic.
    formats: [IDWriteTextFormat; 4],
    /// Advance of one cell and height of one line, in DIPs, as the font measures them.
    cell: (f32, f32),
}

impl TerminalRenderer {
    pub fn new() -> Result<TerminalRenderer> {
        // SAFETY: plain factory creation.
        let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let mut collection = None;
        // SAFETY: `collection` receives the shared system collection.
        unsafe { factory.GetSystemFontCollection(&mut collection, false)? };
        let collection: IDWriteFontCollection = collection.ok_or_else(windows::core::Error::empty)?;
        let family =
            ["Cascadia Mono", "Consolas"].into_iter().find(|f| has_family(&collection, f)).unwrap_or("Consolas");
        let make = |bold: bool, italic: bool| -> Result<IDWriteTextFormat> {
            // SAFETY: the collection and the family name outlive the call.
            let format = unsafe {
                factory.CreateTextFormat(
                    &HSTRING::from(family),
                    &collection,
                    if bold { DWRITE_FONT_WEIGHT_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL },
                    if italic { DWRITE_FONT_STYLE_ITALIC } else { DWRITE_FONT_STYLE_NORMAL },
                    DWRITE_FONT_STRETCH_NORMAL,
                    FONT_SIZE,
                    w!("en-us"),
                )?
            };
            // SAFETY: plain setters on a format made here.
            unsafe {
                format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                format.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_DEFAULT, 0.0, 0.0)?;
            }
            Ok(format)
        };
        let formats = [make(false, false)?, make(true, false)?, make(false, true)?, make(true, true)?];
        // One "M" tells the advance of a cell and the height of a line.
        let probe: Vec<u16> = "M".encode_utf16().collect();
        // SAFETY: `probe` and the format outlive the calls; the metrics struct is written by `GetMetrics`.
        let (cw, ch) = unsafe {
            let layout = factory.CreateTextLayout(&probe, &formats[0], 1000.0, 1000.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            (m.widthIncludingTrailingWhitespace, m.height)
        };
        Ok(TerminalRenderer { factory, formats, cell: (cw, ch) })
    }

    /// One cell in DIPs: the font's advance, and a line height rounded up to whole device pixels at `scale`.
    pub fn cell_size(&self, scale: f32) -> (f32, f32) {
        (self.cell.0, (self.cell.1 * scale).ceil() / scale)
    }

    /// How many columns and rows fit an area `w` x `h` DIPs, padding included; at least 1 x 1.
    pub fn grid_for(&self, w: f32, h: f32, scale: f32) -> (usize, usize) {
        let (cw, ch) = self.cell_size(scale);
        let cols = ((w - 2.0 * PADDING) / cw).floor().max(1.0) as usize;
        let rows = ((h - 2.0 * PADDING) / ch).floor().max(1.0) as usize;
        (cols.min(MAX_CELLS), rows.min(MAX_CELLS))
    }

    /// The `(row, col)` under a point of the window, clamped to the grid (so a drag past the edge still extends).
    pub fn cell_at(&self, area: Rect, scale: f32, grid: (usize, usize), x: f32, y: f32) -> (usize, usize) {
        let (cw, ch) = self.cell_size(scale);
        let col = ((x - area.x - PADDING) / cw).floor().max(0.0) as usize;
        let row = ((y - area.y - PADDING) / ch).floor().max(0.0) as usize;
        (row.min(grid.1.saturating_sub(1)), col.min(grid.0.saturating_sub(1)))
    }

    /// Draws `grid` into `area`. `rows` limits the frame to those viewport rows (a damage list), else everything is
    /// drawn. `focused` fills the cursor; an unfocused panel outlines it.
    pub fn draw(
        &self,
        rt: &ID2D1RenderTarget,
        area: Rect,
        scale: f32,
        grid: &dyn CellGrid,
        focused: bool,
        rows: Option<&[usize]>,
    ) -> Result<()> {
        let (cw, ch) = self.cell_size(scale);
        let (ox, oy) = (area.x + PADDING, area.y + PADDING);
        // SAFETY: plain brush creation on a live render target; the clip is popped below.
        let brush = unsafe { rt.CreateSolidColorBrush(&D2D1_COLOR_F::default(), None)? };
        // SAFETY: balanced with the pop and the mode restore at the end. Aliased fills land on whole device pixels, so
        // cell backgrounds meet without seams and the 1 px cursor outline stays crisp; text keeps its own anti-aliasing.
        unsafe {
            rt.PushAxisAlignedClip(&rect(area.x, area.y, area.w, area.h), D2D1_ANTIALIAS_MODE_ALIASED);
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
        }
        let result = (|| -> Result<()> {
            if rows.is_none() {
                self.fill(rt, &brush, area.x, area.y, area.w, area.h, fixed::TERMINAL_BG);
            }
            let cursor = grid.cursor();
            let (cols, nrows) = (grid.cols(), grid.rows());
            for r in 0..nrows {
                if rows.is_some_and(|list| !list.contains(&r)) {
                    continue;
                }
                let y = oy + r as f32 * ch;
                if rows.is_some() {
                    self.fill(rt, &brush, area.x, y, area.w, ch, fixed::TERMINAL_BG);
                }
                let cells: Vec<Resolved> = (0..cols)
                    .map(|c| resolve(&grid.cell(r, c), grid.selected(r, c), focused && cursor == Some((r, c))))
                    .collect();
                self.draw_row(rt, &brush, &cells, ox, y, cw, ch, scale)?;
                if let (false, Some((cr, cc))) = (focused, cursor)
                    && cr == r
                {
                    let line = 1.0 / scale;
                    let ink = fixed::TERMINAL_INK;
                    let x = ox + cc as f32 * cw;
                    for (rx, ry, rw, rh) in
                        [(x, y, cw, line), (x, y + ch - line, cw, line), (x, y, line, ch), (x + cw - line, y, line, ch)]
                    {
                        self.fill(rt, &brush, rx, ry, rw, rh, ink);
                    }
                }
            }
            Ok(())
        })();
        // SAFETY: balances the push and the mode change above.
        unsafe {
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.PopAxisAlignedClip();
        }
        result
    }

    #[allow(clippy::too_many_arguments)] // the geometry of one row
    fn draw_row(
        &self,
        rt: &ID2D1RenderTarget,
        brush: &ID2D1SolidColorBrush,
        cells: &[Resolved],
        ox: f32,
        y: f32,
        cw: f32,
        ch: f32,
        scale: f32,
    ) -> Result<()> {
        // Backgrounds: merged spans of equal colour (the default is the panel's fill already).
        let mut c = 0;
        while c < cells.len() {
            let bg = cells[c].bg;
            let mut end = c + 1;
            while end < cells.len() && cells[end].bg == bg {
                end += 1;
            }
            if bg != fixed::TERMINAL_BG {
                self.fill(rt, brush, ox + c as f32 * cw, y, (end - c) as f32 * cw, ch, bg);
            }
            c = end;
        }
        // Text: one layout per run of equal style; wide glyphs go alone at their own column.
        let line = 1.0 / scale;
        let mut c = 0;
        while c < cells.len() {
            let head = cells[c];
            if head.width == 0 {
                c += 1;
                continue;
            }
            let style = |x: &Resolved| (x.fg, x.bold, x.italic, x.underline, x.strike);
            let mut end = c + 1;
            if head.width == 1 {
                while end < cells.len() && cells[end].width == 1 && style(&cells[end]) == style(&head) {
                    end += 1;
                }
            }
            let text: String = cells[c..end].iter().map(|x| x.ch).collect();
            let x = ox + c as f32 * cw;
            let span_w = (end - c).max(usize::from(head.width)) as f32 * cw;
            if !text.trim().is_empty() {
                let utf16: Vec<u16> = text.encode_utf16().collect();
                let format = &self.formats[usize::from(head.bold) | (usize::from(head.italic) << 1)];
                // SAFETY: `utf16` and the format outlive the calls; the brush is ours; the layout is drawn on a live target.
                unsafe {
                    let layout = self.factory.CreateTextLayout(&utf16, format, span_w + cw, ch)?;
                    brush.SetColor(&d2d(head.fg));
                    rt.DrawTextLayout(point!(x, y), &layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
            }
            if head.underline {
                self.fill(rt, brush, x, y + ch - 2.0 * line, span_w, line, head.fg);
            }
            if head.strike {
                self.fill(rt, brush, x, y + ch / 2.0, span_w, line, head.fg);
            }
            c = end;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // the geometry and colour of one rectangle
    fn fill(&self, rt: &ID2D1RenderTarget, brush: &ID2D1SolidColorBrush, x: f32, y: f32, w: f32, h: f32, c: Color) {
        // SAFETY: valid rect and brush on a live render target.
        unsafe {
            brush.SetColor(&d2d(c));
            rt.FillRectangle(&rect(x, y, w, h), brush);
        }
    }
}

fn has_family(collection: &IDWriteFontCollection, name: &str) -> bool {
    let (mut index, mut exists) = (0, BOOL(0));
    // SAFETY: out-pointers are valid locals.
    unsafe { collection.FindFamilyName(&HSTRING::from(name), &mut index, &mut exists).is_ok() && exists.as_bool() }
}

// ---------- the terminal model as a grid ----------

/// The adapter between the terminal model (`swapdesk-termview`) and the drawing.
pub struct TermGrid<'a>(pub &'a swapdesk_termview::Screen);

fn paint_of(c: swapdesk_termview::Color) -> Paint {
    match c {
        swapdesk_termview::Color::Default => Paint::Default,
        swapdesk_termview::Color::Indexed(i) => Paint::Indexed(i),
        swapdesk_termview::Color::Rgb(r, g, b) => Paint::Rgb(r, g, b),
    }
}

impl CellGrid for TermGrid<'_> {
    fn cols(&self) -> usize {
        self.0.cols()
    }
    fn rows(&self) -> usize {
        self.0.rows()
    }
    fn cell(&self, row: usize, col: usize) -> GridCell {
        use swapdesk_termview::Attrs;
        let c = self.0.visible_cell(row, col);
        GridCell {
            ch: c.ch,
            fg: paint_of(c.fg),
            bg: paint_of(c.bg),
            bold: c.attrs.has(Attrs::BOLD),
            dim: c.attrs.has(Attrs::DIM),
            italic: c.attrs.has(Attrs::ITALIC),
            underline: c.attrs.has(Attrs::UNDERLINE),
            strike: c.attrs.has(Attrs::STRIKE),
            inverse: c.attrs.has(Attrs::INVERSE),
            hidden: c.attrs.has(Attrs::HIDDEN),
            width: c.width,
        }
    }
    fn cursor(&self) -> Option<(usize, usize)> {
        self.0.cursor_shown().then(|| (self.0.cursor().row, self.0.cursor().col))
    }
    fn selected(&self, row: usize, col: usize) -> bool {
        self.0.is_selected(row, col)
    }
}

// ---------- the output router ----------

/// Rows of history a terminal keeps.
const SCROLLBACK: usize = 5000;
/// Bytes of output one `pump` feeds the parser before it hands the thread back (R-UI-1).
pub const PUMP_BUDGET: usize = 64 * 1024;
/// Chunks that arrive for a terminal before its panel has attached (the reply and the first output race).
const PARKED_CHUNKS: usize = OUTPUT_CHANNEL_CHUNKS;
/// Terminals with parked output at once; the oldest goes first.
const PARKED_IDS: usize = 8;
/// Longest paste accepted, in bytes.
const MAX_PASTE: usize = 1024 * 1024;

#[derive(Default)]
struct Router {
    taps: HashMap<u32, OutputTx>,
    parked: Vec<(u32, Vec<Vec<u8>>)>,
    waker: Option<Arc<dyn UiWaker>>,
}

fn router() -> MutexGuard<'static, Router> {
    static ROUTER: OnceLock<Mutex<Router>> = OnceLock::new();
    ROUTER.get_or_init(Mutex::default).lock().unwrap_or_else(PoisonError::into_inner)
}

/// The UI thread's waker: output wakes it so the panel pumps and repaints.
pub fn set_terminal_waker(waker: Arc<dyn UiWaker>) {
    router().waker = Some(waker);
}

/// Terminal output from the service's pipe (`ViewerSink::term_output`, on its reader thread). It waits for room in the
/// panel's bounded channel, which is the back-pressure that reaches the host's shell, and wakes the UI thread. Output of
/// a terminal whose panel has not attached yet is parked (bounded) and handed over when it does.
pub fn terminal_output(term_id: u32, bytes: Vec<u8>) {
    let mut r = router();
    let waker = r.waker.clone();
    // Deciding and parking happen under one lock, so a panel that attaches meanwhile cannot miss a chunk.
    match r.taps.get(&term_id).cloned() {
        Some(tap) => {
            drop(r);
            // A full channel waits here, with no lock held.
            if tap.send(bytes).is_err() {
                router().taps.remove(&term_id);
            }
        }
        None => {
            r.park(term_id, bytes);
            drop(r);
        }
    }
    if let Some(w) = waker {
        w.wake();
    }
}

impl Router {
    fn park(&mut self, id: u32, bytes: Vec<u8>) {
        if let Some((_, list)) = self.parked.iter_mut().find(|(i, _)| *i == id) {
            if list.len() < PARKED_CHUNKS {
                list.push(bytes);
            }
            return;
        }
        if self.parked.len() >= PARKED_IDS {
            self.parked.remove(0);
        }
        self.parked.push((id, vec![bytes]));
    }
}

/// A model for terminal `id`, with what arrived for it before this call, wired to the router.
fn attach(id: u32, cols: usize, rows: usize) -> TermView {
    let (view, tx) = TermView::new(cols, rows, SCROLLBACK);
    let mut r = router();
    if let Some(pos) = r.parked.iter().position(|(i, _)| *i == id) {
        for chunk in r.parked.remove(pos).1 {
            let _ = tx.try_send(chunk);
        }
    }
    r.taps.insert(id, tx);
    view
}

fn detach(id: u32) {
    let mut r = router();
    r.taps.remove(&id);
    r.parked.retain(|(i, _)| *i != id);
}

// ---------- the panel ----------

/// What one pump of the model asks the shell to do.
#[derive(Debug, Default)]
pub struct Pumped {
    /// The screen changed: repaint.
    pub repaint: bool,
    /// What the terminal answers to the host's program (cursor position reports and the like): the shell writes it back.
    pub replies: Vec<u8>,
    pub id: Option<u32>,
    /// The budget ran out with output still queued: pump again on the next turn.
    pub more: bool,
}

struct Live {
    id: u32,
    view: TermView,
    /// The grid the model has and the host was told.
    grid: (usize, usize),
    selecting: bool,
    /// The last click: when (ms), where, and how many in a row (double and triple click).
    click: (u64, (usize, usize), u8),
    /// A high surrogate waiting for its low half.
    surrogate: Option<u16>,
}

/// The terminal panel of the viewer's drawer: the renderer (made when the drawer first shows the terminal, R-UI-11),
/// the model of the shell that is open, and the mouse and keyboard handling that turns them into bytes for the host.
#[derive(Default)]
pub struct TerminalPane {
    renderer: Option<TerminalRenderer>,
    live: Option<Live>,
    /// Where the grid was drawn last frame (DIPs) and at which scale; `None` while the panel is not on screen.
    area: Option<(Rect, f32)>,
}

impl TerminalPane {
    /// Follows the state: the model of terminal `want` (an open or ended one), none for `None`.
    pub fn sync(&mut self, want: Option<u32>) {
        if self.live.as_ref().map(|l| l.id) == want {
            return;
        }
        if let Some(old) = self.live.take() {
            detach(old.id);
        }
        if let Some(id) = want {
            self.live = Some(Live {
                id,
                view: attach(id, 80, 24),
                grid: (80, 24),
                selecting: false,
                click: (0, (0, 0), 0),
                surrogate: None,
            });
        }
    }

    /// Feeds the model with queued output, within the budget; the replies it owes the host's program are returned.
    pub fn pump(&mut self, budget: usize) -> Pumped {
        let Some(live) = &mut self.live else { return Pumped::default() };
        let p = live.view.pump(budget);
        let damaged = !matches!(live.view.term.screen_mut().take_damage(), swapdesk_termview::Damage::None);
        Pumped { repaint: damaged || p.chunks > 0, replies: p.replies, id: Some(live.id), more: p.more }
    }

    /// Draws the grid into `area` (DIPs; `None` when the panel is not showing the terminal) and returns the commands the
    /// frame calls for: a new size for the host's PTY when the panel's grid changed. Every frame redraws the whole grid:
    /// the window is a flip-model swap chain whose back buffers do not keep the last frame, so the damage list of the
    /// model only decides whether a frame is needed (see `pump`), not which rows it draws.
    pub fn paint(&mut self, rt: &ID2D1RenderTarget, area: Option<Rect>, scale: f32, focused: bool) -> Vec<Command> {
        let Some(area) = area else {
            self.area = None;
            return Vec::new();
        };
        self.area = Some((area, scale));
        // R-UI-11: the renderer, its fonts and its glyph metrics are made the first time the panel shows the terminal.
        if self.renderer.is_none() {
            self.renderer = TerminalRenderer::new().ok();
        }
        let (Some(renderer), Some(live)) = (&self.renderer, &mut self.live) else { return Vec::new() };
        let mut commands = Vec::new();
        let grid = renderer.grid_for(area.w, area.h, scale);
        if grid != live.grid {
            live.view.term.resize(grid.0, grid.1);
            live.grid = grid;
            let (cols, rows) = (u16::try_from(grid.0).unwrap_or(u16::MAX), u16::try_from(grid.1).unwrap_or(u16::MAX));
            commands.push(Command::TermResize { term_id: live.id, cols, rows });
        }
        // A failed frame leaves the dark box the screen drew; the next frame tries again.
        let _ = renderer.draw(rt, area, scale, &TermGrid(live.view.term.screen()), focused, None);
        live.view.term.screen_mut().take_damage();
        commands
    }

    fn cell_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let (area, scale) = self.area?;
        let live = self.live.as_ref()?;
        Some(self.renderer.as_ref()?.cell_at(area, scale, live.grid, x, y))
    }

    /// Typing returns the view to the bottom of the history.
    pub fn reset_view(&mut self) {
        if let Some(l) = &mut self.live {
            l.view.term.screen_mut().reset_view();
        }
    }

    /// What the key handling needs of the model: the grid's rows, whether the alternate screen shows, and the
    /// application-cursor mode. `None` without a terminal.
    pub fn modes_for_keys(&self) -> Option<(usize, bool, bool)> {
        let l = self.live.as_ref()?;
        let m = l.view.term.screen().modes();
        Some((l.grid.1, m.alt_screen, m.app_cursor))
    }

    /// UTF-16 text of a `WM_CHAR` as bytes for the host (surrogate halves are joined across calls).
    pub fn char_bytes(&mut self, unit: u16) -> Option<Vec<u8>> {
        let live = self.live.as_mut()?;
        let c = match (live.surrogate.take(), unit) {
            (_, 0xD800..=0xDBFF) => {
                live.surrogate = Some(unit);
                return None;
            }
            (Some(high), 0xDC00..=0xDFFF) => char::decode_utf16([high, unit]).next()?.ok()?,
            (_, u) => char::from_u32(u32::from(u))?,
        };
        if c.is_control() {
            return None;
        }
        live.view.term.screen_mut().reset_view();
        let mut buf = [0u8; 4];
        Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
    }

    /// A paste as bytes (bracketed when the program asked for it); empty text sends nothing.
    pub fn paste_bytes(&mut self, text: &str) -> Option<Vec<u8>> {
        let live = self.live.as_mut()?;
        if text.is_empty() {
            return None;
        }
        let text = truncate_text(text, MAX_PASTE);
        let screen = live.view.term.screen_mut();
        let bytes = encode_paste(text, screen.modes().bracketed_paste);
        screen.reset_view();
        Some(bytes)
    }

    /// The selected text, when there is one.
    pub fn selection_text(&self) -> Option<String> {
        self.live.as_ref()?.view.term.screen().selection_text().filter(|t| !t.is_empty())
    }

    pub fn clear_selection(&mut self) {
        if let Some(l) = &mut self.live {
            l.view.term.screen_mut().select_clear();
        }
    }

    /// Scrolls the history by `lines` (positive: back).
    pub fn scroll(&mut self, lines: isize) {
        if let Some(l) = &mut self.live {
            l.view.term.screen_mut().scroll_view(lines);
        }
    }

    /// Whether `(x, y)` (DIPs of the window) is inside the grid.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        self.live.is_some() && self.area.is_some_and(|(a, _)| a.contains(x, y))
    }

    pub fn selecting(&self) -> bool {
        self.live.as_ref().is_some_and(|l| l.selecting)
    }

    /// Mouse down inside the grid: one click starts a selection, two select the word, three the line. `now_ms` and
    /// `double_ms` come from the system's double-click time.
    pub fn mouse_down(&mut self, x: f32, y: f32, now_ms: u64, double_ms: u64) {
        let Some(cell) = self.cell_at(x, y) else { return };
        let Some(live) = &mut self.live else { return };
        let (last_ms, last_cell, count) = live.click;
        let count = if now_ms.saturating_sub(last_ms) <= double_ms && last_cell == cell { count % 3 + 1 } else { 1 };
        live.click = (now_ms, cell, count);
        let screen = live.view.term.screen_mut();
        match count {
            1 => screen.select_start(cell.0, cell.1),
            2 => screen.select_word(cell.0, cell.1),
            _ => screen.select_line(cell.0),
        }
        live.selecting = count == 1;
    }

    /// Mouse move while the button is down: extends the selection (clamped to the grid, so a drag past the edge goes on).
    pub fn mouse_move(&mut self, x: f32, y: f32) {
        if !self.selecting() {
            return;
        }
        if let Some((row, col)) = self.cell_at(x, y)
            && let Some(l) = &mut self.live
        {
            l.view.term.screen_mut().select_update(row, col);
        }
    }

    /// Mouse up: ends the drag; a click that selected nothing but its own cell leaves no selection.
    pub fn mouse_up(&mut self) {
        if let Some(live) = &mut self.live
            && std::mem::take(&mut live.selecting)
            && live.view.term.screen().selection().is_some_and(|s| s.anchor == s.head)
        {
            live.view.term.screen_mut().select_clear();
        }
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        if let Some(l) = self.live.take() {
            detach(l.id);
        }
    }
}

fn truncate_text(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

// ---------- the clipboard ----------

const CF_UNICODETEXT: u32 = 13;

/// The clipboard's text, if it holds some.
pub fn clipboard_text(hwnd: HWND) -> Option<String> {
    // SAFETY: the clipboard is opened for this window and closed on every path; the global memory handle belongs to the
    // clipboard and is locked only while the text is copied out, bounded by `GlobalSize`.
    unsafe {
        OpenClipboard(Some(hwnd)).ok()?;
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let global = HGLOBAL(handle.0);
            let ptr = GlobalLock(global).cast::<u16>();
            if ptr.is_null() {
                return None;
            }
            let units = GlobalSize(global) / 2;
            let all = std::slice::from_raw_parts(ptr, units);
            let end = all.iter().position(|&u| u == 0).unwrap_or(units);
            let text = String::from_utf16_lossy(&all[..end]);
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

/// Puts `text` on the clipboard as Unicode text.
pub fn set_clipboard_text(hwnd: HWND, text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = units.len() * 2;
    // SAFETY: the clipboard is opened for this window and closed on every path; the memory is allocated here, filled
    // while locked, and handed to the clipboard, which owns it after a successful `SetClipboardData` (freed here only when
    // it was refused).
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let global = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let ptr = GlobalLock(global).cast::<u16>();
            if ptr.is_null() {
                let _ = GlobalFree(Some(global));
                return None;
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), ptr, units.len());
            let _ = GlobalUnlock(global);
            if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0))).is_err() {
                let _ = GlobalFree(Some(global));
                return None;
            }
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}

// ---------- keys and mouse of the window ----------

/// The key a virtual-key code stands for (the ones that are not text), with the modifier state read from the keyboard.
fn key_of(vk: u16, mods: Mods) -> Option<Key> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    Some(match vk {
        v if v == VK_RETURN.0 => Key::Enter,
        v if v == VK_BACK.0 => Key::Backspace,
        v if v == VK_TAB.0 => {
            if mods.shift {
                Key::BackTab
            } else {
                Key::Tab
            }
        }
        v if v == VK_ESCAPE.0 => Key::Escape,
        v if v == VK_UP.0 => Key::Up,
        v if v == VK_DOWN.0 => Key::Down,
        v if v == VK_LEFT.0 => Key::Left,
        v if v == VK_RIGHT.0 => Key::Right,
        v if v == VK_HOME.0 => Key::Home,
        v if v == VK_END.0 => Key::End,
        v if v == VK_PRIOR.0 => Key::PageUp,
        v if v == VK_NEXT.0 => Key::PageDown,
        v if v == VK_INSERT.0 => Key::Insert,
        v if v == VK_DELETE.0 => Key::Delete,
        v if (VK_F1.0..=VK_F12.0).contains(&v) => Key::F((v - VK_F1.0 + 1) as u8),
        _ => return None,
    })
}

/// A `WM_KEYDOWN` the grid takes, as what it means.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Bytes for the shell.
    Send(Vec<u8>),
    Copy,
    /// `Ctrl+C` with a selection copies it; without one it is the interrupt.
    CopyOrInterrupt,
    Paste,
    /// Back into the history by this many lines (positive) or toward the bottom.
    Scroll(isize),
    /// Not the grid's: the window handles it (Ctrl+Tab leaves the grid).
    Pass,
}

/// What a key press means for the grid. `shift`, `ctrl` and `alt` are the keyboard's state; `ctrl && alt` is AltGr, whose
/// characters arrive as text, so only a real Ctrl makes control codes. `rows` is the grid's height, for a page of
/// scrolling; `alt_screen` turns the history off (full-screen programs get Shift+PageUp as a key).
pub fn key_action(vk: u16, mods: Mods, rows: usize, alt_screen: bool, app_cursor: bool) -> KeyAction {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_INSERT, VK_NEXT, VK_PRIOR, VK_TAB};
    let letter = (u16::from(b'A')..=u16::from(b'Z')).contains(&vk).then(|| char::from(vk as u8).to_ascii_lowercase());
    if vk == VK_TAB.0 && mods.ctrl {
        return KeyAction::Pass;
    }
    if mods.ctrl && !mods.alt {
        match (letter, mods.shift) {
            (Some('c'), true) => return KeyAction::Copy,
            (Some('c'), false) => return KeyAction::CopyOrInterrupt,
            (Some('v'), _) => return KeyAction::Paste,
            (Some(c), _) => {
                return KeyAction::Send(encode_key(Key::Char(c), Mods { shift: false, ..mods }, app_cursor));
            }
            (None, _) if vk == u16::from(b' ') => {
                return KeyAction::Send(encode_key(Key::Char(' '), Mods { shift: false, ..mods }, app_cursor));
            }
            _ => {}
        }
    }
    if mods.shift && !mods.ctrl && !mods.alt {
        if vk == VK_INSERT.0 {
            return KeyAction::Paste;
        }
        if !alt_screen && (vk == VK_PRIOR.0 || vk == VK_NEXT.0) {
            let page = isize::try_from(rows.saturating_sub(1).max(1)).unwrap_or(1);
            return KeyAction::Scroll(if vk == VK_PRIOR.0 { page } else { -page });
        }
    }
    match key_of(vk, mods) {
        Some(key) => KeyAction::Send(encode_key(key, mods, app_cursor)),
        None => KeyAction::Pass,
    }
}

/// Offers a message of the viewer window to the terminal panel; `Some` when the panel took it (the window then does not
/// handle it). Keys go to the shell while the grid has the keyboard focus and a shell runs; the mouse selects, pastes and
/// scrolls over the grid.
pub(crate) fn message(shared: &Rc<Shared>, w: &Window, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if w.kind != WindowKind::Viewer {
        return None;
    }
    match msg {
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MOUSEMOVE | WM_RBUTTONDOWN | WM_MOUSEWHEEL => {
            mouse(shared, w, msg, wparam, lparam)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR => {
            key(shared, w, msg, wparam, lparam)
        }
        _ => None,
    }
}

fn running_terminal(shared: &Shared) -> Option<u32> {
    let st = shared.state.borrow();
    matches!(st.term_phase(), TermPhase::Open { .. }).then(|| st.term_id()).flatten()
}

fn write_to_shell(shared: &Rc<Shared>, term_id: u32, bytes: Vec<u8>) {
    if !bytes.is_empty() {
        shared.send_commands(vec![Command::TermWrite { term_id, bytes }]);
    }
}

fn copy_selection(shared: &Rc<Shared>, w: &Window) -> bool {
    let text = shared.terminal.borrow().selection_text();
    match text {
        Some(t) => set_clipboard_text(w.hwnd(), &t),
        None => false,
    }
}

fn paste(shared: &Rc<Shared>, w: &Window, term_id: u32) {
    let Some(text) = clipboard_text(w.hwnd()) else { return };
    let bytes = shared.terminal.borrow_mut().paste_bytes(&text);
    if let Some(b) = bytes {
        write_to_shell(shared, term_id, b);
    }
}

fn mouse(shared: &Rc<Shared>, w: &Window, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetDoubleClickTime, ReleaseCapture, SetCapture};
    let scale = w.scale();
    let (px, py) = if msg == WM_MOUSEWHEEL {
        // The wheel message carries screen coordinates.
        let mut p = windows::Win32::Foundation::POINT { x: get_x(lparam.0), y: get_y(lparam.0) };
        // SAFETY: our window; converts a point to its client coordinates.
        unsafe {
            let _ = ScreenToClient(w.hwnd(), &mut p);
        }
        (p.x, p.y)
    } else {
        (get_x(lparam.0), get_y(lparam.0))
    };
    let (x, y) = (px as f32 / scale, py as f32 / scale);
    match msg {
        WM_LBUTTONDOWN => {
            if !shared.terminal.borrow().contains(x, y) {
                return None;
            }
            // SAFETY: a plain read of the system's double-click time.
            let double = u64::from(unsafe { GetDoubleClickTime() });
            let now = u64::try_from(click_clock().elapsed().as_millis()).unwrap_or(u64::MAX);
            let selecting = {
                let mut pane = shared.terminal.borrow_mut();
                pane.mouse_down(x, y, now, double);
                pane.selecting()
            };
            if running_terminal(shared).is_some() {
                w.focus_control(ControlId::TermGrid, false);
            }
            if selecting {
                // SAFETY: our own window takes the pointer while the selection is dragged.
                unsafe {
                    SetCapture(w.hwnd());
                }
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_MOUSEMOVE => {
            if !shared.terminal.borrow().selecting() {
                return None;
            }
            shared.terminal.borrow_mut().mouse_move(x, y);
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_LBUTTONUP => {
            if !shared.terminal.borrow().selecting() {
                return None;
            }
            shared.terminal.borrow_mut().mouse_up();
            // SAFETY: balances the capture taken on the button press.
            unsafe {
                let _ = ReleaseCapture();
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_RBUTTONDOWN => {
            if !shared.terminal.borrow().contains(x, y) {
                return None;
            }
            // Windows Terminal's convention: a selection is copied (and cleared), otherwise the clipboard is pasted.
            if copy_selection(shared, w) {
                shared.terminal.borrow_mut().clear_selection();
            } else if let Some(id) = running_terminal(shared) {
                paste(shared, w, id);
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        _ => {
            // WM_MOUSEWHEEL: three lines a notch, away from the user scrolls back into the history.
            if !shared.terminal.borrow().contains(x, y) {
                return None;
            }
            let notches = i32::from(hiword(wparam.0 as isize) as i16) as isize;
            shared.terminal.borrow_mut().scroll(notches * 3 / 120);
            w.invalidate();
            Some(LRESULT(0))
        }
    }
}

/// The origin of the click times (milliseconds since the first use).
fn click_clock() -> &'static std::time::Instant {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now)
}

fn keyboard_mods() -> Mods {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
    // SAFETY: plain key state reads.
    unsafe {
        Mods {
            shift: GetKeyState(i32::from(VK_SHIFT.0)) < 0,
            ctrl: GetKeyState(i32::from(VK_CONTROL.0)) < 0,
            alt: GetKeyState(i32::from(VK_MENU.0)) < 0,
        }
    }
}

fn key(shared: &Rc<Shared>, w: &Window, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_F4, VK_SPACE};
    let _ = lparam;
    if w.focused_control() != Some(ControlId::TermGrid) {
        return None;
    }
    let term_id = running_terminal(shared)?;
    let alt_text = |unit: u16| -> Option<Vec<u8>> {
        // Alt+character: the character, escape-prefixed (meta), as xterm does.
        let mut bytes = vec![0x1b];
        bytes.extend(shared.terminal.borrow_mut().char_bytes(unit)?);
        Some(bytes)
    };
    match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u16;
            let mods = keyboard_mods();
            // Alt+F4 closes the window and Alt+Space opens its menu: the system's.
            if msg == WM_SYSKEYDOWN && (vk == VK_F4.0 || (vk == VK_SPACE.0 && !mods.ctrl)) {
                return None;
            }
            let (rows, alt_screen, app_cursor) = shared.terminal.borrow().modes_for_keys()?;
            match key_action(vk, mods, rows, alt_screen, app_cursor) {
                KeyAction::Pass => return None,
                KeyAction::Send(bytes) => {
                    shared.terminal.borrow_mut().reset_view();
                    write_to_shell(shared, term_id, bytes);
                }
                KeyAction::Copy => {
                    if copy_selection(shared, w) {
                        shared.terminal.borrow_mut().clear_selection();
                    }
                }
                KeyAction::CopyOrInterrupt => {
                    if copy_selection(shared, w) {
                        shared.terminal.borrow_mut().clear_selection();
                    } else {
                        write_to_shell(shared, term_id, vec![0x03]);
                    }
                }
                KeyAction::Paste => paste(shared, w, term_id),
                KeyAction::Scroll(n) => shared.terminal.borrow_mut().scroll(n),
            }
            w.invalidate();
            Some(LRESULT(0))
        }
        WM_KEYUP => Some(LRESULT(0)),
        WM_CHAR => {
            let bytes = shared.terminal.borrow_mut().char_bytes(wparam.0 as u16);
            if let Some(b) = bytes {
                write_to_shell(shared, term_id, b);
                w.invalidate();
            }
            Some(LRESULT(0))
        }
        WM_SYSCHAR => {
            if let Some(b) = alt_text(wparam.0 as u16) {
                write_to_shell(shared, term_id, b);
                w.invalidate();
            }
            Some(LRESULT(0))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
    use windows::Win32::Graphics::Direct2D::{
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
        D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1CreateFactory, ID2D1Factory,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory, WICBitmapCacheOnLoad, WICRect,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };

    /// A scripted grid: `rows` of text, cell overrides, a cursor and a selection.
    struct Fake {
        cols: usize,
        lines: Vec<&'static str>,
        paint: Vec<((usize, usize), GridCell)>,
        cursor: Option<(usize, usize)>,
        selected: Vec<(usize, usize)>,
    }

    impl CellGrid for Fake {
        fn cols(&self) -> usize {
            self.cols
        }
        fn rows(&self) -> usize {
            self.lines.len()
        }
        fn cell(&self, row: usize, col: usize) -> GridCell {
            if let Some((_, c)) = self.paint.iter().find(|(p, _)| *p == (row, col)) {
                return *c;
            }
            GridCell { ch: self.lines[row].chars().nth(col).unwrap_or(' '), ..GridCell::BLANK }
        }
        fn cursor(&self) -> Option<(usize, usize)> {
            self.cursor
        }
        fn selected(&self, row: usize, col: usize) -> bool {
            self.selected.contains(&(row, col))
        }
    }

    struct Shot {
        w: u32,
        pixels: Vec<u8>,
    }

    impl Shot {
        fn rgb(&self, x: u32, y: u32) -> u32 {
            let i = ((y * self.w + x) * 4) as usize;
            (u32::from(self.pixels[i + 2]) << 16) | (u32::from(self.pixels[i + 1]) << 8) | u32::from(self.pixels[i])
        }
    }

    /// Renders one frame at `scale` into a WIC bitmap (as `offscreen.rs` does for the screens).
    fn shot(
        grid: &dyn CellGrid,
        focused: bool,
        rows: Option<&[usize]>,
        scale: f32,
        prefill: bool,
    ) -> (Shot, TerminalRenderer) {
        // SAFETY: COM initialisation per thread; a repeated call is harmless. The rest is plain factory, bitmap and target creation.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let renderer = TerminalRenderer::new().unwrap();
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).unwrap();
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let (w, h) = ((400.0 * scale) as u32, (200.0 * scale) as u32);
            let bitmap = wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).unwrap();
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0 * scale,
                dpiY: 96.0 * scale,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt: ID2D1RenderTarget = factory.CreateWicBitmapRenderTarget(&bitmap, &props).unwrap();
            rt.BeginDraw();
            if prefill {
                rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }));
            }
            renderer.draw(&rt, Rect::new(0.0, 0.0, 400.0, 200.0), scale, grid, focused, rows).unwrap();
            rt.EndDraw(None, None).unwrap();
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let r = WICRect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
            bitmap.CopyPixels(&r as *const WICRect as *const _, w * 4, &mut pixels).unwrap();
            (Shot { w, pixels }, renderer)
        }
    }

    fn cell_px(r: &TerminalRenderer, scale: f32, row: usize, col: usize) -> (u32, u32) {
        let (cw, ch) = r.cell_size(scale);
        // A point near the cell's top-left corner, inside its background and clear of the glyph's middle.
        (((PADDING + col as f32 * cw + 1.0) * scale) as u32, ((PADDING + row as f32 * ch + 1.0) * scale) as u32)
    }

    #[test]
    fn colours_follow_the_terminal_conventions() {
        assert_eq!(paint_color(Paint::Default, true), fixed::TERMINAL_INK);
        assert_eq!(paint_color(Paint::Default, false), fixed::TERMINAL_BG);
        assert_eq!(paint_color(Paint::Indexed(1), true).hex(), 0xE06C75);
        assert_eq!(paint_color(Paint::Indexed(16), true).hex(), 0x000000, "cube origin");
        assert_eq!(paint_color(Paint::Indexed(231), true).hex(), 0xFFFFFF, "cube end");
        assert_eq!(paint_color(Paint::Indexed(196), true).hex(), 0xFF0000, "cube red");
        assert_eq!(paint_color(Paint::Indexed(232), true).hex(), 0x080808, "grey ramp start");
        assert_eq!(paint_color(Paint::Rgb(1, 2, 3), false).hex(), 0x010203);
        let inv = resolve(&GridCell { inverse: true, ..GridCell::BLANK }, false, false);
        assert_eq!((inv.fg, inv.bg), (fixed::TERMINAL_BG, fixed::TERMINAL_INK));
        let hidden = resolve(&GridCell { hidden: true, ..GridCell::BLANK }, false, false);
        assert_eq!(hidden.fg, hidden.bg);
        assert_eq!(resolve(&GridCell::BLANK, true, false).bg, SELECTION);
        assert_eq!(resolve(&GridCell::BLANK, true, true).bg, fixed::TERMINAL_INK, "the cursor wins over the selection");
    }

    #[test]
    fn the_grid_fits_the_area_and_a_click_finds_its_cell() {
        let (r, _) = (TerminalRenderer::new().unwrap(), ());
        for scale in [1.0, 1.5, 2.0] {
            let (cw, ch) = r.cell_size(scale);
            assert!(cw > 4.0 && ch > 8.0, "cell {cw}x{ch}");
            let (cols, rows) = r.grid_for(800.0, 480.0, scale);
            assert!(
                cols as f32 * cw <= 800.0 - 2.0 * PADDING + 0.01 && rows as f32 * ch <= 480.0 - 2.0 * PADDING + 0.01
            );
            assert!((cols + 1) as f32 * cw > 800.0 - 2.0 * PADDING, "no column is wasted");
            assert_eq!(r.grid_for(1.0, 1.0, scale), (1, 1), "never empty");
            assert_eq!(r.grid_for(1.0e6, 1.0e6, scale), (MAX_CELLS, MAX_CELLS), "capped at the protocol's limit");
            let area = Rect::new(10.0, 20.0, 800.0, 480.0);
            assert_eq!(
                r.cell_at(area, scale, (cols, rows), 10.0 + PADDING + cw * 3.5, 20.0 + PADDING + ch * 2.5),
                (2, 3)
            );
            assert_eq!(r.cell_at(area, scale, (cols, rows), -50.0, -50.0), (0, 0), "a drag outside clamps");
            assert_eq!(r.cell_at(area, scale, (cols, rows), 5000.0, 5000.0), (rows - 1, cols - 1));
        }
    }

    #[test]
    fn a_frame_paints_backgrounds_text_selection_and_cursor() {
        let red = GridCell { bg: Paint::Indexed(1), ch: ' ', ..GridCell::BLANK };
        let grid = Fake {
            cols: 20,
            lines: vec!["hello", "", "wide"],
            paint: vec![((1, 2), red)],
            cursor: Some((2, 5)),
            selected: vec![(2, 0)],
        };
        let (s, r) = shot(&grid, true, None, 1.0, false);
        let at = |row, col| {
            let (x, y) = cell_px(&r, 1.0, row, col);
            s.rgb(x, y)
        };
        assert_eq!(at(1, 2), 0xE06C75, "a cell with a background");
        assert_eq!(at(1, 4), fixed::TERMINAL_BG.hex(), "the default background around it");
        assert_eq!(at(2, 0), SELECTION.hex(), "a selected cell");
        assert_eq!(at(2, 5), fixed::TERMINAL_INK.hex(), "the focused cursor is a filled block");
        assert_eq!(s.rgb(1, 1), fixed::TERMINAL_BG.hex(), "the padding is the panel's colour");
        // The text drew something other than the background in the first row.
        let (cw, ch) = r.cell_size(1.0);
        let mut inked = false;
        for y in (PADDING as u32)..((PADDING + ch) as u32) {
            for x in (PADDING as u32)..((PADDING + 5.0 * cw) as u32) {
                inked |= s.rgb(x, y) != fixed::TERMINAL_BG.hex();
            }
        }
        assert!(inked, "\"hello\" has glyph pixels");
    }

    #[test]
    fn an_unfocused_cursor_is_an_outline_and_the_frame_is_sharp_at_150_percent() {
        let grid = Fake { cols: 10, lines: vec!["x"], paint: vec![], cursor: Some((0, 3)), selected: vec![] };
        let (s, r) = shot(&grid, false, None, 1.5, false);
        let (cw, _) = r.cell_size(1.5);
        let mid_y = ((PADDING + 2.0) * 1.5) as u32;
        let left_edge = ((PADDING + 3.0 * cw) * 1.5) as u32;
        assert_eq!(s.rgb(left_edge, mid_y), fixed::TERMINAL_INK.hex(), "the outline's left edge");
        assert_eq!(s.rgb(left_edge + 4, mid_y), fixed::TERMINAL_BG.hex(), "hollow inside");
    }

    #[test]
    fn a_damage_list_redraws_only_the_listed_rows() {
        let red = GridCell { bg: Paint::Indexed(1), ..GridCell::BLANK };
        let grid = Fake {
            cols: 10,
            lines: vec!["", ""],
            paint: vec![((0, 0), red), ((1, 0), red)],
            cursor: None,
            selected: vec![],
        };
        // Rows start green; only row 1 is redrawn, so row 0 keeps the old pixels.
        let (s, r) = shot(&grid, true, Some(&[1]), 1.0, true);
        let (x0, y0) = cell_px(&r, 1.0, 0, 0);
        let (x1, y1) = cell_px(&r, 1.0, 1, 0);
        assert_eq!(s.rgb(x0, y0), 0x00FF00, "row 0 untouched");
        assert_eq!(s.rgb(x1, y1), 0xE06C75, "row 1 redrawn");
    }

    // ---------- stage 43: the model as a grid, the router, the panel and the keys ----------

    fn pane_with_renderer(id: u32) -> TerminalPane {
        let mut pane = TerminalPane::default();
        pane.sync(Some(id));
        pane.renderer = Some(TerminalRenderer::new().unwrap());
        pane
    }

    /// The panel draws `text` into an offscreen target and answers the commands the frame called for.
    fn draw_pane(pane: &mut TerminalPane, area: Rect, scale: f32) -> (Shot, Vec<Command>) {
        // SAFETY: COM initialisation per thread; a repeated call is harmless. The rest is plain factory, bitmap and target creation.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).unwrap();
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let (w, h) = ((400.0 * scale) as u32, (200.0 * scale) as u32);
            let bitmap = wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).unwrap();
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0 * scale,
                dpiY: 96.0 * scale,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt: ID2D1RenderTarget = factory.CreateWicBitmapRenderTarget(&bitmap, &props).unwrap();
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 1.0, b: 0.0, a: 1.0 }));
            let commands = pane.paint(&rt, Some(area), scale, true);
            rt.EndDraw(None, None).unwrap();
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            let r = WICRect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
            bitmap.CopyPixels(&r as *const WICRect as *const _, w * 4, &mut pixels).unwrap();
            (Shot { w, pixels }, commands)
        }
    }

    #[test]
    fn the_models_screen_is_a_grid_with_its_colours_cursor_and_selection() {
        let (mut view, tx) = TermView::new(20, 5, 100);
        tx.send(b"\x1b[31;1mhi\x1b[0m there".to_vec()).unwrap();
        let pumped = view.pump(PUMP_BUDGET);
        assert_eq!((pumped.chunks, pumped.bytes), (1, 19));
        let grid = TermGrid(view.term.screen());
        assert_eq!((grid.cols(), grid.rows()), (20, 5));
        let h = grid.cell(0, 0);
        assert_eq!((h.ch, h.fg, h.bold), ('h', Paint::Indexed(1), true));
        let t = grid.cell(0, 3);
        assert_eq!((t.ch, t.fg, t.bold), ('t', Paint::Default, false));
        assert_eq!(grid.cursor(), Some((0, 8)), "after \"hi there\"");
        assert!(!grid.selected(0, 0));
        view.term.screen_mut().select_start(0, 0);
        view.term.screen_mut().select_update(0, 1);
        assert!(TermGrid(view.term.screen()).selected(0, 1) && !TermGrid(view.term.screen()).selected(0, 2));
    }

    #[test]
    fn output_that_arrives_before_the_panel_is_parked_and_handed_over_in_order() {
        let id = 9001;
        terminal_output(id, b"one ".to_vec());
        terminal_output(id, b"two".to_vec());
        let mut pane = TerminalPane::default();
        pane.sync(Some(id));
        let p = pane.pump(PUMP_BUDGET);
        assert!(p.repaint && p.id == Some(id) && !p.more);
        assert_eq!(
            pane.live.as_ref().unwrap().view.term.screen().visible_text().lines().next().unwrap().trim_end(),
            "one two"
        );
        // After attaching it flows straight in, and wakes the UI thread.
        terminal_output(id, b"!".to_vec());
        assert!(pane.pump(PUMP_BUDGET).repaint);
        // Another id's output does not reach this panel, and a dropped panel takes its tap with it.
        terminal_output(id + 1, b"other".to_vec());
        assert!(!pane.pump(PUMP_BUDGET).repaint);
        pane.sync(None);
        assert!(!router().taps.contains_key(&id));
        terminal_output(id + 1, vec![]);
        router().parked.retain(|(i, _)| *i != id + 1);
    }

    #[test]
    fn parked_output_is_bounded() {
        let id = 9100;
        for n in 0..(PARKED_CHUNKS + 40) {
            terminal_output(id, vec![b'x'; n % 7 + 1]);
        }
        let parked = router().parked.iter().find(|(i, _)| *i == id).map(|(_, l)| l.len()).unwrap();
        assert_eq!(parked, PARKED_CHUNKS);
        for other in 0..(PARKED_IDS as u32 + 3) {
            terminal_output(9200 + other, b"y".to_vec());
        }
        assert!(router().parked.len() <= PARKED_IDS, "the oldest ids go first");
        let mut r = router();
        r.parked.clear();
    }

    #[test]
    fn the_terminal_answers_the_programs_questions_through_the_panel() {
        let id = 9300;
        let mut pane = TerminalPane::default();
        pane.sync(Some(id));
        terminal_output(id, b"\x1b[6n".to_vec());
        let p = pane.pump(PUMP_BUDGET);
        assert_eq!(p.replies, b"\x1b[1;1R", "the cursor position report goes back to the host's program");
        pane.sync(None);
    }

    #[test]
    fn a_burst_of_output_is_pumped_in_budgeted_turns() {
        let id = 9400;
        let mut pane = TerminalPane::default();
        pane.sync(Some(id));
        for _ in 0..8 {
            terminal_output(id, vec![b'a'; 1000]);
        }
        let first = pane.pump(2500);
        assert!(first.more && first.repaint, "stops at the budget with output left");
        let mut turns = 1;
        while pane.pump(2500).more {
            turns += 1;
        }
        assert!(turns >= 2);
        pane.sync(None);
    }

    #[test]
    fn keys_become_bytes_the_way_a_terminal_does() {
        let none = Mods::NONE;
        let ctrl = Mods { ctrl: true, ..none };
        let send = |vk: u16, m: Mods| key_action(vk, m, 30, false, false);
        assert_eq!(send(0x0D, none), KeyAction::Send(b"\r".to_vec()), "Enter");
        assert_eq!(send(0x08, none), KeyAction::Send(vec![0x7f]), "Backspace");
        assert_eq!(send(0x09, none), KeyAction::Send(b"\t".to_vec()), "Tab goes to the shell");
        assert_eq!(send(0x09, Mods { shift: true, ..none }), KeyAction::Send(b"\x1b[Z".to_vec()));
        assert_eq!(send(0x09, ctrl), KeyAction::Pass, "Ctrl+Tab leaves the grid");
        assert_eq!(send(0x1B, none), KeyAction::Send(vec![0x1b]));
        assert_eq!(send(0x26, none), KeyAction::Send(b"\x1b[A".to_vec()), "Up");
        assert_eq!(
            key_action(0x26, none, 30, false, true),
            KeyAction::Send(b"\x1bOA".to_vec()),
            "Up in application-cursor mode"
        );
        assert_eq!(send(0x74, none), KeyAction::Send(b"\x1b[15~".to_vec()), "F5");
        assert_eq!(send(u16::from(b'D'), ctrl), KeyAction::Send(vec![0x04]), "Ctrl+D");
        assert_eq!(send(u16::from(b' '), ctrl), KeyAction::Send(vec![0]), "Ctrl+Space");
        assert_eq!(send(u16::from(b'C'), ctrl), KeyAction::CopyOrInterrupt);
        assert_eq!(send(u16::from(b'C'), Mods { shift: true, ..ctrl }), KeyAction::Copy);
        assert_eq!(send(u16::from(b'V'), ctrl), KeyAction::Paste);
        assert_eq!(send(0x2D, Mods { shift: true, ..none }), KeyAction::Paste, "Shift+Insert");
        // AltGr is Ctrl+Alt: its characters arrive as text, so no control code is made from the key.
        assert_eq!(send(u16::from(b'Q'), Mods { ctrl: true, alt: true, ..none }), KeyAction::Pass);
        // A plain letter is text (WM_CHAR), not a key.
        assert_eq!(send(u16::from(b'A'), none), KeyAction::Pass);
        // Shift+PageUp scrolls the history by a page less one line, except on the alternate screen.
        assert_eq!(send(0x21, Mods { shift: true, ..none }), KeyAction::Scroll(29));
        assert_eq!(send(0x22, Mods { shift: true, ..none }), KeyAction::Scroll(-29));
        assert_eq!(
            key_action(0x21, Mods { shift: true, ..none }, 30, true, false),
            KeyAction::Send(b"\x1b[5;2~".to_vec()),
            "a full-screen program gets the key"
        );
        assert_eq!(send(0x21, none), KeyAction::Send(b"\x1b[5~".to_vec()));
    }

    #[test]
    fn typed_text_pastes_and_surrogates_become_utf8() {
        let id = 9500;
        let mut pane = TerminalPane::default();
        pane.sync(Some(id));
        assert_eq!(pane.char_bytes(u16::from(b'a')), Some(b"a".to_vec()));
        assert_eq!(pane.char_bytes(0x00E7), Some("ç".as_bytes().to_vec()));
        assert_eq!(pane.char_bytes(0x0D), None, "control characters are the key handler's");
        // U+1F600 as a surrogate pair arrives in two messages.
        assert_eq!(pane.char_bytes(0xD83D), None);
        assert_eq!(pane.char_bytes(0xDE00), Some("😀".as_bytes().to_vec()));
        // A paste: newlines become carriage returns, escapes are removed, brackets only when asked.
        assert_eq!(pane.paste_bytes("a\r\nb\nc\x1b[31m").unwrap(), b"a\rb\rc[31m");
        assert_eq!(pane.paste_bytes(""), None);
        terminal_output(id, b"\x1b[?2004h".to_vec());
        pane.pump(PUMP_BUDGET);
        assert_eq!(pane.paste_bytes("x").unwrap(), b"\x1b[200~x\x1b[201~");
        // A very long paste is cut, on a character boundary.
        let long = "é".repeat(MAX_PASTE);
        assert!(pane.paste_bytes(&long).unwrap().len() <= MAX_PASTE + 16);
        pane.sync(None);
        assert_eq!(pane.char_bytes(u16::from(b'a')), None, "no terminal, no bytes");
    }

    #[test]
    fn the_mouse_selects_a_word_a_line_and_a_drag_and_a_click_leaves_nothing() {
        let id = 9600;
        let mut pane = pane_with_renderer(id);
        terminal_output(id, b"hello world\r\nsecond line".to_vec());
        pane.pump(PUMP_BUDGET);
        let area = Rect::new(10.0, 20.0, 400.0, 200.0);
        pane.area = Some((area, 1.0));
        let (cw, ch) = pane.renderer.as_ref().unwrap().cell_size(1.0);
        let at = |col: f32, row: f32| (10.0 + PADDING + cw * (col + 0.5), 20.0 + PADDING + ch * (row + 0.5));
        assert!(pane.contains(at(0.0, 0.0).0, at(0.0, 0.0).1) && !pane.contains(5.0, 5.0));
        // A drag from column 0 to column 4 selects "hello".
        let (x, y) = at(0.0, 0.0);
        pane.mouse_down(x, y, 1000, 500);
        assert!(pane.selecting());
        let (x, y) = at(4.0, 0.0);
        pane.mouse_move(x, y);
        pane.mouse_up();
        assert_eq!(pane.selection_text().as_deref(), Some("hello"));
        // A click that selected nothing but its cell clears the selection.
        let (x, y) = at(6.0, 0.0);
        pane.mouse_down(x, y, 5000, 500);
        pane.mouse_up();
        assert_eq!(pane.selection_text(), None);
        // A double click selects the word, a third click the line.
        pane.mouse_down(x, y, 5100, 500);
        pane.mouse_up();
        assert_eq!(pane.selection_text().as_deref(), Some("world"));
        pane.mouse_down(x, y, 5200, 500);
        assert_eq!(pane.selection_text().as_deref(), Some("hello world"));
        // The wheel scrolls the history without a selection change; `clear_selection` ends it.
        pane.clear_selection();
        assert_eq!(pane.selection_text(), None);
        pane.sync(None);
    }

    #[test]
    fn the_panel_sizes_the_grid_to_its_area_asks_the_host_for_it_and_draws_the_output() {
        let id = 9700;
        let mut pane = TerminalPane::default();
        pane.sync(Some(id));
        terminal_output(id, b"\x1b[41m    \x1b[0m".to_vec());
        pane.pump(PUMP_BUDGET);
        // R-UI-11: no renderer before the panel first shows the terminal.
        assert!(pane.renderer.is_none());
        let area = Rect::new(0.0, 0.0, 400.0, 200.0);
        let (shot, commands) = draw_pane(&mut pane, area, 1.0);
        assert!(pane.renderer.is_some(), "made on the first frame the panel is shown");
        let (cols, rows) = pane.renderer.as_ref().unwrap().grid_for(area.w, area.h, 1.0);
        assert!((cols, rows) != (80, 24), "a 400 x 200 panel is not 80 x 24");
        assert_eq!(commands, vec![Command::TermResize { term_id: id, cols: cols as u16, rows: rows as u16 }]);
        assert_eq!(pane.live.as_ref().unwrap().grid, (cols, rows), "the model has the size the host was told");
        let (cw, ch) = pane.renderer.as_ref().unwrap().cell_size(1.0);
        assert_eq!(
            shot.rgb((PADDING + 1.0) as u32, (PADDING + 1.0) as u32),
            0xE06C75,
            "the red background of the first cells"
        );
        let _ = (cw, ch);
        // The same size again asks for nothing; a new area asks again.
        let (_, again) = draw_pane(&mut pane, area, 1.0);
        assert!(again.is_empty());
        let (_, bigger) = draw_pane(&mut pane, Rect::new(0.0, 0.0, 400.0, 100.0), 1.0);
        assert!(matches!(bigger[..], [Command::TermResize { .. }]));
        // Without an area (the panel is not on screen) nothing is drawn and the mouse finds no grid.
        let mut hidden = pane_with_renderer(id + 1);
        let (_, none) = draw_pane_hidden(&mut hidden);
        assert!(none.is_empty() && !hidden.contains(5.0, 5.0));
        pane.sync(None);
        hidden.sync(None);
    }

    fn draw_pane_hidden(pane: &mut TerminalPane) -> ((), Vec<Command>) {
        // SAFETY: plain factory, bitmap and target creation, as in `draw_pane`.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).unwrap();
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).unwrap();
            let bitmap = wic.CreateBitmap(64, 64, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).unwrap();
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let rt: ID2D1RenderTarget = factory.CreateWicBitmapRenderTarget(&bitmap, &props).unwrap();
            rt.BeginDraw();
            let commands = pane.paint(&rt, None, 1.0, false);
            rt.EndDraw(None, None).unwrap();
            ((), commands)
        }
    }
}
