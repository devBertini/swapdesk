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
//! - the fractions of a wheel notch not yet sent, so slow trackpad motion adds up instead of vanishing;
//! - the axis of the trackpad gesture in progress, so the fingers' sideways jitter never reaches the host.
//! - in text mode, the Option keys not sent yet (`unsent`). Option is the Mac's character modifier: `Option+C` types
//!   `ç` and `Option+E` is a dead acute on a US-based layout. A Windows host reads a held Alt as the start of a
//!   shortcut (a character typed under it arrives as `WM_SYSCHAR`, a menu mnemonic, not text) and a lone Alt
//!   press and release opens the menu bar. So while text mode is in force an Option key goes to the host only when a
//!   key that goes physically is pressed with it (Option+Left, Option+F4), or as a press and release when it was
//!   tapped alone; text typed with it goes without it, and an Option already sent is let go before the text.
//!
//! Which text a key types comes from a [`Layout`]: on a Mac the system's current keyboard layout, translated with
//! its dead keys (`UCKeyTranslate`, in `platform`); elsewhere and in tests the characters AppKit put on the event.

use std::collections::{BTreeSet, HashSet};

use swapdesk_keys::mode::{KeyInput, KeyRouter, KeyboardMode, Routed};
use swapdesk_keys::shortcuts::ViewerOs;
use swapdesk_keys::swap::{swap_event, swap_modifiers};
use swapdesk_keys::{KbKind, PAGE_KEYBOARD, modifier};
use swapdesk_proto::{InputEvent, MouseButton};
use swapdesk_shell_mac::{KeyMsg, Modifiers, MouseKind, MouseMsg, PointerButton};

use crate::keymap::{aggregate_of_bit, hid_of_kvk, is_local_escape, is_system_shortcut, modifier_kvk, typed_text};

/// Whether a physical key is down right now, by `kVK_*` code (`CGEventSourceKeyState` on a Mac).
pub type KeyProbe<'a> = &'a dyn Fn(u16) -> bool;

/// The HID usage of the left Control key.
const CTRL_USAGE: u16 = 0xE0;
/// The Option keys of the HID modifier byte.
const OPTION_BITS: u8 = modifier::LALT | modifier::RALT;

/// What the viewer's keyboard layout types for one key press.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Typed {
    /// The characters, when the press types any (after a dead key: the composed character, or the accent and the
    /// letter when they do not compose).
    pub text: Option<String>,
    /// The press is a dead key: it types nothing now and the next press carries the accent.
    pub dead: bool,
}

/// A typed character is as private as the text: `Debug` shows its size only (CONTRACTS.md C2.3).
impl core::fmt::Debug for Typed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Typed")
            .field("text", &self.text.as_ref().map(|t| swapdesk_proto::RedactedLen(t.len())))
            .field("dead", &self.dead)
            .finish()
    }
}

/// The viewer's keyboard layout, as the text router asks it (the Mac's counterpart of the Windows hook's
/// `ToUnicodeEx`). It keeps the dead-key state between presses.
pub trait Layout: Send {
    /// What the press `msg` types. Asked only for a press that may go as text (text mode in force, no Control or
    /// Command held), once per press and in order, so a dead key and the key after it compose.
    fn type_key(&mut self, msg: &KeyMsg) -> Typed;
    /// A press went as a key (a shortcut, a key in physical mode), or the focus changed: a pending dead key is dropped.
    fn clear_dead_key(&mut self) {}
}

/// The characters AppKit put on the event (`NSEvent.characters`), with no dead-key knowledge: the layout of tests,
/// of non-macOS builds, and the fallback when the system layout has no Unicode table.
#[derive(Debug, Default, Clone, Copy)]
pub struct EventText;

impl Layout for EventText {
    fn type_key(&mut self, msg: &KeyMsg) -> Typed {
        Typed { text: typed_text(msg.text.as_deref()).map(str::to_owned), dead: false }
    }
}

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

/// With precise (trackpad) deltas, the smaller axis is sent only when it is at least this share of the larger one
/// (about 27 degrees off the axis); below it, it is finger jitter (see `InputCore::on_rail`).
const RAIL_RATIO: f64 = 0.5;
/// Points of motion, on either axis, after which a trackpad gesture's axis is decided.
const RAIL_DECIDE: f64 = 3.0;

/// Wheel units (1/120 notch) per unit of `NSEvent.magnification` when a pinch goes to a Windows host as Ctrl and the
/// wheel: a pinch that doubles the picture (about 1.0 summed) is five notches, five zoom steps in a browser. GUESS,
/// tuned on the owner's trackpad.
const PINCH_UNITS: f64 = 600.0;

/// The axis a trackpad gesture scrolls along (precise deltas only).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Rail {
    /// No gesture start seen (a device that reports no phases): each event is judged alone.
    PerEvent,
    /// A gesture started; its motion so far in points, until one axis reaches [`RAIL_DECIDE`].
    Undecided {
        x: f64,
        y: f64,
    },
    Vertical,
    Horizontal,
    /// A deliberate diagonal: both axes go.
    Free,
}

impl Rail {
    fn of(x: f64, y: f64) -> Rail {
        if x < y * RAIL_RATIO {
            Rail::Vertical
        } else if y < x * RAIL_RATIO {
            Rail::Horizontal
        } else {
            Rail::Free
        }
    }
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
    /// A trackpad pinch goes to the host as Ctrl held and the wheel, as a Windows precision touchpad sends it (true
    /// toward a Windows host). False: a pinch sends nothing.
    pub pinch_zoom: bool,
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
            pinch_zoom: false,
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
    /// Option bits of `mods` the host does not hold (text mode defers them; see the module comment).
    unsent: u8,
    /// An unsent Option went down with nothing else held and nothing was pressed since: its release is a tap.
    option_tap: bool,
    caps: bool,
    /// `kVK_*` of the keys held (taken key-downs without their key-up).
    down: HashSet<u16>,
    /// Keys whose key-down the event tap swallowed: their key-up is swallowed too, so macOS never sees an orphan.
    swallowed: HashSet<u16>,
    buttons: BTreeSet<u8>,
    pointer: Option<(u16, u16)>,
    /// Wheel units (1/120 notch) not sent yet, x then y.
    wheel_rest: (f64, f64),
    /// The axis of the trackpad gesture in progress.
    rail: Rail,
    /// A pinch in progress: `Some(true)` when this side pressed the host's Ctrl for it (let go at its end).
    pinch: Option<bool>,
    /// Pinch wheel units not sent yet.
    pinch_rest: f64,
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
            router: KeyRouter::for_os(ViewerOs::MacOs, config.mode, config.layouts_match),
            mods: 0,
            unsent: 0,
            option_tap: false,
            caps: false,
            down: HashSet::new(),
            swallowed: HashSet::new(),
            buttons: BTreeSet::new(),
            pointer: None,
            wheel_rest: (0.0, 0.0),
            rail: Rail::PerEvent,
            pinch: None,
            pinch_rest: 0.0,
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

