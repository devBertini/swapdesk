//! HEVC through VideoToolbox (F-VIEW-07, stage B11): encode a synthetic picture, check the Annex B shape of the output
//! (VPS, SPS and PPS before each keyframe) and decode it back with this crate's decoder. Run on the MacBook with
//! `scripts/mac/remote.sh test -p swapdesk-codec-mac`. On a Mac without a hardware HEVC encoder the round trip tests
//! check that `configure` answers `Unsupported` and stop there.
#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::Instant;

use swapdesk_codec_mac::annexb;
use swapdesk_codec_mac::{DecoderOptions, DecoderOutput, VtH264Decoder, VtH264Encoder, hevc_support};
use swapdesk_platform::{
    AdapterId, Clock, DisplayId, EncodedFrame, EncoderConfig, ErrorKind, Frame, MonoMicros, Pixels, VideoDecoder,
    VideoEncoder,
};
use swapdesk_proto::{Codec, H264Profile, Preset, VideoHeader};

const FRAME_US: u64 = 33_333;
const START_US: u64 = 1_000_000;

struct TestClock(Instant);

impl Clock for TestClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(self.0.elapsed().as_micros() as u64)
    }
}

fn config(width: u32, height: u32, codec: Codec) -> EncoderConfig {
    EncoderConfig { width, height, fps: 30, kbps: 6000, preset: Preset::Auto, adapter: AdapterId(0), codec }
}

/// A gray moving gradient (R = G = B, so the colour matrix does not matter for the luma comparison).
fn gradient(width: usize, height: usize, n: usize) -> Vec<u8> {
    let mut data = vec![255u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let v = ((x * 255 / width) + (y * 64 / height) + n * 4) % 256;
            let o = (y * width + x) * 4;
            data[o..o + 3].fill(v as u8);
        }
    }
    data
}

fn cpu_frame(width: usize, height: usize, n: usize, data: Vec<u8>) -> Frame {
    Frame {
        display: DisplayId(0),
        captured_at: MonoMicros(START_US + n as u64 * FRAME_US),
        width: width as u32,
        height: height as u32,
        pixels: Pixels::Cpu { stride: (width * 4) as u32, data },
        dirty: Vec::new(),
        content_changed: true,
    }
}

fn mean_abs_luma_diff(source: &[u8], decoded: &[u8]) -> f64 {
    assert_eq!(source.len(), decoded.len());
    let luma = |p: &[u8]| 0.299 * f64::from(p[2]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[0]);
    let total: f64 =
        source.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0).map(|(a, b)| (luma(a) - luma(b)).abs()).sum();
    total / (source.len() / 4) as f64
}

fn header(frame_id: u32, f: &EncodedFrame) -> VideoHeader {
    VideoHeader {
        keyframe: f.keyframe,
        config: annexb::parameter_sets_for(Codec::Hevc, &f.annex_b).is_some(),
        discontinuity: false,
        display_id: 0,
        frame_id,
        capture_us: f.captured_at.0,
        encode_us: f.encode_us,
        width: f.width,
        height: f.height,
        codec: Codec::Hevc,
        payload_len: f.annex_b.len() as u32,
    }
}

/// A configured HEVC encoder, or `None` (after checking the refusal) on a Mac without hardware HEVC encoding.
fn hevc_encoder(width: u32, height: u32) -> Option<VtH264Encoder> {
    let mut enc = VtH264Encoder::new(Arc::new(TestClock(Instant::now())));
    match enc.configure(&config(width, height, Codec::Hevc)) {
        Ok(info) => {
            println!("encoder info: {info:?}");
            assert!(info.hardware, "HEVC is hardware only");
            assert_eq!(info.profile, H264Profile::Main);
            assert!(info.name.contains("HEVC"), "{}", info.name);
            Some(enc)
        }
        Err(e) => {
            assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
            assert!(!hevc_support().0, "the probe said HEVC hardware encoding exists, configure refused: {e}");
            None
        }
    }
}

#[test]
fn the_probe_is_cached_and_says_yes_on_apple_silicon() {
    let first = hevc_support();
    println!("hevc_support (encode_hw, decode_hw): {first:?}, arch {}", std::env::consts::ARCH);
    assert_eq!(first, hevc_support(), "the answer is cached");
    if std::env::consts::ARCH == "aarch64" {
        assert_eq!(first, (true, true), "Apple silicon encodes and decodes HEVC in hardware");
    }
}

