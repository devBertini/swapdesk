//! The viewer media path on the MacBook, with no window: `ViewerSink::video` -> mailbox -> decode thread (VideoToolbox) ->
//! `Handoff` -> `MainSide` (the renderer) on a bare `CALayer`, which the test thread drains by hand where the app's
//! main queue would. What is proved: the layer tree, the counters, the feedback to the host and the pointer map; what
//! lands on a screen is not (the launch check and the owner's eyes). Run with `scripts/mac/remote.sh test -p swapdesk-app-mac`.
//!
//! The streams are the codec lane's fixtures: A is 160x96 (IDR at 0 and 4, 8 frames), B is 128x90 (the same shape).

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::CALayer;
use swapdesk_app::shared::mailbox::Cmd;
use swapdesk_app::shared::out::ViewerOut;
use swapdesk_codec_mac::annexb::{self, access_units};
use swapdesk_platform::{NativeSurface, Rect, ScaleMode};
use swapdesk_proto::{Codec, InputEvent, KeyframeRequest, ReceiverReport, VideoHeader};
use swapdesk_ui::service_backend::ViewerSink;

use crate::main_side::{MainSide, PresentEvent};
use crate::pipeline::{MacViewer, ViewerOptions};

const STREAM_A: &[u8] = include_bytes!("../../swapdesk-codec-mac/tests/fixtures/a_160x96.h264");
const STREAM_B: &[u8] = include_bytes!("../../swapdesk-codec-mac/tests/fixtures/b_128x90.h264");

const SESSION: u64 = 7;
const DISPLAY: u16 = 3;
/// How long a step may take. Alone these tests decode their first four frames in 1.6 s (2.5 s with the setup); in the
/// whole Mac suite, while macOS assesses every freshly built test binary (syspolicyd near 100% CPU), the same step
/// took over 10 s even with the media tests one at a time (11/11/2026). Nothing in the product waits on this bound.
const WAIT: Duration = Duration::from_secs(30);

