//! What the viewer window reports to the input lane (`swapdesk-input-mac`): key, modifier, mouse and focus events of the
//! video view, as plain data. The shell translates `NSEvent`s into these; the input lane implements [`ViewerInput`].
//!
//! Every method is called on the main thread and must return quickly (queue the work, never block). Coordinates are in
//! points of the video view with the origin at its top-left corner (the view is flipped); the renderer lane maps them
//! to the remote display.

/// Modifier keys held, as a bit set (`NSEventModifierFlags` reduced to what the remote side needs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const SHIFT: Modifiers = Modifiers(1);
    pub const CONTROL: Modifiers = Modifiers(2);
    pub const OPTION: Modifiers = Modifiers(4);
    pub const COMMAND: Modifiers = Modifiers(8);
    pub const CAPS_LOCK: Modifiers = Modifiers(16);
    pub const FUNCTION: Modifiers = Modifiers(32);

    pub const fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn union(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 | other.0)
    }
}

/// A key press or release in the video view. `key_code` is the hardware virtual key code of `NSEvent.keyCode`
/// (layout independent); `text` is `NSEvent.characters` for a press that produces text (empty for releases, and for
/// keys with no text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMsg {
    pub key_code: u16,
    pub down: bool,
    pub repeat: bool,
    pub text: Option<String>,
    pub modifiers: Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    /// `NSEvent.buttonNumber` 3 and up.
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseKind {
    Move,
    Button {
        button: PointerButton,
        down: bool,
    },
    /// Motion with a button held.
    Drag {
        button: PointerButton,
    },
    /// `precise` is true for trackpads (pixel deltas); a wheel gives lines. Positive `dy` scrolls content up (away).
    Scroll {
        dx: f64,
        dy: f64,
        precise: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseMsg {
    pub kind: MouseKind,
    pub x: f64,
    pub y: f64,
    pub modifiers: Modifiers,
}

/// Whether the video view has the keyboard, and what the input lane was last told about it.
///
/// The view has the keyboard while its window is the key window and the view is that window's first responder. AppKit
/// reports the two halves through different calls: the view becoming or resigning first responder, and the window
/// becoming or resigning key (another window of the app, another app after Command+Tab, Spotlight, a system panel), and
/// the input lane may be attached after the view already became first responder (the viewer window makes it first
/// responder before the shell hands it the input). Every one of those calls feeds this latch; the input lane hears
/// each change once: `focus(false)` makes it release every key and button on the host, so no modifier is left held
/// there when the keys' releases go to another window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FocusLatch {
    told: bool,
}

impl FocusLatch {
    /// The new state of the view; `Some(focused)` when the input lane must be told.
    pub fn update(&mut self, key_window: bool, first_responder: bool) -> Option<bool> {
        let now = key_window && first_responder;
        (std::mem::replace(&mut self.told, now) != now).then_some(now)
    }

    /// The input lane is detached: `Some(false)` when it was told it had the keyboard (tell it it lost it).
    pub fn detach(&mut self) -> Option<bool> {
        std::mem::take(&mut self.told).then_some(false)
    }

    pub fn focused(&self) -> bool {
        self.told
    }
}

/// Implemented by the input lane. All methods have conservative defaults so a partial implementation compiles.
pub trait ViewerInput: Send + Sync {
    /// The video view gained or lost the keyboard focus (the window became key or resigned).
    fn focus(&self, _focused: bool) {}
    /// True while the pipeline wants the key events (the shell then also offers shortcut key equivalents first).
    fn captures_keyboard(&self) -> bool {
        false
    }
    /// True when the key was taken; the shell then keeps it from the rest of AppKit.
    fn key(&self, _msg: &KeyMsg) -> bool {
        false
    }
    /// The modifier set changed (`flagsChanged:`); true when taken.
    fn modifiers_changed(&self, _modifiers: Modifiers) -> bool {
        false
    }
    /// True when the message was taken.
    fn mouse(&self, _msg: MouseMsg) -> bool {
        false
    }
    /// True when the local pointer is hidden over the video (the remote cursor is drawn instead).
    fn hides_pointer(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_are_a_bit_set() {
        let m = Modifiers::SHIFT.union(Modifiers::COMMAND);
        assert!(m.contains(Modifiers::SHIFT) && m.contains(Modifiers::COMMAND));
        assert!(!m.contains(Modifiers::OPTION));
        assert!(m.contains(Modifiers::default()));
    }

    #[test]
    fn the_input_attached_after_the_view_became_first_responder_hears_the_focus() {
        // The viewer window makes the video view first responder, then the shell hands it the input: the input
        // starts unfocused and must be told now (it never was before: no focus, no release on focus loss).
        let mut latch = FocusLatch::default();
        assert_eq!(latch.update(true, true), Some(true));
        assert!(latch.focused());
        assert_eq!(latch.update(true, true), None, "told once");
    }

    #[test]
    fn the_window_resigning_key_with_the_view_still_first_responder_takes_the_keyboard_away() {
        // Command+Tab to another app: the video view stays its window's first responder, the window resigns key.
        let mut latch = FocusLatch::default();
        latch.update(true, true);
        assert_eq!(latch.update(false, true), Some(false), "the input releases every key on the host");
        assert_eq!(latch.update(true, true), Some(true), "back with Command+Tab");
        assert_eq!(latch.update(true, false), Some(false), "a text field of the drawer took the keyboard");
        assert_eq!(latch.update(false, false), None);
    }

    #[test]
    fn detaching_tells_a_focused_input_it_lost_the_keyboard_once() {
        let mut latch = FocusLatch::default();
        assert_eq!(latch.detach(), None, "never focused: nothing to release");
        latch.update(true, true);
        assert_eq!(latch.detach(), Some(false));
        assert_eq!(latch.detach(), None);
        assert_eq!(latch.update(true, true), Some(true), "a new input hears the focus again");
    }

    struct Nothing;
    impl ViewerInput for Nothing {}

    #[test]
    fn the_default_input_takes_nothing() {
        let n = Nothing;
        let k = KeyMsg { key_code: 0, down: true, repeat: false, text: None, modifiers: Modifiers::default() };
        assert!(!n.key(&k) && !n.captures_keyboard() && !n.hides_pointer());
        assert!(!n.mouse(MouseMsg { kind: MouseKind::Move, x: 1.0, y: 2.0, modifiers: Modifiers::default() }));
    }
}
