//! Pointer geometry (PROTOCOL.md 7.3, F-INPUT-03): the wire point of a display to a pixel of the virtual desktop, and
//! that pixel to the 0..65535 range `MOUSEEVENTF_VIRTUALDESK` takes.
//!
//! Everything is in physical pixels, as `DisplayInfo.bounds` is (C3): a display with 175% scaling is described by its
//! real pixel size, so mixed DPI needs no scale factor here. Origins may be negative (a display left of or above the
//! primary one). Out-of-range input is clamped, never wrapped.

use swapdesk_platform::{DisplayId, DisplayInfo, Rect};

/// Pixel of the wire point `(x, y)` (0 to 65535 across the display) inside `bounds`:
/// `round(v * (size - 1) / 65535)` from the display's own origin (PROTOCOL.md 7.3).
pub fn display_pixel(bounds: Rect, x: u16, y: u16) -> (i32, i32) {
    (bounds.x.saturating_add(scale(x, bounds.width)), bounds.y.saturating_add(scale(y, bounds.height)))
}

fn scale(v: u16, size: u32) -> i32 {
    let span = u64::from(size.saturating_sub(1));
    let rounded = (u64::from(v) * span * 2 + 65535) / (2 * 65535);
    i32::try_from(rounded).unwrap_or(i32::MAX)
}

/// The value of `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` that lands on pixel `(px, py)` of the virtual desktop
/// `desk`. Windows maps a value `n` to pixel `floor(n * size / 65536)`; `ceil(offset * 65536 / size)` is the smallest `n`
/// that reaches the pixel and stays inside it. The pixel is clamped to the desktop first.
pub fn virtual_normalized(desk: Rect, px: i32, py: i32) -> (i32, i32) {
    (normalize(px, desk.x, desk.width), normalize(py, desk.y, desk.height))
}

fn normalize(p: i32, origin: i32, size: u32) -> i32 {
    if size == 0 {
        return 0;
    }
    let last = i64::from(size) - 1;
    let offset = (i64::from(p) - i64::from(origin)).clamp(0, last);
    let n = (offset * 65536 + i64::from(size) - 1) / i64::from(size);
    i32::try_from(n.min(65535)).unwrap_or(65535)
}

/// The pixel Windows resolves a normalized value to (`floor(n * size / 65536)`): the inverse used by tests.
pub fn resolved_pixel(n: i32, origin: i32, size: u32) -> i32 {
    let p = i64::from(n) * i64::from(size) / 65536;
    i32::try_from(p + i64::from(origin)).unwrap_or(i32::MAX)
}

/// The wire point on `display` to the pixel of the virtual desktop; `None` when the display is not in the list.
pub fn map_pointer(displays: &[DisplayInfo], display: DisplayId, x: u16, y: u16) -> Option<(i32, i32)> {
    let d = displays.iter().find(|d| d.id == display)?;
    Some(display_pixel(d.bounds, x, y))
}

