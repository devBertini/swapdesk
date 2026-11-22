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

/// The capture environment over a scripted screen and the counting encoder, on the given clock.
fn env_on(
    clock: Arc<dyn Clock>,
    outbox: Outbox,
    counts: &Counts,
    schedule: Vec<Duration>,
    refuse: bool,
) -> CaptureEnv<ScriptedScreen, CountingEncoder> {
    let encoder_counts = counts.clone();
    let sched = Mutex::new(Some(schedule));
    CaptureEnv {
        screen: Arc::new(move |_| {
            Ok(ScriptedScreen {
                schedule: sched.lock().unwrap().take().unwrap_or_default(),
                next: 0,
                started: Instant::now(),
            })
        }),
        encoder: Arc::new(move |_f, _c, _b| Ok(CountingEncoder { counts: encoder_counts.clone(), refuse })),
        clock,
        outbox,
        displays: std::sync::Mutex::new(Vec::new()),
        input_tx: std::sync::Mutex::new(None),
        desktop_epoch: Arc::default(),
        fence: Arc::default(),
        desktop_probe: None,
        desktop_sent: Mutex::default(),
    }
}

/// Runs the capture loop for `run_for` over `schedule`; returns the video frames sent as (keyframe, bytes) and the faults.
fn run(schedule: Vec<Duration>, run_for: Duration, refuse: bool) -> (Vec<(bool, usize)>, Vec<String>, Counts) {
    let (outbox, rx) = Outbox::new(256);
    let counts = Counts::default();
    let env = env_on(Arc::new(RealClock(Instant::now())), outbox, &counts, schedule, refuse);
    let p = StartParams {
        display: 0,
        fps: 60,
        kbps: 5000,
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

/// A clock the test sets.
struct ManualClock(std::sync::atomic::AtomicU64);

impl Clock for ManualClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(self.0.load(Ordering::SeqCst))
    }
}

fn a_frame() -> Frame {
    Frame {
        display: DisplayId(0),
        captured_at: MonoMicros(0),
        width: 64,
        height: 48,
        pixels: Pixels::Cpu { stride: 256, data: vec![0; 256 * 48] },
        dirty: Vec::new(),
        content_changed: true,
    }
}

/// F-VIEW-05: the threshold is exactly `STILL_AFTER_US` (400 ms): a picture still for 399.999 ms is not refined, at
/// 400 ms exactly one refinement goes out, and nothing more while the screen stays still.
#[test]
fn a_still_of_399_ms_sends_no_refinement_and_at_400_ms_exactly_one() {
    use super::{Still, refine_still};
    assert_eq!(super::STILL_AFTER_US, 400_000, "the flow text says 400 ms");
    let (outbox, rx) = Outbox::new(16);
    let counts = Counts::default();
    let clock = Arc::new(ManualClock(std::sync::atomic::AtomicU64::new(1_000_000)));
    let env = env_on(clock.clone(), outbox, &counts, Vec::new(), false);
    let p = StartParams {
        display: 0,
        fps: 60,
        kbps: 5000,
        preset: Preset::Auto,
        backend: VideoBackend::Auto,
        codec: swapdesk_platform::Codec::H264,
    };
    let ids = AtomicU32::new(0);
    let (mut discontinuity, mut force) = (false, false);
    let mut enc = CountingEncoder { counts: counts.clone(), refuse: false };
    // The picture was encoded at 1.000 s on the clock.
    let mut still = Still { frame: Some(a_frame()), since: 1_000_000, ..Default::default() };
    let mut look = |at_us: u64, still: &mut Still| {
        clock.0.store(at_us, Ordering::SeqCst);
        refine_still(&env, p, &ids, &mut enc, still, &mut discontinuity, &mut force);
    };

    look(1_000_000, &mut still);
    look(1_399_999, &mut still);
    assert_eq!(counts.refines.load(Ordering::SeqCst), 0, "399.999 ms of stillness sends nothing");
    assert!(rx.try_iter().next().is_none());

    look(1_400_000, &mut still);
    assert_eq!(counts.refines.load(Ordering::SeqCst), 1, "400 ms sends the refinement");
    let sent: Vec<_> = rx.try_iter().collect();
    assert_eq!(sent.len(), 1, "one frame");
    assert!(matches!(&sent[0], Message::VideoFrame { header, payload } if header.keyframe && payload.0.len() == 5000));

    look(2_400_000, &mut still);
    assert_eq!(counts.refines.load(Ordering::SeqCst), 1, "never again while the screen stays still");
    assert!(rx.try_iter().next().is_none());
}

