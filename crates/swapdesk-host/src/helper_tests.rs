//! Helper loop tests: a service-side driver talks to `run_helper` over an in-memory duplex pipe, with the C7 fakes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use swapdesk_fakes::{FakeClock, InputRecord, Pacing, RecordingInputSink, SineSource, SyntheticScreen};
use swapdesk_ipc::{AudioModeKind, Events, FrameReader, FrameWriter, Handshake, IpcRole, Message, VideoBackend};
use swapdesk_platform::{
    CaptureConfig, CursorUpdate, DisplayId, DisplayInfo, EncodedFrame, EncoderConfig, EncoderInfo, Frame, MonoMicros,
    PlatformError, Result as PResult, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{H264Profile, InputEvent, MouseButton, Preset, VideoHeader};

use crate::helper::{HelperDeps, HelperError, HelperStats, run_helper};
use crate::testpipe::{PipeWriter, duplex};

const WAIT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(400);

/// A synthetic screen that counts how many of it are alive.
struct Tracked {
    inner: SyntheticScreen,
    live: Arc<AtomicUsize>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ScreenSource for Tracked {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        self.inner.displays()
    }
    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> PResult<()> {
        self.inner.open(display, config)
    }
    fn next_frame(&mut self, timeout: Duration) -> PResult<Option<Frame>> {
        self.inner.next_frame(timeout)
    }
    fn reacquire(&mut self) -> PResult<()> {
        self.inner.reacquire()
    }
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        self.inner.take_cursor()
    }
    fn close(&mut self) {
        self.inner.close();
    }
}

/// Emits a 4-byte marker (the encode counter) as the payload; keyframe on the first and on forced frames.
pub(crate) struct Passthrough {
    pub(crate) n: u32,
}

impl VideoEncoder for Passthrough {
    fn configure(&mut self, _config: &EncoderConfig) -> PResult<EncoderInfo> {
        self.n = 0;
        Ok(EncoderInfo {
            name: "passthrough".into(),
            hardware: false,
            profile: H264Profile::Main,
            level: 40,
            low_latency: true,
        })
    }
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        let first = self.n == 0;
        let marker = self.n.to_be_bytes().to_vec();
        self.n += 1;
        Ok(vec![EncodedFrame {
            captured_at: frame.captured_at,
            encode_us: 7,
            keyframe: first || force_keyframe,
            config_change: first,
            width: u16::try_from(frame.width).unwrap(),
            height: u16::try_from(frame.height).unwrap(),
            annex_b: marker,
        }])
    }
    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        Ok(())
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

type Deps = HelperDeps<Tracked, Passthrough, RecordingInputSink, SineSource>;

pub(crate) struct Rig {
    writer: Option<FrameWriter<PipeWriter>>,
    inbox: Receiver<Message>,
    helper: Option<JoinHandle<Result<HelperStats, HelperError>>>,
    input: RecordingInputSink,
    live: Arc<AtomicUsize>,
}

fn deps(lost_at: Option<u32>, input: &RecordingInputSink, live: &Arc<AtomicUsize>) -> Deps {
    let screens = Arc::clone(live);
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let audio_clock = Arc::clone(&clock);
    HelperDeps::new(
        move |_| {
            let mut inner = SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime);
            if let Some(n) = lost_at {
                inner.script_access_lost_at(n);
            }
            screens.fetch_add(1, Ordering::SeqCst);
            Ok(Tracked { inner, live: Arc::clone(&screens) })
        },
        |_frame, _config, _backend| Ok(Passthrough { n: 0 }),
        {
            let input = input.clone();
            move || Ok(input.clone())
        },
        move || Ok(SineSource::new(audio_clock.clone()).realtime()),
        clock,
    )
}

/// Starts the helper on its own thread and does the service's side of the handshake.
fn rig(lost_at: Option<u32>) -> Rig {
    rig_with(lost_at, None)
}

/// As `rig`, the helper serving `fs` (the user's disk) when it is given.
pub(crate) fn rig_with(lost_at: Option<u32>, fs: Option<Arc<swapdesk_files::FsServer>>) -> Rig {
    let input = RecordingInputSink::new();
    let live = Arc::new(AtomicUsize::new(0));
    let (helper_end, service_end) = duplex();
    let d = deps(lost_at, &input, &live);
    let d = match fs {
        Some(server) => d.with_fs(server),
        None => d,
    };
    let helper =
        std::thread::spawn(move || run_helper(helper_end.reader, helper_end.writer, d, IpcRole::HelperSystem, 4242));
    let mut reader = FrameReader::new(service_end.reader);
    let mut writer = FrameWriter::new(service_end.writer);
    let hello = Handshake::server(&mut reader, &mut writer, 3).expect("server handshake");
    assert_eq!(hello.role, IpcRole::HelperSystem);
    assert_eq!(hello.pid, 4242);
    let (tx, inbox) = channel();
    std::thread::spawn(move || {
        while let Ok(Some(m)) = reader.read_message() {
            if tx.send(m).is_err() {
                break;
            }
        }
    });
    Rig { writer: Some(writer), inbox, helper: Some(helper), input, live }
}

