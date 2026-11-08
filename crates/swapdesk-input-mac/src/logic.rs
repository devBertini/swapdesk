//! The decisions of the viewer's input, as a state machine over plain data (no AppKit, no event tap, no channel): what a
//! key, modifier change, mouse event or focus change means on the wire. [`crate::MacInput`] feeds it and sends what it
//! returns; the event tap feeds it too, for the system shortcuts.
//!
//! What it keeps, and why:
//! - the HID modifier byte of the viewer's own keyboard (`mods`): the wire carries "the viewer's state after the event"
//!   (PROTOCOL.md 7.3), and AppKit reports only side-less flags, so the left or right key comes from `KeyProbe`;
//! - the keys held (`down`), so a key-up with no key-down (Cmd+Q, which the shell lets through) is not taken, and
//!   `ReleaseAll` on focus loss leaves nothing pressed on the host (F-INPUT-04);
//! - the buttons held and the last pointer position, so a button never goes down outside the picture and a position is
//!   sent only when it changes;
//! - the fractions of a wheel notch not yet sent, so slow trackpad motion adds up instead of vanishing.

use std::collections::{BTreeSet, HashSet};

use swapdesk_keys::mode::{KeyInput, KeyRouter, KeyboardMode, Routed};
use swapdesk_keys::swap::swap_event;
use swapdesk_keys::{KbKind, PAGE_KEYBOARD, modifier};
use swapdesk_proto::{InputEvent, MouseButton};
use swapdesk_shell_mac::{KeyMsg, Modifiers, MouseKind, MouseMsg, PointerButton};

use crate::keymap::{aggregate_of_bit, hid_of_kvk, is_local_escape, is_system_shortcut, modifier_kvk, typed_text};

/// Whether a physical key is down right now, by `kVK_*` code (`CGEventSourceKeyState` on a Mac).
pub type KeyProbe<'a> = &'a dyn Fn(u16) -> bool;

/// Where a window point falls in the remote display (`Renderer::map_pointer`): `None` outside the picture.
pub type PointerMapFn<'a> = &'a dyn Fn(i32, i32) -> Option<(u16, u16)>;

/// Wheel conversion (GUESS numbers, to be tuned on the owner's trackpad and mouse).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollConfig {
    /// Trackpad pixels (points) that make one wheel notch (120 wire units).
    pub pixels_per_notch: f64,
    /// Wheel notches per line of a mouse wheel (`scrollingDeltaY` counts lines when not precise).
    pub notches_per_line: f64,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self { pixels_per_notch: 40.0, notches_per_line: 1.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputConfig {
    /// "Teclas físicas", "Texto" or "Automático" (F-INPUT-01, F-INPUT-02).
    pub mode: KeyboardMode,
    /// The viewer's and the host's keyboard layouts match ("Automático" then sends physical keys).
    pub layouts_match: bool,
    /// "Trocar Ctrl e Cmd no Mac" (C4 rule 4), applied before sending.
    pub swap_ctrl_cmd: bool,
    /// The viewer's own keyboard kind, for the ISO section/grave swap (C4 rule 2).
    pub keyboard: KbKind,
    /// The remote display the pointer events address (`DisplayInfo::id`).
    pub display: u16,
    pub scroll: ScrollConfig,
    /// Num Lock bit of every `Key` record. A Mac keyboard has no Num Lock but the Windows sink makes the host's
    /// toggles equal to the record's, so `true` keeps the host's keypad on digits (decision: revert with `false`).
    pub num_lock: bool,
    /// Hide the local pointer over the video (the remote cursor is drawn instead).
    pub hide_local_pointer: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            mode: KeyboardMode::Physical,
            layouts_match: true,
            swap_ctrl_cmd: false,
            keyboard: KbKind::Ansi,
            display: 0,
            scroll: ScrollConfig::default(),
            num_lock: true,
            hide_local_pointer: true,
        }
    }
}

/// What the state machine decided for one event.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Records for the host, in order.
    pub events: Vec<InputEvent>,
    /// The event is the viewer's: keep it from the rest of AppKit (or, in the tap, from macOS).
    pub taken: bool,
}

impl Outcome {
    fn taken(events: Vec<InputEvent>) -> Self {
        Self { events, taken: true }
    }
    fn passed() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone)]
pub struct InputCore {
    config: InputConfig,
    router: KeyRouter,
    /// The viewer's HID modifier byte (before the Ctrl/Cmd swap).
    mods: u8,
    caps: bool,
    /// `kVK_*` of the keys held (taken key-downs without their key-up).
    down: HashSet<u16>,
    /// Keys whose key-down the event tap swallowed: their key-up is swallowed too, so macOS never sees an orphan.
    swallowed: HashSet<u16>,
    buttons: BTreeSet<u8>,
    pointer: Option<(u16, u16)>,
    /// Wheel units (1/120 notch) not sent yet, x then y.
    wheel_rest: (f64, f64),
}

