//! H.264 `VideoDecoder` and `VideoEncoder` for macOS (contract C3, `docs/CONTRACTS.md`) over VideoToolbox ([`encoder`]
//! holds the encoder and its pure preset/level/rate rules, which build on every host). Annex B access units come in (the wire format of PROTOCOL.md 7.1), pictures go out as `CVPixelBuffer`s
//! (NV12, IOSurface-backed) for `swapdesk-render-mac`, or as BGRA bytes for tests.
//!
//! [`annexb`] (NAL scan, parameter sets, the Annex B to AVCC conversion, access-unit splitting) is pure and builds on
//! every host; the VideoToolbox code lives behind `cfg(target_os = "macos")`. This crate holds the `unsafe` of the
//! feature; every other crate forbids it. Decoders are created on the thread that uses them: nothing here is `Send`.
//!
//! Keyframe handling follows `swapdesk-codec-win`: after `reset`, a hole in the frame ids or a decode error the decoder
//! drops every non-keyframe and reports [`VtH264Decoder::waiting_for_keyframe`]; the video receiver (`swapdesk-video`)
//! is what asks the host for the keyframe.

pub mod annexb;
pub mod encoder;

#[cfg(target_os = "macos")]
mod decoder;
#[cfg(target_os = "macos")]
mod sys;

#[cfg(target_os = "macos")]
pub use decoder::{DecoderOptions, DecoderOutput, VtH264Decoder};
#[cfg(target_os = "macos")]
pub use encoder::{DEFAULT_WAIT, OutputWait, VtH264Encoder};
