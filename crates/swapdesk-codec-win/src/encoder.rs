//! `VideoEncoder` over a Media Foundation H.264 encoder MFT.
//!
//! Frames reach the MFT as NV12: BGRA (CPU rows or a D3D11 texture) goes through the D3D11 video processor into a pool of
//! NV12 textures; a hardware MFT takes those textures (DXGI buffers), the software MFT takes a system-memory copy. Hardware
//! MFTs are asynchronous (`METransformNeedInput` / `METransformHaveOutput` through [`crate::events`]); the software MFT is
//! synchronous. One input gives at most one output (no B-frames, low latency): `encode` waits for it up to
//! [`EncoderOptions::output_wait`].

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use swapdesk_platform::{
    AdapterId, EncodedFrame, EncoderConfig, EncoderInfo, ErrorKind, Frame, MonoMicros, Pixels, Result, VideoEncoder,
};
use swapdesk_proto::H264Profile;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_FLAG, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonAllowFrameDrops, CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonLowLatency,
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonQuality,
    CODECAPI_AVEncCommonQualityVsSpeed, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncCommonRealTime,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoEncodeQP,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVEncVideoGradualIntraRefresh, CODECAPI_AVEncVideoMaxNumRefFrame,
    CODECAPI_AVLowLatencyMode,
};
use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_LEVEL, MF_MT_MPEG2_PROFILE,
    MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX,
    MF_TRANSFORM_ASYNC_UNLOCK, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFNominalRange_16_235,
    MFSampleExtension_CleanPoint, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
};
use windows::core::{GUID, Interface};

use crate::events::EventPump;
use crate::gpu::{BGRA, Gpu, NV12, Space, Staging, USAGE_DEFAULT, VideoProc};
use crate::mf::{
    Output, OutputInfo, attrs_of, empty_sample, memory_sample, output_info, process_output, sample_bytes, set_bool,
    set_u32, video_type,
};
use crate::mft::{self, Category, Mft};
use crate::preset::{
    LiveRate, MAX_KBPS, RATE_CBR, effective, level_idc, live_rate, preset_plan, rate_limits, rate_modes_for,
};
use crate::sys::{Ctx, err, os_err};
use crate::{annexb, sys};

const POOL: usize = 4;
/// Longest wait for the output of a refinement picture: an IDR at a low QP is several times the size of a normal frame.
const REFINE_OUTPUT_WAIT: Duration = Duration::from_secs(1);
/// `eAVEncCommonRateControlMode_Quality`.
const RATE_QUALITY: u32 = 3;
/// The H.264 QP of a refinement picture in the quality mode (16 is the lowest `AVEncVideoEncodeQP` the Microsoft H.264
/// encoder takes).
const REFINE_QP: u32 = 16;

fn chosen_quality(outcomes: &[SettingOutcome]) -> bool {
    outcomes.iter().any(|o| o.name == "AVEncCommonRateControlMode" && o.applied && o.value == RATE_QUALITY.to_string())
}

/// The rebuilt refinement MFT: frames per second (the VBV buffer is three frames of the bitrate) and the least bitrate.
const REFINE_FPS: u16 = 2;
const REFINE_MIN_KBPS: u32 = 30_000;
const DEFAULT_OUTPUT_WAIT: Duration = Duration::from_millis(100);

/// Which MFT the encoder may use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MftChoice {
    /// The hardware MFT of the capture adapter, then the software MFT.
    #[default]
    Auto,
    /// Only the Windows software MFT (hardware excluded).
    SoftwareOnly,
    /// Only a hardware MFT of the adapter; no fallback.
    HardwareOnly,
    /// The first MFT (hardware of the adapter, then software) whose friendly name contains this text, ignoring case.
    Named(String),
    /// The MFT with this CLSID (lower-case hex without braces, as in [`MftSummary::clsid`]).
    Clsid(String),
}

#[derive(Clone)]
pub struct EncoderOptions {
    pub choice: MftChoice,
    /// A D3D11 device that already owns the frames (the capture device); its adapter overrides `EncoderConfig::adapter`.
    /// `None`: the encoder creates its own device on the configured adapter.
    pub device: Option<windows::Win32::Graphics::Direct3D11::ID3D11Device>,
    /// Frames between keyframes; `None` is an infinite GOP (keyframes only on request).
    pub gop_frames: Option<u32>,
    /// Ask the MFT for gradual intra refresh over this many frames when it supports it (off by default).
    pub intra_refresh_frames: Option<u32>,
    /// How long `encode` waits for the output of the frame it just submitted.
    pub output_wait: Duration,
}

impl Default for EncoderOptions {
    fn default() -> Self {
        Self {
            choice: MftChoice::Auto,
            device: None,
            gop_frames: None,
            intra_refresh_frames: None,
            output_wait: DEFAULT_OUTPUT_WAIT,
        }
    }
}

/// What happened when the encoder asked an MFT for one `ICodecAPI` property (R2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingOutcome {
    pub name: &'static str,
    /// The value asked for, as text.
    pub value: String,
    /// `ICodecAPI::IsSupported` said yes.
    pub supported: bool,
    /// `SetValue` succeeded.
    pub applied: bool,
    /// The failing HRESULT of `SetValue`.
    pub hresult: Option<i32>,
}

/// What `ICodecAPI` says about one property of an MFT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertySupport {
    pub name: &'static str,
    /// `IsSupported` returned S_OK.
    pub supported: bool,
    /// `IsModifiable` returned S_OK (it can change while streaming).
    pub modifiable: bool,
}

/// The MFT in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftSummary {
    pub name: String,
    /// CLSID, lower-case hex without braces.
    pub clsid: String,
    pub vendor: String,
    pub hardware: bool,
    pub asynchronous: bool,
    /// Frames travel to the MFT as D3D11 textures (otherwise as system memory).
    pub d3d_input: bool,
    pub adapter_name: String,
    pub adapter: AdapterId,
}

/// Where one frame's time went, microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameTiming {
    /// Upload and BGRA to NV12 conversion (and the system-memory copy for the software MFT).
    pub convert_us: u32,
    /// `ProcessInput` to the output sample.
    pub mft_us: u32,
}

#[derive(Clone, Copy)]
enum Val {
    U32(u32),
    Bool(bool),
}

