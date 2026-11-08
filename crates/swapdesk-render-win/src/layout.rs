//! Where a decoded picture lands in the video area, and where a window point falls in the picture. Pure integer
//! arithmetic with no OS call, so every scaling mode and the pointer mapping are unit-tested on any host.
//!
//! Coordinates: the *area* is the video rectangle in window pixels (C3 `Renderer::resize`); the swap chain has exactly
//! its size, so [`Layout`] rectangles are relative to the area's top-left corner. Pixels are physical device pixels:
//! with per-monitor DPI v2 the shell hands over physical pixels, and "1:1" means one remote pixel per physical pixel
//! (sharp text at any scale factor).

use swapdesk_platform::{Rect, ScaleMode};

/// A rectangle of unsigned pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PxRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Source rectangle of the picture, destination rectangle in the swap chain (the rest is the black letterbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub src: PxRect,
    pub dest: PxRect,
}

/// `a * b / c`, rounded to nearest, in 64 bits (inputs are pixel counts, far below the overflow limit).
fn mul_div_round(a: u32, b: u32, c: u32) -> u32 {
    ((u64::from(a) * u64::from(b) + u64::from(c) / 2) / u64::from(c.max(1))) as u32
}

impl Layout {
    /// The layout of a `frame` (width, height) in an `area` (width, height) for `mode`.
    ///
    /// - `Fit`: the whole picture, scaled up or down by one factor to the largest size that fits, centered.
    /// - `OneToOne`: unscaled; a picture larger than the area shows its top-left part, a smaller one is centered.
    /// - `Scroll { x, y }`: unscaled with the visible part starting at source pixel `(x, y)`, clamped so the view stays
    ///   inside the picture. The offset is rounded down to even pixels so the 4:2:0 chroma stays aligned.
    ///
    /// `None` when the frame or the area is empty.
    pub fn compute(frame: (u32, u32), area: (u32, u32), mode: ScaleMode) -> Option<Layout> {
        let (fw, fh) = frame;
        let (aw, ah) = area;
        if fw == 0 || fh == 0 || aw == 0 || ah == 0 {
            return None;
        }
        match mode {
            ScaleMode::Fit => {
                // Width limited when aw/fw <= ah/fh.
                let (dw, dh) = if u64::from(aw) * u64::from(fh) <= u64::from(ah) * u64::from(fw) {
                    (aw, mul_div_round(fh, aw, fw).clamp(1, ah))
                } else {
                    (mul_div_round(fw, ah, fh).clamp(1, aw), ah)
                };
                Some(Layout {
                    src: PxRect { x: 0, y: 0, w: fw, h: fh },
                    dest: PxRect { x: (aw - dw) / 2, y: (ah - dh) / 2, w: dw, h: dh },
                })
            }
            ScaleMode::OneToOne => Self::compute(frame, area, ScaleMode::Scroll { x: 0, y: 0 }),
            ScaleMode::Scroll { x, y } => {
                let (vw, vh) = (fw.min(aw), fh.min(ah));
                let sx = (x.max(0) as u32).min(fw - vw) & !1;
                let sy = (y.max(0) as u32).min(fh - vh) & !1;
                Some(Layout {
                    src: PxRect { x: sx, y: sy, w: vw, h: vh },
                    dest: PxRect { x: (aw - vw) / 2, y: (ah - vh) / 2, w: vw, h: vh },
                })
            }
        }
    }

    /// The source pixel under the area-relative point `(x, y)`, or `None` outside the destination rectangle (letterbox).
    /// The pixel whose center the point is nearest to: a destination pixel covers `src.w / dest.w` source pixels.
    pub fn source_pixel(&self, x: i64, y: i64) -> Option<(u32, u32)> {
        let d = self.dest;
        let (rx, ry) = (x - i64::from(d.x), y - i64::from(d.y));
        if rx < 0 || ry < 0 || rx >= i64::from(d.w) || ry >= i64::from(d.h) {
            return None;
        }
        // Center of the destination pixel, in source pixels.
        let sx = (2 * rx + 1) * i64::from(self.src.w) / (2 * i64::from(d.w));
        let sy = (2 * ry + 1) * i64::from(self.src.h) / (2 * i64::from(d.h));
        Some((self.src.x + (sx as u32).min(self.src.w - 1), self.src.y + (sy as u32).min(self.src.h - 1)))
    }

    /// The area-relative point (destination pixels, fractional) where source pixel `(sx, sy)` is drawn, `None` when
    /// the pixel is outside the visible source rectangle. The cursor hot spot is placed with it.
    pub fn dest_point(&self, sx: i32, sy: i32) -> Option<(f32, f32)> {
        let s = self.src;
        let (rx, ry) = (i64::from(sx) - i64::from(s.x), i64::from(sy) - i64::from(s.y));
        if rx < 0 || ry < 0 || rx >= i64::from(s.w) || ry >= i64::from(s.h) {
            return None;
        }
        let d = self.dest;
        Some((
            d.x as f32 + (rx as f32 + 0.5) * d.w as f32 / s.w as f32,
            d.y as f32 + (ry as f32 + 0.5) * d.h as f32 / s.h as f32,
        ))
    }
}

