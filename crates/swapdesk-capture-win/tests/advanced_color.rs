//! The duplicated desktop has the colours applications drew: every display is captured through the duplication (as
//! the host does) and through GDI (8-bit sRGB, what applications see) at the same moment, and the grey levels must
//! match. On a display in Advanced Color (HDR or automatic colour management) the duplication used to come back four
//! times too bright in linear light at an SDR white of 320 nits, clipping everything above sRGB ~138 (2026-11-09); the
//! test reports whether the FP16 conversion ran. Only statistics are printed; no picture is kept.
#![cfg(windows)]

use std::time::Duration;

use swapdesk_capture_win::{CaptureOptions, DevicePool, DxgiScreen};
use swapdesk_platform::{CaptureConfig, DisplayInfo, Pixels, ScreenSource};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDC, GetDIBits, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};

/// The display's pixels through GDI, BGRA rows of `width * 4`.
fn gdi_capture(d: &DisplayInfo) -> Vec<u8> {
    let (w, h) = (d.bounds.width as i32, d.bounds.height as i32);
    let mut out = vec![0u8; (w * h * 4) as usize];
    // SAFETY: a screen DC, a compatible bitmap of the display's size and a DIB read into a buffer of that size; every
    // GDI object is released.
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp.into());
        BitBlt(mem, 0, 0, w, h, Some(screen), d.bounds.x, d.bounds.y, SRCCOPY).expect("BitBlt");
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        GetDIBits(mem, bmp, 0, h as u32, Some(out.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
    out
}

/// For grey GDI pixels, the mean duplication value per GDI value. Returns the gap averaged over those pixels, the
/// worst gap of a level with at least 500 pixels, and how many levels counted.
fn grey_gap(dda: &[u8], stride: usize, gdi: &[u8], w: usize, h: usize) -> (f64, f64, usize) {
    let mut levels = vec![(0u64, 0u64); 256];
    for y in 0..h {
        for x in 0..w {
            let a = &dda[y * stride + x * 4..][..3];
            let b = &gdi[(y * w + x) * 4..][..3];
            if b[0] == b[1] && b[1] == b[2] {
                let l = &mut levels[b[0] as usize];
                l.0 += 1;
                l.1 += u64::from(a[0]) + u64::from(a[1]) + u64::from(a[2]);
            }
        }
    }
    let mut worst = 0f64;
    let mut counted = 0;
    let (mut weighted, mut pixels) = (0f64, 0u64);
    // Below 16 the two paths round near-black differently (measured within 3 levels); the defect is far above it.
    for (v, (n, total)) in levels.iter().enumerate().skip(16) {
        if *n >= 500 {
            counted += 1;
            let gap = (*total as f64 / (*n * 3) as f64 - v as f64).abs();
            worst = worst.max(gap);
            weighted += gap * *n as f64;
            pixels += n;
        }
    }
    (weighted / pixels.max(1) as f64, worst, counted)
}

#[test]
fn the_duplicated_desktop_has_the_colours_gdi_sees() {
    // SAFETY: process-wide DPI awareness before any DC, so GDI works in physical pixels.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let mut screen = DxgiScreen::with_options(DevicePool::new(), CaptureOptions { cpu_readback: true });
    let displays = screen.displays().expect("displays");
    assert!(!displays.is_empty(), "no display to capture");
    for d in displays {
        screen.open(d.id, CaptureConfig { max_fps: 60 }).expect("open");
        let converted_before = screen.stats().converted;
        // The desktop can change between the two captures: three tries, the best one counts.
        let mut best = (f64::MAX, f64::MAX, 0);
        for _ in 0..3 {
            let frame = loop {
                if let Some(f) = screen.next_frame(Duration::from_millis(500)).expect("frame") {
                    break f;
                }
            };
            let gdi = gdi_capture(&d);
            let Pixels::Cpu { stride, data } = &frame.pixels else { panic!("CPU readback asked") };
            let gap = grey_gap(data, *stride as usize, &gdi, frame.width as usize, frame.height as usize);
            if gap.0 < best.0 {
                best = gap;
            }
            screen.close();
            screen.open(d.id, CaptureConfig { max_fps: 60 }).expect("reopen");
        }
        let fp16 = screen.stats().converted > converted_before;
        println!(
            "{} ({}x{}): grey gap {:.2} on average, {:.1} at worst over {} levels; FP16 conversion {}",
            d.name,
            d.bounds.width,
            d.bounds.height,
            best.0,
            best.1,
            best.2,
            if fp16 { "ran (Advanced Color)" } else { "not needed" }
        );
        if best.2 == 0 {
            eprintln!("{}: no grey area large enough to compare, skipped", d.name);
            continue;
        }
        // Before the FP16 path the average gap on the Advanced Color display was ~60 levels (sRGB 50 came back as 101).
        assert!(
            best.0 <= 2.0 && best.1 <= 10.0,
            "{}: the duplication's greys are {:.1} levels off GDI's on average, {:.1} at worst",
            d.name,
            best.0,
            best.1
        );
    }
}
