//! The viewer pipeline end to end, without a window and without a service: a real Media Foundation encode of a
//! `SyntheticScreen` sequence goes in as `ViewerVideo` (through the `ViewerSink` the pipe's reader thread calls), the
//! decode thread decodes it on the D3D11 device of a headless shell surface (a composition tree with no window), the
//! renderer presents into it, and the feedback and the pointer mapping are checked on what comes out.
#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_app::viewer::{PresentEvent, ViewerOptions, ViewerOut, ViewerPipeline};
use swapdesk_codec_win::{EncoderOptions, MfH264Encoder, MftChoice, TextureUploader};
use swapdesk_fakes::{FakeClock, SyntheticScreen};
use swapdesk_platform::{
    AdapterId, CaptureConfig, DisplayId, EncodedFrame, EncoderConfig, MonoMicros, Pixels, Rect, ScreenSource,
    VideoEncoder,
};
use swapdesk_proto::{Codec, InputEvent, KeyframeRequest, Preset, ReceiverReport, VideoHeader};
use swapdesk_shell_win::{HeadlessSurface, MouseKind, MouseMsg, ViewerHost};
use swapdesk_ui::service_backend::ViewerSink;
use swapdesk_ui::{Permission, Permissions, Role, SessionState, UiEvent};

const SESSION: u64 = 7;
const SIZE: (u32, u32) = (640, 360);
const FPS: u16 = 30;

/// What the pipeline sent to the service.
#[derive(Default)]
struct Recorder {
    inputs: Mutex<Vec<(u64, Vec<InputEvent>)>>,
    keyframes: Mutex<Vec<(u64, KeyframeRequest)>>,
    reports: Mutex<Vec<(u64, ReceiverReport)>>,
}

impl ViewerOut for Recorder {
    fn input(&self, session_id: u64, events: Vec<InputEvent>) {
        self.inputs.lock().unwrap().push((session_id, events));
    }
    fn keyframe(&self, session_id: u64, request: KeyframeRequest) {
        self.keyframes.lock().unwrap().push((session_id, request));
    }
    fn report(&self, session_id: u64, report: ReceiverReport) {
        self.reports.lock().unwrap().push((session_id, report));
    }
}

/// `count` encoded frames of the synthetic screen (a moving gradient with a frame number), the first a keyframe.
fn encode(count: u32) -> Vec<EncodedFrame> {
    encode_with(count, &[])
}

/// As [`encode`], with a keyframe asked for at each input frame of `keyframes_at`.
fn encode_with(count: u32, keyframes_at: &[u32]) -> Vec<EncodedFrame> {
    let mut screen = SyntheticScreen::new(SIZE.0, SIZE.1, FPS).with_clock(Arc::new(FakeClock::new(MonoMicros(0))));
    screen.open(DisplayId(0), CaptureConfig { max_fps: FPS }).expect("open the synthetic screen");
    let uploader = TextureUploader::new(AdapterId(0)).expect("uploader");
    let mut enc = MfH264Encoder::new(EncoderOptions {
        choice: MftChoice::Auto,
        device: Some(uploader.device()),
        ..Default::default()
    });
    enc.configure(&EncoderConfig {
        width: SIZE.0,
        height: SIZE.1,
        fps: FPS,
        kbps: 4000,
        preset: Preset::Auto,
        adapter: AdapterId(0),
        codec: swapdesk_platform::Codec::H264,
    })
    .expect("configure");
    let mut out = Vec::new();
    for n in 0..count {
        let frame = screen.next_frame(Duration::from_secs(5)).expect("frame").expect("a frame");
        assert!(matches!(frame.pixels, Pixels::Cpu { .. }));
        let gpu = uploader.upload(&frame).expect("upload");
        out.extend(enc.encode(&gpu, keyframes_at.contains(&n)).expect("encode"));
    }
    out.extend(enc.flush().expect("flush"));
    out
}

fn header(n: u32, e: &EncodedFrame) -> VideoHeader {
    VideoHeader {
        keyframe: e.keyframe,
        config: e.config_change,
        discontinuity: false,
        display_id: 0,
        frame_id: n,
        capture_us: u64::from(n) * 33_333,
        encode_us: e.encode_us,
        width: e.width,
        height: e.height,
        codec: Codec::H264,
        payload_len: e.annex_b.len() as u32,
    }
}

