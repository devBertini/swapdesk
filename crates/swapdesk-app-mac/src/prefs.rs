//! How the viewer's keyboard settings (the UI's `KeyboardMode`, "Trocar Ctrl e Cmd no Mac", the host's OS) become the
//! input lane's `InputConfig` (`swapdesk-input-mac`). Pure.
//!
//! Rules (the Mac side of the ones the Windows viewer applies, `swapdesk-app`'s `viewer::keys`):
//! - Layouts: the contract has no way to learn the host's layout. A Mac host is assumed to have this Mac's layout and a
//!   Windows host not, so "Automático" sends physical keys to a Mac and text to a Windows PC (the Windows viewer's rule
//!   mirrored: a Windows viewer assumes a Windows host matches and a Mac host does not).
//! - Ctrl and Cmd swap: it exists for a Mac keyboard driving a Windows PC (Cmd+C then arrives as Ctrl+C). Between two
//!   Macs the shortcuts already agree, so it never applies to a Mac host.

use swapdesk_input_mac::InputConfig;
use swapdesk_keys::KbKind;
use swapdesk_keys::mode::KeyboardMode as WireMode;
use swapdesk_ui::{KeyboardMode, Os};

/// What decides how a key press is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyPrefs {
    pub mode: KeyboardMode,
    pub host_os: Os,
    /// The "Trocar Ctrl e Cmd no Mac" setting.
    pub swap_ctrl_cmd: bool,
}

impl Default for KeyPrefs {
    fn default() -> Self {
        Self { mode: KeyboardMode::Auto, host_os: Os::Windows, swap_ctrl_cmd: false }
    }
}

pub fn wire_mode(m: KeyboardMode) -> WireMode {
    match m {
        KeyboardMode::Auto => WireMode::Auto,
        KeyboardMode::Physical => WireMode::Physical,
        KeyboardMode::Text => WireMode::Text,
    }
}

/// The input lane's configuration for `prefs`, the remote `display` the pointer addresses and this Mac's keyboard kind.
pub fn input_config(prefs: &KeyPrefs, display: u16, keyboard: KbKind) -> InputConfig {
    InputConfig {
        mode: wire_mode(prefs.mode),
        layouts_match: prefs.host_os == Os::MacOs,
        swap_ctrl_cmd: prefs.swap_ctrl_cmd && prefs.host_os == Os::Windows,
        keyboard,
        display,
        ..InputConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_map_one_to_one() {
        assert_eq!(wire_mode(KeyboardMode::Auto), WireMode::Auto);
        assert_eq!(wire_mode(KeyboardMode::Physical), WireMode::Physical);
        assert_eq!(wire_mode(KeyboardMode::Text), WireMode::Text);
    }

    #[test]
    fn a_windows_host_gets_text_in_auto_and_the_swap_a_mac_host_gets_neither() {
        let win = input_config(
            &KeyPrefs { mode: KeyboardMode::Auto, host_os: Os::Windows, swap_ctrl_cmd: true },
            3,
            KbKind::Iso,
        );
        assert!(!win.layouts_match && win.swap_ctrl_cmd);
        assert_eq!((win.display, win.keyboard), (3, KbKind::Iso));
        let mac = input_config(
            &KeyPrefs { mode: KeyboardMode::Physical, host_os: Os::MacOs, swap_ctrl_cmd: true },
            0,
            KbKind::Ansi,
        );
        assert!(mac.layouts_match && !mac.swap_ctrl_cmd);
        assert_eq!(mac.mode, WireMode::Physical);
    }
}
