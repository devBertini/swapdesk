//! Stage W1.12 (D11): a stalled pipe never stops the helper from reading the service, and the cursor shape (up to 256 KiB)
//! never makes the capture thread wait.
//!
//! The writer of the helper's pipe is gated by the test: it blocks, as a service that stopped reading would. Then
//! * the capture thread of a display that was stopped may be stuck in a send, and the thread that reads the pipe must
//!   still reach the `Input` and the `Shutdown` that follow the `StopVideo` (it used to join the capture thread there);
//! * a cursor shape that finds no room is kept and offered again, and the capture loop goes on and ends on `stop`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use swapdesk_fakes::{FakeClock, RecordingInputSink, SineSource, SyntheticScreen};
use swapdesk_ipc::{Events, FrameReader, FrameWriter, Handshake, IpcRole, Message, VideoBackend};
use swapdesk_platform::{
    CaptureConfig, Clock, CursorShape, CursorUpdate, DisplayId, DisplayInfo, EncodedFrame, EncoderConfig, EncoderInfo,
    ErrorKind, Frame, MonoMicros, PlatformError, Result as PResult, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{H264Profile, InputEvent, Preset};

use super::{CaptureEnv, CursorState, HelperDeps, StartParams, VideoCtl, capture_loop, run_helper};
use crate::helper_tests::Passthrough;
use crate::outbox::{Limits, Outbox};
use crate::testpipe::{PipeWriter, duplex};

const WAIT: Duration = Duration::from_secs(10);

/// A pipe writer that blocks until the test opens the gate.
struct Gated {
    inner: PipeWriter,
    open: Arc<(Mutex<bool>, Condvar)>,
}

impl std::io::Write for Gated {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (m, cv) = &*self.open;
        let _open = cv.wait_while(m.lock().unwrap(), |open| !*open).unwrap();
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// How many capture sources are open at once, and how many were ever opened.
#[derive(Default)]
struct Tracker {
    live: AtomicUsize,
    max_live: AtomicUsize,
    opened: AtomicUsize,
}

/// A screen whose capture always fails with `Unavailable`: the capture thread reports a fault (a control message).
struct FailingScreen {
    inner: SyntheticScreen,
    tracker: Arc<Tracker>,
    is_open: bool,
}

impl ScreenSource for FailingScreen {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        self.inner.displays()
    }
    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> PResult<()> {
        self.inner.open(display, config)?;
        self.is_open = true;
        self.tracker.opened.fetch_add(1, Ordering::SeqCst);
        let live = self.tracker.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.tracker.max_live.fetch_max(live, Ordering::SeqCst);
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> PResult<Option<Frame>> {
        std::thread::sleep(timeout.min(Duration::from_millis(5)));
        Err(PlatformError { kind: ErrorKind::Unavailable, op: "capture", os_code: None, detail: "scripted".into() })
    }
    fn reacquire(&mut self) -> PResult<()> {
        Ok(())
    }
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
    fn close(&mut self) {
        if std::mem::take(&mut self.is_open) {
            self.tracker.live.fetch_sub(1, Ordering::SeqCst);
        }
        self.inner.close();
    }
}

/// A helper whose writer is stopped and whose queue is full: every send waits.
struct Stalled {
    service: FrameWriter<PipeWriter>,
    reader: FrameReader<crate::testpipe::PipeReader>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    input: RecordingInputSink,
    helper: std::thread::JoinHandle<Result<super::HelperStats, super::HelperError>>,
    tracker: Arc<Tracker>,
}

impl Stalled {
    fn new() -> Self {
        let input = RecordingInputSink::new();
        let tracker = Arc::new(Tracker::default());
        let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
        let audio_clock = Arc::clone(&clock);
        let (sink, screens) = (input.clone(), Arc::clone(&tracker));
        // One message of room: with the writer gated, one reply fills the queue and everything after it waits.
        let deps = HelperDeps::new(
            move |_| {
                Ok(FailingScreen {
                    inner: SyntheticScreen::new(64, 48, 100),
                    tracker: Arc::clone(&screens),
                    is_open: false,
                })
            },
            |_frame, _config, _backend| Ok(Passthrough { n: 0 }),
            move || Ok(sink.clone()),
            move || Ok(SineSource::new(audio_clock.clone()).realtime()),
            clock,
        )
        .with_outbox_capacity(1);

        let (helper_end, service_end) = duplex();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let writer = Gated { inner: helper_end.writer, open: Arc::clone(&gate) };
        let helper =
            std::thread::spawn(move || run_helper(helper_end.reader, writer, deps, IpcRole::HelperSystem, 4242));
        let mut reader = FrameReader::new(service_end.reader);
        let mut service = FrameWriter::new(service_end.writer);
        Self::set_gate(&gate, true);
        Handshake::server(&mut reader, &mut service, 3).expect("handshake");
        // `Displays` is the first message the helper writes: read it, then stop the pipe.
        assert!(matches!(reader.read_message().unwrap(), Some(Message::Displays(_))));
        Self::set_gate(&gate, false);
        // Two requests the extras thread answers with control messages: the queue is full and the writer is stuck.
        service.write_message(&Message::ClipChangeCount { req: 1 }).unwrap();
        service.write_message(&Message::ClipChangeCount { req: 2 }).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        Self { service, reader, gate, input, helper, tracker }
    }

    fn set_gate(gate: &Arc<(Mutex<bool>, Condvar)>, open: bool) {
        let (m, cv) = &**gate;
        *m.lock().unwrap() = open;
        cv.notify_all();
    }

    fn start_video(&mut self) {
        self.service
            .write_message(&Message::StartVideo {
                display: 0,
                fps: 100,
                kbps: 1000,
                quality: Preset::Auto,
                backend: VideoBackend::Auto,
            })
            .unwrap();
    }

    /// Opens the gate, ends the helper with `Shutdown` and checks it ended cleanly.
    fn finish(mut self) {
        Self::set_gate(&self.gate, true);
        self.service.write_message(&Message::Shutdown).unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(self.helper.join().unwrap());
        });
        // Drain what the helper writes so its writer is never the thing that waits.
        let mut reader = self.reader;
        std::thread::spawn(move || while let Ok(Some(_)) = reader.read_message() {});
        assert!(rx.recv_timeout(WAIT).expect("the helper ended after the Shutdown").is_ok());
    }
}