fn wire_button(button: PointerButton) -> Option<MouseButton> {
    match button {
        PointerButton::Left => Some(MouseButton::Left),
        PointerButton::Right => Some(MouseButton::Right),
        PointerButton::Middle => Some(MouseButton::Middle),
        // NSEvent.buttonNumber 3 and 4 are the side buttons: back, forward.
        PointerButton::Other(3) => Some(MouseButton::Back),
        PointerButton::Other(4) => Some(MouseButton::Forward),
        PointerButton::Other(_) => None,
    }
}

fn clamp_i16(v: f64) -> i16 {
    v.clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

impl InputCore {
    pub fn new(config: InputConfig) -> Self {
        Self {
            config,
            router: KeyRouter::new(config.mode, config.layouts_match),
            mods: 0,
            caps: false,
            down: HashSet::new(),
            swallowed: HashSet::new(),
            buttons: BTreeSet::new(),
            pointer: None,
            wheel_rest: (0.0, 0.0),
        }
    }

    pub fn config(&self) -> InputConfig {
        self.config
    }

    pub fn set_config(&mut self, config: InputConfig) {
        self.config = config;
        self.router.set_mode(config.mode);
        self.router.set_layouts_match(config.layouts_match);
    }

    /// The viewer's HID modifier byte now.
    pub fn modifiers(&self) -> u8 {
        self.mods
    }

    fn locks(&self) -> u8 {
        u8::from(self.caps) | if self.config.num_lock { 0b010 } else { 0 }
    }

    fn finish(&self, event: InputEvent) -> InputEvent {
        if self.config.swap_ctrl_cmd { swap_event(&event) } else { event }
    }

    fn key_event(&self, usage: u16, down: bool, mods: u8) -> InputEvent {
        self.finish(InputEvent::Key { usage_page: PAGE_KEYBOARD, usage, down, modifiers: mods, locks: self.locks() })
    }

    /// Brings the modifier byte to `wanted`, one record per key that changed (bit order, each carrying the byte after
    /// its own change), and returns them.
    fn move_mods_to(&mut self, wanted: u8) -> Vec<InputEvent> {
        let mut events = Vec::new();
        for i in 0..8 {
            let bit = 1u8 << i;
            if (self.mods ^ wanted) & bit == 0 {
                continue;
            }
            let down = wanted & bit != 0;
            self.mods = if down { self.mods | bit } else { self.mods & !bit };
            if let Some(usage) = modifier::usage_of(bit) {
                events.push(self.key_event(usage, down, self.mods));
            }
        }
        events
    }

    /// The modifier byte the viewer's keyboard holds, from the aggregate flags AppKit reports and the side-specific
    /// state of the keys: an aggregate flag that is set but whose two keys the probe cannot see stands for the left one.
    fn wanted_mods(flags: Modifiers, probe: KeyProbe<'_>) -> u8 {
        let mut wanted = 0u8;
        for group in [
            [modifier::LSHIFT, modifier::RSHIFT],
            [modifier::LCTRL, modifier::RCTRL],
            [modifier::LALT, modifier::RALT],
            [modifier::LGUI, modifier::RGUI],
        ] {
            if !flags.contains(aggregate_of_bit(group[0])) {
                continue;
            }
            let seen: Vec<u8> = group.into_iter().filter(|b| modifier_kvk(*b).is_some_and(probe)).collect();
            if seen.is_empty() {
                wanted |= group[0];
            } else {
                wanted |= seen.into_iter().fold(0, |a, b| a | b);
            }
        }
        wanted
    }

    /// `flagsChanged:` (and, as a safety net, any event that carries the flags): sync the modifier keys and the Caps
    /// Lock toggle to the host.
    pub fn modifiers_changed(&mut self, flags: Modifiers, probe: KeyProbe<'_>) -> Outcome {
        let wanted = Self::wanted_mods(flags, probe);
        let mut events = self.move_mods_to(wanted);
        let caps = flags.contains(Modifiers::CAPS_LOCK);
        if caps != self.caps {
            self.caps = caps;
            // The toggle's new state rides on the record of its own key press; the sink presses it, never reads it.
            let usage = 0x39;
            events.push(self.key_event(usage, true, self.mods));
            events.push(self.key_event(usage, false, self.mods));
        }
        Outcome::taken(events)
    }

    /// The window became key (or the grab starts): modifiers already held on the viewer's keyboard are sent now.
    /// Caps Lock is taken as the viewer's state at that moment without a key press.
    pub fn focus_gained(&mut self, flags: Modifiers, probe: KeyProbe<'_>) -> Vec<InputEvent> {
        self.caps = flags.contains(Modifiers::CAPS_LOCK);
        let wanted = Self::wanted_mods(flags, probe);
        self.move_mods_to(wanted)
    }

    /// Focus lost, the grab ended or system-key capture changed under a held key: every key and button is up on the
    /// host (`ReleaseAll`), and nothing is held here. The Caps Lock toggle is a state, not a held key: it stays.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        self.mods = 0;
        self.down.clear();
        self.swallowed.clear();
        self.buttons.clear();
        self.pointer = None;
        self.wheel_rest = (0.0, 0.0);
        self.router.reset();
        vec![InputEvent::ReleaseAll]
    }

    /// Nothing held, nothing to release (the host has nothing of ours down).
    pub fn is_idle(&self) -> bool {
        self.mods == 0 && self.down.is_empty() && self.buttons.is_empty()
    }

    /// A position record was not delivered (the consumer was behind): the next move or click must send it again.
    pub fn forget_pointer(&mut self) {
        self.pointer = None;
    }

    /// A key press or release from the video view.
    pub fn key(&mut self, msg: &KeyMsg, probe: KeyProbe<'_>) -> Outcome {
        if msg.down && is_local_escape(msg.key_code, msg.modifiers) {
            return Outcome::passed();
        }
        if !msg.down && !self.down.contains(&msg.key_code) {
            // A release whose press we never took (Cmd+Q's, or one from before the grab): not ours.
            return Outcome::passed();
        }
        let mut events = Vec::new();
        if msg.down {
            // The flags the key carries are the truth at its press; bring the held modifiers to them first.
            let wanted = Self::wanted_mods(msg.modifiers, probe);
            if wanted != self.mods {
                events.extend(self.move_mods_to(wanted));
            }
        }
        events.extend(self.translate(msg.key_code, msg.down, typed_text(msg.text.as_deref())));
        Outcome::taken(events)
    }

    fn translate(&mut self, kvk: u16, down: bool, text: Option<&str>) -> Vec<InputEvent> {
        if down {
            self.down.insert(kvk);
        } else {
            self.down.remove(&kvk);
        }
        let Some((page, usage)) = hid_of_kvk(kvk, self.config.keyboard) else {
            // A key with no HID row (the fn key, Mission Control keys): nothing to send, and macOS shall not see it.
            return Vec::new();
        };
        let input = KeyInput {
            usage_page: page,
            usage,
            down,
            modifiers: self.mods,
            locks: self.locks(),
            text,
            dead_key: false,
        };
        match self.router.route(&input) {
            Routed::Physical(e) | Routed::Text(e) => vec![self.finish(e)],
            Routed::Drop => Vec::new(),
        }
    }

    /// A key event the event tap saw. Swallows (and sends) only the system shortcuts, and the release of a key it
    /// swallowed; everything else reaches the view, which sends it through [`InputCore::key`].
    pub fn tap_key(&mut self, kvk: u16, down: bool, flags: Modifiers, probe: KeyProbe<'_>) -> Outcome {
        if !down {
            return if self.swallowed.remove(&kvk) {
                Outcome::taken(self.translate(kvk, false, None))
            } else {
                Outcome::passed()
            };
        }
        if !is_system_shortcut(kvk, flags) || hid_of_kvk(kvk, self.config.keyboard).is_none() {
            return Outcome::passed();
        }
        let mut events = Vec::new();
        let wanted = Self::wanted_mods(flags, probe);
        if wanted != self.mods {
            events.extend(self.move_mods_to(wanted));
        }
        self.swallowed.insert(kvk);
        events.extend(self.translate(kvk, true, None));
        Outcome::taken(events)
    }

    /// System-key capture went off while a swallowed key is held: macOS will never see its release, so the host must
    /// let go of it. Returns true when a `ReleaseAll` is due (and has forgotten what it releases).
    pub fn system_keys_off(&mut self) -> bool {
        if self.swallowed.is_empty() {
            return false;
        }
        self.release_all();
        true
    }

    fn pointer_event(&mut self, at: (u16, u16)) -> Option<InputEvent> {
        if self.pointer == Some(at) {
            return None;
        }
        self.pointer = Some(at);
        Some(InputEvent::Pointer { display: self.config.display, x: at.0, y: at.1 })
    }

    /// A mouse event of the video view. `scale` is the window's pixels per point; `map` is the renderer's
    /// `map_pointer` over window pixels.
    pub fn mouse(&mut self, msg: MouseMsg, map: Option<PointerMapFn<'_>>, scale: f64) -> Outcome {
        let at = map.and_then(|m| {
            let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
            let (px, py) = ((msg.x * scale).round(), (msg.y * scale).round());
            if !px.is_finite() || !py.is_finite() || px.abs() > 1e9 || py.abs() > 1e9 {
                return None;
            }
            m(px as i32, py as i32)
        });
        let mut events = Vec::new();
        match msg.kind {
            MouseKind::Move | MouseKind::Drag { .. } => {
                if let Some(at) = at {
                    events.extend(self.pointer_event(at));
                }
            }
            MouseKind::Button { button, down } => {
                let Some(wire) = wire_button(button) else {
                    return Outcome::taken(events);
                };
                let id = wire as u8;
                if down {
                    // A press in the letterbox or outside the picture clicks nothing, and its release is not sent.
                    if let Some(at) = at {
                        events.extend(self.pointer_event(at));
                        self.buttons.insert(id);
                        events.push(InputEvent::Button { button: wire, down: true });
                    }
                } else if self.buttons.remove(&id) {
                    if let Some(at) = at {
                        events.extend(self.pointer_event(at));
                    }
                    events.push(InputEvent::Button { button: wire, down: false });
                }
            }
            MouseKind::Scroll { dx, dy, precise } => {
                if let Some(at) = at {
                    let (ux, uy) = self.wheel_units(dx, dy, precise);
                    if ux != 0 || uy != 0 {
                        events.extend(self.pointer_event(at));
                        events.push(InputEvent::Wheel { dx: ux, dy: uy });
                    }
                }
            }
        }
        Outcome::taken(events)
    }

    /// Wheel units (1/120 notch) of one scroll event, with the remainder carried to the next.
    ///
    /// Signs: AppKit's deltas follow the content (`deltaY > 0` is "scroll up", `deltaX > 0` "scroll left": the view's
    /// origin moves by minus the delta), and the wire has `dy > 0` away from the user (up) and `dx > 0` right. So `dy`
    /// keeps its sign and `dx` flips. UNPROVEN on the hardware: no primary page states the signs, a probe on the
    /// owner's trackpad settles it.
    fn wheel_units(&mut self, dx: f64, dy: f64, precise: bool) -> (i16, i16) {
        let per_unit = if precise {
            120.0 / self.config.scroll.pixels_per_notch
        } else {
            120.0 * self.config.scroll.notches_per_line
        };
        let take = |rest: &mut f64, delta: f64| {
            if !delta.is_finite() {
                return 0;
            }
            if delta != 0.0 && rest.signum() != delta.signum() {
                // A turn around starts again: the old remainder belonged to the other direction.
                *rest = 0.0;
            }
            *rest += delta * per_unit;
            let whole = rest.trunc();
            *rest -= whole;
            clamp_i16(whole)
        };
        let x = -take(&mut self.wheel_rest.0, dx);
        let y = take(&mut self.wheel_rest.1, dy);
        (x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{KVK_SPACE, KVK_TAB};

    const CMD: Modifiers = Modifiers::COMMAND;

    fn none(_: u16) -> bool {
        false
    }

    fn core() -> InputCore {
        InputCore::new(InputConfig::default())
    }

    fn key(kvk: u16, down: bool, mods: Modifiers, text: Option<&str>) -> KeyMsg {
        KeyMsg { key_code: kvk, down, repeat: false, text: text.map(str::to_owned), modifiers: mods }
    }

    fn k(usage: u16, down: bool, mods: u8) -> InputEvent {
        InputEvent::Key { usage_page: 7, usage, down, modifiers: mods, locks: 0b010 }
    }

    fn mouse(kind: MouseKind, x: f64, y: f64) -> MouseMsg {
        MouseMsg { kind, x, y, modifiers: Modifiers::default() }
    }

    /// A 1920x1080 picture filling a 960x540 point view on a 2x screen, mapped linearly.
    fn identity_map(x: i32, y: i32) -> Option<(u16, u16)> {
        if !(0..1920).contains(&x) || !(0..1080).contains(&y) {
            return None;
        }
        Some((((x as u32) * 65535 / 1919) as u16, ((y as u32) * 65535 / 1079) as u16))
    }

    // ----- keys

    #[test]
    fn a_plain_key_is_a_physical_record_with_the_viewers_state() {
        let mut c = core();
        let down = c.key(&key(0x00, true, Modifiers::default(), Some("a")), &none);
        assert_eq!(down, Outcome { events: vec![k(0x04, true, 0)], taken: true });
        let up = c.key(&key(0x00, false, Modifiers::default(), None), &none);
        assert_eq!(up.events, vec![k(0x04, false, 0)]);
        assert!(c.is_idle());
    }

    #[test]
    fn modifiers_become_left_or_right_keys_from_the_probe_and_carry_the_byte_after_each() {
        let mut c = core();
        // AppKit says "Shift"; the probe says the right key is down.
        let right_shift = |kvk: u16| kvk == 0x3C;
        let out = c.modifiers_changed(Modifiers::SHIFT, &right_shift);
        assert_eq!(out.events, vec![k(0xE5, true, modifier::RSHIFT)]);
        // Both Cmd keys, then Option the probe cannot see (it stands for the left key).
        let both_cmd = |kvk: u16| kvk == 0x37 || kvk == 0x36 || kvk == 0x3C;
        let out = c.modifiers_changed(Modifiers::SHIFT.union(CMD).union(Modifiers::OPTION), &both_cmd);
        assert_eq!(
            out.events,
            vec![
                k(0xE2, true, modifier::RSHIFT | modifier::LALT),
                k(0xE3, true, modifier::RSHIFT | modifier::LALT | modifier::LGUI),
                k(0xE7, true, modifier::RSHIFT | modifier::LALT | modifier::LGUI | modifier::RGUI),
            ]
        );
        // Everything released.
        let out = c.modifiers_changed(Modifiers::default(), &none);
        assert_eq!(out.events.len(), 4);
        assert_eq!(c.modifiers(), 0);
        assert!(out.events.iter().all(|e| matches!(e, InputEvent::Key { down: false, .. })));
        assert_eq!(c.modifiers_changed(Modifiers::default(), &none).events, vec![], "no change, no record");
    }

    #[test]
    fn caps_lock_is_one_press_per_toggle_with_the_new_state_on_it() {
        let mut c = core();
        let on = c.modifiers_changed(Modifiers::CAPS_LOCK, &none);
        let caps = |down| InputEvent::Key { usage_page: 7, usage: 0x39, down, modifiers: 0, locks: 0b011 };
        assert_eq!(on.events, vec![caps(true), caps(false)]);
        // Later records keep the Caps bit.
        let a = c.key(&key(0x00, true, Modifiers::CAPS_LOCK, Some("A")), &none);
        assert_eq!(
            a.events,
            vec![InputEvent::Key { usage_page: 7, usage: 0x04, down: true, modifiers: 0, locks: 0b011 }]
        );
        let off = c.modifiers_changed(Modifiers::default(), &none);
        let caps_off = |down| InputEvent::Key { usage_page: 7, usage: 0x39, down, modifiers: 0, locks: 0b010 };
        assert_eq!(off.events, vec![caps_off(true), caps_off(false)]);
    }

    #[test]
    fn a_key_that_arrives_with_modifiers_the_view_missed_brings_them_first() {
        let mut c = core();
        let probe = |kvk: u16| kvk == 0x3B;
        let out = c.key(&key(0x08, true, Modifiers::CONTROL, Some("\u{3}")), &probe);
        assert_eq!(out.events, vec![k(0xE0, true, modifier::LCTRL), k(0x06, true, modifier::LCTRL)]);
    }

    #[test]
    fn repeats_go_again_and_the_release_of_an_untaken_press_is_not_ours() {
        let mut c = core();
        let msg = KeyMsg { repeat: true, ..key(0x00, true, Modifiers::default(), Some("a")) };
        assert_eq!(c.key(&msg, &none).events, vec![k(0x04, true, 0)]);
        assert_eq!(c.key(&msg, &none).events, vec![k(0x04, true, 0)]);
        // Cmd+Q is the viewer's own: neither the press nor its release is taken.
        assert_eq!(c.key(&key(0x0C, true, CMD, Some("q")), &none), Outcome::passed());
        assert_eq!(c.key(&key(0x0C, false, CMD, None), &none), Outcome::passed());
        // An orphan release is not ours either.
        assert_eq!(c.key(&key(0x01, false, Modifiers::default(), None), &none), Outcome::passed());
    }

    #[test]
    fn a_key_the_tables_do_not_know_is_taken_and_sends_nothing() {
        let mut c = core();
        let out = c.key(&key(0xA0, true, Modifiers::default(), None), &none);
        assert_eq!(out, Outcome { events: vec![], taken: true });
        let up = c.key(&key(0xA0, false, Modifiers::default(), None), &none);
        assert_eq!(up, Outcome { events: vec![], taken: true });
    }

    #[test]
    fn the_iso_swap_applies_to_the_viewers_own_keyboard() {
        let mut c = InputCore::new(InputConfig { keyboard: KbKind::Iso, ..InputConfig::default() });
        // The key left of 1 on an ISO Mac reports 0x0A and is the same HID key as ANSI's 0x32.
        assert_eq!(c.key(&key(0x0A, true, Modifiers::default(), Some("§")), &none).events, vec![k(0x35, true, 0)]);
        let mut a = core();
        assert_eq!(a.key(&key(0x32, true, Modifiers::default(), Some("`")), &none).events, vec![k(0x35, true, 0)]);
    }

    #[test]
    fn text_mode_sends_unicode_and_keeps_shortcuts_and_special_keys_physical() {
        let mut c =
            InputCore::new(InputConfig { mode: KeyboardMode::Text, layouts_match: false, ..InputConfig::default() });
        let out = c.key(&key(0x00, true, Modifiers::default(), Some("ç")), &none);
        assert_eq!(out.events, vec![InputEvent::Text("ç".into())]);
        // Its release has no record.
        assert_eq!(
            c.key(&key(0x00, false, Modifiers::default(), None), &none),
            Outcome { events: vec![], taken: true }
        );
        // The arrow key's characters are private-use: a key, not text.
        let up = c.key(&key(0x7E, true, Modifiers::FUNCTION, Some("\u{F700}")), &none);
        assert_eq!(up.events, vec![k(0x52, true, 0)]);
        // Shortcuts stay physical even with text on the key.
        let probe = |kvk: u16| kvk == 0x37;
        let copy = c.key(&key(0x08, true, CMD, Some("c")), &probe);
        assert_eq!(copy.events, vec![k(0xE3, true, modifier::LGUI), k(0x06, true, modifier::LGUI)]);
        // Right Option alone is AltGr-like: text (Cmd is let go first).
        c.modifiers_changed(Modifiers::default(), &none);
        let ropt = |kvk: u16| kvk == 0x3D;
        let at = c.key(&key(0x13, true, Modifiers::OPTION, Some("@")), &ropt);
        assert_eq!(at.events, vec![k(0xE6, true, modifier::RALT), InputEvent::Text("@".into())]);
    }

    #[test]
    fn physical_mode_ignores_the_text() {
        let mut c = core();
        let out = c.key(&key(0x00, true, Modifiers::default(), Some("ç")), &none);
        assert_eq!(out.events, vec![k(0x04, true, 0)]);
    }

    #[test]
    fn ctrl_and_cmd_swap_exchanges_both_the_keys_and_the_byte() {
        let mut c = InputCore::new(InputConfig { swap_ctrl_cmd: true, ..InputConfig::default() });
        let cmd = |kvk: u16| kvk == 0x37;
        let out = c.modifiers_changed(CMD, &cmd);
        // The viewer's left Cmd is sent as left Ctrl, and the byte follows.
        assert_eq!(out.events, vec![k(0xE0, true, modifier::LCTRL)]);
        let copy = c.key(&key(0x08, true, CMD, Some("c")), &cmd);
        assert_eq!(copy.events, vec![k(0x06, true, modifier::LCTRL)]);
        // Mac Ctrl becomes Win.
        let ctrl = |kvk: u16| kvk == 0x3B;
        let mut d = InputCore::new(InputConfig { swap_ctrl_cmd: true, ..InputConfig::default() });
        assert_eq!(d.modifiers_changed(Modifiers::CONTROL, &ctrl).events, vec![k(0xE3, true, modifier::LGUI)]);
    }

    #[test]
    fn the_num_lock_bit_follows_the_config() {
        let mut c = InputCore::new(InputConfig { num_lock: false, ..InputConfig::default() });
        let out = c.key(&key(0x00, true, Modifiers::default(), None), &none);
        assert_eq!(
            out.events,
            vec![InputEvent::Key { usage_page: 7, usage: 0x04, down: true, modifiers: 0, locks: 0 }]
        );
    }

    // ----- focus

    #[test]
    fn focus_loss_releases_every_key_and_button_and_forgets_them() {
        let mut c = core();
        let probe = |kvk: u16| kvk == 0x37;
        c.modifiers_changed(CMD, &probe);
        c.key(&key(0x00, true, CMD, Some("a")), &probe);
        c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: true }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert!(!c.is_idle());
        assert_eq!(c.release_all(), vec![InputEvent::ReleaseAll]);
        assert!(c.is_idle());
        assert_eq!(c.modifiers(), 0);
        // The releases that follow in AppKit (the key up, the button up) are not ours any more.
        assert_eq!(c.key(&key(0x00, false, Modifiers::default(), None), &none), Outcome::passed());
        let up = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: false }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(up.events, vec![]);
        // The pointer is sent again at the next move.
        let mv = c.mouse(mouse(MouseKind::Move, 100.0, 100.0), Some(&identity_map), 2.0);
        assert_eq!(mv.events.len(), 1);
    }

    #[test]
    fn focus_gain_sends_the_modifiers_already_held_and_adopts_caps_without_a_press() {
        let mut c = core();
        let shift = |kvk: u16| kvk == 0x38;
        let events = c.focus_gained(Modifiers::SHIFT.union(Modifiers::CAPS_LOCK), &shift);
        assert_eq!(
            events,
            vec![InputEvent::Key { usage_page: 7, usage: 0xE1, down: true, modifiers: modifier::LSHIFT, locks: 0b011 }]
        );
        // Caps is already counted: no toggle when the next flagsChanged still has it on.
        assert_eq!(c.modifiers_changed(Modifiers::SHIFT.union(Modifiers::CAPS_LOCK), &shift).events, vec![]);
    }

    // ----- system shortcuts through the tap

    #[test]
    fn the_tap_swallows_cmd_tab_and_sends_it_with_the_command_key_held() {
        let mut c = core();
        let cmd = |kvk: u16| kvk == 0x37;
        let down = c.tap_key(KVK_TAB, true, CMD, &cmd);
        assert!(down.taken);
        assert_eq!(down.events, vec![k(0xE3, true, modifier::LGUI), k(0x2B, true, modifier::LGUI)]);
        // The user lets go of Cmd first: the view still gets that flagsChanged and sends the Cmd release.
        let out = c.modifiers_changed(Modifiers::default(), &none);
        assert_eq!(out.events, vec![k(0xE3, false, 0)]);
        // The release of Tab is swallowed too, and sent, though no modifier is held any more.
        let up = c.tap_key(KVK_TAB, false, Modifiers::default(), &none);
        assert!(up.taken);
        assert_eq!(up.events, vec![k(0x2B, false, 0)]);
        assert!(c.is_idle());
    }

    #[test]
    fn the_tap_leaves_everything_else_to_the_view() {
        let mut c = core();
        assert_eq!(c.tap_key(0x00, true, Modifiers::default(), &none), Outcome::passed());
        assert_eq!(c.tap_key(KVK_TAB, true, Modifiers::default(), &none), Outcome::passed());
        assert_eq!(c.tap_key(0x08, true, CMD, &none), Outcome::passed(), "Cmd+C goes through the view");
        assert_eq!(c.tap_key(KVK_SPACE, false, CMD, &none), Outcome::passed(), "a release it never swallowed");
        assert!(c.is_idle());
    }

    #[test]
    fn turning_system_keys_off_under_a_swallowed_key_releases_everything() {
        let mut c = core();
        let cmd = |kvk: u16| kvk == 0x37;
        assert!(!c.system_keys_off());
        c.tap_key(KVK_SPACE, true, CMD, &cmd);
        assert!(c.system_keys_off());
        assert!(c.is_idle());
        assert!(!c.system_keys_off());
    }

    // ----- pointer

    #[test]
    fn moves_send_the_mapped_position_once_and_skip_the_letterbox() {
        let mut c = core();
        let out = c.mouse(mouse(MouseKind::Move, 0.0, 0.0), Some(&identity_map), 2.0);
        assert_eq!(out.events, vec![InputEvent::Pointer { display: 0, x: 0, y: 0 }]);
        let same = c.mouse(mouse(MouseKind::Move, 0.1, 0.0), Some(&identity_map), 2.0);
        assert_eq!(same.events, vec![], "the same pixel is not sent twice");
        let far =
            c.mouse(mouse(MouseKind::Drag { button: PointerButton::Left }, 959.5, 539.5), Some(&identity_map), 2.0);
        assert_eq!(far.events, vec![InputEvent::Pointer { display: 0, x: 65535, y: 65535 }]);
        let out_of_picture = c.mouse(mouse(MouseKind::Move, 2000.0, 10.0), Some(&identity_map), 2.0);
        assert!(out_of_picture.taken);
        assert_eq!(out_of_picture.events, vec![]);
        let no_map = c.mouse(mouse(MouseKind::Move, 5.0, 5.0), None, 2.0);
        assert_eq!(no_map.events, vec![]);
    }

    #[test]
    fn the_display_id_rides_on_every_position() {
        let mut c = InputCore::new(InputConfig { display: 3, ..InputConfig::default() });
        let out = c.mouse(mouse(MouseKind::Move, 10.0, 10.0), Some(&identity_map), 1.0);
        assert!(matches!(out.events[0], InputEvent::Pointer { display: 3, .. }));
    }

    #[test]
    fn buttons_click_at_the_pointer_and_a_press_outside_the_picture_is_ignored_with_its_release() {
        let mut c = core();
        let down = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Right, down: true }, 480.0, 270.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(down.events.len(), 2);
        assert!(matches!(down.events[0], InputEvent::Pointer { .. }));
        assert_eq!(down.events[1], InputEvent::Button { button: MouseButton::Right, down: true });
        let up = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Right, down: false }, 480.0, 270.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(up.events, vec![InputEvent::Button { button: MouseButton::Right, down: false }]);
        // In the letterbox: nothing goes, neither the press nor its release.
        let out_down = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: true }, 5000.0, 0.0),
            Some(&identity_map),
            2.0,
        );
        assert!(out_down.taken && out_down.events.is_empty());
        let out_up = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: false }, 480.0, 270.0),
            Some(&identity_map),
            2.0,
        );
        assert!(out_up.events.is_empty());
    }

    #[test]
    fn a_release_after_the_pointer_left_the_picture_still_goes() {
        let mut c = core();
        c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: true }, 480.0, 270.0),
            Some(&identity_map),
            2.0,
        );
        let up = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: false }, 5000.0, 270.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(up.events, vec![InputEvent::Button { button: MouseButton::Left, down: false }]);
    }

    #[test]
    fn buttons_map_to_the_wire_numbers() {
        let cases = [
            (PointerButton::Left, Some(MouseButton::Left)),
            (PointerButton::Right, Some(MouseButton::Right)),
            (PointerButton::Middle, Some(MouseButton::Middle)),
            (PointerButton::Other(3), Some(MouseButton::Back)),
            (PointerButton::Other(4), Some(MouseButton::Forward)),
            (PointerButton::Other(5), None),
        ];
        for (b, wire) in cases {
            assert_eq!(wire_button(b), wire);
        }
    }

    #[test]
    fn a_lost_position_is_sent_again_by_the_next_click() {
        let mut c = core();
        c.mouse(mouse(MouseKind::Move, 100.0, 100.0), Some(&identity_map), 2.0);
        c.forget_pointer();
        let down = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: true }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert!(matches!(down.events[0], InputEvent::Pointer { .. }), "the click carries its position");
    }

    // ----- wheel

    #[test]
    fn a_mouse_wheel_gives_lines_and_a_trackpad_gives_pixels() {
        let mut c = core();
        // One line of wheel = one notch = 120 units; positive dy keeps its sign, dx flips.
        let out = c.mouse(
            mouse(MouseKind::Scroll { dx: 0.0, dy: 1.0, precise: false }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(out.events[1], InputEvent::Wheel { dx: 0, dy: 120 });
        let out = c.mouse(
            mouse(MouseKind::Scroll { dx: 2.0, dy: -3.0, precise: false }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(out.events, vec![InputEvent::Wheel { dx: -240, dy: -360 }]);
        // Trackpad: 40 points make a notch, so 10 points are 30 units.
        let out = c.mouse(
            mouse(MouseKind::Scroll { dx: 0.0, dy: 10.0, precise: true }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(out.events, vec![InputEvent::Wheel { dx: 0, dy: 30 }]);
    }

    #[test]
    fn small_trackpad_deltas_add_up_and_a_turn_around_drops_the_remainder() {
        let scroll = |c: &mut InputCore, dy: f64| {
            c.mouse(mouse(MouseKind::Scroll { dx: 0.0, dy, precise: true }, 100.0, 100.0), Some(&identity_map), 2.0)
        };
        // 0.1 point = 0.3 unit: nothing for three events, the fourth crosses one whole unit.
        let mut c = core();
        for _ in 0..3 {
            assert_eq!(scroll(&mut c, 0.1).events, vec![]);
        }
        let fourth = scroll(&mut c, 0.1);
        assert!(fourth.events.contains(&InputEvent::Wheel { dx: 0, dy: 1 }), "{:?}", fourth.events);
        // 0.3 unit is left over; a move the other way must not cancel against it: -1.5 units are one whole unit.
        let mut d = core();
        assert!(scroll(&mut d, 0.1).events.is_empty());
        let back = scroll(&mut d, -0.5);
        assert!(back.events.contains(&InputEvent::Wheel { dx: 0, dy: -1 }), "{:?}", back.events);
    }

    #[test]
    fn scrolling_outside_the_picture_sends_nothing() {
        let mut c = core();
        let out = c.mouse(
            mouse(MouseKind::Scroll { dx: 0.0, dy: 5.0, precise: false }, 5000.0, 0.0),
            Some(&identity_map),
            2.0,
        );
        assert!(out.taken && out.events.is_empty());
    }

    #[test]
    fn nonsense_coordinates_and_scales_do_not_panic() {
        let mut c = core();
        for (x, y, s) in
            [(f64::NAN, 1.0, 2.0), (1e300, 1.0, 1.0), (10.0, 10.0, 0.0), (10.0, 10.0, f64::NAN), (-5.0, -5.0, 2.0)]
        {
            let _ = c.mouse(mouse(MouseKind::Move, x, y), Some(&identity_map), s);
        }
        let _ = c.mouse(
            mouse(MouseKind::Scroll { dx: f64::NAN, dy: f64::INFINITY, precise: true }, 1.0, 1.0),
            Some(&identity_map),
            1.0,
        );
    }
}
