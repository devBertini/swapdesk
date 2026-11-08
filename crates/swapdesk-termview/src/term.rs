//! The parser and the screen it drives.

use vte::Parser;

use crate::screen::Screen;

/// A terminal: `vte`'s parser feeding a [`Screen`].
pub struct Term {
    parser: Parser,
    screen: Screen,
}

impl Term {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Term {
        Term { parser: Parser::new(), screen: Screen::new(cols, rows, scrollback) }
    }

    /// Parses host output. A sequence cut at the end of `bytes` continues with the next call.
    pub fn feed(&mut self, bytes: &[u8]) {
        let before = self.screen.cursor();
        self.parser.advance(&mut self.screen, bytes);
        let after = self.screen.cursor();
        if (before.row, before.col) != (after.row, after.col) {
            self.screen.mark(before.row);
            self.screen.mark(after.row);
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols, rows);
    }
}
