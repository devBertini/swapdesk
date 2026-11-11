//! What to inject, as plain data: the pure half of the sink (no Win32 types, so it is unit tested on any host).
//!
//! Keyboard-page keys go in by scan code (`KEYEVENTF_SCANCODE`, with the extended flag for `0xE0xx`): the host layout
//! then decides the character, which is what "Teclas físicas" means and what makes AltGr work (windows.md, C4).
//! Consumer keys and Pause go in by virtual key: the layout tables have no usable scan code for them (Pause is
//! `E1 1D 45` on the wire of a real keyboard; scan `0x45` without `E0` would be read as NumLock's make code).

use swapdesk_keys::{PAGE_CONSUMER, PAGE_KEYBOARD, key_info, scan_parts};
use swapdesk_proto::MouseButton;

/// One keyboard injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KbInput {
    pub kind: KbKind,
    pub up: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KbKind {
    /// `KEYEVENTF_SCANCODE`: the set-1 make code and the `E0` flag.
    Scan { code: u8, extended: bool },
    /// By virtual key, with the scan code and extended flag kept for the receiver.
    Vk { vk: u16, scan: u8, extended: bool },
    /// `KEYEVENTF_UNICODE`: one UTF-16 code unit.
    Unicode(u16),
}

/// One mouse injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseInput {
    /// `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`: 0 to 65535 across the virtual desktop.
    MoveAbs {
        x: i32,
        y: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    /// Vertical wheel in 1/120 notch, positive away from the user.
    Wheel(i32),
    /// Horizontal wheel in 1/120 notch, positive to the right.
    HWheel(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInput {
    Key(KbInput),
    Mouse(MouseInput),
}

/// The injection for a physical key, `None` when the table has no Windows code for it.
pub fn key_input(page: u16, usage: u16, down: bool) -> Option<KbInput> {
    let info = key_info(page, usage)?;
    let scan = info.win_scan?;
    let (code, extended) = scan_parts(scan);
    let kind = if info.page == PAGE_CONSUMER {
        KbKind::Vk { vk: info.win_vk?, scan: code, extended }
    } else if info.page == PAGE_KEYBOARD && info.usage == 0x48 {
        KbKind::Vk { vk: info.win_vk?, scan: 0, extended: false }
    } else {
        KbKind::Scan { code, extended }
    };
    Some(KbInput { kind, up: !down })
}

const SCAN_RETURN: u8 = 0x1C;
const SCAN_TAB: u8 = 0x0F;

/// The key of the host's keyboard layout that types a character with no modifier but Shift, as the layout says
/// (`VkKeyScanExW`): its set-1 scan code, whether Shift is needed with Caps Lock off, and whether Caps Lock changes it
/// (a letter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharKey {
    pub code: u8,
    pub shift: bool,
    pub letter: bool,
}

/// What the sink knows of the host while it types text: whether Caps Lock is on (`None`: not known), the scan codes of
/// the Shift keys it holds, and whether it holds Ctrl, Alt or the Windows key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypingState {
    pub caps: Option<bool>,
    pub shifts_held: Vec<u8>,
    pub other_modifier_held: bool,
}

const SCAN_LSHIFT: u8 = 0x2A;

fn unicode_inputs(c: char, out: &mut Vec<KbInput>) {
    let mut buf = [0u16; 2];
    for unit in c.encode_utf16(&mut buf) {
        for up in [false, true] {
            out.push(KbInput { kind: KbKind::Unicode(*unit), up });
        }
    }
}

/// The injections that type `text`: per UTF-16 unit a key down and a key up (a character outside the BMP is its two
/// surrogates in order, as `KEYEVENTF_UNICODE` expects). Line breaks and tabs go as Enter and Tab keys; other control
/// characters are skipped.
pub fn text_inputs(text: &str) -> Vec<KbInput> {
    text_inputs_on(text, &|_| None, &TypingState::default())
}

