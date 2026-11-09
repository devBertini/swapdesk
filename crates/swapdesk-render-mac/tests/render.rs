//! The renderer without a window: the parent is a plain `CALayer` that belongs to no view, as the shell's would be before
//! its window shows. What is proved is the layer tree, the geometry, the enqueue and the bookkeeping; what lands on a
//! screen is not (that is the shell's first run, see the report). Run on the MacBook with
//! `scripts/mac/remote.sh test -p swapdesk-render-mac`.
#![cfg(target_os = "macos")]

use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::CALayer;
use swapdesk_codec_mac::annexb::{self, access_units};
use swapdesk_codec_mac::{DecoderOptions, DecoderOutput, VtH264Decoder};
use swapdesk_platform::{
    CursorShape, CursorUpdate, DecodedFrame, DisplayId, ErrorKind, GpuSurface, MonoMicros, NativeSurface, Pixels, Rect,
    Renderer, ScaleMode, VideoDecoder,
};
use swapdesk_proto::{Codec, VideoHeader};
use swapdesk_render_mac::{MacRenderer, RendererOptions};

const STREAM_A: &[u8] = include_bytes!("../../swapdesk-codec-mac/tests/fixtures/a_160x96.h264");
const STREAM_B: &[u8] = include_bytes!("../../swapdesk-codec-mac/tests/fixtures/b_128x90.h264");

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

/// A parent layer of 800x600 points, like the shell's content layer.
fn parent() -> Retained<CALayer> {
    let layer = CALayer::new();
    layer.setFrame(rect(0.0, 0.0, 800.0, 600.0));
    layer
}

fn surface(layer: &CALayer) -> NativeSurface {
    NativeSurface::MacOs { parent_layer: std::ptr::from_ref(layer) as usize as u64 }
}

/// Pixel area at 2 pixels per point.
fn area(x: i32, y: i32, w: u32, h: u32) -> Rect {
    Rect { x, y, width: w, height: h }
}

fn renderer() -> MacRenderer {
    MacRenderer::new(RendererOptions { backing_scale: Some(2.0) })
}

fn cpu_frame(w: u32, h: u32, display: u16, id: u32) -> DecodedFrame {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            data.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 40, 255]);
        }
    }
    DecodedFrame {
        display: DisplayId(display),
        frame_id: id,
        captured_at: MonoMicros(1),
        width: w,
        height: h,
        pixels: Pixels::Cpu { stride: w * 4, data },
    }
}

fn sublayer_count(layer: &CALayer) -> usize {
    // SAFETY: a plain read of the layer's sublayer array.
    unsafe { layer.sublayers() }.map_or(0, |a| a.count())
}

