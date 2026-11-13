//! Physical keys or Unicode text (SPEC F-INPUT-01 and F-INPUT-02).
//!
//! The viewer reads a key as a HID usage and, from its own layout, the text the key produces (`text`). The decision:
//! - "Teclas físicas" (`Physical`): always the physical key.
//! - "Texto" (`Text`): printable text goes as Unicode text.
//! - "Automático" (`Auto`): physical while the two keyboard layouts match, text when they differ.
//! - Shortcuts are always physical, and so is every key that makes no printable text (Enter, arrows, F-keys,
//!   modifiers). Which modifiers make a shortcut depends on the viewer's OS ([`shortcut_mods`]): on Windows any of
//!   Ctrl, left Alt or Win (right Alt alone is AltGr, not a shortcut: `AltGr+Q` is text); on a Mac Control or Command
//!   only, because Option (either side) is the Mac's character modifier (`Option+C` is `ç` on a US-based layout).
//!
//! A key whose press went as text has no key-up on the wire; the router swallows that key-up (and sends the
//! auto-repeats as text again).

use crate::modifier;
use crate::shortcuts::ViewerOs;
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

/// Modifiers that make a key press a shortcut on a Windows viewer.
const SHORTCUT_MODS: u8 = modifier::LCTRL | modifier::RCTRL | modifier::LALT | modifier::LGUI | modifier::RGUI;
/// On a Mac viewer: Control and Command. Option types characters, like AltGr.
const MAC_SHORTCUT_MODS: u8 = modifier::LCTRL | modifier::RCTRL | modifier::LGUI | modifier::RGUI;

/// The modifiers (HID modifier byte, before the Ctrl/Cmd swap) that make a key press a shortcut on a viewer running
/// `os`: the press then goes as a physical key in every keyboard mode.
pub fn shortcut_mods(os: ViewerOs) -> u8 {
    match os {
        ViewerOs::Windows => SHORTCUT_MODS,
        ViewerOs::MacOs => MAC_SHORTCUT_MODS,
    }
}

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

/// The decision for one key press (down) on a Windows viewer. Pure: no memory of earlier keys.
pub fn decide(mode: KeyboardMode, layouts_match: bool, modifiers: u8, text: Option<&str>) -> Decision {
    decide_on(ViewerOs::Windows, mode, layouts_match, modifiers, text)
}

/// The decision for one key press (down) on a viewer running `os`.
pub fn decide_on(os: ViewerOs, mode: KeyboardMode, layouts_match: bool, modifiers: u8, text: Option<&str>) -> Decision {
    if !text_mode(mode, layouts_match) || modifiers & shortcut_mods(os) != 0 {
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
    os: ViewerOs,
    mode: KeyboardMode,
    layouts_match: bool,
    as_text: HashSet<(u16, u16)>,
}

impl KeyRouter {
    /// The router of a Windows viewer.
    pub fn new(mode: KeyboardMode, layouts_match: bool) -> Self {
        Self::for_os(ViewerOs::Windows, mode, layouts_match)
    }

    /// The router of a viewer running `os` (it decides which modifiers make a shortcut, [`shortcut_mods`]).
    pub fn for_os(os: ViewerOs, mode: KeyboardMode, layouts_match: bool) -> Self {
        Self { os, mode, layouts_match, as_text: HashSet::new() }
    }

    /// Text mode is in force now (the setting and the layout comparison).
    pub fn text_mode(&self) -> bool {
        text_mode(self.mode, self.layouts_match)
    }

    /// The press would be a shortcut with `modifiers` held: physical whatever its text.
    pub fn is_shortcut(&self, modifiers: u8) -> bool {
        modifiers & shortcut_mods(self.os) != 0
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
        if key.dead_key && self.text_mode() && !self.is_shortcut(key.modifiers) {
            self.as_text.insert(id);
            return Routed::Drop;
        }
        match decide_on(self.os, self.mode, self.layouts_match, key.modifiers, key.text) {
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
    fn on_a_mac_option_types_and_control_and_command_are_shortcuts() {
        let mac = |mods| decide_on(ViewerOs::MacOs, KeyboardMode::Auto, false, mods, Some("ç"));
        assert_eq!(mac(modifier::LALT), Decision::Text, "left Option + C");
        assert_eq!(mac(modifier::RALT | modifier::LSHIFT), Decision::Text);
        for m in [modifier::LCTRL, modifier::RCTRL, modifier::LGUI, modifier::RGUI, modifier::LGUI | modifier::LALT] {
            assert_eq!(mac(m), Decision::Physical, "mods {m:#04x}");
        }
        // The Windows rule is unchanged: left Alt is a shortcut there.
        assert_eq!(decide(KeyboardMode::Auto, false, modifier::LALT, Some("ç")), Decision::Physical);
        assert_eq!(shortcut_mods(ViewerOs::Windows) & modifier::LALT, modifier::LALT);
        assert_eq!(shortcut_mods(ViewerOs::MacOs) & (modifier::LALT | modifier::RALT), 0);
    }

    #[test]
    fn a_mac_router_drops_an_option_dead_key_and_keeps_command_physical() {
        let mut r = KeyRouter::for_os(ViewerOs::MacOs, KeyboardMode::Auto, false);
        let dead = KeyInput { dead_key: true, ..key(0x08, true, modifier::LALT, None) };
        assert_eq!(r.route(&dead), Routed::Drop, "Option+E on a US Mac: the acute waits for the letter");
        assert_eq!(r.route(&key(0x08, false, 0, None)), Routed::Drop, "its key-up has no record");
        assert!(matches!(r.route(&key(0x06, true, modifier::LGUI, Some("c"))), Routed::Physical(_)));
        assert!(r.is_shortcut(modifier::RCTRL) && !r.is_shortcut(modifier::RALT) && r.text_mode());
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
