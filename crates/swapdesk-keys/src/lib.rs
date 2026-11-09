//! Key tables of Swapdesk (contract C4, `docs/CONTRACTS.md`).
//!
//! The wire carries HID usages only (PROTOCOL.md 7.3); these tables translate them on the edges: to a Windows set-1
//! scan code (extended keys as `0xE0xx`) and to a macOS `kVK_*` code, and back. The data is `data/keys.tsv`, embedded
//! and parsed once. On top of the tables: the "Trocar Ctrl e Cmd no Mac" swap ([`swap`]) and the decision between
//! physical keys and Unicode text ([`mode`]).
//!
//! No OS call and no `unsafe`: the crate builds on every host (it is checked for `aarch64-apple-darwin` too).

pub mod mode;
pub mod shortcuts;
pub mod swap;
mod table;

pub use table::{
    Flags, KbKind, KeyInfo, canonical, entries, hid_to_mac, hid_to_win, key_info, mac_to_hid, scan_parts, win_to_hid,
    win_vk,
};

/// HID usage page of keyboard and keypad keys.
pub const PAGE_KEYBOARD: u16 = 0x07;
/// HID usage page of consumer (media) keys.
pub const PAGE_CONSUMER: u16 = 0x0C;

/// The HID modifier byte (PROTOCOL.md 7.3): bit 0 LCtrl, 1 LShift, 2 LAlt, 3 LGui, 4 RCtrl, 5 RShift, 6 RAlt, 7 RGui.
pub mod modifier {
    pub const LCTRL: u8 = 1 << 0;
    pub const LSHIFT: u8 = 1 << 1;
    pub const LALT: u8 = 1 << 2;
    pub const LGUI: u8 = 1 << 3;
    pub const RCTRL: u8 = 1 << 4;
    pub const RSHIFT: u8 = 1 << 5;
    pub const RALT: u8 = 1 << 6;
    pub const RGUI: u8 = 1 << 7;

    /// The modifier bit of a keyboard-page usage `0xE0..=0xE7`.
    pub fn bit_of(page: u16, usage: u16) -> Option<u8> {
        if page != super::PAGE_KEYBOARD || !(0xE0..=0xE7).contains(&usage) {
            return None;
        }
        Some(1 << (usage - 0xE0))
    }

    /// The keyboard-page usage of one modifier bit (`bit` is a single-bit mask).
    pub fn usage_of(bit: u8) -> Option<u16> {
        if bit.count_ones() != 1 {
            return None;
        }
        Some(0xE0 + u16::try_from(bit.trailing_zeros()).ok()?)
    }

    /// The modifier byte after `usage` went down or up (other usages leave it as it was).
    pub fn after(mods: u8, page: u16, usage: u16, down: bool) -> u8 {
        match bit_of(page, usage) {
            Some(bit) if down => mods | bit,
            Some(bit) => mods & !bit,
            None => mods,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::modifier;

    #[test]
    fn modifier_bits_follow_the_usage_order() {
        for (i, usage) in (0xE0u16..=0xE7).enumerate() {
            assert_eq!(modifier::bit_of(super::PAGE_KEYBOARD, usage), Some(1 << i));
            assert_eq!(modifier::usage_of(1 << i), Some(usage));
        }
        assert_eq!(modifier::bit_of(super::PAGE_KEYBOARD, 0x04), None);
        assert_eq!(modifier::bit_of(super::PAGE_CONSUMER, 0xE0), None);
        assert_eq!(modifier::usage_of(0), None);
        assert_eq!(modifier::usage_of(3), None);
    }

    #[test]
    fn after_sets_and_clears_only_the_modifier_bit() {
        let m = modifier::after(0, 7, 0xE6, true);
        assert_eq!(m, modifier::RALT);
        assert_eq!(modifier::after(m | modifier::LSHIFT, 7, 0xE6, false), modifier::LSHIFT);
        assert_eq!(modifier::after(m, 7, 0x04, true), m);
    }
}