#[test]
fn a_stuck_capture_thread_never_hides_the_input_or_the_shutdown_that_follows_a_stop_video() {
    let mut h = Stalled::new();
    // A capture thread whose fault has nowhere to go, then the stop that used to join it on the reading thread.
    h.start_video();
    std::thread::sleep(Duration::from_millis(150));
    h.service.write_message(&Message::StopVideo { display: 0 }).unwrap();
    h.service.write_message(&Message::Input { events: Events(vec![InputEvent::Wheel { dx: 0, dy: 1 }]) }).unwrap();

    let began = Instant::now();
    let applied = loop {
        if !h.input.events().is_empty() {
            break true;
        }
        if began.elapsed() > Duration::from_secs(3) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // Let everything go before asserting, so a failure does not leave the helper's threads stuck.
    let applied_msg = "the reading thread was held by the stopped capture thread: the input after StopVideo never came";
    if !applied {
        Stalled::set_gate(&h.gate, true);
    }
    assert!(applied, "{applied_msg}");
    h.finish();
}

#[test]
fn a_start_video_after_a_stop_video_waits_for_the_old_capture_thread_before_it_opens_the_display_again() {
    let mut h = Stalled::new();
    h.start_video();
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(h.tracker.opened.load(Ordering::SeqCst), 1, "the first source is open (and its thread is stuck)");
    // Session over and a new one begins at once: Stop, then Start, with the first thread still stuck in its send.
    h.service.write_message(&Message::StopVideo { display: 0 }).unwrap();
    h.start_video();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        h.tracker.opened.load(Ordering::SeqCst),
        1,
        "the second source must not open while the first thread runs"
    );
    assert_eq!(h.tracker.max_live.load(Ordering::SeqCst), 1);
    // The writer moves again: the first thread ends, then the second opens its source.
    Stalled::set_gate(&h.gate, true);
    let until = Instant::now() + Duration::from_secs(5);
    while h.tracker.opened.load(Ordering::SeqCst) < 2 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(h.tracker.opened.load(Ordering::SeqCst), 2, "the second source opens once the first thread is gone");
    assert_eq!(h.tracker.max_live.load(Ordering::SeqCst), 1, "never two sources of one display at once");
    h.finish();
}

/// Frames never come; the cursor does: two shapes of 100 KiB, one after the other.
struct CursorScreen {
    updates: Vec<CursorUpdate>,
}

impl ScreenSource for CursorScreen {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        Ok(Vec::new())
    }
    fn open(&mut self, _display: DisplayId, _config: CaptureConfig) -> PResult<()> {
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> PResult<Option<Frame>> {
        std::thread::sleep(timeout.min(Duration::from_millis(10)));
        Ok(None)
    }
    fn reacquire(&mut self) -> PResult<()> {
        Ok(())
    }
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        (!self.updates.is_empty()).then(|| self.updates.remove(0))
    }
    fn close(&mut self) {}
}