// ----- a still screen must not hold back what the viewer needs (review of 22/11/2026, C1, C2, C7) -----

fn params() -> StartParams {
    StartParams {
        display: 0,
        fps: 60,
        kbps: 5000,
        preset: Preset::Auto,
        backend: VideoBackend::Auto,
        codec: swapdesk_platform::Codec::H264,
    }
}

fn ctl() -> VideoCtl {
    VideoCtl {
        stop: AtomicBool::new(false),
        force_key: AtomicBool::new(false),
        rate: Mutex::new(None),
        scale: Mutex::new((0, 0)),
    }
}

/// Runs the capture loop over `schedule` on `env` while `during` acts on its controls; the video frames sent as
/// (keyframe, width) and the notes and faults told.
fn run_with<E: VideoEncoder + Send>(
    env: &CaptureEnv<ScriptedScreen, E>,
    rx: &crate::outbox::OutboxReader,
    during: impl FnOnce(&VideoCtl),
) -> (Vec<(bool, u16)>, Vec<String>) {
    let (p, ctl, ids) = (params(), ctl(), AtomicU32::new(0));
    std::thread::scope(|s| {
        let h = s.spawn(|| {
            let mut source = (env.screen)(DisplayId(0)).unwrap();
            source.open(DisplayId(0), CaptureConfig { max_fps: 60 }).unwrap();
            capture_loop(env, p, &ctl, &ids, &mut source)
        });
        during(&ctl);
        ctl.stop.store(true, Ordering::Release);
        h.join().unwrap().unwrap();
    });
    let (mut frames, mut told) = (Vec::new(), Vec::new());
    for m in rx.try_iter() {
        match m {
            Message::VideoFrame { header, .. } => frames.push((header.keyframe, header.width)),
            Message::HelperFault { what, .. } | Message::HelperNote { what } => told.push(what.0),
            _ => {}
        }
    }
    (frames, told)
}

/// C1: the viewer's picture must start over (a lost frame, a decoder made again, a window that attached late) while
/// nothing moves on the host's screen. The request used to wait for the next change of the screen, so the viewer
/// kept a frozen or black picture: what the owner met as "a conexão caiu".
#[test]
fn a_keyframe_asked_for_on_a_still_screen_is_answered_from_the_last_picture() {
    let ms = Duration::from_millis;
    let (outbox, rx) = Outbox::new(256);
    let counts = Counts::default();
    // One frame, then the screen stands still for the whole run.
    let env = env_on(Arc::new(RealClock(Instant::now())), outbox, &counts, vec![ms(10)], true);
    let (frames, _) = run_with(&env, &rx, |ctl| {
        std::thread::sleep(ms(300));
        ctl.force_key.store(true, Ordering::Release);
        std::thread::sleep(ms(500));
        // Asked again and again (a viewer that keeps failing): four a second at most, not one per look.
        for _ in 0..40 {
            ctl.force_key.store(true, Ordering::Release);
            std::thread::sleep(ms(10));
        }
    });
    assert!(frames.len() >= 2, "the request was answered without a change of the screen: {frames:?}");
    assert!(frames.iter().all(|(key, _)| *key), "each answer is a keyframe: {frames:?}");
    assert!(frames.len() <= 5, "and a flood of requests is not a flood of keyframes: {frames:?}");
}

/// An encoder that fails `fail` times, then works.
struct FlakyEncoder {
    fail: Arc<AtomicUsize>,
}

