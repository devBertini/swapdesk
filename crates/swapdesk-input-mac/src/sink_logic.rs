//! What the Mac sink does with a wire `InputEvent`, as plain data: the pure half of [`crate::MacInputSink`] (no OS call,
//! so it is unit tested on every host). The planner decides which CG events a record becomes, where the pointer goes,
//! click counts, the set of pressed keys and buttons (and the order they are released in) and the key mapping; the
//! macOS part only turns each [`Planned`] event into a `CGEvent` and posts it.
//!
//! Semantics mirror `swapdesk-input-win`'s sink: modifiers are synced before a key down from the record's HID modifier
//! byte (pressing or releasing the modifier keys, tracked as held), a key repeated while held is an auto-repeat,
//! `ReleaseAll` lifts every key and button this sink pressed, and an unknown key or display is `InvalidInput` that
//! changes nothing. Differences, on purpose: the lock keys (Caps/Num/Scroll bits of `locks`) are not synced (macOS has
//! no way to read the toggle without a tap, and the Mac's own Caps Lock stays the user's); consumer-page media keys
//! without a `kVK_*` code in the C4 table (play/pause, next, previous: they are `NX_SYSDEFINED` events) are
//! `InvalidInput`, and volume and mute go as their `kVK_*` keys, which UNPROVEN on hardware may do nothing.
//!
//! Coordinates: `DisplayInfo.bounds` from the Mac screen source are physical pixels (the point origin times the
//! display's scale), so a wire point goes to a pixel of the display (the protocol's rounding, PROTOCOL.md 7.3) and
//! that pixel divided by the display's scale is a global point (top-left origin, as `CGEventPost` takes).
//!
//! Wheel: the wire is 1/120 notch; the sink posts pixel units, [`PIXELS_PER_NOTCH`] per notch (the viewer's default
//! trackpad figure), carrying the fraction. UNPROVEN on hardware: the signs (`dy > 0` is `wheel1 > 0`; `dx > 0` is
//! `wheel2 < 0`, the inverse of the viewer's AppKit conversion); an interactive probe settles it.

use crate::keymap::kvk_of_hid;
use swapdesk_keys::{KbKind, PAGE_KEYBOARD, canonical, modifier};
use swapdesk_platform::{DisplayInfo, ErrorKind, PlatformError, Result};
use swapdesk_proto::{InputEvent, MouseButton};

/// `CGEventFlags` bits (`CoreGraphics/CGEventTypes.h`), as plain numbers so the planner needs no macOS type.
pub mod flag {
    pub const SHIFT: u64 = 0x0002_0000;
    pub const CONTROL: u64 = 0x0004_0000;
    pub const ALTERNATE: u64 = 0x0008_0000;
    pub const COMMAND: u64 = 0x0010_0000;
    pub const NUMERIC_PAD: u64 = 0x0020_0000;
    pub const SECONDARY_FN: u64 = 0x0080_0000;
}

/// Wire wheel units are 1/120 notch; one notch is this many pixels of scroll.
pub const PIXELS_PER_NOTCH: i32 = 40;

/// Clicks closer than this many points to the previous one may continue a multi-click.
pub const DOUBLE_CLICK_RADIUS: f64 = 4.0;

const KVK_RETURN: u16 = 0x24;
const KVK_TAB: u16 = 0x30;

/// The mouse button of a CG mouse event (`CGMouseButton` numbers: 0 left, 1 right, 2 center, 3 and 4 the extra ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseBtn {
    Left,
    Right,
    /// `kCGMouseButtonCenter` is 2; the back and forward buttons are 3 and 4.
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Moved,
    Down,
    Up,
    /// A move while `button` is down (`kCGEventLeftMouseDragged` and the like).
    Dragged,
}

/// One CG event to build and post, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Planned {
    /// A keyboard event (`modifier`: the key is a modifier, posted as a flags-changed event). `flags` is the full
    /// modifier state the event carries; `repeat` marks an auto-repeat.
    Key { kvk: u16, down: bool, flags: u64, modifier: bool, repeat: bool },
    /// One typed character: a key down and a key up carrying these UTF-16 units.
    Text { units: Vec<u16> },
    /// A mouse event at `at` (global points), or at the current pointer when `None` (nothing placed it yet).
    Mouse { kind: MouseKind, button: MouseBtn, at: Option<(f64, f64)>, click_state: i64, flags: u64 },
    /// A scroll in pixel units: `vertical` is `wheel1` (positive up), `horizontal` is `wheel2`.
    Scroll { vertical: i32, horizontal: i32, flags: u64 },
}

