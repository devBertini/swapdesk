//! The viewer's own shortcuts (D30) as the interface shows and runs them: the definitions, their names for the
//! settings screen, the hint of each button of the session bar, and the full-screen state. Recognising the keys is `swapdesk-keys`'s
//! `shortcuts` (on HID usages); the shells and the viewer pipeline hand the recognised [`ViewerAction`] to
//! [`UiState::run_shortcut`].
//!
//! Windows: release the keyboard Ctrl+Alt (configurable), show the session bar Ctrl+Alt+B, full screen F11,
//! Ctrl+Alt+Del with Ctrl+Alt+End, disconnect Ctrl+Alt+Q. macOS: Control+Option in place of Ctrl+Alt and
//! Control+Command+F for full screen. They stay with Swapdesk when "Capturar atalhos do sistema" is on.

use crate::api::{Command, Msg, SpecialKeys};
use crate::interact::ControlId;
use crate::state::{Activation, Changed, UiState};
use crate::strings::MsgKey;

/// What a viewer shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerAction {
    ReleaseKeyboard,
    ToggleBar,
    FullScreen,
    CtrlAltDel,
    Disconnect,
}

impl ViewerAction {
    pub const ALL: [ViewerAction; 5] = [
        ViewerAction::ReleaseKeyboard,
        ViewerAction::ToggleBar,
        ViewerAction::FullScreen,
        ViewerAction::CtrlAltDel,
        ViewerAction::Disconnect,
    ];

    fn label(self) -> MsgKey {
        match self {
            ViewerAction::ReleaseKeyboard => MsgKey::ScReleaseKeyboard,
            ViewerAction::ToggleBar => MsgKey::ScShowBar,
            ViewerAction::FullScreen => MsgKey::ScFullScreen,
            ViewerAction::CtrlAltDel => MsgKey::ScCtrlAltDel,
            ViewerAction::Disconnect => MsgKey::ScDisconnect,
        }
    }
}

/// Which chord releases the keyboard (a setting of the viewer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReleaseKey {
    #[default]
    CtrlAlt,
    RightCtrl,
    CtrlAltShift,
}

impl ReleaseKey {
    pub const ALL: [ReleaseKey; 3] = [ReleaseKey::CtrlAlt, ReleaseKey::RightCtrl, ReleaseKey::CtrlAltShift];
}

/// The viewer's own OS: it decides the names of the modifiers and the full-screen chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ViewerOs {
    #[default]
    Windows,
    MacOs,
}

impl UiState {
    pub fn viewer_os(&self) -> ViewerOs {
        self.viewer_os
    }

    /// The macOS shell says so at start; the Windows one keeps the default.
    pub fn set_viewer_os(&mut self, os: ViewerOs) {
        self.viewer_os = os;
    }

    pub fn release_key(&self) -> ReleaseKey {
        self.release_key
    }

    /// The release key of a viewer session is changed by the settings (it goes to the pipeline as a UI-local command).
    pub fn set_release_key(&mut self, key: ReleaseKey) -> Command {
        self.release_key = key;
        self.save_prefs();
        Command::SetReleaseKey { key }
    }

    /// The release chord as the interface writes it ("Ctrl+Alt", "Ctrl direito").
    pub fn release_key_name(&self, key: ReleaseKey) -> String {
        let mac = self.viewer_os == ViewerOs::MacOs;
        self.t(match (key, mac) {
            (ReleaseKey::CtrlAlt, false) => MsgKey::ReleaseCtrlAlt,
            (ReleaseKey::RightCtrl, false) => MsgKey::ReleaseRightCtrl,
            (ReleaseKey::CtrlAltShift, false) => MsgKey::ReleaseCtrlAltShift,
            (ReleaseKey::CtrlAlt, true) => MsgKey::ReleaseControlOption,
            (ReleaseKey::RightCtrl, true) => MsgKey::ReleaseRightControl,
            (ReleaseKey::CtrlAltShift, true) => MsgKey::ReleaseControlOptionShift,
        })
    }

    /// The keys of one shortcut, as written on screen.
    pub fn shortcut_keys(&self, action: ViewerAction) -> String {
        let mac = self.viewer_os == ViewerOs::MacOs;
        let prefix = if mac { "Control+Option" } else { "Ctrl+Alt" };
        match action {
            ViewerAction::ReleaseKeyboard => self.release_key_name(self.release_key),
            ViewerAction::ToggleBar => format!("{prefix}+B"),
            ViewerAction::FullScreen if mac => "Control+Command+F".to_owned(),
            ViewerAction::FullScreen => "F11".to_owned(),
            ViewerAction::CtrlAltDel => format!("{prefix}+End"),
            ViewerAction::Disconnect => format!("{prefix}+Q"),
        }
    }

    /// The shortcut list: the name of each action and its keys, in a fixed order.
    pub fn shortcut_rows(&self) -> Vec<(String, String)> {
        ViewerAction::ALL.iter().map(|a| (self.t(a.label()), self.shortcut_keys(*a))).collect()
    }