impl Val {
    fn text(self) -> String {
        match self {
            Val::U32(v) => v.to_string(),
            Val::Bool(v) => v.to_string(),
        }
    }
    fn other(self) -> Self {
        match self {
            Val::U32(v) => Val::Bool(v != 0),
            Val::Bool(v) => Val::U32(u32::from(v)),
        }
    }
}

pub(crate) fn is_supported(codec: &ICodecAPI, key: &GUID) -> bool {
    // SAFETY: the vtable call returns the raw HRESULT: S_OK supported, S_FALSE or an error not supported.
    unsafe { (Interface::vtable(codec).IsSupported)(Interface::as_raw(codec), key).0 == 0 }
}

fn try_set(codec: &ICodecAPI, name: &'static str, key: &GUID, val: Val) -> SettingOutcome {
    let supported = is_supported(codec, key);
    let call = |v: Val| match v {
        Val::U32(x) => set_u32(codec, key, x),
        Val::Bool(x) => set_bool(codec, key, x),
    };
    let mut result = call(val);
    let mut used = val;
    if result.is_err() && supported && matches!(val, Val::Bool(_)) {
        // Some MFTs type a flag as VT_UI4 instead of the documented VT_BOOL.
        let alt = val.other();
        if call(alt).is_ok() {
            result = Ok(());
            used = alt;
        }
    }
    SettingOutcome {
        name,
        value: used.text(),
        supported,
        applied: result.is_ok(),
        hresult: result.err().map(|e| e.code().0),
    }
}

/// Sets `AVEncCommonMaxBitRate` and `AVEncCommonBufferSize` for a mean bitrate (see [`rate_limits`]) and returns both
/// outcomes. An MFT that refuses either is tolerated: the outcome says so (and stays in `settings()`), and the encoder
/// runs on with the mean alone. Source of the properties:
/// <https://learn.microsoft.com/en-us/windows/win32/directshow/avenccommonmaxbitrate-property> and
/// <https://learn.microsoft.com/en-us/windows/win32/directshow/avenccommonbuffersize-property> (read 2026-11-08): both
/// are VT_UI4, in bits per second and in bits.
///
/// Only `configure` sets them. Measured on the Intel Quick Sync MFT of this PC (2026-11-08), both report themselves
/// modifiable while streaming, yet a new buffer size makes the next `ProcessOutput` fail (E_UNEXPECTED) and a new peak
/// makes the MFT emit a keyframe, which a live bitrate change must not do. So a live change leaves them alone while the
/// new mean stays under the configured peak (`Active::peak_bps`) and rebuilds the encoder, which recomputes them, when
/// it does not.
fn limit_outcomes(codec: &ICodecAPI, mean_bps: u32, cbr: bool, fps: u32) -> [SettingOutcome; 2] {
    let (max, buffer) = rate_limits(mean_bps, cbr, fps);
    [
        try_set(codec, "AVEncCommonMaxBitRate", &CODECAPI_AVEncCommonMaxBitRate, Val::U32(max)),
        try_set(codec, "AVEncCommonBufferSize", &CODECAPI_AVEncCommonBufferSize, Val::U32(buffer)),
    ]
}

struct Inflight {
    captured_at: MonoMicros,
    submitted: Instant,
}

/// Everything that exists while the encoder is configured.
struct Active {
    cfg: EncoderConfig,
    options: EncoderOptions,
    candidate: Mft,
    info: EncoderInfo,
    summary: MftSummary,
    outcomes: Vec<SettingOutcome>,
    support: Vec<PropertySupport>,
    rate_modes: Vec<(u32, bool)>,
    /// The MFT takes a new mean bitrate while streaming (`IsModifiable`); otherwise `set_rate` rebuilds it.
    bitrate_live: bool,
    /// The peak bitrate the MFT was given (`u32::MAX`: none), bits per second.
    peak_bps: u32,
    /// How this MFT follows a new rate while streaming ([`LiveRate`]).
    live: LiveRate,
    /// Whether the rate control in force is CBR.
    cbr: bool,
    // Effective numbers.
    coded_fps: u32,
    input_fps: u32,
    kbps_target: u32,
    skip_interval_us: u64,
    last_encoded_at: Option<u64>,
    // COM objects (dropped in field order: the pump before the transform).
    pump: Option<Rc<EventPump>>,
    codec: Option<ICodecAPI>,
    transform: IMFTransform,
    _manager: Option<IMFDXGIDeviceManager>,
    out_info: OutputInfo,
    // GPU side.
    gpu: Gpu,
    proc: VideoProc,
    bgra_in: Option<ID3D11Texture2D>,
    pool: Vec<ID3D11Texture2D>,
    next_pool: usize,
    staging: Option<Staging>,
    // Stream state.
    seq: u64,
    hns_per_frame: i64,
    inflight: VecDeque<Inflight>,
    seq_header: Vec<u8>,
    config_pending: bool,
    force_next: bool,
    timing: FrameTiming,
}

impl Drop for Active {
    fn drop(&mut self) {
        // SAFETY: orderly shutdown messages to a live transform; errors are irrelevant at this point.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
        // The event loop lets go of the MFT first, then `Shutdown` releases its GPU session and ends its event queue
        // (the pending request holds the callback until then). Without both, every reconfigure leaked an MFT.
        if let Some(pump) = &self.pump {
            pump.stop();
        }
        mft::shutdown(&self.transform);
        self.candidate.shutdown_object();
    }
}

/// H.264 encoder over Media Foundation. See the module documentation.
pub struct MfH264Encoder {
    options: EncoderOptions,
    active: Option<Active>,
    last_cfg: Option<EncoderConfig>,
    /// After a refinement by rebuilding the MFT: the normal configuration to rebuild before the next `encode`.
    restore: Option<EncoderConfig>,
    /// Set while `refine` rebuilds the MFT: ask for the quality rate-control mode at this QP.
    quality_qp: Option<u32>,
}

impl MfH264Encoder {
    pub fn new(options: EncoderOptions) -> Self {
        Self { options, active: None, last_cfg: None, restore: None, quality_qp: None }
    }

    /// The MFT in use, once configured.
    pub fn mft(&self) -> Option<&MftSummary> {
        self.active.as_ref().map(|a| &a.summary)
    }

