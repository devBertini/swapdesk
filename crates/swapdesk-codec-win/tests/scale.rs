//! F-VIEW-06: the encoder takes a 1920x1080 frame and encodes it at the configured 960x540. The video processor does
//! the scaling in the pass that converts the colour (a stream destination rectangle smaller than the source), so flat
//! patches must come back with their colours at the new size: a wrong scale shows as a wrong size, a wrong rectangle as
//! black or shifted patches. GPU tests: they run on every encoder this PC offers, like the crate's other encoder tests.
#![cfg(windows)]

mod common;

use common::{H, W, candidates, config, encoder, header};
use swapdesk_codec_win::{
    DecoderOptions, MfH264Decoder, NvencEncoder, NvencOptions, TextureUploader, VENDOR_NVIDIA, list_adapters,
};
use swapdesk_platform::{
    DisplayId, EncodedFrame, EncoderConfig, Frame, MonoMicros, Pixels, VideoDecoder, VideoEncoder,
};
use swapdesk_proto::Preset;

const OUT: (u32, u32) = (W / 2, H / 2);
/// Red, green, blue and three greys, side by side.
const PATCHES: [[u8; 3]; 6] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [16, 16, 16], [128, 128, 128], [235, 235, 235]];
/// Per channel, after the YUV 4:2:0 round trip (the colour test allows a delta E of 3; 12 of 255 is looser on purpose).
const TOLERANCE: i32 = 12;

fn patch_frame(n: usize) -> Frame {
    let patch_w = W as usize / PATCHES.len();
    let mut data = vec![0u8; W as usize * H as usize * 4];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let [r, g, b] = PATCHES[(x / patch_w).min(PATCHES.len() - 1)];
            let o = (y * W as usize + x) * 4;
            data[o..o + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    Frame {
        display: DisplayId(0),
        captured_at: MonoMicros(9_000_000_000 + n as u64 * 16_667),
        width: W,
        height: H,
        pixels: Pixels::Cpu { stride: W * 4, data },
        dirty: Vec::new(),
        content_changed: true,
    }
}

fn check(encoded: &[EncodedFrame], adapter: swapdesk_platform::AdapterId, who: &str) {
    assert!(!encoded.is_empty(), "{who}: nothing encoded");
    assert!(
        encoded.iter().all(|e| (u32::from(e.width), u32::from(e.height)) == OUT),
        "{who}: every encoded frame reports {OUT:?}"
    );
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter, dxva: true, ..Default::default() });
    let mut last = None;
    for (id, e) in encoded.iter().enumerate() {
        if let Some(d) =
            dec.decode(&header(e, id as u32), &e.annex_b).unwrap_or_else(|err| panic!("{who}: {id}: {err}"))
        {
            last = Some(d);
        }
    }
    let d = last.unwrap_or_else(|| panic!("{who}: nothing decoded"));
    assert_eq!((d.width, d.height), OUT, "{who}: the decoded picture is the scaled size");
    let Pixels::Cpu { stride, data } = &d.pixels else { panic!("{who}: a GPU picture") };
    let patch_w = d.width as usize / PATCHES.len();
    for (i, want) in PATCHES.iter().enumerate() {
        let (cx, cy) = (i * patch_w + patch_w / 2, d.height as usize / 2);
        let o = cy * *stride as usize + cx * 4;
        let got = [data[o + 2], data[o + 1], data[o]];
        for c in 0..3 {
            assert!(
                (i32::from(got[c]) - i32::from(want[c])).abs() <= TOLERANCE,
                "{who}: patch {i} is {got:?}, wanted {want:?}"
            );
        }
    }
}

#[test]
fn media_foundation_encoders_scale_1920x1080_to_960x540() {
    let mut ran = 0;
    for c in candidates() {
        let who = format!("MF {} ({})", c.name, if c.hardware { "hardware" } else { "software" });
        let mut enc = encoder(c);
        let mut cfg = config(c, Preset::Auto, 60, 20_000);
        (cfg.width, cfg.height) = OUT;
        enc.configure(&cfg).unwrap_or_else(|e| panic!("{who}: configure: {e}"));
        let mut encoded = Vec::new();
        for n in 0..4 {
            encoded.extend(enc.encode(&patch_frame(n), false).unwrap_or_else(|e| panic!("{who}: encode {n}: {e}")));
        }
        encoded.extend(enc.flush().expect("flush"));
        check(&encoded, c.adapter, &who);
        ran += 1;
    }
    assert!(ran > 0, "no encoder to test on this PC");
}

#[test]
fn nvenc_scales_1920x1080_to_960x540_for_cpu_and_gpu_frames() {
    let Some(nv) = list_adapters().expect("adapters").into_iter().find(|a| a.vendor_id == VENDOR_NVIDIA && !a.software)
    else {
        eprintln!("skipped NVENC: this PC has no NVIDIA adapter");
        return;
    };
    let uploader = TextureUploader::new(nv.id).expect("uploader");
    for gpu_frames in [false, true] {
        let who = format!("NVENC, {} frames", if gpu_frames { "GPU" } else { "CPU" });
        let mut enc = NvencEncoder::new(NvencOptions { device: Some(uploader.device()), ..Default::default() });
        enc.configure(&EncoderConfig {
            width: OUT.0,
            height: OUT.1,
            fps: 60,
            kbps: 20_000,
            preset: Preset::Auto,
            adapter: nv.id,
            codec: swapdesk_platform::Codec::H264,
        })
        .unwrap_or_else(|e| panic!("{who}: configure: {e}"));
        let mut encoded = Vec::new();
        for n in 0..4 {
            let f = patch_frame(n);
            let f = if gpu_frames { uploader.upload(&f).expect("upload") } else { f };
            encoded.extend(enc.encode(&f, false).unwrap_or_else(|e| panic!("{who}: encode {n}: {e}")));
        }
        encoded.extend(enc.flush().expect("flush"));
        check(&encoded, nv.id, &who);
    }
}
