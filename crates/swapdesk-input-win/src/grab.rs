//! The decisions of the keyboard grabber, as a state machine over plain data (no hook, no OS call): what a low-level
//! keyboard event means (HID usage, modifiers, locks), which events are Windows' own fakes, which keys are system
//! keys to swallow, and what goes to the host (physical key or text, with the Ctrl/Cmd swap). The hook in
//! [`crate::hook`] feeds it and applies its verdict.

use std::collections::HashSet;
use swapdesk_keys::mode::{KeyInput, KeyRouter, KeyboardMode, Routed};
use swapdesk_keys::swap::swap_event;
use swapdesk_keys::{PAGE_CONSUMER, PAGE_KEYBOARD, entries, modifier, win_to_hid};
use swapdesk_proto::InputEvent;

pub const LLKHF_EXTENDED: u32 = 0x01;
pub const LLKHF_INJECTED: u32 = 0x10;
pub const LLKHF_UP: u32 = 0x80;

const VK_PAUSE: u32 = 0x13;
const VK_NUMLOCK: u32 = 0x90;

/// One `KBDLLHOOKSTRUCT` as the grabber needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlKey {
    pub vk: u32,
    pub scan: u32,
    pub flags: u32,
}

impl LlKey {
    pub fn up(&self) -> bool {
        self.flags & LLKHF_UP != 0
    }

    pub fn injected(&self) -> bool {
        self.flags & LLKHF_INJECTED != 0
    }

    pub fn extended(&self) -> bool {
        self.flags & LLKHF_EXTENDED != 0
    }

    /// Windows marks the keys it makes up itself (the left Ctrl before AltGr, the shifts around NumLock-affected
    /// navigation keys) with bit 9 of the scan code: `0x21D`, `0x22A`, `0x236`. UNPROVEN as a rule until the ABNT2 test
    /// of this crate has seen it; the sink side of AltGr needs no rule (it injects the right Alt only).
    pub fn is_windows_fake(&self) -> bool {
        self.scan & 0x200 != 0
    }
}

/// What the viewer's layout says a key press types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Translated {
    pub text: Option<String>,
    pub dead: bool,
}

/// What the grabber does with events that `SendInput` injected (`LLKHF_INJECTED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Injected {
    /// Skip them: when the host and the viewer are one PC, the sink's own injections must not come back as viewer
    /// keys. The production setting.
    #[default]
    Ignore,
    /// Treat them like physical keys.
    Accept,
    /// Act on them and skip physical keys (the tests: a person typing at the PC must not reach the checks).
    Only,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrabConfig {
    pub mode: KeyboardMode,
    /// The viewer's and the host's layouts match ("Automático" then sends physical keys).
    pub layouts_match: bool,
    /// "Trocar Ctrl e Cmd no Mac": applied before sending (C4 rule 4).
    pub swap_ctrl_cmd: bool,
    pub injected: Injected,
}

impl Default for GrabConfig {
    fn default() -> Self {
        Self { mode: KeyboardMode::Physical, layouts_match: true, swap_ctrl_cmd: false, injected: Injected::Ignore }
    }
}

/// The verdict for one event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Records for the host, in order.
    pub events: Vec<InputEvent>,
    /// Keep the event from the viewer's own OS and window.
    pub swallow: bool,
}

#[derive(Debug, Clone)]
pub struct GrabState {
    config: GrabConfig,
    router: KeyRouter,
    grab_system_keys: bool,
    modifiers: u8,
    locks: u8,
    /// Keys whose press was swallowed: their release is swallowed too, so the OS never sees an orphan key-up.
    swallowed: HashSet<(u16, u16)>,
    /// Keys currently down (to tell an auto-repeat from a press for the lock toggles).
    down: HashSet<(u16, u16)>,
}

impl GrabState {
    pub fn new(config: GrabConfig, initial_locks: u8) -> Self {
        Self {
            config,
            router: KeyRouter::new(config.mode, config.layouts_match),
            grab_system_keys: false,
            modifiers: 0,
            locks: initial_locks & 0b111,
            swallowed: HashSet::new(),
            down: HashSet::new(),
        }
    }

    pub fn config(&self) -> GrabConfig {
        self.config
    }

    pub fn set_config(&mut self, config: GrabConfig) {
        self.config = config;
        self.router.set_mode(config.mode);
        self.router.set_layouts_match(config.layouts_match);
    }

    pub fn grab_system_keys(&self) -> bool {
        self.grab_system_keys
    }

