//! D3D11 side: the device of an adapter, textures, the video processor (BGRA to NV12 for the encoder, NV12 to BGRA for
//! the decoder's CPU readback path) and staging readback.

use std::mem::ManuallyDrop;

use swapdesk_platform::{AdapterId, ErrorKind, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_CPU_ACCESS_FLAG, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_RESOURCE_MISC_FLAG, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::sys::{AdapterDesc, Ctx, create_device, device_adapter, err, find_adapter, protect_multithread};

/// A device, its immediate context and the adapter it runs on.
#[derive(Clone)]
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: AdapterDesc,
}

impl Gpu {
    pub(crate) fn on_adapter(id: AdapterId) -> Result<Self> {
        let (adapter, desc) = find_adapter(id)?;
        let (device, context) = create_device(&adapter)?;
        Ok(Self { device, context, adapter: desc })
    }

    pub(crate) fn from_device(device: ID3D11Device) -> Result<Self> {
        crate::sys::init_thread()?;
        protect_multithread(&device);
        let adapter = device_adapter(&device)?;
        // SAFETY: GetImmediateContext only returns an interface.
        let context = unsafe { device.GetImmediateContext() }.ctx("GetImmediateContext")?;
        Ok(Self { device, context, adapter })
    }

    pub(crate) fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        usage: D3D11_USAGE,
        bind: D3D11_BIND_FLAG,
        cpu: D3D11_CPU_ACCESS_FLAG,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: usage,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: cpu.0 as u32,
            MiscFlags: D3D11_RESOURCE_MISC_FLAG(0).0 as u32,
        };
        let mut tex = None;
        // SAFETY: `desc` and the out pointer are valid for the call.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.ctx("CreateTexture2D")?;
        tex.ok_or_else(|| err(ErrorKind::Os, "CreateTexture2D", "no texture returned"))
    }

    /// Uploads BGRA rows into a default-usage texture.
    pub(crate) fn upload_bgra(&self, tex: &ID3D11Texture2D, data: &[u8], stride: u32) {
        // SAFETY: `data` holds the rows the caller validated against the texture height and `stride`.
        unsafe {
            self.context.UpdateSubresource(tex, 0, None, data.as_ptr().cast(), stride, 0);
        }
    }
}

/// A staging texture and the readback of its planes.
pub(crate) struct Staging {
    tex: ID3D11Texture2D,
    height: u32,
}

impl Staging {
    pub(crate) fn new(gpu: &Gpu, width: u32, height: u32, format: DXGI_FORMAT) -> Result<Self> {
        let tex = gpu.texture(width, height, format, D3D11_USAGE_STAGING, D3D11_BIND_FLAG(0), D3D11_CPU_ACCESS_READ)?;
        Ok(Self { tex, height })
    }

    /// Copies `src` (a whole texture of the same size and format) and hands the mapped bytes and the row pitch to `f`.
    pub(crate) fn read<R>(&self, gpu: &Gpu, src: &ID3D11Texture2D, f: impl FnOnce(&[u8], usize) -> R) -> Result<R> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: CopyResource between same-size same-format textures; the mapped memory is valid until Unmap and
        // covers RowPitch * rows of the format (plus the chroma plane for NV12).
        unsafe {
            gpu.context.CopyResource(&self.tex, src);
            gpu.context.Map(&self.tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ctx("ID3D11DeviceContext::Map")?;
            let pitch = mapped.RowPitch as usize;
            let rows = if self.is_nv12() { self.height as usize * 3 / 2 } else { self.height as usize };
            let bytes = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * rows);
            let out = f(bytes, pitch);
            gpu.context.Unmap(&self.tex, 0);
            Ok(out)
        }
    }

    fn is_nv12(&self) -> bool {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc only fills the struct.
        unsafe { self.tex.GetDesc(&mut desc) };
        desc.Format == DXGI_FORMAT_NV12
    }
}

/// The colour interpretation of one side of a video-processor conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Space {
    /// BGRA with full-range RGB.
    Rgb,
    /// NV12 with BT.709 and limited (16-235) range: what the encoder is told and the decoder assumes.
    Yuv709Limited,
}

fn color_space(space: Space) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    // Bits: Usage(0) RGB_Range(1) YCbCr_Matrix(2) YCbCr_xvYCC(3) Nominal_Range(4..5).
    let bits = match space {
        Space::Rgb => 0,
        Space::Yuv709Limited => 1 << 2 | 1 << 4,
    };
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: bits }
}

/// A D3D11 video processor between two fixed sizes and colour spaces.
pub(crate) struct VideoProc {
    _vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    vdev: ID3D11VideoDevice,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    in_views: Vec<((usize, u32), ID3D11VideoProcessorInputView)>,
    out_views: Vec<(usize, ID3D11VideoProcessorOutputView)>,
}

