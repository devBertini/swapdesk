//! `VideoEncoder` over NVENC, called directly through `nvEncodeAPI64.dll` (D21). Used for displays whose adapter is an
//! NVIDIA GPU, because the NVIDIA H.264 Media Foundation transform cannot be activated on this PC.
//!
//! The capture's BGRA `ID3D11Texture2D` is registered with NVENC and mapped as an `ARGB` input: no colour-conversion
//! pass and no copy (NVENC converts to YUV inside the encoder). A texture the driver refuses to register (for example one
//! without the render-target bind flag) is copied once on the GPU into an internal texture; [`NvencEncoder::input_copied`]
//! tells which path runs. The session is synchronous (`enableEncodeAsync = 0`), the preset is P1 to P5 (see
//! [`NvencTuning::for_preset`]) with the low-latency tuning, the GOP is infinite and there are no B-frames, so one input gives one Annex B output frame in the
//! same call. SPS and PPS ride on every IDR (`repeatSPSPPS`).

use std::ffi::{CStr, c_void};
use std::rc::Rc;
use std::time::Instant;

use swapdesk_platform::{EncodedFrame, EncoderConfig, EncoderInfo, ErrorKind, Frame, Pixels, Result, VideoEncoder};
use swapdesk_proto::H264Profile;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_FLAG, D3D11_TEXTURE2D_DESC, ID3D11Device,
    ID3D11Resource, ID3D11Texture2D,
};
use windows::core::Interface;

use crate::gpu::{BGRA, Gpu, USAGE_DEFAULT};
use crate::nvenc_sys::*;
use crate::preset::{RATE_CBR, effective, level_idc, preset_plan};
use crate::sys::{Ctx, err};
use crate::{annexb, sys};

/// Registered textures kept per session (the capture pool holds up to 8 textures).
const REGISTRY: usize = 16;

/// The constant QP of a refinement picture (H.264 QP 0 to 51; the 16 of the Media Foundation refinement is the lowest
/// `AVEncVideoEncodeQP` the Microsoft H.264 encoder takes, so both encoders refine to the same level).
const REFINE_QP: u32 = 16;

/// Options of the NVENC encoder.
#[derive(Clone, Default)]
pub struct NvencOptions {
    /// A D3D11 device that already owns the frames (the capture device); its adapter overrides
    /// `EncoderConfig::adapter`. `None`: the encoder creates its own device on the configured adapter.
    pub device: Option<ID3D11Device>,
    /// Frames between IDR frames; `None` is an infinite GOP (IDR only on request).
    pub gop_frames: Option<u32>,
    /// Intra refresh over this many frames instead of periodic IDR (off by default).
    pub intra_refresh_frames: Option<u32>,
    /// The driver library to load; `None` is `nvEncodeAPI64.dll`. A wrong name makes `configure` fail with
    /// `Unavailable` (the fallback test).
    pub library: Option<String>,
    /// Replaces the preset and AQ choice of the viewer preset (measurements); `None` is [`NvencTuning::for_preset`].
    pub tuning: Option<NvencTuning>,
}

/// The NVENC preset and adaptive quantization a viewer preset runs with.
///
/// Sources read on 2026-11-08: the NVENC Video Encoder API Programming Guide 13.0, sections 8.5.1 and 8.5.2
/// (<https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html>)
/// and `nvEncodeAPI.h` n13.1.15.0 (`NV_ENC_RC_PARAMS::enableAQ`, `enableTemporalAQ`, `NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ`).
/// Spatial AQ moves bits to flat areas (text on a plain background) and needs no lookahead. Temporal AQ needs no
/// lookahead either (the guide asks only for the capability bit), but it runs a CUDA pre-pass ("very minor"
/// slowdown) and makes frame sizes fluctuate more inside a GOP: with the infinite GOP here, an I/P-frame can grow
/// well past the mean, which is the VBV peak this crate bounds with a three-frame buffer. The measured cost of
/// each choice is in STATUS.md (stage W1.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvencTuning {
    /// 1 to 5 (`NV_ENC_PRESET_P1_GUID` to `P5`).
    pub preset: u8,
    /// `enableAQ`.
    pub spatial_aq: bool,
    /// `enableTemporalAQ`; ignored (spatial only) when the GPU does not report `NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ`.
    pub temporal_aq: bool,
}