fn first_sublayer_is(parent: &CALayer, child: &CALayer) -> bool {
    // SAFETY: a plain read of the layer's sublayer array.
    unsafe { parent.sublayers() }.is_some_and(|a| a.count() > 0 && std::ptr::eq(&*a.objectAtIndex(0), child))
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

#[test]
fn attach_puts_the_video_container_under_the_shells_own_layers() {
    let parent = parent();
    let toolbar = CALayer::new();
    parent.addSublayer(&toolbar);
    let mut r = renderer();
    r.attach(surface(&parent), area(100, 60, 800, 600)).expect("attach");
    assert_eq!(sublayer_count(&parent), 2);
    let container = r.container_layer().expect("container");
    assert!(first_sublayer_is(&parent, container), "the container is at index 0, below the toolbar");
    // The video layer is in the container; the container is not flipped by us against the parent.
    assert!(r.video_layer().is_some());
    // 100/2, parent 600 high: y = 600 - (60 + 600) / 2 = 270; size 800x600 px = 400x300 pt.
    let f = container.frame();
    assert!(
        approx(f.origin.x, 50.0)
            && approx(f.origin.y, 270.0)
            && approx(f.size.width, 400.0)
            && approx(f.size.height, 300.0),
        "{f:?}"
    );
    r.detach();
    assert_eq!(sublayer_count(&parent), 1, "only the toolbar is left");
}

#[test]
fn a_flipped_parent_counts_y_from_the_top() {
    let parent = parent();
    parent.setGeometryFlipped(true);
    let mut r = renderer();
    r.attach(surface(&parent), area(100, 60, 800, 600)).expect("attach");
    let f = r.container_layer().expect("container").frame();
    assert!(approx(f.origin.y, 30.0), "{f:?}");
}

#[test]
fn the_container_counts_y_downwards_whether_or_not_the_parent_is_flipped() {
    // `geometryFlipped` is relative to the superlayer: inside a flipped parent the container must not flip again, or
    // the cursor (placed from the top) moves upside down.
    for parent_flipped in [false, true] {
        let parent = parent();
        parent.setGeometryFlipped(parent_flipped);
        let mut r = renderer();
        r.attach(surface(&parent), area(0, 0, 800, 600)).expect("attach");
        let container = r.container_layer().expect("container");
        assert_eq!(container.isGeometryFlipped(), !parent_flipped, "parent flipped: {parent_flipped}");
        r.detach();
    }
}

#[test]
fn a_cpu_picture_is_enqueued_and_laid_out_to_fit() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 1000, 1000)).expect("attach");
    assert!(r.layout().is_none() && r.map_pointer(5, 5).is_none(), "nothing shown yet");
    let stats = r.present(cpu_frame(160, 96, 0, 1), ScaleMode::Fit).expect("present");
    assert!(stats.presented_at.0 > 0);
    assert_eq!(r.counters().presented, 1);
    let l = r.layout().expect("layout");
    // 160x96 in 1000x1000: width limited, 96 * 1000 / 160 = 600, centred: y = 200.
    assert_eq!((l.dest.x, l.dest.y, l.dest.w, l.dest.h), (0, 200, 1000, 600));
    // The video layer's frame (points, inside the container) is that rectangle at 2 px per point.
    let v = r.video_layer().expect("video").frame();
    assert!(
        approx(v.origin.x, 0.0)
            && approx(v.origin.y, 100.0)
            && approx(v.size.width, 500.0)
            && approx(v.size.height, 300.0),
        "{v:?}"
    );
    // Pointer: centre of the area is the centre of the picture, the letterbox is nothing.
    let (cx, cy) = r.map_pointer(500, 500).expect("inside");
    assert!((i32::from(cx) - 32768).abs() <= 500 && (i32::from(cy) - 32768).abs() <= 500, "{cx} {cy}");
    assert!(r.map_pointer(500, 100).is_none(), "letterbox above");
    assert!(r.map_pointer(1000, 500).is_none(), "outside the area");
    assert_eq!(r.map_pointer(0, 200), Some((0, 0)));
}

#[test]
fn several_pictures_in_a_row_all_enqueue() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 640, 480)).expect("attach");
    for id in 0..30 {
        r.present(cpu_frame(160, 96, 0, id), ScaleMode::Fit).expect("present");
    }
    let c = r.counters();
    assert_eq!(c.presented, 30);
}

#[test]
fn one_to_one_and_scroll_move_the_picture_inside_the_clipping_container() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 100, 60)).expect("attach");
    r.present(cpu_frame(160, 96, 0, 1), ScaleMode::OneToOne).expect("present");
    let l = r.layout().expect("layout");
    assert_eq!((l.src.w, l.src.h, l.dest.w, l.dest.h), (100, 60, 100, 60));
    // The whole 160x96 picture at 1:1: 80x48 points, starting at the origin; the container (50x30 pt) clips it.
    let v = r.video_layer().expect("video").frame();
    assert!(
        approx(v.origin.x, 0.0) && approx(v.origin.y, 0.0) && approx(v.size.width, 80.0) && approx(v.size.height, 48.0),
        "{v:?}"
    );
    // Scrolling 40 px right and 20 down moves the layer the other way.
    r.set_scale(ScaleMode::Scroll { x: 40, y: 20 }).expect("scroll");
    let v = r.video_layer().expect("video").frame();
    assert!(approx(v.origin.x, -20.0) && approx(v.origin.y, -10.0) && approx(v.size.width, 80.0), "{v:?}");
    let (nx, ny) = r.map_pointer(0, 0).expect("inside");
    // Window (0, 0) is source pixel (40, 20).
    assert_eq!(((u64::from(nx) * 159 + 32767) / 65535, (u64::from(ny) * 95 + 32767) / 65535), (40, 20));
    // Back to Fit without a new picture.
    r.set_scale(ScaleMode::Fit).expect("fit");
    let l = r.layout().expect("layout");
    assert_eq!((l.dest.w, l.dest.h), (100, 60));
}

#[test]
fn resize_moves_the_container_and_keeps_the_picture() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 640, 480)).expect("attach");
    r.present(cpu_frame(160, 96, 0, 1), ScaleMode::Fit).expect("present");
    let before = r.layout().expect("layout");
    r.resize(area(20, 40, 1280, 960)).expect("resize");
    let after = r.layout().expect("layout survives a resize");
    assert_ne!(before, after);
    assert_eq!(after.dest.w, 1280);
    let f = r.container_layer().expect("container").frame();
    assert!(approx(f.size.width, 640.0) && approx(f.size.height, 480.0), "{f:?}");
    // The pointer map follows the new area.
    assert!(r.map_pointer(10, 10).is_none(), "left of the new area");
    assert!(r.map_pointer(20 + 640, 40 + 480).is_some());
    // Nothing was dropped by resizing; a picture after it still goes.
    r.present(cpu_frame(160, 96, 0, 2), ScaleMode::Fit).expect("present");
}

