//! What a Mac key means on the wire, as pure functions (no OS call): the `kVK_*` code to a HID usage through the C4
//! tables (with the ISO section/grave swap for the viewer's own keyboard kind), the modifier keys and the HID modifier
//! byte, the shortcuts macOS handles before an application sees them, and the text an `NSEvent` may carry as typing.
//!
//! The viewer's keyboard type is read once from the OS (`input::detect_keyboard_kind`); everything here takes it as
//! an argument so it can be tested on any host.

use swapdesk_keys::{KbKind, PAGE_KEYBOARD, hid_to_mac, mac_to_hid, modifier};
use swapdesk_shell_mac::Modifiers;

/// `kVK_ANSI_Q`, `kVK_Tab`, `kVK_Space`, `kVK_Escape`, the arrows and the number keys of screenshots
/// (`HIToolbox/Events.h`, as listed in `swapdesk-keys/data/keys.tsv`).
pub const KVK_Q: u16 = 0x0C;
pub const KVK_TAB: u16 = 0x30;
pub const KVK_SPACE: u16 = 0x31;
pub const KVK_ESCAPE: u16 = 0x35;
pub const KVK_CAPS_LOCK: u16 = 0x39;
pub const KVK_LEFT_ARROW: u16 = 0x7B;
pub const KVK_RIGHT_ARROW: u16 = 0x7C;
pub const KVK_DOWN_ARROW: u16 = 0x7D;
pub const KVK_UP_ARROW: u16 = 0x7E;
const KVK_3: u16 = 0x14;
const KVK_4: u16 = 0x15;
const KVK_5: u16 = 0x17;

/// The eight modifier keys in HID modifier-byte order (bit 0 LCtrl ... bit 7 RGui): their `kVK_*` codes on a Mac
/// keyboard, which no keyboard kind changes.
pub fn modifier_kvk(bit: u8) -> Option<u16> {
    let usage = modifier::usage_of(bit)?;
    hid_to_mac(PAGE_KEYBOARD, usage, KbKind::Ansi)
}

/// The HID modifier bit of a `kVK_*` modifier key.
pub fn modifier_bit_of_kvk(kvk: u16) -> Option<u8> {
    let (page, usage) = mac_to_hid(kvk, KbKind::Ansi)?;
    modifier::bit_of(page, usage)
}

/// The aggregate flag a HID modifier bit belongs to (both sides of a key share one flag in `NSEvent`).
pub fn aggregate_of_bit(bit: u8) -> Modifiers {
    match bit {
        modifier::LSHIFT | modifier::RSHIFT => Modifiers::SHIFT,
        modifier::LCTRL | modifier::RCTRL => Modifiers::CONTROL,
        modifier::LALT | modifier::RALT => Modifiers::OPTION,
        _ => Modifiers::COMMAND,
    }
}

/// The aggregate flags a HID modifier byte stands for.
pub fn aggregate_of_byte(byte: u8) -> Modifiers {
    let mut m = Modifiers::default();
    for i in 0..8 {
        let bit = 1u8 << i;
        if byte & bit != 0 {
            m = m.union(aggregate_of_bit(bit));
        }
    }
    m
}

/// The HID usage of a Mac key for a keyboard kind, or `None` when the table has no row for it.
pub fn hid_of_kvk(kvk: u16, kb: KbKind) -> Option<(u16, u16)> {
    mac_to_hid(kvk, kb)
}

/// The shortcuts macOS acts on before an application sees the key (Cmd+Tab, Cmd+Space, Mission Control and Spaces
/// shortcuts, force quit, screenshots, lock screen). `flags` is the modifier state of the key event.
///
/// Cmd+Q is never one of them: it stays the viewer's own way out (the shell lets it through and the menu quits).
pub fn is_system_shortcut(kvk: u16, flags: Modifiers) -> bool {
    let cmd = flags.contains(Modifiers::COMMAND);
    let ctrl = flags.contains(Modifiers::CONTROL);
    let opt = flags.contains(Modifiers::OPTION);
    let shift = flags.contains(Modifiers::SHIFT);
    match kvk {
        // App switcher (Cmd+Tab, Cmd+Shift+Tab); Spotlight (Cmd+Space), Finder search (Cmd+Option+Space), emoji
        // picker (Ctrl+Cmd+Space), input source switch (Ctrl+Space).
        KVK_TAB => cmd,
        KVK_SPACE => cmd || ctrl,
        KVK_ESCAPE => cmd && opt,
        KVK_3 | KVK_4 | KVK_5 => cmd && shift,
        KVK_Q => cmd && ctrl && !opt,
        // Mission Control, Application Windows and the move between Spaces (Ctrl+arrows).
        KVK_LEFT_ARROW | KVK_RIGHT_ARROW | KVK_DOWN_ARROW | KVK_UP_ARROW => ctrl && !cmd,
        _ => false,
    }
}

/// Cmd+Q alone: the key the viewer never takes, so the user can always quit it.
pub fn is_local_escape(kvk: u16, flags: Modifiers) -> bool {
    kvk == KVK_Q && flags.contains(Modifiers::COMMAND) && !flags.contains(Modifiers::CONTROL)
}

