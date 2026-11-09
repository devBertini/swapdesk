//! Host capture for macOS (contract C3, stage 24) over ScreenCaptureKit:
//!
//! * [`screen`]: `ScreenSource` (one `SCStream` per opened display, NV12 `CVPixelBuffer`s handed on as
//!   `Pixels::Gpu` for `swapdesk-codec-mac`'s encoder) and the cursor (`take_cursor`).
//! * [`audio`]: `AudioSource` (an audio-only `SCStream`: system audio, 48 kHz stereo, this process excluded).
//!
//! Both need the Screen Recording grant (TCC). This crate holds the `unsafe` of the feature; the pure pieces (pixel
//! and sample conversions, coordinate maps) build and are unit tested on every host.

pub mod audio;
pub mod screen;

/// The Screen Recording grant, without prompting (`CGPreflightScreenCaptureAccess`).
#[cfg(target_os = "macos")]
pub fn screen_access() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
}

/// Asks macOS for the Screen Recording grant (`CGRequestScreenCaptureAccess`): the first call shows the system's prompt
/// pointing to System Settings > Privacy & Security > Screen Recording; later calls return the current state without
/// asking again. The app calls it once at start when it hosts and the grant is missing.
#[cfg(target_os = "macos")]
pub fn request_screen_access() -> bool {
    objc2_core_graphics::CGRequestScreenCaptureAccess()
}
