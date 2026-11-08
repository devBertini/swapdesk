//! H.264 `VideoEncoder` and `VideoDecoder` for Windows (contract C3, `docs/CONTRACTS.md`) over Media Foundation: the
//! hardware MFT of the capture adapter (NVIDIA, Intel Quick Sync, found with `MFTEnum2` and the adapter LUID) or the
//! Windows software MFT (D4b: no openh264). On an NVIDIA adapter the encoder is NVENC called directly (D21, module `nvenc`;
//! [`H264Encoder`] makes the choice). Input frames reach the encoder as D3D11 textures converted from BGRA to NV12 by
//! the D3D11 video processor; the decoder writes D3D11 NV12 textures (DXVA) and can read a BGRA copy back for tests.
//!
//! [`annexb`] and [`preset`] are pure and build on every host; the Media Foundation and D3D11 code lives behind
//! `cfg(windows)`. This crate holds the `unsafe` of the feature; every other crate forbids it. Objects are created on the
//! thread that uses them (C3 thread rules): none of the types here is `Send`.

pub mod annexb;
pub mod preset;

#[cfg(windows)]
mod auto;
#[cfg(windows)]
mod decoder;
#[cfg(windows)]
mod encoder;
#[cfg(windows)]
mod events;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod mf;
#[cfg(windows)]
mod mft;
#[cfg(windows)]
mod nvenc;
#[cfg(windows)]
mod nvenc_sys;
#[cfg(windows)]
mod probe;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
pub use auto::{ChosenBackend, EncoderBackend, H264Encoder, H264EncoderOptions, VENDOR_NVIDIA};
#[cfg(windows)]
pub use decoder::{DecoderOptions, DecoderOutput, GPU_RING, MfH264Decoder};
#[cfg(windows)]
pub use encoder::{EncoderOptions, FrameTiming, MfH264Encoder, MftChoice, MftSummary, PropertySupport, SettingOutcome};
#[cfg(windows)]
pub use gpu::TextureUploader;
#[cfg(windows)]
pub use nvenc::{NvencEncoder, NvencOptions, NvencSummary, NvencTuning};
#[cfg(windows)]
pub use nvenc_sys::{API_MAJOR as NVENC_HEADER_MAJOR, API_MINOR as NVENC_HEADER_MINOR};
#[cfg(windows)]
pub use probe::{DecoderProbe, EncoderProbe, probe_decoders, probe_encoders};
#[cfg(windows)]
pub use sys::{AdapterDesc, list_adapters};