impl VideoProc {
    pub(crate) fn new(
        gpu: &Gpu,
        in_size: (u32, u32),
        visible: (u32, u32),
        out_size: (u32, u32),
        input: Space,
        output: Space,
        fps: u32,
    ) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast().ctx("ID3D11Device as ID3D11VideoDevice")?;
        let vctx: ID3D11VideoContext = gpu.context.cast().ctx("ID3D11DeviceContext as ID3D11VideoContext")?;
        let rate = DXGI_RATIONAL { Numerator: fps.max(1), Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_size.0,
            InputHeight: in_size.1,
            OutputFrameRate: rate,
            OutputWidth: out_size.0,
            OutputHeight: out_size.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: descriptor pointers are valid for each call; the processor interfaces are kept alive by `self`.
        unsafe {
            let enumerator = vdev.CreateVideoProcessorEnumerator(&content).ctx("CreateVideoProcessorEnumerator")?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0).ctx("CreateVideoProcessor")?;
            // Scaling (F-VIEW-06): the source rectangle is the part of the input surface that is blitted; the stream
            // destination rectangle is the part of the output surface that receives it; the output target rectangle
            // is the area of the output surface drawn into. With `visible` the whole input and `out_size` the whole
            // output, one blit scales `visible` to `out_size` while it converts the colour. Primary pages, read
            // 2026-11-12: learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11videocontext-
            // videoprocessorsetstreamsourcerect, ...-videoprocessorsetstreamdestrect (default is an empty
            // rectangle: nothing is written unless it is set and enabled) and ...-videoprocessorsetoutputtargetrect.
            let src = RECT { left: 0, top: 0, right: visible.0 as i32, bottom: visible.1 as i32 };
            let dst = RECT { left: 0, top: 0, right: out_size.0 as i32, bottom: out_size.1 as i32 };
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &color_space(input));
            vctx.VideoProcessorSetOutputColorSpace(&processor, &color_space(output));
            vctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src));
            vctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dst));
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            Ok(Self {
                _vdev: vdev.clone(),
                vctx,
                vdev,
                enumerator,
                processor,
                in_views: Vec::new(),
                out_views: Vec::new(),
            })
        }
    }

    fn input_view(&mut self, tex: &ID3D11Texture2D, slice: u32) -> Result<ID3D11VideoProcessorInputView> {
        let key = (tex.as_raw() as usize, slice);
        if let Some((_, v)) = self.in_views.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
            },
        };
        let resource: ID3D11Resource = tex.cast().ctx("texture as resource")?;
        let mut view = None;
        // SAFETY: valid descriptor and out pointer.
        unsafe { self.vdev.CreateVideoProcessorInputView(&resource, &self.enumerator, &desc, Some(&mut view)) }
            .ctx("CreateVideoProcessorInputView")?;
        let view = view.ok_or_else(|| err(ErrorKind::Os, "CreateVideoProcessorInputView", "no view returned"))?;
        if self.in_views.len() >= 64 {
            self.in_views.remove(0);
        }
        self.in_views.push((key, view.clone()));
        Ok(view)
    }

    fn output_view(&mut self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorOutputView> {
        let key = tex.as_raw() as usize;
        if let Some((_, v)) = self.out_views.iter().find(|(k, _)| *k == key) {
            return Ok(v.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let resource: ID3D11Resource = tex.cast().ctx("texture as resource")?;
        let mut view = None;
        // SAFETY: valid descriptor and out pointer.
        unsafe { self.vdev.CreateVideoProcessorOutputView(&resource, &self.enumerator, &desc, Some(&mut view)) }
            .ctx("CreateVideoProcessorOutputView")?;
        let view = view.ok_or_else(|| err(ErrorKind::Os, "CreateVideoProcessorOutputView", "no view returned"))?;
        if self.out_views.len() >= 32 {
            self.out_views.remove(0);
        }
        self.out_views.push((key, view.clone()));
        Ok(view)
    }

    /// Converts slice `slice` of `input` into `output` on the GPU.
    pub(crate) fn blt(&mut self, input: &ID3D11Texture2D, slice: u32, output: &ID3D11Texture2D) -> Result<()> {
        let in_view = self.input_view(input, slice)?;
        let out_view = self.output_view(output)?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(in_view)),
            ..Default::default()
        };
        // SAFETY: the stream lives across the call.
        let result =
            unsafe { self.vctx.VideoProcessorBlt(&self.processor, &out_view, 0, std::slice::from_ref(&stream)) };
        // SAFETY: release the input view reference held by the stream exactly once.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.ctx("VideoProcessorBlt")
    }
}

pub(crate) const BGRA: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
pub(crate) const NV12: DXGI_FORMAT = DXGI_FORMAT_NV12;
pub(crate) const USAGE_DEFAULT: D3D11_USAGE = D3D11_USAGE_DEFAULT;

/// Plays the part of the capture crate in tests and measurements: owns a D3D11 device on an adapter and turns CPU BGRA
/// frames into GPU frames (an `ID3D11Texture2D` in `Pixels::Gpu`, the form DDA delivers). Give the encoder the same
/// device through `EncoderOptions::device`.
pub struct TextureUploader {
    gpu: Gpu,
}

impl TextureUploader {
    pub fn new(adapter: AdapterId) -> Result<Self> {
        Ok(Self { gpu: Gpu::on_adapter(adapter)? })
    }

    /// The device the textures live on.
    pub fn device(&self) -> ID3D11Device {
        self.gpu.device.clone()
    }

    /// A GPU copy of a CPU frame.
    pub fn upload(&self, frame: &swapdesk_platform::Frame) -> Result<swapdesk_platform::Frame> {
        use swapdesk_platform::{Frame, GpuSurface, Pixels};
        let Pixels::Cpu { stride, data } = &frame.pixels else {
            return Err(err(ErrorKind::InvalidInput, "TextureUploader::upload", "not a CPU frame"));
        };
        let tex = self.gpu.texture(
            frame.width,
            frame.height,
            BGRA,
            USAGE_DEFAULT,
            D3D11_BIND_FLAG(
                windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE.0
                    | windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET.0,
            ),
            D3D11_CPU_ACCESS_FLAG(0),
        )?;
        self.gpu.upload_bgra(&tex, data, *stride);
        Ok(Frame {
            display: frame.display,
            captured_at: frame.captured_at,
            width: frame.width,
            height: frame.height,
            pixels: Pixels::Gpu(GpuSurface(Box::new(tex))),
            dirty: frame.dirty.clone(),
            content_changed: frame.content_changed,
        })
    }
}