    /// The `ICodecAPI` properties asked of the MFT and what it answered.
    pub fn settings(&self) -> &[SettingOutcome] {
        self.active.as_ref().map_or(&[], |a| &a.outcomes)
    }

    /// Every property the probe knows, with the MFT's answer (risk R2).
    pub fn property_support(&self) -> &[PropertySupport] {
        self.active.as_ref().map_or(&[], |a| &a.support)
    }

    /// Which `AVEncCommonRateControlMode` values (0 CBR, 1 peak-constrained VBR, 2 unconstrained VBR, 3 quality,
    /// 4 low-delay VBR, 5 global VBR, 6 global low-delay VBR) the MFT accepted when probed before configuration.
    pub fn rate_modes(&self) -> &[(u32, bool)] {
        self.active.as_ref().map_or(&[], |a| &a.rate_modes)
    }

    /// Timing of the last submitted frame.
    pub fn last_timing(&self) -> FrameTiming {
        self.active.as_ref().map(|a| a.timing).unwrap_or_default()
    }

    /// Frames per second the MFT encodes after the preset cap and bitrate it was told, kbps.
    pub fn effective(&self) -> Option<(u32, u32)> {
        self.active.as_ref().map(|a| (a.coded_fps, a.kbps_target))
    }

    fn active_mut(&mut self) -> Result<&mut Active> {
        self.active.as_mut().ok_or_else(|| err(ErrorKind::Closed, "VideoEncoder", "configure was not called"))
    }
}

fn candidates(gpu: &Gpu, choice: &MftChoice) -> Result<Vec<Mft>> {
    let hardware = |g: &Gpu| -> Result<Vec<Mft>> {
        if g.adapter.software {
            return Ok(Vec::new());
        }
        mft::hardware(Category::Encoder, Some(g.adapter.id))
    };
    let mut out = Vec::new();
    match choice {
        MftChoice::Auto => {
            out.extend(hardware(gpu)?);
            out.extend(mft::software(Category::Encoder)?);
        }
        MftChoice::SoftwareOnly => out.extend(mft::software(Category::Encoder)?),
        MftChoice::HardwareOnly => out.extend(hardware(gpu)?),
        MftChoice::Named(text) => {
            let needle = text.to_lowercase();
            out.extend(hardware(gpu)?);
            out.extend(mft::software(Category::Encoder)?);
            out.retain(|m| m.name.to_lowercase().contains(&needle));
        }
        MftChoice::Clsid(text) => {
            out.extend(hardware(gpu)?);
            out.extend(mft::software(Category::Encoder)?);
            out.retain(|m| crate::mft::guid_text(&m.clsid) == text.to_lowercase());
        }
    }
    Ok(out)
}

impl VideoEncoder for MfH264Encoder {
    fn configure(&mut self, config: &EncoderConfig) -> Result<EncoderInfo> {
        self.active = None;
        self.restore = None;
        validate(config)?;
        sys::init_thread()?;
        let gpu = match &self.options.device {
            Some(d) => Gpu::from_device(d.clone())?,
            None => Gpu::on_adapter(config.adapter)?,
        };
        let list = candidates(&gpu, &self.options.choice)?;
        if list.is_empty() {
            return Err(err(
                ErrorKind::Unavailable,
                "VideoEncoder::configure",
                format!("no H.264 encoder MFT for {:?} on adapter {}", self.options.choice, gpu.adapter.name),
            ));
        }
        let mut failures = Vec::new();
        for candidate in list {
            match Active::build(&gpu, candidate.clone(), *config, self.options.clone(), self.quality_qp) {
                Ok(active) => {
                    let info = active.info.clone();
                    self.last_cfg = Some(*config);
                    self.active = Some(active);
                    return Ok(info);
                }
                Err(e) => failures.push(format!("{}: {e}", candidate.name)),
            }
        }
        Err(err(ErrorKind::Unavailable, "VideoEncoder::configure", failures.join("; ")))
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        if let Some(normal) = self.restore.take() {
            // The screen changed after a refinement that rebuilt the MFT at a high bitrate: back to the normal rate
            // control (a new MFT, so a keyframe with new SPS/PPS comes first, as after a `set_rate` rebuild).
            self.configure(&normal)?;
        }
        if self.active.is_none() {
            // After `flush`, or a failed reconfigure: start again from the last configuration.
            match self.last_cfg {
                Some(cfg) => {
                    self.configure(&cfg)?;
                }
                None => return Err(err(ErrorKind::Closed, "VideoEncoder::encode", "configure was not called")),
            }
        }
        let (w, h) = {
            let a = self.active_mut()?;
            (a.cfg.width, a.cfg.height)
        };
        if frame.width != w || frame.height != h {
            // A display size change: new MFT instance, new SPS/PPS, first frame a keyframe with `config_change`.
            let mut cfg = self.active_mut()?.cfg;
            cfg.width = frame.width;
            cfg.height = frame.height;
            self.configure(&cfg)?;
        }
        self.active_mut()?.encode(frame, force_keyframe)
    }

    fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()> {
        if let Some(normal) = self.restore.as_mut() {
            // The refinement MFT is idle until the next change: the new rate goes to the configuration that follows it.
            normal.kbps = kbps;
            normal.fps = fps;
            return Ok(());
        }
        let a = self.active_mut()?;
        let eff = effective(a.cfg.preset, kbps, fps);
        if eff.output_fps > a.coded_fps || a.codec.is_none() || !a.bitrate_live {
            // The MFT was configured for fewer frames per second than now wanted: rebuild it.
            let mut cfg = a.cfg;
            cfg.kbps = kbps;
            cfg.fps = fps;
            self.configure(&cfg)?;
            return Ok(());
        }
        let rebuilt = a.set_rate(kbps, fps);
        if rebuilt.is_err() {
            let mut cfg = self.active_mut()?.cfg;
            cfg.kbps = kbps;
            cfg.fps = fps;
            self.configure(&cfg)?;
        }
        Ok(())
    }

