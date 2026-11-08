//! `InputSink` over an [`Injector`]: the logic of F-INPUT-01 to F-INPUT-03 (modifier and lock sync, held-key tracking,
//! release-all, pointer placement), independent of how the events reach the OS. [`crate::WinInputSink`] is this sink
//! over `SendInput`; the tests below run it over a recording injector.

use crate::plan::{MouseInput, RawInput, key_input, text_inputs, wheel_inputs};
use crate::pointer::{display_pixel, union_of, virtual_normalized};
use std::collections::BTreeSet;
use swapdesk_keys::{PAGE_KEYBOARD, modifier};
use swapdesk_platform::{DisplayInfo, ErrorKind, InputSink, PlatformError, Rect, Result};
use swapdesk_proto::{InputEvent, MouseButton};

/// Where planned inputs end up.
pub trait Injector {
    /// Delivers the inputs in order, as one batch where the OS allows it.
    fn send(&mut self, inputs: &[RawInput]) -> Result<()>;
    /// The virtual desktop in physical pixels (`SM_*VIRTUALSCREEN`); `None` falls back to the union of the displays.
    fn virtual_desktop(&self) -> Option<Rect>;
    /// Fresh toggle state of Caps, Num and Scroll Lock on this machine (bit 0, 1, 2), or `None` when it cannot be read.
    /// The sink calls it only when the viewer's locks differ from what it last knew.
    fn lock_state(&self) -> Option<u8>;
    /// Moves the calling thread to the input desktop.
    fn follow_input_desktop(&mut self) -> Result<()>;
}

pub(crate) fn invalid(op: &'static str, detail: String) -> PlatformError {
    PlatformError { kind: ErrorKind::InvalidInput, op, os_code: None, detail }
}

const LOCK_USAGES: [(u8, u16); 3] = [(1, 0x39), (2, 0x53), (4, 0x47)];

/// The sink. One instance lives in each helper (D11); the session layer picks which one serves a session.
pub struct InputSinkCore<I: Injector> {
    injector: I,
    displays: Vec<DisplayInfo>,
    held_keys: BTreeSet<(u16, u16)>,
    held_buttons: BTreeSet<u8>,
    /// The host's lock toggles as last read or set by this sink; `None` until read, and after a lock key was pressed.
    known_locks: Option<u8>,
}

impl<I: Injector> InputSinkCore<I> {
    pub fn new(injector: I) -> Self {
        Self {
            injector,
            displays: Vec::new(),
            held_keys: BTreeSet::new(),
            held_buttons: BTreeSet::new(),
            known_locks: None,
        }
    }

    pub fn injector(&self) -> &I {
        &self.injector
    }

    pub fn injector_mut(&mut self) -> &mut I {
        &mut self.injector
    }

    /// Keys currently held down by this sink, as `(usage_page, usage)`.
    pub fn held_keys(&self) -> Vec<(u16, u16)> {
        self.held_keys.iter().copied().collect()
    }

    /// Mouse buttons currently held down by this sink (1 to 5).
    pub fn held_buttons(&self) -> Vec<u8> {
        self.held_buttons.iter().copied().collect()
    }

    fn key(&self, page: u16, usage: u16, down: bool) -> Result<RawInput> {
        key_input(page, usage, down)
            .map(RawInput::Key)
            .ok_or_else(|| invalid("InputSink.apply", format!("no Windows key for usage {page:#x}/{usage:#x}")))
    }

