//! Capture on this PC (stage 12). Only sizes, timestamps and the flicker window's own pixels are asserted or printed:
//! the owner's screen is never saved or shown. Tests take a cross-process lock because DDA allows four duplications
//! per session and nextest runs the tests of this binary in parallel processes.
#![cfg(windows)]

mod common;

use common::{Flicker, aware, is_flicker_pixel, primary, serial};
use std::time::{Duration, Instant};
use swapdesk_capture_win::geometry;
use swapdesk_capture_win::{
    CaptureOptions, DevicePool, DisplayWatcher, DxgiScreen, QpcClock, WatchEvent, adapter_map, displays, gpu_payload,
};
use swapdesk_platform::{CaptureConfig, Clock, DisplayId, ErrorKind, Frame, MonoMicros, Pixels, ScreenSource};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

fn open_primary(options: CaptureOptions, max_fps: u16) -> (DxgiScreen, swapdesk_platform::DisplayInfo) {
    let p = primary();
    let mut screen = DxgiScreen::with_options(DevicePool::new(), options);
    screen.open(p.id, CaptureConfig { max_fps }).expect("open the primary display");
    (screen, p)
}

/// Frames until `n` arrived or `budget` passed.
fn grab(screen: &mut DxgiScreen, n: usize, budget: Duration) -> Vec<Frame> {
    let started = Instant::now();
    let mut frames = Vec::with_capacity(n);
    while frames.len() < n && started.elapsed() < budget {
        if let Some(f) = screen.next_frame(Duration::from_millis(500)).expect("next_frame") {
            frames.push(f);
        }
    }
    frames
}

/// Like `grab`, but keeps only the timestamps and drops each frame at once.
fn grab_stamps(screen: &mut DxgiScreen, n: usize, budget: Duration) -> Vec<MonoMicros> {
    let started = Instant::now();
    let mut stamps = Vec::with_capacity(n);
    while stamps.len() < n && started.elapsed() < budget {
        if let Some(f) = screen.next_frame(Duration::from_millis(500)).expect("next_frame") {
            stamps.push(f.captured_at);
        }
    }
    stamps
}

fn assert_stamps_non_decreasing(stamps: &[MonoMicros]) {
    for pair in stamps.windows(2) {
        assert!(pair[1] >= pair[0], "timestamps went back: {:?} then {:?}", pair[0], pair[1]);
    }
}

#[test]
fn adapter_luid_and_output_of_every_display_r3() {
    aware();
    let map = adapter_map().expect("adapter map");
    println!("R3 adapter map of this PC:");
    for a in &map {
        println!(
            "  adapter luid={:#018x} vendor={:#06x} vram={} MiB software={} \"{}\" outputs={}",
            a.luid,
            a.vendor_id,
            a.dedicated_video_mib,
            a.software,
            a.description,
            a.outputs.len()
        );
        for o in &a.outputs {
            println!(
                "    output #{} {} attached={} hmonitor={:#x} dxgi_rect=({},{} {}x{})",
                o.output_index,
                o.device_name,
                o.attached_to_desktop,
                o.hmonitor,
                o.dxgi_rect.x,
                o.dxgi_rect.y,
                o.dxgi_rect.width,
                o.dxgi_rect.height
            );
        }
    }
    let list = displays().expect("displays");
    println!("displays (C3):");
    for d in &list {
        println!(
            "  DisplayId({}) \"{}\" bounds=({},{} {}x{}) scale={}% logical={:?} refresh={} Hz rotation={} primary={} adapter luid={:#018x}",
            d.id.0,
            d.name,
            d.bounds.x,
            d.bounds.y,
            d.bounds.width,
            d.bounds.height,
            d.scale_milli / 10,
            geometry::logical_size(d),
            d.refresh_mhz / 1000,
            d.rotation,
            d.primary,
            d.adapter.0
        );
    }
    assert!(!list.is_empty(), "no display");
    assert_eq!(list.iter().filter(|d| d.primary).count(), 1, "exactly one primary");
    assert!(list[0].primary, "the primary comes first");
    let p = &list[0];
    assert_eq!((p.bounds.x, p.bounds.y), (0, 0), "the primary sits at the origin");
    let mut ids: Vec<u16> = list.iter().map(|d| d.id.0).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), list.len(), "ids are unique");
    for d in &list {
        assert!(d.bounds.width > 0 && d.bounds.height > 0);
        assert!(d.scale_milli >= 1000, "scale {} under 100%", d.scale_milli);
        assert!(d.refresh_mhz >= 24_000, "refresh {} mHz", d.refresh_mhz);
        let owner = map.iter().find(|a| a.luid == d.adapter.0).expect("every display's adapter is in the map");
        let out = owner
            .outputs
            .iter()
            .find(|o| o.display.as_ref().is_some_and(|x| x.id == d.id))
            .expect("the owner lists the output");
        // GDI's physical rectangle and DXGI's (read per-monitor DPI aware) agree.
        assert_eq!((out.dxgi_rect.width, out.dxgi_rect.height), (d.bounds.width, d.bounds.height), "{} size", d.name);
        assert_eq!((out.dxgi_rect.x, out.dxgi_rect.y), (d.bounds.x, d.bounds.y), "{} origin", d.name);
    }
    let virt = geometry::virtual_desktop(&list).expect("virtual desktop");
    println!("virtual desktop: ({},{} {}x{})", virt.x, virt.y, virt.width, virt.height);
}

