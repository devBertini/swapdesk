//! The renderer on a real GPU in a real window, read back through the swap chain buffer (stage 15): colour space,
//! scaling modes, letterbox, resize, the pointer mapping against what was drawn, the cursor, and the error cases.
#![cfg(windows)]

#[path = "common/harness.rs"]
mod harness;

use harness::{Pref, ShellStub};
use swapdesk_platform::{
    CursorShape, CursorUpdate, DecodedFrame, DisplayId, ErrorKind, GpuSurface, MonoMicros, Pixels, Rect, Renderer,
    ScaleMode,
};
use swapdesk_render_win::{Probe, RendererOptions, WinRenderer};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

const AREA: Rect = Rect { x: 0, y: 0, width: 800, height: 600 };

struct Rig {
    // Declared first so the renderer drops before the window and devices it draws into.
    renderer: WinRenderer,
    shell: ShellStub,
}

fn rig(area: Rect) -> Rig {
    let shell = ShellStub::new(Pref::Min, (area.width + 100, area.height + 100), None).expect("shell stub");
    let mut renderer = WinRenderer::new(RendererOptions::default());
    renderer.attach(shell.surface(), area).expect("attach");
    renderer.set_probe(Some((0, 0, area.width, area.height)));
    Rig { renderer, shell }
}

/// Left and right colours of the two-colour test pictures (BT.709 limited range): red and blue.
const RED_YUV: (u8, u8, u8) = (63, 102, 240);
const BLUE_YUV: (u8, u8, u8) = (32, 240, 118);

fn nv12_frame(
    device: &ID3D11Device,
    w: u32,
    h: u32,
    split: u32,
    left: (u8, u8, u8),
    right: (u8, u8, u8),
) -> DecodedFrame {
    let mut data = vec![0u8; (w * h * 3 / 2) as usize];
    for y in 0..h {
        for x in 0..w {
            data[(y * w + x) as usize] = if x < split { left.0 } else { right.0 };
        }
    }
    let uv = (w * h) as usize;
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            let (u, v) = if x * 2 < split { (left.1, left.2) } else { (right.1, right.2) };
            data[uv + (y * w + x * 2) as usize] = u;
            data[uv + (y * w + x * 2 + 1) as usize] = v;
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr().cast(), SysMemPitch: w, SysMemSlicePitch: 0 };
    let mut tex: Option<ID3D11Texture2D> = None;
    // SAFETY: valid descriptor, initial data of the NV12 layout (Y plane, then interleaved UV) and out pointer.
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }.expect("NV12 texture");
    DecodedFrame {
        display: DisplayId(0),
        frame_id: 0,
        captured_at: MonoMicros(0),
        width: w,
        height: h,
        pixels: Pixels::Gpu(GpuSurface(Box::new(tex.expect("texture")))),
    }
}

