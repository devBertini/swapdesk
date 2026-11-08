use super::*;

fn term(cols: usize, rows: usize) -> Term {
    Term::new(cols, rows, 100)
}

fn text(t: &Term) -> String {
    t.screen().visible_text()
}

#[test]
fn printing_wraps_at_the_last_column_and_marks_the_soft_wrap() {
    let mut t = term(5, 3);
    t.feed(b"abcdefgh");
    assert_eq!(text(&t), "abcde\nfgh\n");
    assert!(t.screen().visible_row(0).wrapped);
    assert_eq!(t.screen().cursor().col, 3);
}

#[test]
fn newline_scrolls_into_the_scrollback_and_the_viewport_scrolls_back() {
    let mut t = term(6, 2);
    t.feed(b"one\r\ntwo\r\nthree");
    assert_eq!(text(&t), "two\nthree");
    assert_eq!(t.screen().scrollback_len(), 1);
    t.screen_mut().scroll_view(1);
    assert_eq!(text(&t), "one\ntwo");
    assert!(!t.screen().cursor_shown());
    t.screen_mut().reset_view();
    assert_eq!(text(&t), "two\nthree");
}

#[test]
fn scrollback_is_bounded() {
    let mut t = Term::new(4, 2, 3);
    for i in 0..20 {
        t.feed(format!("l{i}\r\n").as_bytes());
    }
    assert_eq!(t.screen().scrollback_len(), 3);
}

#[test]
fn cursor_movement_and_erase() {
    let mut t = term(10, 3);
    t.feed(b"hello\x1b[2;3Hxy\x1b[1;1H\x1b[K");
    assert_eq!(text(&t), "\n  xy\n");
    t.feed(b"\x1b[2J\x1b[Hok");
    assert_eq!(text(&t), "ok\n\n");
}

#[test]
fn sgr_sets_colours_in_every_form() {
    let mut t = term(10, 1);
    t.feed(b"\x1b[1;31ma\x1b[38;5;200mb\x1b[38;2;1;2;3mc\x1b[48:2::9:8:7md\x1b[0me");
    let s = t.screen();
    assert_eq!(s.visible_cell(0, 0).fg, Color::Indexed(1));
    assert!(s.visible_cell(0, 0).attrs.has(Attrs::BOLD));
    assert_eq!(s.visible_cell(0, 1).fg, Color::Indexed(200));
    assert_eq!(s.visible_cell(0, 2).fg, Color::Rgb(1, 2, 3));
    assert_eq!(s.visible_cell(0, 3).bg, Color::Rgb(9, 8, 7));
    assert_eq!(s.visible_cell(0, 4).fg, Color::Default);
    assert_eq!(s.visible_cell(0, 4).attrs, Attrs(0));
}

#[test]
fn utf8_split_across_chunks_and_wide_glyphs() {
    let mut t = term(6, 1);
    let bytes = "a世b".as_bytes();
    t.feed(&bytes[..2]);
    t.feed(&bytes[2..]);
    assert_eq!(text(&t), "a世b");
    assert_eq!(t.screen().visible_cell(0, 1).width, 2);
    assert_eq!(t.screen().visible_cell(0, 2).width, 0);
    assert_eq!(t.screen().cursor().col, 4);
}

#[test]
fn the_alternate_screen_leaves_the_primary_untouched() {
    let mut t = term(6, 2);
    t.feed(b"main\x1b[?1049h\x1b[Hfull\x1b[?1049l");
    assert_eq!(text(&t), "main\n");
    assert!(!t.screen().modes().alt_screen);
    t.feed(b"\x1b[?1049h");
    assert_eq!(text(&t), "\n");
    t.screen_mut().scroll_view(5);
    assert_eq!(t.screen().view_offset(), 0, "no history on the alternate screen");
}

#[test]
fn queries_are_answered_for_the_pty() {
    let mut t = term(10, 5);
    t.feed(b"\x1b[3;4H\x1b[6n\x1b[c");
    let replies = t.screen_mut().take_replies();
    assert_eq!(replies, b"\x1b[3;4R\x1b[?1;2c");
    assert!(t.screen_mut().take_replies().is_empty());
}

#[test]
fn modes_title_and_bell() {
    let mut t = term(10, 2);
    t.feed(b"\x1b[?1h\x1b[?2004h\x1b[?25l\x1b]0;my title\x07\x07");
    let m = t.screen().modes();
    assert!(m.app_cursor && m.bracketed_paste && !m.cursor_visible);
    assert_eq!(t.screen().title(), "my title");
    assert_eq!(t.screen_mut().take_bells(), 1, "the first BEL ends the title, the second rings");
}

#[test]
fn scroll_region_keeps_rows_outside_it() {
    let mut t = term(4, 4);
    t.feed(b"top\r\n1\r\n2\r\nbot\x1b[2;3r\x1b[3;1H\n");
    assert_eq!(text(&t), "top\n2\n\nbot");
    assert_eq!(t.screen().scrollback_len(), 0, "a region that does not start at row 0 keeps no history");
}

#[test]
fn insert_and_delete_characters_and_lines() {
    let mut t = term(6, 3);
    t.feed(b"abcdef\x1b[1;3H\x1b[2@\x1b[1;1H\x1b[P");
    assert_eq!(text(&t), "b  cd\n\n");
    t.feed(b"\x1b[2;1Hx\x1b[1;1H\x1b[L");
    assert_eq!(text(&t), "\nb  cd\nx");
}

