//! The viewer's own shortcuts (D30), recognised on HID usages and the HID modifier byte, so the Windows hook, the
//! Windows window messages and the macOS tap can share one matcher.
//!
//! Windows: release the keyboard with Ctrl+Alt (configurable: right Ctrl, Ctrl+Alt+Shift), show the session bar with
//! Ctrl+Alt+B, full screen with F11, Ctrl+Alt+Del with Ctrl+Alt+End, disconnect with Ctrl+Alt+Q. macOS: Control+Option in
//! place of Ctrl+Alt and Control+Command+F for full screen.
//!
//! The release key is a chord of modifiers alone, so it cannot fire when its keys go down (Ctrl+Alt is also the start of
//! every other shortcut): it arms when the chord is complete and fires when one of its keys is let go, if no other key
//! was pressed in between.

use crate::modifier::{LALT, LCTRL, LGUI, LSHIFT, RALT, RCTRL, RGUI, RSHIFT};
use crate::{PAGE_KEYBOARD, modifier};

/// What a shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerAction {
    /// Give the keyboard back to this PC until the next click on the picture.
    ReleaseKeyboard,
    /// Show or hide the session bar.
    ToggleBar,
    FullScreen,
    CtrlAltDel,
    Disconnect,
}

/// Which chord releases the keyboard (a setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReleaseKey {
    #[default]
    CtrlAlt,
    RightCtrl,
    CtrlAltShift,
}

/// The viewer's own OS: it decides the modifier names and the full-screen chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerOs {
    Windows,
    MacOs,
}

const USAGE_F: u16 = 0x09;
const USAGE_B: u16 = 0x05;
const USAGE_Q: u16 = 0x14;
const USAGE_F11: u16 = 0x44;
const USAGE_END: u16 = 0x4D;

const CTRL: u8 = LCTRL | RCTRL;
const SHIFT: u8 = LSHIFT | RSHIFT;
const GUI: u8 = LGUI | RGUI;

/// What the matcher says about one key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Matched {
    /// The key is a shortcut: it is not for the host, and its release is not either.
    pub consumed: bool,
    pub action: Option<ViewerAction>,
}

#[derive(Debug, Clone)]
pub struct ShortcutMatcher {
    os: ViewerOs,
    release: ReleaseKey,
    /// The release chord is complete and nothing else was pressed since.
    armed: bool,
}

impl ShortcutMatcher {
    pub fn new(os: ViewerOs, release: ReleaseKey) -> Self {
        Self { os, release, armed: false }
    }

    pub fn set_release(&mut self, release: ReleaseKey) {
        self.release = release;
        self.armed = false;
    }

    pub fn reset(&mut self) {
        self.armed = false;
    }

    /// The modifiers the shortcut prefix needs: Ctrl+Alt on Windows, Control+Option on macOS.
    fn prefix(&self, mods: u8) -> bool {
        // On Windows the right Alt is AltGr (Ctrl+Alt on some layouts): only the left Alt counts there.
        let alt = if self.os == ViewerOs::Windows { LALT } else { LALT | RALT };
        mods & CTRL != 0 && mods & alt != 0
    }

    /// Whether the modifier byte holds the release chord.
    fn release_complete(&self, mods: u8) -> bool {
        match self.release {
            ReleaseKey::CtrlAlt => self.prefix(mods),
            ReleaseKey::RightCtrl => mods & RCTRL != 0,
            ReleaseKey::CtrlAltShift => self.prefix(mods) && mods & SHIFT != 0,
        }
    }

    /// One keyboard-page key event. `mods` is the modifier byte after the event; `repeat` marks an auto-repeat.
    pub fn on_key(&mut self, usage: u16, down: bool, repeat: bool, mods: u8) -> Matched {
        if modifier::bit_of(PAGE_KEYBOARD, usage).is_some() {
            return self.on_modifier(down, mods);
        }
        if !down {
            // Releases of consumed keys are decided by the caller (it remembers which presses it consumed).
            return Matched::default();
        }
        self.armed = false;
        let action = self.action_of(usage, mods);
        Matched { consumed: action.is_some(), action: if repeat { None } else { action } }
    }

    fn on_modifier(&mut self, down: bool, mods: u8) -> Matched {
        if down {
            self.armed = self.release_complete(mods);
            return Matched::default();
        }
        if self.armed && !self.release_complete(mods) {
            self.armed = false;
            return Matched { consumed: false, action: Some(ViewerAction::ReleaseKeyboard) };
        }
        Matched::default()
    }

