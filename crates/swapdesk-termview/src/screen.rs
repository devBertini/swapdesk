//! The screen: the cell grid, the scrollback, the cursor, the viewport and the selection. The VT parser (`perform.rs`)
//! drives it; the shell reads it to draw.

use std::collections::VecDeque;

use crate::cell::{Attrs, Cell, Color, Row, char_width};
use crate::selection::{Point, Selection};

/// Modes the host program can switch; the key encoder and the shell read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    /// DECAWM: wrap at the last column.
    pub autowrap: bool,
    /// DECTCEM: the cursor is drawn.
    pub cursor_visible: bool,
    /// DECCKM: the arrow keys send `ESC O x` instead of `ESC [ x`.
    pub app_cursor: bool,
    /// Pasted text goes between `ESC [ 200 ~` and `ESC [ 201 ~`.
    pub bracketed_paste: bool,
    /// The alternate screen (full-screen programs) is showing: no scrollback.
    pub alt_screen: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self { autowrap: true, cursor_visible: true, app_cursor: false, bracketed_paste: false, alt_screen: false }
    }
}

/// Where the cursor is, in screen rows and columns (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    /// The last glyph filled the last column: the next one wraps first.
    pub pending_wrap: bool,
}

/// What changed since the last [`Screen::take_damage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    None,
    /// These viewport rows only (ascending).
    Rows(Vec<usize>),
    All,
}

#[derive(Clone, Copy)]
pub(crate) struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Pen {
    pub(crate) const DEFAULT: Pen = Pen { fg: Color::Default, bg: Color::Default, attrs: Attrs(0) };
}

struct SavedPrimary {
    grid: Vec<Row>,
    cursor: Cursor,
}

#[derive(Clone, Copy)]
struct SavedCursor {
    cursor: Cursor,
    pen: Pen,
    gfx: bool,
}

pub struct Screen {
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    pub(crate) grid: Vec<Row>,
    scrollback: VecDeque<Row>,
    scrollback_limit: usize,
    /// Rows dropped from the front of the scrollback so far: with the index they give a row an id that survives trimming.
    dropped: u64,
    pub(crate) cursor: Cursor,
    saved_cursor: Option<SavedCursor>,
    pub(crate) pen: Pen,
    pub(crate) top: usize,
    pub(crate) bottom: usize,
    pub(crate) modes: Modes,
    pub(crate) tabs: Vec<bool>,
    pub(crate) title: String,
    pub(crate) gfx: bool,
    saved_primary: Option<SavedPrimary>,
    dirty: Vec<bool>,
    dirty_all: bool,
    pub(crate) replies: Vec<u8>,
    pub(crate) bell: u32,
    view_offset: usize,
    selection: Option<Selection>,
}

fn tab_stops(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c % 8 == 0 && c != 0).collect()
}

impl Screen {
    /// A blank screen. Sizes are clamped to at least 1x1.
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Screen {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Screen {
            cols,
            rows,
            grid: vec![Row::blank(cols, Color::Default); rows],
            scrollback: VecDeque::new(),
            scrollback_limit,
            dropped: 0,
            cursor: Cursor::default(),
            saved_cursor: None,
            pen: Pen::DEFAULT,
            top: 0,
            bottom: rows - 1,
            modes: Modes::default(),
            tabs: tab_stops(cols),
            title: String::new(),
            gfx: false,
            saved_primary: None,
            dirty: vec![true; rows],
            dirty_all: true,
            replies: Vec::new(),
            bell: 0,
            view_offset: 0,
            selection: None,
        }
    }

    // ---------- reading ----------

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Whether the cursor shows in the viewport: visible mode and the viewport is at the bottom.
    pub fn cursor_shown(&self) -> bool {
        self.modes.cursor_visible && self.view_offset == 0
    }

