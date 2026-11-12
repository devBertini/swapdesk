//! Stage W1.16 (L4): the capture loop's still-screen refinement. A scripted screen delivers two bursts of frames with a
//! still second between them; the encoder counts its calls. Exactly one refinement frame follows each burst (a keyframe),
//! none while the screen stays still, and the frames after the second burst go through `encode` again.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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

/// Frames at the scheduled offsets from `open`, nothing in between.
struct ScriptedScreen {
    schedule: Vec<Duration>,
    next: usize,
    started: Instant,
}

impl ScreenSource for ScriptedScreen {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        Ok(Vec::new())
    }
    fn open(&mut self, _display: DisplayId, _config: CaptureConfig) -> PResult<()> {
        self.started = Instant::now();
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> PResult<Option<Frame>> {
        let now = self.started.elapsed();
        if let Some(due) = self.schedule.get(self.next).copied()
            && due.saturating_sub(now) <= timeout
        {
            std::thread::sleep(due.saturating_sub(now));
            self.next += 1;
            return Ok(Some(Frame {
                display: DisplayId(0),
                captured_at: MonoMicros(self.started.elapsed().as_micros() as u64),
                width: 64,
                height: 48,
                pixels: Pixels::Cpu { stride: 256, data: vec![0; 256 * 48] },
                dirty: Vec::new(),
                content_changed: true,
            }));
        }
        std::thread::sleep(timeout);
        Ok(None)
    }
    fn reacquire(&mut self) -> PResult<()> {
        Ok(())
    }
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
    fn close(&mut self) {}
}

#[derive(Clone, Default)]
struct Counts {
    encodes: Arc<AtomicUsize>,
    refines: Arc<AtomicUsize>,
}

struct CountingEncoder {
    counts: Counts,
    refuse: bool,
}

fn out(frame: &Frame, keyframe: bool, len: usize) -> EncodedFrame {
    EncodedFrame {
        captured_at: frame.captured_at,
        encode_us: 5,
        keyframe,
        config_change: false,
        width: 64,
        height: 48,
        annex_b: vec![7; len],
    }
}

impl VideoEncoder for CountingEncoder {
    fn configure(&mut self, _config: &EncoderConfig) -> PResult<EncoderInfo> {
        Ok(EncoderInfo {
            name: "counting".into(),
            hardware: false,
            profile: H264Profile::Main,
            level: 40,
            low_latency: true,
        })
    }
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        let n = self.counts.encodes.fetch_add(1, Ordering::SeqCst);
        Ok(vec![out(frame, n == 0 || force_keyframe, 100)])
    }
    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        Ok(())
    }
    fn refine(&mut self, frame: &Frame) -> PResult<Vec<EncodedFrame>> {
        self.counts.refines.fetch_add(1, Ordering::SeqCst);
        if self.refuse {
            return Err(PlatformError {
                kind: ErrorKind::Unsupported,
                op: "refine",
                os_code: None,
                detail: "refused".into(),
            });
        }
        Ok(vec![out(frame, true, 5000)])
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

/// Runs the capture loop for `run_for` over `schedule`; returns the video frames sent as (keyframe, bytes) and the faults.
fn run(schedule: Vec<Duration>, run_for: Duration, refuse: bool) -> (Vec<(bool, usize)>, Vec<String>, Counts) {
    let (outbox, rx) = Outbox::new(256);
    let counts = Counts::default();
    let encoder_counts = counts.clone();
    let sched = Mutex::new(Some(schedule));
    let env = CaptureEnv {
        screen: Arc::new(move |_| {
            Ok(ScriptedScreen {
                schedule: sched.lock().unwrap().take().unwrap_or_default(),
                next: 0,
                started: Instant::now(),
            })
        }),
        encoder: Arc::new(move |_f, _c, _b| Ok(CountingEncoder { counts: encoder_counts.clone(), refuse })),
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
        kbps: 5000,
        preset: Preset::Auto,
        backend: VideoBackend::Auto,
        codec: swapdesk_platform::Codec::H264,
    };
    let ctl = VideoCtl { stop: AtomicBool::new(false), force_key: AtomicBool::new(false), rate: Mutex::new(None) };
    let ids = AtomicU32::new(0);
    std::thread::scope(|s| {
        let h = s.spawn(|| {
            let mut source = (env.screen)(DisplayId(0)).unwrap();
            source.open(DisplayId(0), CaptureConfig { max_fps: 60 }).unwrap();
            capture_loop(&env, p, &ctl, &ids, &mut source)
        });
        std::thread::sleep(run_for);
        ctl.stop.store(true, Ordering::Release);
        h.join().unwrap().unwrap();
    });
    let (mut frames, mut faults) = (Vec::new(), Vec::new());
    for m in rx.try_iter() {
        match m {
            Message::VideoFrame { header, payload } => frames.push((header.keyframe, payload.0.len())),
            Message::HelperFault { what, .. } => faults.push(what.0),
            _ => {}
        }
    }
    (frames, faults, counts)
}

#[test]
fn one_refinement_frame_per_still_period_and_nothing_else() {
    let ms = Duration::from_millis;
    // A burst of three frames, a still stretch of about 1.5 s, then two frames and 1 s of stillness.
    let schedule = vec![ms(10), ms(30), ms(50), ms(1600), ms(1620)];
    let (frames, faults, counts) = run(schedule, ms(2800), false);
    assert!(faults.is_empty(), "{faults:?}");
    assert_eq!(counts.encodes.load(Ordering::SeqCst), 5);
    assert_eq!(counts.refines.load(Ordering::SeqCst), 2, "one refinement per still period");
    // 3 normal, 1 refinement, 2 normal, 1 refinement; the bytes in the still stretches are the refinement frames alone.
    let shape: Vec<(bool, usize)> = frames.clone();
    assert_eq!(
        shape,
        vec![(true, 100), (false, 100), (false, 100), (true, 5000), (false, 100), (false, 100), (true, 5000)],
        "{frames:?}"
    );
}

#[test]
fn a_refusing_encoder_is_logged_once_and_the_stream_goes_on() {
    let ms = Duration::from_millis;
    let (frames, faults, counts) = run(vec![ms(10), ms(1500)], ms(2500), true);
    assert_eq!(frames.len(), 2, "no refinement frames: {frames:?}");
    assert_eq!(counts.refines.load(Ordering::SeqCst), 2);
    assert_eq!(faults.len(), 1, "logged once: {faults:?}");
    assert!(faults[0].contains("refinement"), "{faults:?}");
}