    /// Switches system-key capture. Turning it off while a swallowed key is held returns `true`: the caller sends
    /// `ReleaseAll` so the host does not keep a key the viewer's OS will never release.
    pub fn set_grab_system_keys(&mut self, on: bool) -> bool {
        let was_on = std::mem::replace(&mut self.grab_system_keys, on);
        if was_on && !on {
            let held = !self.swallowed.is_empty();
            // The host is told to release everything (the caller sends `ReleaseAll`): forget those keys here too, so a
            // swallowed Win key does not stay in the modifier byte of every later record.
            for (page, usage) in self.swallowed.drain() {
                self.modifiers = modifier::after(self.modifiers, page, usage, false);
                self.down.remove(&(page, usage));
            }
            return held;
        }
        false
    }

    /// Forgets everything held (focus lost, the grab ends).
    pub fn reset(&mut self) {
        self.modifiers = 0;
        self.swallowed.clear();
        self.down.clear();
        self.router.reset();
    }

    pub fn modifiers(&self) -> u8 {
        self.modifiers
    }

    pub fn locks(&self) -> u8 {
        self.locks
    }

    /// Whether a key press would need the viewer's layout to say what it types (the hook only asks then).
    pub fn wants_text(&self, key: &LlKey) -> bool {
        if key.up() || !swapdesk_keys::mode::text_mode(self.config.mode, self.config.layouts_match) {
            return false;
        }
        match usage_of(key) {
            Some((page, usage)) => modifier::bit_of(page, usage).is_none(),
            None => false,
        }
    }

    /// The meaning of one low-level keyboard event. `translate` is called only when `wants_text` holds.
    pub fn on_key(&mut self, key: LlKey, translate: impl FnOnce(&LlKey, u8) -> Translated) -> Verdict {
        let skip = match self.config.injected {
            Injected::Ignore => key.injected(),
            Injected::Accept => false,
            Injected::Only => !key.injected(),
        };
        if skip {
            return Verdict::default();
        }
        if key.is_windows_fake() {
            // Windows' own AltGr Ctrl and NumLock shifts: nothing for the host (it makes its own), nothing to swallow.
            return Verdict::default();
        }
        let Some((page, usage)) = usage_of(&key) else {
            return Verdict::default();
        };
        let id = (page, usage);
        let down = !key.up();
        let repeat = down && self.down.contains(&id);

        let mods_before = self.modifiers;
        let swallow = if down {
            let system = self.grab_system_keys && is_system_key(page, usage, mods_before);
            if system {
                self.swallowed.insert(id);
            }
            system
        } else {
            self.swallowed.remove(&id)
        };
        if down {
            self.down.insert(id);
        } else {
            self.down.remove(&id);
        }
        self.modifiers = modifier::after(self.modifiers, page, usage, down);
        if down
            && !repeat
            && let Some(bit) = lock_bit(page, usage)
        {
            self.locks ^= bit;
        }

        let t = if self.wants_text(&key) { translate(&key, self.modifiers) } else { Translated::default() };
        let input = KeyInput {
            usage_page: page,
            usage,
            down,
            modifiers: self.modifiers,
            locks: self.locks,
            text: t.text.as_deref(),
            dead_key: t.dead,
        };
        let events = match self.router.route(&input) {
            Routed::Physical(e) | Routed::Text(e) => {
                let e = if self.config.swap_ctrl_cmd { swap_event(&e) } else { e };
                vec![e]
            }
            Routed::Drop => Vec::new(),
        };
        Verdict { events, swallow }
    }
}

/// Win, Alt+Tab, Alt+Esc and Ctrl+Esc (F-INPUT-04). `mods` is the modifier byte before this key.
pub fn is_system_key(page: u16, usage: u16, mods: u8) -> bool {
    if page != PAGE_KEYBOARD {
        return false;
    }
    let alt = mods & (modifier::LALT | modifier::RALT) != 0;
    let ctrl = mods & (modifier::LCTRL | modifier::RCTRL) != 0;
    match usage {
        0xE3 | 0xE7 => true,
        0x2B => alt,
        0x29 => alt || ctrl,
        _ => false,
    }
}

fn lock_bit(page: u16, usage: u16) -> Option<u8> {
    if page != PAGE_KEYBOARD {
        return None;
    }
    match usage {
        0x39 => Some(1),
        0x53 => Some(2),
        0x47 => Some(4),
        _ => None,
    }
}