impl NvencTuning {
    /// Auto: P4. Text (crisp): P4 and spatial AQ (P5 was asked for and measured 6.7 to 11 ms median at 2560x1600, over the
    /// 6 ms budget, so it stepped down). Motion: P3 and temporal AQ (L1 of the 2026-11-08 evaluation; every
    /// choice measured under 6 ms a frame at 2560x1600 at 60 fps, STATUS.md).
    pub fn for_preset(preset: swapdesk_proto::Preset) -> Self {
        use swapdesk_proto::Preset;
        match preset {
            Preset::Auto => Self { preset: 4, spatial_aq: false, temporal_aq: false },
            Preset::Text => Self { preset: 4, spatial_aq: true, temporal_aq: false },
            Preset::Motion => Self { preset: 3, spatial_aq: false, temporal_aq: true },
        }
    }

    fn guid(self) -> windows::core::GUID {
        match self.preset {
            0..=1 => NV_ENC_PRESET_P1_GUID,
            2 => NV_ENC_PRESET_P2_GUID,
            3 => NV_ENC_PRESET_P3_GUID,
            4 => NV_ENC_PRESET_P4_GUID,
            _ => NV_ENC_PRESET_P5_GUID,
        }
    }
}

/// What the session settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvencSummary {
    pub adapter_name: String,
    /// `(major, minor)` of the NVENC API the driver reports as its maximum.
    pub driver_api: (u32, u32),
    /// `(major, minor)` of the header this build follows.
    pub header_api: (u32, u32),
    /// 1 to 5.
    pub preset: u8,
    /// Spatial and temporal AQ in force.
    pub spatial_aq: bool,
    pub temporal_aq: bool,
    /// `NV_ENC_PARAMS_RC_*`.
    pub rate_control: u32,
}

struct Registered {
    /// Keeps the texture (and so its address) alive while it is registered.
    texture: ID3D11Texture2D,
    handle: *mut c_void,
}

struct Active {
    cfg: EncoderConfig,
    api: Rc<NvencApi>,
    session: *mut c_void,
    gpu: Gpu,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    config: Box<NV_ENC_CONFIG>,
    bitstream: *mut c_void,
    registered: Vec<Registered>,
    /// BGRA texture CPU frames are uploaded to.
    cpu_in: Option<ID3D11Texture2D>,
    /// BGRA texture a GPU frame is copied to when the driver would not register it as it came.
    copy_in: Option<ID3D11Texture2D>,
    needs_copy: bool,
    last_copied: bool,
    info: EncoderInfo,
    summary: NvencSummary,
    coded_fps: u32,
    kbps_target: u32,
    skip_interval_us: u64,
    last_encoded_at: Option<u64>,
    seq: u64,
    seq_header: Vec<u8>,
    config_pending: bool,
    force_next: bool,
    submit_us: u32,
}

/// H.264 encoder over NVENC. See the module documentation. Not `Send` (C3 thread rules).
pub struct NvencEncoder {
    options: NvencOptions,
    active: Option<Active>,
    last_cfg: Option<EncoderConfig>,
}

impl NvencEncoder {
    pub fn new(options: NvencOptions) -> Self {
        Self { options, active: None, last_cfg: None }
    }

    /// What the session settled on, once configured.
    pub fn summary(&self) -> Option<&NvencSummary> {
        self.active.as_ref().map(|a| &a.summary)
    }

    /// Frames per second the encoder encodes after the preset cap, and the bitrate it was told, kbps.
    pub fn effective(&self) -> Option<(u32, u32)> {
        self.active.as_ref().map(|a| (a.coded_fps, a.kbps_target))
    }

    /// Whether the last frame went through a GPU copy because the driver would not take the texture as it was
    /// (`false`: the capture texture itself was the encoder input).
    pub fn input_copied(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.last_copied)
    }

    /// Microseconds the last `encode` spent before `nvEncEncodePicture` returned (register, map, submit and encode).
    pub fn last_submit_us(&self) -> u32 {
        self.active.as_ref().map_or(0, |a| a.submit_us)
    }

    fn active_mut(&mut self) -> Result<&mut Active> {
        self.active.as_mut().ok_or_else(|| err(ErrorKind::Closed, "VideoEncoder", "configure was not called"))
    }
}

fn validate(cfg: &EncoderConfig) -> Result<()> {
    let bad = |what: &str| Err(err(ErrorKind::InvalidInput, "VideoEncoder::configure", what.to_string()));
    if cfg.width < 64 || cfg.height < 64 || cfg.width > 4096 || cfg.height > 4096 {
        return bad("size must be 64 to 4096 pixels per side");
    }
    if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
        return bad("the encoder needs an even width and height");
    }
    if cfg.fps == 0 || cfg.fps > 240 {
        return bad("fps must be 1 to 240");
    }
    if cfg.kbps < 100 {
        return bad("kbps must be at least 100");
    }
    Ok(())
}

