//! The terminal panel of the viewer's drawer, the same on every system (F-TERM-01, F-XTERM-01): the colours of the
//! terminal conventions, the model of the open shell (`swapdesk-termview`) drawn as a [`grid`](crate::grid) over the
//! shared [`Painter`], the router that hands the host's output to the panel, the mouse selection and what a key means
//! to the grid. The shells keep only what is the system's own: the clipboard, the key codes and the window's events.
//! No OS API lives here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use swapdesk_termview::{Key, Mods, OUTPUT_CHANNEL_CHUNKS, OutputTx, TermView, encode_key, encode_paste};
use swapdesk_ui::tokens::{Color, fixed, text};
use swapdesk_ui::{Command, UiWaker};

use crate::grid::{self, GridMetrics, grid_metrics, paint_grid};
use crate::painter::{Painter, Rect};

/// Space between the panel's edge and the grid.
pub const PADDING: f32 = 8.0;
/// Largest grid the panel asks for (the protocol caps a terminal at 1000 x 1000).
pub const MAX_CELLS: usize = 1000;
/// Selected cells: the design's link blue, dark.
pub const SELECTION: Color = Color::rgb(0x2E3C86);

/// A colour as the terminal program set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Paint {
    #[default]
    Default,
    /// 0 to 15 the ANSI colours, 16 to 231 the 6x6x6 cube, 232 to 255 the grey ramp.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// One cell as the terminal model holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermCell {
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

