//! `VideoDecoder` over the Media Foundation H.264 decoder MFT (the inbox transform, which decodes on the GPU through DXVA
//! when given a D3D11 device manager and in software otherwise).
//!
//! Low-latency mode, no reordering: one Annex B access unit in, at most one picture out. The picture is an NV12 D3D11
//! texture ([`DecoderOutput::Gpu`], for zero-copy rendering later) or a BGRA copy in system memory
//! ([`DecoderOutput::CpuBgra`], for tests and the barcode reader). The MFT decodes 4:2:0 only (windows.md 3b).

use std::collections::VecDeque;
use std::time::Instant;

use swapdesk_platform::{
    AdapterId, DecodedFrame, DisplayId, ErrorKind, GpuSurface, MonoMicros, Pixels, Result, VideoDecoder,
};
use swapdesk_proto::VideoHeader;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_FLAG, D3D11_TEXTURE2D_DESC,
    ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVLowLatencyMode, ICodecAPI, IMFDXGIBuffer, IMFDXGIDeviceManager, IMFSample, IMFTransform,
    MF_E_NOTACCEPTING, MF_MT_FRAME_SIZE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_SA_D3D11_BINDFLAGS,
    MFCreateDXGIDeviceManager, MFSampleExtension_CleanPoint, MFSampleExtension_Discontinuity,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::core::Interface;

use crate::gpu::{BGRA, Gpu, NV12, Space, Staging, USAGE_DEFAULT, VideoProc};
use crate::mf::{
    Output, OutputInfo, empty_sample, memory_sample, output_info, process_output, set_bool, unpack, video_type,
};
use crate::mft::{self, Category};
use crate::sys::{Ctx, err};

/// Output textures of [`DecoderOutput::Gpu`], reused round-robin.
pub const GPU_RING: usize = 6;

/// Pictures between two `Flush` calls on the immediate context (queued GPU work and deferred destruction do not pile up).
const FLUSH_EVERY: u64 = 60;

/// What the decoder hands back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecoderOutput {
    /// `Pixels::Cpu`: BGRA rows, `stride = width * 4`.
    #[default]
    CpuBgra,
    /// `Pixels::Gpu` holding an `ID3D11Texture2D` in NV12 (exactly the picture size, one slice). The textures are a ring of
    /// [`GPU_RING`] reused round-robin (a texture per picture leaked about 2 KB of private bytes a frame): a picture
    /// stays valid until the decoder has produced `GPU_RING` more, which is far longer than a renderer holds one.
    Gpu,
}

#[derive(Clone)]
pub struct DecoderOptions {
    pub output: DecoderOutput,
    /// The adapter whose GPU decodes (and converts). Ignored when `device` is set.
    pub adapter: AdapterId,
    /// The device the renderer also uses (zero-copy path); its adapter overrides `adapter`.
    pub device: Option<windows::Win32::Graphics::Direct3D11::ID3D11Device>,
    /// Decode on the GPU through DXVA; `false` decodes in software on the CPU (measurements, fallback).
    pub dxva: bool,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self { output: DecoderOutput::CpuBgra, adapter: AdapterId(0), device: None, dxva: true }
    }
}

struct Pending {
    time: i64,
    display: DisplayId,
    frame_id: u32,
    captured_at: MonoMicros,
}

/// Decoder over the inbox H.264 MFT. See the module documentation.
pub struct MfH264Decoder {
    options: DecoderOptions,
    state: Option<State>,
    waiting_keyframe: bool,
    last_id: Option<u32>,
    /// Header of the newest input, the fallback metadata of a picture collected by `poll`.
    last_header: Option<VideoHeader>,
    seq: i64,
    decode_us: u32,
}

/// The NV12 to BGRA processor with the texture size and the visible size it was built for.
type Converter = (VideoProc, (u32, u32), (u32, u32));

