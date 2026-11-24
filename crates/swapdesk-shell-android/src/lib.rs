//! The Android shell of the shared interface (docs/ANDROID.md 5): the screens of `swapdesk-paint`, the same ones
//! Windows and the Mac paint, shown by a Java view that only replays what this crate recorded.
//!
//! * [`recorder`]: the `Painter` that writes a frame's operations into one buffer ([`canvas_ops`]);
//! * [`text_measure`]: text measured by Android in one round trip per new label, never by a call from Rust into
//!   Java;
//! * [`phone_window`]: one window's passes: a frame, or the list of text it needs measured first.
//! * [`phone_shell`]: the shell around the shared state (a finger's press, release and drag, the way back, the
//!   backend, the asks only Android can answer), with [`phone_effects`] as its table of effects.
//!
//! Plain Rust, built and tested on every host: the Java half (`PaintView`, `TextMeasurer`) follows the two formats.

pub mod canvas_ops;
pub mod phone_effects;
pub mod phone_shell;
pub mod phone_window;
pub mod recorder;
pub mod text_measure;

#[cfg(test)]
mod phone_shell_tests;
#[cfg(test)]
mod recorder_tests;

pub use phone_shell::{Ask, PhoneShell};
pub use phone_window::{FrameCost, PASSES_MAX, PhoneWindow, Surface};
pub use recorder::{Metrics, Recorder};
pub use text_measure::{TextCache, TextKey};