/// [`text_inputs`] on a host whose layout `key_for` reads: an ASCII letter or digit that layout types with a plain or
/// shifted key goes as that key, by scan code, with Shift pressed or let go around it as the character and Caps Lock
/// need. A `KEYEVENTF_UNICODE` record carries the character in its scan code field, so a low-level keyboard hook reads
/// `f` (0x66) as the scan code 0x66: a laptop's hotkey service (seen on an Acer, 11/11) swallows it and shows its
/// microphone indicator, and the `f` never arrives. Everything else, and every character while Caps Lock is unknown or
/// another modifier is held, goes as Unicode.
pub fn text_inputs_on(text: &str, key_for: &dyn Fn(char) -> Option<CharKey>, host: &TypingState) -> Vec<KbInput> {
    let mut out = Vec::with_capacity(text.len() * 2);
    let scan = |code: u8, up: bool| KbInput { kind: KbKind::Scan { code, extended: false }, up };
    for ch in text.chars() {
        match ch {
            '\r' | '\n' | '\t' => {
                let code = if ch == '\t' { SCAN_TAB } else { SCAN_RETURN };
                for up in [false, true] {
                    out.push(scan(code, up));
                }
            }
            c if c.is_control() => {}
            c => {
                let key = if c.is_ascii_alphanumeric() && !host.other_modifier_held { key_for(c) } else { None };
                let Some((key, caps)) = key.zip(host.caps) else {
                    unicode_inputs(c, &mut out);
                    continue;
                };
                let shift = key.shift ^ (caps && key.letter);
                let held = !host.shifts_held.is_empty();
                if shift && !held {
                    out.push(scan(SCAN_LSHIFT, false));
                } else if !shift && held {
                    out.extend(host.shifts_held.iter().map(|code| scan(*code, true)));
                }
                out.push(scan(key.code, false));
                out.push(scan(key.code, true));
                if shift && !held {
                    out.push(scan(SCAN_LSHIFT, true));
                } else if !shift && held {
                    out.extend(host.shifts_held.iter().map(|code| scan(*code, false)));
                }
            }
        }
    }
    out
}