#[test]
fn a_new_picture_size_or_display_relays_out_and_hides_a_cursor_of_another_display() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 800, 600)).expect("attach");
    r.present(cpu_frame(160, 96, 0, 1), ScaleMode::Fit).expect("present");
    let first = r.layout().expect("layout");
    // A cursor on display 0.
    r.set_cursor(&CursorUpdate { display: DisplayId(0), x: 10, y: 10, visible: true, shape: Some(shape(7, 8, 8)) })
        .expect("cursor");
    assert!(r.cursor_visible());
    // The host switches to display 1 with another size.
    r.present(cpu_frame(128, 90, 1, 2), ScaleMode::Fit).expect("present");
    let second = r.layout().expect("layout");
    assert_ne!(first, second);
    assert!(!r.cursor_visible(), "the cursor belongs to display 0 and display 1 is shown");
    // Its position update on display 1 shows it again, with no new shape.
    r.set_cursor(&CursorUpdate { display: DisplayId(1), x: 64, y: 45, visible: true, shape: None }).expect("cursor");
    assert!(r.cursor_visible());
    r.set_cursor(&CursorUpdate { display: DisplayId(1), x: 64, y: 45, visible: false, shape: None }).expect("cursor");
    assert!(!r.cursor_visible());
}

fn shape(id: u32, w: u16, h: u16) -> CursorShape {
    let mut rgba = Vec::new();
    for i in 0..usize::from(w) * usize::from(h) {
        rgba.extend_from_slice(&[255, (i % 256) as u8, 0, if i % 3 == 0 { 0 } else { 255 }]);
    }
    CursorShape { id, width: w, height: h, hot_x: 1, hot_y: 2, rgba }
}

#[test]
fn the_cursor_is_hidden_without_a_shape_and_refuses_a_bad_one() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 800, 600)).expect("attach");
    r.present(cpu_frame(160, 96, 0, 1), ScaleMode::Fit).expect("present");
    r.set_cursor(&CursorUpdate { display: DisplayId(0), x: 5, y: 5, visible: true, shape: None }).expect("cursor");
    assert!(!r.cursor_visible(), "no shape yet");
    let mut bad = shape(1, 4, 4);
    bad.rgba.pop();
    let e = r
        .set_cursor(&CursorUpdate { display: DisplayId(0), x: 5, y: 5, visible: true, shape: Some(bad) })
        .expect_err("bad shape");
    assert_eq!(e.kind, ErrorKind::InvalidInput);
    r.set_cursor(&CursorUpdate { display: DisplayId(0), x: 5, y: 5, visible: true, shape: Some(shape(2, 16, 16)) })
        .expect("cursor");
    assert!(r.cursor_visible());
    // A cursor scrolled out of view is hidden.
    r.resize(area(0, 0, 100, 60)).expect("resize");
    r.set_scale(ScaleMode::Scroll { x: 100, y: 0 }).expect("scroll");
    assert!(!r.cursor_visible(), "source pixel (5, 5) is left of the visible part");
}

#[test]
fn use_before_attach_and_after_detach_is_closed() {
    let mut r = renderer();
    let closed = |e: swapdesk_platform::PlatformError| e.kind == ErrorKind::Closed;
    assert!(closed(r.present(cpu_frame(16, 16, 0, 1), ScaleMode::Fit).expect_err("not attached")));
    assert!(closed(r.resize(area(0, 0, 10, 10)).expect_err("not attached")));
    assert!(r.map_pointer(1, 1).is_none());
    let parent = parent();
    r.attach(surface(&parent), area(0, 0, 100, 100)).expect("attach");
    r.detach();
    assert!(closed(r.present(cpu_frame(16, 16, 0, 1), ScaleMode::Fit).expect_err("detached")));
    r.detach();
    assert_eq!(sublayer_count(&parent), 0);
}

#[test]
fn attach_refuses_a_foreign_surface_a_null_layer_and_can_be_repeated() {
    let mut r = renderer();
    let e = r
        .attach(NativeSurface::Windows { dcomp_device: 1, parent_visual: 2, d3d11_device: 3 }, area(0, 0, 10, 10))
        .expect_err("windows surface");
    assert_eq!(e.kind, ErrorKind::Unsupported);
    let e = r.attach(NativeSurface::MacOs { parent_layer: 0 }, area(0, 0, 10, 10)).expect_err("null layer");
    assert_eq!(e.kind, ErrorKind::InvalidInput);
    let parent = parent();
    r.attach(surface(&parent), area(0, 0, 100, 100)).expect("attach");
    r.attach(surface(&parent), area(0, 0, 100, 100)).expect("attach again");
    assert_eq!(sublayer_count(&parent), 1, "a second attach replaces the first container");
}