/// The HID key of a low-level event: Pause and NumLock by virtual key (their scan codes are shared), the rest by scan
/// code and the extended flag, media keys by virtual key when the scan code is unknown.
pub fn usage_of(key: &LlKey) -> Option<(u16, u16)> {
    match key.vk {
        VK_PAUSE => return Some((PAGE_KEYBOARD, 0x48)),
        VK_NUMLOCK => return Some((PAGE_KEYBOARD, 0x53)),
        _ => {}
    }
    let code = (key.scan & 0xFF) as u16;
    let scan = if key.extended() { 0xE000 | code } else { code };
    if let Some(hid) = win_to_hid(scan) {
        return Some(hid);
    }
    entries()
        .iter()
        .find(|r| r.page == PAGE_CONSUMER && r.win_vk.is_some_and(|v| u32::from(v) == key.vk))
        .map(|r| (r.page, r.usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(vk: u32, scan: u32, ext: bool, up: bool) -> LlKey {
        LlKey { vk, scan, flags: (if ext { LLKHF_EXTENDED } else { 0 }) | (if up { LLKHF_UP } else { 0 }) }
    }

    fn no_text(_: &LlKey, _: u8) -> Translated {
        Translated::default()
    }

    fn key_event(usage: u16, down: bool, mods: u8, locks: u8) -> InputEvent {
        InputEvent::Key { usage_page: 7, usage, down, modifiers: mods, locks }
    }

    fn state() -> GrabState {
        GrabState::new(GrabConfig::default(), 0)
    }

    #[test]
    fn plain_keys_become_key_records_with_tracked_modifiers() {
        let mut s = state();
        let v = s.on_key(k(0xA0, 0x2A, false, false), no_text);
        assert_eq!(v, Verdict { events: vec![key_event(0xE1, true, modifier::LSHIFT, 0)], swallow: false });
        let v = s.on_key(k(0x41, 0x1E, false, false), no_text);
        assert_eq!(v.events, vec![key_event(0x04, true, modifier::LSHIFT, 0)]);
        let v = s.on_key(k(0xA0, 0x2A, false, true), no_text);
        assert_eq!(v.events, vec![key_event(0xE1, false, 0, 0)]);
        assert_eq!(s.modifiers(), 0);
    }

    #[test]
    fn extended_flag_selects_the_extended_key() {
        let mut s = state();
        // Scan 0x48 extended is Up arrow, not extended is keypad 8.
        assert_eq!(s.on_key(k(0x26, 0x48, true, false), no_text).events, vec![key_event(0x52, true, 0, 0)]);
        assert_eq!(s.on_key(k(0x68, 0x48, false, false), no_text).events, vec![key_event(0x60, true, 0, 0)]);
    }

    #[test]
    fn pause_and_numlock_are_read_by_virtual_key() {
        let mut s = state();
        assert_eq!(s.on_key(k(0x13, 0x45, false, false), no_text).events, vec![key_event(0x48, true, 0, 0)]);
        // NumLock toggles the lock bit, once per press (not per auto-repeat).
        let v = s.on_key(k(0x90, 0x45, true, false), no_text);
        assert_eq!(v.events, vec![key_event(0x53, true, 0, 0b010)]);
        let v = s.on_key(k(0x90, 0x45, true, false), no_text);
        assert_eq!(v.events, vec![key_event(0x53, true, 0, 0b010)], "auto-repeat does not toggle again");
    }

    #[test]
    fn media_keys_are_consumer_page_records() {
        let mut s = state();
        let v = s.on_key(k(0xB3, 0x22, true, false), no_text);
        assert_eq!(
            v.events,
            vec![InputEvent::Key { usage_page: 0x0C, usage: 0xCD, down: true, modifiers: 0, locks: 0 }]
        );
    }

    #[test]
    fn altgr_on_abnt2_sends_the_right_alt_only_and_drops_the_fake_left_ctrl() {
        let mut s = state();
        // What Windows delivers for AltGr: a fake left Ctrl (scan 0x21D), then the extended right Alt.
        let fake_down = s.on_key(k(0xA2, 0x21D, false, false), no_text);
        assert_eq!(fake_down, Verdict::default(), "the fake Ctrl is neither sent nor swallowed");
        let alt = s.on_key(k(0xA5, 0x38, true, false), no_text);
        assert_eq!(alt.events, vec![key_event(0xE6, true, modifier::RALT, 0)]);
        let q = s.on_key(k(0x51, 0x10, false, false), no_text);
        assert_eq!(q.events, vec![key_event(0x14, true, modifier::RALT, 0)], "no Ctrl bit in the modifier byte");
        let fake_up = s.on_key(k(0xA2, 0x21D, false, true), no_text);
        assert_eq!(fake_up, Verdict::default());
        let alt_up = s.on_key(k(0xA5, 0x38, true, true), no_text);
        assert_eq!(alt_up.events, vec![key_event(0xE6, false, 0, 0)]);
        // A real left Ctrl still goes.
        assert_eq!(
            s.on_key(k(0xA2, 0x1D, false, false), no_text).events,
            vec![key_event(0xE0, true, modifier::LCTRL, 0)]
        );
    }

    #[test]
    fn numlock_fake_shifts_are_dropped() {
        let mut s = state();
        assert_eq!(s.on_key(k(0xA0, 0x22A, true, false), no_text), Verdict::default());
        assert_eq!(s.modifiers(), 0);
    }

    #[test]
    fn injected_events_are_ignored_unless_accepted() {
        let mut s = state();
        let injected = LlKey { vk: 0x41, scan: 0x1E, flags: LLKHF_INJECTED };
        assert_eq!(s.on_key(injected, no_text), Verdict::default());
        let mut s = GrabState::new(GrabConfig { injected: Injected::Accept, ..GrabConfig::default() }, 0);
        assert_eq!(s.on_key(injected, no_text).events.len(), 1);
        let physical = LlKey { vk: 0x41, scan: 0x1E, flags: 0 };
        assert_eq!(s.on_key(physical, no_text).events.len(), 1, "Accept takes both");
        let mut only = GrabState::new(GrabConfig { injected: Injected::Only, ..GrabConfig::default() }, 0);
        assert_eq!(only.on_key(physical, no_text), Verdict::default(), "Only skips physical keys");
        assert_eq!(only.on_key(injected, no_text).events.len(), 1);
    }

    #[test]
    fn unknown_keys_pass_untouched() {
        let mut s = state();
        assert_eq!(s.on_key(k(0xFF, 0x7F, false, false), no_text), Verdict::default());
    }

    #[test]
    fn system_keys_are_swallowed_only_while_the_grab_is_on_and_sent_either_way() {
        let mut s = state();
        let v = s.on_key(k(0x5B, 0x5B, true, false), no_text);
        assert!(!v.swallow, "grab off: the viewer's OS keeps its Win key");
        assert_eq!(v.events, vec![key_event(0xE3, true, modifier::LGUI, 0)]);
        s.on_key(k(0x5B, 0x5B, true, true), no_text);

        s.set_grab_system_keys(true);
        let down = s.on_key(k(0x5B, 0x5B, true, false), no_text);
        assert!(down.swallow);
        assert_eq!(down.events, vec![key_event(0xE3, true, modifier::LGUI, 0)]);
        let up = s.on_key(k(0x5B, 0x5B, true, true), no_text);
        assert!(up.swallow, "the release is swallowed with the press");
        assert_eq!(up.events, vec![key_event(0xE3, false, 0, 0)]);
    }

    #[test]
    fn alt_tab_alt_esc_ctrl_esc_and_the_right_win_key_are_system_keys() {
        for (mods, page, usage, expect) in [
            (modifier::LALT, 7u16, 0x2Bu16, true),
            (modifier::RALT, 7, 0x2B, true),
            (modifier::LALT, 7, 0x29, true),
            (modifier::LCTRL, 7, 0x29, true),
            (modifier::RCTRL, 7, 0x29, true),
            (0, 7, 0x2B, false),
            (0, 7, 0x29, false),
            (modifier::LSHIFT, 7, 0x2B, false),
            (modifier::LCTRL, 7, 0x2B, false),
            (0, 7, 0xE7, true),
            (0, 7, 0xE3, true),
            (0, 7, 0x04, false),
            (0, 0x0C, 0xE3, false),
        ] {
            assert_eq!(is_system_key(page, usage, mods), expect, "mods {mods:#04x} usage {usage:#x}");
        }
    }

    #[test]
    fn alt_tab_through_the_state_swallows_tab_but_not_alt() {
        let mut s = state();
        s.set_grab_system_keys(true);
        let alt = s.on_key(k(0xA4, 0x38, false, false), no_text);
        assert!(!alt.swallow);
        let tab = s.on_key(k(0x09, 0x0F, false, false), no_text);
        assert!(tab.swallow);
        assert_eq!(tab.events, vec![key_event(0x2B, true, modifier::LALT, 0)]);
        assert!(s.on_key(k(0x09, 0x0F, false, true), no_text).swallow);
        assert!(!s.on_key(k(0xA4, 0x38, false, true), no_text).swallow);
    }

    #[test]
    fn disabling_the_grab_with_a_swallowed_key_held_asks_for_a_release_all() {
        let mut s = state();
        assert!(!s.set_grab_system_keys(true));
        s.on_key(k(0x5B, 0x5B, true, false), no_text);
        assert_eq!(s.modifiers(), modifier::LGUI);
        assert!(s.set_grab_system_keys(false), "a swallowed key is still held");
        assert_eq!(s.modifiers(), 0, "the swallowed Win key leaves the modifier byte");
        // Its release now goes to the OS (it was not swallowed), harmlessly.
        assert!(!s.on_key(k(0x5B, 0x5B, true, true), no_text).swallow);
        // Nothing held: no release needed.
        s.set_grab_system_keys(true);
        assert!(!s.set_grab_system_keys(false));
    }

    #[test]
    fn text_mode_asks_the_layout_and_sends_text_for_printable_keys_only() {
        let cfg = GrabConfig { mode: KeyboardMode::Auto, layouts_match: false, ..GrabConfig::default() };
        let mut s = GrabState::new(cfg, 0);
        let c_cedilla = k(0xBA, 0x27, false, false);
        assert!(s.wants_text(&c_cedilla));
        let v = s.on_key(c_cedilla, |_, _| Translated { text: Some("ç".into()), dead: false });
        assert_eq!(v.events, vec![InputEvent::Text("ç".into())]);
        // Its key-up is swallowed from the wire (but not from the OS: swallow stays false).
        let up = s.on_key(k(0xBA, 0x27, false, true), no_text);
        assert_eq!(up, Verdict::default());
        // A modifier never asks.
        assert!(!s.wants_text(&k(0xA0, 0x2A, false, false)));
        // Enter has no text: physical.
        let enter = s.on_key(k(0x0D, 0x1C, false, false), |_, _| Translated::default());
        assert_eq!(enter.events, vec![key_event(0x28, true, 0, 0)]);
    }

    #[test]
    fn text_mode_composes_a_dead_key_with_the_next_letter() {
        let cfg = GrabConfig { mode: KeyboardMode::Text, layouts_match: false, ..GrabConfig::default() };
        let mut s = GrabState::new(cfg, 0);
        let dead = s.on_key(k(0xDE, 0x28, false, false), |_, _| Translated { text: None, dead: true });
        assert_eq!(dead.events, vec![]);
        let a = s.on_key(k(0x41, 0x1E, false, false), |_, _| Translated { text: Some("ã".into()), dead: false });
        assert_eq!(a.events, vec![InputEvent::Text("ã".into())]);
    }

    #[test]
    fn shortcuts_stay_physical_in_text_mode() {
        let cfg = GrabConfig { mode: KeyboardMode::Text, layouts_match: false, ..GrabConfig::default() };
        let mut s = GrabState::new(cfg, 0);
        s.on_key(k(0xA2, 0x1D, false, false), no_text);
        let c = s.on_key(k(0x43, 0x2E, false, false), |_, _| Translated { text: Some("c".into()), dead: false });
        assert_eq!(c.events, vec![key_event(0x06, true, modifier::LCTRL, 0)]);
    }

    #[test]
    fn ctrl_cmd_swap_applies_to_sent_keys_but_the_system_key_rule_reads_the_real_keys() {
        let cfg = GrabConfig { swap_ctrl_cmd: true, ..GrabConfig::default() };
        let mut s = GrabState::new(cfg, 0);
        s.set_grab_system_keys(true);
        let ctrl = s.on_key(k(0xA2, 0x1D, false, false), no_text);
        assert_eq!(ctrl.events, vec![key_event(0xE3, true, modifier::LGUI, 0)], "Ctrl goes as Cmd");
        let c = s.on_key(k(0x43, 0x2E, false, false), no_text);
        assert_eq!(c.events, vec![key_event(0x06, true, modifier::LGUI, 0)]);
        // The viewer's Win key is the viewer's Win key: swallowed, and sent as Ctrl.
        let win = s.on_key(k(0x5B, 0x5B, true, false), no_text);
        assert!(win.swallow);
        assert_eq!(
            win.events[0],
            InputEvent::Key {
                usage_page: 7,
                usage: 0xE0,
                down: true,
                modifiers: modifier::LCTRL | modifier::LGUI,
                locks: 0
            }
        );
    }

    #[test]
    fn reset_forgets_modifiers_and_swallowed_keys() {
        let mut s = state();
        s.set_grab_system_keys(true);
        s.on_key(k(0x5B, 0x5B, true, false), no_text);
        s.reset();
        assert_eq!(s.modifiers(), 0);
        assert!(!s.set_grab_system_keys(false));
    }
}