impl Rig {
    pub(crate) fn send(&mut self, msg: &Message) {
        self.writer.as_mut().unwrap().write_message(msg).unwrap();
    }

    fn start_video(&mut self) {
        self.send(&Message::StartVideo {
            display: 0,
            fps: 100,
            kbps: 1000,
            quality: Preset::Auto,
            backend: VideoBackend::Auto,
        });
    }

    /// The next message matching `f` within `WAIT`; other messages are skipped.
    pub(crate) fn wait<T>(&self, mut f: impl FnMut(&Message) -> Option<T>) -> T {
        let end = Instant::now() + WAIT;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(m) => {
                    if let Some(v) = f(&m) {
                        return v;
                    }
                }
                Err(e) => panic!("no matching message: {e:?}"),
            }
        }
    }

    fn next_frame(&self) -> (VideoHeader, Vec<u8>) {
        self.wait(|m| match m {
            Message::VideoFrame { header, payload } => Some((*header, payload.0.clone())),
            _ => None,
        })
    }

    /// Reads until nothing arrives for `QUIET`; panics if that never happens.
    fn drain_until_quiet(&self) {
        let end = Instant::now() + WAIT;
        loop {
            match self.inbox.recv_timeout(QUIET) {
                Ok(_) => assert!(Instant::now() < end, "messages never stopped"),
                Err(RecvTimeoutError::Timeout) => return,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    pub(crate) fn finish(&mut self) -> Result<HelperStats, HelperError> {
        self.helper.take().unwrap().join().expect("helper thread")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // Close the service's side so a failed test does not leave the helper waiting.
        self.writer = None;
    }
}

#[test]
fn handshake_then_displays_come_first() {
    let mut rig = rig(None);
    let first = rig.inbox.recv_timeout(WAIT).expect("first message");
    let Message::Displays(list) = first else { panic!("first message is not Displays") };
    assert_eq!(list.displays.len(), 1);
    assert_eq!((list.displays[0].id, list.displays[0].width, list.displays[0].height), (0, 64, 48));
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
}

#[test]
fn start_video_streams_frames_and_stop_video_ends_them() {
    let mut rig = rig(None);
    rig.start_video();
    let mut frames = Vec::new();
    for _ in 0..6 {
        frames.push(rig.next_frame());
    }
    let (first, payload) = &frames[0];
    assert!(first.keyframe && first.config, "first frame is a keyframe with config");
    assert_eq!(first.display_id, 0);
    assert_eq!((first.width, first.height), (64, 48));
    assert_eq!(first.encode_us, 7, "FakeClock does not advance, so the encoder's own figure is kept");
    assert_eq!(first.payload_len as usize, payload.len());
    for pair in frames.windows(2) {
        assert!(pair[1].0.frame_id > pair[0].0.frame_id, "frame ids increase");
        assert!(!pair[1].0.keyframe, "no keyframe without a request");
        assert!(!pair[1].0.discontinuity);
    }
    rig.send(&Message::StopVideo { display: 0 });
    rig.drain_until_quiet();
    assert_eq!(rig.live.load(Ordering::SeqCst), 0, "the capture thread dropped its screen");
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
}

#[test]
fn force_keyframe_yields_a_keyframe() {
    let mut rig = rig(None);
    rig.start_video();
    let first = rig.next_frame().0;
    assert!(first.keyframe);
    let plain = rig.wait(|m| match m {
        Message::VideoFrame { header, .. } if !header.keyframe => Some(header.frame_id),
        _ => None,
    });
    rig.send(&Message::ForceKeyframe { display: 0 });
    let forced = rig.wait(|m| match m {
        Message::VideoFrame { header, .. } if header.keyframe => Some(*header),
        _ => None,
    });
    assert!(forced.frame_id > plain);
    assert!(!forced.config, "a forced keyframe is not a reconfigure");
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
}

#[test]
fn access_lost_reacquires_and_the_next_frame_is_a_discontinuity_keyframe() {
    let mut rig = rig(Some(3));
    rig.start_video();
    let frames: Vec<_> = (0..4).map(|_| rig.next_frame().0).collect();
    assert!(frames[0].keyframe);
    for h in &frames[1..3] {
        assert!(!h.keyframe && !h.discontinuity, "frames before the loss are plain");
    }
    let after = frames[3];
    assert!(after.keyframe && after.discontinuity, "frame after reacquire: {after:?}");
    assert_eq!(after.frame_id, 3);
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
}

#[test]
fn an_input_batch_reaches_the_sink_in_order_and_release_all_is_recorded() {
    let mut rig = rig(None);
    let events = vec![
        InputEvent::Pointer { display: 0, x: 100, y: 200 },
        InputEvent::Button { button: MouseButton::Left, down: true },
        InputEvent::Key { usage_page: 0x07, usage: 0x04, down: true, modifiers: 0, locks: 0 },
        InputEvent::Text("ola".to_string()),
        InputEvent::Button { button: MouseButton::Left, down: false },
    ];
    rig.send(&Message::Input { events: Events(events.clone()) });
    rig.send(&Message::Input { events: Events(vec![InputEvent::Wheel { dx: 0, dy: -120 }]) });
    rig.send(&Message::ReleaseAll);
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
    let mut expected = events;
    expected.push(InputEvent::Wheel { dx: 0, dy: -120 });
    assert_eq!(rig.input.events(), expected);
    let records = rig.input.records();
    let release_at = records.iter().position(|r| r.what == InputRecord::ReleaseAll).expect("ReleaseAll recorded");
    let last_event = records.iter().rposition(|r| matches!(r.what, InputRecord::Event(_))).unwrap();
    assert!(release_at > last_event, "ReleaseAll comes after the events sent before it");
}

#[test]
fn audio_packets_flow_until_stop_audio() {
    let mut rig = rig(None);
    rig.send(&Message::StartAudio { exclude_pid: 0 });
    let mode = rig.wait(|m| match m {
        Message::AudioMode { mode } => Some(*mode),
        _ => None,
    });
    assert_eq!(mode, AudioModeKind::Loopback);
    let mut seqs = Vec::new();
    for _ in 0..5 {
        let datagram = rig.wait(|m| match m {
            Message::AudioPacket(d) => Some(d.0.clone()),
            _ => None,
        });
        assert!(datagram.len() <= 1500);
        let (header, _opus) = swapdesk_audio::parse_datagram(&datagram).expect("a valid audio datagram");
        seqs.push(header.seq);
    }
    assert!(seqs.windows(2).all(|w| w[1] > w[0]));
    rig.send(&Message::StopAudio);
    let off = rig.wait(|m| match m {
        Message::AudioMode { mode } => Some(*mode),
        _ => None,
    });
    assert_eq!(off, AudioModeKind::Off);
    rig.drain_until_quiet();
    rig.send(&Message::Shutdown);
    rig.finish().expect("clean end");
}

#[test]
fn shutdown_ends_the_loop_and_joins_every_thread() {
    let mut rig = rig(None);
    rig.start_video();
    rig.send(&Message::StartAudio { exclude_pid: 0 });
    rig.next_frame();
    rig.send(&Message::Shutdown);
    let stats = rig.finish().expect("Shutdown ends with Ok");
    assert_eq!(rig.live.load(Ordering::SeqCst), 0, "every screen was dropped by its thread");
    // The writer thread is joined too: the service's side sees the end of the stream.
    loop {
        match rig.inbox.recv_timeout(WAIT) {
            Ok(_) => {}
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("the helper's pipe never closed"),
        }
    }
    let _ = stats.dropped_media;
}

#[test]
fn end_of_stream_and_bye_end_the_loop_like_shutdown() {
    let mut rig = rig(None);
    rig.start_video();
    rig.next_frame();
    rig.writer = None;
    rig.finish().expect("EOF at a message boundary ends with Ok");
    assert_eq!(rig.live.load(Ordering::SeqCst), 0);

    let mut rig = rig_with_bye();
    rig.finish().expect("Bye ends with Ok");
}

fn rig_with_bye() -> Rig {
    let mut rig = rig(None);
    rig.send(&Message::Bye { code: swapdesk_proto::ErrorCode::NoError });
    rig
}

#[test]
fn a_failing_encoder_factory_is_a_fault_and_the_loop_goes_on() {
    let input = RecordingInputSink::new();
    let live = Arc::new(AtomicUsize::new(0));
    let (helper_end, service_end) = duplex();
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let screens = Arc::clone(&live);
    let d: HelperDeps<Tracked, Passthrough, RecordingInputSink, SineSource> = HelperDeps::new(
        move |_| {
            screens.fetch_add(1, Ordering::SeqCst);
            Ok(Tracked {
                inner: SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime),
                live: Arc::clone(&screens),
            })
        },
        |_, _, _| {
            Err(PlatformError {
                kind: swapdesk_platform::ErrorKind::Unavailable,
                op: "test",
                os_code: None,
                detail: "no encoder".into(),
            })
        },
        move || Ok(input.clone()),
        || {
            Err(PlatformError {
                kind: swapdesk_platform::ErrorKind::Unsupported,
                op: "test",
                os_code: None,
                detail: String::new(),
            })
        },
        clock,
    );
    let helper =
        std::thread::spawn(move || run_helper(helper_end.reader, helper_end.writer, d, IpcRole::HelperUser, 1));
    let mut reader = FrameReader::new(service_end.reader);
    let mut writer = FrameWriter::new(service_end.writer);
    Handshake::server(&mut reader, &mut writer, 1).unwrap();
    assert!(matches!(reader.read_message().unwrap(), Some(Message::Displays(_))));
    writer
        .write_message(&Message::StartVideo {
            display: 0,
            fps: 60,
            kbps: 1000,
            quality: Preset::Auto,
            backend: VideoBackend::Auto,
        })
        .unwrap();
    let Some(Message::HelperFault { code, what }) = reader.read_message().unwrap() else { panic!("expected a fault") };
    assert_eq!(code, swapdesk_proto::ErrorCode::Unavailable);
    assert!(what.0.contains("no encoder"));
    writer.write_message(&Message::StartAudio { exclude_pid: 0 }).unwrap();
    let mut saw_fault = false;
    loop {
        match reader.read_message().unwrap() {
            Some(Message::HelperFault { .. }) => saw_fault = true,
            Some(Message::AudioMode { mode: AudioModeKind::Off }) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(saw_fault);
    writer.write_message(&Message::Shutdown).unwrap();
    helper.join().unwrap().expect("the loop went on and ended cleanly");
}

#[test]
fn a_failing_input_factory_is_a_fault_input_is_dropped_and_the_loop_goes_on() {
    let live = Arc::new(AtomicUsize::new(0));
    let (helper_end, service_end) = duplex();
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let screens = Arc::clone(&live);
    let d: HelperDeps<Tracked, Passthrough, RecordingInputSink, SineSource> = HelperDeps::new(
        move |_| {
            screens.fetch_add(1, Ordering::SeqCst);
            Ok(Tracked {
                inner: SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime),
                live: Arc::clone(&screens),
            })
        },
        |_, _, _| Ok(Passthrough { n: 0 }),
        || {
            Err(PlatformError {
                kind: swapdesk_platform::ErrorKind::Unavailable,
                op: "test",
                os_code: None,
                detail: "no sink".into(),
            })
        },
        || {
            Err(PlatformError {
                kind: swapdesk_platform::ErrorKind::Unsupported,
                op: "test",
                os_code: None,
                detail: String::new(),
            })
        },
        clock,
    );
    let helper =
        std::thread::spawn(move || run_helper(helper_end.reader, helper_end.writer, d, IpcRole::HelperUser, 1));
    let mut reader = FrameReader::new(service_end.reader);
    let mut writer = FrameWriter::new(service_end.writer);
    Handshake::server(&mut reader, &mut writer, 1).unwrap();
    let mut saw_fault = false;
    // Input is dropped (more batches than the queue holds must not block the reader).
    for _ in 0..600 {
        writer.write_message(&Message::Input { events: Events(vec![InputEvent::Wheel { dx: 0, dy: 1 }]) }).unwrap();
    }
    writer
        .write_message(&Message::StartVideo {
            display: 0,
            fps: 60,
            kbps: 1000,
            quality: Preset::Auto,
            backend: VideoBackend::Auto,
        })
        .unwrap();
    loop {
        match reader.read_message().unwrap() {
            Some(Message::HelperFault { what, .. }) => {
                assert!(what.0.contains("no sink"));
                saw_fault = true;
            }
            Some(Message::VideoFrame { .. }) => break,
            Some(_) => {}
            None => panic!("pipe closed"),
        }
    }
    assert!(saw_fault, "the factory failure is reported");
    writer.write_message(&Message::Shutdown).unwrap();
    helper.join().unwrap().expect("the loop went on and ended cleanly");
}