fn invalid(detail: String) -> PlatformError {
    PlatformError { kind: ErrorKind::InvalidInput, op: "InputSink.apply", os_code: None, detail }
}

fn button_of(b: MouseButton) -> MouseBtn {
    match b {
        MouseButton::Left => MouseBtn::Left,
        MouseButton::Right => MouseBtn::Right,
        MouseButton::Middle => MouseBtn::Other(2),
        MouseButton::Back => MouseBtn::Other(3),
        MouseButton::Forward => MouseBtn::Other(4),
    }
}

/// The CG flag a HID modifier bit sets (both sides of a key share one flag).
fn flag_of_bit(bit: u8) -> u64 {
    match bit {
        modifier::LSHIFT | modifier::RSHIFT => flag::SHIFT,
        modifier::LCTRL | modifier::RCTRL => flag::CONTROL,
        modifier::LALT | modifier::RALT => flag::ALTERNATE,
        _ => flag::COMMAND,
    }
}

/// The flags a real keyboard adds to some keys: the keypad sets the numeric-pad flag, the arrows and the navigation
/// and function keys set the Fn flag as well (`kCGEventFlagMaskNumericPad`, `kCGEventFlagMaskSecondaryFn`).
fn key_extra_flags(page: u16, usage: u16) -> u64 {
    if page != PAGE_KEYBOARD {
        return 0;
    }
    match usage {
        0x4F..=0x52 => flag::NUMERIC_PAD | flag::SECONDARY_FN, // arrows
        0x53..=0x63 | 0x67 => flag::NUMERIC_PAD,               // keypad
        0x49..=0x4E | 0x3A..=0x45 | 0x68..=0x73 => flag::SECONDARY_FN, // insert..page down, F1..F12, F13..F24
        _ => 0,
    }
}

/// The planner. One per sink; `now_ms` is any monotonic clock in milliseconds.
pub struct SinkPlanner {
    keyboard: KbKind,
    /// The system double-click interval in milliseconds.
    double_click_ms: u64,
    displays: Vec<DisplayInfo>,
    /// Keys held, in the order they went down: `(usage page, usage, kvk)`.
    held_keys: Vec<(u16, u16, u16)>,
    /// Buttons held, in the order they went down, with the click count of their down event.
    held_buttons: Vec<(MouseButton, i64)>,
    /// Where the pointer was last placed, in global points.
    pos: Option<(f64, f64)>,
    last_click: Option<LastClick>,
    /// Wheel units (1/120 notch) not yet turned into whole pixels, x then y.
    wheel_rest: (i32, i32),
}

#[derive(Clone, Copy)]
struct LastClick {
    button: MouseButton,
    at_ms: u64,
    pos: Option<(f64, f64)>,
    count: i64,
}

impl SinkPlanner {
    pub fn new(keyboard: KbKind, double_click_ms: u64) -> Self {
        Self {
            keyboard,
            double_click_ms,
            displays: Vec::new(),
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
            pos: None,
            last_click: None,
            wheel_rest: (0, 0),
        }
    }

    pub fn displays_changed(&mut self, displays: &[DisplayInfo]) {
        self.displays = displays.to_vec();
    }

    /// Keys held by this sink as `(usage_page, usage)`, in the order pressed.
    pub fn held_keys(&self) -> Vec<(u16, u16)> {
        self.held_keys.iter().map(|k| (k.0, k.1)).collect()
    }

    pub fn held_buttons(&self) -> Vec<MouseButton> {
        self.held_buttons.iter().map(|b| b.0).collect()
    }

    pub fn is_idle(&self) -> bool {
        self.held_keys.is_empty() && self.held_buttons.is_empty()
    }

    /// The modifier flags of the modifier keys held now.
    fn flags(&self) -> u64 {
        let mut f = 0;
        for (page, usage, _) in &self.held_keys {
            if let Some(bit) = modifier::bit_of(*page, *usage) {
                f |= flag_of_bit(bit);
            }
        }
        f
    }

