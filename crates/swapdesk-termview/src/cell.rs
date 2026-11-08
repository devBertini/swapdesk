//! One character cell: the glyph, its two colours and its attributes.

/// A colour as the terminal program set it; the shell maps it to a palette or RGB (light and dark follow the theme).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The theme's foreground or background.
    #[default]
    Default,
    /// 0 to 7 normal, 8 to 15 bright, 16 to 255 the xterm cube and grey ramp.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Attribute bits of a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attrs(pub u8);

impl Attrs {
    pub const BOLD: u8 = 1 << 0;
    pub const DIM: u8 = 1 << 1;
    pub const ITALIC: u8 = 1 << 2;
    pub const UNDERLINE: u8 = 1 << 3;
    pub const INVERSE: u8 = 1 << 4;
    pub const HIDDEN: u8 = 1 << 5;
    pub const STRIKE: u8 = 1 << 6;

    pub const fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    pub fn set(&mut self, bit: u8, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

/// A grid cell. `width` is 1, 2 for the left half of a wide glyph, or 0 for the spacer that follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    pub width: u8,
}

impl Cell {
    pub const BLANK: Cell = Cell { ch: ' ', fg: Color::Default, bg: Color::Default, attrs: Attrs(0), width: 1 };

    /// A blank cell that keeps the background of the pen (erase with the current background).
    pub const fn blank_with(bg: Color) -> Cell {
        Cell { bg, ..Cell::BLANK }
    }

    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.bg == Color::Default && !self.attrs.has(Attrs::INVERSE)
    }
}

impl Default for Cell {
    fn default() -> Self {
        Cell::BLANK
    }
}

/// One row of the grid. `wrapped` is true when the text continues on the next row (a soft wrap), so a copied
/// selection joins the two without a newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    pub fn blank(cols: usize, bg: Color) -> Row {
        Row { cells: vec![Cell::blank_with(bg); cols], wrapped: false }
    }

    /// The row's text without trailing blanks (spacers of wide glyphs are skipped).
    pub fn text(&self) -> String {
        let end = self.cells.iter().rposition(|c| !c.is_blank()).map_or(0, |i| i + 1);
        self.cells[..end].iter().filter(|c| c.width != 0).map(|c| c.ch).collect()
    }
}

/// Display width of a character: 0 for combining and zero-width marks, 2 for the common wide ranges (CJK, emoji), else 1.
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7f..0xa0).contains(&u) {
        return 0;
    }
    if u < 0x300 {
        return 1;
    }
    match u {
        0x0300..=0x036f
        | 0x200b..=0x200f
        | 0x2028..=0x202e
        | 0x2060..=0x2064
        | 0xfe00..=0xfe0f
        | 0xfeff
        | 0x20d0..=0x20ff => 0,
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f680..=0x1f6ff
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}