    fn action_of(&self, usage: u16, mods: u8) -> Option<ViewerAction> {
        match self.os {
            ViewerOs::Windows => {
                if usage == USAGE_F11 && mods & (CTRL | GUI | LALT | RALT) == 0 {
                    return Some(ViewerAction::FullScreen);
                }
                if !self.prefix(mods) {
                    return None;
                }
                match usage {
                    USAGE_B => Some(ViewerAction::ToggleBar),
                    USAGE_END => Some(ViewerAction::CtrlAltDel),
                    USAGE_Q => Some(ViewerAction::Disconnect),
                    _ => None,
                }
            }
            ViewerOs::MacOs => {
                if usage == USAGE_F && mods & CTRL != 0 && mods & GUI != 0 {
                    return Some(ViewerAction::FullScreen);
                }
                if !self.prefix(mods) {
                    return None;
                }
                match usage {
                    USAGE_B => Some(ViewerAction::ToggleBar),
                    USAGE_END => Some(ViewerAction::CtrlAltDel),
                    USAGE_Q => Some(ViewerAction::Disconnect),
                    _ => None,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(release: ReleaseKey) -> ShortcutMatcher {
        ShortcutMatcher::new(ViewerOs::Windows, release)
    }

    #[test]
    fn the_chords_of_d30_on_windows() {
        let mut m = win(ReleaseKey::CtrlAlt);
        let ca = LCTRL | LALT;
        assert_eq!(m.on_key(USAGE_B, true, false, ca).action, Some(ViewerAction::ToggleBar));
        assert_eq!(m.on_key(USAGE_END, true, false, ca).action, Some(ViewerAction::CtrlAltDel));
        assert_eq!(m.on_key(USAGE_Q, true, false, ca).action, Some(ViewerAction::Disconnect));
        assert_eq!(m.on_key(USAGE_F11, true, false, 0).action, Some(ViewerAction::FullScreen));
        // Right Ctrl works as the Ctrl of the prefix; a bare letter does nothing.
        assert_eq!(m.on_key(USAGE_B, true, false, RCTRL | LALT).action, Some(ViewerAction::ToggleBar));
        assert_eq!(m.on_key(USAGE_B, true, false, 0), Matched::default());
        assert_eq!(m.on_key(USAGE_B, true, false, LCTRL), Matched::default());
        // AltGr (right Alt) is text on many layouts, not the prefix.
        assert_eq!(m.on_key(USAGE_B, true, false, LCTRL | RALT), Matched::default());
        // F11 with Ctrl or Alt held belongs to the host.
        assert_eq!(m.on_key(USAGE_F11, true, false, LCTRL), Matched::default());
    }

    #[test]
    fn a_repeat_is_consumed_but_does_not_act_again() {
        let mut m = win(ReleaseKey::CtrlAlt);
        let r = m.on_key(USAGE_F11, true, true, 0);
        assert_eq!(r, Matched { consumed: true, action: None });
    }

    #[test]
    fn macos_uses_control_option_and_control_command_f() {
        let mut m = ShortcutMatcher::new(ViewerOs::MacOs, ReleaseKey::CtrlAlt);
        assert_eq!(m.on_key(USAGE_F, true, false, LCTRL | LGUI).action, Some(ViewerAction::FullScreen));
        assert_eq!(m.on_key(USAGE_F, true, false, LCTRL).action, None);
        assert_eq!(m.on_key(USAGE_F11, true, false, 0).action, None, "F11 is the Windows chord only");
        assert_eq!(m.on_key(USAGE_Q, true, false, LCTRL | RALT).action, Some(ViewerAction::Disconnect));
        assert_eq!(m.on_key(USAGE_B, true, false, LCTRL | LALT).action, Some(ViewerAction::ToggleBar));
    }

    /// Presses the modifiers in order, releases them in order; the actions fired on the way.
    fn chord(m: &mut ShortcutMatcher, press: &[u16], release: &[u16]) -> Vec<ViewerAction> {
        let mut mods = 0u8;
        let mut out = Vec::new();
        for &u in press {
            mods = modifier::after(mods, PAGE_KEYBOARD, u, true);
            out.extend(m.on_key(u, true, false, mods).action);
        }
        for &u in release {
            mods = modifier::after(mods, PAGE_KEYBOARD, u, false);
            out.extend(m.on_key(u, false, false, mods).action);
        }
        out
    }

    #[test]
    fn ctrl_alt_alone_releases_the_keyboard_when_let_go() {
        let mut m = win(ReleaseKey::CtrlAlt);
        // Nothing fires while the keys go down, so Ctrl+Alt+B can still follow.
        assert!(chord(&mut m, &[0xE0, 0xE2], &[]).is_empty());
        assert_eq!(
            chord(&mut ShortcutMatcher::new(ViewerOs::Windows, ReleaseKey::CtrlAlt), &[0xE0, 0xE2], &[0xE2, 0xE0]),
            [ViewerAction::ReleaseKeyboard]
        );
    }

    #[test]
    fn a_key_between_the_modifiers_cancels_the_release() {
        let mut m = win(ReleaseKey::CtrlAlt);
        let mut mods = 0u8;
        for u in [0xE0, 0xE2] {
            mods = modifier::after(mods, PAGE_KEYBOARD, u, true);
            m.on_key(u, true, false, mods);
        }
        assert_eq!(m.on_key(USAGE_B, true, false, mods).action, Some(ViewerAction::ToggleBar));
        mods = modifier::after(mods, PAGE_KEYBOARD, 0xE2, false);
        assert_eq!(m.on_key(0xE2, false, false, mods).action, None);
    }

    #[test]
    fn right_ctrl_and_ctrl_alt_shift_are_the_other_release_keys() {
        let mut m = win(ReleaseKey::RightCtrl);
        assert_eq!(chord(&mut m, &[0xE4], &[0xE4]), [ViewerAction::ReleaseKeyboard]);
        assert!(chord(&mut m, &[0xE0], &[0xE0]).is_empty(), "left Ctrl is not the key");
        let mut m = win(ReleaseKey::CtrlAltShift);
        assert!(chord(&mut m, &[0xE0, 0xE2], &[0xE2, 0xE0]).is_empty(), "Ctrl+Alt is not enough");
        assert_eq!(chord(&mut m, &[0xE0, 0xE2, 0xE1], &[0xE1, 0xE2, 0xE0]), [ViewerAction::ReleaseKeyboard]);
        m.set_release(ReleaseKey::CtrlAlt);
        assert_eq!(chord(&mut m, &[0xE0, 0xE2], &[0xE0, 0xE2]), [ViewerAction::ReleaseKeyboard]);
    }
}