struct State {
    gpu: Gpu,
    transform: IMFTransform,
    activation: mft::Mft,
    _codec: Option<ICodecAPI>,
    _manager: Option<IMFDXGIDeviceManager>,
    out_info: OutputInfo,
    dxva: bool,
    pending: VecDeque<Pending>,
    /// Visible picture size from the current output type.
    size: (u32, u32),
    proc: Option<Converter>,
    bgra: Option<ID3D11Texture2D>,
    staging: Option<Staging>,
    frame_ms: String,
    /// Pictures that were not ready when `ProcessInput` returned and came out on a later call.
    late_pictures: u32,
    /// An input was fed and its picture has not come out yet.
    owed: bool,
    /// The NV12 textures of the GPU output, their size, and the one to fill next.
    ring: Vec<ID3D11Texture2D>,
    ring_size: (u32, u32),
    ring_next: usize,
    copies: u64,
}

impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: orderly shutdown messages to a live transform.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
        }
        mft::shutdown(&self.transform);
        self.activation.shutdown_object();
    }
}

impl MfH264Decoder {
    pub fn new(options: DecoderOptions) -> Self {
        Self { options, state: None, waiting_keyframe: true, last_id: None, last_header: None, seq: 0, decode_us: 0 }
    }

    /// Microseconds the last `decode` call took (input to picture).
    pub fn last_decode_us(&self) -> u32 {
        self.decode_us
    }

    /// Name of the decoder MFT once it is running, and whether it decodes through DXVA.
    pub fn mft_name(&self) -> Option<(&str, bool)> {
        self.state.as_ref().map(|s| (s.frame_ms.as_str(), s.dxva))
    }

    /// Pictures that were not ready right after the input and came out on a later call (GPU busy).
    pub fn late_pictures(&self) -> u32 {
        self.state.as_ref().map_or(0, |s| s.late_pictures)
    }

    /// True while a fed input has no picture out yet (`decode` returned `None` for it): `poll` may collect it.
    pub fn owes_picture(&self) -> bool {
        !self.waiting_keyframe && self.state.as_ref().is_some_and(|s| s.owed)
    }

    /// Non-blocking: the picture a late input left behind, if it is out now. Never waits, so the last frame before a pause can still be shown.
    pub fn poll(&mut self) -> Result<Option<DecodedFrame>> {
        let output = self.options.output;
        let (Some(state), Some(header)) = (self.state.as_mut(), self.last_header) else { return Ok(None) };
        if self.waiting_keyframe || !state.owed {
            return Ok(None);
        }
        let mut result = None;
        let pulled = (|| -> Result<()> {
            while let Some(frame) = state.pull(output, &header)? {
                result = Some(frame);
            }
            Ok(())
        })();
        if let Err(e) = pulled {
            self.waiting_keyframe = true;
            return Err(e);
        }
        if result.is_some() {
            state.owed = false;
            state.late_pictures += 1;
        }
        Ok(result)
    }

    /// `decode`, then waits (polling, at most 40 ms) for a late picture of this very input. For tests, tools and measurements that count one picture per
    /// access unit; the viewer's decode thread never calls it (it uses `decode` and `poll`).
    pub fn decode_settled(&mut self, header: &VideoHeader, annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
        let got = self.decode(header, annex_b)?;
        if got.is_some() {
            return Ok(got);
        }
        let deadline = Instant::now() + std::time::Duration::from_millis(40);
        while self.owes_picture() && Instant::now() < deadline {
            if let Some(frame) = self.poll()? {
                return Ok(Some(frame));
            }
            std::thread::sleep(std::time::Duration::from_micros(300));
        }
        Ok(None)
    }

