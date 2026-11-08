//! The viewer's keyboard (F-INPUT-01, F-INPUT-02, F-INPUT-04).
//!
//! Two paths feed the same decision logic (`swapdesk_input_win::grab::GrabState`, which holds the C4 tables, the
//! modifier and lock tracking, the physical-or-text router of `swapdesk-keys::mode` and the Ctrl/Cmd swap):
//!
//! - **"Capturar atalhos do sistema" on:** the system-wide keyboard hook (`WinGrabber`, started while the viewer
//!   window has the focus) sees every key, including Win and Alt+Tab, and keeps the system ones from the viewer's own
//!   OS. The window's key messages are then taken and ignored.
//! - **Off:** the window's own key messages (`WM_KEYDOWN`...) go through a `GrabState` here ([`WindowKeys`]); system
//!   shortcuts keep acting on the viewer's PC.
//!
//! Which keys are text depends on the keyboard mode setting and on whether the two keyboards' layouts match. The
//! contract has no way to learn the host's layout; the rule used is that a Windows host is assumed to have the
//! viewer's layout and a Mac host is not, so "Automático" sends text to a Mac and physical keys to a Windows PC.
//! The Ctrl/Cmd swap applies to Mac hosts only.

use swapdesk_input_win::grab::{GrabConfig, GrabState, Injected, LLKHF_EXTENDED, LLKHF_UP, LlKey, Translated};
use swapdesk_keys::mode::KeyboardMode as WireMode;
use swapdesk_proto::InputEvent;
use swapdesk_shell_win::KeyMsg;
use swapdesk_ui::{KeyboardMode, Os};

/// What decides how a key press is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyConfig {
    pub mode: KeyboardMode,
    pub host_os: Os,
    /// The "Trocar Ctrl e Cmd no Mac" setting (applies to a Mac host only).
    pub swap_ctrl_cmd: bool,
}

impl Default for KeyConfig {
    fn default() -> Self {
        Self { mode: KeyboardMode::Auto, host_os: Os::Windows, swap_ctrl_cmd: false }
    }
}

impl KeyConfig {
    pub fn grab_config(&self) -> GrabConfig {
        GrabConfig {
            mode: match self.mode {
                KeyboardMode::Auto => WireMode::Auto,
                KeyboardMode::Physical => WireMode::Physical,
                KeyboardMode::Text => WireMode::Text,
            },
            layouts_match: self.host_os == Os::Windows,
            swap_ctrl_cmd: self.swap_ctrl_cmd && self.host_os == Os::MacOs,
            injected: Injected::Ignore,
        }
    }
}

/// The window-message path.
pub struct WindowKeys {
    config: KeyConfig,
    state: Option<GrabState>,
}

impl WindowKeys {
    pub fn new(config: KeyConfig) -> Self {
        Self { config, state: None }
    }

    pub fn set_config(&mut self, config: KeyConfig) {
        self.config = config;
        if let Some(s) = self.state.as_mut() {
            s.set_config(config.grab_config());
        }
    }

    /// The records for one key message.
    pub fn on_key(&mut self, k: &KeyMsg) -> Vec<InputEvent> {
        let config = self.config;
        let state = self.state.get_or_insert_with(|| GrabState::new(config.grab_config(), k.locks));
        let flags = (if k.extended { LLKHF_EXTENDED } else { 0 }) | (if k.down { 0 } else { LLKHF_UP });
        let key = LlKey { vk: k.vk, scan: k.scan, flags };
        state.on_key(key, |_, _| Translated { text: k.text.clone(), dead: k.dead }).events
    }

