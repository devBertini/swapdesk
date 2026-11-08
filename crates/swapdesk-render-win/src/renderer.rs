//! `Renderer` for Windows: decoded pictures into a composition swap chain that is the content of a visual in the
//! shell's DirectComposition tree (D18, `docs/research/ui.md` A1).
//!
//! Path of a picture: decoder NV12 texture (or an uploaded BGRA CPU frame) on the renderer's D3D11 device, the D3D11
//! video processor (BT.709 limited range to full-range BGRA, scaled into the picture rectangle of the [`Layout`], black
//! letterbox), the back buffer of a DXGI flip-model swap chain made with `CreateSwapChainForComposition`, `Present`.
//! The swap chain waits on its frame-latency object (maximum latency 1), so a picture is never queued behind another.
//! The video visual sits inside the shell's parent visual, under the toolbar; the remote cursor is a visual above it.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use swapdesk_platform::{
    AdapterId, CursorUpdate, DecodedFrame, DisplayId, ErrorKind, NativeSurface, Pixels, PresentStats, Rect, Renderer,
    Result, ScaleMode,
};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, ID3D11Device,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::DirectComposition::{IDCompositionDevice, IDCompositionVisual};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_PRESENT_DO_NOT_WAIT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain2,
};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::Interface;

use crate::cursor::CursorNode;
use crate::layout::{self, Layout, PxRect};
use crate::proc::{Proc, SourceKind};
use crate::sys::{Ctx, DXGI_ERROR_WAS_STILL_DRAWING, Gpu, Waitable, err, now, os_err};

const SWAP_FLAGS: i32 = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0;

/// How a [`WinRenderer`] is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RendererOptions {
    /// `None` (the default): the renderer runs on the shell's D3D11 device from the `NativeSurface`, so a decoder given
    /// [`WinRenderer::device`] shares it (zero copy). `Some(adapter)`: a device of its own on that adapter, which the
    /// decoder must then use instead (the swap chain still composes into the shell's tree, across adapters).
    pub adapter: Option<AdapterId>,
    /// How long `present` waits for the swap chain's frame-latency object before it drops the picture.
    pub wait: Duration,
    /// Buffers of the swap chain (2 to 4).
    pub buffers: u32,
}

impl Default for RendererOptions {
    fn default() -> Self {
        Self { adapter: None, wait: Duration::from_millis(50), buffers: 2 }
    }
}

/// Microseconds the last `present` spent in each step (measurements).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timing {
    /// Waiting for the frame-latency object.
    pub wait_us: u32,
    /// Recording the video processor blit (the GPU runs it on after this returns).
    pub blt_us: u32,
    /// The `Present` call.
    pub present_us: u32,
}

/// Pictures presented and pictures dropped (the latency object did not come in time) since `attach`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    pub presented: u64,
    pub dropped: u64,
}

/// BGRA pixels copied from the swap chain's back buffer just before the frame was presented ([`WinRenderer::set_probe`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

/// The texture a picture is drawn from.
#[derive(Clone)]
struct Source {
    tex: ID3D11Texture2D,
    kind: SourceKind,
    size: (u32, u32),
    display: DisplayId,
}

struct Shown {
    size: (u32, u32),
    display: DisplayId,
    layout: Layout,
}

struct Attached {
    dcomp: IDCompositionDevice,
    parent: IDCompositionVisual,
    video: IDCompositionVisual,
    /// The shell's device (the composition device's), where the cursor surface is drawn.
    shell: Gpu,
    /// The device the video processor and the swap chain use: the shell's, or one of the renderer's own.
    gpu: Gpu,
    swap: IDXGISwapChain2,
    waitable: Waitable,
    area: Rect,
    size: (u32, u32),
    proc: Option<Proc>,
    cursor: CursorNode,
    cursor_at: Option<(i32, i32, bool, DisplayId)>,
    shown: Option<Shown>,
    last: Option<Source>,
    upload: Option<((u32, u32), ID3D11Texture2D)>,
    scale: ScaleMode,
    probe_rect: Option<PxRect>,
    staging: Option<((u32, u32), ID3D11Texture2D)>,
    probe_pending: Option<(u32, u32)>,
    timing: Timing,
    starved: bool,
    counters: Counters,
    wait: Duration,
}

/// The Windows renderer. Created on the viewer thread and used only from it (C3: nothing here is `Send`).
pub struct WinRenderer {
    options: RendererOptions,
    att: Option<Attached>,
}

