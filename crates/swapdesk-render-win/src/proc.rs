//! The D3D11 video processor that turns a decoded picture into the swap chain's back buffer: NV12 BT.709 limited range
//! (or BGRA) in, full-range BGRA out, with the scaling of a [`Layout`] and a black letterbox around it.

use std::mem::ManuallyDrop;

use swapdesk_platform::{ErrorKind, Result};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
    ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_RATIONAL;
use windows::core::Interface;

use crate::layout::Layout;
use crate::sys::{Ctx, Gpu, err};

/// What the source texture holds, for the colour interpretation of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// Decoder output: BT.709, limited (16 to 235) range: what the encoder is told and the decoder assumes.
    Nv12,
    /// CPU frames uploaded by the renderer: full-range RGB.
    Bgra,
}

fn color_space(kind: SourceKind) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    // Bits: Usage(0) RGB_Range(1) YCbCr_Matrix(2) YCbCr_xvYCC(3) Nominal_Range(4..5).
    let bits = match kind {
        SourceKind::Nv12 => 1 << 2 | 1 << 4, // matrix BT.709, nominal range 16 to 235
        SourceKind::Bgra => 0,               // RGB full range
    };
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: bits }
}

/// A processor for one pair of sizes (input texture, swap chain).
pub(crate) struct Proc {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    ctx: ID3D11DeviceContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    pub input: (u32, u32),
    pub output: (u32, u32),
    kind: Option<SourceKind>,
    out_views: Vec<(usize, ID3D11VideoProcessorOutputView)>,
    /// Input views by (texture, array slice). A view holds its texture, so the address cannot be reused while cached.
    in_views: Vec<((usize, u32), ID3D11VideoProcessorInputView)>,
    blits: u64,
}

/// Input views kept (the decoder reuses a ring of six textures; the renderer's own upload texture is one more).
const IN_VIEWS: usize = 16;

/// Blits between two `Flush` calls on the immediate context.
const FLUSH_EVERY: u64 = 60;

impl Proc {
    pub(crate) fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32)) -> Result<Self> {
        let vdev: ID3D11VideoDevice = gpu.device.cast().ctx("ID3D11Device as ID3D11VideoDevice")?;
        let vctx: ID3D11VideoContext = gpu.context.cast().ctx("ID3D11DeviceContext as ID3D11VideoContext")?;
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: input.0,
            InputHeight: input.1,
            OutputFrameRate: rate,
            OutputWidth: output.0,
            OutputHeight: output.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        // SAFETY: descriptor pointers are valid for each call; the interfaces are kept alive by `self`.
        unsafe {
            let enumerator = vdev.CreateVideoProcessorEnumerator(&content).ctx("CreateVideoProcessorEnumerator")?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0).ctx("CreateVideoProcessor")?;
            // Full-range RGB out, black outside the pictures.
            vctx.VideoProcessorSetOutputColorSpace(&processor, &color_space(SourceKind::Bgra));
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
            };
            vctx.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            let target = RECT { left: 0, top: 0, right: output.0 as i32, bottom: output.1 as i32 };
            vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
            Ok(Self {
                vdev,
                vctx,
                ctx: gpu.context.clone(),
                enumerator,
                processor,
                input,
                output,
                kind: None,
                out_views: Vec::new(),
                in_views: Vec::new(),
                blits: 0,
            })
        }
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
        // A flip-model swap chain has a handful of buffers; more entries mean the chain was rebuilt.
        if self.out_views.len() >= 8 {
            self.out_views.remove(0);
        }
        self.out_views.push((key, view.clone()));
        Ok(view)
    }

    /// The input view of (`tex`, `slice`), made once and kept: a view per blit leaked memory in the viewer's long runs.
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
        if self.in_views.len() >= IN_VIEWS {
            self.in_views.remove(0);
            // The evicted view's texture is released: submit so the runtime frees it instead of queueing it.
            // SAFETY: Flush only submits the queued commands of the immediate context.
            unsafe { self.ctx.Flush() };
        }
        self.in_views.push((key, view.clone()));
        Ok(view)
    }

    /// Draws the `layout` of `source` into `target` (the back buffer): scaled picture, black around it.
    pub(crate) fn blt(
        &mut self,
        source: &ID3D11Texture2D,
        kind: SourceKind,
        layout: &Layout,
        target: &ID3D11Texture2D,
    ) -> Result<()> {
        let out_view = self.output_view(target)?;
        let in_view = self.input_view(source, 0)?;
        let rect = |x: u32, y: u32, w: u32, h: u32| RECT {
            left: x as i32,
            top: y as i32,
            right: (x + w) as i32,
            bottom: (y + h) as i32,
        };
        let (s, d) = (layout.src, layout.dest);
        let (src, dst) = (rect(s.x, s.y, s.w, s.h), rect(d.x, d.y, d.w, d.h));
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(in_view)),
            ..Default::default()
        };
        // SAFETY: the stream and the rectangles live across the calls.
        let result = unsafe {
            if self.kind != Some(kind) {
                self.vctx.VideoProcessorSetStreamColorSpace(&self.processor, 0, &color_space(kind));
                self.kind = Some(kind);
            }
            self.vctx.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
            self.vctx.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
            self.vctx.VideoProcessorBlt(&self.processor, &out_view, 0, std::slice::from_ref(&stream))
        };
        // SAFETY: release the input view reference held by the stream exactly once.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        self.blits += 1;
        if self.blits.is_multiple_of(FLUSH_EVERY) {
            // SAFETY: Flush only submits the queued commands of the immediate context.
            unsafe { self.ctx.Flush() };
        }
        result.ctx("VideoProcessorBlt")
    }

    /// The swap chain's buffers were released (resize): forget their views.
    pub(crate) fn forget_outputs(&mut self) {
        self.out_views.clear();
    }
}
