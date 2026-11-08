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
