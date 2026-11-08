//! The encoder choice of D21: [`H264Encoder`] picks NVENC when the capture adapter's vendor is NVIDIA (`0x10DE`) and
//! NVENC opens, and the Media Foundation path ([`MfH264Encoder`], Quick Sync and the software MFT) otherwise. The
//! Media Foundation encoder keeps its own API untouched; this type only decides which of the two serves a session.

use swapdesk_platform::{EncodedFrame, EncoderConfig, EncoderInfo, ErrorKind, Frame, Result, VideoEncoder};

use crate::encoder::{EncoderOptions, MfH264Encoder, MftChoice};
use crate::nvenc::{NvencEncoder, NvencOptions, NvencSummary};
use crate::sys::{self, device_adapter, err};

/// PCI vendor id of NVIDIA.
pub const VENDOR_NVIDIA: u32 = 0x10DE;

/// Which encoder family may serve a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderBackend {
    /// NVENC on an NVIDIA adapter when it opens, Media Foundation otherwise (also when NVENC fails to open).
    #[default]
    Auto,
    /// NVENC only; no fallback.
    Nvenc,
    /// Media Foundation only (what `MfH264Encoder` does alone).
    MediaFoundation,
}

/// The family that serves the configured session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChosenBackend {
    Nvenc,
    MediaFoundation,
}

#[derive(Clone, Default)]
pub struct H264EncoderOptions {
    pub backend: EncoderBackend,
    /// Options of the Media Foundation path; its device, GOP and intra-refresh settings also go to NVENC. A `choice` other
    /// than [`MftChoice::Auto`] names an MFT, so `Auto` then does not try NVENC.
    pub mf: EncoderOptions,
    /// The NVENC driver library name; `None` is `nvEncodeAPI64.dll`.
    pub nvenc_library: Option<String>,
}

enum Inner {
    None,
    Nvenc(Box<NvencEncoder>),
    Mf(Box<MfH264Encoder>),
}

/// H.264 encoder that is NVENC on NVIDIA adapters and Media Foundation elsewhere. Not `Send`.
pub struct H264Encoder {
    options: H264EncoderOptions,
    inner: Inner,
    fallback_reason: Option<String>,
}

impl H264Encoder {
    pub fn new(options: H264EncoderOptions) -> Self {
        Self { options, inner: Inner::None, fallback_reason: None }
    }

    /// The family serving the session, once configured.
    pub fn backend(&self) -> Option<ChosenBackend> {
        match self.inner {
            Inner::None => None,
            Inner::Nvenc(_) => Some(ChosenBackend::Nvenc),
            Inner::Mf(_) => Some(ChosenBackend::MediaFoundation),
        }
    }

    /// Why `Auto` did not use NVENC on an NVIDIA adapter (NVENC failed to open), if that happened.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    /// NVENC session details when NVENC serves the session.
    pub fn nvenc(&self) -> Option<&NvencSummary> {
        match &self.inner {
            Inner::Nvenc(e) => e.summary(),
            _ => None,
        }
    }

    /// The Media Foundation encoder when it serves the session.
    pub fn mf(&self) -> Option<&MfH264Encoder> {
        match &self.inner {
            Inner::Mf(e) => Some(e),
            _ => None,
        }
    }

    /// Frames per second and kbps the active encoder was told.
    pub fn effective(&self) -> Option<(u32, u32)> {
        match &self.inner {
            Inner::None => None,
            Inner::Nvenc(e) => e.effective(),
            Inner::Mf(e) => e.effective(),
        }
    }

    fn nvenc_options(&self) -> NvencOptions {
        NvencOptions {
            device: self.options.mf.device.clone(),
            gop_frames: self.options.mf.gop_frames,
            intra_refresh_frames: self.options.mf.intra_refresh_frames,
            library: self.options.nvenc_library.clone(),
            tuning: None,
        }
    }

    /// The vendor of the adapter the session runs on (the capture device's, else the configured adapter).
    fn adapter_vendor(&self, config: &EncoderConfig) -> Result<(u32, bool)> {
        let desc = match &self.options.mf.device {
            Some(d) => {
                sys::init_thread()?;
                device_adapter(d)?
            }
            None => sys::find_adapter(config.adapter)?.1,
        };
        Ok((desc.vendor_id, desc.software))
    }
}

impl VideoEncoder for H264Encoder {
    fn configure(&mut self, config: &EncoderConfig) -> Result<EncoderInfo> {
        self.inner = Inner::None;
        self.fallback_reason = None;
        let try_nvenc = match self.options.backend {
            EncoderBackend::MediaFoundation => false,
            EncoderBackend::Nvenc => true,
            EncoderBackend::Auto => {
                let (vendor, software) = self.adapter_vendor(config)?;
                vendor == VENDOR_NVIDIA && !software && self.options.mf.choice == MftChoice::Auto
            }
        };
        if try_nvenc {
            let mut nv = Box::new(NvencEncoder::new(self.nvenc_options()));
            match nv.configure(config) {
                Ok(info) => {
                    self.inner = Inner::Nvenc(nv);
                    return Ok(info);
                }
                Err(e) if self.options.backend == EncoderBackend::Nvenc || e.kind == ErrorKind::InvalidInput => {
                    return Err(e);
                }
                Err(e) => self.fallback_reason = Some(e.to_string()),
            }
        }
        let mut mf = Box::new(MfH264Encoder::new(self.options.mf.clone()));
        let info = mf.configure(config)?;
        self.inner = Inner::Mf(mf);
        Ok(info)
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        match &mut self.inner {
            Inner::None => Err(err(ErrorKind::Closed, "VideoEncoder::encode", "configure was not called")),
            Inner::Nvenc(e) => e.encode(frame, force_keyframe),
            Inner::Mf(e) => e.encode(frame, force_keyframe),
        }
    }

    fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()> {
        match &mut self.inner {
            Inner::None => Err(err(ErrorKind::Closed, "VideoEncoder::set_rate", "configure was not called")),
            Inner::Nvenc(e) => e.set_rate(kbps, fps),
            Inner::Mf(e) => e.set_rate(kbps, fps),
        }
    }

    fn refine(&mut self, frame: &Frame) -> Result<Vec<EncodedFrame>> {
        match &mut self.inner {
            Inner::None => Err(err(ErrorKind::Closed, "VideoEncoder::refine", "configure was not called")),
            Inner::Nvenc(e) => e.refine(frame),
            Inner::Mf(e) => e.refine(frame),
        }
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        match &mut self.inner {
            Inner::None => Ok(Vec::new()),
            Inner::Nvenc(e) => e.flush(),
            Inner::Mf(e) => e.flush(),
        }
    }
}
