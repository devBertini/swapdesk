//! The graphics device and a window's composition tree (D18): one D3D11 device shared by every window, a
//! DirectComposition target per window with a root visual holding two children, the video parent visual (below,
//! empty until stage 15 attaches the renderer's swap chain) and the UI visual (above, a composition swap chain the UI
//! draws into with Direct2D).

use std::mem::ManuallyDrop;

use swapdesk_platform::NativeSurface;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_CAP_STYLE_ROUND,
    D2D1_DASH_STYLE_DASH, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_LINE_JOIN_ROUND,
    D2D1_STROKE_STYLE_PROPERTIES, D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
    D2D1CreateFactory, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_GPU_PREFERENCE_MINIMUM_POWER, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIAdapter1, IDXGIDevice, IDXGIFactory2, IDXGIFactory6, IDXGISurface, IDXGISwapChain1,
};
use windows::core::{Interface, Result};

use crate::gfx::{Fonts, Strokes};

/// The adapter the UI draws on: minimum power by default, high performance on request; `None` lets D3D choose.
fn preferred_adapter(high: bool) -> Option<IDXGIAdapter1> {
    // SAFETY: plain factory and enumeration calls on objects created here.
    unsafe {
        let factory: IDXGIFactory6 =
            CreateDXGIFactory2(windows::Win32::Graphics::Dxgi::DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let pref = if high { DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE } else { DXGI_GPU_PREFERENCE_MINIMUM_POWER };
        factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(0, pref).ok()
    }
}

/// Everything shared by the windows of the process.
pub struct Gfx {
    pub d3d: ID3D11Device,
    factory: IDXGIFactory2,
    pub d2d_factory: ID2D1Factory1,
    d2d_device: ID2D1Device,
    pub dcomp: IDCompositionDevice,
    pub fonts: Fonts,
    pub strokes: Strokes,
}

impl Gfx {
    pub fn new() -> Result<Gfx> {
        let mut device = None;
        // The UI is light 2D work: it runs on the low-power adapter (the integrated GPU on a hybrid laptop), so the
        // discrete GPU is not woken for a window that mostly sits still. `SWAPDESK_ADAPTER=high` picks the other.
        let high = std::env::var("SWAPDESK_ADAPTER").as_deref() == Ok("high");
        let adapter = preferred_adapter(high);
        // SAFETY: plain device creation with out pointers to locals.
        unsafe {
            match &adapter {
                Some(a) => D3D11CreateDevice(
                    a,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )?,
                None => D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )?,
            }
        }
        let d3d = device.ok_or_else(windows::core::Error::empty)?;
        // The viewer's decoder and renderer share this device from their own thread (CONTRACTS.md, readings of
        // stage 15): the immediate context is made multithread-protected before Direct2D and DirectComposition
        // take it, so their calls go through the same lock.
        if let Ok(context) = unsafe { d3d.GetImmediateContext() }.and_then(|c| c.cast::<ID3D11Multithread>()) {
            // SAFETY: a plain flag on a live context; the previous value is not needed.
            let _ = unsafe { context.SetMultithreadProtected(true) };
        }
        let dxgi: IDXGIDevice = d3d.cast()?;
        // SAFETY: plain factory and device creation on live objects.
        unsafe {
            let factory: IDXGIFactory2 =
                CreateDXGIFactory2(windows::Win32::Graphics::Dxgi::DXGI_CREATE_FACTORY_FLAGS(0))?;
            let d2d_factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi)?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let base: windows::Win32::Graphics::Direct2D::ID2D1Factory = d2d_factory.clone().into();
            let round = base.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    ..Default::default()
                },
                None,
            )?;
            let dashed = base.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES { dashStyle: D2D1_DASH_STYLE_DASH, ..Default::default() },
                None,
            )?;
            Ok(Gfx {
                d3d,
                factory,
                d2d_factory,
                d2d_device,
                dcomp,
                fonts: Fonts::new()?,
                strokes: Strokes { round, dashed },
            })
        }
    }

    /// A Direct2D 1.0 view of the factory, for the painter.
    pub fn factory(&self) -> windows::Win32::Graphics::Direct2D::ID2D1Factory {
        self.d2d_factory.clone().into()
    }
}