    /// The events for one record. State changes with the plan (what is held after it), also when the OS later fails
    /// to post: a key counted as held that never went down is only lifted again by `ReleaseAll`, which is harmless.
    pub fn plan(&mut self, event: &InputEvent, now_ms: u64) -> Result<Vec<Planned>> {
        match event {
            InputEvent::Key { usage_page, usage, down, modifiers, .. } => {
                self.plan_key(*usage_page, *usage, *down, *modifiers)
            }
            InputEvent::Text(text) => Ok(plan_text(text)),
            InputEvent::Pointer { display, x, y } => self.plan_pointer(*display, *x, *y),
            InputEvent::Button { button, down } => Ok(self.plan_button(*button, *down, now_ms)),
            InputEvent::Wheel { dx, dy } => Ok(self.plan_wheel(*dx, *dy)),
            InputEvent::ReleaseAll => Ok(self.release_all()),
        }
    }

    fn kvk(&self, page: u16, usage: u16) -> Result<u16> {
        kvk_of_hid(page, usage, self.keyboard)
            .ok_or_else(|| invalid(format!("no macOS key for usage {page:#x}/{usage:#x}")))
    }

    fn plan_key(&mut self, page: u16, usage: u16, down: bool, modifiers: u8) -> Result<Vec<Planned>> {
        let (page, usage) = canonical(page, usage);
        let kvk = self.kvk(page, usage)?;
        let own_bit = modifier::bit_of(page, usage);
        let mut out = Vec::new();
        if down {
            // Modifiers first: the record carries the viewer's state after the event (PROTOCOL.md 7.3). A modifier
            // key's own record is applied as it is, so its bit is not synced here.
            for i in 0..8u8 {
                let bit = 1u8 << i;
                if Some(bit) == own_bit {
                    continue;
                }
                let m_usage = 0xE0 + u16::from(i);
                let held = self.held_keys.iter().any(|k| (k.0, k.1) == (PAGE_KEYBOARD, m_usage));
                let wanted = modifiers & bit != 0;
                if wanted == held {
                    continue;
                }
                let m_kvk = self.kvk(PAGE_KEYBOARD, m_usage)?;
                if wanted {
                    self.held_keys.push((PAGE_KEYBOARD, m_usage, m_kvk));
                } else {
                    self.held_keys.retain(|k| (k.0, k.1) != (PAGE_KEYBOARD, m_usage));
                }
                out.push(Planned::Key { kvk: m_kvk, down: wanted, flags: self.flags(), modifier: true, repeat: false });
            }
        }
        let was_held = self.held_keys.iter().any(|k| (k.0, k.1) == (page, usage));
        if down {
            if !was_held {
                self.held_keys.push((page, usage, kvk));
            }
        } else {
            self.held_keys.retain(|k| (k.0, k.1) != (page, usage));
        }
        let extra = if own_bit.is_some() { 0 } else { key_extra_flags(page, usage) };
        out.push(Planned::Key {
            kvk,
            down,
            flags: self.flags() | extra,
            modifier: own_bit.is_some(),
            repeat: down && was_held,
        });
        Ok(out)
    }

    /// The global point of a wire point on a display.
    fn point_of(&self, display: u16, x: u16, y: u16) -> Result<(f64, f64)> {
        let d = self
            .displays
            .iter()
            .find(|d| d.id.0 == display)
            .ok_or_else(|| invalid(format!("unknown display {display}")))?;
        let px = i64::from(d.bounds.x) + scale_axis(x, d.bounds.width);
        let py = i64::from(d.bounds.y) + scale_axis(y, d.bounds.height);
        let scale = if d.scale_milli == 0 { 1.0 } else { f64::from(d.scale_milli) / 1000.0 };
        Ok((px as f64 / scale, py as f64 / scale))
    }

    fn plan_pointer(&mut self, display: u16, x: u16, y: u16) -> Result<Vec<Planned>> {
        let at = self.point_of(display, x, y)?;
        self.pos = Some(at);
        let (kind, button) = match self.held_buttons.last() {
            Some((b, _)) => (MouseKind::Dragged, button_of(*b)),
            None => (MouseKind::Moved, MouseBtn::Left),
        };
        let click_state = self.held_buttons.last().map_or(0, |b| b.1);
        Ok(vec![Planned::Mouse { kind, button, at: Some(at), click_state, flags: self.flags() }])
    }

