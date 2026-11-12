//! Frames captured through DXGI Desktop Duplication from a display that is not on the NVIDIA adapter (DISPLAY1, Intel,
//! on this PC) encoded by Media Foundation, the path `H264Encoder` Auto takes there. The capture texture goes straight
//! into the D3D11 video processor (BGRA to NV12), which only takes textures with the bind flags its input view allows.
//! The frames are never saved or printed; only counts, sizes and flags are asserted.
#![cfg(windows)]

use std::time::{Duration, Instant};

use swapdesk_capture_win::{DxgiScreen, gpu_payload};
use swapdesk_codec_win::{
    ChosenBackend, EncoderOptions, H264Encoder, H264EncoderOptions, VENDOR_NVIDIA, list_adapters,
};
use swapdesk_platform::{
    CaptureConfig, EncoderConfig, ErrorKind, Frame, GpuSurface, MonoMicros, Pixels, ScreenSource, VideoEncoder,
};
use swapdesk_proto::Preset;

#[test]
fn real_capture_of_a_display_off_the_nvidia_adapter_encodes_through_media_foundation() {
    let adapters = list_adapters().expect("adapters");
    let mut screen = DxgiScreen::new();
    let displays = screen.displays().expect("displays");
    // A display whose adapter is a hardware adapter other than NVIDIA: Auto serves it with Media Foundation.
    let Some(display) = displays
        .iter()
        .find(|d| adapters.iter().any(|a| a.id == d.adapter && a.vendor_id != VENDOR_NVIDIA && !a.software))
    else {
        eprintln!("skipped: no display is owned by a non-NVIDIA hardware adapter");
        return;
    };
    screen.open(display.id, CaptureConfig { max_fps: 60 }).expect("open the display");
    let started = Instant::now();
    let first = loop {
        assert!(started.elapsed() < Duration::from_secs(5), "no first frame in 5 s");
        match screen.next_frame(Duration::from_millis(500)) {
            Ok(Some(f)) => break f,
            Ok(None) => {}
            Err(e) => panic!("capture: {e}"),
        }
    };
    let gf = gpu_payload(&first).expect("GPU frame");
    let (w, h) = (first.width, first.height);
    let mut enc = H264Encoder::new(H264EncoderOptions {
        mf: EncoderOptions { device: Some(gf.device().device.clone()), ..Default::default() },
        ..Default::default()
    });
    let config = EncoderConfig {
        width: w,
        height: h,
        fps: 30,
        kbps: 20_000,
        preset: Preset::Auto,
        adapter: display.adapter,
        codec: swapdesk_platform::Codec::H264,
    };
    let info = enc.configure(&config).expect("configure on the capture device");
    assert_eq!(enc.backend(), Some(ChosenBackend::MediaFoundation), "{info:?}");

    let mut current = first;
    let mut outputs = Vec::new();
    let total = 30u64;
    for seq in 0..total {
        // DDA delivers a frame only when the desktop changes: a timeout encodes the last capture again.
        match screen.next_frame(Duration::from_millis(5)) {
            Ok(Some(f)) => current = f,
            Ok(None) => {}
            Err(e) if e.kind == ErrorKind::AccessLost => screen.reacquire().expect("reacquire"),
            Err(e) => panic!("capture: {e}"),
        }
        let tex = gpu_payload(&current).expect("GPU frame").texture().clone();
        let frame = Frame {
            display: current.display,
            captured_at: MonoMicros(1_000_000 + seq * 33_333),
            width: current.width,
            height: current.height,
            pixels: Pixels::Gpu(GpuSurface(Box::new(tex))),
            dirty: Vec::new(),
            content_changed: true,
        };
        outputs.extend(enc.encode(&frame, false).expect("encode a captured frame"));
    }
    outputs.extend(enc.flush().expect("flush"));
    println!(
        "{} {w}x{h} on {}: {} outputs for {total} captured frames, first {} bytes",
        display.name,
        info.name,
        outputs.len(),
        outputs.first().map_or(0, |e| e.annex_b.len())
    );
    assert_eq!(outputs.len() as u64, total, "one output per input");
    assert!(outputs[0].keyframe, "the first output is a keyframe");
    assert!(outputs.iter().all(|e| (u32::from(e.width), u32::from(e.height)) == (w, h) && !e.annex_b.is_empty()));
}