impl VideoEncoder for NvencEncoder {
    fn configure(&mut self, config: &EncoderConfig) -> Result<EncoderInfo> {
        self.active = None;
        validate(config)?;
        sys::init_thread()?;
        let api = Rc::new(NvencApi::load(self.options.library.as_deref().unwrap_or(DEFAULT_LIBRARY))?);
        let gpu = match &self.options.device {
            Some(d) => Gpu::from_device(d.clone())?,
            None => Gpu::on_adapter(config.adapter)?,
        };
        if gpu.adapter.vendor_id != 0x10DE {
            return Err(err(
                ErrorKind::Unsupported,
                "VideoEncoder::configure",
                format!("NVENC needs an NVIDIA adapter, this is {}", gpu.adapter.name),
            ));
        }
        let active = Active::build(api, gpu, *config, self.options.clone())?;
        let info = active.info.clone();
        self.last_cfg = Some(*config);
        self.active = Some(active);
        Ok(info)
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        if self.active.is_none() {
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
            // A display size change: a new session, new SPS/PPS, the first frame an IDR with `config_change`.
            let mut cfg = self.active_mut()?.cfg;
            cfg.width = frame.width;
            cfg.height = frame.height;
            self.configure(&cfg)?;
        }
        self.active_mut()?.encode(frame, force_keyframe)
    }

    fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()> {
        self.active_mut()?.set_rate(kbps, fps)
    }

    fn refine(&mut self, frame: &Frame) -> Result<Vec<EncodedFrame>> {
        let (w, h) = {
            let a = self.active_mut()?;
            (a.cfg.width, a.cfg.height)
        };
        if frame.width != w || frame.height != h {
            return Ok(Vec::new()); // the display changed size since the picture was captured: nothing to refine
        }
        let (out, restored) = self.active_mut()?.refine(frame)?;
        if !restored {
            // Constant QP would stay in force: the next `encode` starts a new session from the last configuration.
            self.active = None;
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        // Nothing is buffered (synchronous session, no B-frames, no lookahead): the session just ends, and the next
        // `encode` starts again from the last configuration, as the Media Foundation encoder does.
        self.active = None;
        Ok(Vec::new())
    }
}

/// Whether the GPU reports temporal AQ (`NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ`).
fn temporal_aq_supported(api: &NvencApi, session: *mut c_void) -> bool {
    let Some(get_caps) = api.fns.nvEncGetEncodeCaps else { return false };
    let mut param = zeroed::<NV_ENC_CAPS_PARAM>();
    param.version = NV_ENC_CAPS_PARAM_VER;
    param.capsToQuery = NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ;
    let mut value = 0i32;
    // SAFETY: valid session, a caps request with its version set and a valid out pointer.
    let status = unsafe { get_caps(session, NV_ENC_CODEC_H264_GUID, &mut *param, &mut value) };
    status == NV_ENC_SUCCESS && value != 0
}

/// Rate-control block for a bitrate: CBR (Motion) or VBR with a 2x ceiling (Auto and Text, where a still screen costs few
/// bits), and a buffer of three frames so an IDR of a sharp screen fits without a long delay.
fn set_rate_control(rc: &mut NV_ENC_RC_PARAMS, cbr: bool, kbps: u32, fps: u32) {
    let avg = kbps.saturating_mul(1000);
    let max = if cbr { avg } else { avg.saturating_mul(2) };
    rc.version = NV_ENC_RC_PARAMS_VER;
    rc.rateControlMode = if cbr { NV_ENC_PARAMS_RC_CBR } else { NV_ENC_PARAMS_RC_VBR };
    rc.averageBitRate = avg;
    rc.maxBitRate = max;
    let vbv = ((u64::from(max) * 3) / u64::from(fps.max(1))).min(u64::from(u32::MAX)) as u32;
    rc.vbvBufferSize = vbv;
    rc.vbvInitialDelay = vbv;
    rc.bits |= RC_ZERO_REORDER_DELAY;
    rc.multiPass = 0;
}

impl Active {
    fn check(&self, status: u32, op: &'static str) -> Result<()> {
        check(&self.api, self.session, status, op)
    }

