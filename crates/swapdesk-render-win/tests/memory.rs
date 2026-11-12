//! Memory of the viewer's decode and present loop (stage 38): the decoder and the renderer on one device and one thread,
//! as the viewer's decode thread runs them, over 3000 frames. The D3D11 debug layer is not installed on this PC (no
//! `D3D11*SDKLayers.dll`), so the measure is the process's private bytes and handle count at frame 300 and at frame 3000.
#![cfg(windows)]

#[path = "common/harness.rs"]
mod harness;

use std::sync::Arc;
use std::time::Duration;

use harness::{Pref, ShellStub};
use swapdesk_codec_win::{DecoderOptions, DecoderOutput, EncoderOptions, MfH264Decoder, MfH264Encoder, MftChoice};
use swapdesk_fakes::{FakeClock, SyntheticScreen};
use swapdesk_platform::{
    AdapterId, CaptureConfig, DisplayId, EncodedFrame, EncoderConfig, Frame, MonoMicros, Pixels, Rect, Renderer,
    ScaleMode, ScreenSource, VideoEncoder,
};
use swapdesk_proto::{Codec, Preset, VideoHeader};
use swapdesk_render_win::{RendererOptions, WinRenderer};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

const FRAMES: usize = 3000;
const WARM: usize = 300;
/// Growth allowed between the warm-up and the end. The leak this guards against is 0.17 MB/s, 2 to 3 KB per frame.
const BUDGET_MB: f64 = 2.0;

fn private_bytes() -> u64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: fills the counters of this process; `cb` tells the size of the extended struct.
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb);
    }
    c.PrivateUsage as u64
}

fn handle_count() -> u32 {
    let mut n = 0u32;
    // SAFETY: fills one counter for this process.
    unsafe {
        let _ = GetProcessHandleCount(GetCurrentProcess(), &mut n);
    }
    n
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// 64 frames of a 1920x1080 synthetic screen, encoded: the first access unit is a keyframe.
fn stream() -> Vec<EncodedFrame> {
    let clock = FakeClock::new(MonoMicros(5_000_000_000));
    let mut screen = SyntheticScreen::new(1920, 1080, 60).with_clock(Arc::new(clock.clone()));
    screen.open(DisplayId(0), CaptureConfig { max_fps: 60 }).expect("open synthetic screen");
    let mut enc = MfH264Encoder::new(EncoderOptions { choice: MftChoice::Auto, ..Default::default() });
    enc.configure(&EncoderConfig {
        width: 1920,
        height: 1080,
        fps: 60,
        kbps: 20_000,
        preset: Preset::Auto,
        adapter: AdapterId(0),
        codec: swapdesk_platform::Codec::H264,
    })
    .expect("configure");
    let mut out = Vec::new();
    for _ in 0..64 {
        let f: Frame = screen.next_frame(Duration::from_secs(1)).expect("frame").expect("some frame");
        assert!(matches!(f.pixels, Pixels::Cpu { .. }));
        out.extend(enc.encode(&f, false).expect("encode"));
        clock.advance(Duration::from_micros(16_667));
    }
    out.extend(enc.flush().expect("flush"));
    assert!(out[0].keyframe);
    out
}

fn header(e: &EncodedFrame, frame_id: u32) -> VideoHeader {
    VideoHeader {
        keyframe: e.keyframe,
        config: e.config_change,
        discontinuity: false,
        display_id: 0,
        frame_id,
        capture_us: e.captured_at.0,
        encode_us: e.encode_us,
        width: e.width,
        height: e.height,
        codec: Codec::H264,
        payload_len: e.annex_b.len() as u32,
    }
}

/// The viewer's loop: decode on the renderer's device, present, over `FRAMES` access units (the 64 over and over, each
/// loop from its keyframe). Returns (private bytes, handles) at frame `WARM` and at the end.
///
/// Measurement, not a gate: 3000 frames take 10 to 35 s and the byte count moves with other processes' load. Run it
/// alone, serially:
/// `cargo nextest run -p swapdesk-render-win --run-ignored only --test-threads 1 -E 'test(decode_and_present_keep_private_bytes_flat)'`
#[test]
#[ignore = "measure: run alone, serially"]
fn decode_and_present_keep_private_bytes_flat() {
    let encoded = stream();
    let area = Rect { x: 0, y: 0, width: 960, height: 540 };
    let shell = ShellStub::new(Pref::Min, (area.width, area.height), None).expect("shell stub");
    let mut renderer = WinRenderer::new(RendererOptions { wait: Duration::from_millis(200), ..Default::default() });
    renderer.attach(shell.surface(), area).expect("attach");
    let mut decoder = MfH264Decoder::new(DecoderOptions {
        output: DecoderOutput::Gpu,
        adapter: AdapterId(0),
        device: renderer.device(),
        dxva: true,
    });
    let (mut warm, mut presented, mut dropped) = ((0, 0), 0u32, 0u32);
    for n in 0..FRAMES {
        let e = &encoded[n % encoded.len()];
        let picture =
            decoder.decode_settled(&header(e, n as u32), &e.annex_b).unwrap_or_else(|err| panic!("decode {n}: {err}"));
        if let Some(frame) = picture {
            match renderer.present(frame, ScaleMode::Fit) {
                Ok(_) => presented += 1,
                Err(_) => dropped += 1,
            }
        }
        shell.pump();
        if n + 1 == WARM {
            warm = (private_bytes(), handle_count());
        }
    }
    let end = (private_bytes(), handle_count());
    println!(
        "decode+present: {presented} presented, {dropped} dropped of {FRAMES}; private {:.1} MB -> {:.1} MB ({:+.1} MB), handles {} -> {}",
        mb(warm.0),
        mb(end.0),
        mb(end.0) - mb(warm.0),
        warm.1,
        end.1
    );
    assert!(presented as usize >= FRAMES / 2, "only {presented} frames presented");
    let grown = mb(end.0) - mb(warm.0);
    assert!(
        grown <= BUDGET_MB,
        "private bytes grew {grown:.1} MB between frame {WARM} and {FRAMES} (budget {BUDGET_MB} MB)"
    );
    assert!(end.1 <= warm.1 + 8, "handles grew from {} to {}", warm.1, end.1);
    renderer.detach();
}
