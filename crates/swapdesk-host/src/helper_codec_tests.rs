//! F-VIEW-07, the helper's side: the codec of a session's `StartParams` reaches the encoder's configuration and every
//! video header, so a viewer that was promised HEVC gets frames that say HEVC (and H.264 stays what it was).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_ipc::{Message, VideoBackend};
use swapdesk_platform::{
    CaptureConfig, Clock, Codec, CursorUpdate, DisplayId, DisplayInfo, EncodedFrame, EncoderConfig, EncoderInfo, Frame,
    MonoMicros, Pixels, Result as PResult, ScreenSource, VideoEncoder,
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

/// A frame every 10 ms.
struct SteadyScreen(Instant);

impl ScreenSource for SteadyScreen {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        Ok(Vec::new())
    }
    fn open(&mut self, _display: DisplayId, _config: CaptureConfig) -> PResult<()> {
        Ok(())
    }
    fn next_frame(&mut self, _timeout: Duration) -> PResult<Option<Frame>> {
        std::thread::sleep(Duration::from_millis(10));
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

/// Records the codec of every configuration it is given.
#[derive(Clone, Default)]
struct RecordingEncoder {
    codecs: Arc<Mutex<Vec<Codec>>>,
    keyframe_next: bool,
}

impl VideoEncoder for RecordingEncoder {
    fn configure(&mut self, config: &EncoderConfig) -> PResult<EncoderInfo> {
        self.codecs.lock().unwrap().push(config.codec);
        self.keyframe_next = true;
        Ok(EncoderInfo {
            name: "recording".into(),
            hardware: true,
            profile: H264Profile::Main,
            level: 0,
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
        Ok(())
    }
    fn refine(&mut self, _frame: &Frame) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

/// Runs the capture loop for `codec` until 5 frames came out; returns the codecs the encoder was configured with and
/// the codec every video header named.
fn run(codec: Codec) -> (Vec<Codec>, Vec<Codec>) {
    let (outbox, rx) = Outbox::new(1024);
    let enc = RecordingEncoder::default();
    let factory_enc = enc.clone();
    let env = CaptureEnv {
        screen: Arc::new(|_| Ok(SteadyScreen(Instant::now()))),
        encoder: Arc::new(move |_f, _c, _b| Ok(factory_enc.clone())),
        clock: Arc::new(RealClock(Instant::now())),
        outbox,
        displays: Mutex::new(Vec::new()),
        input_tx: Mutex::new(None),
        desktop_epoch: Arc::default(),
        fence: Arc::default(),
        desktop_probe: None,
        desktop_sent: Mutex::default(),
    };
    let p = StartParams { display: 0, fps: 60, kbps: 8_000, preset: Preset::Auto, backend: VideoBackend::Auto, codec };
    let ctl = VideoCtl {
        stop: AtomicBool::new(false),
        force_key: AtomicBool::new(false),
        rate: Mutex::new(None),
        scale: Mutex::new((0, 0)),
    };
    let ids = AtomicU32::new(0);
    let headers = std::thread::scope(|s| {
        let h = s.spawn(|| {
            let mut source = (env.screen)(DisplayId(0)).unwrap();
            source.open(DisplayId(0), CaptureConfig { max_fps: 60 }).unwrap();
            capture_loop(&env, p, &ctl, &ids, &mut source)
        });
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while got.len() < 5 && Instant::now() < deadline {
            if let Some(Message::VideoFrame { header, .. }) = rx.recv_timeout(Duration::from_millis(50)) {
                got.push(header.codec);
            }
        }
        ctl.stop.store(true, Ordering::Release);
        h.join().unwrap().expect("the loop ends cleanly when stopped");
        got
    });
    let configured = enc.codecs.lock().unwrap().clone();
    (configured, headers)
}

#[test]
fn an_hevc_session_configures_the_encoder_and_labels_every_frame_hevc() {
    let (configured, headers) = run(Codec::Hevc);
    assert_eq!(configured, [Codec::Hevc]);
    assert_eq!(headers.len(), 5);
    assert!(headers.iter().all(|c| *c == Codec::Hevc), "{headers:?}");
}

#[test]
fn an_h264_session_stays_h264() {
    let (configured, headers) = run(Codec::H264);
    assert_eq!(configured, [Codec::H264]);
    assert!(headers.iter().all(|c| *c == Codec::H264), "{headers:?}");
}
