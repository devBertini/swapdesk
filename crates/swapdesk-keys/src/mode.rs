//! Physical keys or Unicode text (SPEC F-INPUT-01 and F-INPUT-02).
//!
//! The viewer reads a key as a HID usage and, from its own layout, the text the key produces (`text`). The decision:
//! - "Teclas físicas" (`Physical`): always the physical key.
//! - "Texto" (`Text`): printable text goes as Unicode text.
//! - "Automático" (`Auto`): physical while the two keyboard layouts match, text when they differ.
//! - Shortcuts (any of Ctrl, left Alt or GUI held) are always physical, and so is every key that makes no printable
//!   text (Enter, arrows, F-keys, modifiers). Right Alt alone is AltGr, not a shortcut: `AltGr+Q` is text.
//!
//! A key whose press went as text has no key-up on the wire; the router swallows that key-up (and sends the
//! auto-repeats as text again).

use crate::modifier;
use std::collections::HashSet;
use swapdesk_proto::{InputEvent, MAX_INPUT_TEXT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardMode {
    Physical,
    Text,
    Auto,
}

/// What the viewer's keyboard produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput<'a> {
    pub usage_page: u16,
    pub usage: u16,
    pub down: bool,
    /// The viewer's HID modifier byte after this event.
    pub modifiers: u8,
    pub locks: u8,
    /// The text this key press produces on the viewer's layout with the held modifiers, when it produces any.
    pub text: Option<&'a str>,
    /// The key is a dead key on the viewer's layout (accent waiting for the next letter): in text mode it sends nothing
    /// and the next key's `text` carries the composed character.
    pub dead_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Physical,
    Text,
}

/// Modifiers that make a key press a shortcut.
const SHORTCUT_MODS: u8 = modifier::LCTRL | modifier::RCTRL | modifier::LALT | modifier::LGUI | modifier::RGUI;

/// True when `text` is non-empty, short enough for one record and has no control character.
pub fn is_printable(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_INPUT_TEXT && !text.chars().any(char::is_control)
}

/// Text mode is in force for this setting and layout comparison.
pub fn text_mode(mode: KeyboardMode, layouts_match: bool) -> bool {
    match mode {
        KeyboardMode::Physical => false,
        KeyboardMode::Text => true,
        KeyboardMode::Auto => !layouts_match,
    }
}

/// The decision for one key press (down). Pure: no memory of earlier keys.
pub fn decide(mode: KeyboardMode, layouts_match: bool, modifiers: u8, text: Option<&str>) -> Decision {
    if !text_mode(mode, layouts_match) || modifiers & SHORTCUT_MODS != 0 {
        return Decision::Physical;
    }
    match text {
        Some(t) if is_printable(t) => Decision::Text,
        _ => Decision::Physical,
    }
}

/// What to send for one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// A key record, exactly as the keyboard gave it.
    Physical(InputEvent),
    /// A text record in place of the press.
    Text(InputEvent),
    /// Nothing: the key-up of a key that went as text.
    Drop,
}

#[derive(Debug, Clone)]
pub struct KeyRouter {
    mode: KeyboardMode,
    layouts_match: bool,
    as_text: HashSet<(u16, u16)>,
}

impl KeyRouter {
    pub fn new(mode: KeyboardMode, layouts_match: bool) -> Self {
        Self { mode, layouts_match, as_text: HashSet::new() }
    }

    pub fn set_mode(&mut self, mode: KeyboardMode) {
        self.mode = mode;
    }

    pub fn set_layouts_match(&mut self, same: bool) {
        self.layouts_match = same;
    }

    /// Forgets the keys that went as text (focus lost, grab off).
    pub fn reset(&mut self) {
        self.as_text.clear();
    }