#[test]
fn captures_300_frames_from_the_primary_display() {
    aware();
    let _lock = serial();
    let p = primary();
    let flicker = Flicker::start(&p);
    let (mut screen, p) = open_primary(CaptureOptions::default(), 60);
    let (w, h) =
        if p.rotation % 180 == 0 { (p.bounds.width, p.bounds.height) } else { (p.bounds.height, p.bounds.width) };
    let mut stamps = Vec::with_capacity(300);
    let mut touching = 0usize;
    let started = Instant::now();
    // Frames are consumed and dropped as they arrive, as an encoder would: the pool must not grow.
    while stamps.len() < 300 {
        assert!(started.elapsed() < Duration::from_secs(20), "only {} frames in 20 s", stamps.len());
        let Some(f) = screen.next_frame(Duration::from_millis(500)).expect("next_frame") else { continue };
        assert_eq!((f.width, f.height), (w, h), "frame size is the display's mode");
        assert_eq!(f.display, p.id);
        assert!(f.content_changed);
        let g = gpu_payload(&f).expect("the default path delivers GPU frames");
        assert_eq!((g.width(), g.height()), (w, h));
        // Dirty rectangles reach the window that changes (empty = unknown = whole frame, also fine).
        let reaches = f.dirty.is_empty()
            || f.dirty.iter().any(|r| {
                geometry::intersects(
                    &swapdesk_platform::Rect {
                        x: r.x + p.bounds.x,
                        y: r.y + p.bounds.y,
                        width: r.width,
                        height: r.height,
                    },
                    &flicker.rect,
                )
            });
        touching += usize::from(reaches);
        stamps.push(f.captured_at);
    }
    let elapsed = started.elapsed();
    assert!(stamps.windows(2).all(|p| p[1] >= p[0]), "timestamps must not go back");
    let strictly = stamps.windows(2).filter(|p| p[1] > p[0]).count();
    assert!(strictly >= 298, "{strictly} of 299 steps strictly increase");
    // Timestamps are on the machine's QPC clock: in the past, within the run.
    let now = QpcClock.now();
    let last = *stamps.last().expect("frames");
    assert!(last <= now && now.0 - last.0 < 5_000_000, "last frame {last:?}, now {now:?}");
    let span_s = (last.0 - stamps[0].0) as f64 / 1e6;
    let s = screen.stats();
    println!(
        "300 frames in {elapsed:?} (timestamp span {span_s:.2} s, {:.1} fps); dirty rects reach the window in {touching}/300; {s:?}",
        299.0 / span_s
    );
    assert!(touching >= 270, "dirty rects miss the window that changes in {} frames", 300 - touching);
    assert!(
        s.textures_created <= 4,
        "the pool allocated {} textures for a consumer that holds none",
        s.textures_created
    );
    // Held frames are legal: the pool hands out more textures, and takes at most a few back.
    let held = grab(&mut screen, 12, Duration::from_secs(5));
    assert_eq!(held.len(), 12);
    drop(held);
    screen.close();
}

