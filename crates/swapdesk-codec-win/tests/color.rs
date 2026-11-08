//! Stage W1.15 (L3 of the 2026-11-08 evaluation): colour fidelity of every encoder this PC offers. Flat patches (pure red,
//! green, blue and greys 16, 128 and 235 in 8-bit RGB) go through the encoder and the decoder to CPU BGRA, and each
//! patch centre must come back within a CIE76 delta E of 3.0 of what went in. A wrong RGB to YUV matrix (BT.601 against
//! BT.709) moves saturated colours by far more than that.
#![cfg(windows)]

mod common;

use common::{H, W, candidates, config, encoder, header};
use swapdesk_codec_win::{
    DecoderOptions, MfH264Decoder, NvencEncoder, NvencOptions, TextureUploader, VENDOR_NVIDIA, list_adapters,
};
use swapdesk_platform::{
    AdapterId, DisplayId, EncodedFrame, EncoderConfig, Frame, MonoMicros, Pixels, VideoDecoder, VideoEncoder,
};
use swapdesk_proto::Preset;

const PATCHES: [(&str, [u8; 3]); 6] = [
    ("red", [255, 0, 0]),
    ("green", [0, 255, 0]),
    ("blue", [0, 0, 255]),
    ("grey 16", [16, 16, 16]),
    ("grey 128", [128, 128, 128]),
    ("grey 235", [235, 235, 235]),
];
const MAX_DELTA_E: f64 = 3.0;
/// Frames per encoder: one IDR and a few P-frames of the same picture.
const FRAMES: usize = 4;

/// BGRA rows of the six patches side by side, each `W / 6` wide and the full height.
fn patch_rows() -> Vec<u8> {
    let patch_w = W as usize / PATCHES.len();
    let mut data = vec![0u8; W as usize * H as usize * 4];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let [r, g, b] = PATCHES[(x / patch_w).min(PATCHES.len() - 1)].1;
            let o = (y * W as usize + x) * 4;
            data[o..o + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    data
}

fn frame(data: &[u8], n: usize) -> Frame {
    Frame {
        display: DisplayId(0),
        captured_at: MonoMicros(9_000_000_000 + n as u64 * 16_667),
        width: W,
        height: H,
        pixels: Pixels::Cpu { stride: W * 4, data: data.to_vec() },
        dirty: Vec::new(),
        content_changed: true,
    }
}

/// sRGB (8 bit) to CIE L*a*b*, D65 white.
fn lab([r, g, b]: [u8; 3]) -> [f64; 3] {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    let (r, g, b) = (lin(r), lin(g), lin(b));
    let x = (0.4124564 * r + 0.3575761 * g + 0.1804375 * b) / 0.95047;
    let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
    let z = (0.0193339 * r + 0.1191920 * g + 0.9503041 * b) / 1.08883;
    let f = |t: f64| if t > 216.0 / 24389.0 { t.cbrt() } else { (24389.0 / 27.0 * t + 16.0) / 116.0 };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn delta_e76(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (a, b) = (lab(a), lab(b));
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Decodes `encoded` on `adapter` and returns, per patch, the largest delta E in the 9x9 block at the patch centre of the
/// last picture.
fn measure(encoded: &[EncodedFrame], adapter: AdapterId, dxva: bool, who: &str) -> Vec<(&'static str, f64)> {
    assert!(!encoded.is_empty(), "{who}: nothing encoded");
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter, dxva, ..Default::default() });
    let mut last = None;
    for (id, e) in encoded.iter().enumerate() {
        if let Some(d) =
            dec.decode(&header(e, id as u32), &e.annex_b).unwrap_or_else(|err| panic!("{who}: decode {id}: {err}"))
        {
            last = Some(d);
        }
    }
    let d = last.unwrap_or_else(|| panic!("{who}: nothing decoded"));
    let Pixels::Cpu { stride, data } = &d.pixels else { panic!("{who}: the decoder returned a GPU picture") };
    let patch_w = d.width as usize / PATCHES.len();
    PATCHES
        .iter()
        .enumerate()
        .map(|(i, (name, want))| {
            let (cx, cy) = (i * patch_w + patch_w / 2, d.height as usize / 2);
            let mut worst = 0f64;
            for y in cy - 4..=cy + 4 {
                for x in cx - 4..=cx + 4 {
                    let o = y * (*stride) as usize + x * 4;
                    worst = worst.max(delta_e76(*want, [data[o + 2], data[o + 1], data[o]]));
                }
            }
            (*name, worst)
        })
        .collect()
}

fn report(who: &str, deltas: &[(&'static str, f64)]) {
    let line: Vec<String> = deltas.iter().map(|(n, d)| format!("{n} {d:.2}")).collect();
    println!("delta E (CIE76, max over the centre 9x9) {who}: {}", line.join(", "));
    for (name, d) in deltas {
        assert!(*d <= MAX_DELTA_E, "{who}: patch {name} delta E {d:.2} is over {MAX_DELTA_E}");
    }
}

#[test]
fn color_patches_round_trip_within_delta_e() {
    let data = patch_rows();
    let mut ran = 0;

    // NVENC, when this PC has an NVIDIA adapter.
    if let Some(nv) =
        list_adapters().expect("adapters").into_iter().find(|a| a.vendor_id == VENDOR_NVIDIA && !a.software)
    {
        for preset in [Preset::Auto, Preset::Text, Preset::Motion] {
            let who = format!("NVENC {preset:?}");
            let uploader = TextureUploader::new(nv.id).expect("uploader");
            let mut enc = NvencEncoder::new(NvencOptions { device: Some(uploader.device()), ..Default::default() });
            enc.configure(&EncoderConfig { width: W, height: H, fps: 60, kbps: 20_000, preset, adapter: nv.id })
                .unwrap_or_else(|e| panic!("{who}: configure: {e}"));
            let mut encoded = Vec::new();
            for n in 0..FRAMES {
                encoded
                    .extend(enc.encode(&frame(&data, n), false).unwrap_or_else(|e| panic!("{who}: encode {n}: {e}")));
            }
            encoded.extend(enc.flush().expect("flush"));
            report(&who, &measure(&encoded, nv.id, true, &who));
        }
        ran += 1;
    } else {
        eprintln!("skipped NVENC: this PC has no NVIDIA adapter");
    }

    // Every Media Foundation encoder that configures here: hardware (Quick Sync) and the software MFT.
    for c in candidates() {
        let who = format!("MF {} ({})", c.name, if c.hardware { "hardware" } else { "software" });
        let mut enc = encoder(c);
        enc.configure(&config(c, Preset::Auto, 60, 20_000)).unwrap_or_else(|e| panic!("{who}: configure: {e}"));
        let mut encoded = Vec::new();
        for n in 0..FRAMES {
            encoded.extend(enc.encode(&frame(&data, n), false).unwrap_or_else(|e| panic!("{who}: encode {n}: {e}")));
        }
        encoded.extend(enc.flush().expect("flush"));
        report(&who, &measure(&encoded, c.adapter, true, &who));
        ran += 1;
    }
    assert!(ran > 0, "no encoder to test on this PC");
}
