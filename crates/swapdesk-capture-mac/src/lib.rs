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
