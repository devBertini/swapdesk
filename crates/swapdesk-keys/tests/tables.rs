//! Table tests of contract C4 (stage 16). The expected values below are written out by hand from the IBM set-1 scan
//! code layout and Apple's `kVK_*` constants, independently of the generated `keys.tsv`.

use std::collections::HashSet;
use swapdesk_keys::{
    Flags, KbKind, PAGE_CONSUMER, PAGE_KEYBOARD, canonical, entries, hid_to_mac, hid_to_win, key_info, mac_to_hid,
    scan_parts, win_to_hid, win_vk,
};

/// (usage, name, win scan, ANSI kVK) of every key of a US 104-key keyboard (plus the Apple Command keys by kVK).
fn us_keys() -> Vec<(u16, String, u16, u16)> {
    let mut v: Vec<(u16, String, u16, u16)> = Vec::new();
    let letters = [
        ('A', 0x1E, 0x00),
        ('B', 0x30, 0x0B),
        ('C', 0x2E, 0x08),
        ('D', 0x20, 0x02),
        ('E', 0x12, 0x0E),
        ('F', 0x21, 0x03),
        ('G', 0x22, 0x05),
        ('H', 0x23, 0x04),
        ('I', 0x17, 0x22),
        ('J', 0x24, 0x26),
        ('K', 0x25, 0x28),
        ('L', 0x26, 0x25),
        ('M', 0x32, 0x2E),
        ('N', 0x31, 0x2D),
        ('O', 0x18, 0x1F),
        ('P', 0x19, 0x23),
        ('Q', 0x10, 0x0C),
        ('R', 0x13, 0x0F),
        ('S', 0x1F, 0x01),
        ('T', 0x14, 0x11),
        ('U', 0x16, 0x20),
        ('V', 0x2F, 0x09),
        ('W', 0x11, 0x0D),
        ('X', 0x2D, 0x07),
        ('Y', 0x15, 0x10),
        ('Z', 0x2C, 0x06),
    ];
    for (i, (c, scan, kvk)) in letters.iter().enumerate() {
        v.push((0x04 + i as u16, format!("Keyboard{c}"), *scan, *kvk));
    }
    let digits = [
        ('1', 0x02, 0x12),
        ('2', 0x03, 0x13),
        ('3', 0x04, 0x14),
        ('4', 0x05, 0x15),
        ('5', 0x06, 0x17),
        ('6', 0x07, 0x16),
        ('7', 0x08, 0x1A),
        ('8', 0x09, 0x1C),
        ('9', 0x0A, 0x19),
        ('0', 0x0B, 0x1D),
    ];
    for (i, (c, scan, kvk)) in digits.iter().enumerate() {
        v.push((0x1E + i as u16, format!("Keyboard{c}"), *scan, *kvk));
    }
    let named: [(u16, &str, u16, u16); 17] = [
        (0x28, "KeyboardReturn", 0x1C, 0x24),
        (0x29, "KeyboardEscape", 0x01, 0x35),
        (0x2A, "KeyboardDeleteBackspace", 0x0E, 0x33),
        (0x2B, "KeyboardTab", 0x0F, 0x30),
        (0x2C, "KeyboardSpacebar", 0x39, 0x31),
        (0x2D, "KeyboardMinus", 0x0C, 0x1B),
        (0x2E, "KeyboardEqual", 0x0D, 0x18),
        (0x2F, "KeyboardLeftBracket", 0x1A, 0x21),
        (0x30, "KeyboardRightBracket", 0x1B, 0x1E),
        (0x31, "KeyboardBackslash", 0x2B, 0x2A),
        (0x33, "KeyboardSemicolon", 0x27, 0x29),
        (0x34, "KeyboardQuote", 0x28, 0x27),
        (0x35, "KeyboardGraveAccent", 0x29, 0x32),
        (0x36, "KeyboardComma", 0x33, 0x2B),
        (0x37, "KeyboardPeriod", 0x34, 0x2F),
        (0x38, "KeyboardSlash", 0x35, 0x2C),
        (0x39, "KeyboardCapsLock", 0x3A, 0x39),
    ];
    for (u, n, s, k) in named {
        v.push((u, n.to_owned(), s, k));
    }
    let fscan = [0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58];
    let fkvk = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F];
    for i in 0..12 {
        v.push((0x3A + i as u16, format!("KeyboardF{}", i + 1), fscan[i], fkvk[i]));
    }
    let nav: [(u16, &str, u16, u16); 10] = [
        (0x49, "KeyboardInsert", 0xE052, 0x72),
        (0x4A, "KeyboardHome", 0xE047, 0x73),
        (0x4B, "KeyboardPageUp", 0xE049, 0x74),
        (0x4C, "KeyboardDeleteForward", 0xE053, 0x75),
        (0x4D, "KeyboardEnd", 0xE04F, 0x77),
        (0x4E, "KeyboardPageDown", 0xE051, 0x79),
        (0x4F, "KeyboardRightArrow", 0xE04D, 0x7C),
        (0x50, "KeyboardLeftArrow", 0xE04B, 0x7B),
        (0x51, "KeyboardDownArrow", 0xE050, 0x7D),
        (0x52, "KeyboardUpArrow", 0xE048, 0x7E),
    ];
    for (u, n, s, k) in nav {
        v.push((u, n.to_owned(), s, k));
    }
    // Keypad: slash, asterisk, minus, plus, enter, 1..9, 0, period.
    let pad: [(u16, &str, u16, u16); 17] = [
        (0x54, "KeypadSlash", 0xE035, 0x4B),
        (0x55, "KeypadAsterisk", 0x37, 0x43),
        (0x56, "KeypadMinus", 0x4A, 0x4E),
        (0x57, "KeypadPlus", 0x4E, 0x45),
        (0x58, "KeypadEnter", 0xE01C, 0x4C),
        (0x59, "Keypad1", 0x4F, 0x53),
        (0x5A, "Keypad2", 0x50, 0x54),
        (0x5B, "Keypad3", 0x51, 0x55),
        (0x5C, "Keypad4", 0x4B, 0x56),
        (0x5D, "Keypad5", 0x4C, 0x57),
        (0x5E, "Keypad6", 0x4D, 0x58),
        (0x5F, "Keypad7", 0x47, 0x59),
        (0x60, "Keypad8", 0x48, 0x5B),
        (0x61, "Keypad9", 0x49, 0x5C),
        (0x62, "Keypad0", 0x52, 0x52),
        (0x63, "KeypadPeriod", 0x53, 0x41),
        (0x53, "KeypadNumLock", 0xE045, 0x47),
    ];
    for (u, n, s, k) in pad {
        v.push((u, n.to_owned(), s, k));
    }
    let mods: [(u16, &str, u16, u16); 8] = [
        (0xE0, "KeyboardLeftControl", 0x1D, 0x3B),
        (0xE1, "KeyboardLeftShift", 0x2A, 0x38),
        (0xE2, "KeyboardLeftAlt", 0x38, 0x3A),
        (0xE3, "KeyboardLeftGUI", 0xE05B, 0x37),
        (0xE4, "KeyboardRightControl", 0xE01D, 0x3E),
        (0xE5, "KeyboardRightShift", 0x36, 0x3C),
        (0xE6, "KeyboardRightAlt", 0xE038, 0x3D),
        (0xE7, "KeyboardRightGUI", 0xE05C, 0x36),
    ];
    for (u, n, s, k) in mods {
        v.push((u, n.to_owned(), s, k));
    }
    v.push((0x65, "KeyboardApplication".to_owned(), 0xE05D, 0x6E));
    v
}

