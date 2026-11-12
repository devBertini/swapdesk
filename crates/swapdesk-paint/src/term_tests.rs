//! The terminal panel over the shared painter: colours, the model as a grid, the router, the panel and the keys.

use swapdesk_termview::{Key, Mods, TermView};
use swapdesk_ui::Command;
use swapdesk_ui::tokens::fixed;

use super::*;
use crate::tests::Fixed;

/// A scripted grid: `lines` of text, cell overrides, a cursor and a selection.
struct Fake {
    cols: usize,
    lines: Vec<&'static str>,
    paint: Vec<((usize, usize), TermCell)>,
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
    fn cell(&self, row: usize, col: usize) -> TermCell {
        if let Some((_, c)) = self.paint.iter().find(|(p, _)| *p == (row, col)) {
            return *c;
        }
        TermCell { ch: self.lines[row].chars().nth(col).unwrap_or(' '), ..TermCell::BLANK }
    }
    fn cursor(&self) -> Option<(usize, usize)> {
        self.cursor
    }
    fn selected(&self, row: usize, col: usize) -> bool {
        self.selected.contains(&(row, col))
    }
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
    let inv = resolve(&TermCell { inverse: true, ch: 'x', ..TermCell::BLANK }, false, false);
    assert_eq!((inv.fg, inv.bg), (fixed::TERMINAL_BG, Some(fixed::TERMINAL_INK)));
    let hidden = resolve(&TermCell { hidden: true, ch: 'x', ..TermCell::BLANK }, false, false);
    assert!(hidden.text.is_empty(), "a hidden cell draws no glyph");
    assert_eq!(resolve(&TermCell::BLANK, false, false).bg, None, "the panel's own background is not painted again");
    assert_eq!(resolve(&TermCell::BLANK, true, false).bg, Some(SELECTION));
    assert_eq!(
        resolve(&TermCell::BLANK, true, true).bg,
        Some(fixed::TERMINAL_INK),
        "the cursor wins over the selection"
    );
    let wide = resolve(&TermCell { ch: '漢', width: 2, ..TermCell::BLANK }, false, false);
    assert!(wide.wide && wide.text == "漢");
    assert!(resolve(&TermCell { ch: '漢', width: 0, ..TermCell::BLANK }, false, false).text.is_empty(), "the spacer");
}

#[test]
fn the_grid_fits_the_area_and_a_click_finds_its_cell() {
    let p = Fixed::default();
    let m = cell_metrics(&p);
    assert_eq!((m.cell_w, m.cell_h), (8.0, 17.0), "the cell box of the shared grid");
    let (cols, rows) = grid_for(m, Rect::new(0.0, 0.0, 800.0, 480.0));
    assert!(cols as f32 * m.cell_w <= 800.0 - 2.0 * PADDING && rows as f32 * m.cell_h <= 480.0 - 2.0 * PADDING);
    assert!((cols + 1) as f32 * m.cell_w > 800.0 - 2.0 * PADDING, "no column is wasted");
    assert_eq!(grid_for(m, Rect::new(0.0, 0.0, 1.0, 1.0)), (1, 1), "never empty");
    assert_eq!(
        grid_for(m, Rect::new(0.0, 0.0, 1.0e6, 1.0e6)),
        (MAX_CELLS, MAX_CELLS),
        "capped at the protocol's limit"
    );
    let area = Rect::new(10.0, 20.0, 800.0, 480.0);
    let at = |x, y| cell_at(m, area, (cols, rows), x, y);
    assert_eq!(at(10.0 + PADDING + m.cell_w * 3.5, 20.0 + PADDING + m.cell_h * 2.5), (2, 3));
    assert_eq!(at(-50.0, -50.0), (0, 0), "a drag outside clamps");
    assert_eq!(at(5000.0, 5000.0), (rows - 1, cols - 1));
}

