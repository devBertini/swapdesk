//! The remote cursor as a visual of its own above the video: a DirectComposition surface holding the shape, moved by
//! changing the visual's offset, so a cursor move never re-draws or re-presents the video.

use swapdesk_platform::{CursorShape, ErrorKind, Result};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, ID3D11Texture2D};
use windows::Win32::Graphics::DirectComposition::{IDCompositionDevice, IDCompositionSurface, IDCompositionVisual};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM};

use crate::sys::{Ctx, Gpu, err};

pub(crate) struct CursorNode {
    visual: IDCompositionVisual,
    surface: Option<IDCompositionSurface>,
    hot: (u16, u16),
    shape_id: Option<u32>,
    /// The visual shows the surface (it is hidden by dropping the content).
    shown: bool,
}

impl CursorNode {
    /// Adds the cursor visual on top of `parent`'s children.
    pub(crate) fn new(dcomp: &IDCompositionDevice, parent: &IDCompositionVisual) -> Result<Self> {
        // SAFETY: plain creation and insertion on live composition objects.
        let visual = unsafe {
            let v = dcomp.CreateVisual().ctx("CreateVisual (cursor)")?;
            parent.AddVisual(&v, true, None::<&IDCompositionVisual>).ctx("AddVisual (cursor)")?;
            v
        };
        Ok(Self { visual, surface: None, hot: (0, 0), shape_id: None, shown: false })
    }

    pub(crate) fn has_shape(&self) -> bool {
        self.surface.is_some()
    }

    /// Draws a new shape (straight RGBA in, premultiplied BGRA on the surface). Shapes are at most 256x256 (PROTOCOL.md 7.4).
    pub(crate) fn set_shape(&mut self, dcomp: &IDCompositionDevice, shell: &Gpu, shape: &CursorShape) -> Result<()> {
        let (w, h) = (u32::from(shape.width), u32::from(shape.height));
        if w == 0 || h == 0 || w > 256 || h > 256 || shape.rgba.len() != (w * h * 4) as usize {
            return Err(err(ErrorKind::InvalidInput, "set_cursor", "cursor shape size does not match its pixels"));
        }
        if self.shape_id == Some(shape.id) && self.surface.is_some() {
            self.hot = (shape.hot_x, shape.hot_y);
            return Ok(());
        }
        let mut bgra = Vec::with_capacity(shape.rgba.len());
        for px in shape.rgba.as_chunks::<4>().0 {
            let a = u32::from(px[3]);
            let mul = |c: u8| ((u32::from(c) * a + 127) / 255) as u8;
            bgra.extend_from_slice(&[mul(px[2]), mul(px[1]), mul(px[0]), px[3]]);
        }
        // SAFETY: the surface and the texture it hands out are used between BeginDraw and EndDraw only; the pixel buffer
        // holds `w * h * 4` bytes, as checked above.
        unsafe {
            let surface = dcomp
                .CreateSurface(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_PREMULTIPLIED)
                .ctx("CreateSurface (cursor)")?;
            let mut offset = POINT::default();
            let tex: ID3D11Texture2D = surface.BeginDraw(None, &mut offset).ctx("IDCompositionSurface::BeginDraw")?;
            let target = D3D11_BOX {
                left: offset.x as u32,
                top: offset.y as u32,
                front: 0,
                right: offset.x as u32 + w,
                bottom: offset.y as u32 + h,
                back: 1,
            };
            shell.context.UpdateSubresource(&tex, 0, Some(&target), bgra.as_ptr().cast(), w * 4, 0);
            surface.EndDraw().ctx("IDCompositionSurface::EndDraw")?;
            self.visual.SetContent(&surface).ctx("SetContent (cursor)")?;
            self.surface = Some(surface);
        }
        self.shape_id = Some(shape.id);
        self.hot = (shape.hot_x, shape.hot_y);
        self.shown = true;
        Ok(())
    }

    /// Moves the cursor so its hot spot sits at `hot_spot` (window pixels), or hides it (`None`).
    pub(crate) fn place(&mut self, hot_spot: Option<(f32, f32)>) -> Result<()> {
        // SAFETY: property writes on a live visual.
        unsafe {
            match (hot_spot, &self.surface) {
                (Some((x, y)), Some(surface)) => {
                    if !self.shown {
                        self.visual.SetContent(surface).ctx("SetContent (cursor)")?;
                        self.shown = true;
                    }
                    self.visual.SetOffsetX2((x - f32::from(self.hot.0)).round()).ctx("SetOffsetX2")?;
                    self.visual.SetOffsetY2((y - f32::from(self.hot.1)).round()).ctx("SetOffsetY2")?;
                }
                _ => {
                    if self.shown {
                        self.visual.SetContent(None::<&windows::core::IUnknown>).ctx("SetContent (cursor, hide)")?;
                        self.shown = false;
                    }
                }
            }
        }
        Ok(())
    }

    /// Removes the visual from `parent`.
    pub(crate) fn remove(&mut self, parent: &IDCompositionVisual) {
        // SAFETY: both visuals are live; a failure leaves a visual the parent drops with the window anyway.
        unsafe {
            let _ = parent.RemoveVisual(&self.visual);
        }
        self.surface = None;
        self.shown = false;
    }
}
