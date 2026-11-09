//! The host's cursor shape as the local pointer over the video, while the viewer controls the host. The drawn remote
//! arrow would trail the local pointer by a network round trip; the local pointer moves with the hand and only its
//! shape comes from the host (an I-beam over text, a hand over a link).
//!
//! Microsoft documentation: `CreateIconIndirect` and `ICONINFO` (learn.microsoft.com/windows/win32/api/winuser/nf-winuser-createiconindirect:
//! a 32-bit colour bitmap with an alpha channel carries the transparency, the mask is still required),
//! `CreateDIBSection` (top-down when `biHeight` is negative), `DestroyCursor`.

use std::cell::RefCell;
use std::ffi::c_void;

use swapdesk_platform::CursorShape;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyCursor, HCURSOR, ICONINFO};
use windows::core::Result;

/// Shapes are at most 256x256 (PROTOCOL.md 7.4).
const MAX_SIDE: u16 = 256;

/// One Win32 cursor for the last shape asked for; a new shape replaces (and destroys) it.
#[derive(Default)]
pub struct CursorCache {
    current: RefCell<Option<(u32, usize, HCURSOR)>>,
}

impl CursorCache {
    /// The cursor for `shape`, built on first use. `None` when the shape is malformed or Windows refuses it (the
    /// caller then shows the arrow).
    pub fn get(&self, shape: &CursorShape) -> Option<HCURSOR> {
        let key = (shape.id, shape.rgba.len());
        if let Some((id, len, handle)) = *self.current.borrow()
            && (id, len) == key
        {
            return Some(handle);
        }
        let handle = create(shape).ok()?;
        if let Some((_, _, old)) = self.current.replace(Some((key.0, key.1, handle))) {
            // SAFETY: a cursor this cache created with CreateIconIndirect and no longer hands out.
            unsafe {
                let _ = DestroyCursor(old);
            }
        }
        Some(handle)
    }
}

impl Drop for CursorCache {
    fn drop(&mut self) {
        if let Some((_, _, handle)) = self.current.take() {
            // SAFETY: as in `get`.
            unsafe {
                let _ = DestroyCursor(handle);
            }
        }
    }
}

/// Straight RGBA rows (top first) as the BGRA rows of a 32-bit DIB.
pub fn bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    out
}

fn create(shape: &CursorShape) -> Result<HCURSOR> {
    let (w, h) = (shape.width, shape.height);
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || shape.rgba.len() != usize::from(w) * usize::from(h) * 4 {
        return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: i32::from(w),
            biHeight: -i32::from(h),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let pixels = bgra(&shape.rgba);
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: a top-down 32-bit DIB of w x h; `bits` receives its memory, which holds exactly `pixels.len()` bytes
    // (w * 4 bytes per row, no padding at 32 bits) and is written once before the bitmap is used.
    let color = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    // SAFETY: as above.
    unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len()) };
    // The 1-bit mask must exist; with an alpha channel in the colour bitmap it is ignored. Rows are WORD-aligned.
    let mask_bits = vec![0u8; usize::from(w).div_ceil(16) * 2 * usize::from(h)];
    // SAFETY: `mask_bits` holds a full monochrome bitmap of w x h and outlives the call (CreateBitmap copies it).
    let mask = unsafe { CreateBitmap(i32::from(w), i32::from(h), 1, 1, Some(mask_bits.as_ptr().cast())) };
    let icon = ICONINFO {
        fIcon: false.into(),
        xHotspot: u32::from(shape.hot_x.min(w - 1)),
        yHotspot: u32::from(shape.hot_y.min(h - 1)),
        hbmMask: mask,
        hbmColor: color,
    };
    // SAFETY: both bitmaps are valid; CreateIconIndirect copies them, so they are deleted right after.
    let made = unsafe { CreateIconIndirect(&icon) };
    // SAFETY: the bitmaps created above, owned here.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
    }
    Ok(HCURSOR(made?.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, HICON};

    fn shape(id: u32, w: u16, h: u16, hot: (u16, u16)) -> CursorShape {
        CursorShape {
            id,
            width: w,
            height: h,
            hot_x: hot.0,
            hot_y: hot.1,
            rgba: vec![200; usize::from(w) * usize::from(h) * 4],
        }
    }

    #[test]
    fn rgba_becomes_bgra_with_alpha_kept() {
        assert_eq!(bgra(&[1, 2, 3, 4, 5, 6, 7, 8]), vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn a_shape_becomes_a_cursor_with_its_hot_spot_and_is_reused_until_the_shape_changes() {
        let cache = CursorCache::default();
        let a = cache.get(&shape(1, 32, 32, (4, 9))).expect("cursor");
        let mut info = ICONINFO::default();
        // SAFETY: a live cursor of the cache; the bitmaps GetIconInfo returns are deleted.
        unsafe {
            GetIconInfo(HICON(a.0), &mut info).expect("icon info");
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }
        assert!(!info.fIcon.as_bool(), "a cursor, not an icon");
        assert_eq!((info.xHotspot, info.yHotspot), (4, 9));
        assert_eq!(cache.get(&shape(1, 32, 32, (4, 9))), Some(a), "same shape, same cursor");
        let b = cache.get(&shape(2, 16, 24, (40, 40))).expect("cursor");
        assert_ne!(a, b);
    }

    #[test]
    fn a_malformed_shape_gives_no_cursor() {
        let cache = CursorCache::default();
        let mut bad = shape(3, 8, 8, (0, 0));
        bad.rgba.pop();
        assert!(cache.get(&bad).is_none());
        assert!(cache.get(&shape(4, 0, 8, (0, 0))).is_none());
        assert!(cache.get(&shape(5, 300, 8, (0, 0))).is_none());
    }
}
