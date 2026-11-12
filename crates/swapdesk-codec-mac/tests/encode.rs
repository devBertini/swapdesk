//! The VideoToolbox encoder against synthetic frames, decoded back with this crate's own decoder. Nothing here needs a
//! TCC grant; the hardware encoder is used when the Mac has one (the test prints which one served). Run on the MacBook
//! with `scripts/mac/remote.sh test -p swapdesk-codec-mac`.
#![cfg(target_os = "macos")]

use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey, kCVReturnSuccess,
};
use swapdesk_codec_mac::annexb;
use swapdesk_codec_mac::{DecoderOptions, DecoderOutput, OutputWait, VtH264Decoder, VtH264Encoder};
use swapdesk_platform::{
    AdapterId, Clock, DisplayId, EncodedFrame, EncoderConfig, ErrorKind, Frame, GpuSurface, MonoMicros, Pixels,
    VideoDecoder, VideoEncoder,
};
use swapdesk_proto::{Codec, Preset, VideoHeader};

/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange` ('420f'), what the screen lane delivers.
const NV12_FULL_RANGE: u32 = 0x3432_3066;
const FRAME_US: u64 = 33_333;
const START_US: u64 = 1_000_000;

struct TestClock(Instant);

impl Clock for TestClock {
    fn now(&self) -> MonoMicros {
        MonoMicros(self.0.elapsed().as_micros() as u64)
    }
}

fn encoder() -> VtH264Encoder {
    VtH264Encoder::new(Arc::new(TestClock(Instant::now())))
}

fn config(width: u32, height: u32, kbps: u32) -> EncoderConfig {
    EncoderConfig { width, height, fps: 30, kbps, preset: Preset::Auto, adapter: AdapterId(0) }
}

/// A gray moving gradient (R = G = B, so the colour matrix does not matter for the luma comparison).
fn gradient(width: usize, height: usize, n: usize) -> Vec<u8> {
    let mut data = vec![255u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let v = ((x * 255 / width) + (y * 64 / height) + n * 4) % 256;
            let o = (y * width + x) * 4;
            data[o] = v as u8;
            data[o + 1] = v as u8;
            data[o + 2] = v as u8;
        }
    }
    data
}

/// A busy picture: a fast-moving gradient with pseudo-random grain (+-24 levels) that changes every frame, so no
/// encoder can make the sequence small, yet it stays inside what a real-time encoder can follow.
fn noise(width: usize, height: usize, n: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9u32 ^ (n as u32).wrapping_mul(0x85EB_CA6B);
    let mut data = gradient(width, height, n * 7);
    for px in data.as_chunks_mut::<4>().0 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let grain = i32::from((state >> 24) as u8 % 49) - 24;
        for c in &mut px[..3] {
            *c = (i32::from(*c) + grain).clamp(0, 255) as u8;
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

fn luma(b: u8, g: u8, r: u8) -> f64 {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
}

fn mean_abs_luma_diff(source: &[u8], decoded: &[u8]) -> f64 {
    assert_eq!(source.len(), decoded.len());
    let total: f64 = source
        .as_chunks::<4>()
        .0
        .iter()
        .zip(decoded.as_chunks::<4>().0)
        .map(|(a, b)| (luma(a[0], a[1], a[2]) - luma(b[0], b[1], b[2])).abs())
        .sum();
    total / (source.len() / 4) as f64
}

fn header(frame_id: u32, f: &EncodedFrame) -> VideoHeader {
    VideoHeader {
        keyframe: f.keyframe,
        config: annexb::parameter_sets(&f.annex_b).is_some(),
        discontinuity: false,
        display_id: 0,
        frame_id,
        capture_us: f.captured_at.0,
        encode_us: f.encode_us,
        width: f.width,
        height: f.height,
        codec: Codec::H264,
        payload_len: f.annex_b.len() as u32,
    }
}

fn percentile(sorted: &[u32], p: f64) -> u32 {
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn report_times(label: &str, outputs: &[EncodedFrame]) {
    let mut us: Vec<u32> = outputs.iter().map(|f| f.encode_us).collect();
    us.sort_unstable();
    println!(
        "{label}: {} frames, encode_us median {} p95 {} max {}",
        us.len(),
        percentile(&us, 0.5),
        percentile(&us, 0.95),
        us.last().copied().unwrap_or(0)
    );
}

fn encode_sequence(
    enc: &mut VtH264Encoder,
    w: usize,
    h: usize,
    range: std::ops::Range<usize>,
    make: fn(usize, usize, usize) -> Vec<u8>,
) -> Vec<EncodedFrame> {
    let mut outputs = Vec::new();
    for n in range {
        outputs.extend(enc.encode(&cpu_frame(w, h, n, make(w, h, n)), false).expect("encode"));
    }
    outputs
}

#[test]
fn sixty_frames_start_with_a_keyframe_and_decode_back_to_the_source() {
    let (w, h) = (1280usize, 720usize);
    let mut enc = encoder();
    let info = enc.configure(&config(w as u32, h as u32, 8000)).expect("configure");
    println!("encoder info: {info:?}");
    assert_eq!(info.level, 31, "720p30 fits level 3.1");

    let mut outputs = Vec::new();
    let mut sources = Vec::new();
    for n in 0..60 {
        let data = gradient(w, h, n);
        sources.push(data.clone());
        let force = n == 30;
        let out = enc.encode(&cpu_frame(w, h, n, data), force).expect("encode");
        assert_eq!(out.len(), 1, "frame {n} comes back in the same call");
        assert_eq!(out[0].captured_at.0, START_US + n as u64 * FRAME_US);
        outputs.extend(out);
    }
    assert!(enc.flush().expect("flush").is_empty());
    report_times("sixty_frames", &outputs);

    let first = &outputs[0];
    assert!(first.keyframe && first.config_change);
    let sets = annexb::parameter_sets(&first.annex_b).expect("SPS and PPS on the first keyframe");
    assert_eq!((sets.sps.len(), sets.pps.len()), (1, 1));
    assert!(annexb::is_idr(&first.annex_b));
    assert_eq!((first.width, first.height), (1280, 720));

    let keyframes: Vec<usize> = outputs.iter().enumerate().filter(|(_, f)| f.keyframe).map(|(i, _)| i).collect();
    assert_eq!(keyframes, [0, 30], "only the first frame and the forced one are keyframes");
    let forced = &outputs[30];
    assert!(annexb::parameter_sets(&forced.annex_b).is_some() && annexb::is_idr(&forced.annex_b));
    assert!(!forced.config_change, "a forced keyframe is not a configuration change");
    for f in outputs.iter().filter(|f| !f.keyframe) {
        assert!(!annexb::is_idr(&f.annex_b));
    }

    let mut dec = VtH264Decoder::new(DecoderOptions { output: DecoderOutput::CpuBgra });
    let mut worst = 0.0f64;
    let mut total = 0.0f64;
    for (i, f) in outputs.iter().enumerate() {
        let frame = dec.decode(&header(i as u32, f), &f.annex_b).expect("decode").expect("a picture");
        let Pixels::Cpu { data, .. } = &frame.pixels else { panic!("CPU pixels") };
        let diff = mean_abs_luma_diff(&sources[i], data);
        worst = worst.max(diff);
        total += diff;
    }
    println!("luma mean abs diff: mean {:.2}, worst frame {worst:.2}", total / 60.0);
    // Tolerance: 8 Mbps for a smooth 720p gradient is far above what it needs, so the loss is the 8-bit RGB to
    // limited-range YCbCr and back round trip (a few levels) plus quantisation; an encoder that scrambled the picture
    // would miss by tens of levels.
    assert!(worst < 6.0, "worst mean absolute luma difference {worst}");
}

/// B7 (F-VIEW-05): after a busy stretch at a low rate, refining the still picture gives one key frame that decodes
/// closer to the source than the last normal frame did, and the stream goes on from the same session with a P-frame.
#[test]
fn refinement_raises_the_psnr_of_a_still_frame() {
    let (w, h) = (1280usize, 720usize);
    let mut enc = encoder();
    enc.configure(&config(w as u32, h as u32, 1500)).expect("configure");
    let mut stream = encode_sequence(&mut enc, w, h, 0..30, noise);
    let still = noise(w, h, 29);
    let mut dec = VtH264Decoder::new(DecoderOptions { output: DecoderOutput::CpuBgra });
    let mut last_diff = f64::MAX;
    for (i, f) in stream.iter().enumerate() {
        let frame = dec.decode(&header(i as u32, f), &f.annex_b).expect("decode").expect("a picture");
        let Pixels::Cpu { data, .. } = &frame.pixels else { panic!("CPU pixels") };
        last_diff = mean_abs_luma_diff(&still, data);
    }
    let refined = enc.refine(&cpu_frame(w, h, 29, still.clone())).expect("refine");
    assert_eq!(refined.len(), 1, "exactly one refinement frame");
    assert!(refined[0].keyframe && annexb::is_idr(&refined[0].annex_b), "the refinement frame is an IDR");
    stream.extend(refined);
    let f = stream.last().expect("the refinement frame");
    let frame = dec.decode(&header(stream.len() as u32 - 1, f), &f.annex_b).expect("decode").expect("a picture");
    let Pixels::Cpu { data, .. } = &frame.pixels else { panic!("CPU pixels") };
    let refined_diff = mean_abs_luma_diff(&still, data);
    println!("refinement: mean abs luma diff {last_diff:.2} -> {refined_diff:.2}, {} bytes", f.annex_b.len());
    assert!(refined_diff < last_diff * 0.7, "the refined picture is not closer: {last_diff} -> {refined_diff}");

    // The session is the same: the next changed picture is a P-frame with no new parameter sets.
    let next = enc.encode(&cpu_frame(w, h, 30, noise(w, h, 30)), false).expect("encode after refine");
    assert_eq!(next.len(), 1);
    assert!(!next[0].keyframe && !next[0].config_change, "the session was rebuilt by the refinement");
    // The normal rate is back: once the buffer the boost left is spent (the first frames after it may use it), the
    // frames are as large as before the refinement, not as large as the refinement frame.
    let before: usize = stream[20..30].iter().map(|f| f.annex_b.len()).sum::<usize>() / 10;
    let after_frames = encode_sequence(&mut enc, w, h, 31..46, noise);
    let after: usize = after_frames[5..].iter().map(|f| f.annex_b.len()).sum::<usize>() / (after_frames.len() - 5);
    println!("normal frames: {before} bytes before the refinement, {after} after");
    assert!(after < before * 2, "the rate control is not back to normal: {before} -> {after} bytes a frame");
    assert!(after < f.annex_b.len() / 2, "the frames after are refinement-sized: {after}");
}

#[test]
fn set_rate_moves_the_output_size_the_right_way() {
    let (w, h) = (1280usize, 720usize);
    let measure = |first_kbps: u32, then_kbps: u32| -> usize {
        let mut enc = encoder();
        enc.configure(&config(w as u32, h as u32, first_kbps)).expect("configure");
        let _warm = encode_sequence(&mut enc, w, h, 0..20, noise);
        enc.set_rate(then_kbps, 30).expect("set_rate");
        // Rate control needs a moment to settle: skip frames, measure the next 40.
        let _settle = encode_sequence(&mut enc, w, h, 20..50, noise);
        let measured = encode_sequence(&mut enc, w, h, 50..90, noise);
        // Frames the encoder dropped itself count as zero bytes: dropping is how a real-time encoder obeys a low rate.
        assert!(measured.len() >= 10, "only {} of 40 frames came back", measured.len());
        measured.iter().map(|f| f.annex_b.len()).sum()
    };
    let to_low = measure(12_000, 1_000);
    let to_high = measure(1_000, 12_000);
    println!("bytes in 40 noisy frames: 12000 -> 1000 kbps: {to_low}, 1000 -> 12000 kbps: {to_high}");
    assert!(
        to_low < to_high,
        "after set_rate to 1000 kbps the output ({to_low}) must be smaller than after 12000 kbps ({to_high})"
    );
}

#[test]
fn a_new_size_rebuilds_the_session_and_flags_the_config_change() {
    let mut enc = encoder();
    enc.configure(&config(1280, 720, 6000)).expect("configure");
    let first = encode_sequence(&mut enc, 1280, 720, 0..5, gradient);
    assert_eq!(first.len(), 5);
    assert!(first[0].config_change && !first[1].config_change);

    let small = enc.encode(&cpu_frame(640, 360, 5, gradient(640, 360, 5)), false).expect("encode at a new size");
    assert_eq!(small.len(), 1);
    assert!(small[0].keyframe && small[0].config_change);
    assert_eq!((small[0].width, small[0].height), (640, 360));
    assert!(annexb::parameter_sets(&small[0].annex_b).is_some());
    let next = enc.encode(&cpu_frame(640, 360, 6, gradient(640, 360, 6)), false).expect("encode");
    assert!(!next[0].keyframe && !next[0].config_change);

    // The decoder follows the size change through the new parameter sets.
    let mut dec = VtH264Decoder::new(DecoderOptions { output: DecoderOutput::CpuBgra });
    for (id, f) in first.iter().chain(small.iter()).chain(next.iter()).enumerate() {
        let pic = dec.decode(&header(id as u32, f), &f.annex_b).expect("decode").expect("picture");
        assert_eq!((pic.width, pic.height), (u32::from(f.width), u32::from(f.height)));
    }

    // `configure` with a new size also flags the first output.
    let mut again = encoder();
    again.configure(&config(640, 360, 2000)).expect("configure");
    again.configure(&config(320, 180, 1000)).expect("configure again");
    let out = again.encode(&cpu_frame(320, 180, 0, gradient(320, 180, 0)), false).expect("encode");
    assert!(out[0].keyframe && out[0].config_change && out[0].width == 320);
}

fn nv12_buffer(width: usize, height: usize, n: usize) -> CFRetained<CVPixelBuffer> {
    let surface = CFDictionary::<CFString, CFType>::empty();
    // SAFETY: the framework's own static.
    let key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
    let attrs = CFDictionary::<CFString, CFType>::from_slices(&[key], &[surface.as_ref()]);
    // SAFETY: a CFDictionary whatever its key and value types say.
    let attrs = unsafe { CFRetained::cast_unchecked::<CFDictionary>(attrs) };
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: valid attributes and out pointer; one reference on success.
    let status =
        unsafe { CVPixelBufferCreate(None, width, height, NV12_FULL_RANGE, Some(&attrs), NonNull::from(&mut out)) };
    assert_eq!(status, kCVReturnSuccess, "CVPixelBufferCreate");
    // SAFETY: Create rule.
    let buffer = unsafe { CFRetained::from_raw(NonNull::new(out).expect("buffer")) };
    // SAFETY: plane memory is touched only between the lock and the unlock; rows are written inside `bytes_per_row`.
    unsafe {
        assert_eq!(CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)), kCVReturnSuccess);
        let y = CVPixelBufferGetBaseAddressOfPlane(&buffer, 0).cast::<u8>();
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0);
        for row in 0..height {
            for col in 0..width {
                *y.add(row * y_stride + col) = ((col * 255 / width) + n * 3) as u8;
            }
        }
        let uv = CVPixelBufferGetBaseAddressOfPlane(&buffer, 1).cast::<u8>();
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 1);
        for row in 0..height / 2 {
            std::ptr::write_bytes(uv.add(row * uv_stride), 128, width);
        }
        CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
    }
    buffer
}

#[test]
fn an_nv12_pixel_buffer_goes_in_through_pixels_gpu() {
    let (w, h) = (640usize, 360usize);
    let mut enc = encoder();
    enc.configure(&config(w as u32, h as u32, 3000)).expect("configure");
    let mut outputs = Vec::new();
    for n in 0..10 {
        let frame = Frame {
            display: DisplayId(0),
            captured_at: MonoMicros(START_US + n as u64 * FRAME_US),
            width: w as u32,
            height: h as u32,
            pixels: Pixels::Gpu(GpuSurface(Box::new(nv12_buffer(w, h, n)))),
            dirty: Vec::new(),
            content_changed: true,
        };
        outputs.extend(enc.encode(&frame, false).expect("encode"));
    }
    assert_eq!(outputs.len(), 10);
    assert!(outputs[0].keyframe && annexb::parameter_sets(&outputs[0].annex_b).is_some());
    report_times("nv12", &outputs);
    let mut dec = VtH264Decoder::new(DecoderOptions { output: DecoderOutput::CpuBgra });
    let pic = dec.decode(&header(0, &outputs[0]), &outputs[0].annex_b).expect("decode").expect("picture");
    assert_eq!((pic.width, pic.height), (640, 360));
    // The left edge is dark and the right edge bright, as filled.
    let Pixels::Cpu { data, .. } = &pic.pixels else { panic!("CPU pixels") };
    let at = |x: usize| luma(data[(180 * w + x) * 4], data[(180 * w + x) * 4 + 1], data[(180 * w + x) * 4 + 2]);
    assert!(at(8) + 100.0 < at(w - 8), "left {} right {}", at(8), at(w - 8));
}

#[test]
fn bad_input_and_a_missing_configuration_are_errors_not_panics() {
    let mut enc = encoder();
    let frame = cpu_frame(64, 64, 0, vec![0; 64 * 64 * 4]);
    assert_eq!(enc.encode(&frame, false).expect_err("not configured").kind, ErrorKind::Closed);
    assert_eq!(enc.set_rate(1000, 30).expect_err("not configured").kind, ErrorKind::Closed);
    assert_eq!(enc.configure(&config(0, 0, 1000)).expect_err("zero size").kind, ErrorKind::InvalidInput);
    enc.configure(&config(64, 64, 1000)).expect("configure");
    let short = cpu_frame(64, 64, 0, vec![0; 100]);
    assert_eq!(enc.encode(&short, false).expect_err("short data").kind, ErrorKind::InvalidInput);
    let not_a_buffer = Frame { pixels: Pixels::Gpu(GpuSurface(Box::new(5u32))), ..cpu_frame(64, 64, 1, Vec::new()) };
    assert_eq!(enc.encode(&not_a_buffer, false).expect_err("wrong surface").kind, ErrorKind::InvalidInput);
}

#[test]
fn text_preset_skips_frames_above_its_frame_rate_cap() {
    let (w, h) = (640usize, 360usize);
    let mut enc = encoder();
    enc.configure(&EncoderConfig { preset: Preset::Text, fps: 60, ..config(w as u32, h as u32, 2000) })
        .expect("configure");
    let mut kept = 0;
    for n in 0..60u64 {
        // 60 fps arrivals.
        let mut frame = cpu_frame(w, h, 0, gradient(w, h, n as usize));
        frame.captured_at = MonoMicros(START_US + n * 16_667);
        kept += enc.encode(&frame, false).expect("encode").len();
    }
    println!("text preset kept {kept} of 60 frames at 60 fps arrival");
    assert!((28..=32).contains(&kept), "kept {kept}");
}

/// Prints the latency of both ways to get a frame's output (module documentation of `encoder.rs`).
#[test]
fn wait_modes_compared() {
    let (w, h) = (1280usize, 720usize);
    type Make = fn(usize, usize, usize) -> Vec<u8>;
    for mode in [OutputWait::Condvar, OutputWait::CompleteFrames] {
        for (name, make) in [("gradient", gradient as Make), ("busy", noise as Make)] {
            let mut enc = encoder();
            enc.set_output_wait(mode);
            enc.configure(&config(w as u32, h as u32, 8000)).expect("configure");
            let t0 = Instant::now();
            let mut outputs = encode_sequence(&mut enc, w, h, 0..90, make);
            let wall = t0.elapsed();
            let before_flush = outputs.len();
            outputs.extend(enc.flush().expect("flush"));
            println!(
                "{mode:?} {name}: {before_flush} of 90 frames returned by encode itself, {} after flush,{} dropped by the encoder, 90 frames in {wall:?} wall",
                outputs.len(),
                enc.dropped_frames()
            );
            report_times(&format!("{mode:?} {name}"), &outputs);
            assert_eq!(
                outputs.len() as u64 + enc.dropped_frames(),
                90,
                "{mode:?} {name}: every frame is accounted for"
            );
        }
    }
}