    /// The modifier byte the host holds: the viewer's, without the Option keys text mode has not sent.
    fn wire_mods(&self) -> u8 {
        self.mods & !self.unsent
    }

    /// Brings the modifier byte to `wanted`, one record per key that changed (bit order, each carrying the byte after
    /// its own change), and returns them. In text mode an Option key that goes down is held back (see the module
    /// comment); its release sends nothing, or a press and release when it was tapped alone.
    fn move_mods_to(&mut self, wanted: u8) -> Vec<InputEvent> {
        let mut events = Vec::new();
        for i in 0..8 {
            let bit = 1u8 << i;
            if (self.mods ^ wanted) & bit == 0 {
                continue;
            }
            let down = wanted & bit != 0;
            let Some(usage) = modifier::usage_of(bit) else { continue };
            if down {
                self.mods |= bit;
                if bit & OPTION_BITS != 0 && self.router.text_mode() {
                    if self.unsent == 0 {
                        self.option_tap = self.down.is_empty();
                    }
                    self.unsent |= bit;
                    continue;
                }
                events.push(self.key_event(usage, true, self.wire_mods()));
            } else if self.unsent & bit != 0 {
                self.mods &= !bit;
                self.unsent &= !bit;
                if self.option_tap {
                    events.push(self.key_event(usage, true, self.wire_mods() | bit));
                    events.push(self.key_event(usage, false, self.wire_mods()));
                }
            } else {
                self.mods &= !bit;
                events.push(self.key_event(usage, false, self.wire_mods()));
            }
        }
        events
    }

    /// The Option keys held back go to the host now (a key that goes physically is pressed with them).
    fn flush_options(&mut self) -> Vec<InputEvent> {
        self.option_tap = false;
        let mut events = Vec::new();
        for bit in [modifier::LALT, modifier::RALT] {
            if self.unsent & bit != 0 {
                self.unsent &= !bit;
                if let Some(usage) = modifier::usage_of(bit) {
                    events.push(self.key_event(usage, true, self.wire_mods()));
                }
            }
        }
        events
    }