    /// True after `reset` or a detected gap, until a keyframe arrives.
    pub fn waiting_for_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    fn start(&mut self) -> Result<()> {
        crate::sys::init_thread()?;
        let gpu = match &self.options.device {
            Some(d) => Gpu::from_device(d.clone())?,
            None => Gpu::on_adapter(self.options.adapter)?,
        };
        if !self.options.dxva && self.options.output == DecoderOutput::Gpu {
            return Err(err(
                ErrorKind::InvalidInput,
                "VideoDecoder",
                "GPU output needs DXVA decoding (DecoderOptions::dxva)",
            ));
        }
        let candidates = mft::software(Category::Decoder)?;
        let mft = candidates
            .into_iter()
            .next()
            .ok_or_else(|| err(ErrorKind::Unavailable, "VideoDecoder", "no H.264 decoder MFT"))?;
        let transform = mft.activate()?;
        // Any `?` below shuts the transform down; `State`'s own `Drop` takes over once it is built.
        let mut guard = mft::ActivationGuard::new(&transform, &mft);
        let mut manager = None;
        let mut dxva = false;
        // SAFETY: messages and attribute writes on the transform we just activated.
        unsafe {
            if self.options.dxva {
                let mut token = 0u32;
                let mut m = None;
                MFCreateDXGIDeviceManager(&mut token, &mut m).ctx("MFCreateDXGIDeviceManager")?;
                let m = m.ok_or_else(|| err(ErrorKind::Os, "MFCreateDXGIDeviceManager", "no manager"))?;
                m.ResetDevice(&gpu.device, token).ctx("IMFDXGIDeviceManager::ResetDevice")?;
                if transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, Interface::as_raw(&m) as usize).is_ok() {
                    dxva = true;
                    manager = Some(m);
                    // Output textures the video processor and a renderer can read.
                    if let Ok(attrs) = transform.GetOutputStreamAttributes(0) {
                        let _ = attrs.SetUINT32(&MF_SA_D3D11_BINDFLAGS, D3D11_BIND_SHADER_RESOURCE.0 as u32);
                    }
                }
            }
        }
        let codec = mft::codec_api(&transform);
        if let Some(c) = &codec {
            // The decoder types this flag VT_UI4 where the encoders take VT_BOOL.
            let _ = crate::mf::set_u32(c, &CODECAPI_AVLowLatencyMode, 1)
                .or_else(|_| set_bool(c, &CODECAPI_AVLowLatencyMode, true));
        }
        // `MF_LOW_LATENCY` is what makes this decoder output each picture at once (the property above alone did not when
        // it was typed VT_BOOL); without it the decoder holds about nine frames back.
        // SAFETY: attribute write on the transform.
        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&windows::Win32::Media::MediaFoundation::MF_LOW_LATENCY, 1);
            }
        }
        // SAFETY: offers the input type, then the first NV12 output type the MFT lists.
        unsafe {
            let input = video_type(&MFVideoFormat_H264, 0, 0, 0)?;
            transform.SetInputType(0, &input, 0).ctx("IMFTransform::SetInputType(H264)")?;
        }
        let mut state = State {
            gpu,
            transform,
            activation: mft.clone(),
            _codec: codec,
            _manager: manager,
            out_info: OutputInfo { provides_samples: true, buffer_size: 0 },
            dxva,
            pending: VecDeque::new(),
            size: (0, 0),
            proc: None,
            bgra: None,
            staging: None,
            frame_ms: mft.name.clone(),
            late_pictures: 0,
            owed: false,
            ring: Vec::new(),
            ring_size: (0, 0),
            ring_next: 0,
            copies: 0,
        };
        guard.disarm();
        let _ = state.negotiate_output();
        // SAFETY: streaming start messages.
        unsafe {
            state.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).ctx("BEGIN_STREAMING")?;
            state.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).ctx("START_OF_STREAM")?;
        }
        self.state = Some(state);
        Ok(())
    }
}