/// BGRA frame, left `split` columns `left`, the rest `right` (B, G, R).
fn bgra_frame(w: u32, h: u32, split: u32, left: [u8; 3], right: [u8; 3]) -> DecodedFrame {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..h {
        for x in 0..w {
            let c = if x < split { left } else { right };
            data.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    DecodedFrame {
        display: DisplayId(0),
        frame_id: 0,
        captured_at: MonoMicros(0),
        width: w,
        height: h,
        pixels: Pixels::Cpu { stride: w * 4, data },
    }
}

fn pixel(p: &Probe, x: u32, y: u32) -> [u8; 3] {
    let i = (y * p.stride + x * 4) as usize;
    [p.data[i + 2], p.data[i + 1], p.data[i]] // R, G, B
}

fn near(got: [u8; 3], want: [u8; 3], tol: i32) -> bool {
    got.iter().zip(&want).all(|(g, w)| (i32::from(*g) - i32::from(*w)).abs() <= tol)
}

fn present_and_read(r: &mut Rig, frame: DecodedFrame, scale: ScaleMode) -> Probe {
    r.renderer.present(frame, scale).expect("present");
    r.renderer.take_probe().expect("probe").expect("a probe was taken")
}

#[test]
fn nv12_bt709_limited_range_comes_out_as_full_range_rgb() {
    let mut r = rig(AREA);
    let device = r.renderer.device().expect("device");
    // White, black and the two colours: limited-range 16..235 must stretch to 0..255.
    for (yuv, want) in [
        ((235, 128, 128), [255, 255, 255]),
        ((16, 128, 128), [0, 0, 0]),
        (RED_YUV, [255, 0, 0]),
        (BLUE_YUV, [0, 0, 255]),
    ] {
        let p = present_and_read(&mut r, nv12_frame(&device, 800, 600, 400, yuv, yuv), ScaleMode::Fit);
        let got = pixel(&p, 400, 300);
        assert!(near(got, want, 4), "YUV {yuv:?}: got {got:?}, want {want:?}");
    }
}

#[test]
fn fit_letterboxes_with_black_and_keeps_the_picture_in_place() {
    let mut r = rig(AREA);
    let device = r.renderer.device().expect("device");
    // 640x360 into 800x600: scaled to 800x450, bars of 75 rows above and below.
    let p = present_and_read(&mut r, nv12_frame(&device, 640, 360, 320, RED_YUV, BLUE_YUV), ScaleMode::Fit);
    let layout = r.renderer.layout().expect("layout");
    assert_eq!((layout.dest.x, layout.dest.y, layout.dest.w, layout.dest.h), (0, 75, 800, 450));
    assert!(near(pixel(&p, 400, 20), [0, 0, 0], 2), "top bar {:?}", pixel(&p, 400, 20));
    assert!(near(pixel(&p, 400, 580), [0, 0, 0], 2), "bottom bar {:?}", pixel(&p, 400, 580));
    assert!(near(pixel(&p, 100, 300), [255, 0, 0], 6), "left half {:?}", pixel(&p, 100, 300));
    assert!(near(pixel(&p, 700, 300), [0, 0, 255], 6), "right half {:?}", pixel(&p, 700, 300));
    // The colour boundary sits at the middle of the picture (source x 320 -> window x 400).
    assert!(near(pixel(&p, 390, 300), [255, 0, 0], 6) && near(pixel(&p, 410, 300), [0, 0, 255], 6));
}

#[test]
fn one_to_one_and_scroll_show_unscaled_parts_of_a_larger_picture() {
    let mut r = rig(AREA);
    let device = r.renderer.device().expect("device");
    // 1000x700 picture, colour change at source x 500; the area shows 800x600 of it.
    let p = present_and_read(&mut r, nv12_frame(&device, 1000, 700, 500, RED_YUV, BLUE_YUV), ScaleMode::OneToOne);
    assert!(
        near(pixel(&p, 450, 300), [255, 0, 0], 6) && near(pixel(&p, 550, 300), [0, 0, 255], 6),
        "1:1 boundary at window x 500"
    );
    let p = present_and_read(
        &mut r,
        nv12_frame(&device, 1000, 700, 500, RED_YUV, BLUE_YUV),
        ScaleMode::Scroll { x: 100, y: 50 },
    );
    assert!(
        near(pixel(&p, 350, 300), [255, 0, 0], 6) && near(pixel(&p, 450, 300), [0, 0, 255], 6),
        "scrolled by 100: boundary at window x 400"
    );
    // The scroll stops at the picture's edge: 1000 - 800 = 200 is the most.
    let p = present_and_read(
        &mut r,
        nv12_frame(&device, 1000, 700, 500, RED_YUV, BLUE_YUV),
        ScaleMode::Scroll { x: 900, y: 0 },
    );
    assert!(
        near(pixel(&p, 250, 300), [255, 0, 0], 6) && near(pixel(&p, 350, 300), [0, 0, 255], 6),
        "clamped to 200: boundary at window x 300"
    );
    // A picture smaller than the area is centered at 1:1 with black around it.
    let p = present_and_read(&mut r, nv12_frame(&device, 400, 300, 200, RED_YUV, BLUE_YUV), ScaleMode::OneToOne);
    assert!(near(pixel(&p, 100, 300), [0, 0, 0], 2), "left margin");
    assert!(near(pixel(&p, 250, 300), [255, 0, 0], 6) && near(pixel(&p, 550, 300), [0, 0, 255], 6));
}

#[test]
fn cpu_frames_are_drawn_too() {
    let mut r = rig(AREA);
    let p = present_and_read(&mut r, bgra_frame(400, 300, 200, [0, 200, 0], [200, 0, 0]), ScaleMode::Fit);
    assert!(near(pixel(&p, 100, 300), [0, 200, 0], 2), "{:?}", pixel(&p, 100, 300));
    assert!(near(pixel(&p, 700, 300), [0, 0, 200], 2), "{:?}", pixel(&p, 700, 300));
}

#[test]
fn resize_rebuilds_the_chain_and_draws_the_last_picture_again() {
    let mut r = rig(AREA);
    let device = r.renderer.device().expect("device");
    present_and_read(&mut r, nv12_frame(&device, 640, 360, 320, RED_YUV, BLUE_YUV), ScaleMode::Fit);
    // A static host sends no new picture: resizing alone must leave the picture on screen.
    r.renderer.resize(Rect { x: 20, y: 10, width: 400, height: 300 }).expect("resize");
    let p = r.renderer.take_probe().expect("probe").expect("redrawn on resize");
    assert_eq!((p.width, p.height), (400, 300));
    let layout = r.renderer.layout().expect("layout");
    assert_eq!((layout.dest.w, layout.dest.h), (400, 225));
    assert!(near(pixel(&p, 50, 150), [255, 0, 0], 6) && near(pixel(&p, 350, 150), [0, 0, 255], 6));
    assert!(near(pixel(&p, 200, 10), [0, 0, 0], 2), "letterbox after resize");
    // The scale mode changes without a new picture too.
    r.renderer.set_scale(ScaleMode::OneToOne).expect("set_scale");
    let layout = r.renderer.layout().expect("layout");
    assert_eq!((layout.src.w, layout.src.h, layout.dest.w, layout.dest.h), (400, 300, 400, 300));
}

#[test]
fn map_pointer_agrees_with_what_was_drawn() {
    let area = Rect { x: 30, y: 20, width: 800, height: 600 };
    let mut r = rig(area);
    let device = r.renderer.device().expect("device");
    assert_eq!(r.renderer.map_pointer(100, 100), None, "no picture yet");
    let p = present_and_read(&mut r, nv12_frame(&device, 640, 360, 320, RED_YUV, BLUE_YUV), ScaleMode::Fit);
    let source_x = |nx: u16| ((u64::from(nx) * 639 + 32767) / 65535) as u32;
    // Window points left and right of the boundary (area x offset 30): the mapped source pixel is on the drawn side.
    for (wx, left) in [(30 + 100, true), (30 + 390, true), (30 + 410, false), (30 + 700, false)] {
        let (nx, ny) = r.renderer.map_pointer(wx, 20 + 300).expect("inside the picture");
        assert_eq!(source_x(nx) < 320, left, "window x {wx} -> source x {}", source_x(nx));
        let drawn = pixel(&p, (wx - 30) as u32, 300);
        assert!(near(drawn, if left { [255, 0, 0] } else { [0, 0, 255] }, 6), "drawn {drawn:?}");
        assert!((i32::from(ny) - 32768).abs() < 200, "middle row maps to the middle, got {ny}");
    }
    // Letterbox, outside the area, and the corners of the picture.
    assert_eq!(r.renderer.map_pointer(30 + 400, 20 + 10), None, "top bar");
    assert_eq!(r.renderer.map_pointer(29, 300), None, "left of the area");
    assert_eq!(r.renderer.map_pointer(30 + 800, 300), None, "right of the area");
    assert_eq!(r.renderer.map_pointer(30, 20 + 75), Some((0, 0)));
    assert_eq!(r.renderer.map_pointer(30 + 799, 20 + 75 + 449), Some((65535, 65535)));
}

#[test]
fn the_cursor_moves_hides_and_rejects_a_bad_shape() {
    let mut r = rig(AREA);
    let device = r.renderer.device().expect("device");
    present_and_read(&mut r, nv12_frame(&device, 640, 360, 320, RED_YUV, BLUE_YUV), ScaleMode::Fit);
    let shape = CursorShape { id: 7, width: 12, height: 20, hot_x: 1, hot_y: 2, rgba: vec![200; 12 * 20 * 4] };
    let at = |x, y, visible, shape: Option<CursorShape>| CursorUpdate { display: DisplayId(0), x, y, visible, shape };
    r.renderer.set_cursor(&at(320, 180, true, Some(shape.clone()))).expect("shape and position");
    r.renderer.set_cursor(&at(321, 181, true, None)).expect("move");
    r.renderer.set_cursor(&at(1000, 181, true, None)).expect("outside the display hides it");
    r.renderer.set_cursor(&at(10, 10, false, None)).expect("hide");
    r.renderer.set_cursor(&at(10, 10, true, Some(shape))).expect("show again with the cached shape");
    let bad = CursorShape { id: 8, width: 4, height: 4, hot_x: 0, hot_y: 0, rgba: vec![0; 10] };
    assert_eq!(r.renderer.set_cursor(&at(0, 0, true, Some(bad))).unwrap_err().kind, ErrorKind::InvalidInput);
    // The video is untouched by cursor moves.
    assert!(r.renderer.layout().is_some());
}

#[test]
fn a_texture_from_another_device_and_calls_after_detach_are_refused() {
    let mut r = rig(AREA);
    let mut other = None;
    // SAFETY: plain device creation with out pointers to locals.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            Default::default(),
            None,
            D3D11_SDK_VERSION,
            Some(&mut other),
            None,
            None,
        )
    }
    .expect("second device");
    let foreign = nv12_frame(&other.expect("device"), 64, 64, 32, RED_YUV, BLUE_YUV);
    let e = r.renderer.present(foreign, ScaleMode::Fit).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
    assert!(e.detail.contains("WinRenderer::device"), "{e}");
    r.renderer.detach();
    assert_eq!(
        r.renderer.present(bgra_frame(8, 8, 4, [0; 3], [255; 3]), ScaleMode::Fit).unwrap_err().kind,
        ErrorKind::Closed
    );
    assert_eq!(r.renderer.resize(AREA).unwrap_err().kind, ErrorKind::Closed);
    assert_eq!(r.renderer.map_pointer(10, 10), None);
    // A fresh attach works again on the same tree.
    r.renderer.attach(r.shell.surface(), AREA).expect("attach again");
    r.renderer.present(bgra_frame(8, 8, 4, [0; 3], [255; 3]), ScaleMode::Fit).expect("present after re-attach");
}

#[test]
fn a_surface_of_another_os_or_with_null_pointers_does_not_attach() {
    let mut renderer = WinRenderer::default();
    let e = renderer.attach(swapdesk_platform::NativeSurface::MacOs { parent_layer: 1 }, AREA).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    let e = renderer
        .attach(swapdesk_platform::NativeSurface::Windows { dcomp_device: 0, parent_visual: 0, d3d11_device: 0 }, AREA)
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InvalidInput);
}