#[test]
fn table_parses_and_has_unique_keys_per_keyboard_kind() {
    let rows = entries();
    assert!(rows.len() > 120, "only {} rows", rows.len());
    let mut hid = HashSet::new();
    let mut names = HashSet::new();
    let mut win = HashSet::new();
    for r in rows {
        assert!(r.page == PAGE_KEYBOARD || r.page == PAGE_CONSUMER, "{} has page {:#x}", r.name, r.page);
        assert!(hid.insert((r.page, r.usage)), "duplicate usage {:#x}/{:#x} ({})", r.page, r.usage, r.name);
        assert!(names.insert(r.name), "duplicate name {}", r.name);
        if let Some(s) = r.win_scan {
            assert!(win.insert(s), "duplicate win_scan {s:#x} ({})", r.name);
            let (code, ext) = scan_parts(s);
            assert!(code != 0 && (s <= 0xFF || ext), "{} has a malformed scan {s:#x}", r.name);
        }
    }
    for kb in KbKind::ALL {
        let mut mac = HashSet::new();
        for r in rows {
            if let Some(k) = hid_to_mac(r.page, r.usage, kb) {
                assert!(mac.insert(k), "duplicate kVK {k:#x} ({}) on {kb:?}", r.name);
            }
        }
    }
}

#[test]
fn every_us_key_matches_the_hand_written_scan_and_kvk() {
    for (usage, name, scan, kvk) in us_keys() {
        let info = key_info(PAGE_KEYBOARD, usage).unwrap_or_else(|| panic!("{name} ({usage:#x}) missing"));
        assert_eq!(info.name, name);
        assert_eq!(hid_to_win(PAGE_KEYBOARD, usage), Some(scan), "{name} scan");
        assert_eq!(win_to_hid(scan), Some((PAGE_KEYBOARD, usage)), "{name} reverse scan");
        for kb in [KbKind::Ansi, KbKind::Iso, KbKind::Jis] {
            let expected = match (usage, kb) {
                (0x35, KbKind::Iso) => 0x0A,
                _ => kvk,
            };
            assert_eq!(hid_to_mac(PAGE_KEYBOARD, usage, kb), Some(expected), "{name} kVK on {kb:?}");
        }
        assert_eq!(mac_to_hid(kvk, KbKind::Ansi), Some((PAGE_KEYBOARD, usage)), "{name} reverse kVK");
    }
}