#[test]
#[ignore = "interactive: needs the display on and unlocked; run on demand with --run-ignored only"]
fn cpu_readback_returns_a_real_image_of_the_flicker_window() {
    aware();
    let _lock = serial();
    let p = primary();
    let flicker = Flicker::start(&p);
    let (mut screen, p) = open_primary(CaptureOptions { cpu_readback: true }, 60);
    let frames = grab(&mut screen, 5, Duration::from_secs(5));
    assert_eq!(frames.len(), 5);
    let (cx, cy) = flicker.center_local(&p);
    for f in &frames {
        let Pixels::Cpu { stride, data } = &f.pixels else { panic!("readback must deliver CPU pixels") };
        assert!(!data.is_empty());
        assert_eq!(*stride, f.width * 4);
        assert_eq!(data.len(), (*stride as usize) * f.height as usize);
        let at = |x: i32, y: i32| {
            let o = y as usize * *stride as usize + x as usize * 4;
            &data[o..o + 4]
        };
        for (dx, dy) in [(0, 0), (-50, -50), (50, 50), (-50, 50), (50, -50)] {
            let px = at(cx + dx, cy + dy);
            assert!(is_flicker_pixel(px), "pixel at flicker offset ({dx},{dy}) is {px:?}");
        }
        // Simple statistics: the image is not one flat colour.
        assert!(data.as_chunks::<4>().0.iter().any(|px| px[..] != data[0..4]), "image is a single colour");
    }
    // The GPU path read back through `GpuFrame::read_back` agrees at the same point.
    drop(screen);
    let (mut gpu_screen, _) = open_primary(CaptureOptions::default(), 60);
    let gframes = grab(&mut gpu_screen, 3, Duration::from_secs(5));
    assert_eq!(gframes.len(), 3);
    for f in &gframes {
        let img = gpu_payload(f).expect("gpu frame").read_back().expect("read back");
        assert_eq!((img.width, img.height, img.stride), (f.width, f.height, f.width * 4));
        let o = cy as usize * img.stride as usize + cx as usize * 4;
        assert!(is_flicker_pixel(&img.data[o..o + 4]));
    }
}

#[test]
fn forced_access_lost_then_reacquire_resumes_with_monotonic_timestamps() {
    aware();
    let _lock = serial();
    let p = primary();
    let _flicker = Flicker::start(&p);
    let mut screen = DxgiScreen::new();
    // Before open there is nothing to reacquire.
    assert_eq!(screen.reacquire().expect_err("never opened").kind, ErrorKind::InvalidInput);
    screen.open(p.id, CaptureConfig { max_fps: 60 }).expect("open");
    let mut all = grab_stamps(&mut screen, 60, Duration::from_secs(5));
    assert_eq!(all.len(), 60);
    let devices = screen.device_pool().len();
    assert_eq!(devices, 1, "one device per adapter that owns the open output");

    screen.simulate_access_lost();
    for _ in 0..2 {
        let e = screen.next_frame(Duration::from_millis(100)).err().expect("lost until reacquire");
        assert_eq!(e.kind, ErrorKind::AccessLost);
    }
    let t0 = Instant::now();
    screen.reacquire().expect("reacquire after AccessLost");
    let reacquire_took = t0.elapsed();
    let first = loop {
        if let Some(f) = screen.next_frame(Duration::from_millis(500)).expect("next_frame") {
            break f.captured_at;
        }
        assert!(t0.elapsed() < Duration::from_secs(5), "no frame after reacquire");
    };
    println!("reacquire call {reacquire_took:?}; first frame {:?} after the call began", t0.elapsed());
    all.push(first);
    let mut after = grab_stamps(&mut screen, 59, Duration::from_secs(5));
    assert_eq!(after.len(), 59, "capture resumes after reacquire");
    all.append(&mut after);
    assert_stamps_non_decreasing(&all);
    assert_eq!(screen.device_pool().len(), devices, "the device is reused on the same adapter");

    // A reacquire on a healthy duplication (a desktop-switch notice that turned out harmless) also works.
    screen.reacquire().expect("reacquire while healthy");
    let mut again = grab_stamps(&mut screen, 30, Duration::from_secs(5));
    assert_eq!(again.len(), 30);
    all.append(&mut again);
    assert_stamps_non_decreasing(&all);
    let s = screen.stats();
    assert_eq!(s.reacquires, 2);
    assert!(s.textures_created <= 2, "{s:?}");
    println!("stats: {s:?}");

    // Closed sources say so; a later open works again.
    screen.close();
    assert!(screen.next_frame(Duration::from_millis(10)).is_err());
    screen.open(p.id, CaptureConfig { max_fps: 60 }).expect("open again");
    assert!(!grab(&mut screen, 5, Duration::from_secs(5)).is_empty());
}

#[test]
fn opening_an_unknown_display_is_refused_and_other_threads_are_refused() {
    aware();
    let _lock = serial();
    let mut screen = DxgiScreen::new();
    let e = screen.open(DisplayId(999), CaptureConfig { max_fps: 30 }).expect_err("no such display");
    assert_eq!(e.kind, ErrorKind::InvalidInput);
    // The source is bound to the thread that built it (SetThreadDesktop binds a thread).
    let p = primary();
    let mut screen = DxgiScreen::new();
    screen.open(p.id, CaptureConfig { max_fps: 30 }).expect("open");
    let moved = std::thread::scope(|s| {
        // The type is not `Send`: reach it only through a raw address to prove the runtime check, never to use it.
        let addr = &mut screen as *mut DxgiScreen as usize;
        s.spawn(move || {
            // SAFETY: the owning thread is blocked in `scope` until this closure ends, so there is no concurrent use;
            // the call is refused before touching any thread state.
            let other = unsafe { &mut *(addr as *mut DxgiScreen) };
            other.next_frame(Duration::from_millis(10)).err().map(|e| e.kind)
        })
        .join()
        .expect("thread")
    });
    assert_eq!(moved, Some(ErrorKind::InvalidInput));
}