    fn build(api: Rc<NvencApi>, gpu: Gpu, cfg: EncoderConfig, options: NvencOptions) -> Result<Self> {
        let eff = effective(cfg.preset, cfg.kbps, cfg.fps);
        let plan = preset_plan(cfg.preset);
        let coded_fps = eff.output_fps;
        let f = &api.fns;
        let missing =
            |name: &'static str| err(ErrorKind::Unsupported, "NVENC::load", format!("the driver table has no {name}"));
        let open = f.nvEncOpenEncodeSessionEx.ok_or_else(|| missing("nvEncOpenEncodeSessionEx"))?;
        let get_preset = f.nvEncGetEncodePresetConfigEx.ok_or_else(|| missing("nvEncGetEncodePresetConfigEx"))?;
        let initialize = f.nvEncInitializeEncoder.ok_or_else(|| missing("nvEncInitializeEncoder"))?;
        let create_bits = f.nvEncCreateBitstreamBuffer.ok_or_else(|| missing("nvEncCreateBitstreamBuffer"))?;

        // Session on the D3D11 device.
        let mut open_params = zeroed::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>();
        open_params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        open_params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        open_params.device = Interface::as_raw(&gpu.device);
        open_params.apiVersion = API_VERSION;
        let mut session: *mut c_void = std::ptr::null_mut();
        // SAFETY: `open_params` is a fully initialised session request and the out pointer is valid.
        let status = unsafe { open(&mut *open_params, &mut session) };
        if status != NV_ENC_SUCCESS || session.is_null() {
            return Err(err(
                ErrorKind::Unavailable,
                "nvEncOpenEncodeSessionEx",
                format!("NV_ENC_ERR_{} ({status})", status_name(status)),
            ));
        }
        // From here on the session is destroyed on every error path.
        let guard = SessionGuard { api: api.clone(), session };

        // The driver's own preset, then ours on top.
        let tuning = options.tuning.unwrap_or_else(|| NvencTuning::for_preset(cfg.preset));
        let (preset_no, preset_guid) = (tuning.preset.clamp(1, 5), tuning.guid());
        let mut pc = zeroed::<NV_ENC_PRESET_CONFIG>();
        pc.version = NV_ENC_PRESET_CONFIG_VER;
        pc.presetCfg.version = NV_ENC_CONFIG_VER;
        // SAFETY: valid session and a preset-config struct with its versions set.
        let status = unsafe {
            get_preset(session, NV_ENC_CODEC_H264_GUID, preset_guid, NV_ENC_TUNING_INFO_LOW_LATENCY, &mut *pc)
        };
        check(&api, session, status, "nvEncGetEncodePresetConfigEx")?;
        let mut config = zeroed::<NV_ENC_CONFIG>();
        *config = pc.presetCfg;
        drop(pc);
        config.version = NV_ENC_CONFIG_VER;
        config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        let gop = options.gop_frames.unwrap_or(NVENC_INFINITE_GOPLENGTH);
        config.gopLength = gop;
        config.frameIntervalP = 1; // no B-frames
        let cbr = plan.rate_modes.first() == Some(&RATE_CBR);
        set_rate_control(&mut config.rcParams, cbr, eff.kbps, coded_fps);
        // Adaptive quantization is set explicitly: the driver's preset values are not relied on.
        let temporal = tuning.temporal_aq && temporal_aq_supported(&api, session);
        let spatial = tuning.spatial_aq || (tuning.temporal_aq && !temporal);
        config.rcParams.bits &= !(RC_ENABLE_AQ | RC_ENABLE_TEMPORAL_AQ);
        if spatial {
            config.rcParams.bits |= RC_ENABLE_AQ;
        }
        if temporal {
            config.rcParams.bits |= RC_ENABLE_TEMPORAL_AQ;
        }
        let h = &mut config.h264Config;
        h.idrPeriod = gop;
        h.bits |= H264_REPEAT_SPSPPS;
        h.level = 0; // autoselect
        h.chromaFormatIDC = 1; // 4:2:0, the only format every decoder here takes
        // L3: say what the stream is. NVENC converts the ARGB input to YUV itself and documents no choice of matrix, so
        // the VUI states BT.709 limited range (what the decoder and the renderer assume) and the colour test
        // (`color_patches_round_trip_within_delta_e`) checks what the hardware really does.
        let vui = &mut h.h264VUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = NV_ENC_VUI_BT709;
        vui.transferCharacteristics = NV_ENC_VUI_BT709;
        vui.colourMatrix = NV_ENC_VUI_BT709;
        if let Some(frames) = options.intra_refresh_frames {
            h.bits |= H264_ENABLE_INTRA_REFRESH;
            h.intraRefreshCnt = frames.max(2);
            h.intraRefreshPeriod = frames.max(2).saturating_mul(4);
        }

        let mut init = zeroed::<NV_ENC_INITIALIZE_PARAMS>();
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = NV_ENC_CODEC_H264_GUID;
        init.presetGUID = preset_guid;
        init.encodeWidth = cfg.width;
        init.encodeHeight = cfg.height;
        init.darWidth = cfg.width;
        init.darHeight = cfg.height;
        init.frameRateNum = coded_fps;
        init.frameRateDen = 1;
        init.enableEncodeAsync = 0;
        init.enablePTD = 1;
        init.encodeConfig = &mut *config;
        init.tuningInfo = NV_ENC_TUNING_INFO_LOW_LATENCY;
        init.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
        // SAFETY: `init` is complete and `encodeConfig` points into the boxed config that outlives the session.
        let status = unsafe { initialize(session, &mut *init) };
        check(&api, session, status, "nvEncInitializeEncoder")?;

        let mut bits = zeroed::<NV_ENC_CREATE_BITSTREAM_BUFFER>();
        bits.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        bits.size = cfg.width * cfg.height * 3 / 2; // ignored by current drivers
        bits.memoryHeap = NV_ENC_MEMORY_HEAP_AUTOSELECT;
        // SAFETY: valid session and request.
        let status = unsafe { create_bits(session, &mut *bits) };
        check(&api, session, status, "nvEncCreateBitstreamBuffer")?;
        let bitstream = bits.bitstreamBuffer;

        let seq_header = sequence_header(&api, session);
        let level = level_idc(cfg.width, cfg.height, coded_fps);
        let info = EncoderInfo {
            name: format!("NVIDIA NVENC H.264 on {}", gpu.adapter.name),
            hardware: true,
            profile: H264Profile::High,
            level: level as u8,
            low_latency: true,
        };
        let summary = NvencSummary {
            adapter_name: gpu.adapter.name.clone(),
            driver_api: (api.driver_version_code >> 4, api.driver_version_code & 0xf),
            header_api: (API_MAJOR, API_MINOR),
            preset: preset_no,
            spatial_aq: config.rcParams.bits & RC_ENABLE_AQ != 0,
            temporal_aq: config.rcParams.bits & RC_ENABLE_TEMPORAL_AQ != 0,
            rate_control: config.rcParams.rateControlMode,
        };
        let skip_interval_us = if eff.output_fps < eff.input_fps { 1_000_000 / u64::from(eff.output_fps) } else { 0 };
        std::mem::forget(guard); // `Active::drop` owns the session from now on
        Ok(Self {
            cfg,
            api,
            session,
            gpu,
            init,
            config,
            bitstream,
            registered: Vec::new(),
            cpu_in: None,
            copy_in: None,
            needs_copy: false,
            last_copied: false,
            info,
            summary,
            coded_fps,
            kbps_target: eff.kbps,
            skip_interval_us,
            last_encoded_at: None,
            seq: 0,
            seq_header,
            config_pending: true,
            force_next: false,
            submit_us: 0,
        })
    }