#[test]
fn dropping_the_renderer_removes_its_layers() {
    let parent = parent();
    {
        let mut r = renderer();
        r.attach(surface(&parent), area(0, 0, 100, 100)).expect("attach");
        assert_eq!(sublayer_count(&parent), 1);
    }
    assert_eq!(sublayer_count(&parent), 0);
}

#[test]
fn a_surface_that_is_not_a_pixel_buffer_is_unsupported_and_an_empty_picture_invalid() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 100, 100)).expect("attach");
    let mut f = cpu_frame(16, 16, 0, 1);
    f.pixels = Pixels::Gpu(GpuSurface(Box::new(7u32)));
    assert_eq!(r.present(f, ScaleMode::Fit).expect_err("wrong surface").kind, ErrorKind::Unsupported);
    let mut f = cpu_frame(16, 16, 0, 1);
    f.width = 0;
    assert_eq!(r.present(f, ScaleMode::Fit).expect_err("empty").kind, ErrorKind::InvalidInput);
    assert_eq!(r.counters().presented, 0);
}

#[test]
fn a_new_backing_scale_rescales_the_geometry() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 800, 600)).expect("attach");
    r.present(cpu_frame(160, 120, 0, 1), ScaleMode::Fit).expect("present");
    assert!(approx(r.container_layer().expect("container").frame().size.width, 400.0));
    r.set_backing_scale(1.0).expect("scale");
    assert!(approx(r.container_layer().expect("container").frame().size.width, 800.0));
    assert!(r.set_backing_scale(0.0).is_err());
    assert!(r.set_backing_scale(f64::NAN).is_err());
}

fn header(frame_id: u32, unit: &[u8]) -> VideoHeader {
    VideoHeader {
        keyframe: annexb::is_idr(unit),
        config: annexb::parameter_sets(unit).is_some(),
        discontinuity: false,
        display_id: 0,
        frame_id,
        capture_us: 1_000 + u64::from(frame_id),
        encode_us: 0,
        width: 160,
        height: 96,
        codec: Codec::H264,
        payload_len: unit.len() as u32,
    }
}

#[test]
fn a_layer_that_nobody_drains_stays_healthy_and_keeps_taking_the_newest_picture() {
    // No window: the layer shows nothing and its queue does not drain (it reported "not ready" after two pictures on
    // the first run). The renderer flushes the backlog each time instead of dropping the new picture.
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 640, 480)).expect("attach");
    for id in 0..60 {
        r.present(cpu_frame(160, 96, 0, id), ScaleMode::Fit).expect("present");
    }
    let c = r.counters();
    assert_eq!(c.presented, 60);
    assert!(c.flushed > 0, "the queue was flushed at least once: {c:?}");
    // SAFETY: plain property reads on the layer's own renderer.
    let (status, error) = unsafe {
        let sink = r.video_layer().expect("video").sampleBufferRenderer();
        (sink.status(), sink.error().map(|e| e.to_string()))
    };
    assert_ne!(status, objc2_av_foundation::AVQueuedSampleBufferRenderingStatus::Failed, "layer failed: {error:?}");
}

/// Decoder output (NV12 `CVPixelBuffer`s) straight into the renderer: the zero-copy path of the viewer.
#[test]
fn decoded_pictures_go_through_the_whole_video_path() {
    let parent = parent();
    let mut r = renderer();
    r.attach(surface(&parent), area(0, 0, 640, 480)).expect("attach");
    let mut dec = VtH264Decoder::new(DecoderOptions { output: DecoderOutput::Gpu });
    let mut shown = 0;
    for (stream, w, h) in [(STREAM_A, 160u32, 96u32), (STREAM_B, 128, 90)] {
        for (i, range) in access_units(stream).into_iter().enumerate() {
            let unit = &stream[range];
            let frame =
                dec.decode(&header(shown, unit), unit).expect("decode").unwrap_or_else(|| panic!("picture {i}"));
            assert!(matches!(frame.pixels, Pixels::Gpu(_)));
            r.present(frame, ScaleMode::Fit).expect("present");
            shown += 1;
            let l = r.layout().expect("layout");
            // The stream's own aspect ratio fills one side of the 640x480 area.
            assert!(l.src.w == w && l.src.h == h, "layout follows the picture: {l:?}");
        }
    }
    assert_eq!(r.counters().presented, 16);
}
