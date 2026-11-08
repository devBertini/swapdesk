//! The VT command set the screen understands, driven by the `vte` parser (D19): cursor movement, erasing, scrolling
//! regions, SGR colours, the alternate screen, the private modes the key encoder needs, titles and the few queries
//! programs ask (cursor position, device attributes).

use vte::{Params, Perform};

use crate::cell::{Attrs, Color};
use crate::screen::{Pen, Screen};

/// The first value of every parameter (subparameters dropped), at most 32.
struct Flat {
    v: [u16; 32],
    n: usize,
}

impl Flat {
    fn of(params: &Params) -> Flat {
        let mut f = Flat { v: [0; 32], n: 0 };
        for p in params.iter().take(32) {
            f.v[f.n] = p.first().copied().unwrap_or(0);
            f.n += 1;
        }
        f
    }

    /// Parameter `i`, or `default` when absent or 0.
    fn get(&self, i: usize, default: usize) -> usize {
        if i < self.n && self.v[i] != 0 { usize::from(self.v[i]) } else { default }
    }

    /// Parameter `i` as given, 0 when absent.
    fn raw(&self, i: usize) -> usize {
        if i < self.n { usize::from(self.v[i]) } else { 0 }
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell += 1,
            0x08 => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.cursor.pending_wrap = false;
            }
            0x09 => self.tab_forward(1),
            0x0a..=0x0c => self.linefeed(),
            0x0d => {
                self.cursor.col = 0;
                self.cursor.pending_wrap = false;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let [kind, text, ..] = params
            && matches!(*kind, b"0" | b"2")
        {
            self.title = String::from_utf8_lossy(text).chars().filter(|c| !c.is_control()).take(256).collect();
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.cursor.col = 0;
                self.linefeed();
            }
            ([], b'M') => self.reverse_index(),
            ([], b'H') => {
                let c = self.cursor.col;
                self.tabs[c] = true;
            }
            ([], b'c') => self.full_reset(),
            ([b'('], b'0') => self.gfx = true,
            ([b'('], _) => self.gfx = false,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let p = Flat::of(params);
        let private = intermediates.first() == Some(&b'?');
        if !intermediates.is_empty() && !private {
            // `CSI > c`, `CSI ! p` and the like: only the secondary attributes query is answered.
            if intermediates == b">" && action == 'c' {
                self.replies.extend_from_slice(b"\x1b[>0;0;0c");
            }
            return;
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        let n = p.get(0, 1);
        match action {
            'A' => self.move_to(row.saturating_sub(n).max(if row >= self.top { self.top } else { 0 }), col),
            'B' => self.move_to((row + n).min(if row <= self.bottom { self.bottom } else { self.rows - 1 }), col),
            'C' => self.move_to(row, col + n),
            'D' => self.move_to(row, col.saturating_sub(n)),
            'E' => self.move_to((row + n).min(self.rows - 1), 0),
            'F' => self.move_to(row.saturating_sub(n), 0),
            'G' | '`' => self.move_to(row, n - 1),
            'H' | 'f' => self.move_to(p.get(0, 1) - 1, p.get(1, 1) - 1),
            'd' => self.move_to(n - 1, col),
            'I' => self.tab_forward(n),
            'Z' => self.tab_back(n),
            'J' => match p.raw(0) {
                0 => {
                    self.erase_cells(row, col, self.cols);
                    self.erase_rows(row + 1, self.rows);
                }
                1 => {
                    self.erase_rows(0, row);
                    self.erase_cells(row, 0, col + 1);
                }
                2 | 3 => self.erase_rows(0, self.rows),
                _ => {}
            },
            'K' => match p.raw(0) {
                0 => self.erase_cells(row, col, self.cols),
                1 => self.erase_cells(row, 0, col + 1),
                2 => self.erase_cells(row, 0, self.cols),
                _ => {}
            },
            'L' => self.insert_lines(n, true),
            'M' => self.insert_lines(n, false),
            '@' => self.insert_cells(n),
            'P' => self.delete_cells(n),
            'X' => self.erase_cells(row, col, col + n),
            'S' => {
                for _ in 0..n.min(self.rows) {
                    self.scroll_up_one();
                }
            }
            'T' => {
                for _ in 0..n.min(self.rows) {
                    self.scroll_down_one();
                }
            }
            'r' => {
                let bottom = if p.raw(1) == 0 { self.rows } else { p.raw(1) };
                self.set_region(p.get(0, 1) - 1, bottom - 1);
            }
            's' if !private => self.save_cursor(),
            'u' if !private => self.restore_cursor(),
            'm' => self.sgr(params),
            'h' | 'l' => {
                let on = action == 'h';
                for i in 0..p.n {
                    self.set_mode(usize::from(p.v[i]), private, on);
                }
            }
            'n' => match p.raw(0) {
                5 if !private => self.replies.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let report = format!("\x1b[{}{};{}R", if private { "?" } else { "" }, row + 1, col + 1);
                    self.replies.extend_from_slice(report.as_bytes());
                }
                _ => {}
            },
            'c' if !private && p.raw(0) == 0 => self.replies.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }
}

impl Screen {
    fn set_mode(&mut self, mode: usize, private: bool, on: bool) {
        if !private {
            return;
        }
        match mode {
            1 => self.modes.app_cursor = on,
            7 => self.modes.autowrap = on,
            25 => {
                self.modes.cursor_visible = on;
                let r = self.cursor.row;
                self.mark(r);
            }
            47 | 1047 => self.set_alt_screen(on, false),
            1049 => self.set_alt_screen(on, true),
            2004 => self.modes.bracketed_paste = on,
            _ => {}
        }
    }