#[test]
fn damage_lists_only_the_changed_rows() {
    let mut t = term(8, 4);
    assert_eq!(t.screen_mut().take_damage(), Damage::All);
    assert_eq!(t.screen_mut().take_damage(), Damage::None);
    t.feed(b"\x1b[3;1Hx");
    assert_eq!(t.screen_mut().take_damage(), Damage::Rows(vec![0, 2]), "the cursor's old row redraws too");
    t.feed(b"y");
    assert_eq!(t.screen_mut().take_damage(), Damage::Rows(vec![2]));
}

#[test]
fn resize_keeps_the_cursor_row_and_moves_history() {
    let mut t = term(5, 4);
    t.feed(b"a\r\nb\r\nc\r\nd");
    t.resize(5, 2);
    assert_eq!(text(&t), "c\nd");
    assert_eq!(t.screen().scrollback_len(), 2);
    t.resize(5, 4);
    assert_eq!(text(&t), "a\nb\nc\nd");
    assert_eq!(t.screen().cursor().row, 3);
    t.resize(3, 4);
    assert_eq!(t.screen().cols(), 3);
}

#[test]
fn selection_copies_text_across_rows_and_follows_the_scroll() {
    let mut t = term(8, 4);
    t.feed(b"hello world\r\nsecond\r\n");
    // "hello wo" wrapped into "rld".
    t.screen_mut().select_start(0, 6);
    t.screen_mut().select_update(0, 7);
    assert_eq!(t.screen().selection_text().unwrap(), "wo");
    assert!(t.screen().is_selected(0, 6) && !t.screen().is_selected(0, 5));
    // Drag across the soft wrap: no newline between the wrapped halves.
    t.screen_mut().select_start(0, 6);
    t.screen_mut().select_update(1, 2);
    assert_eq!(t.screen().selection_text().unwrap(), "world");
    // More output scrolls the text away; the selection stays on it.
    t.feed(b"x\r\ny\r\n");
    assert_eq!(t.screen().selection_text().unwrap(), "world");
    t.screen_mut().select_clear();
    assert!(t.screen().selection_text().is_none());
}

#[test]
fn double_and_triple_click_and_select_all() {
    let mut t = term(20, 3);
    t.feed(b"foo bar(baz)\r\nline two");
    t.screen_mut().select_word(0, 5);
    assert_eq!(t.screen().selection_text().unwrap(), "bar");
    t.screen_mut().select_line(1);
    assert_eq!(t.screen().selection_text().unwrap(), "line two");
    t.screen_mut().select_all();
    assert_eq!(t.screen().selection_text().unwrap(), "foo bar(baz)\nline two\n");
}

#[test]
fn keys_encode_as_xterm_does() {
    let n = Mods::NONE;
    assert_eq!(encode_key(Key::Char('a'), n, false), b"a");
    assert_eq!(encode_key(Key::Char('c'), Mods { ctrl: true, ..n }, false), [3]);
    assert_eq!(encode_key(Key::Char('x'), Mods { alt: true, ..n }, false), b"\x1bx");
    assert_eq!(encode_key(Key::Char('é'), n, false), "é".as_bytes());
    assert_eq!(encode_key(Key::Enter, n, false), b"\r");
    assert_eq!(encode_key(Key::Backspace, n, false), [0x7f]);
    assert_eq!(encode_key(Key::Up, n, false), b"\x1b[A");
    assert_eq!(encode_key(Key::Up, n, true), b"\x1bOA");
    assert_eq!(encode_key(Key::Left, Mods { ctrl: true, ..n }, true), b"\x1b[1;5D");
    assert_eq!(encode_key(Key::Delete, n, false), b"\x1b[3~");
    assert_eq!(encode_key(Key::F(1), n, false), b"\x1bOP");
    assert_eq!(encode_key(Key::F(5), n, false), b"\x1b[15~");
    assert_eq!(encode_key(Key::F(12), Mods { shift: true, ..n }, false), b"\x1b[24;2~");
    assert!(encode_key(Key::F(13), n, false).is_empty());
}

#[test]
fn paste_is_bracketed_and_cannot_inject_an_escape() {
    assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc");
    assert_eq!(encode_paste("x\x1b[201~y", true), b"\x1b[200~x[201~y\x1b[201~");
}

#[test]
fn the_pump_respects_its_budget_and_reports_the_end() {
    let (mut v, tx) = TermView::with_capacity(10, 2, 10, 4);
    for _ in 0..4 {
        tx.send(b"abcd".to_vec()).unwrap();
    }
    assert_eq!(tx.try_send(b"x".to_vec()), Err(Some(b"x".to_vec())), "a full channel hands the chunk back");
    let p = v.pump(8);
    assert_eq!((p.bytes, p.chunks, p.more, p.closed), (8, 2, true, false));
    let p = v.pump(1000);
    assert_eq!((p.bytes, p.more, p.closed), (8, false, false));
    assert_eq!(v.term.screen().visible_row(0).text(), "abcdabcdab");
    tx.send(b"\x1b[6n".to_vec()).unwrap();
    assert_eq!(v.pump(100).replies, b"\x1b[2;7R");
    drop(tx);
    assert!(v.pump(100).closed);
}

#[test]
fn garbage_never_panics() {
    let mut t = term(7, 5);
    let mut x: u32 = 12345;
    let mut buf = Vec::new();
    for _ in 0..20_000 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        buf.push((x >> 24) as u8);
    }
    for chunk in buf.chunks(97) {
        t.feed(chunk);
        let _ = t.screen_mut().take_damage();
    }
    t.resize(3, 2);
    t.screen_mut().select_all();
    let _ = t.screen().selection_text();
}