    /// What a button of the session bar does, in one line: the hint shown under the button while the pointer is on
    /// it, and its description for a screen reader. `None` for a control that is not a button of the bar. The
    /// shortcut list is in the settings, not here (D42).
    pub fn toolbar_hint(&self, id: ControlId) -> Option<String> {
        let key = match id {
            ControlId::TbScreen => MsgKey::TipScreen,
            ControlId::TbFit => MsgKey::TipFit,
            ControlId::TbAudio => MsgKey::TipAudio,
            ControlId::TbGrab => MsgKey::TipGrab,
            ControlId::TbKeys => MsgKey::TbKeys,
            ControlId::TbTerminal => MsgKey::TipTerminal,
            ControlId::TbFiles => MsgKey::TipFiles,
            ControlId::TbChat => MsgKey::TipChat,
            ControlId::TbSwap => MsgKey::TipSwap,
            ControlId::TbElevate => MsgKey::ElevateHint,
            ControlId::TbDisconnect => {
                let keys = self.shortcut_keys(ViewerAction::Disconnect);
                return Some(self.tm(&Msg::new(MsgKey::TipDisconnect).with(keys)));
            }
            ControlId::TbCollapse => {
                let keys = self.shortcut_keys(ViewerAction::ToggleBar);
                return Some(self.tm(&Msg::new(MsgKey::TipCollapse).with(keys)));
            }
            _ => return None,
        };
        Some(self.t(key))
    }

    // ----- full screen -----

    /// The viewer is full screen (only while a session shows).
    pub fn fullscreen(&self) -> bool {
        self.fullscreen && self.viewer_session().is_some()
    }

    /// The session bar (the toolbar) shows: always in a window, only when asked in full screen.
    pub fn session_bar_visible(&self) -> bool {
        (!self.fullscreen() || self.bar_shown) && !self.bar_collapsed
    }

    /// The handle of a collapsed session bar shows where the bar would: in a window, or in full screen once the bar was
    /// brought up (full screen starts with nothing over the picture, D30).
    pub fn bar_handle_visible(&self) -> bool {
        (!self.fullscreen() || self.bar_shown) && self.bar_collapsed
    }

    /// The monitor full screen goes to (an index of the shell's monitor list); `None` is the one the window is on.
    pub fn fullscreen_monitor(&self) -> Option<u8> {
        self.fullscreen_monitor
    }

    /// How many monitors the shell found (it says so when it knows); the setting offers one entry for each.
    pub fn set_local_monitors(&mut self, count: u8) {
        self.local_monitors = count.max(1);
        if self.fullscreen_monitor.is_some_and(|m| m >= self.local_monitors) {
            self.fullscreen_monitor = None;
        }
    }

    pub fn local_monitors(&self) -> u8 {
        self.local_monitors
    }

    /// The options of the full-screen monitor select, the current one marked.
    pub fn fullscreen_monitor_options(&self) -> Vec<(String, bool)> {
        let mut v = vec![(self.t(MsgKey::FullscreenMonitorAuto), self.fullscreen_monitor.is_none())];
        // One monitor leaves nothing to choose: only "where the window is".
        let listed = if self.local_monitors < 2 { 0 } else { self.local_monitors };
        for n in 0..listed {
            let name = self.tm(&Msg::new(MsgKey::FullscreenMonitorN).with((n + 1).to_string()));
            v.push((name, self.fullscreen_monitor == Some(n)));
        }
        v
    }

    pub fn choose_fullscreen_monitor(&mut self, index: usize) -> Activation {
        self.fullscreen_monitor =
            index.checked_sub(1).and_then(|n| u8::try_from(n).ok()).filter(|n| *n < self.local_monitors);
        Activation { changed: Changed::ALL, ..Activation::default() }
    }

    /// One of the viewer's shortcuts fired. `ReleaseKeyboard` is the pipeline's alone (the keyboard is not the state's),
    /// the rest act on the session or the window.
    pub fn run_shortcut(&mut self, action: ViewerAction) -> Activation {
        if self.viewer_session().is_none() {
            return Activation::default();
        }
        match action {
            ViewerAction::ReleaseKeyboard => Activation::default(),
            ViewerAction::FullScreen => {
                self.fullscreen = !self.fullscreen;
                self.bar_shown = false;
                Activation { changed: Changed::VIEWER, ..Activation::default() }
            }
            // A collapsed bar comes back; in a window an open bar collapses (B22); in full screen it shows or hides.
            ViewerAction::ToggleBar => {
                if self.bar_collapsed {
                    self.bar_collapsed = false;
                    if self.fullscreen() {
                        self.bar_shown = true;
                    }
                } else if self.fullscreen() {
                    self.bar_shown = !self.bar_shown;
                } else {
                    self.bar_collapsed = true;
                }
                Activation { changed: Changed::VIEWER, ..Activation::default() }
            }
            ViewerAction::CtrlAltDel => self.send_keys(SpecialKeys::CtrlAltDel),
            ViewerAction::Disconnect => self.activate(ControlId::TbDisconnect),
        }
    }
}