fn wait_until(what: &str, limit: Duration, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Rig {
    // The surface goes last: the pipeline detaches from it first.
    pipeline: Arc<ViewerPipeline>,
    recorder: Arc<Recorder>,
    presented: Arc<Mutex<Vec<PresentEvent>>>,
    _surface: HeadlessSurface,
}

fn rig() -> Rig {
    rig_with(None)
}

/// `present_delay`: every present takes at least this long (a desktop that holds a window's pictures back).
fn rig_with(present_delay: Option<Duration>) -> Rig {
    let surface = HeadlessSurface::new().expect("headless shell surface");
    let presented = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&presented);
    let pipeline = ViewerPipeline::new(ViewerOptions {
        on_present: Some(Arc::new(move |e| seen.lock().unwrap().push(*e))),
        audio: false,
        probe: None,
        present_delay,
    })
    .expect("pipeline");
    let recorder = Arc::new(Recorder::default());
    pipeline.set_out(recorder.clone());
    pipeline.opened(surface.native(), Rect { x: 0, y: 0, width: SIZE.0, height: SIZE.1 }, 1.0);
    wait_until("the renderer to attach", Duration::from_secs(10), || pipeline.stats().attached);
    Rig { pipeline, recorder, presented, _surface: surface }
}

/// Feeds the stream's keyframe and waits for its picture. The first decode builds the decoder, which takes many
/// frame times on a PC whose GPU is busy with other tests: a stream that kept coming meanwhile would overflow the
/// mailbox, which then asks the host for a new keyframe, and no host answers in a test. A session starts the same
/// way: the host sends the keyframe, then what follows it.
fn first_picture(r: &Rig, frames: &[EncodedFrame]) {
    let e = &frames[0];
    assert!(e.keyframe, "the stream starts with a keyframe");
    r.pipeline.video(SESSION, header(0, e), e.annex_b.clone());
    wait_until("the keyframe's picture", Duration::from_secs(20), || !r.presented.lock().unwrap().is_empty());
}

fn control_granted() -> UiEvent {
    UiEvent::Session {
        session_id: SESSION,
        device_id: "mac".into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::Control),
            elevation: Default::default(),
        },
    }
}

#[test]
fn encoded_frames_are_decoded_and_presented_with_increasing_ids_and_the_pointer_maps_on_them() {
    let frames = encode(40);
    let r = rig();
    first_picture(&r, &frames);
    for (n, e) in frames.iter().enumerate().skip(1) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    let total = frames.len();
    // The last picture, not a count: under load the decoder can hand out two pictures at once and the viewer shows
    // only the newer, by design (measured in the full suite on a loaded PC: fewer than 36 of 40 in 10 s).
    let last = u32::try_from(total - 1).unwrap();
    wait_until("the last frame to be presented", Duration::from_secs(10), || {
        r.presented.lock().unwrap().last().is_some_and(|e| e.frame_id == last)
    });

    let ids: Vec<u32> = r.presented.lock().unwrap().iter().map(|e| e.frame_id).collect();
    assert!(ids.windows(2).all(|w| w[1] > w[0]), "frame ids must increase: {ids:?}");
    assert!(ids.len() * 2 >= total, "at least half the pictures reach the screen: {} of {total}", ids.len());
    assert_eq!(ids[0], 0, "the first presented picture is the keyframe");
    assert!(r.presented.lock().unwrap().iter().all(|e| (e.width, e.height) == SIZE));
    assert_eq!(r.pipeline.last_error(), None);
    let stats = r.pipeline.stats();
    assert_eq!(stats.presented as usize, ids.len());
    assert_eq!(stats.presented + stats.skipped, total as u64, "every frame was decoded: {stats:?}");
    assert_eq!(stats.lost, 0);
    assert_eq!(stats.decode_errors, 0);

    // Pointer: with Control granted, the middle of the window maps to the middle of the 640x360 picture.
    assert!(
        !r.pipeline.mouse(MouseMsg { kind: MouseKind::Move, x: 10, y: 10, inside: true }),
        "no grants yet: nothing is sent"
    );
    r.pipeline.observe_event(&control_granted());
    assert!(r.pipeline.mouse(MouseMsg { kind: MouseKind::Move, x: 320, y: 180, inside: true }));
    wait_until("the pointer record", Duration::from_secs(5), || !r.recorder.inputs.lock().unwrap().is_empty());
    let inputs = r.recorder.inputs.lock().unwrap();
    match inputs[0].1.as_slice() {
        [InputEvent::Pointer { display: 0, x, y }] => {
            assert_eq!(inputs[0].0, SESSION);
            assert!((i32::from(*x) - 32768).abs() < 130 && (i32::from(*y) - 32768).abs() < 130, "{x} {y}");
        }
        other => panic!("expected one pointer record, got {other:?}"),
    }
}