    pub fn modes(&self) -> Modes {
        self.modes
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Bells since the last call.
    pub fn take_bells(&mut self) -> u32 {
        core::mem::take(&mut self.bell)
    }

    /// Bytes the terminal answers to the host program (cursor position reports, device attributes); the owner sends them
    /// back to the PTY.
    pub fn take_replies(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.replies)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// How many rows the viewport is scrolled back (0 = at the bottom).
    pub fn view_offset(&self) -> usize {
        self.view_offset
    }

    /// The row shown at viewport row `r` (0 = top), honouring the scroll position.
    pub fn visible_row(&self, r: usize) -> &Row {
        let r = r.min(self.rows - 1);
        let idx = self.scrollback.len() - self.view_offset + r;
        if idx < self.scrollback.len() { &self.scrollback[idx] } else { &self.grid[idx - self.scrollback.len()] }
    }

    /// The cell at viewport row `r`, column `c` (a blank outside the grid).
    pub fn visible_cell(&self, r: usize, c: usize) -> Cell {
        self.visible_row(r).cells.get(c).copied().unwrap_or(Cell::BLANK)
    }

    /// The whole viewport as text, one line per row, trailing blanks trimmed.
    pub fn visible_text(&self) -> String {
        (0..self.rows).map(|r| self.visible_row(r).text()).collect::<Vec<_>>().join("\n")
    }

    // ---------- damage ----------

    pub(crate) fn mark(&mut self, row: usize) {
        if let Some(d) = self.dirty.get_mut(row) {
            *d = true;
        }
    }

    pub(crate) fn mark_all(&mut self) {
        self.dirty_all = true;
    }

    /// The viewport rows to redraw since the last call; the shell draws only these (R-UI: nothing when `None`).
    pub fn take_damage(&mut self) -> Damage {
        if core::mem::take(&mut self.dirty_all) {
            self.dirty.iter_mut().for_each(|d| *d = false);
            return Damage::All;
        }
        let rows: Vec<usize> = self.dirty.iter().enumerate().filter(|(_, d)| **d).map(|(i, _)| i).collect();
        self.dirty.iter_mut().for_each(|d| *d = false);
        // With the viewport scrolled back, the grid row `i` shows at a different viewport row: redraw everything.
        if rows.is_empty() {
            Damage::None
        } else if self.view_offset > 0 {
            Damage::All
        } else {
            Damage::Rows(rows)
        }
    }

    // ---------- viewport ----------

    /// Scrolls the viewport by `delta` rows (positive = back into history). Ignored on the alternate screen.
    pub fn scroll_view(&mut self, delta: isize) {
        if self.modes.alt_screen {
            return;
        }
        let max = self.scrollback.len() as isize;
        let next = (self.view_offset as isize + delta).clamp(0, max) as usize;
        if next != self.view_offset {
            self.view_offset = next;
            self.mark_all();
        }
    }

    /// Back to the bottom (the user typed).
    pub fn reset_view(&mut self) {
        if self.view_offset != 0 {
            self.view_offset = 0;
            self.mark_all();
        }
    }

    // ---------- cursor and rows ----------

    pub(crate) fn blank_row(&self) -> Row {
        Row::blank(self.cols, self.pen.bg)
    }

    pub(crate) fn move_to(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.rows - 1);
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor { cursor: self.cursor, pen: self.pen, gfx: self.gfx });
    }

    pub(crate) fn restore_cursor(&mut self) {
        if let Some(s) = self.saved_cursor {
            self.cursor = s.cursor;
            self.pen = s.pen;
            self.gfx = s.gfx;
            self.move_to(self.cursor.row, self.cursor.col);
        } else {
            self.move_to(0, 0);
        }
    }

    /// Scrolls the region up by one row; on the primary screen a row leaving the top of the whole screen joins the scrollback.
    pub(crate) fn scroll_up_one(&mut self) {
        let blank = self.blank_row();
        let gone = self.grid.remove(self.top);
        self.grid.insert(self.bottom, blank);
        for r in self.top..=self.bottom {
            self.mark(r);
        }
        if self.top == 0 && !self.modes.alt_screen && self.scrollback_limit > 0 {
            self.scrollback.push_back(gone);
            if self.scrollback.len() > self.scrollback_limit {
                self.scrollback.pop_front();
                self.dropped += 1;
            }
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + 1).min(self.scrollback.len());
                self.mark_all();
            }
        }
    }

    pub(crate) fn scroll_down_one(&mut self) {
        let blank = self.blank_row();
        self.grid.remove(self.bottom);
        self.grid.insert(self.top, blank);
        for r in self.top..=self.bottom {
            self.mark(r);
        }
    }

    pub(crate) fn linefeed(&mut self) {
        if self.cursor.row == self.bottom {
            self.scroll_up_one();
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn reverse_index(&mut self) {
        if self.cursor.row == self.top {
            self.scroll_down_one();
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
        self.cursor.pending_wrap = false;
    }

    /// Writes a glyph at the cursor with the pen, wrapping as DECAWM says.
    pub(crate) fn put_char(&mut self, c: char) {
        let c = if self.gfx { dec_graphics(c) } else { c };
        let w = char_width(c);
        if w == 0 {
            return;
        }
        if self.cursor.pending_wrap {
            if !self.modes.autowrap {
                self.cursor.pending_wrap = false;
            } else {
                self.grid[self.cursor.row].wrapped = true;
                self.cursor.col = 0;
                self.linefeed();
            }
        }
        if w == 2 && self.cursor.col + 1 >= self.cols {
            if !self.modes.autowrap || self.cols < 2 {
                return;
            }
            let row = self.cursor.row;
            let last = self.cols - 1;
            self.grid[row].cells[last] = Cell::blank_with(self.pen.bg);
            self.grid[row].wrapped = true;
            self.cursor.col = 0;
            self.linefeed();
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.break_wide(row, col);
        if w == 2 {
            self.break_wide(row, col + 1);
        }
        let pen = self.pen;
        self.grid[row].cells[col] = Cell { ch: c, fg: pen.fg, bg: pen.bg, attrs: pen.attrs, width: w as u8 };
        if w == 2 {
            self.grid[row].cells[col + 1] = Cell { ch: ' ', fg: pen.fg, bg: pen.bg, attrs: pen.attrs, width: 0 };
        }
        self.mark(row);
        if col + w >= self.cols {
            self.cursor.col = self.cols - 1;
            self.cursor.pending_wrap = true;
        } else {
            self.cursor.col = col + w;
        }
    }

    /// Overwriting half of a wide glyph blanks the other half.
    fn break_wide(&mut self, row: usize, col: usize) {
        let Some(cell) = self.grid[row].cells.get(col).copied() else { return };
        let bg = self.pen.bg;
        if cell.width == 0 && col > 0 {
            self.grid[row].cells[col - 1] = Cell::blank_with(bg);
        } else if cell.width == 2 && col + 1 < self.cols {
            self.grid[row].cells[col + 1] = Cell::blank_with(bg);
        }
    }

    pub(crate) fn tab_forward(&mut self, n: usize) {
        for _ in 0..n.max(1) {
            let mut c = self.cursor.col + 1;
            while c < self.cols - 1 && !self.tabs[c] {
                c += 1;
            }
            self.cursor.col = c.min(self.cols - 1);
        }
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn tab_back(&mut self, n: usize) {
        for _ in 0..n.max(1) {
            let mut c = self.cursor.col.saturating_sub(1);
            while c > 0 && !self.tabs[c] {
                c -= 1;
            }
            self.cursor.col = c;
        }
        self.cursor.pending_wrap = false;
    }

    /// Erases cells `from..to` of a row with the pen's background.
    pub(crate) fn erase_cells(&mut self, row: usize, from: usize, to: usize) {
        let bg = self.pen.bg;
        let to = to.min(self.cols);
        for c in from.min(to)..to {
            self.break_wide(row, c);
            self.grid[row].cells[c] = Cell::blank_with(bg);
        }
        self.grid[row].wrapped = false;
        self.mark(row);
    }

    pub(crate) fn erase_rows(&mut self, from: usize, to: usize) {
        for r in from..to.min(self.rows) {
            self.grid[r] = self.blank_row();
            self.mark(r);
        }
    }

    pub(crate) fn insert_cells(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let n = n.max(1).min(self.cols - col);
        let bg = self.pen.bg;
        let cells = &mut self.grid[row].cells;
        cells.truncate(self.cols - n);
        for _ in 0..n {
            cells.insert(col, Cell::blank_with(bg));
        }
        self.cursor.pending_wrap = false;
        self.mark(row);
    }

    pub(crate) fn delete_cells(&mut self, n: usize) {
        let (row, col) = (self.cursor.row, self.cursor.col);
        let n = n.max(1).min(self.cols - col);
        let bg = self.pen.bg;
        let cells = &mut self.grid[row].cells;
        cells.drain(col..col + n);
        cells.extend(core::iter::repeat_n(Cell::blank_with(bg), n));
        self.cursor.pending_wrap = false;
        self.mark(row);
    }

    /// IL (`down` true: lines pushed down) and DL: only inside the scroll region.
    pub(crate) fn insert_lines(&mut self, n: usize, down: bool) {
        let row = self.cursor.row;
        if row < self.top || row > self.bottom {
            return;
        }
        let n = n.max(1).min(self.bottom - row + 1);
        for _ in 0..n {
            let blank = self.blank_row();
            if down {
                self.grid.remove(self.bottom);
                self.grid.insert(row, blank);
            } else {
                self.grid.remove(row);
                self.grid.insert(self.bottom, blank);
            }
        }
        for r in row..=self.bottom {
            self.mark(r);
        }
        self.cursor.col = 0;
        self.cursor.pending_wrap = false;
    }

    pub(crate) fn set_region(&mut self, top: usize, bottom: usize) {
        if top < bottom && bottom < self.rows {
            self.top = top;
            self.bottom = bottom;
        } else {
            self.top = 0;
            self.bottom = self.rows - 1;
        }
        self.move_to(0, 0);
    }

    pub(crate) fn full_reset(&mut self) {
        let keep = (self.cols, self.rows, self.scrollback_limit);
        let title = core::mem::take(&mut self.title);
        let scrollback = core::mem::take(&mut self.scrollback);
        let dropped = self.dropped;
        *self = Screen::new(keep.0, keep.1, keep.2);
        self.title = title;
        self.scrollback = scrollback;
        self.dropped = dropped;
    }

    pub(crate) fn set_alt_screen(&mut self, on: bool, save_cursor: bool) {
        if on == self.modes.alt_screen {
            return;
        }
        self.selection = None;
        self.view_offset = 0;
        if on {
            let blank = vec![Row::blank(self.cols, Color::Default); self.rows];
            let grid = core::mem::replace(&mut self.grid, blank);
            self.saved_primary = Some(SavedPrimary { grid, cursor: self.cursor });
            self.modes.alt_screen = true;
            self.top = 0;
            self.bottom = self.rows - 1;
            if save_cursor {
                self.save_cursor();
            }
        } else {
            self.modes.alt_screen = false;
            if let Some(p) = self.saved_primary.take() {
                self.grid = p.grid;
                self.cursor = p.cursor;
            }
            self.top = 0;
            self.bottom = self.rows - 1;
            if save_cursor {
                self.restore_cursor();
            }
        }
        self.mark_all();
    }

    // ---------- resize ----------

    /// Resizes the grid. Rows that no longer fit leave through the top into the scrollback (keeping the cursor on screen);
    /// a taller screen takes history back into view. Columns are cut or padded (no reflow). Clears the selection.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.selection = None;
        let bg = Color::Default;
        for row in self.grid.iter_mut().chain(self.scrollback.iter_mut()) {
            row.cells.resize(cols, Cell::blank_with(bg));
        }
        if let Some(p) = &mut self.saved_primary {
            for row in &mut p.grid {
                row.cells.resize(cols, Cell::blank_with(bg));
            }
            p.grid.resize(rows, Row::blank(cols, bg));
            p.cursor.row = p.cursor.row.min(rows - 1);
            p.cursor.col = p.cursor.col.min(cols - 1);
        }
        let alt = self.modes.alt_screen;
        if rows < self.rows {
            let excess = (self.cursor.row + 1).saturating_sub(rows);
            for _ in 0..excess {
                let gone = self.grid.remove(0);
                if !alt && self.scrollback_limit > 0 {
                    self.scrollback.push_back(gone);
                    if self.scrollback.len() > self.scrollback_limit {
                        self.scrollback.pop_front();
                        self.dropped += 1;
                    }
                }
            }
            self.cursor.row -= excess;
            self.grid.truncate(rows);
        } else if rows > self.rows {
            let mut add = rows - self.rows;
            if !alt {
                let pull = add.min(self.scrollback.len());
                for _ in 0..pull {
                    if let Some(r) = self.scrollback.pop_back() {
                        self.grid.insert(0, r);
                    }
                }
                self.cursor.row += pull;
                add -= pull;
            }
            for _ in 0..add {
                self.grid.push(Row::blank(cols, bg));
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.tabs = tab_stops(cols);
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.pending_wrap = false;
        self.view_offset = self.view_offset.min(self.scrollback.len());
        self.dirty = vec![true; rows];
        self.mark_all();
    }

    // ---------- selection ----------

    fn view_top_id(&self) -> u64 {
        self.dropped + (self.scrollback.len() - self.view_offset) as u64
    }

    /// The stable id of a viewport row: it follows the text as it scrolls into the history.
    pub fn row_id(&self, viewport_row: usize) -> u64 {
        self.view_top_id() + viewport_row as u64
    }

    fn row_by_id(&self, id: u64) -> Option<&Row> {
        let idx = id.checked_sub(self.dropped)? as usize;
        if idx < self.scrollback.len() { self.scrollback.get(idx) } else { self.grid.get(idx - self.scrollback.len()) }
    }

    fn last_id(&self) -> u64 {
        self.dropped + (self.scrollback.len() + self.rows) as u64 - 1
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Starts a selection at a viewport cell (mouse down).
    pub fn select_start(&mut self, viewport_row: usize, col: usize) {
        let p = Point { row: self.row_id(viewport_row), col: col.min(self.cols - 1) };
        self.selection = Some(Selection { anchor: p, head: p });
        self.mark_all();
    }

    /// Extends the selection to a viewport cell (mouse drag).
    pub fn select_update(&mut self, viewport_row: usize, col: usize) {
        let head = Point { row: self.row_id(viewport_row), col: col.min(self.cols - 1) };
        if let Some(s) = &mut self.selection
            && s.head != head
        {
            s.head = head;
            self.mark_all();
        }
    }

    /// Selects the word under a viewport cell (double click): a run of non-blank, non-separator characters.
    pub fn select_word(&mut self, viewport_row: usize, col: usize) {
        let id = self.row_id(viewport_row);
        let Some(row) = self.row_by_id(id) else { return };
        let col = col.min(self.cols - 1);
        let is_word = |c: &Cell| !c.ch.is_whitespace() && !"\"'`()[]{}<>|;,".contains(c.ch);
        if !is_word(&row.cells[col]) {
            self.selection = Some(Selection { anchor: Point { row: id, col }, head: Point { row: id, col } });
        } else {
            let (mut a, mut b) = (col, col);
            while a > 0 && is_word(&row.cells[a - 1]) {
                a -= 1;
            }
            while b + 1 < self.cols && is_word(&row.cells[b + 1]) {
                b += 1;
            }
            self.selection = Some(Selection { anchor: Point { row: id, col: a }, head: Point { row: id, col: b } });
        }
        self.mark_all();
    }

    /// Selects the whole row (triple click).
    pub fn select_line(&mut self, viewport_row: usize) {
        let id = self.row_id(viewport_row);
        self.selection =
            Some(Selection { anchor: Point { row: id, col: 0 }, head: Point { row: id, col: self.cols - 1 } });
        self.mark_all();
    }

    pub fn select_all(&mut self) {
        let first = self.dropped;
        self.selection = Some(Selection {
            anchor: Point { row: first, col: 0 },
            head: Point { row: self.last_id(), col: self.cols - 1 },
        });
        self.mark_all();
    }

    pub fn select_clear(&mut self) {
        if self.selection.take().is_some() {
            self.mark_all();
        }
    }

    /// Whether the viewport cell is inside the selection (the shell paints it inverted).
    pub fn is_selected(&self, viewport_row: usize, col: usize) -> bool {
        self.selection.is_some_and(|s| s.contains(self.row_id(viewport_row), col))
    }

    /// The selected text: rows joined by newlines unless a row soft-wrapped, trailing blanks trimmed.
    pub fn selection_text(&self) -> Option<String> {
        let s = self.selection?;
        let (a, b) = s.ordered();
        let mut out = String::new();
        for id in a.row.max(self.dropped)..=b.row.min(self.last_id()) {
            let Some(row) = self.row_by_id(id) else { continue };
            let from = if id == a.row { a.col } else { 0 };
            let to = if id == b.row { b.col + 1 } else { self.cols };
            let cells = &row.cells[from.min(self.cols)..to.min(self.cols)];
            let end = if id == b.row && to < self.cols {
                cells.len()
            } else {
                cells.iter().rposition(|c| !c.is_blank()).map_or(0, |i| i + 1)
            };
            out.extend(cells[..end].iter().filter(|c| c.width != 0).map(|c| c.ch));
            if id != b.row && !row.wrapped {
                out.push('\n');
            }
        }
        Some(out)
    }
}

/// The DEC special graphics set (`ESC ( 0`): line drawing.
fn dec_graphics(c: char) -> char {
    match c {
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'q' => '─',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'a' => '▒',
        '`' => '◆',
        'f' => '°',
        'g' => '±',
        '~' => '·',
        _ => c,
    }
}
