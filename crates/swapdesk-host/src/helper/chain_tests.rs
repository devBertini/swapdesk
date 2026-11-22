//! Review of 22/11/2026, C8: what the capture loop sends after it broke the chain of frames itself (a frame over the
//! size limit, a frame the service's queue had no room for). The next frame then carries `discontinuity`, and every
//! keyframe gate downstream (the media path, the sender, the viewer's sequencer) took that flag for a keyframe, also
//! on a P-frame whose reference never left this machine.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_ipc::{Message, VideoBackend};
use swapdesk_platform::{
    CaptureConfig, Clock, CursorUpdate, DisplayId, DisplayInfo, EncodedFrame, EncoderConfig, EncoderInfo, Frame,
    MonoMicros, Pixels, Result as PResult, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{H264Profile, MAX_VIDEO_PAYLOAD, Preset, VideoHeader};

use super::{CaptureEnv, StartParams, VideoCtl, capture_loop};
use crate::outbox::Outbox;

struct RealClock(Instant);

impl Clock for RealClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(self.0.elapsed().as_micros() as u64)
    }
}

/// A screen that changes every few milliseconds, `left` times, then stands still.
struct Changing {
    left: usize,
    started: Instant,
}

impl ScreenSource for Changing {
    fn displays(&mut self) -> PResult<Vec<DisplayInfo>> {
        Ok(Vec::new())
    }
    fn open(&mut self, _display: DisplayId, _config: CaptureConfig) -> PResult<()> {
        Ok(())
    }
    fn next_frame(&mut self, timeout: Duration) -> PResult<Option<Frame>> {
        if self.left == 0 {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        self.left -= 1;
        std::thread::sleep(Duration::from_millis(3).min(timeout));
        Ok(Some(Frame {
            display: DisplayId(0),
            captured_at: MonoMicros(self.started.elapsed().as_micros() as u64),
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

/// An encoder whose first outputs follow a script of (keyframe, bytes), whatever was asked: a hardware encoder with
/// a frame of latency answers a forced keyframe one frame late. After the script it honours every force.
struct Scripted {
    script: Vec<(bool, usize)>,
    calls: Arc<AtomicUsize>,
}

impl VideoEncoder for Scripted {
    fn configure(&mut self, _config: &EncoderConfig) -> PResult<EncoderInfo> {
        Ok(EncoderInfo {
            name: "scripted".into(),
            hardware: false,
            profile: H264Profile::Main,
            level: 40,
            low_latency: true,
        })
    }
    fn encode(&mut self, frame: &Frame, force_keyframe: bool) -> PResult<Vec<EncodedFrame>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let (keyframe, len) = self.script.get(n).copied().unwrap_or((force_keyframe, 100));
        Ok(vec![EncodedFrame {
            captured_at: frame.captured_at,
            encode_us: 5,
            keyframe,
            config_change: keyframe,
            width: 64,
            height: 48,
            annex_b: vec![7; len],
        }])
    }
    fn set_rate(&mut self, _kbps: u32, _fps: u16) -> PResult<()> {
        Ok(())
    }
    fn flush(&mut self) -> PResult<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

/// Runs the capture loop over `frames` screen changes and the encoder's `script`; returns the video headers sent.
fn sent(frames: usize, script: Vec<(bool, usize)>) -> Vec<VideoHeader> {
    let (outbox, rx) = Outbox::new(256);
    let calls = Arc::new(AtomicUsize::new(0));
    let script = Mutex::new(Some(script));
    let encoder_calls = Arc::clone(&calls);
    let env: CaptureEnv<Changing, Scripted> = CaptureEnv {
        screen: Arc::new(move |_| Ok(Changing { left: frames, started: Instant::now() })),
        encoder: Arc::new(move |_f, _c, _b| {
            Ok(Scripted {
                script: script.lock().unwrap().take().unwrap_or_default(),
                calls: Arc::clone(&encoder_calls),
            })
        }),
        clock: Arc::new(RealClock(Instant::now())),
        outbox,
        displays: Mutex::new(Vec::new()),
        input_tx: Mutex::new(None),
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
            capture_loop(&env, p, &ctl, &ids, &mut source)
        });
        let t0 = Instant::now();
        while calls.load(Ordering::SeqCst) < frames && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        ctl.stop.store(true, Ordering::Release);
        h.join().unwrap().unwrap();
    });
    assert_eq!(calls.load(Ordering::SeqCst), frames, "every change was encoded");
    rx.try_iter()
        .filter_map(|m| match m {
            Message::VideoFrame { header, .. } => Some(header),
            _ => None,
        })
        .collect()
}

/// Review of 22/11/2026, C8: after a frame over the size limit the encoder (one frame late) gave a P-frame for the
/// forced keyframe, and it left marked `discontinuity`: the gates downstream let it start a chain whose reference
/// never left this machine. A broken chain starts again only with a keyframe, and the P-frames before it stay here.
#[test]
fn after_a_frame_the_capture_dropped_only_a_keyframe_starts_the_chain_again() {
    let big = MAX_VIDEO_PAYLOAD as usize + 1;
    let headers = sent(5, vec![(true, 100), (false, 100), (false, big), (false, 100), (true, 100)]);
    let seen: Vec<(u32, bool, bool)> = headers.iter().map(|h| (h.frame_id, h.keyframe, h.discontinuity)).collect();
    assert!(
        headers.iter().all(|h| h.keyframe || !h.discontinuity),
        "a P-frame marked discontinuity left the helper: {seen:?}"
    );
    // The first keyframe, its P-frame, then (the frame over the limit and the late P-frame stay here) the keyframe
    // that starts the chain again.
    assert_eq!(seen, vec![(0, true, true), (1, false, false), (2, true, true)], "{seen:?}");
}