    /// L4. No per-picture quality control works on the MFTs here: Quick Sync lists `AVEncVideoMaxQP` and `AVEncVideoMinQP`
    /// as unsupported and every rate property as not modifiable while streaming, and on the software MFT capping
    /// `AVEncVideoMaxQP` was measured to leave the picture worse (17 dB against 30 dB) and the stream degraded after
    /// it. `AVEncVideoEncodeQP` and `AVEncCommonQuality` apply only in the quality rate-control mode (Microsoft H.264
    /// encoder page, <https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-encoder>, read 2026-11-08),
    /// which this encoder does not run. So the MFT is rebuilt for the refinement at a high bitrate and a very low frame
    /// rate, which makes the three-frame VBV buffer three seconds of bits; the next `encode` rebuilds the normal
    /// configuration lazily (a keyframe with new SPS/PPS, as after a `set_rate` rebuild).
    fn refine(&mut self, frame: &Frame) -> Result<Vec<EncodedFrame>> {
        if self.restore.is_some() {
            return Ok(Vec::new()); // already refined, nothing changed since
        }
        let a = self.active_mut()?;
        if frame.width != a.cfg.width || frame.height != a.cfg.height {
            return Ok(Vec::new()); // the display changed size since the picture was captured: nothing to refine
        }
        let normal = a.cfg;
        let hardware = a.candidate.hardware;
        let cfg = EncoderConfig {
            fps: REFINE_FPS,
            kbps: (normal.kbps.saturating_mul(8)).clamp(REFINE_MIN_KBPS, MAX_KBPS),
            ..normal
        };
        // Measured here (2560x1600 would only scale it): Quick Sync gains 34 dB through the high bitrate against 12 dB
        // through the quality mode, the software MFT 6.1 dB through the quality mode against 4.6 dB through the bitrate.
        self.quality_qp = if hardware { None } else { Some(REFINE_QP) };
        let built = self.configure(&cfg);
        self.quality_qp = None;
        built?;
        self.restore = Some(normal);
        // The MFT lives until the next change, so the longer output wait does no harm.
        match self.active.as_mut() {
            Some(a) => {
                a.options.output_wait = REFINE_OUTPUT_WAIT;
                a.encode(frame, true)
            }
            None => Err(err(ErrorKind::Closed, "VideoEncoder::refine", "the refinement MFT is gone")),
        }
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        if let Some(normal) = self.restore.take() {
            self.last_cfg = Some(normal);
        }
        let Some(a) = self.active.as_mut() else { return Ok(Vec::new()) };
        let frames = a.flush()?;
        // The MFT leaves the drained state only through a restart.
        self.active = None;
        Ok(frames)
    }
}

fn validate(cfg: &EncoderConfig) -> Result<()> {
    let bad = |what: &str| Err(err(ErrorKind::InvalidInput, "VideoEncoder::configure", what.to_string()));
    if cfg.width < 64 || cfg.height < 64 || cfg.width > 4096 || cfg.height > 4096 {
        return bad("size must be 64 to 4096 pixels per side");
    }
    if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
        return bad("NV12 needs an even width and height");
    }
    if cfg.fps == 0 || cfg.fps > 240 {
        return bad("fps must be 1 to 240");
    }
    if cfg.kbps < 100 {
        return bad("kbps must be at least 100");
    }
    Ok(())
}