#[test]
fn frames_are_paced_to_max_fps() {
    aware();
    let _lock = serial();
    let p = primary();
    let _flicker = Flicker::start(&p);
    let (mut screen, _) = open_primary(CaptureOptions::default(), 30);
    let started = Instant::now();
    let frames = grab(&mut screen, 60, Duration::from_secs(10));
    let secs = started.elapsed().as_secs_f64();
    assert_eq!(frames.len(), 60);
    let fps = 60.0 / secs;
    println!("max_fps 30 delivered {fps:.1} fps");
    assert!((24.0..=33.0).contains(&fps), "{fps:.1} fps at max_fps 30");
}

struct RestoreCursor(POINT);

impl Drop for RestoreCursor {
    fn drop(&mut self) {
        // SAFETY: plain call; puts the pointer back where the owner left it.
        let _ = unsafe { SetCursorPos(self.0.x, self.0.y) };
    }
}

#[test]
#[ignore = "interactive: injects real mouse movement; run on an idle PC with --run-ignored only"]
fn cursor_channel_reports_shape_and_hot_spot_position() {
    aware();
    let _lock = serial();
    let p = primary();
    let flicker = Flicker::start(&p);
    let (mut screen, p) = open_primary(CaptureOptions::default(), 60);
    let mut original = POINT::default();
    // SAFETY: valid out pointer.
    unsafe { GetCursorPos(&mut original) }.expect("cursor position");
    let _restore = RestoreCursor(original);
    let (lx, ly) = flicker.center_local(&p);
    let (px, py) = (p.bounds.x + lx, p.bounds.y + ly);
    // SAFETY: moves the pointer onto the test window; restored on drop.
    unsafe { SetCursorPos(px, py) }.expect("set cursor");
    let mut got_shape: Option<swapdesk_platform::CursorShape> = None;
    let mut got_at: Option<swapdesk_platform::CursorUpdate> = None;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(4)
        && !(got_at.is_some() && got_shape.as_ref().is_some_and(|s| s.hot_x > 0))
    {
        let _ = screen.next_frame(Duration::from_millis(50)).expect("next_frame");
        if let Some(u) = screen.take_cursor() {
            assert_eq!(u.display, p.id);
            if let Some(s) = u.shape.clone() {
                got_shape = Some(s);
            }
            if u.visible && (u.x, u.y) == (lx, ly) {
                got_at = Some(u);
            } else if u.visible {
                println!("update at ({},{}) while expecting ({lx},{ly})", u.x, u.y);
            }
        }
    }
    let at = got_at.expect("a cursor update at the hot spot position");
    assert!(at.visible);
    let shape = got_shape.expect("a shape came with the first updates");
    println!(
        "cursor at ({},{}) shape id={} {}x{} hot=({},{})",
        at.x, at.y, shape.id, shape.width, shape.height, shape.hot_x, shape.hot_y
    );
    assert!(shape.width > 0 && shape.height > 0 && shape.width <= 256 && shape.height <= 256);
    assert_eq!(shape.rgba.len(), shape.width as usize * shape.height as usize * 4);
    assert!(shape.hot_x < shape.width && shape.hot_y < shape.height);
    assert!(shape.rgba.as_chunks::<4>().0.iter().any(|px| px[3] != 0), "the shape has visible pixels");
    // Nothing pending after the read.
    assert!(screen.take_cursor().is_none() || screen.take_cursor().is_none());
}

#[test]
fn watcher_delivers_display_change_and_stops_cleanly() {
    aware();
    let watcher = DisplayWatcher::start().expect("watcher");
    assert!(watcher.try_recv().is_none());
    watcher.simulate_display_change();
    assert_eq!(watcher.recv_timeout(Duration::from_secs(2)), Some(WatchEvent::DisplayChange));
    // A thread of its own: the test thread owns no window or hook and can follow the input desktop.
    drop(watcher);
    let watcher = DisplayWatcher::start().expect("a second watcher after the first stopped");
    drop(watcher);
}

#[test]
fn poll_display_changes_sees_no_change_on_a_quiet_machine() {
    aware();
    let mut screen = DxgiScreen::new();
    let list = screen.displays().expect("displays");
    assert!(!list.is_empty());
    assert!(screen.poll_display_changes().expect("poll").is_empty());
}