/// A screen slower than the stream: each present takes three frame times and more, as on a desktop that holds back
/// the pictures of a window that is not in front (Windows: 30 a second for a 60 a second stream). Every frame is still decoded (each depends on the one
/// before), the pictures that are old when they come out are not shown, the newest is, and the host is asked for
/// nothing: no frame is dropped on the way in, the decoder is never reset, no keyframe is requested.
#[test]
fn a_present_slower_than_the_stream_shows_the_newest_picture_and_loses_nothing() {
    let frames = encode(90);
    // A frame every 33 ms against presents of 100 ms and more: the proportion is what matters, and these numbers
    // leave the decoder a whole frame time on a PC whose GPU is busy with other tests (at 16 ms a busy GPU alone
    // made the decoder fall behind, which is another matter: the mailbox then drops by design).
    let r = rig_with(Some(Duration::from_millis(100)));
    // The session is known before the frames come, so a keyframe request would be sent if one were made.
    r.pipeline.observe_event(&control_granted());
    first_picture(&r, &frames);
    for (n, e) in frames.iter().enumerate().skip(1) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    let last = u32::try_from(frames.len() - 1).unwrap();
    let t0 = Instant::now();
    while r.presented.lock().unwrap().last().is_none_or(|e| e.frame_id != last) {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the last frame was never presented: {:?}, last error {:?}",
            r.pipeline.stats(),
            r.pipeline.last_error()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let stats = r.pipeline.stats();
    assert_eq!(stats.lost, 0, "no frame was dropped before the decoder: {stats:?}");
    assert_eq!(stats.decode_errors, 0, "{stats:?}");
    assert_eq!(stats.keyframe_requests, 0, "the host is asked for nothing: {stats:?}");
    assert!(r.recorder.keyframes.lock().unwrap().is_empty());
    assert_eq!(r.pipeline.last_error(), None);
    // Every frame was decoded: each one is either on the screen or was skipped for a newer one.
    assert_eq!(stats.presented + stats.skipped, frames.len() as u64, "{stats:?}");
    assert!(stats.skipped > 0, "the screen was slower than the stream, so pictures were skipped: {stats:?}");
    let ids: Vec<u32> = r.presented.lock().unwrap().iter().map(|e| e.frame_id).collect();
    assert!(ids.windows(2).all(|w| w[1] > w[0]), "frame ids must increase: {ids:?}");
    // What is shown is never far behind: between two pictures on the screen at most the bound of skips.
    assert!(ids.windows(2).all(|w| w[1] - w[0] <= 6), "a gap longer than the bound: {ids:?}");
}

#[test]
fn a_hole_in_the_frame_ids_asks_for_a_keyframe_and_nothing_wrong_is_shown() {
    let frames = encode(30);
    let r = rig();
    first_picture(&r, &frames);
    for (n, e) in frames.iter().enumerate().take(10).skip(1) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    // The picture of frame 9, the last before the hole. Not a count of pictures: under GPU load the decoder can hand
    // two pictures out at once and the viewer shows only the newer, by design.
    wait_until("the picture of frame 9", Duration::from_secs(10), || {
        r.presented.lock().unwrap().last().is_some_and(|e| e.frame_id == 9)
    });
    // Frame 10 is lost on the way: 11 and the rest are P-frames the decoder cannot use.
    for (n, e) in frames.iter().enumerate().skip(11) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(10));
    }
    wait_until("the keyframe request", Duration::from_secs(5), || !r.recorder.keyframes.lock().unwrap().is_empty());
    let (session, request) = r.recorder.keyframes.lock().unwrap()[0].clone();
    assert_eq!(session, SESSION);
    assert_eq!(request.display, 0);
    let last_shown = r.presented.lock().unwrap().last().map(|e| e.frame_id).expect("pictures were shown");
    assert_eq!(request.last_good_frame, last_shown, "the request names the last picture that was good");
    assert!(last_shown <= 9, "no picture from after the hole was shown (last {last_shown})");
}

#[test]
fn the_report_goes_out_once_a_second_with_a_decode_time() {
    let frames = encode(45);
    let r = rig();
    first_picture(&r, &frames);
    for (n, e) in frames.iter().enumerate().skip(1) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    wait_until("a report", Duration::from_secs(5), || !r.recorder.reports.lock().unwrap().is_empty());
    let (session, report) = r.recorder.reports.lock().unwrap()[0].clone();
    assert_eq!(session, SESSION);
    assert!(report.frames_presented > 0, "{report:?}");
    assert!(report.decode_us_p50 > 0, "{report:?}");
    assert!(report.present_lag_us_p50 > 0, "{report:?}");
}