impl VideoEncoder for FlakyEncoder {
    fn configure(&mut self, _config: &EncoderConfig) -> PResult<EncoderInfo> {
        Ok(EncoderInfo {
            name: "flaky".into(),
            hardware: false,
            profile: H264Profile::Main,
            level: 40,
            low_latency: true,
        })
    }
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        if self.fail.load(Ordering::SeqCst) > 0 {
            self.fail.fetch_sub(1, Ordering::SeqCst);
            return Err(PlatformError {
                kind: ErrorKind::Os,
                op: "encode",
                os_code: None,
                detail: "device removed".into(),
            });
        }
        Ok(vec![out(frame, force_keyframe, 100)])
    }
    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        Ok(())
    }
    fn refine(&mut self, _frame: &Frame) -> PResult<Vec<EncodedFrame>> {
        Err(PlatformError { kind: ErrorKind::Unsupported, op: "refine", os_code: None, detail: "none".into() })
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

fn flaky_env(
    outbox: Outbox,
    schedule: Vec<Duration>,
    made: &Arc<AtomicUsize>,
    fail: &Arc<AtomicUsize>,
) -> CaptureEnv<ScriptedScreen, FlakyEncoder> {
    let sched = Mutex::new(Some(schedule));
    let (made, fail) = (Arc::clone(made), Arc::clone(fail));
    CaptureEnv {
        screen: Arc::new(move |_| {
            Ok(ScriptedScreen {
                schedule: sched.lock().unwrap().take().unwrap_or_default(),
                next: 0,
                started: Instant::now(),
            })
        }),
        encoder: Arc::new(move |_f, _c, _b| {
            made.fetch_add(1, Ordering::SeqCst);
            Ok(FlakyEncoder { fail: Arc::clone(&fail) })
        }),
        clock: Arc::new(RealClock(Instant::now())),
        outbox,
        displays: std::sync::Mutex::new(Vec::new()),
        input_tx: std::sync::Mutex::new(None),
        desktop_epoch: Arc::default(),
        fence: Arc::default(),
        desktop_probe: None,
        desktop_sent: Mutex::default(),
    }
}

/// C2: an encoder error (a device the system reset, a driver replaced) used to end the capture thread for the rest
/// of the session, and the viewer kept the last picture until it disconnected.
#[test]
fn an_encoder_that_fails_is_built_again_and_the_stream_restarts_with_a_keyframe() {
    let ms = Duration::from_millis;
    let (outbox, rx) = Outbox::new(256);
    let (made, fail) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    // Two frames, the encoder breaks on the third, and the screen stands still after it.
    let env = flaky_env(outbox, vec![ms(10), ms(30), ms(200)], &made, &fail);
    let (frames, told) = run_with(&env, &rx, |_| {
        std::thread::sleep(ms(120));
        fail.store(1, Ordering::SeqCst);
        std::thread::sleep(ms(900));
    });
    assert_eq!(made.load(Ordering::SeqCst), 2, "a new encoder was built: {told:?}");
    assert_eq!(frames.len(), 3, "the picture the encoder failed on is sent by the new one: {frames:?}");
    assert!(frames[2].0, "the stream restarts with a keyframe: {frames:?}");
    assert!(told.iter().any(|t| t.contains("the encoder failed")), "the service's log says so: {told:?}");
}

/// C2: an encoder that keeps failing ends this run of the capture with its error, for `capture_main` to start the
/// whole capture again; it never spins on a dead encoder.
#[test]
fn an_encoder_that_keeps_failing_ends_the_run_with_its_error() {
    let ms = Duration::from_millis;
    let (outbox, _rx) = Outbox::new(256);
    let (made, fail) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(usize::MAX)));
    let env = flaky_env(outbox, vec![ms(10)], &made, &fail);
    let (p, ctl, ids) = (params(), ctl(), AtomicU32::new(0));
    let mut source = (env.screen)(DisplayId(0)).unwrap();
    source.open(DisplayId(0), CaptureConfig { max_fps: 60 }).unwrap();
    let started = Instant::now();
    let end = capture_loop(&env, p, &ctl, &ids, &mut source);
    assert!(end.is_err(), "the run ends with the encoder's error");
    assert_eq!(made.load(Ordering::SeqCst), super::ENCODER_REBUILDS as usize + 1, "after its rebuilds");
    assert!(started.elapsed() < Duration::from_secs(5), "and soon");
}

/// C7: the viewer's window changed size while the host's screen stood still: the picture kept its old size (blurry
/// when the window grew) until something moved on the host.
#[test]
fn a_new_window_size_on_a_still_screen_is_encoded_from_the_last_picture() {
    let ms = Duration::from_millis;
    let (outbox, rx) = Outbox::new(256);
    let counts = Counts::default();
    let env = env_on(Arc::new(RealClock(Instant::now())), outbox, &counts, vec![ms(10)], true);
    let (frames, _) = run_with(&env, &rx, |ctl| {
        std::thread::sleep(ms(300));
        // The window now shows the 64x48 picture at 32x24.
        *ctl.scale.lock().unwrap() = (32, 24);
        std::thread::sleep(ms(1200));
    });
    assert!(frames.len() >= 2, "the last picture went out again at the new size: {frames:?}");
    assert!(frames.last().unwrap().0, "as a keyframe: {frames:?}");
}