#[test]
fn round_trips_hid_win_hid_and_hid_mac_hid_for_every_row() {
    for r in entries() {
        if let Some(scan) = hid_to_win(r.page, r.usage) {
            assert_eq!(win_to_hid(scan), Some((r.page, r.usage)), "{} via scan {scan:#x}", r.name);
        }
        for kb in KbKind::ALL {
            if let Some(kvk) = hid_to_mac(r.page, r.usage, kb) {
                assert_eq!(mac_to_hid(kvk, kb), Some((r.page, r.usage)), "{} via kVK {kvk:#x} on {kb:?}", r.name);
            }
        }
    }
}

#[test]
fn abnt2_keyboard_is_covered_key_by_key() {
    // Positions of a 107-key ABNT2 keyboard as (HID usage, set-1 scan), left to right, top to bottom.
    let abnt2: Vec<(u16, u16, &str)> = vec![
        (0x35, 0x29, "' \""),
        (0x1E, 0x02, "1"),
        (0x1F, 0x03, "2"),
        (0x20, 0x04, "3"),
        (0x21, 0x05, "4"),
        (0x22, 0x06, "5"),
        (0x23, 0x07, "6"),
        (0x24, 0x08, "7"),
        (0x25, 0x09, "8"),
        (0x26, 0x0A, "9"),
        (0x27, 0x0B, "0"),
        (0x2D, 0x0C, "- _"),
        (0x2E, 0x0D, "= +"),
        (0x2A, 0x0E, "Backspace"),
        (0x2B, 0x0F, "Tab"),
        (0x14, 0x10, "Q"),
        (0x1A, 0x11, "W"),
        (0x08, 0x12, "E"),
        (0x15, 0x13, "R"),
        (0x17, 0x14, "T"),
        (0x1C, 0x15, "Y"),
        (0x18, 0x16, "U"),
        (0x0C, 0x17, "I"),
        (0x12, 0x18, "O"),
        (0x13, 0x19, "P"),
        (0x2F, 0x1A, "acute grave"),
        (0x30, 0x1B, "[ {"),
        (0x28, 0x1C, "Enter"),
        (0x39, 0x3A, "Caps"),
        (0x04, 0x1E, "A"),
        (0x16, 0x1F, "S"),
        (0x07, 0x20, "D"),
        (0x09, 0x21, "F"),
        (0x0A, 0x22, "G"),
        (0x0B, 0x23, "H"),
        (0x0D, 0x24, "J"),
        (0x0E, 0x25, "K"),
        (0x0F, 0x26, "L"),
        (0x33, 0x27, "c cedilla"),
        (0x34, 0x28, "~ ^"),
        (0x31, 0x2B, "] }"),
        (0xE1, 0x2A, "LShift"),
        (0x64, 0x56, "\\ |"),
        (0x1D, 0x2C, "Z"),
        (0x1B, 0x2D, "X"),
        (0x06, 0x2E, "C"),
        (0x19, 0x2F, "V"),
        (0x05, 0x30, "B"),
        (0x11, 0x31, "N"),
        (0x10, 0x32, "M"),
        (0x36, 0x33, ", <"),
        (0x37, 0x34, ". >"),
        (0x38, 0x35, "; :"),
        (0x87, 0x73, "/ ?"),
        (0xE5, 0x36, "RShift"),
        (0xE0, 0x1D, "LCtrl"),
        (0xE3, 0xE05B, "LWin"),
        (0xE2, 0x38, "LAlt"),
        (0x2C, 0x39, "Space"),
        (0xE6, 0xE038, "AltGr"),
        (0xE7, 0xE05C, "RWin"),
        (0x65, 0xE05D, "Menu"),
        (0xE4, 0xE01D, "RCtrl"),
        (0x53, 0xE045, "NumLock"),
        (0x54, 0xE035, "Num /"),
        (0x55, 0x37, "Num *"),
        (0x56, 0x4A, "Num -"),
        (0x57, 0x4E, "Num +"),
        (0x58, 0xE01C, "Num Enter"),
        (0x85, 0x7E, "Num . (ABNT)"),
        (0x63, 0x53, "Num ."),
    ];
    for (usage, scan, label) in &abnt2 {
        assert_eq!(hid_to_win(PAGE_KEYBOARD, *usage), Some(*scan), "ABNT2 key {label}");
        assert_eq!(win_to_hid(*scan), Some((PAGE_KEYBOARD, *usage)), "ABNT2 key {label} reverse");
        // A Mac hosting an ABNT2 viewer needs a kVK for every one of them (the two ABNT keys use the JIS codes).
        assert!(hid_to_mac(PAGE_KEYBOARD, *usage, KbKind::Iso).is_some(), "ABNT2 key {label} has no kVK");
    }
    // International1 (/ ?) and the keypad comma, with the codes of C4's last bullet.
    assert_eq!(key_info(PAGE_KEYBOARD, 0x87).map(|k| k.win_vk), Some(Some(0xC1)));
    assert_eq!(key_info(PAGE_KEYBOARD, 0x85).map(|k| k.win_vk), Some(Some(0xC2)));
    assert!(key_info(PAGE_KEYBOARD, 0x87).is_some_and(|k| k.flags.contains(Flags::ABNT2)));
    assert!(key_info(PAGE_KEYBOARD, 0x85).is_some_and(|k| k.flags.contains(Flags::ABNT2)));
    assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0x87, KbKind::Iso), Some(0x5E));
    assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0x85, KbKind::Iso), Some(0x5F));
}