#[test]
fn a_keyframe_carries_vps_sps_and_pps_and_a_forced_one_does_too() {
    let (w, h) = (640usize, 360usize);
    let Some(mut enc) = hevc_encoder(w as u32, h as u32) else { return };
    let mut outputs = Vec::new();
    for n in 0..12 {
        let out = enc.encode(&cpu_frame(w, h, n, gradient(w, h, n)), n == 8).expect("encode");
        assert_eq!(out.len(), 1, "frame {n} comes back in the same call");
        outputs.extend(out);
    }
    let keyframes: Vec<usize> = outputs.iter().enumerate().filter(|(_, f)| f.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keyframes, [0, 8], "the first frame and the forced one are keyframes");
    assert!(outputs[0].config_change && !outputs[8].config_change);
    for (i, f) in outputs.iter().enumerate() {
        let kinds: Vec<u8> = annexb::scan(&f.annex_b).iter().map(|n| annexb::hevc_type(f.annex_b[n.payload])).collect();
        if f.keyframe {
            assert_eq!(&kinds[..3], [annexb::HEVC_VPS, annexb::HEVC_SPS, annexb::HEVC_PPS], "frame {i}: {kinds:?}");
            assert!(annexb::is_idr_for(Codec::Hevc, &f.annex_b), "frame {i}: {kinds:?}");
            let sets = annexb::parameter_sets_for(Codec::Hevc, &f.annex_b).expect("parameter sets");
            assert_eq!((sets.vps.len(), sets.sps.len(), sets.pps.len()), (1, 1, 1));
        } else {
            assert!(!kinds.iter().any(|k| (annexb::HEVC_VPS..=annexb::HEVC_PPS).contains(k)), "frame {i}: {kinds:?}");
            assert!(!annexb::is_idr_for(Codec::Hevc, &f.annex_b), "frame {i}");
            assert!(annexb::has_picture_for(Codec::Hevc, &f.annex_b), "frame {i}");
        }
        assert!(f.annex_b.starts_with(&annexb::START_CODE), "frame {i} is Annex B");
    }
}

#[test]
fn hevc_encode_then_decode_gives_the_picture_back() {
    let (w, h) = (1280usize, 720usize);
    let Some(mut enc) = hevc_encoder(w as u32, h as u32) else { return };
    let mut sources = Vec::new();
    let mut outputs = Vec::new();
    for n in 0..30 {
        let data = gradient(w, h, n);
        sources.push(data.clone());
        outputs.extend(enc.encode(&cpu_frame(w, h, n, data), false).expect("encode"));
    }
    assert_eq!(outputs.len(), 30);
    assert!(outputs[0].keyframe);

    let mut dec = VtH264Decoder::new(DecoderOptions { output: DecoderOutput::CpuBgra });
    let mut worst = 0.0f64;
    for (i, f) in outputs.iter().enumerate() {
        let frame = dec.decode(&header(i as u32, f), &f.annex_b).expect("decode").expect("a picture");
        assert_eq!((frame.width, frame.height), (w as u32, h as u32));
        let Pixels::Cpu { data, .. } = &frame.pixels else { panic!("CPU pixels") };
        worst = worst.max(mean_abs_luma_diff(&sources[i], data));
    }
    println!("HEVC luma mean abs diff, worst frame {worst:.2}");
    // The same tolerance as the H.264 round trip (tests/encode.rs): the RGB to limited-range YCbCr and back loss plus
    // quantisation at 6 Mbps for a smooth gradient.
    assert!(worst < 6.0, "worst mean absolute luma difference {worst}");
}

#[test]
fn the_decoder_follows_a_codec_switch_and_a_non_hevc_config_is_unchanged() {
    // H.264 still works from the same encoder type, and a decoder fed H.264 then HEVC keyframes decodes both.
    let (w, h) = (640usize, 360usize);
    let mut h264 = VtH264Encoder::new(Arc::new(TestClock(Instant::now())));
    h264.configure(&config(w as u32, h as u32, Codec::H264)).expect("configure H.264");
    let a = h264.encode(&cpu_frame(w, h, 0, gradient(w, h, 0)), false).expect("encode H.264").remove(0);
    assert!(annexb::parameter_sets_for(Codec::H264, &a.annex_b).is_some());
    assert!(annexb::parameter_sets_for(Codec::Hevc, &a.annex_b).is_none());

    let mut dec = VtH264Decoder::new(DecoderOptions { output: DecoderOutput::CpuBgra });
    let h264_header = VideoHeader {
        keyframe: true,
        config: true,
        discontinuity: false,
        display_id: 0,
        frame_id: 0,
        capture_us: a.captured_at.0,
        encode_us: 0,
        width: a.width,
        height: a.height,
        codec: Codec::H264,
        payload_len: a.annex_b.len() as u32,
    };
    assert!(dec.decode(&h264_header, &a.annex_b).expect("decode H.264").is_some());

    let Some(mut enc) = hevc_encoder(w as u32, h as u32) else { return };
    let b = enc.encode(&cpu_frame(w, h, 0, gradient(w, h, 0)), false).expect("encode HEVC").remove(0);
    let frame = dec.decode(&header(1, &b), &b.annex_b).expect("decode HEVC").expect("a picture");
    assert_eq!((frame.width, frame.height), (w as u32, h as u32));
}
