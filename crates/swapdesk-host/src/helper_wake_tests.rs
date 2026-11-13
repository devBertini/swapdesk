//! A Mac whose display sleeps lists no display, so the first starts of the capture and of the audio fail with
//! `Unavailable`. The helper asks again, quietly, and the session's video and audio start when the display is back.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use swapdesk_fakes::{FakeClock, Pacing, RecordingInputSink, SineSource, SyntheticScreen};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake, IpcRole, Message, VideoBackend};
use swapdesk_platform::{
    AudioBlock, AudioSource, CaptureConfig, CursorUpdate, DisplayId, DisplayInfo, ErrorKind, Frame, MonoMicros,
    PlatformError, Result as PResult, ScreenSource,
};
use swapdesk_proto::Preset;

use crate::helper::{HelperDeps, run_helper};
use crate::helper_tests::Passthrough;
use crate::testpipe::duplex;

fn no_display() -> PlatformError {
    PlatformError {
        kind: ErrorKind::Unavailable,
        op: "SCShareableContent.displays",
        os_code: None,
        detail: "no display".into(),
    }
}

/// Fails `open` with "no display" while `down` is above zero (each failed attempt counts it down).
struct Sleeping {
    inner: SyntheticScreen,
    down: Arc<AtomicU32>,
}

impl ScreenSource for Sleeping {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        // As on a Mac whose display sleeps: nothing is listed until it wakes.
        if self.down.load(Ordering::SeqCst) > 0 {
            return Err(no_display());
        }
        self.inner.displays()
    }
    fn open(&mut self, display: DisplayId, config: CaptureConfig) -> PResult<()> {
        if self.down.load(Ordering::SeqCst) > 0 {
            self.down.fetch_sub(1, Ordering::SeqCst);
            return Err(no_display());
        }
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

/// The same for the audio capture's `start`.
struct SleepingAudio {
    inner: SineSource,
    down: Arc<AtomicU32>,
}

impl AudioSource for SleepingAudio {
    fn start(&mut self, exclude_pid: Option<u32>) -> PResult<()> {
        if self.down.load(Ordering::SeqCst) > 0 {
            self.down.fetch_sub(1, Ordering::SeqCst);
            return Err(no_display());
        }
        self.inner.start(exclude_pid)
    }
    fn next_block(&mut self, timeout: Duration) -> PResult<Option<AudioBlock>> {
        self.inner.next_block(timeout)
    }
    fn stop(&mut self) {
        self.inner.stop();
    }
}

#[test]
fn video_and_audio_start_after_the_display_wakes_with_no_fault_meanwhile() {
    let (video_down, audio_down) = (Arc::new(AtomicU32::new(2)), Arc::new(AtomicU32::new(2)));
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let (screen_down, source_down, audio_clock) = (Arc::clone(&video_down), Arc::clone(&audio_down), clock.clone());
    let d: HelperDeps<Sleeping, Passthrough, RecordingInputSink, SleepingAudio> = HelperDeps::new(
        move |_| {
            Ok(Sleeping {
                inner: SyntheticScreen::new(64, 48, 100).with_pacing(Pacing::Realtime),
                down: Arc::clone(&screen_down),
            })
        },
        |_, _, _| Ok(Passthrough { n: 0, size: None }),
        || Ok(RecordingInputSink::new()),
        move || {
            Ok(SleepingAudio { inner: SineSource::new(audio_clock.clone()).realtime(), down: Arc::clone(&source_down) })
        },
        clock,
    );
    let (helper_end, service_end) = duplex();
    let helper =
        std::thread::spawn(move || run_helper(helper_end.reader, helper_end.writer, d, IpcRole::HelperUser, 1));
    let mut reader = FrameReader::new(service_end.reader);
    let mut writer = FrameWriter::new(service_end.writer);
    Handshake::server(&mut reader, &mut writer, 1).unwrap();
    writer
        .write_message(&Message::StartVideo {
            display: 0,
            fps: 100,
            kbps: 1000,
            quality: Preset::Auto,
            backend: VideoBackend::Auto,
            codec: swapdesk_proto::Codec::H264,
        })
        .unwrap();
    writer.write_message(&Message::StartAudio { exclude_pid: 0 }).unwrap();
    let (mut frame, mut packet) = (false, false);
    while !(frame && packet) {
        match reader.read_message().unwrap().expect("the helper ended early") {
            Message::VideoFrame { .. } => frame = true,
            Message::AudioPacket(_) => packet = true,
            Message::HelperFault { what, .. } => panic!("a fault while the display was away: {}", what.0),
            _ => {}
        }
    }
    assert_eq!(video_down.load(Ordering::SeqCst), 0, "the video was asked again until the display was back");
    assert_eq!(audio_down.load(Ordering::SeqCst), 0, "the audio was asked again until the display was back");
    writer.write_message(&Message::Shutdown).unwrap();
    helper.join().unwrap().expect("clean end");
}

/// A helper that starts while the display sleeps lists nothing; once the display is back it tells the service by
/// itself, so the service can start the video it had no display to ask for.
#[test]
fn a_helper_started_with_no_display_lists_it_by_itself_once_it_is_back() {
    let down = Arc::new(AtomicU32::new(1));
    let clock = Arc::new(FakeClock::new(MonoMicros(1_000)));
    let (screen_down, audio_clock) = (Arc::clone(&down), clock.clone());
    let d: HelperDeps<Sleeping, Passthrough, RecordingInputSink, SleepingAudio> = HelperDeps::new(
        move |_| Ok(Sleeping { inner: SyntheticScreen::new(64, 48, 100), down: Arc::clone(&screen_down) }),
        |_, _, _| Ok(Passthrough { n: 0, size: None }),
        || Ok(RecordingInputSink::new()),
        move || Ok(SleepingAudio { inner: SineSource::new(audio_clock.clone()), down: Arc::new(AtomicU32::new(0)) }),
        clock,
    );
    let (helper_end, service_end) = duplex();
    let helper =
        std::thread::spawn(move || run_helper(helper_end.reader, helper_end.writer, d, IpcRole::HelperUser, 1));
    let mut reader = FrameReader::new(service_end.reader);
    let mut writer = FrameWriter::new(service_end.writer);
    Handshake::server(&mut reader, &mut writer, 1).unwrap();
    let first = loop {
        if let Message::Displays(list) = reader.read_message().unwrap().expect("the helper ended early") {
            break list;
        }
    };
    assert!(first.displays.is_empty(), "nothing is listed while the display sleeps");
    down.store(0, Ordering::SeqCst);
    let again = loop {
        if let Message::Displays(list) = reader.read_message().unwrap().expect("the helper ended early") {
            break list;
        }
    };
    assert!(!again.displays.is_empty(), "the display is listed once it is back, with no request");
    writer.write_message(&Message::Shutdown).unwrap();
    helper.join().unwrap().expect("clean end");
}