    fn plan_button(&mut self, button: MouseButton, down: bool, now_ms: u64) -> Vec<Planned> {
        let flags = self.flags();
        let click_state = if down {
            if let Some((_, count)) = self.held_buttons.iter().find(|b| b.0 == button) {
                *count
            } else {
                let count = match self.last_click {
                    Some(c)
                        if c.button == button
                            && now_ms.saturating_sub(c.at_ms) <= self.double_click_ms
                            && near(c.pos, self.pos) =>
                    {
                        c.count + 1
                    }
                    _ => 1,
                };
                self.last_click = Some(LastClick { button, at_ms: now_ms, pos: self.pos, count });
                self.held_buttons.push((button, count));
                count
            }
        } else {
            let count = self.held_buttons.iter().find(|b| b.0 == button).map_or(1, |b| b.1);
            self.held_buttons.retain(|b| b.0 != button);
            count
        };
        vec![Planned::Mouse {
            kind: if down { MouseKind::Down } else { MouseKind::Up },
            button: button_of(button),
            at: self.pos,
            click_state,
            flags,
        }]
    }

    fn plan_wheel(&mut self, dx: i16, dy: i16) -> Vec<Planned> {
        let vertical = take_pixels(&mut self.wheel_rest.1, i32::from(dy));
        // `dx > 0` is right, which `wheel2` spells as a negative number (see the module doc: unproven).
        let horizontal = -take_pixels(&mut self.wheel_rest.0, i32::from(dx));
        if vertical == 0 && horizontal == 0 {
            return Vec::new();
        }
        vec![Planned::Scroll { vertical, horizontal, flags: self.flags() }]
    }

    /// Every key up (last pressed first), then every button up (last pressed first), and the tracking forgotten.
    pub fn release_all(&mut self) -> Vec<Planned> {
        let mut out = Vec::new();
        let keys = std::mem::take(&mut self.held_keys);
        let mut remaining = keys.clone();
        for (page, usage, kvk) in keys.iter().rev() {
            remaining.retain(|k| (k.0, k.1) != (*page, *usage));
            let is_mod = modifier::bit_of(*page, *usage).is_some();
            let mut f = 0;
            for (p, u, _) in &remaining {
                if let Some(bit) = modifier::bit_of(*p, *u) {
                    f |= flag_of_bit(bit);
                }
            }
            out.push(Planned::Key { kvk: *kvk, down: false, flags: f, modifier: is_mod, repeat: false });
        }
        let buttons = std::mem::take(&mut self.held_buttons);
        for (button, count) in buttons.iter().rev() {
            out.push(Planned::Mouse {
                kind: MouseKind::Up,
                button: button_of(*button),
                at: self.pos,
                click_state: *count,
                flags: 0,
            });
        }
        self.wheel_rest = (0, 0);
        out
    }
}

/// `round(v * (size - 1) / 65535)`, the protocol's rounding (PROTOCOL.md 7.3).
fn scale_axis(v: u16, size: u32) -> i64 {
    let span = i64::from(size.saturating_sub(1));
    (i64::from(v) * span * 2 + 65535) / (2 * 65535)
}

fn near(a: Option<(f64, f64)>, b: Option<(f64, f64)>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a.0 - b.0).abs() <= DOUBLE_CLICK_RADIUS && (a.1 - b.1).abs() <= DOUBLE_CLICK_RADIUS,
        _ => true,
    }
}

/// Adds `units` (1/120 notch) to the carried rest and takes the whole pixels out; a turn around drops the old rest.
fn take_pixels(rest: &mut i32, units: i32) -> i32 {
    if units != 0 && *rest != 0 && (*rest > 0) != (units > 0) {
        *rest = 0;
    }
    let total = *rest + units;
    let per = 120 / PIXELS_PER_NOTCH; // wire units per pixel
    let pixels = total / per;
    *rest = total % per;
    pixels
}