    pub fn route(&mut self, key: &KeyInput<'_>) -> Routed {
        let id = (key.usage_page, key.usage);
        let physical = || {
            Routed::Physical(InputEvent::Key {
                usage_page: key.usage_page,
                usage: key.usage,
                down: key.down,
                modifiers: key.modifiers,
                locks: key.locks,
            })
        };
        if !key.down {
            return if self.as_text.remove(&id) { Routed::Drop } else { physical() };
        }
        if key.dead_key && text_mode(self.mode, self.layouts_match) && key.modifiers & SHORTCUT_MODS == 0 {
            self.as_text.insert(id);
            return Routed::Drop;
        }
        match decide(self.mode, self.layouts_match, key.modifiers, key.text) {
            Decision::Text => {
                self.as_text.insert(id);
                Routed::Text(InputEvent::Text(key.text.unwrap_or_default().to_owned()))
            }
            Decision::Physical => {
                self.as_text.remove(&id);
                physical()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(usage: u16, down: bool, mods: u8, text: Option<&str>) -> KeyInput<'_> {
        KeyInput { usage_page: 7, usage, down, modifiers: mods, locks: 0, text, dead_key: false }
    }

    #[test]
    fn physical_mode_never_sends_text() {
        for same in [true, false] {
            assert_eq!(decide(KeyboardMode::Physical, same, 0, Some("ç")), Decision::Physical);
        }
    }

    #[test]
    fn text_mode_sends_printable_text_whatever_the_layouts() {
        for same in [true, false] {
            assert_eq!(decide(KeyboardMode::Text, same, 0, Some("ç")), Decision::Text);
        }
    }

    #[test]
    fn auto_is_physical_when_layouts_match_and_text_when_they_differ() {
        assert_eq!(decide(KeyboardMode::Auto, true, 0, Some("a")), Decision::Physical);
        assert_eq!(decide(KeyboardMode::Auto, false, 0, Some("a")), Decision::Text);
        assert_eq!(decide(KeyboardMode::Auto, false, modifier::LSHIFT, Some("A")), Decision::Text);
    }

    #[test]
    fn shortcuts_are_always_physical() {
        for m in [
            modifier::LCTRL,
            modifier::RCTRL,
            modifier::LALT,
            modifier::LGUI,
            modifier::RGUI,
            modifier::LCTRL | modifier::LSHIFT,
        ] {
            assert_eq!(decide(KeyboardMode::Text, false, m, Some("c")), Decision::Physical, "mods {m:#04x}");
        }
    }

    #[test]
    fn altgr_alone_is_text_not_a_shortcut() {
        assert_eq!(decide(KeyboardMode::Auto, false, modifier::RALT, Some("@")), Decision::Text);
        assert_eq!(decide(KeyboardMode::Auto, false, modifier::RALT | modifier::LSHIFT, Some("¢")), Decision::Text);
    }

    #[test]
    fn keys_without_printable_text_stay_physical() {
        assert_eq!(decide(KeyboardMode::Text, false, 0, None), Decision::Physical);
        assert_eq!(decide(KeyboardMode::Text, false, 0, Some("")), Decision::Physical);
        assert_eq!(decide(KeyboardMode::Text, false, 0, Some("\r")), Decision::Physical);
        assert_eq!(decide(KeyboardMode::Text, false, 0, Some("\u{7f}")), Decision::Physical);
        assert_eq!(decide(KeyboardMode::Text, false, 0, Some(&"x".repeat(257))), Decision::Physical);
    }

    #[test]
    fn router_sends_text_and_swallows_its_key_up() {
        let mut r = KeyRouter::new(KeyboardMode::Auto, false);
        assert_eq!(r.route(&key(0x33, true, 0, Some("ç"))), Routed::Text(InputEvent::Text("ç".into())));
        // Auto-repeat: another down, text again.
        assert_eq!(r.route(&key(0x33, true, 0, Some("ç"))), Routed::Text(InputEvent::Text("ç".into())));
        assert_eq!(r.route(&key(0x33, false, 0, Some("ç"))), Routed::Drop);
        // The second key-up has nothing to pair with: it goes physical.
        assert!(matches!(r.route(&key(0x33, false, 0, None)), Routed::Physical(_)));
    }

    #[test]
    fn router_keeps_a_physical_press_physical_even_if_modifiers_change() {
        let mut r = KeyRouter::new(KeyboardMode::Auto, false);
        let down = r.route(&key(0x06, true, modifier::LCTRL, Some("c")));
        assert!(matches!(down, Routed::Physical(InputEvent::Key { down: true, .. })));
        // Ctrl released first, then C: the up still pairs with the physical down.
        let up = r.route(&key(0x06, false, 0, Some("c")));
        assert!(matches!(up, Routed::Physical(InputEvent::Key { down: false, .. })));
    }

    #[test]
    fn router_sends_modifiers_and_functional_keys_physically_in_text_mode() {
        let mut r = KeyRouter::new(KeyboardMode::Text, false);
        for usage in [0xE1u16, 0x28, 0x3A, 0x4F] {
            assert!(matches!(r.route(&key(usage, true, 0, None)), Routed::Physical(_)));
        }
    }

    #[test]
    fn a_dead_key_sends_nothing_in_text_mode_and_the_next_key_carries_the_accent() {
        let mut r = KeyRouter::new(KeyboardMode::Auto, false);
        let dead = KeyInput { dead_key: true, ..key(0x2F, true, 0, None) };
        assert_eq!(r.route(&dead), Routed::Drop);
        assert_eq!(r.route(&key(0x04, true, 0, Some("á"))), Routed::Text(InputEvent::Text("á".into())));
        assert_eq!(r.route(&key(0x2F, false, 0, None)), Routed::Drop);
        assert_eq!(r.route(&key(0x04, false, 0, None)), Routed::Drop);
        // With matching layouts the dead key is an ordinary physical key.
        let mut same = KeyRouter::new(KeyboardMode::Auto, true);
        assert!(matches!(same.route(&dead), Routed::Physical(_)));
    }

    #[test]
    fn reset_forgets_pending_text_keys() {
        let mut r = KeyRouter::new(KeyboardMode::Text, false);
        r.route(&key(0x04, true, 0, Some("a")));
        r.reset();
        assert!(matches!(r.route(&key(0x04, false, 0, None)), Routed::Physical(_)));
    }

    #[test]
    fn router_follows_a_mode_change() {
        let mut r = KeyRouter::new(KeyboardMode::Physical, false);
        assert!(matches!(r.route(&key(0x04, true, 0, Some("a"))), Routed::Physical(_)));
        r.set_mode(KeyboardMode::Auto);
        assert!(matches!(r.route(&key(0x05, true, 0, Some("b"))), Routed::Text(_)));
        r.set_layouts_match(true);
        assert!(matches!(r.route(&key(0x06, true, 0, Some("c"))), Routed::Physical(_)));
    }
}