#[test]
fn the_us_list_has_every_key_of_a_104_key_keyboard_but_the_system_ones() {
    // 104 keys minus PrintScreen, ScrollLock, Pause (system keys, checked in the AltGr and scan tests below).
    let us = us_keys();
    let usages: HashSet<u16> = us.iter().map(|k| k.0).collect();
    assert_eq!(usages.len(), us.len(), "duplicate usage in the hand-written list");
    assert_eq!(us.len(), 101, "a 104-key keyboard has 101 of these keys");
    for usage in [0x46u16, 0x47, 0x48] {
        assert!(key_info(PAGE_KEYBOARD, usage).is_some());
    }
}

#[test]
fn altgr_is_the_extended_right_alt_and_the_only_altgr_row() {
    let altgr: Vec<_> = entries().iter().filter(|r| r.flags.contains(Flags::ALTGR)).collect();
    assert_eq!(altgr.len(), 1);
    assert_eq!((altgr[0].page, altgr[0].usage), (PAGE_KEYBOARD, 0xE6));
    assert_eq!(hid_to_win(PAGE_KEYBOARD, 0xE6), Some(0xE038));
    assert_eq!(scan_parts(0xE038), (0x38, true));
    // Left Alt shares the make code 0x38 and differs only by the E0 flag.
    assert_eq!(scan_parts(hid_to_win(PAGE_KEYBOARD, 0xE2).unwrap()), (0x38, false));
    // The fake left Ctrl Windows adds before AltGr is an ordinary LCtrl row: nothing in the table models it.
    assert_eq!(hid_to_win(PAGE_KEYBOARD, 0xE0), Some(0x1D));
    // Mac right Option.
    assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0xE6, KbKind::Iso), Some(0x3D));
}