impl Default for WinRenderer {
    fn default() -> Self {
        Self::new(RendererOptions::default())
    }
}

fn closed() -> swapdesk_platform::PlatformError {
    err(ErrorKind::Closed, "Renderer", "not attached")
}

fn borrow<T: Interface>(raw: u64, what: &'static str) -> Result<T> {
    let ptr = raw as usize as *mut c_void;
    // SAFETY: the shell owns the object for the renderer's lifetime (C3 `NativeSurface`); a clone adds a reference.
    match unsafe { T::from_raw_borrowed(&ptr) } {
        Some(i) if !ptr.is_null() => Ok(i.clone()),
        _ => Err(err(ErrorKind::InvalidInput, "attach", format!("NativeSurface {what} is null"))),
    }
}

fn create_swap(gpu: &Gpu, size: (u32, u32), buffers: u32) -> Result<(IDXGISwapChain2, Waitable)> {
    let desc = |alpha| DXGI_SWAP_CHAIN_DESC1 {
        Width: size.0,
        Height: size.1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: buffers.clamp(2, 4),
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: alpha,
        Flags: SWAP_FLAGS as u32,
    };
    // SAFETY: plain factory and swap chain creation on live objects; the waitable handle is owned by `Waitable`.
    unsafe {
        let dxgi: IDXGIDevice = gpu.device.cast().ctx("ID3D11Device as IDXGIDevice")?;
        let factory: IDXGIFactory2 =
            dxgi.GetAdapter().ctx("IDXGIDevice::GetAdapter")?.GetParent().ctx("IDXGIAdapter::GetParent")?;
        let swap = match factory.CreateSwapChainForComposition(&gpu.device, &desc(DXGI_ALPHA_MODE_IGNORE), None) {
            Ok(s) => s,
            Err(_) => factory
                .CreateSwapChainForComposition(&gpu.device, &desc(DXGI_ALPHA_MODE_PREMULTIPLIED), None)
                .ctx("CreateSwapChainForComposition")?,
        };
        let swap: IDXGISwapChain2 = swap.cast().ctx("IDXGISwapChain1 as IDXGISwapChain2")?;
        swap.SetMaximumFrameLatency(1).ctx("SetMaximumFrameLatency")?;
        let waitable = Waitable(swap.GetFrameLatencyWaitableObject());
        Ok((swap, waitable))
    }
}

impl WinRenderer {
    pub fn new(options: RendererOptions) -> Self {
        Self { options, att: None }
    }

    /// The D3D11 device the decoder must use for zero-copy output (`DecoderOptions::device`): the shell's device, or the
    /// renderer's own when [`RendererOptions::adapter`] is set. `None` before `attach`.
    pub fn device(&self) -> Option<ID3D11Device> {
        self.att.as_ref().map(|a| a.gpu.device.clone())
    }

    /// Draws the last picture again under `scale` (the user changed Fit, 1:1 or the scroll position; a static host sends
    /// no new picture). `present` also takes the scale with every picture.
    pub fn set_scale(&mut self, scale: ScaleMode) -> Result<()> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        a.scale = scale;
        a.redraw()
    }

    /// The layout of the last picture, in area-relative pixels (the shell draws scroll bars from it).
    pub fn layout(&self) -> Option<Layout> {
        self.att.as_ref().and_then(|a| a.shown.as_ref().map(|s| s.layout))
    }

    pub fn timing(&self) -> Timing {
        self.att.as_ref().map(|a| a.timing).unwrap_or_default()
    }

    pub fn counters(&self) -> Counters {
        self.att.as_ref().map(|a| a.counters).unwrap_or_default()
    }

    /// Measurements: copy this swap chain rectangle (swap chain pixels) of every picture just before it is presented,
    /// for [`take_probe`](Self::take_probe). `None` stops copying.
    pub fn set_probe(&mut self, rect: Option<(u32, u32, u32, u32)>) {
        if let Some(a) = self.att.as_mut() {
            a.probe_rect = rect.map(|(x, y, w, h)| PxRect { x, y, w, h });
            a.probe_pending = None;
        }
    }

    /// The pixels copied for the last picture, once the GPU has finished it (blocks until then). Call before the next
    /// `present`.
    pub fn take_probe(&mut self) -> Result<Option<Probe>> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        let (Some((w, h)), Some((_, staging))) = (a.probe_pending.take(), a.staging.as_ref()) else { return Ok(None) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture holds `h` rows of `RowPitch` bytes; the slice is copied before Unmap.
        unsafe {
            a.gpu.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ctx("ID3D11DeviceContext::Map")?;
            let pitch = mapped.RowPitch as usize;
            let row = w as usize * 4;
            let src = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), pitch * h as usize);
            let mut data = Vec::with_capacity(row * h as usize);
            for y in 0..h as usize {
                data.extend_from_slice(&src[y * pitch..y * pitch + row]);
            }
            a.gpu.context.Unmap(staging, 0);
            Ok(Some(Probe { width: w, height: h, stride: w * 4, data }))
        }
    }
}