impl State {
    /// Picks the first NV12 output type and reads the visible size. `Err` when the MFT lists none yet.
    fn negotiate_output(&mut self) -> Result<()> {
        // SAFETY: type enumeration and selection on a live transform.
        unsafe {
            for i in 0.. {
                let t = match self.transform.GetOutputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(_) => return Err(err(ErrorKind::Unavailable, "VideoDecoder", "no NV12 output type offered")),
                };
                if t.GetGUID(&MF_MT_SUBTYPE).map(|g| g == MFVideoFormat_NV12).unwrap_or(false) {
                    self.transform.SetOutputType(0, &t, 0).ctx("IMFTransform::SetOutputType(NV12)")?;
                    break;
                }
            }
            self.out_info = output_info(&self.transform)?;
            let cur = self.transform.GetOutputCurrentType(0).ctx("GetOutputCurrentType")?;
            let (w, h) = cur.GetUINT64(&MF_MT_FRAME_SIZE).map(unpack).unwrap_or((0, 0));
            let mut area = [0u8; 16];
            let mut size = (w, h);
            if cur.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut area, None).is_ok() {
                // MFVideoArea: OffsetX (fract u16, value i16), OffsetY, Area (cx i32, cy i32).
                let cx = i32::from_le_bytes([area[8], area[9], area[10], area[11]]);
                let cy = i32::from_le_bytes([area[12], area[13], area[14], area[15]]);
                if cx > 0 && cy > 0 {
                    size = (cx as u32, cy as u32);
                }
            }
            self.size = size;
        }
        Ok(())
    }

    /// One `ProcessOutput`: a finished picture, `None` when the MFT wants more input. Output type changes are handled here.
    fn pull(&mut self, output: DecoderOutput, header: &VideoHeader) -> Result<Option<DecodedFrame>> {
        loop {
            let provide = if self.out_info.provides_samples {
                None
            } else {
                Some(empty_sample(self.out_info.buffer_size.max(self.size.0 * self.size.1 * 3 / 2))?)
            };
            match process_output(&self.transform, provide)? {
                Output::Sample(out) => {
                    // SAFETY: a getter on the sample.
                    let t = unsafe { out.GetSampleTime() }.unwrap_or(-1);
                    let (pixels, w, h) = self.picture(&out, output)?;
                    let p = match self.pending.iter().position(|p| p.time == t) {
                        Some(i) => {
                            // Inputs older than this picture produced none (dropped or concealed).
                            self.pending.drain(..i);
                            self.pending.pop_front()
                        }
                        None => self.pending.pop_front(),
                    };
                    let p = p.unwrap_or(Pending {
                        time: t,
                        display: DisplayId(header.display_id),
                        frame_id: header.frame_id,
                        captured_at: MonoMicros(header.capture_us),
                    });
                    return Ok(Some(DecodedFrame {
                        display: p.display,
                        frame_id: p.frame_id,
                        captured_at: p.captured_at,
                        width: w,
                        height: h,
                        pixels,
                    }));
                }
                Output::NeedMoreInput => return Ok(None),
                Output::StreamChange => self.negotiate_output()?,
            }
        }
    }

    fn picture(&mut self, sample: &IMFSample, output: DecoderOutput) -> Result<(Pixels, u32, u32)> {
        let (w, h) = self.size;
        if w == 0 || h == 0 {
            return Err(err(ErrorKind::Os, "VideoDecoder", "picture size unknown"));
        }
        // SAFETY: buffer access on a sample the MFT just returned.
        let buffer = unsafe { sample.GetBufferByIndex(0) }.ctx("GetBufferByIndex")?;
        if let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() {
            let mut raw = std::ptr::null_mut();
            // SAFETY: asks for the texture behind the DXGI buffer; `from_raw` takes the reference GetResource added.
            let (tex, slice) = unsafe {
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).ctx("IMFDXGIBuffer::GetResource")?;
                (ID3D11Texture2D::from_raw(raw), dxgi.GetSubresourceIndex().ctx("GetSubresourceIndex")?)
            };
            return self.picture_from_texture(&tex, slice, output, w, h);
        }
        // System-memory NV12 (software decoding).
        // SAFETY: the contiguous buffer is locked for the copy and unlocked after.
        let packed = unsafe {
            let contiguous = sample.ConvertToContiguousBuffer().ctx("ConvertToContiguousBuffer")?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            contiguous.Lock(&mut ptr, None, Some(&mut len)).ctx("IMFMediaBuffer::Lock")?;
            let v = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            contiguous.Unlock().ctx("IMFMediaBuffer::Unlock")?;
            v
        };
        // The MFT's stride for NV12 is the coded width; derive it from the buffer length (rows * 3 / 2).
        let coded_h = h.div_ceil(16) * 16;
        let rows_for = |rows: u32| (rows as usize) * 3 / 2;
        let stride = if packed.len() >= w as usize * rows_for(coded_h) {
            packed.len() / rows_for(coded_h)
        } else {
            packed.len() / rows_for(h)
        };
        let plane_rows = packed.len() / stride * 2 / 3;
        Ok((Pixels::Cpu { stride: w * 4, data: nv12_to_bgra(&packed, stride, plane_rows, w, h) }, w, h))
    }

    /// The next texture of the ring, which is (re)built when the picture size changes.
    fn ring_texture(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        if self.ring_size != (w, h) || self.ring.len() != GPU_RING {
            self.ring.clear();
            // The old textures are released by now: submit so the runtime frees them before the new ones are made.
            // SAFETY: Flush only submits the queued commands of our immediate context.
            unsafe { self.gpu.context.Flush() };
            for _ in 0..GPU_RING {
                self.ring.push(self.gpu.texture(
                    w,
                    h,
                    NV12,
                    USAGE_DEFAULT,
                    D3D11_BIND_FLAG(D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0),
                    D3D11_CPU_ACCESS_FLAG(0),
                )?);
            }
            self.ring_size = (w, h);
            self.ring_next = 0;
        }
        let tex = self.ring[self.ring_next].clone();
        self.ring_next = (self.ring_next + 1) % GPU_RING;
        Ok(tex)
    }

    fn picture_from_texture(
        &mut self,
        tex: &ID3D11Texture2D,
        slice: u32,
        output: DecoderOutput,
        w: u32,
        h: u32,
    ) -> Result<(Pixels, u32, u32)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc only fills the struct.
        unsafe { tex.GetDesc(&mut desc) };
        match output {
            DecoderOutput::Gpu => {
                // A single-slice NV12 copy in the next texture of the ring: the decoder's pool slot goes back to it at once.
                let own = self.ring_texture(w, h)?;
                let src_box = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: w,
                    bottom: h,
                    back: 1,
                };
                // SAFETY: copies the visible rectangle of one slice into our texture, on the same device.
                unsafe {
                    self.gpu.context.CopySubresourceRegion(&own, 0, 0, 0, 0, tex, slice, Some(&src_box));
                }
                self.copies += 1;
                if self.copies.is_multiple_of(FLUSH_EVERY) {
                    // SAFETY: Flush only submits the queued commands of our immediate context.
                    unsafe { self.gpu.context.Flush() };
                }
                Ok((Pixels::Gpu(GpuSurface(Box::new(own))), w, h))
            }
            DecoderOutput::CpuBgra => {
                let in_size = (desc.Width, desc.Height);
                let rebuild = match &self.proc {
                    Some((_, i, v)) => *i != in_size || *v != (w, h),
                    None => true,
                };
                if rebuild {
                    let proc =
                        VideoProc::new(&self.gpu, in_size, (w, h), (w, h), Space::Yuv709Limited, Space::Rgb, 60)?;
                    self.proc = Some((proc, in_size, (w, h)));
                    self.bgra = Some(self.gpu.texture(
                        w,
                        h,
                        BGRA,
                        USAGE_DEFAULT,
                        D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0),
                        D3D11_CPU_ACCESS_FLAG(0),
                    )?);
                    self.staging = Some(Staging::new(&self.gpu, w, h, BGRA)?);
                }
                let bgra = self.bgra.clone().expect("created with the processor");
                let (proc, _, _) = self.proc.as_mut().expect("created above");
                proc.blt(tex, slice, &bgra)?;
                let staging = self.staging.as_ref().expect("created with the processor");
                let data = staging.read(&self.gpu, &bgra, |bytes, pitch| {
                    let row = w as usize * 4;
                    let mut out = Vec::with_capacity(row * h as usize);
                    for y in 0..h as usize {
                        out.extend_from_slice(&bytes[y * pitch..y * pitch + row]);
                    }
                    out
                })?;
                Ok((Pixels::Cpu { stride: w * 4, data }, w, h))
            }
        }
    }
}

