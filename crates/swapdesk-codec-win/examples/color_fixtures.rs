//! Writes the colour fixtures of the macOS decoder tests (`crates/swapdesk-codec-mac/tests/fixtures/*_patches.h264`):
//! six flat patches (red, green, blue, greys 16, 128 and 235 in 8-bit sRGB) side by side, 1280x720, two frames
//! (an IDR with SPS/PPS and a P-frame), encoded by every encoder family this PC has: NVENC, the Quick Sync MFT and the
//! Windows software MFT. Synthetic pictures only.
//!
//! Run: `cargo run -p swapdesk-codec-win --example color_fixtures -- <output folder>`.

#[cfg(windows)]
fn main() {
    use std::path::PathBuf;
    use swapdesk_codec_win::{
        EncoderOptions, MfH264Encoder, MftChoice, NvencEncoder, NvencOptions, TextureUploader, VENDOR_NVIDIA,
        list_adapters,
    };
    use swapdesk_platform::{DisplayId, EncoderConfig, Frame, MonoMicros, Pixels, VideoEncoder};
    use swapdesk_proto::Preset;

    // The fixture size; another size (`<folder> <width> <height>`) is for measurements. Below HD a stream without a
    // colour description is taken as BT.601 by VideoToolbox, at HD and above as BT.709 (measured 2026-11-09).
    let arg = |i: usize, d: u32| std::env::args().nth(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let (w, h) = (arg(2, 1280), arg(3, 720));
    const PATCHES: [[u8; 3]; 6] =
        [[255, 0, 0], [0, 255, 0], [0, 0, 255], [16, 16, 16], [128, 128, 128], [235, 235, 235]];

    let out = PathBuf::from(std::env::args().nth(1).expect("an output folder"));
    let mut data = vec![0u8; (w * h * 4) as usize];
    let patch_w = w as usize / PATCHES.len();
    for y in 0..h as usize {
        for x in 0..w as usize {
            let [r, g, b] = PATCHES[(x / patch_w).min(PATCHES.len() - 1)];
            data[(y * w as usize + x) * 4..][..4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    let frame = |n: u64| Frame {
        display: DisplayId(0),
        captured_at: MonoMicros(1_000_000 + n * 16_667),
        width: w,
        height: h,
        pixels: Pixels::Cpu { stride: w * 4, data: data.clone() },
        dirty: Vec::new(),
        content_changed: true,
    };
    let adapters = list_adapters().expect("adapters");
    let write = |name: &str, enc: &mut dyn VideoEncoder| {
        let mut bytes = Vec::new();
        for n in 0..2 {
            for e in enc.encode(&frame(n), n == 0).expect("encode") {
                bytes.extend_from_slice(&e.annex_b);
            }
        }
        for e in enc.flush().expect("flush") {
            bytes.extend_from_slice(&e.annex_b);
        }
        let suffix = if (w, h) == (1280, 720) { String::new() } else { format!("_{w}x{h}") };
        let path = out.join(format!("{name}_patches{suffix}.h264"));
        std::fs::write(&path, &bytes).expect("write");
        println!("{} ({} bytes)", path.display(), bytes.len());
    };
    if let Some(nv) = adapters.iter().find(|a| a.vendor_id == VENDOR_NVIDIA && !a.software) {
        let uploader = TextureUploader::new(nv.id).expect("uploader");
        let mut enc = NvencEncoder::new(NvencOptions { device: Some(uploader.device()), ..Default::default() });
        enc.configure(&EncoderConfig {
            width: w,
            height: h,
            fps: 60,
            kbps: 8_000,
            preset: Preset::Auto,
            adapter: nv.id,
            codec: swapdesk_platform::Codec::H264,
        })
        .expect("NVENC");
        write("nvenc", &mut enc);
    }
    let hw =
        adapters.iter().find(|a| !a.software && a.vendor_id != VENDOR_NVIDIA).or(adapters.first()).expect("an adapter");
    for (name, choice) in [("qsv", MftChoice::Named("Quick Sync".into())), ("mfsoft", MftChoice::SoftwareOnly)] {
        let mut enc = MfH264Encoder::new(EncoderOptions { choice, ..Default::default() });
        match enc.configure(&EncoderConfig {
            width: w,
            height: h,
            fps: 60,
            kbps: 8_000,
            preset: Preset::Auto,
            adapter: hw.id,
            codec: swapdesk_platform::Codec::H264,
        }) {
            Ok(_) => write(name, &mut enc),
            Err(e) => println!("{name}: {e}"),
        }
    }
}

#[cfg(not(windows))]
fn main() {}