impl TermCell {
    pub const BLANK: TermCell = TermCell {
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
    fn cell(&self, row: usize, col: usize) -> TermCell;
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

/// A cell after inverse, dim, selection and the block cursor are applied, as the grid draws it. The background is
/// left out (`None`) where it is the panel's own, so the grid paints runs only where a colour differs.
pub fn resolve(c: &TermCell, selected: bool, block_cursor: bool) -> grid::GridCell {
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
    let text = if c.width == 0 || c.ch == ' ' || c.hidden { String::new() } else { c.ch.to_string() };
    grid::GridCell {
        text,
        fg,
        bg: (bg != fixed::TERMINAL_BG).then_some(bg),
        bold: c.bold,
        italic: c.italic,
        underline: c.underline && !c.hidden,
        strike: c.strike && !c.hidden,
        wide: c.width == 2,
    }
}

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
    fn cell(&self, row: usize, col: usize) -> TermCell {
        use swapdesk_termview::Attrs;
        let c = self.0.visible_cell(row, col);
        TermCell {
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

/// The cell box of the panel's grid on this painter.
pub fn cell_metrics<P: Painter + ?Sized>(p: &P) -> GridMetrics {
    grid_metrics(p, text::TERM)
}

/// The grid that fits `area` (the panel's box, padding included): at least 1 x 1, at most the protocol's limit.
pub fn grid_for(m: GridMetrics, area: Rect) -> (usize, usize) {
    let inner = Rect::new(0.0, 0.0, (area.w - 2.0 * PADDING).max(0.0), (area.h - 2.0 * PADDING).max(0.0));
    let (cols, rows) = m.fit(inner);
    (cols.clamp(1, MAX_CELLS), rows.clamp(1, MAX_CELLS))
}

/// The cell under `(x, y)`, clamped to the grid (a drag past the edge keeps selecting).
pub fn cell_at(m: GridMetrics, area: Rect, grid: (usize, usize), x: f32, y: f32) -> (usize, usize) {
    let col = ((x - area.x - PADDING) / m.cell_w).floor();
    let row = ((y - area.y - PADDING) / m.cell_h).floor();
    let clamp = |v: f32, n: usize| if v < 0.0 { 0 } else { (v as usize).min(n.saturating_sub(1)) };
    (clamp(row, grid.1), clamp(col, grid.0))
}

/// Draws `grid` into `area`: the panel's background, then the cells (selection and cursor applied). `focused` fills
/// the cursor as a block; an unfocused panel outlines it.
pub fn draw<P: Painter + ?Sized>(p: &P, area: Rect, grid: &dyn CellGrid, focused: bool) -> GridMetrics {
    let m = cell_metrics(p);
    p.push_clip(area);
    p.fill_rect(area, fixed::TERMINAL_BG);
    let cursor = grid.cursor();
    let rows: Vec<Vec<grid::GridCell>> = (0..grid.rows())
        .map(|r| {
            (0..grid.cols())
                .map(|c| resolve(&grid.cell(r, c), grid.selected(r, c), focused && cursor == Some((r, c))))
                .collect()
        })
        .collect();
    let origin = (area.x + PADDING, area.y + PADDING);
    paint_grid(p, origin, &rows, text::TERM);
    if let (false, Some((cr, cc))) = (focused, cursor) {
        let line = p.hairline();
        let (x, y) = (origin.0 + cc as f32 * m.cell_w, origin.1 + cr as f32 * m.cell_h);
        let (w, h) = (m.cell_w, m.cell_h);
        for r in [
            Rect::new(x, y, w, line),
            Rect::new(x, y + h - line, w, line),
            Rect::new(x, y, line, h),
            Rect::new(x + w - line, y, line, h),
        ] {
            p.fill_rect(r, fixed::TERMINAL_INK);
        }
    }
    p.pop_clip();
    m
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
pub const MAX_PASTE: usize = 1024 * 1024;

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

/// Terminal output from the service (`ViewerSink::term_output`, on its reader thread). It waits for room in the
/// panel's bounded channel, which is the back-pressure that reaches the host's shell, and wakes the UI thread. Output
/// of a terminal whose panel has not attached yet is parked (bounded) and handed over when it does.
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
    /// What the terminal answers to the host's program (cursor position reports and the like): the shell writes it
    /// back.
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

/// The terminal panel of the viewer's drawer: the model of the shell that is open, and the mouse and keyboard
/// handling that turns them into bytes for the host. Nothing is made for it until a terminal opens (R-UI-11).
#[derive(Default)]
pub struct TerminalPane {
    live: Option<Live>,
    /// Where the grid was drawn last frame (DIPs) and its cell box; `None` while the panel is not on screen.
    area: Option<(Rect, GridMetrics)>,
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

    /// Draws the grid into `area` (DIPs; `None` when the panel is not showing the terminal) and returns the commands
    /// the frame calls for: a new size for the host's PTY when the panel's grid changed. Every frame draws the whole
    /// grid; the damage of the model only decides whether a frame is needed (see `pump`).
    pub fn paint<P: Painter + ?Sized>(&mut self, p: &P, area: Option<Rect>, focused: bool) -> Vec<Command> {
        let Some(area) = area else {
            self.area = None;
            return Vec::new();
        };
        let m = cell_metrics(p);
        self.area = Some((area, m));
        let Some(live) = &mut self.live else { return Vec::new() };
        let mut commands = Vec::new();
        let grid = grid_for(m, area);
        if grid != live.grid {
            live.view.term.resize(grid.0, grid.1);
            live.grid = grid;
            let (cols, rows) = (u16::try_from(grid.0).unwrap_or(u16::MAX), u16::try_from(grid.1).unwrap_or(u16::MAX));
            commands.push(Command::TermResize { term_id: live.id, cols, rows });
        }
        draw(p, area, &TermGrid(live.view.term.screen()), focused);
        live.view.term.screen_mut().take_damage();
        commands
    }

    fn cell_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        let (area, m) = self.area?;
        let live = self.live.as_ref()?;
        Some(cell_at(m, area, live.grid, x, y))
    }

    /// The grid the model has (columns, rows), when a terminal is open.
    pub fn grid(&self) -> Option<(usize, usize)> {
        self.live.as_ref().map(|l| l.grid)
    }

    /// The text the model shows, one line per row (for tests and the window's accessibility text).
    pub fn visible_text(&self) -> Option<String> {
        Some(self.live.as_ref()?.view.term.screen().visible_text())
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

    /// One UTF-16 unit of typed text (Windows' `WM_CHAR`) as bytes for the host; surrogate halves are joined across
    /// calls.
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
        self.text_bytes(c.encode_utf8(&mut [0u8; 4]))
    }

    /// Typed text (the Mac's `keyDown` characters, or one character of Windows) as bytes for the host; control
    /// characters are the key handling's, so they are left out. Typing returns the view to the bottom.
    pub fn text_bytes(&mut self, text: &str) -> Option<Vec<u8>> {
        let live = self.live.as_mut()?;
        let kept: String = text.chars().filter(|c| !c.is_control()).collect();
        if kept.is_empty() {
            return None;
        }
        live.view.term.screen_mut().reset_view();
        Some(kept.into_bytes())
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

    /// Mouse move while the button is down: extends the selection (clamped to the grid, so a drag past the edge goes
    /// on).
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

// ---------- keys ----------

/// A key press the grid takes, as what it means.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Bytes for the shell.
    Send(Vec<u8>),
    Copy,
    /// `Ctrl+C` with a selection copies it on Windows; without one it is the interrupt.
    CopyOrInterrupt,
    Paste,
    /// Back into the history by this many lines (positive) or toward the bottom.
    Scroll(isize),
    /// Not the grid's: the window handles it (Ctrl+Tab leaves the grid), or it arrives as text.
    Pass,
}

/// What a key press means for the grid. `key` is the key the system's code stands for: a letter `a` to `z` or the
/// space as `Key::Char` (lowercase), the keys that are not text by name, `None` for any other. `ctrl && alt` is AltGr,
/// whose characters arrive as text, so only a real Ctrl makes control codes. `rows` is the grid's height, for a page of
/// scrolling; `alt_screen` turns the history off (full-screen programs get Shift+PageUp as a key). A plain character
/// is `Pass`: it arrives as text.
pub fn key_action(key: Option<Key>, mods: Mods, rows: usize, alt_screen: bool, app_cursor: bool) -> KeyAction {
    let Some(key) = key else { return KeyAction::Pass };
    if matches!(key, Key::Tab | Key::BackTab) && mods.ctrl {
        return KeyAction::Pass;
    }
    if mods.ctrl && !mods.alt {
        match (key, mods.shift) {
            (Key::Char('c'), true) => return KeyAction::Copy,
            (Key::Char('c'), false) => return KeyAction::CopyOrInterrupt,
            (Key::Char('v'), _) => return KeyAction::Paste,
            (Key::Char(c), _) if c.is_ascii_lowercase() || c == ' ' => {
                return KeyAction::Send(encode_key(Key::Char(c), Mods { shift: false, ..mods }, app_cursor));
            }
            _ => {}
        }
    }
    if mods.shift && !mods.ctrl && !mods.alt {
        if key == Key::Insert {
            return KeyAction::Paste;
        }
        if !alt_screen && matches!(key, Key::PageUp | Key::PageDown) {
            let page = isize::try_from(rows.saturating_sub(1).max(1)).unwrap_or(1);
            return KeyAction::Scroll(if key == Key::PageUp { page } else { -page });
        }
    }
    match key {
        Key::Char(_) => KeyAction::Pass,
        key => KeyAction::Send(encode_key(key, mods, app_cursor)),
    }
}

#[cfg(test)]
mod tests;
