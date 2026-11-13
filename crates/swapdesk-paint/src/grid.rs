//! A terminal grid over the shared [`Painter`]: cells of one size in DIPs, each with its own background, a glyph in
//! the monospaced face (bold, italic), underline and strike, and wide cells (CJK, emoji) that take two columns. The cell
//! box comes from the style alone (0.6 em wide, one line high), never from the face's own advance, so the grid has
//! the same geometry on Windows (Cascadia Mono or Consolas) and on the Mac (Menlo); each glyph is centred in its box.
//! Underline and strike are drawn here as rectangles, so both systems draw them the same way.

use swapdesk_ui::tokens::{Color, TextStyle};

use crate::painter::{Painter, Rect};

/// One cell of the grid as the terminal model hands it over.
#[derive(Debug, Clone, PartialEq)]
pub struct GridCell {
    /// One grapheme, or empty for a blank cell and for the second column of a wide cell.
    pub text: String,
    pub fg: Color,
    /// `None`: the grid's own background.
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// The glyph takes this cell and the next one; the next cell is not drawn.
    pub wide: bool,
}

impl GridCell {
    pub fn blank(fg: Color) -> GridCell {
        GridCell {
            text: String::new(),
            fg,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            wide: false,
        }
    }
}

/// The size of one cell in DIPs, snapped to whole device pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridMetrics {
    pub cell_w: f32,
    pub cell_h: f32,
}

impl GridMetrics {
    /// How many columns and rows fit in `area`.
    pub fn fit(&self, area: Rect) -> (usize, usize) {
        ((area.w / self.cell_w).floor().max(1.0) as usize, (area.h / self.cell_h).floor().max(1.0) as usize)
    }
}

/// The cell box of a style: 0.6 em wide (the advance of the common monospaced faces) and one line high.
pub fn grid_metrics<P: Painter + ?Sized>(p: &P, st: TextStyle) -> GridMetrics {
    let one = 1.0 / p.scale();
    GridMetrics { cell_w: p.snap(st.size * 0.6).max(one), cell_h: p.snap(st.size * st.line).max(one) }
}

/// Paints `rows` from `origin` (the top left of the first cell), over a background `bg` the caller already painted.
/// Backgrounds go first, row by row in runs of one colour, then the glyphs, then underline and strike.
pub fn paint_grid<P: Painter + ?Sized>(
    p: &P,
    origin: (f32, f32),
    rows: &[Vec<GridCell>],
    st: TextStyle,
) -> GridMetrics {
    let m = grid_metrics(p, st);
    let line = p.hairline();
    for (r, row) in rows.iter().enumerate() {
        let y = origin.1 + r as f32 * m.cell_h;
        // Backgrounds, in runs of one colour.
        let mut c = 0;
        while c < row.len() {
            let Some(bg) = row[c].bg else {
                c += 1;
                continue;
            };
            let start = c;
            while c < row.len() && row[c].bg == Some(bg) {
                c += 1;
            }
            p.fill_rect(Rect::new(origin.0 + start as f32 * m.cell_w, y, (c - start) as f32 * m.cell_w, m.cell_h), bg);
        }
        // Glyphs and their lines.
        let mut c = 0;
        while c < row.len() {
            let cell = &row[c];
            let span = if cell.wide { 2 } else { 1 };
            let x = origin.0 + c as f32 * m.cell_w;
            let w = span as f32 * m.cell_w;
            if !cell.text.is_empty() && cell.text != " " {
                let mut style = st.weight(if cell.bold { 700 } else { st.weight });
                if cell.italic {
                    style = style.italic();
                }
                let label = p.label(&cell.text, style);
                p.draw(&label, p.snap(x + (w - label.w) / 2.0), p.snap(y + (m.cell_h - label.h) / 2.0), cell.fg);
            }
            if cell.underline {
                p.fill_rect(Rect::new(x, y + m.cell_h - 2.0 * line, w, line), cell.fg);
            }
            if cell.strike {
                p.fill_rect(Rect::new(x, p.snap(y + m.cell_h * 0.55), w, line), cell.fg);
            }
            c += span;
        }
    }
    m
}