impl Attached {
    fn source_of(&mut self, frame: &DecodedFrame) -> Result<Source> {
        let size = (frame.width, frame.height);
        match &frame.pixels {
            Pixels::Gpu(surface) => {
                let tex = surface
                    .0
                    .downcast_ref::<ID3D11Texture2D>()
                    .ok_or_else(|| err(ErrorKind::Unsupported, "present", "the GPU surface is not a D3D11 texture"))?
                    .clone();
                // SAFETY: GetDevice and GetDesc only return an interface and fill a struct.
                let (same, format) = unsafe {
                    let dev = tex.GetDevice().ok();
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    tex.GetDesc(&mut d);
                    (dev.is_some_and(|d| d.as_raw() == self.gpu.device.as_raw()), d.Format)
                };
                if !same {
                    return Err(err(
                        ErrorKind::Unsupported,
                        "present",
                        "the texture lives on another D3D11 device: create the decoder with WinRenderer::device()",
                    ));
                }
                let kind = if format == DXGI_FORMAT_NV12 {
                    SourceKind::Nv12
                } else if format == DXGI_FORMAT_B8G8R8A8_UNORM {
                    SourceKind::Bgra
                } else {
                    return Err(err(
                        ErrorKind::Unsupported,
                        "present",
                        format!("texture format {} is neither NV12 nor BGRA", format.0),
                    ));
                };
                Ok(Source { tex, kind, size, display: frame.display })
            }
            Pixels::Cpu { stride, data } => {
                let (w, h) = size;
                if w == 0 || h == 0 || (*stride as usize) < w as usize * 4 || data.len() < *stride as usize * h as usize
                {
                    return Err(err(
                        ErrorKind::InvalidInput,
                        "present",
                        "CPU frame smaller than its width, height and stride",
                    ));
                }
                if self.upload.as_ref().is_none_or(|(s, _)| *s != size) {
                    let desc = D3D11_TEXTURE2D_DESC {
                        Width: w,
                        Height: h,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                    };
                    let mut tex = None;
                    // SAFETY: valid descriptor and out pointer.
                    unsafe { self.gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                        .ctx("CreateTexture2D (upload)")?;
                    self.upload =
                        Some((size, tex.ok_or_else(|| err(ErrorKind::Os, "CreateTexture2D", "no texture returned"))?));
                }
                let tex = self.upload.as_ref().map(|(_, t)| t.clone()).expect("created above");
                // SAFETY: `data` holds `stride * h` bytes, checked above. The flush matters: the video processor (a separate
                // engine on some GPUs) did not see a pending UpdateSubresource of the first upload and drew black.
                unsafe {
                    self.gpu.context.UpdateSubresource(&tex, 0, None, data.as_ptr().cast(), *stride, 0);
                    self.gpu.context.Flush();
                }
                Ok(Source { tex, kind: SourceKind::Bgra, size, display: frame.display })
            }
        }
    }

    /// Records the blit, copies the probe, presents. Remembers the picture for `redraw`.
    fn draw(&mut self, source: Source, wait: bool) -> Result<PresentStats> {
        let layout = Layout::compute(source.size, self.size, self.scale)
            .ok_or_else(|| err(ErrorKind::InvalidInput, "present", "empty picture or area"))?;
        self.last = Some(source.clone());
        let t0 = Instant::now();
        // Wait until the previous picture left the queue; after a timeout (window hidden) do not wait again until it signals.
        if wait {
            let ms = if self.starved { 0 } else { self.wait.as_millis().min(1000) as u32 };
            // SAFETY: waits on the swap chain's own handle.
            let signalled = unsafe { WaitForSingleObjectEx(self.waitable.0, ms, true) } == WAIT_OBJECT_0;
            self.starved = !signalled;
            if !signalled {
                self.counters.dropped += 1;
                self.timing = Timing { wait_us: t0.elapsed().as_micros() as u32, ..Default::default() };
                return Err(err(
                    ErrorKind::Timeout,
                    "present",
                    "frame dropped: the swap chain is still presenting the previous one",
                ));
            }
        }
        let t1 = Instant::now();
        if self.proc.as_ref().is_none_or(|p| p.input != source.size || p.output != self.size) {
            self.proc = Some(Proc::new(&self.gpu, source.size, self.size)?);
        }
        // SAFETY: the back buffer is the swap chain's own; the probe copy reads inside it (clamped).
        let back: ID3D11Texture2D = unsafe { self.swap.GetBuffer(0) }.ctx("IDXGISwapChain::GetBuffer")?;
        self.proc.as_mut().expect("created above").blt(&source.tex, source.kind, &layout, &back)?;
        if let Some(r) = self.probe_rect {
            self.copy_probe(&back, r)?;
        }
        let t2 = Instant::now();
        let flags = if self.starved { DXGI_PRESENT_DO_NOT_WAIT } else { DXGI_PRESENT(0) };
        // SAFETY: presents the swap chain of this renderer.
        let hr = unsafe { self.swap.Present(0, flags) };
        let presented_at = now();
        let t3 = Instant::now();
        if hr.0 == DXGI_ERROR_WAS_STILL_DRAWING {
            self.counters.dropped += 1;
            return Err(err(ErrorKind::Timeout, "present", "frame dropped: the swap chain is still drawing"));
        }
        hr.ok().map_err(|e| os_err("IDXGISwapChain::Present", &e))?;
        self.counters.presented += 1;
        self.timing = Timing {
            wait_us: t1.duration_since(t0).as_micros() as u32,
            blt_us: t2.duration_since(t1).as_micros() as u32,
            present_us: t3.duration_since(t2).as_micros() as u32,
        };
        let moved = self
            .shown
            .as_ref()
            .is_none_or(|s| s.layout != layout || s.size != source.size || s.display != source.display);
        self.shown = Some(Shown { size: source.size, display: source.display, layout });
        if moved {
            self.place_cursor()?;
        }
        Ok(PresentStats { presented_at })
    }

    fn redraw(&mut self) -> Result<()> {
        match self.last.clone() {
            Some(source) => self.draw(source, false).map(|_| ()),
            None => Ok(()),
        }
    }

    fn copy_probe(&mut self, back: &ID3D11Texture2D, r: PxRect) -> Result<()> {
        let (w, h) = (r.w.min(self.size.0.saturating_sub(r.x)), r.h.min(self.size.1.saturating_sub(r.y)));
        if w == 0 || h == 0 {
            return Ok(());
        }
        if self.staging.as_ref().is_none_or(|(s, _)| *s != (w, h)) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex = None;
            // SAFETY: valid descriptor and out pointer.
            unsafe { self.gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.ctx("CreateTexture2D (probe)")?;
            self.staging =
                Some(((w, h), tex.ok_or_else(|| err(ErrorKind::Os, "CreateTexture2D", "no texture returned"))?));
        }
        let (_, staging) = self.staging.as_ref().expect("created above");
        let src = D3D11_BOX { left: r.x, top: r.y, front: 0, right: r.x + w, bottom: r.y + h, back: 1 };
        // SAFETY: copies a rectangle inside the back buffer into the staging texture of the same format.
        unsafe { self.gpu.context.CopySubresourceRegion(staging, 0, 0, 0, 0, back, 0, Some(&src)) };
        self.probe_pending = Some((w, h));
        Ok(())
    }

    /// Puts the cursor visual where the last cursor update says, or hides it, and commits.
    fn place_cursor(&mut self) -> Result<()> {
        let spot = match (&self.shown, self.cursor_at) {
            (Some(s), Some((x, y, true, display))) if display == s.display => {
                s.layout.dest_point(x, y).map(|(dx, dy)| (self.area.x as f32 + dx, self.area.y as f32 + dy))
            }
            _ => None,
        };
        self.cursor.place(spot)?;
        // SAFETY: commits the shell's composition device.
        unsafe { self.dcomp.Commit() }.ctx("IDCompositionDevice::Commit")
    }

    fn set_area(&mut self, area: Rect) -> Result<()> {
        let size = (area.width.max(1), area.height.max(1));
        // SAFETY: property writes on a live visual; ResizeBuffers runs after every reference to the buffers is dropped
        // (output views are forgotten, the probe copy goes through a separate staging texture).
        unsafe {
            self.video.SetOffsetX2(area.x as f32).ctx("SetOffsetX2")?;
            self.video.SetOffsetY2(area.y as f32).ctx("SetOffsetY2")?;
            if size != self.size {
                if let Some(p) = self.proc.as_mut() {
                    p.forget_outputs();
                }
                self.swap
                    .ResizeBuffers(0, size.0, size.1, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(SWAP_FLAGS))
                    .ctx("IDXGISwapChain::ResizeBuffers")?;
                self.size = size;
            }
        }
        self.area = area;
        // A resized chain is black until drawn again: the last picture (a static host sends none) goes back in.
        if self.last.is_some() {
            self.redraw()?;
        }
        self.place_cursor()
    }
}

impl Renderer for WinRenderer {
    fn attach(&mut self, surface: NativeSurface, area: Rect) -> Result<()> {
        if self.att.is_some() {
            self.detach();
        }
        let NativeSurface::Windows { dcomp_device, parent_visual, d3d11_device } = surface else {
            return Err(err(ErrorKind::Unsupported, "attach", "not a Windows NativeSurface"));
        };
        let dcomp: IDCompositionDevice = borrow(dcomp_device, "dcomp_device")?;
        let parent: IDCompositionVisual = borrow(parent_visual, "parent_visual")?;
        let shell = Gpu::from_device(borrow::<ID3D11Device>(d3d11_device, "d3d11_device")?)?;
        let gpu = match self.options.adapter {
            Some(id) => Gpu::on_adapter(id)?,
            None => shell.clone(),
        };
        let size = (area.width.max(1), area.height.max(1));
        let (swap, waitable) = create_swap(&gpu, size, self.options.buffers)?;
        // SAFETY: plain creation and insertion on live composition objects.
        let video = unsafe {
            let v = dcomp.CreateVisual().ctx("CreateVisual (video)")?;
            v.SetContent(&swap).ctx("SetContent (video)")?;
            parent.AddVisual(&v, true, None::<&IDCompositionVisual>).ctx("AddVisual (video)")?;
            v
        };
        let cursor = CursorNode::new(&dcomp, &parent)?;
        let mut att = Attached {
            dcomp,
            parent,
            video,
            shell,
            gpu,
            swap,
            waitable,
            area,
            size,
            proc: None,
            cursor,
            cursor_at: None,
            shown: None,
            last: None,
            upload: None,
            scale: ScaleMode::Fit,
            probe_rect: None,
            staging: None,
            probe_pending: None,
            timing: Timing::default(),
            starved: false,
            counters: Counters::default(),
            wait: self.options.wait,
        };
        att.set_area(area)?;
        self.att = Some(att);
        Ok(())
    }