    /// The Option keys the host holds are let go before text (Alt held on a Windows host turns text into a menu
    /// mnemonic); they stay held on the viewer, so a later physical key sends them again.
    fn lift_options(&mut self) -> Vec<InputEvent> {
        self.option_tap = false;
        let mut events = Vec::new();
        for bit in [modifier::LALT, modifier::RALT] {
            if self.wire_mods() & bit != 0 {
                self.unsent |= bit;
                if let Some(usage) = modifier::usage_of(bit) {
                    events.push(self.key_event(usage, false, self.wire_mods()));
                }
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
        self.unsent = 0;
        self.option_tap = false;
        self.down.clear();
        self.swallowed.clear();
        self.buttons.clear();
        self.pointer = None;
        self.wheel_rest = (0.0, 0.0);
        self.pinch = None;
        self.pinch_rest = 0.0;
        self.router.reset();
        vec![InputEvent::ReleaseAll]
    }

    /// Nothing held, nothing to release (the host has nothing of ours down).
    pub fn is_idle(&self) -> bool {
        self.mods == 0 && self.down.is_empty() && self.buttons.is_empty() && self.pinch != Some(true)
    }

    /// A position record was not delivered (the consumer was behind): the next move or click must send it again.
    pub fn forget_pointer(&mut self) {
        self.pointer = None;
    }

    /// A wheel record was not delivered (the consumer was behind): its units go back into the remainder, so the next
    /// scroll event in the same direction carries them instead of losing them. Capped at one record's range per axis.
    pub fn refund_wheel(&mut self, dx: i16, dy: i16) {
        let cap = f64::from(i16::MAX);
        if self.pinch.is_some() {
            // A pinch's wheel: it rides on the pinch's next event, under the same Ctrl.
            self.pinch_rest = (self.pinch_rest + f64::from(dy)).clamp(-cap, cap);
            return;
        }
        // The remainder is kept before the horizontal sign flip of `wheel_units`.
        self.wheel_rest.0 = (self.wheel_rest.0 - f64::from(dx)).clamp(-cap, cap);
        self.wheel_rest.1 = (self.wheel_rest.1 + f64::from(dy)).clamp(-cap, cap);
    }

    /// A key press or release from the video view; `layout` says what a press types (asked only when the press may go
    /// as text).
    pub fn key(&mut self, msg: &KeyMsg, probe: KeyProbe<'_>, layout: &mut dyn Layout) -> Outcome {
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
        let typed = if self.wants_text(msg.key_code, msg.down) {
            layout.type_key(msg)
        } else {
            if msg.down {
                layout.clear_dead_key();
            }
            Typed::default()
        };
        events.extend(self.translate(msg.key_code, msg.down, &typed));
        Outcome::taken(events)
    }

    /// Whether a press may go as text, so the layout is asked what it types: text mode in force, a key with a HID row
    /// that is no modifier, and no shortcut modifier (Control, Command) held.
    fn wants_text(&self, kvk: u16, down: bool) -> bool {
        if !down || !self.router.text_mode() || self.router.is_shortcut(self.mods) {
            return false;
        }
        hid_of_kvk(kvk, self.config.keyboard).is_some_and(|(page, usage)| modifier::bit_of(page, usage).is_none())
    }

    fn translate(&mut self, kvk: u16, down: bool, typed: &Typed) -> Vec<InputEvent> {
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
            // A press is decided on the viewer's real modifiers; a release carries what the host holds.
            modifiers: if down { self.mods } else { self.wire_mods() },
            locks: self.locks(),
            text: typed.text.as_deref(),
            dead_key: typed.dead,
        };
        match self.router.route(&input) {
            Routed::Text(e) => {
                let mut events = self.lift_options();
                events.push(self.finish(e));
                events
            }
            Routed::Physical(e) => {
                // A physical press takes the held-back Option keys with it (its record already carries them).
                let mut events = if down { self.flush_options() } else { Vec::new() };
                events.push(self.finish(e));
                events
            }
            Routed::Drop => {
                if down {
                    // A dead key: something was pressed under the Option keys, so their release is no tap.
                    self.option_tap = false;
                }
                Vec::new()
            }
        }
    }

    /// A key event the event tap saw. Swallows (and sends) only the system shortcuts, and the release of a key it
    /// swallowed; everything else reaches the view, which sends it through [`InputCore::key`].
    pub fn tap_key(&mut self, kvk: u16, down: bool, flags: Modifiers, probe: KeyProbe<'_>) -> Outcome {
        if !down {
            return if self.swallowed.remove(&kvk) {
                Outcome::taken(self.translate(kvk, false, &Typed::default()))
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
        events.extend(self.translate(kvk, true, &Typed::default()));
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
                        // Option+click is Alt+click on the host: the held-back Option keys go first.
                        events.extend(self.flush_options());
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
            MouseKind::Scroll { dx, dy, precise, starts } => {
                // Out of the picture too: a gesture's axis comes from all of its motion.
                let (dx, dy) = if precise { self.on_rail(dx, dy, starts) } else { (dx, dy) };
                if let Some(at) = at {
                    let (ux, uy) = self.wheel_units(dx, dy, precise);
                    if ux != 0 || uy != 0 {
                        events.extend(self.pointer_event(at));
                        events.push(InputEvent::Wheel { dx: ux, dy: uy });
                    }
                }
            }
            MouseKind::Magnify { amount, ends } => {
                if self.config.pinch_zoom {
                    events.extend(self.pinch_event(amount, ends, at));
                }
            }
        }
        Outcome::taken(events)
    }

    /// The modifier byte the host holds, as the records carry it (after the Ctrl/Cmd swap).
    fn host_mods(&self) -> u8 {
        let m = self.wire_mods();
        if self.config.swap_ctrl_cmd { swap_modifiers(m) } else { m }
    }

    /// One pinch event toward a Windows host: what its precision touchpad sends for a pinch, Ctrl held and the
    /// wheel (positive, away from the user, zooms in). The Ctrl goes down before the first wheel record, unless the
    /// host already holds one, and up at the gesture's end; it is built after the swap, so it stays Ctrl whatever
    /// "Trocar Ctrl e Cmd" says.
    fn pinch_event(&mut self, amount: f64, ends: bool, at: Option<(u16, u16)>) -> Vec<InputEvent> {
        let mut events = Vec::new();
        if let Some(at) = at.filter(|_| amount.is_finite() && amount != 0.0) {
            if self.pinch_rest != 0.0 && self.pinch_rest.signum() != amount.signum() {
                self.pinch_rest = 0.0;
            }
            self.pinch_rest += amount * PINCH_UNITS;
            let whole = clamp_i16(self.pinch_rest.trunc());
            self.pinch_rest -= f64::from(whole);
            if whole != 0 {
                if self.pinch.is_none() {
                    let host = self.host_mods();
                    let press = host & (modifier::LCTRL | modifier::RCTRL) == 0;
                    if press {
                        events.push(InputEvent::Key {
                            usage_page: PAGE_KEYBOARD,
                            usage: CTRL_USAGE,
                            down: true,
                            modifiers: host | modifier::LCTRL,
                            locks: self.locks(),
                        });
                    }
                    self.pinch = Some(press);
                }
                events.extend(self.pointer_event(at));
                events.push(InputEvent::Wheel { dx: 0, dy: whole });
            }
        }
        if ends {
            if self.pinch == Some(true) {
                events.push(InputEvent::Key {
                    usage_page: PAGE_KEYBOARD,
                    usage: CTRL_USAGE,
                    down: false,
                    modifiers: self.host_mods() & !modifier::LCTRL,
                    locks: self.locks(),
                });
            }
            self.pinch = None;
            self.pinch_rest = 0.0;
        }
        events
    }

    /// The trackpad deltas of one scroll event kept on the gesture's axis.
    ///
    /// A swipe along one axis also reports a little motion of both signs on the other. Sent, it reaches the host as
    /// tiny wheel records of alternating sign all along the swipe, and an app that counts notches with one accumulator
    /// for both wheels (Windows Terminal in VT mouse mode or alternate scroll clears it on an event of the other sign)
    /// never reaches a notch: the scroll sticks. So a gesture keeps to the axis its first [`RAIL_DECIDE`] points
    /// took (both axes for a diagonal, the minor one at least [`RAIL_RATIO`] of the major), through its momentum, and
    /// the other axis is dropped with its remainder. Without gesture phases each event is judged alone.
    fn on_rail(&mut self, dx: f64, dy: f64, starts: bool) -> (f64, f64) {
        if starts {
            self.rail = Rail::Undecided { x: 0.0, y: 0.0 };
        }
        let size = |v: f64| if v.is_finite() { v.abs() } else { 0.0 };
        let (ax, ay) = (size(dx), size(dy));
        let axis = match self.rail {
            Rail::PerEvent => Rail::of(ax, ay),
            Rail::Undecided { x, y } => {
                let (x, y) = (x + ax, y + ay);
                if x.max(y) >= RAIL_DECIDE {
                    self.rail = Rail::of(x, y);
                    self.rail
                } else {
                    self.rail = Rail::Undecided { x, y };
                    Rail::of(ax, ay)
                }
            }
            decided => decided,
        };
        match axis {
            Rail::Vertical => {
                self.wheel_rest.0 = 0.0;
                (0.0, dy)
            }
            Rail::Horizontal => {
                self.wheel_rest.1 = 0.0;
                (dx, 0.0)
            }
            _ => (dx, dy),
        }
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
            // What one record cannot carry stays for the next (at most one more record's worth).
            let whole = clamp_i16(rest.trunc());
            *rest = (*rest - f64::from(whole)).clamp(f64::from(i16::MIN), f64::from(i16::MAX));
            whole
        };
        let x = take(&mut self.wheel_rest.0, dx).saturating_neg();
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
        let down = c.key(&key(0x00, true, Modifiers::default(), Some("a")), &none, &mut EventText);
        assert_eq!(down, Outcome { events: vec![k(0x04, true, 0)], taken: true });
        let up = c.key(&key(0x00, false, Modifiers::default(), None), &none, &mut EventText);
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
        let a = c.key(&key(0x00, true, Modifiers::CAPS_LOCK, Some("A")), &none, &mut EventText);
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
        let out = c.key(&key(0x08, true, Modifiers::CONTROL, Some("\u{3}")), &probe, &mut EventText);
        assert_eq!(out.events, vec![k(0xE0, true, modifier::LCTRL), k(0x06, true, modifier::LCTRL)]);
    }

    #[test]
    fn repeats_go_again_and_the_release_of_an_untaken_press_is_not_ours() {
        let mut c = core();
        let msg = KeyMsg { repeat: true, ..key(0x00, true, Modifiers::default(), Some("a")) };
        assert_eq!(c.key(&msg, &none, &mut EventText).events, vec![k(0x04, true, 0)]);
        assert_eq!(c.key(&msg, &none, &mut EventText).events, vec![k(0x04, true, 0)]);
        // Cmd+Q is the viewer's own: neither the press nor its release is taken.
        assert_eq!(c.key(&key(0x0C, true, CMD, Some("q")), &none, &mut EventText), Outcome::passed());
        assert_eq!(c.key(&key(0x0C, false, CMD, None), &none, &mut EventText), Outcome::passed());
        // An orphan release is not ours either.
        assert_eq!(c.key(&key(0x01, false, Modifiers::default(), None), &none, &mut EventText), Outcome::passed());
    }

    #[test]
    fn a_key_the_tables_do_not_know_is_taken_and_sends_nothing() {
        let mut c = core();
        let out = c.key(&key(0xA0, true, Modifiers::default(), None), &none, &mut EventText);
        assert_eq!(out, Outcome { events: vec![], taken: true });
        let up = c.key(&key(0xA0, false, Modifiers::default(), None), &none, &mut EventText);
        assert_eq!(up, Outcome { events: vec![], taken: true });
    }

    #[test]
    fn the_iso_swap_applies_to_the_viewers_own_keyboard() {
        let mut c = InputCore::new(InputConfig { keyboard: KbKind::Iso, ..InputConfig::default() });
        // The key left of 1 on an ISO Mac reports 0x0A and is the same HID key as ANSI's 0x32.
        assert_eq!(
            c.key(&key(0x0A, true, Modifiers::default(), Some("§")), &none, &mut EventText).events,
            vec![k(0x35, true, 0)]
        );
        let mut a = core();
        assert_eq!(
            a.key(&key(0x32, true, Modifiers::default(), Some("`")), &none, &mut EventText).events,
            vec![k(0x35, true, 0)]
        );
    }

    #[test]
    fn text_mode_sends_unicode_and_keeps_shortcuts_and_special_keys_physical() {
        let mut c =
            InputCore::new(InputConfig { mode: KeyboardMode::Text, layouts_match: false, ..InputConfig::default() });
        let out = c.key(&key(0x00, true, Modifiers::default(), Some("ç")), &none, &mut EventText);
        assert_eq!(out.events, vec![InputEvent::Text("ç".into())]);
        // Its release has no record.
        assert_eq!(
            c.key(&key(0x00, false, Modifiers::default(), None), &none, &mut EventText),
            Outcome { events: vec![], taken: true }
        );
        // The arrow key's characters are private-use: a key, not text.
        let up = c.key(&key(0x7E, true, Modifiers::FUNCTION, Some("\u{F700}")), &none, &mut EventText);
        assert_eq!(up.events, vec![k(0x52, true, 0)]);
        // Shortcuts stay physical even with text on the key.
        let probe = |kvk: u16| kvk == 0x37;
        let copy = c.key(&key(0x08, true, CMD, Some("c")), &probe, &mut EventText);
        assert_eq!(copy.events, vec![k(0xE3, true, modifier::LGUI), k(0x06, true, modifier::LGUI)]);
        // Right Option is the Mac's character modifier: text, and no Alt reaches the host (Cmd is let go first).
        c.modifiers_changed(Modifiers::default(), &none);
        let ropt = |kvk: u16| kvk == 0x3D;
        let at = c.key(&key(0x13, true, Modifiers::OPTION, Some("@")), &ropt, &mut EventText);
        assert_eq!(at.events, vec![InputEvent::Text("@".into())]);
    }

    /// The dead keys of "Brazilian - Pro" (the owner's Mac layout, US-based): ' is a dead acute, Shift+` a dead tilde,
    /// and Option+E a dead acute as on every US-based Mac layout. What `UCKeyTranslate` answers for them (proven against
    /// the real layout in `tests/mac_layout.rs`), as a script.
    #[derive(Default)]
    struct FakePro {
        accent: Option<char>,
        asked: Vec<u16>,
        cleared: u32,
    }

    impl Layout for FakePro {
        fn type_key(&mut self, msg: &KeyMsg) -> Typed {
            self.asked.push(msg.key_code);
            let shift = msg.modifiers.contains(Modifiers::SHIFT);
            let option = msg.modifiers.contains(Modifiers::OPTION);
            let dead = match (msg.key_code, shift, option) {
                (0x27, false, false) => Some('\''),
                (0x32, true, false) => Some('~'),
                (0x0E, false, true) => Some('\''),
                _ => None,
            };
            if let (Some(d), None) = (dead, self.accent) {
                self.accent = Some(d);
                return Typed { text: None, dead: true };
            }
            let base = match (msg.key_code, option) {
                (0x00, _) => 'a',
                (0x0E, _) => 'e',
                (0x08, false) => 'c',
                (0x08, true) => 'ç',
                (0x31, _) => ' ',
                (0x11, _) => 't',
                (0x27, _) => '\'',
                _ => '?',
            };
            let text = match (self.accent.take(), base) {
                (None, b) => b.to_string(),
                (Some('\''), 'a') => "á".into(),
                (Some('\''), 'e') => "é".into(),
                (Some('\''), 'c') => "ç".into(),
                (Some('~'), 'a') => "ã".into(),
                (Some(acc), ' ') => acc.to_string(),
                (Some(acc), b) => format!("{acc}{b}"),
            };
            Typed { text: Some(text), dead: false }
        }

        fn clear_dead_key(&mut self) {
            self.accent = None;
            self.cleared += 1;
        }
    }

    fn auto_to_windows() -> InputCore {
        InputCore::new(InputConfig { mode: KeyboardMode::Auto, layouts_match: false, ..InputConfig::default() })
    }

    fn type_keys(c: &mut InputCore, layout: &mut FakePro, keys: &[(u16, Modifiers)]) -> Vec<InputEvent> {
        let mut events = Vec::new();
        for &(kvk, mods) in keys {
            events.extend(c.key(&key(kvk, true, mods, None), &none, layout).events);
            events.extend(c.key(&key(kvk, false, mods, None), &none, layout).events);
        }
        events
    }

    /// The owner's report: on "Brazilian - Pro" á, é, ã and ç are a dead key then the letter. AppKit gives the dead
    /// key's press no characters (NSEvent.characters: "an empty string for dead keys"), and the press used to go to the
    /// Windows host as the physical ' key. Now it sends nothing, its release neither, and the letter carries the
    /// composed character.
    #[test]
    fn a_dead_key_press_sends_nothing_in_text_mode() {
        let mut c = auto_to_windows();
        let mut pro = FakePro::default();
        let plain = Modifiers::default();
        let text = |t: &str| InputEvent::Text(t.into());
        assert_eq!(type_keys(&mut c, &mut pro, &[(0x27, plain), (0x00, plain)]), vec![text("á")]);
        assert_eq!(type_keys(&mut c, &mut pro, &[(0x27, plain), (0x0E, plain)]), vec![text("é")]);
        assert_eq!(type_keys(&mut c, &mut pro, &[(0x27, plain), (0x08, plain)]), vec![text("ç")]);
        // ' then space is the apostrophe itself; ' then a letter it does not compose with types both.
        assert_eq!(type_keys(&mut c, &mut pro, &[(0x27, plain), (0x31, plain)]), vec![text("'")]);
        assert_eq!(type_keys(&mut c, &mut pro, &[(0x27, plain), (0x11, plain)]), vec![text("'t")]);
        // Shift+` is a dead tilde: Shift goes to the host as a key, the tilde waits, Shift is let go, then ã.
        let lshift = |kvk: u16| kvk == 0x38;
        let mut events = c.modifiers_changed(Modifiers::SHIFT, &lshift).events;
        events.extend(c.key(&key(0x32, true, Modifiers::SHIFT, None), &lshift, &mut pro).events);
        events.extend(c.key(&key(0x32, false, Modifiers::SHIFT, None), &lshift, &mut pro).events);
        events.extend(c.modifiers_changed(Modifiers::default(), &none).events);
        events.extend(type_keys(&mut c, &mut pro, &[(0x00, plain)]));
        assert_eq!(
            events,
            vec![k(0xE1, true, modifier::LSHIFT), k(0xE1, false, 0), text("ã")],
            "no ` or ~ key reached the host"
        );
        assert!(c.is_idle(), "no key is left held on either side");
    }

    #[test]
    fn a_shortcut_or_a_key_with_no_text_drops_a_pending_dead_key_and_the_layout_is_not_asked() {
        let mut c = auto_to_windows();
        let mut pro = FakePro::default();
        let cmd = |kvk: u16| kvk == 0x37;
        assert_eq!(type_keys(&mut c, &mut pro, &[(0x27, Modifiers::default())]), vec![]);
        c.modifiers_changed(CMD, &cmd);
        let copy = c.key(&key(0x08, true, CMD, Some("c")), &cmd, &mut pro);
        assert_eq!(copy.events, vec![k(0x06, true, modifier::LGUI)], "Cmd+C stays a physical shortcut");
        assert_eq!(pro.asked, vec![0x27], "the layout is not asked for a shortcut");
        assert_eq!(pro.cleared, 1, "the pending accent is dropped");
        c.key(&key(0x08, false, CMD, None), &cmd, &mut pro);
        c.modifiers_changed(Modifiers::default(), &none);
        assert_eq!(type_keys(&mut c, &mut pro, &[(0x00, Modifiers::default())]), vec![InputEvent::Text("a".into())]);
    }

    /// Copy and paste toward a Windows host in text mode: Control+C and Command+V stay physical keys with their
    /// modifier (never text, the layout is not asked), and with "Trocar Ctrl e Cmd no Mac" Command+C arrives as Ctrl+C.
    #[test]
    fn copy_and_paste_shortcuts_stay_physical_in_text_mode_and_follow_the_swap() {
        let mut c = auto_to_windows();
        let mut pro = FakePro::default();
        let ctrl = |kvk: u16| kvk == 0x3B;
        let mut events = c.modifiers_changed(Modifiers::CONTROL, &ctrl).events;
        events.extend(c.key(&key(0x08, true, Modifiers::CONTROL, Some("\u{3}")), &ctrl, &mut pro).events);
        assert_eq!(events, vec![k(0xE0, true, modifier::LCTRL), k(0x06, true, modifier::LCTRL)]);
        c.key(&key(0x08, false, Modifiers::CONTROL, None), &ctrl, &mut pro);
        c.modifiers_changed(Modifiers::default(), &none);
        let cmd = |kvk: u16| kvk == 0x37;
        let mut events = c.modifiers_changed(CMD, &cmd).events;
        events.extend(c.key(&key(0x09, true, CMD, Some("v")), &cmd, &mut pro).events);
        assert_eq!(
            events,
            vec![k(0xE3, true, modifier::LGUI), k(0x19, true, modifier::LGUI)],
            "Win+V without the swap"
        );
        assert!(pro.asked.is_empty(), "no shortcut asked the layout");

        let mut s = InputCore::new(InputConfig { swap_ctrl_cmd: true, ..auto_to_windows().config() });
        let mut events = s.modifiers_changed(CMD, &cmd).events;
        events.extend(s.key(&key(0x08, true, CMD, Some("c")), &cmd, &mut pro).events);
        assert_eq!(events, vec![k(0xE0, true, modifier::LCTRL), k(0x06, true, modifier::LCTRL)], "Ctrl+C on the host");
    }

    #[test]
    fn losing_the_keyboard_with_command_and_option_held_leaves_nothing_held_on_the_host() {
        for mode in [KeyboardMode::Physical, KeyboardMode::Auto] {
            let mut c = InputCore::new(InputConfig { mode, layouts_match: false, ..InputConfig::default() });
            let probe = |kvk: u16| kvk == 0x37 || kvk == 0x3A;
            c.modifiers_changed(CMD.union(Modifiers::OPTION), &probe);
            // Command+Option+Left went to the host (Option with it), then the window lost the keyboard.
            c.key(&key(0x7B, true, CMD.union(Modifiers::OPTION), None), &probe, &mut EventText);
            assert!(!c.is_idle());
            assert_eq!(c.release_all(), vec![InputEvent::ReleaseAll], "{mode:?}");
            assert!(c.is_idle() && c.modifiers() == 0, "{mode:?}");
            // Back with nothing held: the releases AppKit sends to the window now are not ours, and nothing is sent.
            assert_eq!(c.modifiers_changed(Modifiers::default(), &none).events, vec![], "{mode:?}");
        }
    }

    #[test]
    fn physical_mode_never_asks_the_layout_and_sends_the_dead_key_as_a_key() {
        let mut c = core();
        let mut pro = FakePro::default();
        let events = type_keys(&mut c, &mut pro, &[(0x27, Modifiers::default()), (0x00, Modifiers::default())]);
        assert_eq!(events, vec![k(0x34, true, 0), k(0x34, false, 0), k(0x04, true, 0), k(0x04, false, 0)]);
        assert!(pro.asked.is_empty());
    }

    /// Left Option + C types ç on a US-based Mac layout, and Option+E then E types é: Option is the Mac's character
    /// modifier, not a shortcut, and the host never sees an Alt (held, it turns text into a menu mnemonic on Windows;
    /// pressed and released alone, it opens the menu bar).
    #[test]
    fn option_text_is_text_with_no_alt_on_the_host() {
        let mut c = auto_to_windows();
        let mut pro = FakePro::default();
        let lopt = |kvk: u16| kvk == 0x3A;
        let mut events = c.modifiers_changed(Modifiers::OPTION, &lopt).events;
        events.extend(c.key(&key(0x08, true, Modifiers::OPTION, Some("ç")), &lopt, &mut EventText).events);
        events.extend(c.key(&key(0x08, false, Modifiers::OPTION, None), &lopt, &mut EventText).events);
        events.extend(c.modifiers_changed(Modifiers::default(), &none).events);
        assert_eq!(events, vec![InputEvent::Text("ç".into())]);
        // Option+E (dead), Option let go, then E.
        let mut events = c.modifiers_changed(Modifiers::OPTION, &lopt).events;
        events.extend(c.key(&key(0x0E, true, Modifiers::OPTION, None), &lopt, &mut pro).events);
        events.extend(c.key(&key(0x0E, false, Modifiers::OPTION, None), &lopt, &mut pro).events);
        events.extend(c.modifiers_changed(Modifiers::default(), &none).events);
        events.extend(type_keys(&mut c, &mut pro, &[(0x0E, Modifiers::default())]));
        assert_eq!(events, vec![InputEvent::Text("é".into())]);
        assert!(c.is_idle());
    }

    #[test]
    fn option_goes_to_the_host_with_a_physical_key_and_is_let_go_before_text() {
        let mut c = auto_to_windows();
        let lopt = |kvk: u16| kvk == 0x3A;
        assert_eq!(c.modifiers_changed(Modifiers::OPTION, &lopt).events, vec![], "held back");
        // Option+Left (word left on the host's Alt+Left): Alt goes first, then the arrow with the byte.
        let left = c.key(&key(0x7B, true, Modifiers::OPTION.union(Modifiers::FUNCTION), None), &lopt, &mut EventText);
        assert_eq!(left.events, vec![k(0xE2, true, modifier::LALT), k(0x50, true, modifier::LALT)]);
        c.key(&key(0x7B, false, Modifiers::OPTION, None), &lopt, &mut EventText);
        // Still holding Option, C types ç: the host's Alt is let go first (after the arrow, so no menu opens).
        let cedilla = c.key(&key(0x08, true, Modifiers::OPTION, Some("ç")), &lopt, &mut EventText);
        assert_eq!(cedilla.events, vec![k(0xE2, false, 0), InputEvent::Text("ç".into())]);
        c.key(&key(0x08, false, Modifiers::OPTION, None), &lopt, &mut EventText);
        // Option let go: the host holds no Alt, nothing to send.
        assert_eq!(c.modifiers_changed(Modifiers::default(), &none).events, vec![]);
        assert!(c.is_idle());
    }

    #[test]
    fn option_tapped_alone_is_a_press_and_release_and_option_click_carries_alt() {
        let mut c = auto_to_windows();
        let lopt = |kvk: u16| kvk == 0x3A;
        c.modifiers_changed(Modifiers::OPTION, &lopt);
        assert_eq!(
            c.modifiers_changed(Modifiers::default(), &none).events,
            vec![k(0xE2, true, modifier::LALT), k(0xE2, false, 0)]
        );
        // Option+click: Alt goes down before the button, and up when Option is let go.
        c.modifiers_changed(Modifiers::OPTION, &lopt);
        let click = c.mouse(
            mouse(MouseKind::Button { button: PointerButton::Left, down: true }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(click.events[0], k(0xE2, true, modifier::LALT));
        assert_eq!(click.events.last(), Some(&InputEvent::Button { button: MouseButton::Left, down: true }));
        assert_eq!(c.modifiers_changed(Modifiers::default(), &none).events, vec![k(0xE2, false, 0)]);
        // Another modifier pressed under a held-back Option does not carry Alt in its byte.
        c.modifiers_changed(Modifiers::OPTION, &lopt);
        let lshift = |kvk: u16| kvk == 0x38 || kvk == 0x3A;
        let shift = c.modifiers_changed(Modifiers::OPTION.union(Modifiers::SHIFT), &lshift);
        assert_eq!(shift.events, vec![k(0xE1, true, modifier::LSHIFT)]);
    }

    #[test]
    fn physical_mode_sends_option_at_once_as_before() {
        let mut c = core();
        let lopt = |kvk: u16| kvk == 0x3A;
        assert_eq!(c.modifiers_changed(Modifiers::OPTION, &lopt).events, vec![k(0xE2, true, modifier::LALT)]);
        let o = c.key(&key(0x08, true, Modifiers::OPTION, Some("ç")), &lopt, &mut EventText);
        assert_eq!(o.events, vec![k(0x06, true, modifier::LALT)]);
    }

    #[test]
    fn debug_of_typed_text_shows_its_size_only() {
        let shown = format!("{:?}", Typed { text: Some("senha".into()), dead: false });
        assert!(!shown.contains("senha") && shown.contains("<redacted 5 bytes>"), "{shown}");
    }

    #[test]
    fn physical_mode_ignores_the_text() {
        let mut c = core();
        let out = c.key(&key(0x00, true, Modifiers::default(), Some("ç")), &none, &mut EventText);
        assert_eq!(out.events, vec![k(0x04, true, 0)]);
    }

    #[test]
    fn ctrl_and_cmd_swap_exchanges_both_the_keys_and_the_byte() {
        let mut c = InputCore::new(InputConfig { swap_ctrl_cmd: true, ..InputConfig::default() });
        let cmd = |kvk: u16| kvk == 0x37;
        let out = c.modifiers_changed(CMD, &cmd);
        // The viewer's left Cmd is sent as left Ctrl, and the byte follows.
        assert_eq!(out.events, vec![k(0xE0, true, modifier::LCTRL)]);
        let copy = c.key(&key(0x08, true, CMD, Some("c")), &cmd, &mut EventText);
        assert_eq!(copy.events, vec![k(0x06, true, modifier::LCTRL)]);
        // Mac Ctrl becomes Win.
        let ctrl = |kvk: u16| kvk == 0x3B;
        let mut d = InputCore::new(InputConfig { swap_ctrl_cmd: true, ..InputConfig::default() });
        assert_eq!(d.modifiers_changed(Modifiers::CONTROL, &ctrl).events, vec![k(0xE3, true, modifier::LGUI)]);
    }

    #[test]
    fn the_num_lock_bit_follows_the_config() {
        let mut c = InputCore::new(InputConfig { num_lock: false, ..InputConfig::default() });
        let out = c.key(&key(0x00, true, Modifiers::default(), None), &none, &mut EventText);
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
        c.key(&key(0x00, true, CMD, Some("a")), &probe, &mut EventText);
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
        assert_eq!(c.key(&key(0x00, false, Modifiers::default(), None), &none, &mut EventText), Outcome::passed());
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
            mouse(MouseKind::Scroll { dx: 0.0, dy: 1.0, precise: false, starts: false }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(out.events[1], InputEvent::Wheel { dx: 0, dy: 120 });
        let out = c.mouse(
            mouse(MouseKind::Scroll { dx: 2.0, dy: -3.0, precise: false, starts: false }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(out.events, vec![InputEvent::Wheel { dx: -240, dy: -360 }]);
        // Trackpad: 40 points make a notch, so 10 points are 30 units.
        let out = c.mouse(
            mouse(MouseKind::Scroll { dx: 0.0, dy: 10.0, precise: true, starts: false }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert_eq!(out.events, vec![InputEvent::Wheel { dx: 0, dy: 30 }]);
    }

    #[test]
    fn small_trackpad_deltas_add_up_and_a_turn_around_drops_the_remainder() {
        let scroll = |c: &mut InputCore, dy: f64| {
            c.mouse(
                mouse(MouseKind::Scroll { dx: 0.0, dy, precise: true, starts: false }, 100.0, 100.0),
                Some(&identity_map),
                2.0,
            )
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

    /// A two-finger swipe down on a trackpad as `scrollWheel:` reports it (natural scrolling: the content goes up, so
    /// `scrollingDeltaY` is negative), in points: 40 events of 1 to 6 points over about half a second, then 25
    /// momentum events that fade out. The fingers never move straight: `scrollingDeltaX` carries a little sideways
    /// jitter of both signs, under a point (the momentum keeps some here too, the worst case).
    fn swipe_down() -> Vec<(f64, f64)> {
        const JITTER: [f64; 8] = [0.4, -0.3, 0.0, 0.6, -0.5, 0.2, -0.7, 0.0];
        let mut events = Vec::new();
        for i in 0..40 {
            let dy = 1.0 + 5.0 * (std::f64::consts::PI * f64::from(i) / 39.0).sin();
            events.push((JITTER[i as usize % 8], -dy));
        }
        let mut dy = 6.0;
        for i in 0..25 {
            events.push((JITTER[(i + 3) % 8] * 0.5, -dy));
            dy *= 0.85;
        }
        events
    }

    /// The wheel records of trackpad events; with `gesture`, the first one starts a touch gesture (`NSEvent.phase`
    /// `Began`) and the rest are its changes and momentum, else the device reports no phases.
    fn wheels(c: &mut InputCore, events: &[(f64, f64)], gesture: bool) -> Vec<(i16, i16)> {
        let mut wire = Vec::new();
        for (i, &(dx, dy)) in events.iter().enumerate() {
            let starts = gesture && i == 0;
            let out = c.mouse(
                mouse(MouseKind::Scroll { dx, dy, precise: true, starts }, 100.0, 100.0),
                Some(&identity_map),
                2.0,
            );
            for e in out.events {
                if let InputEvent::Wheel { dx, dy } = e {
                    wire.push((dx, dy));
                }
            }
        }
        wire
    }

    /// The lines Windows Terminal scrolls in VT mouse mode or alternate scroll for the wheel records the host injects
    /// (microsoft/terminal `src/terminal/input/mouseInput.cpp`, `TerminalInput::HandleMouse`, read 2026-11-10): one
    /// accumulator serves the vertical and the horizontal wheel; an event whose sign differs from it clears it; a line
    /// goes out when it reaches a notch (120), and then it is cleared. The host injects each record's vertical part,
    /// then its horizontal part (`swapdesk-input-win` `wheel_inputs`). Returns (vertical lines, horizontal lines).
    fn terminal_lines(wire: &[(i16, i16)]) -> (i32, i32) {
        let (mut acc, mut lines) = (0i32, (0i32, 0i32));
        let mut feed = |delta: i32, vertical: bool| {
            if delta.signum() != acc.signum() {
                acc = 0;
            }
            acc += delta;
            if acc.abs() >= 120 {
                acc = 0;
                if vertical { lines.0 += delta.signum() } else { lines.1 += delta.signum() }
            }
        };
        for &(dx, dy) in wire {
            if dy != 0 {
                feed(i32::from(dy), true);
            }
            if dx != 0 {
                feed(i32::from(dx), false);
            }
        }
        lines
    }

    fn sum(wire: &[(i16, i16)]) -> (i32, i32) {
        wire.iter().fold((0, 0), |a, w| (a.0 + i32::from(w.0), a.1 + i32::from(w.1)))
    }

    #[test]
    fn a_vertical_trackpad_swipe_reaches_the_wire_whole_and_without_a_sideways_wheel() {
        let swipe = swipe_down();
        let total: f64 = swipe.iter().map(|(_, dy)| dy * 3.0).sum();
        let wire = wheels(&mut core(), &swipe, true);
        let sideways: Vec<i16> = wire.iter().map(|w| w.0).filter(|dx| *dx != 0).collect();
        assert_eq!(sideways, Vec::<i16>::new(), "the jitter of the fingers went out as a horizontal wheel");
        let sent = f64::from(sum(&wire).1);
        assert!((sent - total).abs() < 1.0, "sent {sent} units of {total}: only the last fraction may stay behind");
        // The same swipe drawn without jitter gives the same records.
        let straight: Vec<(f64, f64)> = swipe.iter().map(|(_, dy)| (0.0, *dy)).collect();
        assert_eq!(wire, wheels(&mut core(), &straight, true));
    }

    #[test]
    fn windows_terminal_scrolls_every_line_of_a_jittery_swipe() {
        let swipe = swipe_down();
        let straight: Vec<(f64, f64)> = swipe.iter().map(|(_, dy)| (0.0, *dy)).collect();
        let want = terminal_lines(&wheels(&mut core(), &straight, true));
        // About 207 points are 621 units: five whole notches down.
        assert_eq!(want, (-5, 0), "the straight swipe");
        assert_eq!(terminal_lines(&wheels(&mut core(), &swipe, true)), want, "lines lost to the sideways jitter");
    }

    #[test]
    fn a_gesture_keeps_its_axis_to_the_end_and_the_next_gesture_picks_its_own() {
        let mut c = core();
        // Down, then a slow wobble as wide as the motion: still the vertical wheel only.
        let mut down = vec![(0.2, -2.0), (-0.3, -2.0)];
        down.extend([(0.8, -1.0), (-0.8, -1.0), (0.8, -1.0)]);
        let wire = wheels(&mut c, &down, true);
        assert_eq!(sum(&wire), (0, -21));
        // A new gesture to the right: the horizontal wheel only (dx flips: content to the left is `dx > 0`).
        let wire = wheels(&mut c, &[(-3.0, 0.4), (-3.0, -0.4), (-3.0, 0.4)], true);
        assert_eq!(sum(&wire), (27, 0));
        // A diagonal gesture: both, as long as it lasts.
        let wire = wheels(&mut c, &[(2.0, 2.0), (2.0, 1.5), (0.1, 2.0)], true);
        assert_eq!(sum(&wire), (-12, 16));
    }

    #[test]
    fn without_gesture_phases_each_event_keeps_to_its_own_axis() {
        // A deliberate diagonal: both axes go.
        let wire = wheels(&mut core(), &[(4.0, 3.0); 10], false);
        assert_eq!(sum(&wire), (-120, 90), "dx flips, dy keeps its sign");
        // A sideways swipe with vertical jitter: only the horizontal wheel goes.
        let side: Vec<(f64, f64)> = (0..20).map(|i| (5.0, if i % 2 == 0 { 0.6 } else { -0.4 })).collect();
        let wire = wheels(&mut core(), &side, false);
        assert!(wire.iter().all(|w| w.1 == 0), "{wire:?}");
        assert_eq!(sum(&wire).0, -300);
    }

    #[test]
    fn scrolling_outside_the_picture_sends_nothing() {
        let mut c = core();
        let out = c.mouse(
            mouse(MouseKind::Scroll { dx: 0.0, dy: 5.0, precise: false, starts: false }, 5000.0, 0.0),
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
            mouse(MouseKind::Scroll { dx: f64::NAN, dy: f64::INFINITY, precise: true, starts: true }, 1.0, 1.0),
            Some(&identity_map),
            1.0,
        );
    }

    fn pinch(c: &mut InputCore, amount: f64, ends: bool) -> Vec<InputEvent> {
        c.mouse(mouse(MouseKind::Magnify { amount, ends }, 100.0, 100.0), Some(&identity_map), 2.0).events
    }

    fn toward_windows() -> InputCore {
        InputCore::new(InputConfig { pinch_zoom: true, swap_ctrl_cmd: true, ..InputConfig::default() })
    }

    fn pinch_wheels(events: &[InputEvent]) -> Vec<i16> {
        events.iter().filter_map(|e| if let InputEvent::Wheel { dx: 0, dy } = e { Some(*dy) } else { None }).collect()
    }

    /// A pinch toward a Windows host is what its precision touchpad sends: Ctrl held and the wheel (spreading the
    /// fingers zooms in, away from the user), the Ctrl let go at the gesture's end. Ctrl stays Ctrl with the swap on.
    #[test]
    fn a_pinch_toward_windows_is_ctrl_and_the_wheel_and_the_ctrl_goes_up_at_its_end() {
        let mut c = toward_windows();
        let first = pinch(&mut c, 0.1, false);
        assert_eq!(first[0], k(0xE0, true, modifier::LCTRL), "{first:?}");
        assert!(matches!(first[1], InputEvent::Pointer { .. }), "{first:?}");
        assert_eq!(pinch_wheels(&first), vec![60]);
        assert_eq!(pinch(&mut c, 0.05, false), vec![InputEvent::Wheel { dx: 0, dy: 30 }], "no second Ctrl");
        assert_eq!(pinch_wheels(&pinch(&mut c, -0.02, false)), vec![-12], "pinching back zooms out");
        assert!(!c.is_idle(), "the host holds our Ctrl");
        assert_eq!(pinch(&mut c, 0.0, true), vec![k(0xE0, false, 0)]);
        assert!(c.is_idle());
        // The next pinch presses its own Ctrl again.
        assert_eq!(pinch(&mut c, 0.1, false)[0], k(0xE0, true, modifier::LCTRL));
    }

    #[test]
    fn small_pinch_steps_add_up_and_a_pinch_with_no_whole_unit_presses_nothing() {
        let mut c = toward_windows();
        assert_eq!(pinch(&mut c, 0.001, false), vec![], "0.6 unit");
        assert_eq!(pinch(&mut c, 0.0, true), vec![], "no Ctrl went down, none goes up");
        let mut d = toward_windows();
        assert!(pinch(&mut d, 0.001, false).is_empty());
        assert_eq!(pinch_wheels(&pinch(&mut d, 0.001, false)), vec![1], "1.2 units: one whole");
    }

    /// Command held on the Mac is Ctrl on the host (the swap): the pinch then uses that Ctrl and leaves it held.
    #[test]
    fn a_pinch_under_a_ctrl_the_host_already_holds_presses_and_releases_no_ctrl_of_its_own() {
        let mut c = toward_windows();
        let held = c.modifiers_changed(CMD, &none).events;
        assert_eq!(held, vec![k(0xE0, true, modifier::LCTRL)]);
        assert_eq!(pinch_wheels(&pinch(&mut c, 0.1, false)), vec![60]);
        let end = pinch(&mut c, 0.0, true);
        assert!(end.is_empty(), "{end:?}");
    }

    #[test]
    fn a_pinch_toward_a_mac_host_sends_nothing() {
        let mut c = core();
        assert_eq!(pinch(&mut c, 0.3, false), vec![]);
        assert_eq!(pinch(&mut c, 0.0, true), vec![]);
        assert!(c.is_idle());
    }

    /// The keyboard went away mid-pinch: the host let go of everything, so the gesture's end sends no Ctrl release,
    /// and a pinch wheel the full queue refused rides on the pinch's next event, not on a later scroll.
    #[test]
    fn a_pinch_cut_by_a_release_all_ends_quietly_and_a_refused_pinch_wheel_stays_in_the_pinch() {
        let mut c = toward_windows();
        assert!(!pinch(&mut c, 0.1, false).is_empty());
        assert_eq!(c.release_all(), vec![InputEvent::ReleaseAll]);
        assert_eq!(pinch(&mut c, 0.0, true), vec![]);
        let mut d = toward_windows();
        assert_eq!(pinch_wheels(&pinch(&mut d, 0.1, false)), vec![60]);
        d.refund_wheel(0, 60);
        assert_eq!(pinch_wheels(&pinch(&mut d, 0.01, false)), vec![66]);
        let scroll = d.mouse(
            mouse(MouseKind::Scroll { dx: 0.0, dy: 0.0, precise: true, starts: true }, 100.0, 100.0),
            Some(&identity_map),
            2.0,
        );
        assert!(scroll.events.is_empty(), "{:?}", scroll.events);
    }
}
