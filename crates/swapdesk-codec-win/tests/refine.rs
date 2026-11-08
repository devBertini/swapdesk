//! Stage W1.16 (L4 of the 2026-11-08 evaluation): still-screen refinement. A synthetic text page (black glyph-like strokes
//! on white) scrolls through the encoder at a motion bitrate, then stands still: `VideoEncoder::refine` encodes the last
//! picture once as an IDR at a low QP. The decoded refinement picture (the existing decoder, CPU BGRA) must be at least
//! 6 dB PSNR above the decoded last normal picture, must be a keyframe, and the encoder must be back at its normal rate
//! control for the next changed picture.
#![cfg(windows)]

mod common;

use common::{candidates, config, encoder, header};
use swapdesk_codec_win::{
    DecoderOptions, MfH264Decoder, NvencEncoder, NvencOptions, TextureUploader, VENDOR_NVIDIA, list_adapters,
};
use swapdesk_platform::{
    AdapterId, DisplayId, EncodedFrame, EncoderConfig, ErrorKind, Frame, MonoMicros, Pixels, VideoDecoder, VideoEncoder,
};
use swapdesk_proto::Preset;

const W: u32 = 1920;
const H: u32 = 1080;
/// Pages scrolled before the screen stands still.
const SCROLLED: usize = 30;
/// A motion bitrate low enough that 1080p text is visibly lossy at it, kbps.
const KBPS: u32 = 2500;
const MIN_GAIN_DB: f64 = 6.0;