    fn resize(&mut self, area: Rect) -> Result<()> {
        self.att.as_mut().ok_or_else(closed)?.set_area(area)
    }

    fn present(&mut self, frame: DecodedFrame, scale: ScaleMode) -> Result<PresentStats> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        a.scale = scale;
        let source = a.source_of(&frame)?;
        a.draw(source, true)
    }

    fn set_cursor(&mut self, cursor: &CursorUpdate) -> Result<()> {
        let a = self.att.as_mut().ok_or_else(closed)?;
        if let Some(shape) = &cursor.shape {
            a.cursor.set_shape(&a.dcomp, &a.shell, shape)?;
        }
        a.cursor_at = Some((cursor.x, cursor.y, cursor.visible && a.cursor.has_shape(), cursor.display));
        a.place_cursor()
    }

    fn map_pointer(&self, x: i32, y: i32) -> Option<(u16, u16)> {
        let a = self.att.as_ref()?;
        let s = a.shown.as_ref()?;
        layout::map_pointer(a.area, s.size, &s.layout, x, y)
    }

    fn detach(&mut self) {
        if let Some(mut a) = self.att.take() {
            a.cursor.remove(&a.parent);
            // SAFETY: removes our visual from the shell's parent and commits; failures leave nothing the shell owns.
            unsafe {
                let _ = a.parent.RemoveVisual(&a.video);
                let _ = a.video.SetContent(None::<&windows::core::IUnknown>);
                let _ = a.dcomp.Commit();
            }
        }
    }
}

impl Drop for WinRenderer {
    fn drop(&mut self) {
        self.detach();
    }
}