/// PROTOCOL.md 7.3 coordinates of a source pixel on a display `size` wide and high: 0 to 65535 across the display,
/// `pixel = round(norm * (size - 1) / 65535)` on the host, so this is the exact inverse for sizes up to 65536.
pub fn normalize(pixel: u32, size: u32) -> u16 {
    if size <= 1 {
        return 0;
    }
    let max = u64::from(size - 1);
    ((u64::from(pixel.min(size - 1)) * 65535 + max / 2) / max) as u16
}

/// What C3 `Renderer::map_pointer` computes: the window point `(x, y)` (window pixels) to normalized display
/// coordinates, given the video `area`, the shown `frame` size and the layout of the last picture. `None` outside the
/// area or in the letterbox.
pub fn map_pointer(area: Rect, frame: (u32, u32), layout: &Layout, x: i32, y: i32) -> Option<(u16, u16)> {
    let (rx, ry) = (i64::from(x) - i64::from(area.x), i64::from(y) - i64::from(area.y));
    if rx < 0 || ry < 0 || rx >= i64::from(area.width) || ry >= i64::from(area.height) {
        return None;
    }
    let (px, py) = layout.source_pixel(rx, ry)?;
    Some((normalize(px, frame.0), normalize(py, frame.1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fit(frame: (u32, u32), area: (u32, u32)) -> Layout {
        Layout::compute(frame, area, ScaleMode::Fit).expect("layout")
    }

    #[test]
    fn fit_letterboxes_a_wide_picture_in_a_square_area() {
        let l = fit((1920, 1080), (1000, 1000));
        assert_eq!(l.src, PxRect { x: 0, y: 0, w: 1920, h: 1080 });
        // 1080 * 1000 / 1920 = 562.5, rounded to 563; centered: (1000 - 563) / 2 = 218.
        assert_eq!(l.dest, PxRect { x: 0, y: 218, w: 1000, h: 563 });
    }

    #[test]
    fn fit_pillarboxes_a_tall_picture_in_a_wide_area() {
        let l = fit((1080, 1920), (1600, 900));
        assert_eq!(l.dest.h, 900);
        assert_eq!(l.dest.w, 506); // 1080 * 900 / 1920 = 506.25
        assert_eq!(l.dest.x, (1600 - 506) / 2);
        assert_eq!(l.dest.y, 0);
    }

    #[test]
    fn fit_fills_the_area_when_the_aspect_ratios_match_and_scales_up() {
        assert_eq!(fit((1920, 1080), (1920, 1080)).dest, PxRect { x: 0, y: 0, w: 1920, h: 1080 });
        assert_eq!(fit((1920, 1080), (3840, 2160)).dest, PxRect { x: 0, y: 0, w: 3840, h: 2160 });
        assert_eq!(fit((640, 480), (1280, 960)).dest, PxRect { x: 0, y: 0, w: 1280, h: 960 });
        // A retina-sized picture scaled down.
        assert_eq!(fit((2880, 1800), (1440, 900)).dest, PxRect { x: 0, y: 0, w: 1440, h: 900 });
    }

    #[test]
    fn fit_never_leaves_the_area_nor_collapses() {
        for (fw, fh) in [(1920, 1080), (1366, 768), (5120, 1440), (100, 3000), (7, 5)] {
            for (aw, ah) in [(1, 1), (3, 900), (900, 3), (1280, 720), (2559, 1439), (10, 10)] {
                let l = fit((fw, fh), (aw, ah));
                assert!(l.dest.w >= 1 && l.dest.h >= 1, "{fw}x{fh} in {aw}x{ah}: {l:?}");
                assert!(l.dest.x + l.dest.w <= aw && l.dest.y + l.dest.h <= ah, "{fw}x{fh} in {aw}x{ah}: {l:?}");
                // One factor for both axes, within a pixel of rounding.
                let kx = f64::from(l.dest.w) / f64::from(fw);
                let ky = f64::from(l.dest.h) / f64::from(fh);
                assert!(
                    (l.dest.w as f64 - fw as f64 * ky).abs() <= 1.0 || (l.dest.h as f64 - fh as f64 * kx).abs() <= 1.0,
                    "{fw}x{fh} in {aw}x{ah}: {l:?}"
                );
            }
        }
    }

    #[test]
    fn one_to_one_centers_a_small_picture_and_crops_a_large_one_to_the_top_left() {
        let small = Layout::compute((800, 600), (1000, 700), ScaleMode::OneToOne).expect("layout");
        assert_eq!(small.src, PxRect { x: 0, y: 0, w: 800, h: 600 });
        assert_eq!(small.dest, PxRect { x: 100, y: 50, w: 800, h: 600 });
        let large = Layout::compute((1920, 1080), (1000, 700), ScaleMode::OneToOne).expect("layout");
        assert_eq!(large.src, PxRect { x: 0, y: 0, w: 1000, h: 700 });
        assert_eq!(large.dest, PxRect { x: 0, y: 0, w: 1000, h: 700 });
        // No scaling in either case.
        assert_eq!((small.src.w, small.src.h), (small.dest.w, small.dest.h));
        assert_eq!((large.src.w, large.src.h), (large.dest.w, large.dest.h));
    }

    #[test]
    fn scroll_moves_the_view_and_clamps_it_inside_the_picture() {
        let m = |x, y| Layout::compute((1920, 1080), (1000, 700), ScaleMode::Scroll { x, y }).expect("layout");
        assert_eq!(m(100, 50).src, PxRect { x: 100, y: 50, w: 1000, h: 700 });
        // Clamped at the far edge: 1920 - 1000 = 920, 1080 - 700 = 380.
        assert_eq!(m(5000, 5000).src, PxRect { x: 920, y: 380, w: 1000, h: 700 });
        // Negative offsets (a drag past the origin) clamp to zero.
        assert_eq!(m(-30, -4).src, PxRect { x: 0, y: 0, w: 1000, h: 700 });
        // Odd offsets round down to even so the chroma stays aligned.
        assert_eq!(m(101, 51).src, PxRect { x: 100, y: 50, w: 1000, h: 700 });
        // Scrolling a picture that fits changes nothing.
        let fits = Layout::compute((800, 600), (1000, 700), ScaleMode::Scroll { x: 40, y: 40 }).expect("layout");
        assert_eq!(fits.src, PxRect { x: 0, y: 0, w: 800, h: 600 });
    }

    #[test]
    fn empty_sizes_have_no_layout() {
        assert!(Layout::compute((0, 1080), (100, 100), ScaleMode::Fit).is_none());
        assert!(Layout::compute((1920, 1080), (0, 100), ScaleMode::OneToOne).is_none());
    }

    #[test]
    fn normalize_is_the_inverse_of_the_hosts_rounding() {
        // PROTOCOL.md 7.3: pixel = round(x * (width - 1) / 65535).
        for size in [1920u32, 2560, 3840, 1366, 1000, 65536, 2] {
            for px in (0..size).step_by(((size / 997).max(1)) as usize).chain([size - 1]) {
                let n = normalize(px, size);
                let back = ((u64::from(n) * u64::from(size - 1) + 32767) / 65535) as u32;
                assert_eq!(back, px, "size {size} pixel {px} -> {n}");
            }
        }
        assert_eq!(normalize(0, 1920), 0);
        assert_eq!(normalize(1919, 1920), 65535);
        assert_eq!(normalize(5, 1), 0);
    }

    #[test]
    fn map_pointer_hits_the_corners_and_misses_the_letterbox() {
        let area = Rect { x: 40, y: 80, width: 1000, height: 1000 };
        let l = fit((1920, 1080), (1000, 1000));
        let m = |x, y| map_pointer(area, (1920, 1080), &l, x, y);
        // Picture rows 218..781 of the area: window y 298..861.
        assert_eq!(m(40, 298), Some((0, 0)));
        assert_eq!(m(40 + 999, 80 + 218 + 562), Some((65535, 65535)));
        // Letterbox above and below, outside the area on every side.
        assert_eq!(m(500, 80 + 217), None);
        assert_eq!(m(500, 80 + 218 + 563), None);
        assert_eq!(m(39, 500), None);
        assert_eq!(m(1040, 500), None);
        assert_eq!(m(500, 79), None);
        assert_eq!(m(500, 1080), None);
        // The middle maps to the middle.
        let (cx, cy) = m(40 + 500, 80 + 500).expect("inside");
        assert!((i32::from(cx) - 32768).abs() <= 130 && (i32::from(cy) - 32768).abs() <= 130, "{cx} {cy}");
    }

    #[test]
    fn map_pointer_in_one_to_one_is_exact_per_pixel_and_follows_the_scroll() {
        let area = Rect { x: 0, y: 0, width: 800, height: 600 };
        let frame = (1920, 1080);
        let l = Layout::compute(frame, (800, 600), ScaleMode::Scroll { x: 300, y: 200 }).expect("layout");
        for (wx, wy) in [(0, 0), (1, 1), (799, 599), (400, 300)] {
            let (nx, ny) = map_pointer(area, frame, &l, wx, wy).expect("inside");
            let px = ((u64::from(nx) * 1919 + 32767) / 65535) as i32;
            let py = ((u64::from(ny) * 1079 + 32767) / 65535) as i32;
            assert_eq!((px, py), (300 + wx, 200 + wy));
        }
    }

    #[test]
    fn dest_point_places_the_cursor_and_hides_it_when_scrolled_out() {
        let l = Layout::compute((1920, 1080), (800, 600), ScaleMode::Scroll { x: 300, y: 200 }).expect("layout");
        assert_eq!(l.dest_point(300, 200), Some((0.5, 0.5)));
        assert_eq!(l.dest_point(299, 200), None);
        assert_eq!(l.dest_point(1100, 300), None);
        let f = fit((1920, 1080), (960, 540));
        assert_eq!(f.dest_point(960, 540), Some((480.25, 270.25)));
    }
}