#[test]
fn iso_swap_exchanges_grave_and_section_only_on_an_iso_mac() {
    let swap_rows: Vec<_> = entries().iter().filter(|r| r.flags.contains(Flags::ISO_SWAP)).map(|r| r.usage).collect();
    assert_eq!(swap_rows, vec![0x35, 0x64]);
    // ANSI and JIS Macs: kVK_ANSI_Grave 0x32 is the grave key, kVK_ISO_Section 0x0A the key near the left Shift.
    for kb in [KbKind::Ansi, KbKind::Jis] {
        assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0x35, kb), Some(0x32));
        assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0x64, kb), Some(0x0A));
        assert_eq!(mac_to_hid(0x32, kb), Some((PAGE_KEYBOARD, 0x35)));
        assert_eq!(mac_to_hid(0x0A, kb), Some((PAGE_KEYBOARD, 0x64)));
    }
    // ISO Mac: the codes exchange.
    assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0x35, KbKind::Iso), Some(0x0A));
    assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0x64, KbKind::Iso), Some(0x32));
    assert_eq!(mac_to_hid(0x0A, KbKind::Iso), Some((PAGE_KEYBOARD, 0x35)));
    assert_eq!(mac_to_hid(0x32, KbKind::Iso), Some((PAGE_KEYBOARD, 0x64)));
    // Nothing else differs between the kinds.
    for r in entries().iter().filter(|r| !r.flags.contains(Flags::ISO_SWAP)) {
        let ansi = hid_to_mac(r.page, r.usage, KbKind::Ansi);
        assert_eq!(hid_to_mac(r.page, r.usage, KbKind::Iso), ansi, "{}", r.name);
        assert_eq!(hid_to_mac(r.page, r.usage, KbKind::Jis), ansi, "{}", r.name);
    }
}

#[test]
fn apple_keyboard_kvk_codes_map_to_hid() {
    // The Apple keys with no PC twin and the JIS keys of an Apple Japanese keyboard.
    assert_eq!(mac_to_hid(0x37, KbKind::Ansi), Some((PAGE_KEYBOARD, 0xE3)), "Command");
    assert_eq!(mac_to_hid(0x36, KbKind::Ansi), Some((PAGE_KEYBOARD, 0xE7)), "Right Command");
    assert_eq!(mac_to_hid(0x3A, KbKind::Ansi), Some((PAGE_KEYBOARD, 0xE2)), "Option");
    assert_eq!(mac_to_hid(0x3B, KbKind::Ansi), Some((PAGE_KEYBOARD, 0xE0)), "Control");
    assert_eq!(mac_to_hid(0x66, KbKind::Jis), Some((PAGE_KEYBOARD, 0x91)), "Eisu");
    assert_eq!(mac_to_hid(0x68, KbKind::Jis), Some((PAGE_KEYBOARD, 0x90)), "Kana");
    assert_eq!(mac_to_hid(0x5D, KbKind::Jis), Some((PAGE_KEYBOARD, 0x89)), "Yen");
    assert_eq!(mac_to_hid(0x72, KbKind::Ansi), Some((PAGE_KEYBOARD, 0x49)), "Help is Insert");
    assert_eq!(hid_to_mac(PAGE_KEYBOARD, 0x75, KbKind::Ansi), None, "Help has no kVK of its own");
    assert_eq!(mac_to_hid(0x47, KbKind::Ansi), Some((PAGE_KEYBOARD, 0x53)), "Clear is NumLock");
    // Mac function-row volume keys are the consumer-page keys.
    assert_eq!(mac_to_hid(0x4A, KbKind::Ansi), Some((PAGE_CONSUMER, 0xE2)), "kVK_Mute");
    assert_eq!(mac_to_hid(0x48, KbKind::Ansi), Some((PAGE_CONSUMER, 0xE9)), "kVK_VolumeUp");
    assert_eq!(mac_to_hid(0x49, KbKind::Ansi), Some((PAGE_CONSUMER, 0xEA)), "kVK_VolumeDown");
}