    fn bgra_texture(&self) -> Result<ID3D11Texture2D> {
        self.gpu.texture(
            self.cfg.width,
            self.cfg.height,
            BGRA,
            USAGE_DEFAULT,
            windows::Win32::Graphics::Direct3D11::D3D11_BIND_FLAG(
                D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0,
            ),
            D3D11_CPU_ACCESS_FLAG(0),
        )
    }

    /// The texture holding this frame as it came (CPU frames are uploaded first).
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
                if self.cpu_in.is_none() {
                    self.cpu_in = Some(self.bgra_texture()?);
                }
                let tex = self.cpu_in.clone().expect("created above");
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
                        "the texture belongs to another D3D11 device: pass the capture device in NvencOptions::device",
                    ));
                }
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                // SAFETY: GetDesc only fills the struct.
                unsafe { tex.GetDesc(&mut desc) };
                if desc.Format != BGRA || desc.Width != self.cfg.width || desc.Height != self.cfg.height {
                    return Err(err(
                        ErrorKind::InvalidInput,
                        "VideoEncoder::encode",
                        "GPU frame is not BGRA of the configured size",
                    ));
                }
                Ok(tex.clone())
            }
        }
    }

    /// The registered handle of a texture, registering it (once) when new.
    fn register(&mut self, tex: &ID3D11Texture2D) -> Result<*mut c_void> {
        let raw = Interface::as_raw(tex);
        if let Some(r) = self.registered.iter().find(|r| Interface::as_raw(&r.texture) == raw) {
            return Ok(r.handle);
        }
        let register = self
            .api
            .fns
            .nvEncRegisterResource
            .ok_or_else(|| err(ErrorKind::Unsupported, "NVENC", "the driver table has no nvEncRegisterResource"))?;
        let mut reg = zeroed::<NV_ENC_REGISTER_RESOURCE>();
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.width = self.cfg.width;
        reg.height = self.cfg.height;
        reg.pitch = 0;
        reg.subResourceIndex = 0;
        reg.resourceToRegister = raw;
        reg.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
        reg.bufferUsage = NV_ENC_INPUT_IMAGE;
        // SAFETY: valid session; `raw` is a live texture on the session's device, kept alive by the registry entry.
        let status = unsafe { register(self.session, &mut *reg) };
        self.check(status, "nvEncRegisterResource")?;
        if self.registered.len() >= REGISTRY {
            let old = self.registered.remove(0);
            self.unregister(old.handle);
        }
        let handle = reg.registeredResource;
        self.registered.push(Registered { texture: tex.clone(), handle });
        Ok(handle)
    }

    fn unregister(&self, handle: *mut c_void) {
        if let Some(f) = self.api.fns.nvEncUnregisterResource {
            // SAFETY: `handle` came from nvEncRegisterResource on this session and is unregistered once.
            let _ = unsafe { f(self.session, handle) };
        }
    }

    /// A registered handle for this frame's texture; falls back to a GPU copy when the driver refuses the texture.
    fn input_handle(&mut self, src: &ID3D11Texture2D) -> Result<*mut c_void> {
        self.last_copied = false;
        if !self.needs_copy {
            match self.register(src) {
                Ok(h) => return Ok(h),
                Err(first) => {
                    self.needs_copy = true;
                    if self.copy_in.is_none() {
                        self.copy_in = Some(self.bgra_texture().map_err(|_| first.clone())?);
                    }
                }
            }
        }
        if self.copy_in.is_none() {
            self.copy_in = Some(self.bgra_texture()?);
        }
        let dst = self.copy_in.clone().expect("created above");
        let (d, s): (ID3D11Resource, ID3D11Resource) =
            (dst.cast().ctx("texture as resource")?, src.cast().ctx("texture as resource")?);
        // SAFETY: both textures are BGRA of the same size on this device.
        unsafe { self.gpu.context.CopyResource(&d, &s) };
        self.last_copied = true;
        self.register(&dst)
    }

    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> Result<Vec<EncodedFrame>> {
        let t0 = Instant::now();
        if self.skip_interval_us > 0
            && !force_keyframe
            && !self.force_next
            && let Some(last) = self.last_encoded_at
            && frame.captured_at.0.saturating_sub(last) < self.skip_interval_us * 9 / 10
        {
            return Ok(Vec::new());
        }
        let src = self.source_texture(frame)?;
        let handle = self.input_handle(&src)?;

        let map = self
            .api
            .fns
            .nvEncMapInputResource
            .ok_or_else(|| err(ErrorKind::Unsupported, "NVENC", "no nvEncMapInputResource"))?;
        let mut mapped = zeroed::<NV_ENC_MAP_INPUT_RESOURCE>();
        mapped.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        mapped.registeredResource = handle;
        // SAFETY: `handle` is registered on this session.
        let status = unsafe { map(self.session, &mut *mapped) };
        self.check(status, "nvEncMapInputResource")?;
        let result = self.encode_mapped(&mapped, frame, force_keyframe || self.force_next, t0);
        if let Some(unmap) = self.api.fns.nvEncUnmapInputResource {
            // SAFETY: the resource was mapped just above and is unmapped once.
            let _ = unsafe { unmap(self.session, mapped.mappedResource) };
        }
        self.last_encoded_at = Some(frame.captured_at.0);
        self.force_next = false;
        Ok(result?.into_iter().collect())
    }

    fn encode_mapped(
        &mut self,
        mapped: &NV_ENC_MAP_INPUT_RESOURCE,
        frame: &Frame,
        force: bool,
        t0: Instant,
    ) -> Result<Option<EncodedFrame>> {
        let encode = self
            .api
            .fns
            .nvEncEncodePicture
            .ok_or_else(|| err(ErrorKind::Unsupported, "NVENC", "no nvEncEncodePicture"))?;
        let mut pic = zeroed::<NV_ENC_PIC_PARAMS>();
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = self.cfg.width;
        pic.inputHeight = self.cfg.height;
        pic.inputPitch = self.cfg.width;
        pic.encodePicFlags =
            if force || self.seq == 0 { NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS } else { 0 };
        pic.inputTimeStamp = self.seq;
        pic.inputBuffer = mapped.mappedResource;
        pic.outputBitstream = self.bitstream;
        pic.bufferFmt = mapped.mappedBufferFmt;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        self.seq += 1;
        // SAFETY: valid session, mapped input and output buffer; the call blocks until the picture is encoded.
        let status = unsafe { encode(self.session, &mut *pic) };
        self.submit_us = t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        if status == NV_ENC_ERR_NEED_MORE_INPUT || status == NV_ENC_ERR_NEED_MORE_OUTPUT {
            return Ok(None);
        }
        self.check(status, "nvEncEncodePicture")?;
        let data = self.take_bitstream()?;
        Ok(Some(self.finish_frame(frame, data, t0)))
    }

    /// Locks the output buffer, copies the Annex B bytes out and unlocks it. Returns the picture type with the bytes.
    fn take_bitstream(&mut self) -> Result<(Vec<u8>, u32)> {
        let (Some(lock_fn), Some(unlock_fn)) = (self.api.fns.nvEncLockBitstream, self.api.fns.nvEncUnlockBitstream)
        else {
            return Err(err(ErrorKind::Unsupported, "NVENC", "no nvEncLockBitstream"));
        };
        let mut lock = zeroed::<NV_ENC_LOCK_BITSTREAM>();
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        // SAFETY: valid session and a bitstream buffer created on it.
        let status = unsafe { lock_fn(self.session, &mut *lock) };
        self.check(status, "nvEncLockBitstream")?;
        // SAFETY: while locked, `bitstreamBufferPtr` addresses `bitstreamSizeInBytes` valid bytes.
        let data = unsafe {
            std::slice::from_raw_parts(lock.bitstreamBufferPtr.cast::<u8>(), lock.bitstreamSizeInBytes as usize)
                .to_vec()
        };
        let pic_type = lock.pictureType;
        // SAFETY: unlocks the buffer locked above.
        let status = unsafe { unlock_fn(self.session, self.bitstream) };
        self.check(status, "nvEncUnlockBitstream")?;
        Ok((data, pic_type))
    }

    fn finish_frame(&mut self, frame: &Frame, (mut data, pic_type): (Vec<u8>, u32), t0: Instant) -> EncodedFrame {
        let keyframe = pic_type == NV_ENC_PIC_TYPE_IDR || annexb::is_idr(&data);
        if annexb::contains(&data, annexb::NAL_SPS) {
            let ps = annexb::parameter_sets(&data);
            if !ps.is_empty() {
                self.seq_header = ps;
            }
        } else if keyframe && !self.seq_header.is_empty() {
            let mut with = self.seq_header.clone();
            with.extend_from_slice(&data);
            data = with;
        }
        let config_change = keyframe && annexb::contains(&data, annexb::NAL_SPS) && self.config_pending;
        if config_change {
            self.config_pending = false;
        }
        EncodedFrame {
            captured_at: frame.captured_at,
            encode_us: t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32,
            keyframe,
            config_change,
            width: self.cfg.width as u16,
            height: self.cfg.height as u16,
            annex_b: data,
        }
    }

    /// Pushes `self.config` and `self.init` to the live session (`nvEncReconfigureEncoder`; neither a reset nor an IDR).
    fn reconfigure_now(&mut self) -> Result<()> {
        let reconfigure = self
            .api
            .fns
            .nvEncReconfigureEncoder
            .ok_or_else(|| err(ErrorKind::Unsupported, "NVENC", "no nvEncReconfigureEncoder"))?;
        self.init.encodeConfig = &mut *self.config;
        let mut params = zeroed::<NV_ENC_RECONFIGURE_PARAMS>();
        params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        params.reInitEncodeParams = *self.init;
        params.bits = 0; // neither a reset nor an IDR
        // SAFETY: valid session; the embedded init params point at the boxed config.
        let status = unsafe { reconfigure(self.session, &mut *params) };
        self.check(status, "nvEncReconfigureEncoder")
    }

    /// L4: one IDR of `frame` at a constant low QP. `NV_ENC_PIC_PARAMS` carries no per-picture QP (only `qpDeltaMap`,
    /// which needs `qpMapMode` set at initialisation), so the rate control is switched to `NV_ENC_PARAMS_RC_CONSTQP`
    /// with `nvEncReconfigureEncoder` for this one picture and put back after it: the programming guide (13.0,
    /// section 8.4) lists what a reconfigure cannot change (GOP structure, sync mode, max size, `enablePTD`, bit depth,
    /// chroma format) and rate-control mode is not among them. The second value is whether the normal rate control
    /// is back in force (when it is not, the caller must rebuild the session).
    fn refine(&mut self, frame: &Frame) -> Result<(Vec<EncodedFrame>, bool)> {
        let saved = self.config.rcParams;
        let mut rc = saved;
        rc.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
        rc.constQP = NV_ENC_QP { qpInterP: REFINE_QP, qpInterB: REFINE_QP, qpIntra: REFINE_QP };
        self.config.rcParams = rc;
        if let Err(e) = self.reconfigure_now() {
            self.config.rcParams = saved;
            return Err(e);
        }
        let out = self.encode(frame, true);
        self.config.rcParams = saved;
        let restored = self.reconfigure_now().is_ok();
        Ok((out?, restored))
    }

    /// New bitrate and frame rate while streaming (`nvEncReconfigureEncoder`, no IDR, no new SPS/PPS).
    fn set_rate(&mut self, kbps: u32, fps: u16) -> Result<()> {
        let eff = effective(self.cfg.preset, kbps, fps);
        let cbr = self.config.rcParams.rateControlMode == NV_ENC_PARAMS_RC_CBR;
        set_rate_control(&mut self.config.rcParams, cbr, eff.kbps, eff.output_fps);
        self.init.frameRateNum = eff.output_fps;
        self.init.frameRateDen = 1;
        self.reconfigure_now()?;
        self.cfg.kbps = kbps;
        self.cfg.fps = fps;
        self.coded_fps = eff.output_fps;
        self.kbps_target = eff.kbps;
        self.skip_interval_us = if eff.output_fps < eff.input_fps { 1_000_000 / u64::from(eff.output_fps) } else { 0 };
        Ok(())
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        for r in std::mem::take(&mut self.registered) {
            self.unregister(r.handle);
        }
        // SAFETY: orderly teardown of resources created on this session; errors are irrelevant at this point.
        unsafe {
            if let Some(f) = self.api.fns.nvEncDestroyBitstreamBuffer {
                let _ = f(self.session, self.bitstream);
            }
            if let Some(f) = self.api.fns.nvEncDestroyEncoder {
                let _ = f(self.session);
            }
        }
    }
}