impl Active {
    fn build(
        gpu: &Gpu,
        candidate: Mft,
        cfg: EncoderConfig,
        options: EncoderOptions,
        quality_qp: Option<u32>,
    ) -> Result<Self> {
        let eff = effective(cfg.preset, cfg.kbps, cfg.fps);
        let plan = preset_plan(cfg.preset);
        let live = live_rate(gpu.adapter.vendor_id, candidate.hardware);
        let modes = rate_modes_for(cfg.preset, live);
        let coded_fps = eff.output_fps;
        let transform = candidate.activate()?;
        // Any `?` below shuts the transform down; `Active`'s own `Drop` takes over once it is built.
        let mut guard = mft::ActivationGuard::new(&transform, &candidate);
        let asynchronous = mft::is_async(&transform);
        // SAFETY: attribute writes and messages on the transform we just activated.
        let mut manager = None;
        let mut d3d_input = false;
        unsafe {
            if asynchronous {
                let attrs = transform.GetAttributes().ctx("IMFTransform::GetAttributes")?;
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).ctx("MF_TRANSFORM_ASYNC_UNLOCK")?;
            }
            if candidate.hardware {
                if let Ok(attrs) = transform.GetAttributes() {
                    let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
                }
                let mut token = 0u32;
                let mut m = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m).ctx("MFCreateDXGIDeviceManager")?;
                let m = m.ok_or_else(|| err(ErrorKind::Os, "MFCreateDXGIDeviceManager", "no manager"))?;
                m.ResetDevice(&gpu.device, token).ctx("IMFDXGIDeviceManager::ResetDevice")?;
                if transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, Interface::as_raw(&m) as usize).is_ok() {
                    d3d_input = true;
                    manager = Some(m);
                }
            }
        }
        let codec = mft::codec_api(&transform);
        let mut outcomes = Vec::new();
        let mut pending: Vec<(&'static str, GUID, Val)> = Vec::new();
        let mut rate_modes: Vec<(u32, bool)> = Vec::new();
        let mut cbr = false;
        // The peak the MFT was given (`u32::MAX` while it has none).
        let mut peak_bps = u32::MAX;
        if let Some(c) = &codec {
            let gop = options.gop_frames.unwrap_or(u32::MAX);
            let plan_values: Vec<(&'static str, GUID, Val)> = vec![
                ("AVEncCommonLowLatency", CODECAPI_AVEncCommonLowLatency, Val::Bool(true)),
                ("AVLowLatencyMode", CODECAPI_AVLowLatencyMode, Val::Bool(true)),
                ("AVEncCommonRealTime", CODECAPI_AVEncCommonRealTime, Val::Bool(true)),
                ("AVEncMPVDefaultBPictureCount", CODECAPI_AVEncMPVDefaultBPictureCount, Val::U32(0)),
                ("AVEncMPVGOPSize", CODECAPI_AVEncMPVGOPSize, Val::U32(gop)),
                ("AVEncCommonQualityVsSpeed", CODECAPI_AVEncCommonQualityVsSpeed, Val::U32(plan.quality_vs_speed)),
                ("AVEncVideoMaxNumRefFrame", CODECAPI_AVEncVideoMaxNumRefFrame, Val::U32(2)),
                ("AVEncCommonAllowFrameDrops", CODECAPI_AVEncCommonAllowFrameDrops, Val::Bool(false)),
            ];
            for (name, key, val) in plan_values {
                let outcome = try_set(c, name, &key, val);
                if !outcome.applied && outcome.supported {
                    pending.push((name, key, val));
                }
                outcomes.push(outcome);
            }
            // Rate control: find which modes the MFT takes, then settle on the preset's first accepted choice.
            for mode in 0..=6u32 {
                rate_modes.push((mode, set_u32(c, &CODECAPI_AVEncCommonRateControlMode, mode).is_ok()));
            }
            // A refinement rebuild asks for the quality mode (3), the one `AVEncVideoEncodeQP` applies to.
            let quality = quality_qp.is_some() && rate_modes.iter().any(|(rm, ok)| *rm == RATE_QUALITY && *ok);
            let chosen = if quality {
                Some(RATE_QUALITY)
            } else {
                modes.iter().copied().find(|m| rate_modes.iter().any(|(rm, ok)| rm == m && *ok))
            };
            outcomes.push(match chosen {
                Some(m) => {
                    let o = try_set(c, "AVEncCommonRateControlMode", &CODECAPI_AVEncCommonRateControlMode, Val::U32(m));
                    cbr = m == RATE_CBR && o.applied;
                    o
                }
                None => SettingOutcome {
                    name: "AVEncCommonRateControlMode",
                    value: format!("none of {modes:?}"),
                    supported: is_supported(c, &CODECAPI_AVEncCommonRateControlMode),
                    applied: false,
                    hresult: None,
                },
            });
            if let (Some(qp), true) = (quality_qp, chosen_quality(&outcomes)) {
                // Quality mode: the QP directly, else the quality level that maps to it (both before the types are set,
                // as the Microsoft page asks for the quality level).
                let o = try_set(c, "AVEncVideoEncodeQP", &CODECAPI_AVEncVideoEncodeQP, Val::U32(qp));
                let eq = o.applied;
                outcomes.push(o);
                if !eq {
                    outcomes.push(try_set(c, "AVEncCommonQuality", &CODECAPI_AVEncCommonQuality, Val::U32(100)));
                }
            }
            if let Some(frames) = options.intra_refresh_frames {
                // The packing of this UINT32 is not in the headers: try the plausible ones, first accepted wins.
                let mut last = None;
                for v in [frames, 1, frames << 16 | 1, 1 << 16 | (frames & 0xFFFF), frames << 16] {
                    let o = try_set(
                        c,
                        "AVEncVideoGradualIntraRefresh",
                        &CODECAPI_AVEncVideoGradualIntraRefresh,
                        Val::U32(v),
                    );
                    let done = o.applied;
                    last = Some(o);
                    if done {
                        break;
                    }
                }
                outcomes.extend(last);
            }
        }
        // GOP size: an MFT that refuses "infinite" gets one keyframe per minute at the coded rate.
        if options.gop_frames.is_none()
            && let (Some(c), Some(o)) = (&codec, outcomes.iter_mut().find(|o| o.name == "AVEncMPVGOPSize"))
            && !o.applied
            && o.supported
        {
            let long = coded_fps * 60;
            let retry = try_set(c, "AVEncMPVGOPSize", &CODECAPI_AVEncMPVGOPSize, Val::U32(long));
            if retry.applied {
                *o = retry;
            }
        }

        // Types: output first, then input.
        let level = level_idc(cfg.width, cfg.height, coded_fps, eff.kbps);
        let (profile, level) = set_types(&transform, &cfg, coded_fps, eff.kbps, level)?;

        // Properties some MFTs only take once the types are set.
        if let Some(c) = &codec {
            for (name, key, val) in pending {
                let retry = try_set(c, name, &key, val);
                if retry.applied
                    && let Some(o) = outcomes.iter_mut().find(|o| o.name == name)
                {
                    *o = retry;
                }
            }
            let mean =
                try_set(c, "AVEncCommonMeanBitRate", &CODECAPI_AVEncCommonMeanBitRate, Val::U32(eff.kbps * 1000));
            outcomes.push(mean);
            // L2: a ceiling and a three-frame buffer, from the mean the MFT was just given.
            let limits = limit_outcomes(c, eff.kbps * 1000, cbr, coded_fps);
            if limits[0].applied {
                peak_bps = rate_limits(eff.kbps * 1000, cbr, coded_fps).0;
            }
            outcomes.extend(limits);
            let force_ok = is_supported(c, &CODECAPI_AVEncVideoForceKeyFrame);
            outcomes.push(SettingOutcome {
                name: "AVEncVideoForceKeyFrame",
                value: "1 (per frame)".to_string(),
                supported: force_ok,
                applied: false,
                hresult: None,
            });
        }

        let out_info = output_info(&transform)?;

        // Streaming start.
        // SAFETY: messages to a configured transform.
        unsafe {
            // The software MFT refuses a flush before the first sample; nothing to flush yet anyway.
            let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .ctx("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .ctx("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
        }
        let pump = if asynchronous {
            let generator: IMFMediaEventGenerator = transform.cast().ctx("IMFTransform as IMFMediaEventGenerator")?;
            Some(Rc::new(EventPump::start(generator)?))
        } else {
            None
        };

        // What the MFT answers while streaming (IsModifiable tells what can change at run time).
        let support = match &codec {
            Some(c) => mft::probe_properties()
                .into_iter()
                .map(|(name, key)| PropertySupport {
                    name,
                    supported: is_supported(c, &key),
                    // SAFETY: IsModifiable only reads the property state.
                    modifiable: unsafe { c.IsModifiable(&key) }.0 == 0,
                })
                .collect(),
            None => Vec::new(),
        };

        // `IsModifiable` is read once streaming runs and is believed: Quick Sync under CBR reports the mean bitrate as
        // fixed and ignores a new value (measured), under peak-constrained VBR it follows it; the software MFT accepts
        // the call and ignores it. A fixed bitrate makes `set_rate` rebuild the MFT (new SPS/PPS, a keyframe).
        let bitrate_live = support.iter().any(|p| p.name == "AVEncCommonMeanBitRate" && p.modifiable)
            || (live == LiveRate::CbrWithPeak && cbr);

        // The converter and its textures.
        let proc = VideoProc::new(
            gpu,
            (cfg.width, cfg.height),
            (cfg.width, cfg.height),
            (cfg.width, cfg.height),
            Space::Rgb,
            Space::Yuv709Limited,
            coded_fps,
        )?;
        let mut pool = Vec::with_capacity(POOL);
        for _ in 0..POOL {
            pool.push(gpu.texture(
                cfg.width,
                cfg.height,
                NV12,
                USAGE_DEFAULT,
                windows::Win32::Graphics::Direct3D11::D3D11_BIND_FLAG(
                    D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0,
                ),
                D3D11_CPU_ACCESS_FLAG(0),
            )?);
        }
        let staging = if d3d_input { None } else { Some(Staging::new(gpu, cfg.width, cfg.height, NV12)?) };

        // SPS/PPS from the output type, when the MFT publishes them.
        let seq_header = sequence_header(&transform);

        let low_latency =
            outcomes.iter().any(|o| (o.name == "AVEncCommonLowLatency" || o.name == "AVLowLatencyMode") && o.applied)
                || candidate.hardware;
        let info = EncoderInfo {
            name: candidate.name.clone(),
            hardware: candidate.hardware,
            profile,
            level: level as u8,
            low_latency,
        };
        let summary = MftSummary {
            name: candidate.name.clone(),
            clsid: crate::mft::guid_text(&candidate.clsid),
            vendor: candidate.vendor.clone(),
            hardware: candidate.hardware,
            asynchronous,
            d3d_input,
            adapter_name: gpu.adapter.name.clone(),
            adapter: gpu.adapter.id,
        };
        let input_fps = eff.input_fps;
        let skip_interval_us = if eff.output_fps < input_fps { 1_000_000 / u64::from(eff.output_fps) } else { 0 };
        guard.disarm();
        Ok(Self {
            cfg,
            options,
            candidate,
            info,
            summary,
            outcomes,
            support,
            rate_modes,
            bitrate_live,
            peak_bps,
            live,
            cbr,
            coded_fps,
            input_fps,
            kbps_target: eff.kbps,
            skip_interval_us,
            last_encoded_at: None,
            pump,
            codec,
            transform,
            _manager: manager,
            out_info,
            gpu: gpu.clone(),
            proc,
            bgra_in: None,
            pool,
            next_pool: 0,
            staging,
            seq: 0,
            hns_per_frame: 10_000_000 / i64::from(coded_fps),
            inflight: VecDeque::new(),
            seq_header,
            config_pending: true,
            force_next: false,
            timing: FrameTiming::default(),
        })
    }

    /// The BGRA texture holding this frame (uploaded for CPU frames).
    fn source_texture(&mut self, frame: &Frame) -> Result<ID3D11Texture2D> {
        match &frame.pixels {
            Pixels::Cpu { stride, data } => {
                let need = *stride as usize * frame.height as usize;
                if *stride < frame.width * 4 || data.len() < need {
                    return Err(err(
                        ErrorKind::InvalidInput,
                        "VideoEncoder::encode",
                        "BGRA buffer smaller than stride * height",
                    ));
                }
                if self.bgra_in.is_none() {
                    self.bgra_in = Some(self.gpu.texture(
                        frame.width,
                        frame.height,
                        BGRA,
                        USAGE_DEFAULT,
                        windows::Win32::Graphics::Direct3D11::D3D11_BIND_FLAG(
                            D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0,
                        ),
                        D3D11_CPU_ACCESS_FLAG(0),
                    )?);
                }
                let tex = self.bgra_in.clone().expect("created above");
                self.gpu.upload_bgra(&tex, data, *stride);
                Ok(tex)
            }
            Pixels::Gpu(surface) => {
                let tex = surface.0.downcast_ref::<ID3D11Texture2D>().ok_or_else(|| {
                    err(ErrorKind::InvalidInput, "VideoEncoder::encode", "GPU frame is not an ID3D11Texture2D")
                })?;
                // SAFETY: a getter on a live texture.
                let device = unsafe { tex.GetDevice() }.ctx("ID3D11Texture2D::GetDevice")?;
                if Interface::as_raw(&device) != Interface::as_raw(&self.gpu.device) {
                    return Err(err(
                        ErrorKind::Unsupported,
                        "VideoEncoder::encode",
                        "the texture belongs to another D3D11 device: pass the capture device in EncoderOptions::device",
                    ));
                }
                Ok(tex.clone())
            }
        }
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let t0 = Instant::now();
        let mut out = Vec::new();
        // Frames the MFT finished since the last call come first.
        self.collect(Duration::ZERO, &mut out)?;

        if self.skip_interval_us > 0
            && !force_keyframe
            && !self.force_next
            && let Some(last) = self.last_encoded_at
            && frame.captured_at.0.saturating_sub(last) < self.skip_interval_us * 9 / 10
        {
            return Ok(out);
        }

        let src = self.source_texture(frame)?;
        let nv12 = self.pool[self.next_pool].clone();
        self.next_pool = (self.next_pool + 1) % POOL;
        self.proc.blt(&src, 0, &nv12)?;
        let sample = self.input_sample(&nv12)?;
        let convert_done = Instant::now();
        // SAFETY: timestamps on a sample we own.
        unsafe {
            sample.SetSampleTime(self.seq as i64 * self.hns_per_frame).ctx("SetSampleTime")?;
            sample.SetSampleDuration(self.hns_per_frame).ctx("SetSampleDuration")?;
        }
        self.seq += 1;

        if force_keyframe || self.force_next {
            self.force_next = false;
            self.request_keyframe()?;
        }

        // Submit.
        if let Some(pump) = self.pump.clone() {
            // Pull finished outputs while waiting for a free input slot so the MFT never stalls on us.
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if pump.take_need_input(Duration::from_millis(2))? {
                    break;
                }
                self.collect(Duration::ZERO, &mut out)?;
                if Instant::now() > deadline {
                    return Err(err(ErrorKind::Timeout, "VideoEncoder::encode", "the MFT did not ask for input"));
                }
            }
            // SAFETY: the transform asked for input; the sample is valid.
            unsafe { self.transform.ProcessInput(0, &sample, 0) }.ctx("IMFTransform::ProcessInput")?;
        } else {
            // SAFETY: as above, for a synchronous MFT.
            let first = unsafe { self.transform.ProcessInput(0, &sample, 0) };
            if let Err(e) = first {
                if e.code() == MF_E_NOTACCEPTING {
                    self.collect(Duration::ZERO, &mut out)?;
                    // SAFETY: retry after draining the output.
                    unsafe { self.transform.ProcessInput(0, &sample, 0) }.ctx("IMFTransform::ProcessInput")?;
                } else {
                    return Err(os_err("IMFTransform::ProcessInput", &e));
                }
            }
        }
        self.inflight.push_back(Inflight { captured_at: frame.captured_at, submitted: t0 });
        self.last_encoded_at = Some(frame.captured_at.0);

        // Collect: wait for this frame's output.
        let wait = self.options.output_wait;
        self.collect(wait, &mut out)?;
        self.timing = FrameTiming {
            convert_us: convert_done.duration_since(t0).as_micros().min(u128::from(u32::MAX)) as u32,
            mft_us: convert_done.elapsed().as_micros().min(u128::from(u32::MAX)) as u32,
        };
        Ok(out)
    }

    fn input_sample(&self, nv12: &ID3D11Texture2D) -> Result<IMFSample> {
        let size = self.cfg.width * self.cfg.height * 3 / 2;
        if let Some(staging) = &self.staging {
            // System-memory copy of the two planes, rows packed.
            let (w, h) = (self.cfg.width as usize, self.cfg.height as usize);
            let mut packed = vec![0u8; size as usize];
            staging.read(&self.gpu, nv12, |bytes, pitch| {
                for y in 0..h {
                    packed[y * w..(y + 1) * w].copy_from_slice(&bytes[y * pitch..y * pitch + w]);
                }
                let uv = pitch * h;
                for y in 0..h / 2 {
                    let dst = w * h + y * w;
                    packed[dst..dst + w].copy_from_slice(&bytes[uv + y * pitch..uv + y * pitch + w]);
                }
            })?;
            return memory_sample(&packed);
        }
        // SAFETY: wraps the texture in a DXGI buffer; the sample keeps it alive.
        unsafe {
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false).ctx("MFCreateDXGISurfaceBuffer")?;
            buffer.SetCurrentLength(size).ctx("SetCurrentLength")?;
            let sample = windows::Win32::Media::MediaFoundation::MFCreateSample().ctx("MFCreateSample")?;
            sample.AddBuffer(&buffer).ctx("AddBuffer")?;
            Ok(sample)
        }
    }

    fn request_keyframe(&mut self) -> Result<()> {
        let Some(codec) = &self.codec else {
            return Err(err(
                ErrorKind::Unsupported,
                "VideoEncoder::encode",
                "the MFT has no ICodecAPI to force a keyframe",
            ));
        };
        let result = set_u32(codec, &CODECAPI_AVEncVideoForceKeyFrame, 1);
        if let Some(o) = self.outcomes.iter_mut().find(|o| o.name == "AVEncVideoForceKeyFrame") {
            o.applied = result.is_ok();
            o.hresult = result.as_ref().err().map(|e| e.code().0);
        }
        result.ctx("ICodecAPI::SetValue(AVEncVideoForceKeyFrame)")
    }

    /// Takes the outputs the MFT has ready; waits up to `wait` for the first one while frames are in flight.
    fn collect(&mut self, wait: Duration, out: &mut Vec<EncodedFrame>) -> Result<()> {
        match self.pump.clone() {
            Some(pump) => {
                let mut w = wait;
                loop {
                    if self.inflight.is_empty() && pump.pending_outputs() == 0 {
                        break;
                    }
                    if !pump.take_have_output(w)? {
                        break;
                    }
                    w = Duration::ZERO;
                    self.pull_one(out)?;
                }
            }
            None => {
                // Synchronous MFT: output is ready right after the input or not at all.
                while !self.inflight.is_empty() {
                    if !self.pull_one(out)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// One `ProcessOutput`. `Ok(false)` when the MFT has nothing.
    fn pull_one(&mut self, out: &mut Vec<EncodedFrame>) -> Result<bool> {
        for _ in 0..3 {
            let provide = if self.out_info.provides_samples {
                None
            } else {
                Some(empty_sample(self.out_info.buffer_size.max(self.cfg.width * self.cfg.height))?)
            };
            match process_output(&self.transform, provide)? {
                Output::Sample(sample) => {
                    let frame = self.finish_frame(&sample)?;
                    out.push(frame);
                    return Ok(true);
                }
                Output::NeedMoreInput => return Ok(false),
                Output::StreamChange => {
                    // SAFETY: renegotiate the output type with the first type the MFT offers.
                    unsafe {
                        let t = self.transform.GetOutputAvailableType(0, 0).ctx("GetOutputAvailableType")?;
                        self.transform.SetOutputType(0, &t, 0).ctx("SetOutputType")?;
                    }
                    self.out_info = output_info(&self.transform)?;
                }
            }
        }
        Err(err(ErrorKind::Os, "IMFTransform::ProcessOutput", "repeated stream change"))
    }

    fn finish_frame(&mut self, sample: &IMFSample) -> Result<EncodedFrame> {
        let mut data = sample_bytes(sample)?;
        if !(data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])) {
            return Err(err(ErrorKind::Os, "IMFTransform::ProcessOutput", "output is not Annex B (no start code)"));
        }
        // SAFETY: a getter on the sample.
        let clean = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.map(|v| v != 0).unwrap_or(false);
        let keyframe = annexb::is_idr(&data) || clean;
        let has_sps = annexb::contains(&data, annexb::NAL_SPS);
        if has_sps {
            let ps = annexb::parameter_sets(&data);
            if !ps.is_empty() {
                self.seq_header = ps;
            }
        } else if keyframe && !self.seq_header.is_empty() {
            let mut with = self.seq_header.clone();
            with.extend_from_slice(&data);
            data = with;
        }
        let carries_ps = annexb::contains(&data, annexb::NAL_SPS);
        let config_change = keyframe && carries_ps && self.config_pending;
        if config_change {
            self.config_pending = false;
        }
        let pending = self.inflight.pop_front();
        let encode_us =
            pending.as_ref().map_or(0, |p| p.submitted.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
        Ok(EncodedFrame {
            captured_at: pending.map_or(MonoMicros(0), |p| p.captured_at),
            encode_us,
            keyframe,
            config_change,
            width: self.cfg.width as u16,
            height: self.cfg.height as u16,
            annex_b: data,
        })
    }

    fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()> {
        let eff = effective(self.cfg.preset, kbps, fps);
        let codec = self.codec.clone().ok_or_else(|| err(ErrorKind::Unsupported, "set_rate", "no ICodecAPI"))?;
        // The MFT keeps the frame rate it was configured with; scale the target so the real bitrate matches.
        let scaled = (u64::from(eff.kbps) * u64::from(self.coded_fps) / u64::from(eff.output_fps))
            .min(u64::from(u32::MAX / 1000));
        let mean = scaled as u32 * 1000;
        let set_mean = || {
            set_u32(&codec, &CODECAPI_AVEncCommonMeanBitRate, mean).ctx("ICodecAPI::SetValue(AVEncCommonMeanBitRate)")
        };
        if self.live == LiveRate::CbrWithPeak && self.peak_bps != u32::MAX {
            // The peak moves with the mean (CBR: the peak is the mean); this MFT takes both live (`LiveRate`). Raised
            // peak first, lowered mean first, so the pair never has the mean above the peak.
            let (peak, _) = rate_limits(mean, self.cbr, self.coded_fps);
            let set_peak = || {
                set_u32(&codec, &CODECAPI_AVEncCommonMaxBitRate, peak).ctx("ICodecAPI::SetValue(AVEncCommonMaxBitRate)")
            };
            if peak >= self.peak_bps {
                set_peak()?;
                set_mean()?;
            } else {
                set_mean()?;
                set_peak()?;
            }
            self.peak_bps = peak;
        } else if mean > self.peak_bps {
            // A mean above the configured peak is not a valid pair: the caller rebuilds the encoder (new limits).
            return Err(err(ErrorKind::Unsupported, "set_rate", "the new bitrate is above the configured peak"));
        } else {
            set_mean()?;
        }
        self.cfg.kbps = kbps;
        self.cfg.fps = fps;
        self.kbps_target = eff.kbps;
        self.input_fps = eff.input_fps;
        self.skip_interval_us = if eff.output_fps < eff.input_fps { 1_000_000 / u64::from(eff.output_fps) } else { 0 };
        let _ = &self.candidate;
        Ok(())
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        self.collect(self.options.output_wait, &mut out)?;
        // SAFETY: drain request to a live transform.
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .ctx("MFT_MESSAGE_NOTIFY_END_OF_STREAM")?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0).ctx("MFT_MESSAGE_COMMAND_DRAIN")?;
        }
        match self.pump.clone() {
            Some(pump) => {
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    if pump.take_have_output(Duration::from_millis(20))? {
                        self.pull_one(&mut out)?;
                    } else if pump.wait_drained(Duration::ZERO)? || Instant::now() > deadline {
                        break;
                    }
                }
            }
            None => while self.pull_one(&mut out)? {},
        }
        Ok(out)
    }
}

