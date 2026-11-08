//! Offscreen rendering: the same drawing code as the windows, into a WIC bitmap, so every screen can be compared
//! with the design (and tested) without opening a window.

use std::path::Path;

use swapdesk_ui::{UiState, WindowKind};
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1CreateFactory, ID2D1Factory, ID2D1RenderTarget,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICImagingFactory,
    WICBitmapCacheOnLoad, WICBitmapEncoderNoCache, WICRect,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::{HSTRING, Interface, Result};

use crate::gfx::{Fonts, Painter, Strokes};
use crate::view::{Frame, Interaction, Output};
use crate::{screens_main, screens_viewer, view};

/// A rendered window: BGRA pixels (premultiplied) and what the frame registered.
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub output: Output,
    bitmap: IWICBitmap,
}

impl Rendered {
    /// The colour of the pixel at `(x, y)` as 0xRRGGBB.
    pub fn rgb_at(&self, x: u32, y: u32) -> u32 {
        let i = ((y * self.width + x) * 4) as usize;
        (u32::from(self.pixels[i + 2]) << 16) | (u32::from(self.pixels[i + 1]) << 8) | u32::from(self.pixels[i])
    }

    /// The number of distinct colours in the picture (a blank or failed frame has one or two).
    pub fn distinct_colors(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for px in self.pixels.as_chunks::<4>().0 {
            seen.insert([px[0], px[1], px[2]]);
            if seen.len() > 64 {
                break;
            }
        }
        seen.len()
    }

    /// Writes the picture as a PNG.
    pub fn save_png(&self, path: &Path) -> Result<()> {
        // SAFETY: plain WIC calls on objects created here.
        unsafe {
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            let stream = wic.CreateStream()?;
            stream.InitializeFromFilename(&HSTRING::from(path.as_os_str()), GENERIC_WRITE.0)?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.ok_or_else(windows::core::Error::empty)?;
            frame.Initialize(None)?;
            frame.SetSize(self.width, self.height)?;
            let mut format = GUID_WICPixelFormat32bppPBGRA;
            frame.SetPixelFormat(&mut format)?;
            frame.WriteSource(&self.bitmap, std::ptr::null())?;
            frame.Commit()?;
            encoder.Commit()?;
        }
        Ok(())
    }
}

/// Renders the window `kind` of `state` at `size` DIPs and `scale` (1.0 is 100%, 1.5 is 150%).
pub fn render(state: &UiState, kind: WindowKind, size: (f32, f32), scale: f32, ix: &Interaction) -> Result<Rendered> {
    // SAFETY: COM initialisation per thread; a repeated call is harmless.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    render_with(&Fonts::new()?, state, kind, size, scale, ix)
}

/// As [`render`], with the caller's fonts (and their layout cache), as a window keeps them from paint to paint.
pub(crate) fn render_with(
    fonts: &Fonts,
    state: &UiState,
    kind: WindowKind,
    size: (f32, f32),
    scale: f32,
    ix: &Interaction,
) -> Result<Rendered> {
    // SAFETY: plain factory, bitmap and render target creation on objects created here.
    unsafe {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let strokes = {
            use windows::Win32::Graphics::Direct2D::{
                D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_DASH, D2D1_LINE_JOIN_ROUND, D2D1_STROKE_STYLE_PROPERTIES,
            };
            Strokes {
                round: factory.CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES {
                        startCap: D2D1_CAP_STYLE_ROUND,
                        endCap: D2D1_CAP_STYLE_ROUND,
                        dashCap: D2D1_CAP_STYLE_ROUND,
                        lineJoin: D2D1_LINE_JOIN_ROUND,
                        ..Default::default()
                    },
                    None,
                )?,
                dashed: factory.CreateStrokeStyle(
                    &D2D1_STROKE_STYLE_PROPERTIES { dashStyle: D2D1_DASH_STYLE_DASH, ..Default::default() },
                    None,
                )?,
            }
        };
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let (w, h) = ((size.0 * scale).round() as u32, (size.1 * scale).round() as u32);
        let bitmap = wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0 * scale,
            dpiY: 96.0 * scale,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt: ID2D1RenderTarget = factory.CreateWicBitmapRenderTarget(&bitmap, &props)?;
        let painter = Painter::new(&rt, &factory, &strokes, fonts, scale)?;
        rt.BeginDraw();
        let mut frame = Frame::new(&painter, state, ix, size, true);
        match kind {
            WindowKind::Main => screens_main::paint(&mut frame),
            WindowKind::Viewer => screens_viewer::paint_viewer(&mut frame),
            WindowKind::Banner => screens_viewer::paint_banner(&mut frame),
        }
        let output = view::finish(frame, kind == WindowKind::Viewer);
        rt.EndDraw(None, None)?;
        drop(painter);
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let rect = WICRect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
        bitmap.CopyPixels(&rect as *const WICRect as *const _, w * 4, &mut pixels)?;
        let _ = bitmap.cast::<IWICBitmap>();
        Ok(Rendered { width: w, height: h, pixels, output, bitmap })
    }
}

/// The window size a scenario's window has in the design.
pub fn design_size(kind: WindowKind) -> (f32, f32) {
    match kind {
        WindowKind::Banner => screens_viewer::banner_size(),
        _ => (swapdesk_ui::tokens::layout::DESIGN_W, swapdesk_ui::tokens::layout::DESIGN_H),
    }
}