#[test]
fn numpad_keys_carry_the_numpad_flag_and_have_both_codes() {
    let pad: Vec<_> = entries().iter().filter(|r| r.flags.contains(Flags::NUMPAD)).collect();
    assert!(pad.len() >= 18, "{} numpad rows", pad.len());
    for r in &pad {
        assert!(r.win_scan.is_some(), "{} has no scan", r.name);
        assert!(r.page == PAGE_KEYBOARD && (0x53..=0x67).contains(&r.usage) || r.usage == 0x85, "{}", r.name);
    }
    // Numpad Enter and Slash are extended on Windows; the digits are not.
    assert!(scan_parts(hid_to_win(PAGE_KEYBOARD, 0x58).unwrap()).1);
    assert!(scan_parts(hid_to_win(PAGE_KEYBOARD, 0x54).unwrap()).1);
    assert!(!scan_parts(hid_to_win(PAGE_KEYBOARD, 0x5A).unwrap()).1);
    // Navigation keys share make codes with the numpad and differ by the E0 flag only.
    assert_eq!(scan_parts(hid_to_win(PAGE_KEYBOARD, 0x52).unwrap()), (0x48, true));
    assert_eq!(scan_parts(hid_to_win(PAGE_KEYBOARD, 0x60).unwrap()), (0x48, false));
}

#[test]
fn media_keys_use_the_consumer_page_and_the_volume_alias() {
    let media: Vec<_> = entries().iter().filter(|r| r.flags.contains(Flags::MEDIA)).collect();
    assert!(media.len() >= 15);
    for r in &media {
        assert_eq!(r.page, PAGE_CONSUMER, "{}", r.name);
        let scan = r.win_scan.unwrap_or_else(|| panic!("{} has no scan", r.name));
        assert!(scan_parts(scan).1, "{} is an extended key", r.name);
        if r.name != "Eject" {
            assert!(r.win_vk.is_some(), "{} needs a VK to be injected", r.name);
        }
        if !matches!(r.usage, 0xE2 | 0xE9 | 0xEA) {
            assert_eq!(r.mac_kvk, None, "{}: macOS media keys are not kVK codes", r.name);
        }
    }
    let expect = [
        (0xE2u16, 0xE020u16, 0xADu16),
        (0xE9, 0xE030, 0xAF),
        (0xEA, 0xE02E, 0xAE),
        (0xB5, 0xE019, 0xB0),
        (0xB6, 0xE010, 0xB1),
        (0xB7, 0xE024, 0xB2),
        (0xCD, 0xE022, 0xB3),
    ];
    for (usage, scan, vk) in expect {
        assert_eq!(hid_to_win(PAGE_CONSUMER, usage), Some(scan));
        assert_eq!(win_to_hid(scan), Some((PAGE_CONSUMER, usage)));
        assert_eq!(win_vk(PAGE_CONSUMER, usage), Some(vk));
    }
    // The keyboard-page volume usages are the same keys.
    for (kb_usage, cons_usage) in [(0x7F, 0xE2), (0x80, 0xE9), (0x81, 0xEA)] {
        assert_eq!(canonical(PAGE_KEYBOARD, kb_usage), (PAGE_CONSUMER, cons_usage));
        assert_eq!(hid_to_win(PAGE_KEYBOARD, kb_usage), hid_to_win(PAGE_CONSUMER, cons_usage));
    }
    assert_eq!(canonical(PAGE_KEYBOARD, 0x04), (PAGE_KEYBOARD, 0x04));
}

#[test]
fn unknown_keys_are_none_not_panics() {
    assert_eq!(hid_to_win(0x07, 0xFFFF), None);
    assert_eq!(hid_to_win(0x09, 0x01), None);
    assert_eq!(win_to_hid(0xFFFF), None);
    assert_eq!(mac_to_hid(0xFFFF, KbKind::Iso), None);
    assert_eq!(hid_to_mac(0x0C, 0xCD, KbKind::Ansi), None);
}