    fn apply_key(&mut self, page: u16, usage: u16, down: bool, modifiers: u8, locks: u8) -> Result<()> {
        let id = swapdesk_keys::canonical(page, usage);
        let mut batch: Vec<RawInput> = Vec::new();
        let mut pressed: Vec<(u16, u16)> = Vec::new();
        let mut released: Vec<(u16, u16)> = Vec::new();
        if down {
            // Locks first, so the case of the letter that follows is the viewer's. The host is read only when the viewer
            // differs from what the sink last knew (a read costs a thread), then the differing locks are toggled.
            let lock_key_bit = LOCK_USAGES.iter().find(|(_, u)| id == (PAGE_KEYBOARD, *u)).map_or(0, |(b, _)| *b);
            let wanted = locks & 0b111;
            if self.known_locks.is_none_or(|k| (k ^ wanted) & !lock_key_bit != 0) {
                self.known_locks = self.injector.lock_state();
            }
            if let Some(host) = self.known_locks {
                let mut now = host;
                for (bit, lock_usage) in LOCK_USAGES {
                    if bit != lock_key_bit && (host ^ wanted) & bit != 0 {
                        batch.push(self.key(PAGE_KEYBOARD, lock_usage, true)?);
                        batch.push(self.key(PAGE_KEYBOARD, lock_usage, false)?);
                        now ^= bit;
                    }
                }
                self.known_locks = Some(now);
            }
            if lock_key_bit != 0 {
                // The lock key flips its own toggle: read again next time.
                self.known_locks = None;
            }
            // Then modifiers: the record carries the viewer's state after the event (7.3). A modifier key's own record is
            // applied as it is, so its bit is not synced here.
            let own = modifier::bit_of(id.0, id.1).unwrap_or(0);
            for i in 0..8u8 {
                let bit = 1u8 << i;
                if bit == own {
                    continue;
                }
                let usage_m = 0xE0 + u16::from(i);
                let held = self.held_keys.contains(&(PAGE_KEYBOARD, usage_m));
                let wanted = modifiers & bit != 0;
                if wanted && !held {
                    batch.push(self.key(PAGE_KEYBOARD, usage_m, true)?);
                    pressed.push((PAGE_KEYBOARD, usage_m));
                } else if !wanted && held {
                    batch.push(self.key(PAGE_KEYBOARD, usage_m, false)?);
                    released.push((PAGE_KEYBOARD, usage_m));
                }
            }
        }
        batch.push(self.key(id.0, id.1, down)?);
        if let Err(e) = self.injector.send(&batch) {
            self.known_locks = None;
            return Err(e);
        }
        for k in pressed {
            self.held_keys.insert(k);
        }
        for k in released {
            self.held_keys.remove(&k);
        }
        if down {
            self.held_keys.insert(id);
        } else {
            self.held_keys.remove(&id);
        }
        Ok(())
    }

    fn apply_pointer(&mut self, display: u16, x: u16, y: u16) -> Result<()> {
        let d = self
            .displays
            .iter()
            .find(|d| d.id.0 == display)
            .ok_or_else(|| invalid("InputSink.apply", format!("unknown display {display}")))?;
        let (px, py) = display_pixel(d.bounds, x, y);
        let desk = self
            .injector
            .virtual_desktop()
            .or_else(|| union_of(&self.displays))
            .ok_or_else(|| invalid("InputSink.apply", "no virtual desktop".to_owned()))?;
        let (nx, ny) = virtual_normalized(desk, px, py);
        self.injector.send(&[RawInput::Mouse(MouseInput::MoveAbs { x: nx, y: ny })])
    }
}

impl<I: Injector> InputSink for InputSinkCore<I> {
    fn displays_changed(&mut self, displays: &[DisplayInfo]) {
        self.displays = displays.to_vec();
    }

    fn apply(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::Key { usage_page, usage, down, modifiers, locks } => {
                self.apply_key(*usage_page, *usage, *down, *modifiers, *locks)
            }
            InputEvent::Text(text) => {
                let inputs: Vec<RawInput> = text_inputs(text).into_iter().map(RawInput::Key).collect();
                if inputs.is_empty() {
                    return Ok(());
                }
                self.injector.send(&inputs)
            }
            InputEvent::Pointer { display, x, y } => self.apply_pointer(*display, *x, *y),
            InputEvent::Button { button, down } => {
                self.injector.send(&[RawInput::Mouse(MouseInput::Button { button: *button, down: *down })])?;
                let n = button_number(*button);
                if *down {
                    self.held_buttons.insert(n);
                } else {
                    self.held_buttons.remove(&n);
                }
                Ok(())
            }
            InputEvent::Wheel { dx, dy } => {
                let inputs: Vec<RawInput> = wheel_inputs(*dx, *dy).into_iter().map(RawInput::Mouse).collect();
                if inputs.is_empty() {
                    return Ok(());
                }
                self.injector.send(&inputs)
            }
            InputEvent::ReleaseAll => self.release_all(),
        }
    }

    fn release_all(&mut self) -> Result<()> {
        let mut batch: Vec<RawInput> = Vec::new();
        // Most recently pressed last in the set's order does not matter: every key goes up.
        for (page, usage) in self.held_keys.iter().rev() {
            if let Some(k) = key_input(*page, *usage, false) {
                batch.push(RawInput::Key(k));
            }
        }
        for n in self.held_buttons.iter().rev() {
            if let Some(button) = button_from_number(*n) {
                batch.push(RawInput::Mouse(MouseInput::Button { button, down: false }));
            }
        }
        // The tracking is cleared even when the OS refuses: a stuck record would keep releasing forever.
        self.held_keys.clear();
        self.held_buttons.clear();
        if batch.is_empty() {
            return Ok(());
        }
        self.injector.send(&batch)
    }

    fn follow_input_desktop(&mut self) -> Result<()> {
        self.injector.follow_input_desktop()
    }
}

