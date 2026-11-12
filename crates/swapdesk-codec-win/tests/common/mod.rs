//! Shared helpers of the codec integration tests: synthetic frames, the list of encoders this PC offers, the video
//! header of an encoded frame.
#![cfg(windows)]
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use swapdesk_codec_win::{EncoderOptions, MfH264Encoder, MftChoice, list_adapters, probe_encoders};
use swapdesk_fakes::{FakeClock, SyntheticScreen};
use swapdesk_platform::{
    AdapterId, CaptureConfig, DisplayId, EncodedFrame, EncoderConfig, Frame, MonoMicros, Pixels, ScreenSource,
};
use swapdesk_proto::{Codec, Preset, VideoHeader};

pub const W: u32 = 1920;
pub const H: u32 = 1080;

type Raw = (MonoMicros, Vec<u8>);

/// Frames of a 1920x1080 synthetic screen, rendered once per test process (the renderer is slow in the dev profile).
/// Timestamps advance 1/60 s per frame on a fake clock, counters count from 0.
fn raw_frames() -> &'static Vec<Raw> {
    static FRAMES: OnceLock<Vec<Raw>> = OnceLock::new();
    FRAMES.get_or_init(|| {
        let clock = FakeClock::new(MonoMicros(5_000_000_000));
        let mut screen = SyntheticScreen::new(W, H, 60).with_clock(Arc::new(clock.clone()));
        screen.open(DisplayId(0), CaptureConfig { max_fps: 60 }).expect("open synthetic screen");
        let mut out = Vec::new();
        for _ in 0..64 {
            let f = screen.next_frame(Duration::from_secs(1)).expect("frame").expect("some frame");
            let Pixels::Cpu { data, .. } = f.pixels else { unreachable!("synthetic frames are CPU frames") };
            out.push((f.captured_at, data));
            clock.advance(Duration::from_micros(16_667));
        }
        out
    })
}

/// Frame `n` (0 to 63) of the synthetic screen as a CPU BGRA frame.
pub fn frame(n: usize) -> Frame {
    let (captured_at, data) = &raw_frames()[n];
    Frame {
        display: DisplayId(0),
        captured_at: *captured_at,
        width: W,
        height: H,
        pixels: Pixels::Cpu { stride: W * 4, data: data.clone() },
        dirty: Vec::new(),
        content_changed: true,
    }
}

/// The counter the barcode of frame `n` carries.
pub fn counter(n: usize) -> u32 {
    n as u32
}

pub fn captured_at(n: usize) -> MonoMicros {
    raw_frames()[n].0
}

/// One encoder this PC offers.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub clsid: String,
    pub adapter: AdapterId,
    pub hardware: bool,
}

/// Every encoder MFT that configures on this PC (hardware per adapter, then the software one).
pub fn candidates() -> &'static Vec<Candidate> {
    static LIST: OnceLock<Vec<Candidate>> = OnceLock::new();
    LIST.get_or_init(|| {
        probe_encoders()
            .expect("probe")
            .into_iter()
            .filter_map(|p| {
                let m = p.mft.ok()?;
                Some(Candidate { name: m.name, clsid: m.clsid, adapter: m.adapter, hardware: m.hardware })
            })
            .collect()
    })
}

pub fn software_candidate() -> Candidate {
    candidates().iter().find(|c| !c.hardware).cloned().expect("the Windows software H.264 encoder is present")
}

pub fn config(c: &Candidate, preset: Preset, fps: u16, kbps: u32) -> EncoderConfig {
    EncoderConfig { width: W, height: H, fps, kbps, preset, adapter: c.adapter, codec: swapdesk_platform::Codec::H264 }
}

pub fn encoder(c: &Candidate) -> MfH264Encoder {
    MfH264Encoder::new(EncoderOptions { choice: MftChoice::Clsid(c.clsid.clone()), ..Default::default() })
}

/// The video header of the `frame_id`-th encoded frame.
pub fn header(e: &EncodedFrame, frame_id: u32) -> VideoHeader {
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

/// Adapter of the first hardware GPU (the one frames are made on when a test does not care).
pub fn first_hardware_adapter() -> AdapterId {
    list_adapters().expect("adapters").into_iter().find(|a| !a.software).expect("a hardware adapter").id
}

/// Private bytes of this process (`PrivateUsage`): what a leak of CPU-side or driver memory grows.
pub fn private_bytes() -> u64 {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: fills the counters of this process; `cb` tells the size of the extended struct.
    unsafe {
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb);
    }
    c.PrivateUsage as u64
}

/// Open kernel handles of this process (a leaked MFT or D3D object usually leaks events and sections with it).
pub fn handle_count() -> u32 {
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
    let mut n = 0u32;
    // SAFETY: fills one counter for this process.
    unsafe {
        let _ = GetProcessHandleCount(GetCurrentProcess(), &mut n);
    }
    n
}

pub fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Encodes the 64 synthetic frames with `c` and returns the access units (the first is a keyframe with SPS/PPS).
pub fn encode_stream(c: &Candidate) -> Vec<EncodedFrame> {
    use swapdesk_platform::VideoEncoder;
    let mut enc = encoder(c);
    enc.configure(&config(c, Preset::Auto, 60, 20_000)).expect("configure");
    let mut out = Vec::new();
    for i in 0..64 {
        out.extend(enc.encode(&frame(i), false).unwrap_or_else(|e| panic!("encode {i}: {e}")));
    }
    out.extend(enc.flush().expect("flush"));
    out
}