fn shape_update(id: u32) -> CursorUpdate {
    CursorUpdate {
        display: DisplayId(0),
        x: 1,
        y: 1,
        visible: true,
        shape: Some(CursorShape { id, width: 160, height: 160, hot_x: 0, hot_y: 0, rgba: vec![id as u8; 100 * 1024] }),
    }
}

struct NoEncoder;

impl VideoEncoder for NoEncoder {
    fn configure(&mut self, _config: &EncoderConfig) -> PResult<EncoderInfo> {
        Ok(EncoderInfo {
            name: "none".into(),
            hardware: false,
            profile: H264Profile::Main,
            level: 40,
            low_latency: true,
        })
    }
    fn encode(&mut self, _frame: &Frame, _force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        Ok(())
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

struct RealClock(Instant);

impl Clock for RealClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(self.0.elapsed().as_micros() as u64)
    }
}

#[test]
fn a_cursor_shape_that_finds_no_room_is_kept_and_sent_again_and_the_capture_loop_never_waits_for_it() {
    // Room for one message and 1 MiB: the first shape takes the empty queue, the second finds it full.
    let (outbox, rx) = Outbox::with_limits(Limits::new(1, 1024 * 1024));
    let env = CaptureEnv::<CursorScreen, NoEncoder> {
        screen: Arc::new(|_| unreachable!("the source is handed to the loop")),
        encoder: Arc::new(|_f, _c, _b| Ok(NoEncoder)),
        clock: Arc::new(RealClock(Instant::now())),
        outbox,
        displays: Vec::new(),
        desktop_epoch: Arc::default(),
    };
    let p = StartParams { display: 0, fps: 60, kbps: 1000, preset: Preset::Auto, backend: VideoBackend::Auto };
    let ctl = VideoCtl::default();
    let ids = std::sync::atomic::AtomicU32::new(0);
    let looped = Arc::new(AtomicBool::new(false));
    let shape_ids = |rx: &crate::outbox::OutboxReader| {
        rx.try_iter()
            .filter_map(|m| match m {
                Message::CursorShape(s) => Some(s.0.header.shape_id),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    std::thread::scope(|s| {
        let handle = s.spawn(|| {
            let mut source = CursorScreen { updates: vec![shape_update(1), shape_update(2)] };
            source.open(DisplayId(0), CaptureConfig { max_fps: 60 }).unwrap();
            let r = capture_loop(&env, p, &ctl, &ids, &mut source);
            looped.store(true, Ordering::SeqCst);
            r
        });
        std::thread::sleep(Duration::from_millis(250));
        assert!(!looped.load(Ordering::SeqCst), "the loop runs: it did not wait for the full queue");
        assert_eq!(env.outbox.queued().0, 1, "shape 1 is queued, shape 2 is kept by the capture thread");
        // The writer takes shape 1: the capture thread offers shape 2 again at its next look.
        assert_eq!(shape_ids(&rx), vec![1]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            got = shape_ids(&rx);
        }
        assert_eq!(got, vec![2], "shape 2 was sent once there was room");
        ctl.stop.store(true, Ordering::Release);
        handle.join().unwrap().unwrap();
    });
    assert!(looped.load(Ordering::SeqCst));
}

#[test]
fn the_cursor_state_names_a_shape_only_once_its_message_is_queued() {
    let (outbox, rx) = Outbox::with_limits(Limits::new(1, 1024 * 1024));
    let env = CaptureEnv::<CursorScreen, NoEncoder> {
        screen: Arc::new(|_| unreachable!()),
        encoder: Arc::new(|_f, _c, _b| Ok(NoEncoder)),
        clock: Arc::new(RealClock(Instant::now())),
        outbox,
        displays: Vec::new(),
        desktop_epoch: Arc::default(),
    };
    let mut state = CursorState::default();
    let mut source = CursorScreen { updates: vec![shape_update(1), shape_update(2)] };
    super::pump_cursor(&env, &mut source, 0, (100, 100), &mut state);
    assert_eq!(state.shape_id, 1, "shape 1 went into the empty queue");
    assert!(state.unsent.as_ref().is_some_and(|(id, _)| *id == 2), "shape 2 waits");
    // Positions keep naming the shape the viewer holds.
    assert!(matches!(rx.try_recv(), Some(Message::CursorShape(_))));
    super::pump_cursor(&env, &mut source, 0, (100, 100), &mut state);
    assert_eq!(state.shape_id, 2, "offered again and queued");
    assert!(state.unsent.is_none());
}