/// Output and input types; returns the profile and level the MFT accepted.
fn set_types(
    transform: &IMFTransform,
    cfg: &EncoderConfig,
    coded_fps: u32,
    kbps: u32,
    level: u32,
) -> Result<(H264Profile, u32)> {
    let attempts: [(u32, Option<u32>, H264Profile); 3] =
        [(100, Some(level), H264Profile::High), (100, None, H264Profile::High), (77, None, H264Profile::Main)];
    let mut last = None;
    let mut accepted = None;
    for (profile, lvl, which) in attempts {
        let t: IMFMediaType = video_type(&MFVideoFormat_H264, cfg.width, cfg.height, coded_fps)?;
        // SAFETY: attribute writes on our media type, then the type is offered to the MFT.
        let result = unsafe {
            t.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000).ctx("MF_MT_AVG_BITRATE")?;
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile).ctx("MF_MT_MPEG2_PROFILE")?;
            if let Some(l) = lvl {
                t.SetUINT32(&MF_MT_MPEG2_LEVEL, l).ctx("MF_MT_MPEG2_LEVEL")?;
            }
            // The colour the stream is in, for the VUI: BT.709 limited range, as NVENC signals it (an MFT that does
            // not write a colour description leaves the decoder to guess, and VideoToolbox guesses BT.601 below HD).
            let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
            let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
            let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
            let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
            transform.SetOutputType(0, &t, 0)
        };
        match result {
            Ok(()) => {
                accepted = Some((which, lvl.unwrap_or(level)));
                break;
            }
            Err(e) => last = Some(os_err("IMFTransform::SetOutputType", &e)),
        }
    }
    let Some((profile, level)) = accepted else {
        return Err(last.unwrap_or_else(|| err(ErrorKind::Os, "SetOutputType", "no output type accepted")));
    };

    // Input: NV12, with the colour description first and without it when the MFT refuses it.
    for with_color in [true, false] {
        let t = video_type(&MFVideoFormat_NV12, cfg.width, cfg.height, coded_fps)?;
        if with_color {
            let attrs = attrs_of(&t)?;
            // SAFETY: attribute writes on our media type.
            unsafe {
                let _ = attrs.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
                let _ = attrs.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
                let _ = attrs.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
                let _ = attrs.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
            }
        }
        // SAFETY: offers our type to the MFT.
        match unsafe { transform.SetInputType(0, &t, 0) } {
            Ok(()) => return Ok((profile, level)),
            Err(e) if !with_color => return Err(os_err("IMFTransform::SetInputType", &e)),
            Err(_) => {}
        }
    }
    Err(err(ErrorKind::Os, "SetInputType", "no input type accepted"))
}

/// SPS and PPS the MFT published in its output type (`MF_MT_MPEG_SEQUENCE_HEADER`), as Annex B.
fn sequence_header(transform: &IMFTransform) -> Vec<u8> {
    // SAFETY: reads a blob attribute of the current output type into a buffer sized from GetBlobSize.
    unsafe {
        let Ok(t) = transform.GetOutputCurrentType(0) else { return Vec::new() };
        let Ok(attrs) = attrs_of(&t) else { return Vec::new() };
        let Ok(size) = attrs.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return Vec::new() };
        let mut buf = vec![0u8; size as usize];
        if attrs.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).is_err() {
            return Vec::new();
        }
        annexb::parameter_sets(&buf)
    }
}
