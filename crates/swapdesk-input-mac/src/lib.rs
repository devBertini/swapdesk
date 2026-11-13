//! Viewer input for macOS (contract C3, stage 23; the sink and the clipboard are stage 24): the keyboard, pointer and
//! wheel of the video view as wire `InputEvent` records, and the `InputGrabber` that takes the system shortcuts.
//! This crate holds the `unsafe` of the feature.
//!
//! * [`MacInput`] implements the shell's `ViewerInput` (`swapdesk-shell-mac`): the shell turns `NSEvent`s into
//!   `KeyMsg`/`MouseMsg`; this turns those into records on the session's `SyncSender<InputEvent>` (F-INPUT-01 to 04).
//!   Physical keys (HID usages through the C4 tables, with the ISO section/grave swap for the viewer's keyboard and
//!   "Trocar Ctrl e Cmd no Mac"), Unicode text ("Texto"/"Automático"), absolute pointer positions through the
//!   renderer's `map_pointer`, buttons, and the wheel (trackpad pixels or mouse lines, in 1/120 notch). Focus loss
//!   sends `ReleaseAll`.
//! * [`MacGrabber`] (macOS only) implements `InputGrabber` over a `CGEventTap`: with "capturar atalhos do sistema" on
//!   and the viewer focused, Cmd+Tab, Cmd+Space, the Mission Control and Spaces shortcuts and the like go to the host
//!   instead of macOS. The tap needs Input Monitoring and Accessibility; [`MacInput::grab_status`] reports a missing
//!   grant as [`GrabStatus::NeedsPermission`] and nothing prompts or retries by itself.
//!
//! The decisions are in [`logic`] and [`keymap`], plain data that build and are unit tested on every host (the Windows
//! counterpart is `swapdesk-input-win`'s `grab`); the AppKit-facing glue is [`input`]; the macOS calls are in
//! `platform`.

pub mod clip_logic;
#[cfg(target_os = "macos")]
mod clipboard;
pub mod input;
pub mod keymap;
pub mod logic;
#[cfg(target_os = "macos")]
mod platform;
#[cfg(target_os = "macos")]
mod sink;
pub mod sink_logic;

#[cfg(target_os = "macos")]
pub use clipboard::MacClipboard;
#[cfg(target_os = "macos")]
pub use sink::{MacInputSink, can_post, posted_key_presses};

pub use input::{Access, GrabStatus, KeyboardState, MacInput, NoKeyboardState, PointerMap};
pub use logic::{EventText, InputConfig, InputCore, Layout, Outcome, ScrollConfig, Typed};
#[cfg(target_os = "macos")]
pub use platform::{MacGrabber, MacLayout, SystemKeyboardState, access, detect_keyboard_kind, request_access};

impl InputConfig {
    /// The defaults with the keyboard kind of this Mac (ANSI on other hosts).
    pub fn detected() -> Self {
        #[cfg(target_os = "macos")]
        let keyboard = detect_keyboard_kind();
        #[cfg(not(target_os = "macos"))]
        let keyboard = swapdesk_keys::KbKind::Ansi;
        Self { keyboard, ..Self::default() }
    }
}