/// A page of text-like strokes, scrolled up by `n * 3` rows: cells of 11x22 pixels, each with a few 2-pixel bars and
/// ticks picked by a hash of the cell, black (20) on white (240), hard edges.
fn page(n: usize) -> Vec<u8> {
    let scroll = n * 3;
    let mut data = vec![240u8; (W * H * 4) as usize];
    for y in 0..H as usize {
        let sy = y + scroll;
        let (row, cy) = (sy / 22, sy % 22);
        for x in 0..W as usize {
            let (col, cx) = (x / 11, x % 11);
            let h = (row as u32).wrapping_mul(0x9E37_79B1) ^ (col as u32).wrapping_mul(0x85EB_CA6B);
            let h = (h ^ (h >> 15)).wrapping_mul(0x2C1B_3C6D);
            let h = h ^ (h >> 12);
            // a blank cell now and then: word gaps
            if h & 7 == 0 {
                continue;
            }
            let bar_h = (4 + (h >> 4) % 3) as usize..(6 + (h >> 4) % 3) as usize;
            let bar_v = (2 + (h >> 8) % 4) as usize..(4 + (h >> 8) % 4) as usize;
            let ink = (h >> 12) & 3;
            let hit = (ink != 3 && bar_h.contains(&cy) && (1..10).contains(&cx))
                || (ink != 2 && bar_v.contains(&cx) && (3..20).contains(&cy))
                || (ink == 1 && cx == 8 && (3..14).contains(&cy));
            if hit {
                let o = (y * W as usize + x) * 4;
                data[o..o + 3].copy_from_slice(&[20, 20, 20]);
            }
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

/// PSNR in dB over the B, G and R bytes of every pixel.
fn psnr(source: &[u8], decoded: &swapdesk_platform::DecodedFrame) -> f64 {
    let Pixels::Cpu { stride, data } = &decoded.pixels else { panic!("the decoder returned a GPU picture") };
    assert_eq!((decoded.width, decoded.height), (W, H));
    let mut sum = 0u64;
    for y in 0..H as usize {
        for x in 0..W as usize {
            let (a, b) = ((y * W as usize + x) * 4, y * *stride as usize + x * 4);
            for c in 0..3 {
                let d = i64::from(source[a + c]) - i64::from(data[b + c]);
                sum += (d * d) as u64;
            }
        }
    }
    let mse = sum as f64 / f64::from(W * H * 3);
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

fn decode_all(encoded: &[EncodedFrame], adapter: AdapterId, who: &str) -> Vec<swapdesk_platform::DecodedFrame> {
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter, dxva: true, ..Default::default() });
    let mut out = Vec::new();
    for (id, e) in encoded.iter().enumerate() {
        if let Some(d) =
            dec.decode(&header(e, id as u32), &e.annex_b).unwrap_or_else(|err| panic!("{who}: decode {id}: {err}"))
        {
            out.push(d);
        }
    }
    out
}

/// Runs the scenario on `enc`. `Ok(false)`: the encoder refused the refinement (tolerated, reported).
fn scenario(enc: &mut dyn VideoEncoder, adapter: AdapterId, who: &str) -> bool {
    let pages: Vec<Vec<u8>> = (0..=SCROLLED).map(page).collect();
    let mut stream = Vec::new();
    for (n, p) in pages.iter().enumerate().take(SCROLLED) {
        stream.extend(enc.encode(&frame(p, n), false).unwrap_or_else(|e| panic!("{who}: encode {n}: {e}")));
    }
    let last = &pages[SCROLLED - 1];
    let normal_bytes: usize = stream.iter().skip(1).map(|e| e.annex_b.len()).sum::<usize>() / (stream.len() - 1).max(1);
    let before_pics = decode_all(&stream, adapter, who);
    assert_eq!(before_pics.len(), stream.len(), "{who}: every normal picture decodes");
    let before = psnr(last, before_pics.last().unwrap());

    let refined = match enc.refine(&frame(last, SCROLLED - 1)) {
        Ok(r) => r,
        Err(e) if e.kind == ErrorKind::Unsupported => {
            println!("REFINE {who}: refused ({e}); refinement skipped");
            return false;
        }
        Err(e) => panic!("{who}: refine: {e}"),
    };
    assert_eq!(refined.len(), 1, "{who}: exactly one refinement frame");
    assert!(refined[0].keyframe, "{who}: the refinement frame is an IDR");
    let mut with_refine = stream.clone();
    with_refine.extend(refined.iter().cloned());
    let pics = decode_all(&with_refine, adapter, who);
    assert_eq!(pics.len(), with_refine.len(), "{who}: the refinement picture decodes");
    let after = psnr(last, pics.last().unwrap());

    // A viewer that lost every earlier frame recovers from the refinement frame alone.
    let alone = decode_all(&refined, adapter, who);
    assert_eq!(alone.len(), 1, "{who}: the refinement frame decodes by itself");

    // The next change returns to the normal rate control: a P-frame of ordinary size that decodes.
    let next = enc
        .encode(&frame(&pages[SCROLLED], SCROLLED), false)
        .unwrap_or_else(|e| panic!("{who}: encode after refine: {e}"));
    assert_eq!(next.len(), 1, "{who}: one frame after the refinement");
    // NVENC and the capped-QP MFT continue with a P-frame; an MFT rebuilt for the refinement restarts with a keyframe
    // (new SPS/PPS) at the normal rate control.
    let next_kind = if next[0].keyframe { "IDR after a rebuild" } else { "P-frame" };
    let refine_bytes = refined[0].annex_b.len();
    with_refine.extend(next.iter().cloned());
    let pics = decode_all(&with_refine, adapter, who);
    assert_eq!(pics.len(), with_refine.len(), "{who}: the frame after the refinement decodes");
    let next_psnr = psnr(&pages[SCROLLED], pics.last().unwrap());
    println!(
        "REFINE {who}: PSNR before {before:.2} dB, after {after:.2} dB (gain {:.2}); refinement frame {refine_bytes} bytes, mean normal frame {normal_bytes} bytes, next frame ({next_kind}) {} bytes at {next_psnr:.2} dB",
        after - before,
        next[0].annex_b.len(),
    );
    assert!(
        after >= before + MIN_GAIN_DB,
        "{who}: PSNR {before:.2} -> {after:.2} dB, less than {MIN_GAIN_DB} dB of gain"
    );
    // (A keyframe after a rebuild is the MFT's own normal keyframe, whose size says nothing about the rate control.)
    assert!(
        next[0].keyframe || next[0].annex_b.len() * 2 < refine_bytes,
        "{who}: the rate control is not back to normal after the refinement"
    );
    true
}

#[test]
fn refinement_raises_psnr_by_six_db_and_the_rate_control_comes_back() {
    let mut refined = 0;
    if let Some(nv) =
        list_adapters().expect("adapters").into_iter().find(|a| a.vendor_id == VENDOR_NVIDIA && !a.software)
    {
        for preset in [Preset::Motion, Preset::Auto] {
            let who = format!("NVENC {preset:?}");
            let uploader = TextureUploader::new(nv.id).expect("uploader");
            let mut enc = NvencEncoder::new(NvencOptions { device: Some(uploader.device()), ..Default::default() });
            enc.configure(&EncoderConfig { width: W, height: H, fps: 60, kbps: KBPS, preset, adapter: nv.id })
                .unwrap_or_else(|e| panic!("{who}: configure: {e}"));
            refined += usize::from(scenario(&mut enc, nv.id, &who));
        }
    } else {
        eprintln!("skipped NVENC: this PC has no NVIDIA adapter");
    }
    for c in candidates() {
        let who = format!("MF {} ({})", c.name, if c.hardware { "hardware" } else { "software" });
        let mut enc = encoder(c);
        enc.configure(&config(c, Preset::Motion, 60, KBPS)).unwrap_or_else(|e| panic!("{who}: configure: {e}"));
        refined += usize::from(scenario(&mut enc, c.adapter, &who));
    }
    assert!(refined > 0, "no encoder on this PC took the refinement");
}