fn button_number(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 3,
        MouseButton::Back => 4,
        MouseButton::Forward => 5,
    }
}

fn button_from_number(n: u8) -> Option<MouseButton> {
    Some(match n {
        1 => MouseButton::Left,
        2 => MouseButton::Right,
        3 => MouseButton::Middle,
        4 => MouseButton::Back,
        5 => MouseButton::Forward,
        _ => return None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::plan::{KbInput, KbKind};
    use swapdesk_platform::{AdapterId, DisplayId};

    /// Records every batch; locks and the virtual desktop are settable.
    #[derive(Default)]
    pub(crate) struct Recorder {
        pub batches: Vec<Vec<RawInput>>,
        pub desk: Option<Rect>,
        pub locks: Option<u8>,
        pub fail: bool,
        pub reads: std::cell::Cell<u32>,
    }

    impl Injector for Recorder {
        fn send(&mut self, inputs: &[RawInput]) -> Result<()> {
            if self.fail {
                return Err(invalid("test", "refused".into()));
            }
            self.batches.push(inputs.to_vec());
            Ok(())
        }
        fn virtual_desktop(&self) -> Option<Rect> {
            self.desk
        }
        fn lock_state(&self) -> Option<u8> {
            self.reads.set(self.reads.get() + 1);
            self.locks
        }
        fn follow_input_desktop(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn sink() -> InputSinkCore<Recorder> {
        InputSinkCore::new(Recorder { locks: Some(0), ..Recorder::default() })
    }

    fn all(s: &InputSinkCore<Recorder>) -> Vec<RawInput> {
        s.injector().batches.iter().flatten().copied().collect()
    }

    fn scan(code: u8, extended: bool, up: bool) -> RawInput {
        RawInput::Key(KbInput { kind: KbKind::Scan { code, extended }, up })
    }

    fn key(usage: u16, down: bool, mods: u8, locks: u8) -> InputEvent {
        InputEvent::Key { usage_page: 7, usage, down, modifiers: mods, locks }
    }

    #[test]
    fn plain_key_down_and_up_inject_one_scan_code_each() {
        let mut s = sink();
        s.apply(&key(0x04, true, 0, 0)).unwrap();
        assert_eq!(s.held_keys(), vec![(7, 0x04)]);
        s.apply(&key(0x04, false, 0, 0)).unwrap();
        assert!(s.held_keys().is_empty());
        assert_eq!(all(&s), vec![scan(0x1E, false, false), scan(0x1E, false, true)]);
    }

    #[test]
    fn modifiers_are_pressed_before_the_key_and_released_when_the_record_drops_them() {
        let mut s = sink();
        s.apply(&key(0x06, true, modifier::LCTRL | modifier::LSHIFT, 0)).unwrap();
        assert_eq!(
            all(&s),
            vec![scan(0x1D, false, false), scan(0x2A, false, false), scan(0x2E, false, false)],
            "Ctrl, Shift, then C"
        );
        s.injector_mut().batches.clear();
        // The viewer released Shift while holding Ctrl and pressed V: Shift goes up first.
        s.apply(&key(0x19, true, modifier::LCTRL, 0)).unwrap();
        assert_eq!(all(&s), vec![scan(0x2A, false, true), scan(0x2F, false, false)]);
        assert_eq!(s.held_keys(), vec![(7, 0x06), (7, 0x19), (7, 0xE0)], "C is still down");
    }

    #[test]
    fn a_modifier_records_own_bit_is_not_pressed_twice() {
        let mut s = sink();
        s.apply(&key(0xE1, true, modifier::LSHIFT, 0)).unwrap();
        assert_eq!(all(&s), vec![scan(0x2A, false, false)]);
        s.apply(&key(0xE1, false, 0, 0)).unwrap();
        assert_eq!(all(&s).len(), 2);
        assert!(s.held_keys().is_empty());
    }

    #[test]
    fn altgr_is_one_extended_right_alt_with_no_ctrl_from_the_sink() {
        let mut s = sink();
        s.apply(&key(0xE6, true, modifier::RALT, 0)).unwrap();
        s.apply(&key(0x14, true, modifier::RALT, 0)).unwrap();
        s.apply(&key(0x14, false, modifier::RALT, 0)).unwrap();
        s.apply(&key(0xE6, false, 0, 0)).unwrap();
        assert_eq!(
            all(&s),
            vec![scan(0x38, true, false), scan(0x10, false, false), scan(0x10, false, true), scan(0x38, true, true)]
        );
        assert!(!all(&s).contains(&scan(0x1D, false, false)), "no Ctrl is injected");
    }

    #[test]
    fn locks_are_synced_before_a_key_but_not_for_the_lock_key_itself() {
        let mut s = InputSinkCore::new(Recorder { locks: Some(0b010), ..Recorder::default() });
        // Host: NumLock on. Viewer: Caps on, NumLock off.
        s.apply(&key(0x04, true, 0, 0b001)).unwrap();
        assert_eq!(
            all(&s),
            vec![
                scan(0x3A, false, false),
                scan(0x3A, false, true),
                scan(0x45, true, false),
                scan(0x45, true, true),
                scan(0x1E, false, false)
            ]
        );
        s.injector_mut().batches.clear();
        // The Caps Lock key itself: its record carries the new state, the sink only presses it.
        s.apply(&key(0x39, true, 0, 0b001)).unwrap();
        let sent = all(&s);
        assert_eq!(sent.iter().filter(|i| **i == scan(0x3A, false, false)).count(), 1);
    }

    #[test]
    fn the_host_is_read_only_when_the_viewer_differs_and_a_toggle_is_not_repeated() {
        // The recorder keeps answering "NumLock on", like a stale read would.
        let mut s = InputSinkCore::new(Recorder { locks: Some(0b010), ..Recorder::default() });
        s.apply(&key(0x04, true, 0, 0b010)).unwrap();
        assert_eq!(s.injector().reads.get(), 1, "first key reads the host");
        s.apply(&key(0x04, false, 0, 0b010)).unwrap();
        s.apply(&key(0x05, true, 0, 0b010)).unwrap();
        assert_eq!(s.injector().reads.get(), 1, "same locks: no new read");
        // The viewer turns NumLock off: one toggle, once.
        s.injector_mut().batches.clear();
        s.apply(&key(0x06, true, 0, 0)).unwrap();
        s.apply(&key(0x07, true, 0, 0)).unwrap();
        s.apply(&key(0x08, true, 0, 0)).unwrap();
        let toggles = all(&s).iter().filter(|i| **i == scan(0x45, true, false)).count();
        assert_eq!(toggles, 1, "NumLock toggled once, not before every key");
        // A lock key itself forces a fresh read next time.
        s.apply(&key(0x39, true, 0, 0b001)).unwrap();
        let reads = s.injector().reads.get();
        s.apply(&key(0x09, true, 0, 0b001)).unwrap();
        assert_eq!(s.injector().reads.get(), reads + 1);
    }

    #[test]
    fn an_unreadable_lock_state_syncs_nothing() {
        let mut s = InputSinkCore::new(Recorder::default());
        s.apply(&key(0x04, true, 0, 0b111)).unwrap();
        assert_eq!(all(&s), vec![scan(0x1E, false, false)]);
    }

    #[test]
    fn media_keys_go_by_virtual_key() {
        let mut s = sink();
        s.apply(&InputEvent::Key { usage_page: 0x0C, usage: 0xE9, down: true, modifiers: 0, locks: 0 }).unwrap();
        assert_eq!(
            all(&s),
            vec![RawInput::Key(KbInput { kind: KbKind::Vk { vk: 0xAF, scan: 0x30, extended: true }, up: false })]
        );
    }

    #[test]
    fn an_unknown_usage_is_invalid_input_and_changes_nothing() {
        let mut s = sink();
        let e = s.apply(&key(0xFFFF, true, 0, 0)).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert!(s.held_keys().is_empty() && all(&s).is_empty());
    }

    #[test]
    fn text_goes_as_one_batch_of_unicode_units() {
        let mut s = sink();
        s.apply(&InputEvent::Text("Ação".into())).unwrap();
        assert_eq!(s.injector().batches.len(), 1);
        assert_eq!(s.injector().batches[0].len(), 8);
        s.apply(&InputEvent::Text(String::new())).unwrap();
        assert_eq!(s.injector().batches.len(), 1, "empty text sends nothing");
    }

    #[test]
    fn buttons_and_wheel() {
        let mut s = sink();
        s.apply(&InputEvent::Button { button: MouseButton::Back, down: true }).unwrap();
        s.apply(&InputEvent::Wheel { dx: 120, dy: -240 }).unwrap();
        assert_eq!(s.held_buttons(), vec![4]);
        s.apply(&InputEvent::Button { button: MouseButton::Back, down: false }).unwrap();
        assert!(s.held_buttons().is_empty());
        assert_eq!(
            all(&s),
            vec![
                RawInput::Mouse(MouseInput::Button { button: MouseButton::Back, down: true }),
                RawInput::Mouse(MouseInput::Wheel(-240)),
                RawInput::Mouse(MouseInput::HWheel(120)),
                RawInput::Mouse(MouseInput::Button { button: MouseButton::Back, down: false }),
            ]
        );
    }

    #[test]
    fn release_all_lifts_every_held_key_and_button_and_forgets_them() {
        let mut s = sink();
        s.apply(&key(0xE0, true, modifier::LCTRL, 0)).unwrap();
        s.apply(&key(0x04, true, modifier::LCTRL, 0)).unwrap();
        s.apply(&key(0x52, true, modifier::LCTRL, 0)).unwrap();
        s.apply(&InputEvent::Button { button: MouseButton::Left, down: true }).unwrap();
        s.apply(&InputEvent::Button { button: MouseButton::Right, down: true }).unwrap();
        s.injector_mut().batches.clear();
        s.apply(&InputEvent::ReleaseAll).unwrap();
        let sent = all(&s);
        assert_eq!(sent.len(), 5);
        for expect in [scan(0x1D, false, true), scan(0x1E, false, true), scan(0x48, true, true)] {
            assert!(sent.contains(&expect), "{expect:?} missing");
        }
        assert!(sent.contains(&RawInput::Mouse(MouseInput::Button { button: MouseButton::Left, down: false })));
        assert!(sent.contains(&RawInput::Mouse(MouseInput::Button { button: MouseButton::Right, down: false })));
        assert!(s.held_keys().is_empty() && s.held_buttons().is_empty());
        s.injector_mut().batches.clear();
        s.release_all().unwrap();
        assert!(all(&s).is_empty(), "nothing held, nothing sent");
    }

    #[test]
    fn release_all_forgets_even_when_the_os_refuses() {
        let mut s = sink();
        s.apply(&key(0x04, true, 0, 0)).unwrap();
        s.injector_mut().fail = true;
        assert!(s.release_all().is_err());
        assert!(s.held_keys().is_empty());
    }

    fn display(id: u16, bounds: Rect) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: String::new(),
            bounds,
            scale_milli: 1000,
            refresh_mhz: 60_000,
            rotation: 0,
            primary: id == 1,
            adapter: AdapterId(0),
        }
    }

    #[test]
    fn pointer_lands_on_the_normalized_point_of_the_chosen_display() {
        let mut s = sink();
        let displays = vec![
            display(1, Rect { x: 0, y: 0, width: 1920, height: 1080 }),
            display(2, Rect { x: -2560, y: -200, width: 2560, height: 1440 }),
        ];
        s.displays_changed(&displays);
        s.injector_mut().desk = Some(Rect { x: -2560, y: -200, width: 4480, height: 1440 });
        s.apply(&InputEvent::Pointer { display: 2, x: 0, y: 0 }).unwrap();
        s.apply(&InputEvent::Pointer { display: 1, x: 65535, y: 65535 }).unwrap();
        let moves: Vec<MouseInput> = all(&s)
            .into_iter()
            .filter_map(|i| match i {
                RawInput::Mouse(m) => Some(m),
                _ => None,
            })
            .collect();
        assert_eq!(moves[0], MouseInput::MoveAbs { x: 0, y: 0 }, "top-left of the virtual desktop");
        let MouseInput::MoveAbs { x, y } = moves[1] else { panic!() };
        assert_eq!(crate::pointer::resolved_pixel(x, -2560, 4480), 1919);
        assert_eq!(crate::pointer::resolved_pixel(y, -200, 1440), 1079);
        let e = s.apply(&InputEvent::Pointer { display: 7, x: 1, y: 1 }).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
    }

    #[test]
    fn pointer_without_a_virtual_desktop_falls_back_to_the_union_of_the_displays() {
        let mut s = sink();
        s.displays_changed(&[display(1, Rect { x: -100, y: 0, width: 200, height: 100 })]);
        s.apply(&InputEvent::Pointer { display: 1, x: 0, y: 0 }).unwrap();
        assert_eq!(all(&s), vec![RawInput::Mouse(MouseInput::MoveAbs { x: 0, y: 0 })]);
    }

    #[test]
    fn displays_changed_replaces_the_list() {
        let mut s = sink();
        s.displays_changed(&[display(1, Rect { x: 0, y: 0, width: 100, height: 100 })]);
        s.displays_changed(&[display(2, Rect { x: 0, y: 0, width: 100, height: 100 })]);
        assert!(s.apply(&InputEvent::Pointer { display: 1, x: 0, y: 0 }).is_err());
        assert!(s.apply(&InputEvent::Pointer { display: 2, x: 0, y: 0 }).is_ok());
    }
}