/// The smallest rectangle holding every display: the virtual desktop when the list is complete.
pub fn union_of(displays: &[DisplayInfo]) -> Option<Rect> {
    let mut it = displays.iter();
    let first = it.next()?;
    let (mut l, mut t) = (i64::from(first.bounds.x), i64::from(first.bounds.y));
    let (mut r, mut b) = (l + i64::from(first.bounds.width), t + i64::from(first.bounds.height));
    for d in it {
        l = l.min(i64::from(d.bounds.x));
        t = t.min(i64::from(d.bounds.y));
        r = r.max(i64::from(d.bounds.x) + i64::from(d.bounds.width));
        b = b.max(i64::from(d.bounds.y) + i64::from(d.bounds.height));
    }
    Some(Rect {
        x: i32::try_from(l).ok()?,
        y: i32::try_from(t).ok()?,
        width: u32::try_from(r - l).ok()?,
        height: u32::try_from(b - t).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_platform::AdapterId;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> Rect {
        Rect { x, y, width, height }
    }

    fn display(id: u16, bounds: Rect, scale_milli: u16, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId(id),
            name: format!("DISPLAY{id}"),
            bounds,
            scale_milli,
            refresh_mhz: 60_000,
            rotation: 0,
            primary,
            adapter: AdapterId(0),
        }
    }

    /// Left display 2560x1440 at 150%, primary 1920x1080 at 100%, right display 3840x2160 at 200%, one above-left.
    fn mixed() -> Vec<DisplayInfo> {
        vec![
            display(1, rect(0, 0, 1920, 1080), 1000, true),
            display(2, rect(-2560, -200, 2560, 1440), 1500, false),
            display(3, rect(1920, -1080, 3840, 2160), 2000, false),
        ]
    }

    #[test]
    fn corners_and_centre_of_a_display() {
        let b = rect(0, 0, 1920, 1080);
        assert_eq!(display_pixel(b, 0, 0), (0, 0));
        assert_eq!(display_pixel(b, 65535, 65535), (1919, 1079));
        // Centre: 32767.5 * 1919 / 65535 = 959.5 -> rounds to 960 (round half up on the exact value 32768 * ... see below).
        assert_eq!(display_pixel(b, 32768, 32768), (960, 540));
    }

    #[test]
    fn rounding_follows_the_protocol_formula() {
        let b = rect(0, 0, 1000, 1000);
        for x in (0..=65535u32).step_by(997) {
            let exact = f64::from(x) * 999.0 / 65535.0;
            let got = display_pixel(b, x as u16, 0).0;
            assert!((f64::from(got) - exact).abs() <= 0.5 + 1e-9, "x={x} exact={exact} got={got}");
        }
    }

    #[test]
    fn negative_origins_offset_the_pixel() {
        let d = mixed();
        assert_eq!(map_pointer(&d, DisplayId(2), 0, 0), Some((-2560, -200)));
        assert_eq!(map_pointer(&d, DisplayId(2), 65535, 65535), Some((-1, 1239)));
        assert_eq!(map_pointer(&d, DisplayId(3), 0, 65535), Some((1920, 1079)));
        assert_eq!(map_pointer(&d, DisplayId(3), 65535, 0), Some((5759, -1080)));
    }

    #[test]
    fn mixed_dpi_uses_physical_pixels_not_scaled_ones() {
        let d = mixed();
        // The 200% display is 3840 physical pixels wide: its right edge is pixel 3839 from its origin, whatever the scale.
        let (x, _) = map_pointer(&d, DisplayId(3), 65535, 0).unwrap();
        assert_eq!(x - 1920, 3839);
        let (x, _) = map_pointer(&d, DisplayId(2), 65535, 0).unwrap();
        assert_eq!(x + 2560, 2559);
        // The same wire point lands proportionally on each display.
        for id in [1u16, 2, 3] {
            let info = d.iter().find(|i| i.id == DisplayId(id)).unwrap();
            let (px, py) = map_pointer(&d, DisplayId(id), 16384, 49152).unwrap();
            let fx = f64::from(px - info.bounds.x) / f64::from(info.bounds.width - 1);
            let fy = f64::from(py - info.bounds.y) / f64::from(info.bounds.height - 1);
            assert!((fx - 0.25).abs() < 0.002 && (fy - 0.75).abs() < 0.002, "display {id}: {fx} {fy}");
        }
    }

    #[test]
    fn unknown_display_maps_nowhere() {
        assert_eq!(map_pointer(&mixed(), DisplayId(9), 1, 1), None);
        assert_eq!(map_pointer(&[], DisplayId(1), 1, 1), None);
    }

    #[test]
    fn union_is_the_virtual_desktop_with_a_negative_origin() {
        assert_eq!(union_of(&mixed()), Some(rect(-2560, -1080, 8320, 2320)));
        assert_eq!(union_of(&[]), None);
        let one = vec![display(1, rect(0, 0, 1920, 1080), 1000, true)];
        assert_eq!(union_of(&one), Some(rect(0, 0, 1920, 1080)));
    }

    #[test]
    fn virtual_normalization_lands_on_every_pixel_of_a_big_desktop() {
        let desk = union_of(&mixed()).unwrap();
        // Every pixel of the x and y ranges, sampled densely, resolves back to itself.
        for px in (desk.x..desk.x + desk.width as i32).step_by(7).chain([desk.x, desk.x + desk.width as i32 - 1]) {
            let (nx, _) = virtual_normalized(desk, px, desk.y);
            assert!((0..=65535).contains(&nx));
            assert_eq!(resolved_pixel(nx, desk.x, desk.width), px, "x pixel {px}");
        }
        for py in (desk.y..desk.y + desk.height as i32).step_by(5).chain([desk.y, desk.y + desk.height as i32 - 1]) {
            let (_, ny) = virtual_normalized(desk, desk.x, py);
            assert_eq!(resolved_pixel(ny, desk.y, desk.height), py, "y pixel {py}");
        }
    }

    #[test]
    fn normalization_clamps_instead_of_wrapping() {
        let desk = rect(-1920, 0, 3840, 1080);
        assert_eq!(virtual_normalized(desk, -100_000, -5), (0, 0));
        let (nx, ny) = virtual_normalized(desk, 100_000, 100_000);
        assert_eq!(resolved_pixel(nx, desk.x, desk.width), 1919);
        assert_eq!(resolved_pixel(ny, desk.y, desk.height), 1079);
        assert!(nx <= 65535 && ny <= 65535);
        assert_eq!(virtual_normalized(rect(0, 0, 0, 0), 5, 5), (0, 0), "an empty desktop never divides by zero");
    }

    #[test]
    fn a_wire_point_goes_through_both_steps_to_the_same_pixel() {
        let d = mixed();
        let desk = union_of(&d).unwrap();
        for (id, x, y) in [(2u16, 0u16, 0u16), (2, 65535, 65535), (3, 12345, 54321), (1, 40000, 100)] {
            let (px, py) = map_pointer(&d, DisplayId(id), x, y).unwrap();
            let (nx, ny) = virtual_normalized(desk, px, py);
            assert_eq!(resolved_pixel(nx, desk.x, desk.width), px);
            assert_eq!(resolved_pixel(ny, desk.y, desk.height), py);
        }
    }

    #[test]
    fn single_pixel_display_does_not_underflow() {
        assert_eq!(display_pixel(rect(10, 20, 1, 1), 65535, 65535), (10, 20));
        assert_eq!(display_pixel(rect(10, 20, 0, 0), 1, 1), (10, 20));
    }
}
