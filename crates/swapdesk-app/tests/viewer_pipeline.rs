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
    })
    .expect("configure");
    let mut out = Vec::new();
    for _ in 0..count {
        let frame = screen.next_frame(Duration::from_secs(5)).expect("frame").expect("a frame");
        assert!(matches!(frame.pixels, Pixels::Cpu { .. }));
        let gpu = uploader.upload(&frame).expect("upload");
        out.extend(enc.encode(&gpu, false).expect("encode"));
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
    let surface = HeadlessSurface::new().expect("headless shell surface");
    let presented = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&presented);
    let pipeline = ViewerPipeline::new(ViewerOptions {
        on_present: Some(Arc::new(move |e| seen.lock().unwrap().push(*e))),
        audio: false,
    })
    .expect("pipeline");
    let recorder = Arc::new(Recorder::default());
    pipeline.set_out(recorder.clone());
    pipeline.opened(surface.native(), Rect { x: 0, y: 0, width: SIZE.0, height: SIZE.1 }, 1.0);
    wait_until("the renderer to attach", Duration::from_secs(10), || pipeline.stats().attached);
    Rig { pipeline, recorder, presented, _surface: surface }
}

fn control_granted() -> UiEvent {
    UiEvent::Session {
        session_id: SESSION,
        device_id: "mac".into(),
        state: SessionState::Active {
            since_ms: 0,
            role: Role::Viewer,
            granted: Permissions::NONE.with(Permission::Control),
        },
    }
}

#[test]
fn encoded_frames_are_decoded_and_presented_with_increasing_ids_and_the_pointer_maps_on_them() {
    let frames = encode(40);
    assert!(frames.first().is_some_and(|f| f.keyframe), "the stream starts with a keyframe");
    let r = rig();
    for (n, e) in frames.iter().enumerate() {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    let total = frames.len();
    wait_until("the frames to be presented", Duration::from_secs(10), || {
        r.presented.lock().unwrap().len() + 4 >= total
    });

    let ids: Vec<u32> = r.presented.lock().unwrap().iter().map(|e| e.frame_id).collect();
    assert!(ids.windows(2).all(|w| w[1] > w[0]), "frame ids must increase: {ids:?}");
    assert!(ids.len() >= total - 4, "presented {} of {total}", ids.len());
    assert_eq!(ids[0], 0, "the first presented picture is the keyframe");
    assert!(r.presented.lock().unwrap().iter().all(|e| (e.width, e.height) == SIZE));
    assert_eq!(r.pipeline.last_error(), None);
    let stats = r.pipeline.stats();
    assert_eq!(stats.presented as usize, ids.len());
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

#[test]
fn a_hole_in_the_frame_ids_asks_for_a_keyframe_and_nothing_wrong_is_shown() {
    let frames = encode(30);
    let r = rig();
    for (n, e) in frames.iter().enumerate().take(10) {
        r.pipeline.video(SESSION, header(n as u32, e), e.annex_b.clone());
        std::thread::sleep(Duration::from_millis(33));
    }
    wait_until("ten pictures", Duration::from_secs(10), || r.presented.lock().unwrap().len() >= 9);
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
    for (n, e) in frames.iter().enumerate() {
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