    /// Focus lost or the session ended: forget every held key; `ReleaseAll` when any key was seen.
    pub fn reset(&mut self) -> Vec<InputEvent> {
        match self.state.take() {
            Some(_) => vec![InputEvent::ReleaseAll],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(vk: u32, scan: u32, extended: bool, down: bool, text: Option<&str>) -> KeyMsg {
        KeyMsg { vk, scan, extended, down, text: text.map(str::to_owned), dead: false, locks: 0 }
    }

    fn cfg(mode: KeyboardMode, host_os: Os) -> KeyConfig {
        KeyConfig { mode, host_os, swap_ctrl_cmd: false }
    }

    #[test]
    fn physical_mode_sends_the_key_even_when_the_layout_made_text() {
        let mut k = WindowKeys::new(cfg(KeyboardMode::Physical, Os::MacOs));
        let e = k.on_key(&key(0x41, 0x1E, false, true, Some("a")));
        assert_eq!(e, vec![InputEvent::Key { usage_page: 7, usage: 0x04, down: true, modifiers: 0, locks: 0 }]);
    }

    #[test]
    fn text_mode_sends_text_and_swallows_the_key_up() {
        let mut k = WindowKeys::new(cfg(KeyboardMode::Text, Os::Windows));
        assert_eq!(k.on_key(&key(0x41, 0x1E, false, true, Some("á"))), vec![InputEvent::Text("á".into())]);
        assert!(k.on_key(&key(0x41, 0x1E, false, false, None)).is_empty());
    }

    #[test]
    fn auto_is_physical_to_a_windows_host_and_text_to_a_mac() {
        let press = key(0x41, 0x1E, false, true, Some("a"));
        let mut win = WindowKeys::new(cfg(KeyboardMode::Auto, Os::Windows));
        assert!(matches!(win.on_key(&press).as_slice(), [InputEvent::Key { .. }]));
        let mut mac = WindowKeys::new(cfg(KeyboardMode::Auto, Os::MacOs));
        assert_eq!(mac.on_key(&press), vec![InputEvent::Text("a".into())]);
    }

    #[test]
    fn a_shortcut_is_physical_in_every_mode() {
        let mut k = WindowKeys::new(cfg(KeyboardMode::Text, Os::MacOs));
        // Left Ctrl down, then C: the layout says "\u{3}" (no text), the router must send the key.
        let ctrl = k.on_key(&key(0xA2, 0x1D, false, true, None));
        assert!(matches!(ctrl.as_slice(), [InputEvent::Key { usage: 0xE0, down: true, .. }]));
        let c = k.on_key(&key(0x43, 0x2E, false, true, Some("c")));
        assert!(matches!(c.as_slice(), [InputEvent::Key { usage: 0x06, down: true, modifiers: 1, .. }]), "{c:?}");
    }

    #[test]
    fn the_ctrl_cmd_swap_applies_to_a_mac_host_only() {
        let swapped = KeyConfig { mode: KeyboardMode::Physical, host_os: Os::MacOs, swap_ctrl_cmd: true };
        let mut mac = WindowKeys::new(swapped);
        assert!(
            matches!(mac.on_key(&key(0xA2, 0x1D, false, true, None)).as_slice(), [InputEvent::Key { usage: 0xE3, .. }]),
            "Ctrl becomes Cmd"
        );
        let mut win = WindowKeys::new(KeyConfig { host_os: Os::Windows, ..swapped });
        assert!(matches!(
            win.on_key(&key(0xA2, 0x1D, false, true, None)).as_slice(),
            [InputEvent::Key { usage: 0xE0, .. }]
        ));
    }

    #[test]
    fn extended_keys_use_the_extended_scan_code() {
        let mut k = WindowKeys::new(cfg(KeyboardMode::Physical, Os::Windows));
        assert!(
            matches!(k.on_key(&key(0x26, 0x48, true, true, None)).as_slice(), [InputEvent::Key { usage: 0x52, .. }]),
            "Up arrow"
        );
        assert!(
            matches!(k.on_key(&key(0x68, 0x48, false, true, None)).as_slice(), [InputEvent::Key { usage: 0x60, .. }]),
            "keypad 8"
        );
    }

    #[test]
    fn reset_releases_everything_once_a_key_was_seen() {
        let mut k = WindowKeys::new(cfg(KeyboardMode::Physical, Os::Windows));
        assert!(k.reset().is_empty());
        k.on_key(&key(0x41, 0x1E, false, true, None));
        assert_eq!(k.reset(), vec![InputEvent::ReleaseAll]);
        assert!(k.reset().is_empty());
    }
}