#[test]
fn a_frame_paints_backgrounds_text_selection_and_cursor() {
    let red = TermCell { bg: Paint::Indexed(1), ..TermCell::BLANK };
    let grid = Fake {
        cols: 20,
        lines: vec!["hello", "", "wide"],
        paint: vec![((1, 2), red)],
        cursor: Some((2, 5)),
        selected: vec![(2, 0)],
    };
    let p = Fixed::default();
    let area = Rect::new(0.0, 0.0, 400.0, 200.0);
    let m = draw(&p, area, &grid, true);
    assert_eq!(p.clips.get(), 0, "the clip is popped");
    let fills = p.fills.borrow();
    assert_eq!(fills[0], (area, fixed::TERMINAL_BG, false), "the panel's background first");
    let cell = |row: usize, col: usize| {
        Rect::new(PADDING + col as f32 * m.cell_w, PADDING + row as f32 * m.cell_h, m.cell_w, m.cell_h)
    };
    let has = |r: Rect, c: Color| fills.iter().any(|(fr, fc, _)| *fr == r && *fc == c);
    assert!(has(cell(1, 2), Color::rgb(0xE06C75)), "a cell with a background");
    assert!(has(cell(2, 0), SELECTION), "a selected cell");
    assert!(has(cell(2, 5), fixed::TERMINAL_INK), "the focused cursor is a filled block");
    let drawn = p.drawn.borrow();
    assert_eq!(drawn.join(""), "hellowide", "every glyph, no spaces");
}

