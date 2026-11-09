//! Input for Windows (contract C3): the `InputSink` over `SendInput` and the `InputGrabber` over a `WH_KEYBOARD_LL`
//! hook (F-INPUT-01 to F-INPUT-05). This crate holds the `unsafe` of the feature.
//!
//! The pure parts (what to inject for a key, text or wheel record, the pointer geometry, the sink's modifier and lock
//! sync and held-key tracking) build and are unit tested on every host; the Win32 calls live in the Windows modules.
//!
//! Where it runs (D11): one [`WinInputSink`] in each helper. The user helper's sink (the logged-in user's medium-integrity
//! token) serves prompted sessions and the system helper's sink serves only `UNATTENDED` ones; the session layer picks
//! the sink, never the sink itself. The grabber runs in the viewer, on its own thread, with no window.

pub mod clipboard;
pub mod grab;
pub mod plan;
pub mod pointer;
pub mod sink;

#[cfg(windows)]
mod hook;
#[cfg(windows)]
mod winsend;

#[cfg(windows)]
pub use hook::{ShortcutHandler, WinGrabber};
#[cfg(windows)]
pub use winsend::{SendInputInjector, virtual_screen};

/// The sink over `SendInput`.
#[cfg(windows)]
pub type WinInputSink = sink::InputSinkCore<SendInputInjector>;

/// A sink over `SendInput` on the calling thread's desktop.
#[cfg(windows)]
pub fn open_sink() -> WinInputSink {
    sink::InputSinkCore::new(SendInputInjector::new())
}