/// F-VIEW-08: the graphics device the viewer draws on is removed in the middle of a stream (a driver reset, the
/// adapter gone). The pipeline sees the failed present, finds that the shell's device itself is gone, stops drawing and
/// tells the shell, which makes a new device and hands a new surface; the renderer and the decoder are built on it, the
/// host is asked for a keyframe, and the picture comes back. Nothing ends the session: no disconnect is asked for.
#[test]
fn a_lost_graphics_device_is_built_again_and_the_picture_returns_without_ending_the_session() {
    let frames = encode_with(60, &[40]);
    let again =
        frames.iter().enumerate().skip(30).find(|(_, e)| e.keyframe).map(|(n, _)| n).expect("a second keyframe");
    let r = rig();
    r.pipeline.observe_event(&control_granted());
    first_picture(&r, &frames);
    for (n, e) in frames.iter().enumerate().take(20).skip(1) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    wait_until("the picture of frame 19", Duration::from_secs(10), || {
        r.presented.lock().unwrap().last().is_some_and(|e| e.frame_id == 19)
    });
    assert!(!r.pipeline.device_lost(), "nothing is lost yet");

    // The driver removes the shell's device.
    let swapdesk_platform::NativeSurface::Windows { d3d11_device, .. } = r._surface.native() else {
        panic!("a Windows surface")
    };
    swapdesk_render_win::simulate_device_removed(d3d11_device);
    for (n, e) in frames.iter().enumerate().take(again).skip(20) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(10));
    }
    // The pipeline stops drawing on the dead surface and tells the shell, once.
    wait_until("the device loss to reach the shell", Duration::from_secs(10), || r.pipeline.device_lost());
    assert!(!r.pipeline.device_lost(), "the shell is told once");
    assert!(!r.pipeline.stats().attached, "nothing stays built on the dead device");
    let shown_before = r.presented.lock().unwrap().last().map(|e| e.frame_id).expect("pictures were shown");
    assert!(shown_before < 20, "no picture is presented on the removed device (last {shown_before})");

    // The shell makes a new device and surface and hands it over, as `Shared::recover_device` does.
    let fresh = HeadlessSurface::new().expect("a new device and surface");
    r.pipeline.closed();
    r.pipeline.opened(fresh.native(), Rect { x: 0, y: 0, width: SIZE.0, height: SIZE.1 }, 1.0);
    wait_until("the renderer to attach on the new device", Duration::from_secs(10), || r.pipeline.stats().attached);
    wait_until("the keyframe request", Duration::from_secs(5), || !r.recorder.keyframes.lock().unwrap().is_empty());
    assert_eq!(r.recorder.keyframes.lock().unwrap()[0].0, SESSION);

    // The host answers with a keyframe, then the stream goes on. As at the start of a session ([`first_picture`]), the
    // keyframe goes alone and the test waits for its picture: the new decoder is built by its first decode, which
    // takes many frame times on a loaded PC, and a stream that kept coming meanwhile is by design shown newest-first
    // (`show_newest` skips a picture while the stream is two frames ahead), so the keyframe's own picture would never
    // reach the screen and the test would not see "the picture came back with the keyframe".
    let key = &frames[again];
    assert!(key.keyframe, "the stream's second keyframe");
    r.pipeline.video(SESSION, header(again as u32, key), key.annex_b.clone());
    wait_until("the keyframe's picture on the new device", Duration::from_secs(20), || {
        r.presented.lock().unwrap().last().is_some_and(|e| e.frame_id == again as u32)
    });
    for (n, e) in frames.iter().enumerate().skip(again + 1) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    let last = u32::try_from(frames.len() - 1).unwrap();
    wait_until("the last frame on the new device", Duration::from_secs(10), || {
        r.presented.lock().unwrap().last().is_some_and(|e| e.frame_id == last)
    });
    let ids: Vec<u32> = r.presented.lock().unwrap().iter().map(|e| e.frame_id).collect();
    assert!(ids.windows(2).all(|w| w[1] > w[0]), "frame ids must increase: {ids:?}");
    assert!(ids.contains(&(again as u32)), "the picture came back with the keyframe: {ids:?}");
    assert!(!r.pipeline.device_lost());
    assert!(r.pipeline.stats().attached);
    // The pipeline detaches before the surface it attached to goes away.
    r.pipeline.closed();
    drop(fresh);
}