#[test]
fn an_unfocused_cursor_is_an_outline() {
    let grid = Fake { cols: 10, lines: vec!["x"], paint: vec![], cursor: Some((0, 3)), selected: vec![] };
    let p = Fixed::default();
    let m = draw(&p, Rect::new(0.0, 0.0, 200.0, 50.0), &grid, false);
    let x = PADDING + 3.0 * m.cell_w;
    let ink: Vec<Rect> =
        p.fills.borrow().iter().filter(|(_, c, _)| *c == fixed::TERMINAL_INK).map(|(r, ..)| *r).collect();
    assert_eq!(ink.len(), 4, "four edges: {ink:?}");
    assert!(ink.contains(&Rect::new(x, PADDING, 1.0, m.cell_h)), "the left edge, one device pixel");
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
    assert_eq!(pane.visible_text().unwrap().lines().next().unwrap().trim_end(), "one two");
    // After attaching it flows straight in.
    terminal_output(id, b"!".to_vec());
    assert!(pane.pump(PUMP_BUDGET).repaint);
    // Another id's output does not reach this panel, and a dropped panel takes its tap with it.
    terminal_output(id + 1, b"other".to_vec());
    assert!(!pane.pump(PUMP_BUDGET).repaint);
    pane.sync(None);
    assert!(!router().taps.contains_key(&id));
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
    router().parked.clear();
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
    let shift = Mods { shift: true, ..none };
    let send = |k: Key, m: Mods| key_action(Some(k), m, 30, false, false);
    assert_eq!(send(Key::Enter, none), KeyAction::Send(b"\r".to_vec()), "Enter");
    assert_eq!(send(Key::Backspace, none), KeyAction::Send(vec![0x7f]), "Backspace");
    assert_eq!(send(Key::Tab, none), KeyAction::Send(b"\t".to_vec()), "Tab goes to the shell");
    assert_eq!(send(Key::BackTab, shift), KeyAction::Send(b"\x1b[Z".to_vec()));
    assert_eq!(send(Key::Tab, ctrl), KeyAction::Pass, "Ctrl+Tab leaves the grid");
    assert_eq!(send(Key::Escape, none), KeyAction::Send(vec![0x1b]));
    assert_eq!(send(Key::Up, none), KeyAction::Send(b"\x1b[A".to_vec()), "Up");
    assert_eq!(
        key_action(Some(Key::Up), none, 30, false, true),
        KeyAction::Send(b"\x1bOA".to_vec()),
        "Up in application-cursor mode"
    );
    assert_eq!(send(Key::F(5), none), KeyAction::Send(b"\x1b[15~".to_vec()), "F5");
    assert_eq!(send(Key::Char('d'), ctrl), KeyAction::Send(vec![0x04]), "Ctrl+D");
    assert_eq!(send(Key::Char(' '), ctrl), KeyAction::Send(vec![0]), "Ctrl+Space");
    assert_eq!(send(Key::Char('c'), ctrl), KeyAction::CopyOrInterrupt);
    assert_eq!(send(Key::Char('c'), Mods { shift: true, ..ctrl }), KeyAction::Copy);
    assert_eq!(send(Key::Char('v'), ctrl), KeyAction::Paste);
    assert_eq!(send(Key::Insert, shift), KeyAction::Paste, "Shift+Insert");
    // AltGr is Ctrl+Alt: its characters arrive as text, so no control code is made from the key.
    assert_eq!(send(Key::Char('q'), Mods { ctrl: true, alt: true, ..none }), KeyAction::Pass);
    // A plain letter is text, not a key; an unknown key is the window's.
    assert_eq!(send(Key::Char('a'), none), KeyAction::Pass);
    assert_eq!(key_action(None, none, 30, false, false), KeyAction::Pass);
    // Shift+PageUp scrolls the history by a page less one line, except on the alternate screen.
    assert_eq!(send(Key::PageUp, shift), KeyAction::Scroll(29));
    assert_eq!(send(Key::PageDown, shift), KeyAction::Scroll(-29));
    assert_eq!(
        key_action(Some(Key::PageUp), shift, 30, true, false),
        KeyAction::Send(b"\x1b[5;2~".to_vec()),
        "a full-screen program gets the key"
    );
    assert_eq!(send(Key::PageUp, none), KeyAction::Send(b"\x1b[5~".to_vec()));
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
    // The Mac hands whole strings: control characters left out.
    assert_eq!(pane.text_bytes("é😀"), Some("é😀".as_bytes().to_vec()));
    assert_eq!(pane.text_bytes("\r"), None);
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
    let mut pane = TerminalPane::default();
    pane.sync(Some(id));
    terminal_output(id, b"hello world\r\nsecond line".to_vec());
    pane.pump(PUMP_BUDGET);
    let p = Fixed::default();
    let area = Rect::new(10.0, 20.0, 400.0, 200.0);
    pane.paint(&p, Some(area), true);
    let m = cell_metrics(&p);
    let at = |col: f32, row: f32| (10.0 + PADDING + m.cell_w * (col + 0.5), 20.0 + PADDING + m.cell_h * (row + 0.5));
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
    let p = Fixed::default();
    let area = Rect::new(0.0, 0.0, 400.0, 200.0);
    let commands = pane.paint(&p, Some(area), true);
    let (cols, rows) = grid_for(cell_metrics(&p), area);
    assert!((cols, rows) != (80, 24), "a 400 x 200 panel is not 80 x 24");
    assert_eq!(commands, vec![Command::TermResize { term_id: id, cols: cols as u16, rows: rows as u16 }]);
    assert_eq!(pane.grid(), Some((cols, rows)), "the model has the size the host was told");
    let m = cell_metrics(&p);
    let first = Rect::new(PADDING, PADDING, 4.0 * m.cell_w, m.cell_h);
    assert!(p.fills.borrow().iter().any(|(r, c, _)| *r == first && *c == Color::rgb(0xE06C75)), "the red run");
    // The same size again asks for nothing; a new area asks again.
    assert!(pane.paint(&p, Some(area), true).is_empty());
    let bigger = pane.paint(&p, Some(Rect::new(0.0, 0.0, 400.0, 100.0)), true);
    assert!(matches!(bigger[..], [Command::TermResize { .. }]));
    // Without an area (the panel is not on screen) nothing is drawn and the mouse finds no grid.
    let before = p.fills.borrow().len();
    assert!(pane.paint(&p, None, false).is_empty());
    assert_eq!(p.fills.borrow().len(), before);
    assert!(!pane.contains(5.0, 5.0));
    pane.sync(None);
}