/// The events that type `text`: one `Text` per character (a character outside the BMP is its surrogate pair in one
/// event), line breaks and tabs as the Return and Tab keys, other control characters skipped.
pub fn plan_text(text: &str) -> Vec<Planned> {
    let mut out = Vec::new();
    for ch in text.chars() {
        match ch {
            '\r' | '\n' | '\t' => {
                let kvk = if ch == '\t' { KVK_TAB } else { KVK_RETURN };
                for down in [true, false] {
                    out.push(Planned::Key { kvk, down, flags: 0, modifier: false, repeat: false });
                }
            }
            c if c.is_control() => {}
            c => {
                let mut buf = [0u16; 2];
                out.push(Planned::Text { units: c.encode_utf16(&mut buf).to_vec() });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_platform::{AdapterId, DisplayId, Rect};

    fn planner() -> SinkPlanner {
        SinkPlanner::new(KbKind::Ansi, 500)
    }

    fn key(usage: u16, down: bool, mods: u8) -> InputEvent {
        InputEvent::Key { usage_page: 7, usage, down, modifiers: mods, locks: 0 }
    }

    fn display(id: u16, bounds: Rect, scale_milli: u16) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: String::new(),
            bounds,
            scale_milli,
            refresh_mhz: 60_000,
            rotation: 0,
            primary: id == 1,
            adapter: AdapterId(0),
        }
    }

    fn k(kvk: u16, down: bool, flags: u64) -> Planned {
        Planned::Key { kvk, down, flags, modifier: false, repeat: false }
    }

    fn m(kvk: u16, down: bool, flags: u64) -> Planned {
        Planned::Key { kvk, down, flags, modifier: true, repeat: false }
    }

    #[test]
    fn a_plain_key_goes_down_and_up_by_its_mac_code() {
        let mut p = planner();
        // HID 0x04 (A) is kVK_ANSI_A = 0x00; 0x28 (Enter) is kVK_Return = 0x24.
        assert_eq!(p.plan(&key(0x04, true, 0), 0).unwrap(), vec![k(0x00, true, 0)]);
        assert_eq!(p.held_keys(), vec![(7, 0x04)]);
        assert_eq!(p.plan(&key(0x04, false, 0), 0).unwrap(), vec![k(0x00, false, 0)]);
        assert!(p.is_idle());
        assert_eq!(p.plan(&key(0x28, true, 0), 0).unwrap(), vec![k(0x24, true, 0)]);
    }

    #[test]
    fn modifiers_are_pressed_before_the_key_and_released_when_the_record_drops_them() {
        let mut p = planner();
        let out = p.plan(&key(0x06, true, modifier::LCTRL | modifier::LSHIFT), 0).unwrap();
        assert_eq!(
            out,
            vec![
                m(0x3B, true, flag::CONTROL),
                m(0x38, true, flag::CONTROL | flag::SHIFT),
                k(0x08, true, flag::CONTROL | flag::SHIFT)
            ],
            "Ctrl, Shift, then C"
        );
        // The viewer released Shift while holding Ctrl and pressed V: Shift goes up first.
        let out = p.plan(&key(0x19, true, modifier::LCTRL), 0).unwrap();
        assert_eq!(out, vec![m(0x38, false, flag::CONTROL), k(0x09, true, flag::CONTROL)]);
        assert_eq!(p.held_keys(), vec![(7, 0xE0), (7, 0x06), (7, 0x19)]);
    }

    #[test]
    fn a_modifier_records_own_bit_is_not_pressed_twice_and_its_flag_follows_it() {
        let mut p = planner();
        assert_eq!(p.plan(&key(0xE1, true, modifier::LSHIFT), 0).unwrap(), vec![m(0x38, true, flag::SHIFT)]);
        assert_eq!(p.plan(&key(0xE1, false, 0), 0).unwrap(), vec![m(0x38, false, 0)]);
        assert!(p.is_idle());
        // Right Cmd is its own key (0x36) with the shared Command flag.
        assert_eq!(p.plan(&key(0xE7, true, modifier::RGUI), 0).unwrap(), vec![m(0x36, true, flag::COMMAND)]);
    }

    #[test]
    fn a_key_down_while_held_is_an_auto_repeat() {
        let mut p = planner();
        p.plan(&key(0x04, true, 0), 0).unwrap();
        let out = p.plan(&key(0x04, true, 0), 0).unwrap();
        assert_eq!(out, vec![Planned::Key { kvk: 0, down: true, flags: 0, modifier: false, repeat: true }]);
        assert_eq!(p.held_keys().len(), 1);
    }

    #[test]
    fn arrows_and_keypad_carry_their_flags_like_a_real_keyboard() {
        let mut p = planner();
        // Up arrow HID 0x52 is kVK_UpArrow 0x7E; keypad 8 is HID 0x60, kVK_ANSI_Keypad8 0x5B; F1 is HID 0x3A, kVK_F1 0x7A.
        assert_eq!(
            p.plan(&key(0x52, true, 0), 0).unwrap(),
            vec![k(0x7E, true, flag::NUMERIC_PAD | flag::SECONDARY_FN)]
        );
        assert_eq!(p.plan(&key(0x60, true, 0), 0).unwrap(), vec![k(0x5B, true, flag::NUMERIC_PAD)]);
        assert_eq!(p.plan(&key(0x3A, true, 0), 0).unwrap(), vec![k(0x7A, true, flag::SECONDARY_FN)]);
    }

    #[test]
    fn the_iso_keyboard_kind_picks_the_iso_section_code() {
        let mut iso = SinkPlanner::new(KbKind::Iso, 500);
        let mut ansi = planner();
        let ev = key(0x35, true, 0); // the key left of 1
        assert_eq!(iso.plan(&ev, 0).unwrap(), vec![k(0x0A, true, 0)]);
        assert_eq!(ansi.plan(&ev, 0).unwrap(), vec![k(0x32, true, 0)]);
    }

    #[test]
    fn an_unknown_usage_or_a_media_key_is_invalid_input_and_changes_nothing() {
        let mut p = planner();
        assert_eq!(p.plan(&key(0xFFFF, true, 0), 0).unwrap_err().kind, ErrorKind::InvalidInput);
        assert!(p.is_idle());
        // Play/Pause has no kVK_* code (the table says `-`); volume and mute have (0x48 to 0x4A).
        let media = InputEvent::Key { usage_page: 0x0C, usage: 0xCD, down: true, modifiers: 0, locks: 0 };
        assert_eq!(p.plan(&media, 0).unwrap_err().kind, ErrorKind::InvalidInput);
        assert!(p.is_idle());
    }

    #[test]
    fn text_is_one_event_per_character_and_line_breaks_are_keys() {
        let t = plan_text("aç\t\u{1F600}\n\u{7}");
        assert_eq!(
            t,
            vec![
                Planned::Text { units: vec![0x61] },
                Planned::Text { units: vec![0xE7] },
                k(0x30, true, 0),
                k(0x30, false, 0),
                Planned::Text { units: vec![0xD83D, 0xDE00] },
                k(0x24, true, 0),
                k(0x24, false, 0),
            ]
        );
        assert!(plan_text("").is_empty());
        let mut p = planner();
        assert_eq!(p.plan(&InputEvent::Text("x".into()), 0).unwrap().len(), 1);
    }

    #[test]
    fn pointer_goes_through_the_display_in_pixels_then_points() {
        let mut p = planner();
        // A Retina primary (scale 2): 3024x1964 pixels, 1512x982 points; a second one to its left at scale 1.
        p.displays_changed(&[
            display(1, Rect { x: 0, y: 0, width: 3024, height: 1964 }, 2000),
            display(2, Rect { x: -1920, y: -100, width: 1920, height: 1080 }, 1000),
        ]);
        let at = |out: Vec<Planned>| match &out[0] {
            Planned::Mouse { at, .. } => at.unwrap(),
            other => panic!("{other:?}"),
        };
        assert_eq!(at(p.plan(&InputEvent::Pointer { display: 1, x: 0, y: 0 }, 0).unwrap()), (0.0, 0.0));
        assert_eq!(at(p.plan(&InputEvent::Pointer { display: 1, x: 65535, y: 65535 }, 0).unwrap()), (1511.5, 981.5));
        assert_eq!(at(p.plan(&InputEvent::Pointer { display: 2, x: 0, y: 0 }, 0).unwrap()), (-1920.0, -100.0));
        assert_eq!(at(p.plan(&InputEvent::Pointer { display: 2, x: 65535, y: 65535 }, 0).unwrap()), (-1.0, 979.0));
        let e = p.plan(&InputEvent::Pointer { display: 9, x: 1, y: 1 }, 0).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        p.displays_changed(&[display(2, Rect { x: 0, y: 0, width: 10, height: 10 }, 1000)]);
        assert!(p.plan(&InputEvent::Pointer { display: 1, x: 0, y: 0 }, 0).is_err(), "the list is replaced");
    }

    fn mouse_kinds(out: &[Planned]) -> Vec<(MouseKind, MouseBtn, i64)> {
        out.iter()
            .map(|p| match p {
                Planned::Mouse { kind, button, click_state, .. } => (*kind, *button, *click_state),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn click(p: &mut SinkPlanner, b: MouseButton, t: u64) -> (i64, i64) {
        let d = mouse_kinds(&p.plan(&InputEvent::Button { button: b, down: true }, t).unwrap())[0].2;
        let u = mouse_kinds(&p.plan(&InputEvent::Button { button: b, down: false }, t + 10).unwrap())[0].2;
        (d, u)
    }

    #[test]
    fn buttons_map_to_the_cg_buttons() {
        let mut p = planner();
        for (b, expect) in [
            (MouseButton::Left, MouseBtn::Left),
            (MouseButton::Right, MouseBtn::Right),
            (MouseButton::Middle, MouseBtn::Other(2)),
            (MouseButton::Back, MouseBtn::Other(3)),
            (MouseButton::Forward, MouseBtn::Other(4)),
        ] {
            let out = p.plan(&InputEvent::Button { button: b, down: true }, 0).unwrap();
            assert_eq!(mouse_kinds(&out), vec![(MouseKind::Down, expect, 1)]);
            p.plan(&InputEvent::Button { button: b, down: false }, 0).unwrap();
            p = planner();
        }
    }

    #[test]
    fn clicks_within_the_interval_and_radius_count_up() {
        let mut p = planner();
        p.displays_changed(&[display(1, Rect { x: 0, y: 0, width: 1000, height: 1000 }, 1000)]);
        p.plan(&InputEvent::Pointer { display: 1, x: 1000, y: 1000 }, 0).unwrap();
        assert_eq!(click(&mut p, MouseButton::Left, 0), (1, 1));
        assert_eq!(click(&mut p, MouseButton::Left, 300), (2, 2), "second click within 500 ms");
        assert_eq!(click(&mut p, MouseButton::Left, 600), (3, 3), "each click is measured from the last");
        assert_eq!(click(&mut p, MouseButton::Left, 1200), (1, 1), "too late starts again");
        assert_eq!(click(&mut p, MouseButton::Right, 1300), (1, 1), "another button starts again");
        assert_eq!(click(&mut p, MouseButton::Right, 1400), (2, 2));
        // Far away: the pointer moved 100 points, so it is a first click.
        p.plan(&InputEvent::Pointer { display: 1, x: 60000, y: 60000 }, 1450).unwrap();
        assert_eq!(click(&mut p, MouseButton::Right, 1500), (1, 1));
    }

    #[test]
    fn a_move_with_a_button_down_is_a_drag_of_that_button() {
        let mut p = planner();
        p.displays_changed(&[display(1, Rect { x: 0, y: 0, width: 100, height: 100 }, 1000)]);
        let mv =
            |p: &mut SinkPlanner| mouse_kinds(&p.plan(&InputEvent::Pointer { display: 1, x: 5, y: 5 }, 0).unwrap());
        assert_eq!(mv(&mut p)[0].0, MouseKind::Moved);
        p.plan(&InputEvent::Button { button: MouseButton::Left, down: true }, 0).unwrap();
        assert_eq!(mv(&mut p), vec![(MouseKind::Dragged, MouseBtn::Left, 1)]);
        p.plan(&InputEvent::Button { button: MouseButton::Back, down: true }, 0).unwrap();
        assert_eq!(mv(&mut p), vec![(MouseKind::Dragged, MouseBtn::Other(3), 1)], "the last pressed one");
        p.plan(&InputEvent::Button { button: MouseButton::Back, down: false }, 0).unwrap();
        p.plan(&InputEvent::Button { button: MouseButton::Left, down: false }, 0).unwrap();
        assert_eq!(mv(&mut p)[0].0, MouseKind::Moved);
    }

    #[test]
    fn a_button_event_is_where_the_pointer_last_was_and_carries_the_held_modifiers() {
        let mut p = planner();
        p.displays_changed(&[display(1, Rect { x: 0, y: 0, width: 101, height: 101 }, 1000)]);
        let none = p.plan(&InputEvent::Button { button: MouseButton::Left, down: true }, 0).unwrap();
        assert!(matches!(none[0], Planned::Mouse { at: None, .. }), "nothing placed the pointer yet");
        p.plan(&InputEvent::Button { button: MouseButton::Left, down: false }, 0).unwrap();
        p.plan(&InputEvent::Pointer { display: 1, x: 65535, y: 0 }, 0).unwrap();
        p.plan(&key(0xE3, true, modifier::LGUI), 0).unwrap();
        let out = p.plan(&InputEvent::Button { button: MouseButton::Left, down: true }, 5000).unwrap();
        assert_eq!(
            out,
            vec![Planned::Mouse {
                kind: MouseKind::Down,
                button: MouseBtn::Left,
                at: Some((100.0, 0.0)),
                click_state: 1,
                flags: flag::COMMAND
            }]
        );
    }

    #[test]
    fn the_wheel_is_pixels_with_the_fraction_carried_and_zero_sends_nothing() {
        let mut p = planner();
        let sc = |out: Vec<Planned>| match out.as_slice() {
            [Planned::Scroll { vertical, horizontal, .. }] => (*vertical, *horizontal),
            [] => (0, 0),
            other => panic!("{other:?}"),
        };
        assert_eq!(sc(p.plan(&InputEvent::Wheel { dx: 0, dy: 0 }, 0).unwrap()), (0, 0));
        assert_eq!(sc(p.plan(&InputEvent::Wheel { dx: 0, dy: 120 }, 0).unwrap()), (40, 0), "one notch up");
        assert_eq!(sc(p.plan(&InputEvent::Wheel { dx: 120, dy: -240 }, 0).unwrap()), (-80, -40));
        // 3 wire units are one pixel; 1 + 1 + 1 adds up to it.
        assert_eq!(sc(p.plan(&InputEvent::Wheel { dx: 0, dy: 1 }, 0).unwrap()), (0, 0));
        assert_eq!(sc(p.plan(&InputEvent::Wheel { dx: 0, dy: 1 }, 0).unwrap()), (0, 0));
        assert_eq!(sc(p.plan(&InputEvent::Wheel { dx: 0, dy: 1 }, 0).unwrap()), (1, 0));
        // A turn around drops the rest instead of cancelling against it.
        p.plan(&InputEvent::Wheel { dx: 0, dy: 2 }, 0).unwrap();
        assert_eq!(sc(p.plan(&InputEvent::Wheel { dx: 0, dy: -3 }, 0).unwrap()), (-1, 0));
        let mut q = planner();
        assert_eq!(sc(q.plan(&InputEvent::Wheel { dx: i16::MIN, dy: i16::MAX }, 0).unwrap()), (10922, 10922));
    }

    #[test]
    fn release_all_lifts_keys_then_buttons_last_pressed_first_and_forgets_them() {
        let mut p = planner();
        p.plan(&key(0xE0, true, modifier::LCTRL), 0).unwrap();
        p.plan(&key(0x04, true, modifier::LCTRL), 0).unwrap();
        p.plan(&key(0x52, true, modifier::LCTRL), 0).unwrap();
        p.plan(&InputEvent::Button { button: MouseButton::Left, down: true }, 0).unwrap();
        p.plan(&InputEvent::Button { button: MouseButton::Right, down: true }, 10_000).unwrap();
        let out = p.plan(&InputEvent::ReleaseAll, 0).unwrap();
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], k(0x7E, false, flag::CONTROL), "arrow, Ctrl still held (the arrow's own flags drop)");
        assert_eq!(out[1], k(0x00, false, flag::CONTROL));
        assert_eq!(out[2], m(0x3B, false, 0));
        assert_eq!(
            mouse_kinds(&out[3..]),
            vec![(MouseKind::Up, MouseBtn::Right, 1), (MouseKind::Up, MouseBtn::Left, 1)]
        );
        assert!(p.is_idle());
        assert!(p.release_all().is_empty(), "nothing held, nothing sent");
    }
}
