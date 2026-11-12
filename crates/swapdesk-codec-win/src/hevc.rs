//! Whether this PC can encode and decode HEVC in hardware (D39, F-VIEW-07), found once by trying: a hardware HEVC
//! encoder MFT or an NVENC HEVC session on a hardware adapter, and an HEVC decoder MFT that runs on the GPU (it takes the
//! D3D11 device manager) on an adapter whose driver lists the HEVC Main profile for DXVA (`D3D11_DECODER_PROFILE_HEVC_VLD_MAIN`
//! with `DXGI_FORMAT_NV12`). The answer is cached for the life of the process: HEVC is advertised only when it is true.

use std::sync::OnceLock;

use swapdesk_platform::{Codec, EncoderConfig, ErrorKind, VideoEncoder};
use swapdesk_proto::Preset;
use windows::Win32::Graphics::Direct3D11::{D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, ID3D11VideoDevice};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::core::Interface;

use crate::decoder::{DecoderOptions, MfH264Decoder};
use crate::encoder::{EncoderOptions, MfH264Encoder, MftChoice};
use crate::gpu::Gpu;
use crate::nvenc::{NvencEncoder, NvencOptions};
use crate::sys::{AdapterDesc, list_adapters};

/// What the probe found, with the reasons for what it did not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HevcProbe {
    /// Hardware HEVC encoders that configured, one line each ("NVIDIA NVENC HEVC on <adapter>").
    pub encoders: Vec<String>,
    /// HEVC decoders that run on the GPU of an adapter whose driver lists the HEVC Main profile.
    pub decoders: Vec<String>,
    /// Why each other attempt failed (diagnostics for the trace; never shown to a person).
    pub refusals: Vec<String>,
}

impl HevcProbe {
    /// A hardware HEVC encoder exists.
    pub fn encode_hw(&self) -> bool {
        !self.encoders.is_empty()
    }

    /// A hardware HEVC decoder exists.
    pub fn decode_hw(&self) -> bool {
        !self.decoders.is_empty()
    }
}

/// `(encode_hw, decode_hw)`: whether this PC encodes and decodes HEVC in hardware. Tries once, then answers from memory.
pub fn hevc_support() -> (bool, bool) {
    let p = hevc_probe();
    (p.encode_hw(), p.decode_hw())
}

/// The cached probe behind [`hevc_support`].
pub fn hevc_probe() -> &'static HevcProbe {
    static PROBE: OnceLock<HevcProbe> = OnceLock::new();
    PROBE.get_or_init(run)
}

/// 1280x720 is above every HEVC minimum size and small enough to configure quickly.
fn config(adapter: &AdapterDesc) -> EncoderConfig {
    EncoderConfig {
        width: 1280,
        height: 720,
        fps: 60,
        kbps: 10_000,
        preset: Preset::Auto,
        adapter: adapter.id,
        codec: Codec::Hevc,
    }
}

fn run() -> HevcProbe {
    let mut out = HevcProbe::default();
    let adapters = match list_adapters() {
        Ok(a) => a,
        Err(e) => {
            out.refusals.push(format!("adapters: {e}"));
            return out;
        }
    };
    for adapter in adapters.iter().filter(|a| !a.software) {
        let cfg = config(adapter);
        if adapter.vendor_id == crate::preset::VENDOR_NVIDIA {
            let mut nv = NvencEncoder::new(NvencOptions::default());
            match nv.configure(&cfg) {
                Ok(info) => out.encoders.push(info.name),
                Err(e) => out.refusals.push(format!("NVENC HEVC on {}: {e}", adapter.name)),
            }
        }
        let mut mf = MfH264Encoder::new(EncoderOptions { choice: MftChoice::HardwareOnly, ..Default::default() });
        match mf.configure(&cfg) {
            Ok(info) if info.hardware => out.encoders.push(format!("{} on {}", info.name, adapter.name)),
            Ok(info) => out.refusals.push(format!("{} on {}: not hardware", info.name, adapter.name)),
            Err(e) => out.refusals.push(format!("Media Foundation HEVC on {}: {e}", adapter.name)),
        }
        match decoder_on(adapter) {
            Ok(line) => out.decoders.push(line),
            Err(why) => out.refusals.push(format!("HEVC decoder on {}: {why}", adapter.name)),
        }
    }
    out
}

/// An HEVC decoder MFT that runs on this adapter's GPU, when the driver lists the HEVC Main profile for it.
fn decoder_on(adapter: &AdapterDesc) -> Result<String, String> {
    let gpu = Gpu::on_adapter(adapter.id).map_err(|e| e.to_string())?;
    let video: ID3D11VideoDevice = gpu.device.cast().map_err(|e| format!("no ID3D11VideoDevice: {e}"))?;
    // SAFETY: a capability query with a valid profile GUID.
    let listed = unsafe { video.CheckVideoDecoderFormat(&D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, DXGI_FORMAT_NV12) }
        .map(|b| b.as_bool())
        .map_err(|e| format!("CheckVideoDecoderFormat: {e}"))?;
    if !listed {
        return Err("the driver does not list the HEVC Main profile for DXVA".to_string());
    }
    let mut dec = MfH264Decoder::new(DecoderOptions {
        adapter: adapter.id,
        device: Some(gpu.device.clone()),
        ..Default::default()
    });
    match dec.probe_start(Codec::Hevc) {
        Ok(name) => Ok(format!("{name} on {}", adapter.name)),
        Err(e) if e.kind == ErrorKind::Unsupported => Err(e.to_string()),
        Err(e) => Err(format!("decoder MFT: {e}")),
    }
}