/// One window's swap chain and composition tree.
pub struct Surface {
    swap: IDXGISwapChain1,
    pub dc: ID2D1DeviceContext,
    _target: IDCompositionTarget,
    _root: IDCompositionVisual,
    _ui: IDCompositionVisual,
    video: IDCompositionVisual,
    pub size: (u32, u32),
    alpha: bool,
}

impl Surface {
    /// `alpha`: the UI layer is premultiplied-alpha so a video visual below can show through (the viewer);
    /// otherwise it is opaque and text uses ClearType.
    pub fn new(gfx: &Gfx, hwnd: HWND, width: u32, height: u32, alpha: bool) -> Result<Surface> {
        let (w, h) = (width.max(1), height.max(1));
        let desc = |mode| DXGI_SWAP_CHAIN_DESC1 {
            Width: w,
            Height: h,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: mode,
            Flags: 0,
        };
        // SAFETY: plain creation of composition objects on live devices; the window outlives the target.
        unsafe {
            let first = if alpha { DXGI_ALPHA_MODE_PREMULTIPLIED } else { DXGI_ALPHA_MODE_IGNORE };
            let swap = match gfx.factory.CreateSwapChainForComposition(&gfx.d3d, &desc(first), None) {
                Ok(s) => s,
                Err(_) => {
                    gfx.factory.CreateSwapChainForComposition(&gfx.d3d, &desc(DXGI_ALPHA_MODE_PREMULTIPLIED), None)?
                }
            };
            let dc = gfx.d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            dc.SetTextAntialiasMode(if alpha {
                D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE
            } else {
                D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE
            });
            // Not topmost: the visual tree is composed beneath the window's child windows (the native text inputs).
            let target = gfx.dcomp.CreateTargetForHwnd(hwnd, false)?;
            let root = gfx.dcomp.CreateVisual()?;
            let video = gfx.dcomp.CreateVisual()?;
            let ui = gfx.dcomp.CreateVisual()?;
            root.AddVisual(&video, false, None)?;
            root.AddVisual(&ui, true, None)?;
            ui.SetContent(&swap)?;
            target.SetRoot(&root)?;
            gfx.dcomp.Commit()?;
            Ok(Surface { swap, dc, _target: target, _root: root, _ui: ui, video, size: (w, h), alpha })
        }
    }

    /// The raw COM pointers the renderer of stage 15 attaches its video visual with (C3 `NativeSurface`).
    pub fn native(&self, gfx: &Gfx) -> NativeSurface {
        NativeSurface::Windows {
            dcomp_device: gfx.dcomp.as_raw() as usize as u64,
            parent_visual: self.video.as_raw() as usize as u64,
            d3d11_device: gfx.d3d.as_raw() as usize as u64,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let (w, h) = (width.max(1), height.max(1));
        if (w, h) == self.size {
            return Ok(());
        }
        // SAFETY: the target bitmap that references the buffers is released before they are resized.
        unsafe {
            self.dc.SetTarget(None);
            self.swap.ResizeBuffers(
                0,
                w,
                h,
                DXGI_FORMAT_UNKNOWN,
                windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }
        self.size = (w, h);
        Ok(())
    }

    /// Binds the back buffer and starts drawing at `dpi`.
    pub fn begin(&self, dpi: f32) -> Result<&ID2D1DeviceContext> {
        // SAFETY: the buffer and bitmap are local; the target is cleared again in `end`.
        unsafe {
            let buffer: IDXGISurface = self.swap.GetBuffer(0)?;
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: if self.alpha { D2D1_ALPHA_MODE_PREMULTIPLIED } else { D2D1_ALPHA_MODE_IGNORE },
                },
                dpiX: dpi,
                dpiY: dpi,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let bitmap = self.dc.CreateBitmapFromDxgiSurface(&buffer, Some(&props))?;
            self.dc.SetTarget(&bitmap);
            self.dc.SetDpi(dpi, dpi);
            self.dc.BeginDraw();
        }
        Ok(&self.dc)
    }

    /// Ends the frame and presents it.
    pub fn end(&self) -> Result<()> {
        // SAFETY: balances `begin`.
        unsafe {
            let drawn = self.dc.EndDraw(None, None);
            self.dc.SetTarget(None);
            drawn?;
            self.swap.Present(0, DXGI_PRESENT(0)).ok()
        }
    }
}