fn units(stream: &'static [u8]) -> Vec<&'static [u8]> {
    access_units(stream).into_iter().map(|r| &stream[r]).collect()
}

fn header(frame_id: u32, unit: &[u8], size: (u16, u16)) -> VideoHeader {
    VideoHeader {
        keyframe: annexb::is_idr(unit),
        config: annexb::parameter_sets(unit).is_some(),
        discontinuity: false,
        display_id: DISPLAY,
        frame_id,
        capture_us: 1_000_000 + u64::from(frame_id) * 33_333,
        encode_us: 100,
        width: size.0,
        height: size.1,
        codec: Codec::H264,
        payload_len: unit.len() as u32,
    }
}

/// Where the pipeline's feedback and input end up.
#[derive(Default)]
pub(crate) struct Recorder {
    pub keyframes: Mutex<Vec<(u64, KeyframeRequest)>>,
    pub inputs: Mutex<Vec<(u64, Vec<InputEvent>)>>,
    pub reports: Mutex<Vec<(u64, ReceiverReport)>>,
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

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

/// The pixel area of the video: 800x600 pixels at 2 pixels per point.
const AREA: Rect = Rect { x: 0, y: 0, width: 800, height: 600 };

struct Rig {
    viewer: Arc<MacViewer>,
    rec: Arc<Recorder>,
    parent: Retained<CALayer>,
    side: Option<MainSide>,
    presents: Arc<Mutex<Vec<PresentEvent>>>,
}

impl Rig {
    fn new() -> Self {
        let presents: Arc<Mutex<Vec<PresentEvent>>> = Arc::default();
        let log = Arc::clone(&presents);
        let viewer = MacViewer::new(ViewerOptions {
            on_present: Some(Arc::new(move |e| log.lock().unwrap().push(*e))),
            audio: false,
            system_keys: false,
            // The test thread plays the main thread: it drains by hand, nothing is queued on the main dispatch queue.
            wake: Box::new(|| {}),
        })
        .expect("the pipeline starts");
        let rec = Arc::new(Recorder::default());
        viewer.set_out(rec.clone());
        let parent = CALayer::new();
        parent.setFrame(rect(0.0, 0.0, 400.0, 300.0));
        Self { viewer, rec, parent, side: None, presents }
    }

    /// What `MacVideoSlot::attached` does, on a bare layer.
    fn attach(&mut self) {
        let parent_layer = std::ptr::from_ref::<CALayer>(&self.parent) as usize as u64;
        let side = MainSide::attach(
            parent_layer,
            None,
            AREA,
            2.0,
            ScaleMode::Fit,
            Arc::clone(&self.viewer.shared),
            Arc::clone(&self.viewer.layout),
            Arc::clone(&self.viewer.handoff),
            self.viewer.observer.clone(),
        )
        .expect("the renderer attaches to a bare layer");
        self.side = Some(side);
        self.viewer.mailbox.push_cmd(Cmd::Attach {
            surface: NativeSurface::MacOs { parent_layer },
            area: AREA,
            scale: ScaleMode::Fit,
        });
    }

    fn push(&self, stream: &'static [u8], first_id: u32, size: (u16, u16), which: impl Fn(usize) -> bool) {
        for (i, unit) in units(stream).into_iter().enumerate() {
            if which(i) {
                self.viewer.video(SESSION, header(first_id + i as u32, unit, size), unit.to_vec());
            }
        }
    }

    /// Drains the hand-over as the main queue would, until `done` or the wait ends.
    fn pump(&mut self, what: &str, done: impl Fn(&Rig) -> bool) {
        let end = Instant::now() + WAIT;
        loop {
            if let Some(side) = self.side.as_mut() {
                side.drain();
            }
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < end,
                "timed out waiting for {what}: stats {:?}, last error {:?}",
                self.viewer.stats(),
                self.viewer.last_error()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn sublayers(&self) -> usize {
        // SAFETY: a plain read of the layer's sublayer array.
        unsafe { self.parent.sublayers() }.map_or(0, |a| a.count())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // The renderer goes before the pipeline, as the window's close does.
        self.side = None;
        self.viewer.shutdown();
    }
}

#[test]
fn a_stream_goes_from_the_sink_to_the_layer_and_the_pointer_map_follows() {
    let mut rig = Rig::new();
    rig.attach();
    assert_eq!(rig.sublayers(), 1, "the video container is in the parent layer");
    // A keyframe empties the queue of what the decoder has not taken yet (nothing before it is ever shown), so each group
    // of frames waits until the decoder has taken it.
    rig.push(STREAM_A, 0, (160, 96), |i| i < 4);
    rig.pump("stream A, first group", |r| r.viewer.stats().decoded == 4 && r.viewer.stats().last_frame_id == Some(3));
    rig.push(STREAM_A, 0, (160, 96), |i| i >= 4);
    rig.pump("stream A on the layer", |r| r.viewer.stats().decoded == 8 && r.viewer.stats().last_frame_id == Some(7));

    let s = rig.viewer.stats();
    assert_eq!(s.decode_errors, 0, "{:?}", rig.viewer.last_error());
    assert_eq!(s.keyframe_requests, 0, "a stream that starts with a keyframe asks for none");
    assert!(s.attached && s.presented >= 1 && s.presented + s.lost >= 8, "{s:?}");
    {
        let presents = rig.presents.lock().unwrap();
        let last = presents.last().expect("a picture was presented");
        assert_eq!((last.frame_id, last.display, last.width, last.height), (7, DISPLAY, 160, 96));
    }

    // 160x96 fitted into 800x600 pixels: 5x, 800x480, 60 pixels of letterbox above and below.
    let layout = rig.viewer.layout();
    assert_eq!(layout.get().expect("published").frame, (160, 96));
    assert_eq!(layout.map(0, 60), Some((0, 0)), "the picture's top-left corner");
    assert_eq!(layout.map(799, 539), Some((65535, 65535)), "the picture's bottom-right corner");
    assert_eq!(layout.map(400, 30), None, "letterbox above the picture");
    assert_eq!(layout.map(800, 300), None, "outside the area");
    let centre = layout.map(400, 300).expect("the centre maps");
    assert!(centre.0.abs_diff(32768) < 400 && centre.1.abs_diff(32768) < 700, "{centre:?}");

    // The picture changes size (the host changed its display mode): the map follows the new picture.
    rig.push(STREAM_B, 8, (128, 90), |i| i < 4);
    rig.pump("stream B, first group", |r| r.viewer.stats().decoded == 12 && r.viewer.stats().last_frame_id == Some(11));
    rig.push(STREAM_B, 8, (128, 90), |i| i >= 4);
    rig.pump("stream B on the layer", |r| r.viewer.stats().decoded == 16 && r.viewer.stats().last_frame_id == Some(15));
    assert_eq!(layout.get().expect("published").frame, (128, 90));
    assert!(layout.map(400, 30).is_some(), "B is 800x563 in the same area: 18 pixels of letterbox");
    assert_eq!(rig.viewer.stats().decode_errors, 0, "{:?}", rig.viewer.last_error());

    // Fit becomes 1:1 (the shell's scale command): the layout is published again.
    let before = layout.get().expect("published").layout;
    rig.side.as_mut().expect("attached").set_scale(ScaleMode::OneToOne);
    assert_ne!(layout.get().expect("published").layout, before, "1:1 moves the picture");

    // Closing the window: the layers go, the pointer map is forgotten and the decoder is released.
    rig.side = None;
    assert_eq!(layout.map(400, 300), None, "no picture, no pointer position");
    assert_eq!(rig.sublayers(), 0, "the container left the parent layer");
    let (tx, rx) = std::sync::mpsc::channel();
    rig.viewer.mailbox.push_cmd(Cmd::Detach(tx));
    rx.recv_timeout(WAIT).expect("the decode thread acknowledges the detach");
    assert!(!rig.viewer.stats().attached);
}

#[test]
fn a_stream_that_starts_between_keyframes_asks_the_host_for_one_and_draws_nothing_until_it_comes() {
    let mut rig = Rig::new();
    rig.attach();
    // The first thing that arrives is a delta frame (the viewer joined mid-stream).
    rig.push(STREAM_A, 0, (160, 96), |i| i == 1);
    let end = Instant::now() + WAIT;
    while rig.rec.keyframes.lock().unwrap().is_empty() {
        assert!(Instant::now() < end, "no keyframe request: {:?}", rig.viewer.stats());
        std::thread::sleep(Duration::from_millis(5));
    }
    {
        let asked = rig.rec.keyframes.lock().unwrap();
        assert_eq!(asked[0].0, SESSION, "the request goes to the session the frame came from");
    }
    rig.side.as_mut().expect("attached").drain();
    let s = rig.viewer.stats();
    assert_eq!((s.decoded, s.presented), (0, 0), "nothing can be drawn without a keyframe");
    assert!(s.keyframe_requests >= 1);

    // The keyframe the host answers with (stream A's unit 0 and the deltas after it) starts the picture.
    rig.push(STREAM_A, 10, (160, 96), |i| i < 4);
    rig.pump("the picture after the keyframe", |r| {
        r.viewer.stats().decoded == 4 && r.viewer.stats().last_frame_id == Some(13)
    });
    assert_eq!(rig.viewer.stats().decode_errors, 0, "{:?}", rig.viewer.last_error());
}

#[test]
fn frames_before_the_window_exists_are_dropped_and_the_attach_asks_for_a_keyframe() {
    let mut rig = Rig::new();
    rig.push(STREAM_A, 0, (160, 96), |i| i < 3);
    // The decode thread takes them within milliseconds (it learns the session from them) but has no decoder.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(rig.viewer.stats().decoded, 0, "no window, no decoder");
    assert!(rig.rec.keyframes.lock().unwrap().is_empty());

    rig.attach();
    let end = Instant::now() + WAIT;
    while rig.rec.keyframes.lock().unwrap().is_empty() {
        assert!(Instant::now() < end, "the attach did not ask for a keyframe: {:?}", rig.viewer.stats());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(rig.viewer.shared.attached.load(Ordering::SeqCst));
    rig.push(STREAM_A, 3, (160, 96), |i| i == 4);
    rig.pump("the first picture after the attach", |r| r.viewer.stats().decoded == 1);
}
