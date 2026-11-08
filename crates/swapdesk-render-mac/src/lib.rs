//! `Renderer` for macOS (contract C3, `docs/CONTRACTS.md`): decoded `CVPixelBuffer`s (from `swapdesk-codec-mac`) or CPU
//! BGRA frames are enqueued on an `AVSampleBufferDisplayLayer`, through its `sampleBufferRenderer` with
//! `DisplayImmediately`, inside a container layer that this crate inserts at index 0 of the shell's parent layer, so the
//! shell's toolbar layer (a sibling above) draws over the video (D18). One window, no child `NSView`.
//!
//! [`layout`] (Fit, 1:1, scroll, letterbox, the pointer mapping) is pure and builds on every host; the AVFoundation and
//! Core Animation code lives behind `cfg(target_os = "macos")`. This crate holds the `unsafe` of the feature; every
//! other crate forbids it. A renderer is created on the shell's thread and used only from it: nothing here is `Send`.

pub mod layout;

#[cfg(target_os = "macos")]
mod renderer;
#[cfg(target_os = "macos")]
mod sys;

#[cfg(target_os = "macos")]
pub use renderer::{Counters, MacRenderer, RendererOptions};
#[cfg(target_os = "macos")]
pub use sys::now;