/// Destroys a half-built session on an error path.
struct SessionGuard {
    api: Rc<NvencApi>,
    session: *mut c_void,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(f) = self.api.fns.nvEncDestroyEncoder {
            // SAFETY: the session was opened by `Active::build` and is destroyed once.
            let _ = unsafe { f(self.session) };
        }
    }
}

/// Turns a non-success status into an error carrying the driver's own message.
fn check(api: &NvencApi, session: *mut c_void, status: u32, op: &'static str) -> Result<()> {
    if status == NV_ENC_SUCCESS {
        return Ok(());
    }
    let mut detail = format!("NV_ENC_ERR_{} ({status})", status_name(status));
    if let Some(last) = api.fns.nvEncGetLastErrorString {
        // SAFETY: returns a NUL-terminated string owned by the driver, or null.
        let p = unsafe { last(session) };
        if !p.is_null() {
            // SAFETY: non-null, NUL-terminated.
            let text = unsafe { CStr::from_ptr(p) }.to_string_lossy();
            if !text.is_empty() {
                detail.push_str(": ");
                detail.push_str(&text);
            }
        }
    }
    let kind = if status == NV_ENC_ERR_INVALID_VERSION { ErrorKind::Unsupported } else { ErrorKind::Os };
    Err(swapdesk_platform::PlatformError { kind, op, os_code: Some(i64::from(status)), detail })
}

/// SPS and PPS the driver reports for the session, as Annex B.
fn sequence_header(api: &NvencApi, session: *mut c_void) -> Vec<u8> {
    let Some(get) = api.fns.nvEncGetSequenceParams else { return Vec::new() };
    let mut buf = vec![0u8; 512];
    let mut written = 0u32;
    let mut payload = zeroed::<NV_ENC_SEQUENCE_PARAM_PAYLOAD>();
    payload.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
    payload.inBufferSize = buf.len() as u32;
    payload.spsppsBuffer = buf.as_mut_ptr().cast();
    payload.outSPSPPSPayloadSize = &mut written;
    // SAFETY: the buffer and the size out pointer outlive the call.
    let status = unsafe { get(session, &mut *payload) };
    if status != NV_ENC_SUCCESS {
        return Vec::new();
    }
    buf.truncate((written as usize).min(512));
    annexb::parameter_sets(&buf)
}