/// The wheel injections for a record (zero deltas inject nothing).
pub fn wheel_inputs(dx: i16, dy: i16) -> Vec<MouseInput> {
    let mut out = Vec::new();
    if dy != 0 {
        out.push(MouseInput::Wheel(i32::from(dy)));
    }
    if dx != 0 {
        out.push(MouseInput::HWheel(i32::from(dx)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_numpad_go_by_scan_code_with_the_extended_flag_where_the_table_says() {
        assert_eq!(
            key_input(7, 0x04, true),
            Some(KbInput { kind: KbKind::Scan { code: 0x1E, extended: false }, up: false })
        );
        assert_eq!(
            key_input(7, 0x04, false),
            Some(KbInput { kind: KbKind::Scan { code: 0x1E, extended: false }, up: true })
        );
        assert_eq!(
            key_input(7, 0x52, true).map(|k| k.kind),
            Some(KbKind::Scan { code: 0x48, extended: true }),
            "arrow up"
        );
        assert_eq!(
            key_input(7, 0x60, true).map(|k| k.kind),
            Some(KbKind::Scan { code: 0x48, extended: false }),
            "numpad 8"
        );
        assert_eq!(
            key_input(7, 0xE6, true).map(|k| k.kind),
            Some(KbKind::Scan { code: 0x38, extended: true }),
            "AltGr"
        );
        assert_eq!(
            key_input(7, 0x53, true).map(|k| k.kind),
            Some(KbKind::Scan { code: 0x45, extended: true }),
            "NumLock"
        );
        assert_eq!(
            key_input(7, 0x87, true).map(|k| k.kind),
            Some(KbKind::Scan { code: 0x73, extended: false }),
            "ABNT2 /?"
        );
    }

    #[test]
    fn media_keys_and_pause_go_by_virtual_key() {
        assert_eq!(
            key_input(0x0C, 0xCD, true).map(|k| k.kind),
            Some(KbKind::Vk { vk: 0xB3, scan: 0x22, extended: true })
        );
        assert_eq!(
            key_input(7, 0x7F, true).map(|k| k.kind),
            Some(KbKind::Vk { vk: 0xAD, scan: 0x20, extended: true }),
            "volume alias"
        );
        assert_eq!(
            key_input(7, 0x48, true).map(|k| k.kind),
            Some(KbKind::Vk { vk: 0x13, scan: 0, extended: false }),
            "Pause"
        );
    }

    #[test]
    fn unknown_keys_inject_nothing() {
        assert_eq!(key_input(7, 0xFFFF, true), None);
        assert_eq!(key_input(0x0C, 0xB8 + 0x500, true), None);
    }

    #[test]
    fn text_is_down_up_per_utf16_unit_with_surrogate_pairs() {
        let t = text_inputs("ç");
        assert_eq!(t.len(), 2);
        assert_eq!(t[0], KbInput { kind: KbKind::Unicode(0xE7), up: false });
        assert_eq!(t[1], KbInput { kind: KbKind::Unicode(0xE7), up: true });
        // U+1F600 is D83D DE00.
        let e = text_inputs("\u{1F600}");
        let units: Vec<(u16, bool)> = e
            .iter()
            .map(|k| match k.kind {
                KbKind::Unicode(u) => (u, k.up),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(units, vec![(0xD83D, false), (0xD83D, true), (0xDE00, false), (0xDE00, true)]);
    }

    #[test]
    fn text_keeps_order_and_maps_line_breaks_and_tabs_to_keys() {
        let t = text_inputs("a\tb\n\u{7}");
        let kinds: Vec<KbKind> = t.iter().filter(|k| !k.up).map(|k| k.kind).collect();
        assert_eq!(
            kinds,
            vec![
                KbKind::Unicode(u16::from(b'a')),
                KbKind::Scan { code: 0x0F, extended: false },
                KbKind::Unicode(u16::from(b'b')),
                KbKind::Scan { code: 0x1C, extended: false },
            ]
        );
    }

    #[test]
    fn wheel_splits_into_vertical_and_horizontal_and_skips_zero() {
        assert_eq!(wheel_inputs(0, 0), vec![]);
        assert_eq!(wheel_inputs(0, 120), vec![MouseInput::Wheel(120)]);
        assert_eq!(wheel_inputs(-30, -60), vec![MouseInput::Wheel(-60), MouseInput::HWheel(-30)]);
        assert_eq!(wheel_inputs(i16::MIN, i16::MAX), vec![MouseInput::Wheel(32767), MouseInput::HWheel(-32768)]);
    }

    /// A US-like layout: letters on their set-1 keys, digits on the top row, shifted for capitals; nothing else.
    fn us(c: char) -> Option<CharKey> {
        let code = match c.to_ascii_lowercase() {
            'f' => 0x21,
            'g' => 0x22,
            '1' => 0x02,
            _ => return None,
        };
        Some(CharKey { code, shift: c.is_ascii_uppercase(), letter: c.is_ascii_alphabetic() })
    }

    fn sc(code: u8, up: bool) -> KbInput {
        KbInput { kind: KbKind::Scan { code, extended: false }, up }
    }

    fn caps(on: bool) -> TypingState {
        TypingState { caps: Some(on), ..TypingState::default() }
    }

    /// The defect of 11/11: a Unicode `f` carries 0x66 in its scan code field, which a laptop's hotkey hook swallowed
    /// (and showed its microphone indicator). On a layout that has the key, `f` goes as that key and no record of the
    /// text carries 0x66.
    #[test]
    fn a_letter_the_host_layout_has_goes_as_its_key_and_never_as_a_unicode_record() {
        let out = text_inputs_on("fg1", &us, &caps(false));
        assert_eq!(
            out,
            vec![sc(0x21, false), sc(0x21, true), sc(0x22, false), sc(0x22, true), sc(0x02, false), sc(0x02, true)]
        );
        assert!(out.iter().all(|k| !matches!(k.kind, KbKind::Unicode(_))), "{out:?}");
    }

    #[test]
    fn shift_is_pressed_or_let_go_around_a_key_as_the_case_and_caps_lock_need() {
        // A capital with no Shift held: Shift around it.
        assert_eq!(
            text_inputs_on("F", &us, &caps(false)),
            vec![sc(0x2A, false), sc(0x21, false), sc(0x21, true), sc(0x2A, true)]
        );
        // The viewer holds Shift (it went as a key): the capital needs nothing more.
        let shifted = TypingState { caps: Some(false), shifts_held: vec![0x2A], other_modifier_held: false };
        assert_eq!(text_inputs_on("F", &us, &shifted), vec![sc(0x21, false), sc(0x21, true)]);
        // A small letter under a held Shift: the Shift is let go around it and held again.
        assert_eq!(
            text_inputs_on("f", &us, &shifted),
            vec![sc(0x2A, true), sc(0x21, false), sc(0x21, true), sc(0x2A, false)]
        );
        // Caps Lock on the host turns the letter's case, not the digit's.
        assert_eq!(
            text_inputs_on("f", &us, &caps(true)),
            vec![sc(0x2A, false), sc(0x21, false), sc(0x21, true), sc(0x2A, true)]
        );
        assert_eq!(text_inputs_on("F", &us, &caps(true)), vec![sc(0x21, false), sc(0x21, true)]);
        assert_eq!(text_inputs_on("1", &us, &caps(true)), vec![sc(0x02, false), sc(0x02, true)]);
    }

    #[test]
    fn what_the_layout_lacks_or_an_unknown_caps_lock_or_a_held_ctrl_leaves_the_text_to_unicode() {
        let uni = |c: char| {
            vec![
                KbInput { kind: KbKind::Unicode(c as u16), up: false },
                KbInput { kind: KbKind::Unicode(c as u16), up: true },
            ]
        };
        assert_eq!(text_inputs_on("h", &us, &caps(false)), uni('h'), "not on this layout");
        assert_eq!(text_inputs_on("ç", &us, &caps(false)), uni('ç'), "not ASCII");
        assert_eq!(text_inputs_on("f", &us, &TypingState::default()), uni('f'), "Caps Lock unknown");
        let ctrl = TypingState { caps: Some(false), shifts_held: vec![], other_modifier_held: true };
        assert_eq!(text_inputs_on("f", &us, &ctrl), uni('f'), "a key under Ctrl would be a shortcut");
        assert_eq!(text_inputs("f"), uni('f'), "no layout: Unicode as before");
    }
}
