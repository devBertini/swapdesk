//! A rate change the encoder cannot follow live must not end the video: the capture loop configures the encoder again
//! at the new rate (a keyframe restarts the stream) and logs it, and every encoder choice is noted for the service's
//! log at INFO (`HelperNote`). Before, `set_rate`'s error ended the capture thread and the viewer's picture froze.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_ipc::{Message, VideoBackend};
use swapdesk_platform::{
    CaptureConfig, Clock, CursorUpdate, DisplayId, DisplayInfo, EncodedFrame, EncoderConfig, EncoderInfo, ErrorKind,
    Frame, MonoMicros, Pixels, PlatformError, Result as PResult, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{H264Profile, Preset};

use super::{CaptureEnv, StartParams, VideoCtl, capture_loop};
use crate::outbox::Outbox;

struct RealClock(Instant);

impl Clock for RealClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(self.0.elapsed().as_micros() as u64)
    }
}

/// A frame every 20 ms.
struct SteadyScreen(Instant);

impl ScreenSource for SteadyScreen {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        Ok(Vec::new())
    }
    fn open(&mut self, _display: DisplayId, _config: CaptureConfig) -> PResult<()> {
        Ok(())
    }
    fn next_frame(&mut self, _timeout: Duration) -> PResult<Option<Frame>> {
        std::thread::sleep(Duration::from_millis(20));
        Ok(Some(Frame {
            display: DisplayId(0),
            captured_at: MonoMicros(self.0.elapsed().as_micros() as u64),
            width: 64,
            height: 48,
            pixels: Pixels::Cpu { stride: 256, data: vec![0; 256 * 48] },
            dirty: Vec::new(),
            content_changed: true,
        }))
    }
    fn reacquire(&mut self) -> PResult<()> {
        Ok(())
    }
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
    fn close(&mut self) {}
}

/// Refuses every live rate change, as an encoder whose session was dropped or whose MFT cannot follow one.
#[derive(Clone, Default)]
struct RefusingEncoder {
    configured: Arc<Mutex<Vec<(u32, u16)>>>,
    rate_calls: Arc<AtomicUsize>,
    keyframe_next: bool,
}

impl VideoEncoder for RefusingEncoder {
    fn configure(&mut self, config: &EncoderConfig) -> PResult<EncoderInfo> {
        self.configured.lock().unwrap().push((config.kbps, config.fps));
        self.keyframe_next = true;
        Ok(EncoderInfo {
            name: "refusing".into(),
            hardware: true,
            profile: H264Profile::High,
            level: 40,
            low_latency: true,
        })
    }
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        let keyframe = std::mem::take(&mut self.keyframe_next) || force_keyframe;
        Ok(vec![EncodedFrame {
            captured_at: frame.captured_at,
            encode_us: 5,
            keyframe,
            config_change: keyframe,
            width: 64,
            height: 48,
            annex_b: vec![7; 100],
        }])
    }
    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        self.rate_calls.fetch_add(1, Ordering::SeqCst);
        Err(PlatformError { kind: ErrorKind::Closed, op: "set_rate", os_code: None, detail: "session gone".into() })
    }
    fn refine(&mut self, _frame: &Frame) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

#[test]
fn a_rate_change_the_encoder_refuses_rebuilds_it_and_the_video_goes_on() {
    let (outbox, rx) = Outbox::new(1024);
    let enc = RefusingEncoder::default();
    let factory_enc = enc.clone();
    let env = CaptureEnv {
        screen: Arc::new(|_| Ok(SteadyScreen(Instant::now()))),
        encoder: Arc::new(move |_f, _c, _b| Ok(factory_enc.clone())),
        clock: Arc::new(RealClock(Instant::now())),
        outbox,
        displays: std::sync::Mutex::new(Vec::new()),
        input_tx: std::sync::Mutex::new(None),
        desktop_epoch: Arc::default(),
        fence: Arc::default(),
        desktop_probe: None,
        desktop_sent: Mutex::default(),
    };
    let p = StartParams {
        display: 0,
        fps: 60,
        kbps: 20_000,
        preset: Preset::Auto,
        backend: VideoBackend::Auto,
        codec: swapdesk_platform::Codec::H264,
    };
    let ctl = VideoCtl {
        stop: AtomicBool::new(false),
        force_key: AtomicBool::new(false),
        rate: Mutex::new(None),
        scale: Mutex::new((0, 0)),
    };
    let ids = AtomicU32::new(0);
    let result = std::thread::scope(|s| {
        let h = s.spawn(|| {
            let mut source = (env.screen)(DisplayId(0)).unwrap();
            source.open(DisplayId(0), CaptureConfig { max_fps: 60 }).unwrap();
            capture_loop(&env, p, &ctl, &ids, &mut source)
        });
        // Counted, not timed: a slow machine (a CI runner) gets the same number of frames on each side.
        let mut got = Vec::new();
        let frames_in = |got: &Vec<Message>| got.iter().filter(|m| matches!(m, Message::VideoFrame { .. })).count();
        let wait_for = |got: &mut Vec<Message>, n: usize| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while frames_in(got) < n && Instant::now() < deadline {
                got.extend(rx.recv_timeout(Duration::from_millis(50)));
            }
        };
        wait_for(&mut got, 10);
        *ctl.rate.lock().unwrap() = Some((21_000, 60));
        wait_for(&mut got, 20);
        let still_running = !h.is_finished();
        ctl.stop.store(true, Ordering::Release);
        let joined = h.join().unwrap();
        got.extend(rx.try_iter());
        (still_running, joined, got)
    });
    assert!(result.0, "the capture loop ended on the refused rate change");
    result.1.expect("the loop ends cleanly when stopped");
    assert_eq!(enc.rate_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*enc.configured.lock().unwrap(), vec![(20_000, 60), (21_000, 60)], "configured again at the new rate");

    let (mut frames, mut notes, mut faults) = (Vec::new(), Vec::new(), Vec::new());
    for m in result.2 {
        match m {
            Message::VideoFrame { header, .. } => frames.push(header),
            Message::HelperNote { what } => notes.push(what.0),
            Message::HelperFault { what, .. } => faults.push(what.0),
            _ => {}
        }
    }
    assert!(faults.is_empty(), "{faults:?}");
    // Frames before and after the change; the first after the rebuild is a keyframe that opens a discontinuity.
    let keys: Vec<usize> = frames.iter().enumerate().filter(|(_, h)| h.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keys.len(), 2, "a keyframe at the start and one after the rebuild: {keys:?}");
    assert!(
        keys[1] > 5 && keys[1] < frames.len() - 5,
        "frames on both sides of the rebuild: {keys:?} of {}",
        frames.len()
    );
    assert!(frames[keys[1]].discontinuity, "the rebuilt stream starts a new chain");
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[0].starts_with("video 0: encoding with refusing (hardware), 64x48 at 60 fps, 20000 kbps"),
        "{notes:?}"
    );
    assert!(notes[1].contains("set_rate failed") && notes[1].contains("21000 kbps"), "{notes:?}");
}
