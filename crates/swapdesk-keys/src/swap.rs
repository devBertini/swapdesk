//! "Trocar Ctrl e Cmd no Mac" (C4 rule 4): applied by the viewer before sending, so host sinks stay table-only.
//! Left Ctrl exchanges with left GUI and right Ctrl with right GUI, in usages and in the modifier byte.

use crate::{PAGE_KEYBOARD, modifier};
use swapdesk_proto::InputEvent;

/// The usage after the swap (only the four keys move).
pub fn swap_usage(page: u16, usage: u16) -> u16 {
    if page != PAGE_KEYBOARD {
        return usage;
    }
    match usage {
        0xE0 => 0xE3,
        0xE3 => 0xE0,
        0xE4 => 0xE7,
        0xE7 => 0xE4,
        other => other,
    }
}

/// The modifier byte after the swap: bit 0 with bit 3 and bit 4 with bit 7.
pub fn swap_modifiers(mods: u8) -> u8 {
    let keep = mods & !(modifier::LCTRL | modifier::LGUI | modifier::RCTRL | modifier::RGUI);
    let l_ctrl = mods & modifier::LCTRL != 0;
    let l_gui = mods & modifier::LGUI != 0;
    let r_ctrl = mods & modifier::RCTRL != 0;
    let r_gui = mods & modifier::RGUI != 0;
    keep | if l_ctrl { modifier::LGUI } else { 0 }
        | if l_gui { modifier::LCTRL } else { 0 }
        | if r_ctrl { modifier::RGUI } else { 0 }
        | if r_gui { modifier::RCTRL } else { 0 }
}

/// A key record after the swap; every other record is returned unchanged.
pub fn swap_event(event: &InputEvent) -> InputEvent {
    match event {
        InputEvent::Key { usage_page, usage, down, modifiers, locks } => InputEvent::Key {
            usage_page: *usage_page,
            usage: swap_usage(*usage_page, *usage),
            down: *down,
            modifiers: swap_modifiers(*modifiers),
            locks: *locks,
        },
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usages_swap_both_ways_and_nothing_else_moves() {
        for (a, b) in [(0xE0, 0xE3), (0xE4, 0xE7)] {
            assert_eq!(swap_usage(7, a), b);
            assert_eq!(swap_usage(7, b), a);
        }
        for u in [0x04, 0xE1, 0xE2, 0xE5, 0xE6] {
            assert_eq!(swap_usage(7, u), u);
        }
        assert_eq!(swap_usage(0x0C, 0xE0), 0xE0, "consumer page is untouched");
    }

    #[test]
    fn modifier_bytes_swap_and_double_swap_is_identity() {
        assert_eq!(swap_modifiers(modifier::LCTRL), modifier::LGUI);
        assert_eq!(swap_modifiers(modifier::LGUI), modifier::LCTRL);
        assert_eq!(swap_modifiers(modifier::RCTRL), modifier::RGUI);
        assert_eq!(swap_modifiers(modifier::RGUI), modifier::RCTRL);
        assert_eq!(
            swap_modifiers(modifier::LSHIFT | modifier::RALT | modifier::LCTRL),
            modifier::LSHIFT | modifier::RALT | modifier::LGUI
        );
        for m in 0..=255u8 {
            assert_eq!(swap_modifiers(swap_modifiers(m)), m);
        }
    }

    #[test]
    fn events_swap_only_keys() {
        let e = InputEvent::Key { usage_page: 7, usage: 0x06, down: true, modifiers: modifier::LCTRL, locks: 2 };
        assert_eq!(
            swap_event(&e),
            InputEvent::Key { usage_page: 7, usage: 0x06, down: true, modifiers: modifier::LGUI, locks: 2 }
        );
        let ctrl = InputEvent::Key { usage_page: 7, usage: 0xE0, down: true, modifiers: modifier::LCTRL, locks: 0 };
        assert_eq!(
            swap_event(&ctrl),
            InputEvent::Key { usage_page: 7, usage: 0xE3, down: true, modifiers: modifier::LGUI, locks: 0 }
        );
        let t = InputEvent::Text("a".into());
        assert_eq!(swap_event(&t), t);
    }
}