/// The text of a key press when it is something to type. AppKit gives arrows, function keys and the like as private
/// use characters (`NSUpArrowFunctionKey` is U+F700 and so on, `NSEvent.h`): those are keys, not text.
pub fn typed_text(characters: Option<&str>) -> Option<&str> {
    let t = characters?;
    if t.is_empty() || t.chars().any(|c| ('\u{F700}'..='\u{F8FF}').contains(&c)) {
        return None;
    }
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eight_modifier_keys_have_their_mac_codes() {
        let expect =
            [(modifier::LCTRL, 0x3B), (modifier::LSHIFT, 0x38), (modifier::LALT, 0x3A), (modifier::LGUI, 0x37)];
        for (bit, kvk) in expect {
            assert_eq!(modifier_kvk(bit), Some(kvk));
            assert_eq!(modifier_bit_of_kvk(kvk), Some(bit));
        }
        let right = [(modifier::RCTRL, 0x3E), (modifier::RSHIFT, 0x3C), (modifier::RALT, 0x3D), (modifier::RGUI, 0x36)];
        for (bit, kvk) in right {
            assert_eq!(modifier_kvk(bit), Some(kvk));
            assert_eq!(modifier_bit_of_kvk(kvk), Some(bit));
        }
        assert_eq!(modifier_kvk(3), None, "not a single bit");
        assert_eq!(modifier_bit_of_kvk(0x00), None, "A is no modifier");
        assert_eq!(modifier_bit_of_kvk(KVK_CAPS_LOCK), None, "Caps Lock is a lock, not a held modifier");
    }

    #[test]
    fn aggregates_follow_the_bytes() {
        assert_eq!(aggregate_of_byte(modifier::LSHIFT | modifier::RSHIFT), Modifiers::SHIFT);
        assert_eq!(aggregate_of_byte(modifier::RGUI | modifier::LCTRL), Modifiers::COMMAND.union(Modifiers::CONTROL));
        assert_eq!(aggregate_of_byte(0), Modifiers::default());
        for i in 0..8 {
            let bit = 1u8 << i;
            assert!(aggregate_of_byte(bit).contains(aggregate_of_bit(bit)));
        }
    }

    #[test]
    fn letters_and_the_iso_swap_map_through_the_c4_tables() {
        // kVK_ANSI_A -> KeyboardA, kVK_Return -> KeyboardReturn (Enter), kVK_Delete -> Backspace.
        assert_eq!(hid_of_kvk(0x00, KbKind::Ansi), Some((7, 0x04)));
        assert_eq!(hid_of_kvk(0x24, KbKind::Ansi), Some((7, 0x28)));
        assert_eq!(hid_of_kvk(0x33, KbKind::Ansi), Some((7, 0x2A)));
        // The key left of 1 is kVK_ANSI_Grave 0x32 on ANSI and kVK_ISO_Section 0x0A on ISO: the same HID key.
        assert_eq!(hid_of_kvk(0x32, KbKind::Ansi), Some((7, 0x35)));
        assert_eq!(hid_of_kvk(0x0A, KbKind::Iso), Some((7, 0x35)));
        // And the ISO key next to left Shift reports 0x32 there.
        assert_eq!(hid_of_kvk(0x32, KbKind::Iso), Some((7, 0x64)));
        assert_eq!(hid_of_kvk(0xFF, KbKind::Ansi), None);
    }

    #[test]
    fn system_shortcuts_are_the_ones_macos_takes_first() {
        let cmd = Modifiers::COMMAND;
        assert!(is_system_shortcut(KVK_TAB, cmd));
        assert!(is_system_shortcut(KVK_TAB, cmd.union(Modifiers::SHIFT)));
        assert!(is_system_shortcut(KVK_SPACE, cmd));
        assert!(is_system_shortcut(KVK_SPACE, Modifiers::CONTROL));
        assert!(is_system_shortcut(KVK_ESCAPE, cmd.union(Modifiers::OPTION)));
        assert!(is_system_shortcut(KVK_UP_ARROW, Modifiers::CONTROL.union(Modifiers::FUNCTION)));
        assert!(is_system_shortcut(KVK_LEFT_ARROW, Modifiers::CONTROL));
        assert!(is_system_shortcut(KVK_4, cmd.union(Modifiers::SHIFT)));
        assert!(is_system_shortcut(KVK_Q, cmd.union(Modifiers::CONTROL)));
    }

    #[test]
    fn ordinary_keys_and_the_escape_hatch_are_not_system_shortcuts() {
        let none = Modifiers::default();
        assert!(!is_system_shortcut(KVK_TAB, none), "plain Tab types");
        assert!(!is_system_shortcut(KVK_TAB, Modifiers::CONTROL), "Ctrl+Tab is the host's tab switch");
        assert!(!is_system_shortcut(KVK_SPACE, none));
        assert!(!is_system_shortcut(KVK_UP_ARROW, none));
        assert!(!is_system_shortcut(KVK_LEFT_ARROW, Modifiers::COMMAND), "Cmd+Left is Home on the host");
        assert!(!is_system_shortcut(0x08, Modifiers::COMMAND), "Cmd+C goes to the host, not to macOS");
        assert!(!is_system_shortcut(KVK_Q, Modifiers::COMMAND), "Cmd+Q is the viewer's own way out");
        assert!(is_local_escape(KVK_Q, Modifiers::COMMAND));
        assert!(is_local_escape(KVK_Q, Modifiers::COMMAND.union(Modifiers::SHIFT)));
        assert!(!is_local_escape(KVK_Q, none));
        assert!(!is_local_escape(KVK_Q, Modifiers::COMMAND.union(Modifiers::CONTROL)));
    }

    #[test]
    fn function_key_characters_are_not_text() {
        assert_eq!(typed_text(Some("a")), Some("a"));
        assert_eq!(typed_text(Some("ç")), Some("ç"));
        assert_eq!(typed_text(Some("\u{F700}")), None, "NSUpArrowFunctionKey");
        assert_eq!(typed_text(Some("\u{F704}")), None, "NSF1FunctionKey");
        assert_eq!(typed_text(Some("")), None);
        assert_eq!(typed_text(None), None);
    }
}