/// BT.709 limited-range NV12 to BGRA, for the software decoding path.
fn nv12_to_bgra(nv12: &[u8], stride: usize, plane_rows: usize, w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let uv_base = stride * plane_rows;
    let mut out = vec![255u8; w * h * 4];
    for y in 0..h {
        let uv_row = uv_base + (y / 2) * stride;
        for x in 0..w {
            let yy = i32::from(nv12[y * stride + x]) - 16;
            let u = i32::from(nv12[uv_row + (x & !1)]) - 128;
            let v = i32::from(nv12[uv_row + (x & !1) + 1]) - 128;
            let c = 1192 * yy; // 1.164 * 1024
            let r = (c + 1836 * v) >> 10; // 1.793
            let g = (c - 218 * u - 546 * v) >> 10; // 0.213, 0.533
            let b = (c + 2163 * u) >> 10; // 2.112
            let o = (y * w + x) * 4;
            out[o] = b.clamp(0, 255) as u8;
            out[o + 1] = g.clamp(0, 255) as u8;
            out[o + 2] = r.clamp(0, 255) as u8;
        }
    }
    out
}

impl VideoDecoder for MfH264Decoder {
    fn decode(&mut self, header: &VideoHeader, annex_b: &[u8]) -> Result<Option<DecodedFrame>> {
        if self.state.is_none() {
            self.start()?;
        }
        if !header.keyframe {
            // A hole in the frame ids: the picture would be wrong from here to the next keyframe.
            if let Some(last) = self.last_id
                && header.frame_id != last.wrapping_add(1)
            {
                self.reset();
            }
            if self.waiting_keyframe {
                return Ok(None);
            }
        }
        if header.discontinuity && !header.keyframe {
            self.reset();
            return Ok(None);
        }
        self.waiting_keyframe = false;
        self.last_id = Some(header.frame_id);
        self.last_header = Some(*header);
        let t0 = Instant::now();
        let output = self.options.output;
        let time = self.seq * 166_666;
        self.seq += 1;
        let state = self.state.as_mut().expect("started above");
        state.pending.push_back(Pending {
            time,
            display: DisplayId(header.display_id),
            frame_id: header.frame_id,
            captured_at: MonoMicros(header.capture_us),
        });
        let sample = memory_sample(annex_b)?;
        // SAFETY: sample attributes and timestamps on a sample we own.
        unsafe {
            sample.SetSampleTime(time).ctx("SetSampleTime")?;
            sample.SetSampleDuration(166_666).ctx("SetSampleDuration")?;
            if header.keyframe {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
            if header.discontinuity {
                let _ = sample.SetUINT32(&MFSampleExtension_Discontinuity, 1);
            }
        }
        let mut input_done = false;
        let mut result: Option<DecodedFrame> = None;
        let outcome = (|| -> Result<()> {
            for _ in 0..8 {
                if !input_done {
                    // SAFETY: feeds one access unit to the decoder.
                    match unsafe { state.transform.ProcessInput(0, &sample, 0) } {
                        Ok(()) => input_done = true,
                        Err(e) if e.code() == MF_E_NOTACCEPTING => {}
                        Err(e) => return Err(crate::sys::os_err("IMFTransform::ProcessInput", &e)),
                    }
                }
                // Take pictures until the MFT wants more input.
                while let Some(frame) = state.pull(output, header)? {
                    result = Some(frame);
                }
                if input_done {
                    break;
                }
            }
            // In low-latency mode the decoder owes one picture per access unit. Under GPU load the DXVA picture can be a moment late: report nothing
            // now (the decode thread never sleeps here) and let `poll` or the next input collect it.
            if result.is_some() && state.owed {
                state.late_pictures += 1;
            }
            state.owed = result.is_none();
            Ok(())
        })();
        if let Err(e) = outcome {
            self.waiting_keyframe = true;
            return Err(e);
        }
        self.decode_us = t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        Ok(result)
    }

    fn reset(&mut self) {
        self.waiting_keyframe = true;
        self.last_id = None;
        self.last_header = None;
        if let Some(state) = self.state.as_mut() {
            state.pending.clear();
            state.owed = false;
            // SAFETY: flush and restart of the stream on a live transform.
            unsafe {
                let _ = state.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = state.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            }
        }
    }
}