    fn sgr(&mut self, params: &Params) {
        let list: Vec<&[u16]> = params.iter().collect();
        if list.is_empty() {
            self.pen = Pen::DEFAULT;
            return;
        }
        let mut i = 0;
        while i < list.len() {
            let sub = list[i];
            let code = sub.first().copied().unwrap_or(0);
            i += 1;
            let pen = &mut self.pen;
            match code {
                0 => *pen = Pen::DEFAULT,
                1 => pen.attrs.set(Attrs::BOLD, true),
                2 => pen.attrs.set(Attrs::DIM, true),
                3 => pen.attrs.set(Attrs::ITALIC, true),
                4 => pen.attrs.set(Attrs::UNDERLINE, sub.get(1) != Some(&0)),
                7 => pen.attrs.set(Attrs::INVERSE, true),
                8 => pen.attrs.set(Attrs::HIDDEN, true),
                9 => pen.attrs.set(Attrs::STRIKE, true),
                21 | 22 => {
                    pen.attrs.set(Attrs::BOLD, false);
                    pen.attrs.set(Attrs::DIM, false);
                }
                23 => pen.attrs.set(Attrs::ITALIC, false),
                24 => pen.attrs.set(Attrs::UNDERLINE, false),
                27 => pen.attrs.set(Attrs::INVERSE, false),
                28 => pen.attrs.set(Attrs::HIDDEN, false),
                29 => pen.attrs.set(Attrs::STRIKE, false),
                30..=37 => pen.fg = Color::Indexed((code - 30) as u8),
                39 => pen.fg = Color::Default,
                40..=47 => pen.bg = Color::Indexed((code - 40) as u8),
                49 => pen.bg = Color::Default,
                90..=97 => pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => pen.bg = Color::Indexed((code - 100 + 8) as u8),
                38 | 48 => {
                    // Either `38;5;n` / `38;2;r;g;b` as separate parameters, or `38:5:n` / `38:2::r:g:b` as subparameters.
                    let color = if sub.len() > 1 {
                        extended_color(&sub[1..], true)
                    } else {
                        let rest: Vec<u16> = list[i..].iter().map(|s| s.first().copied().unwrap_or(0)).collect();
                        let used = match rest.first() {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => rest.len().min(1),
                        };
                        let c = extended_color(&rest, false);
                        i += used.min(rest.len());
                        c
                    };
                    if let Some(c) = color {
                        if code == 38 {
                            self.pen.fg = c;
                        } else {
                            self.pen.bg = c;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// `5;n` or `2;r;g;b`; the colon form may carry an empty colour-space slot (`2::r:g:b`).
fn extended_color(v: &[u16], colon: bool) -> Option<Color> {
    let rgb = |r: u16, g: u16, b: u16| Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8);
    match v {
        [5, n, ..] => Some(Color::Indexed((*n).min(255) as u8)),
        [2, _, r, g, b, ..] if colon => Some(rgb(*r, *g, *b)),
        [2, r, g, b, ..] => Some(rgb(*r, *g, *b)),
        _ => None,
    }
}
