//! Encode synthetic 1920x1080 frames with every encoder MFT of this PC and decode them back (stage 13).
#![cfg(windows)]

mod common;

use common::*;
use swapdesk_codec_win::{DecoderOptions, MfH264Decoder, MfH264Encoder, annexb};
use swapdesk_fakes::BarcodeReader;
use swapdesk_platform::{DecodedFrame, EncodedFrame, ErrorKind, MonoMicros, VideoDecoder, VideoEncoder};
use swapdesk_proto::Preset;

/// Encodes frames `0..n` and returns the encoded frames in output order (the encoder is flushed at the end).
fn encode_all(enc: &mut MfH264Encoder, n: usize, force_at: &[usize]) -> Vec<EncodedFrame> {
    let mut out = Vec::new();
    for i in 0..n {
        out.extend(enc.encode(&frame(i), force_at.contains(&i)).unwrap_or_else(|e| panic!("encode {i}: {e}")));
    }
    out.extend(enc.flush().expect("flush"));
    out
}

/// The index of the synthetic frame an encoded frame came from, by its timestamp.
fn source_index(e: &EncodedFrame) -> usize {
    (0..64)
        .find(|&i| captured_at(i) == e.captured_at)
        .unwrap_or_else(|| panic!("unknown timestamp {:?}", e.captured_at))
}

/// Decodes in order, ids counting from 0; returns the decoded frames.
fn decode_all(dec: &mut MfH264Decoder, encoded: &[EncodedFrame]) -> Vec<DecodedFrame> {
    let mut out = Vec::new();
    for (id, e) in encoded.iter().enumerate() {
        if let Some(d) =
            dec.decode_settled(&header(e, id as u32), &e.annex_b).unwrap_or_else(|err| panic!("decode {id}: {err}"))
        {
            out.push(d);
        }
    }
    out
}

fn assert_exact(d: &DecodedFrame, expected_index: usize, who: &str) {
    let reading = BarcodeReader::new()
        .read_decoded(d)
        .unwrap_or_else(|| panic!("{who}: barcode unreadable on frame {}", d.frame_id));
    assert_eq!(reading.counter, counter(expected_index), "{who}: counter of frame {}", d.frame_id);
    assert_eq!(reading.captured_at, captured_at(expected_index), "{who}: timestamp of frame {}", d.frame_id);
    assert_eq!(d.captured_at, captured_at(expected_index), "{who}: header timestamp of frame {}", d.frame_id);
    assert_eq!((d.width, d.height), (W, H), "{who}: size");
}

#[test]
fn the_pc_offers_the_software_encoder() {
    let list = candidates();
    assert!(list.iter().any(|c| !c.hardware), "software MFT missing: {list:?}");
    println!("encoders: {list:?}");
}

fn roundtrip_with(preset: Preset, n: usize, expect_all: bool) {
    for c in candidates() {
        let who = format!("{} ({preset:?})", c.name);
        let mut enc = encoder(c);
        let info = enc.configure(&config(c, preset, 60, 20_000)).unwrap_or_else(|e| panic!("{who}: {e}"));
        assert_eq!(info.hardware, c.hardware, "{who}");
        let encoded = encode_all(&mut enc, n, &[]);
        assert!(!encoded.is_empty(), "{who}: no output");
        assert!(encoded[0].keyframe && encoded[0].config_change, "{who}: first frame is a keyframe with config");
        for e in &encoded {
            assert!(
                e.annex_b.starts_with(&[0, 0, 0, 1]) || e.annex_b.starts_with(&[0, 0, 1]),
                "{who}: Annex B start code"
            );
            if e.keyframe {
                assert!(annexb::contains(&e.annex_b, annexb::NAL_SPS), "{who}: keyframe carries SPS");
                assert!(annexb::contains(&e.annex_b, annexb::NAL_PPS), "{who}: keyframe carries PPS");
            }
            assert_eq!((e.width, e.height), (W as u16, H as u16));
        }
        if expect_all {
            assert_eq!(encoded.len(), n, "{who}: one output per input");
        }
        let mut dec = MfH264Decoder::new(DecoderOptions { adapter: c.adapter, ..Default::default() });
        let decoded = decode_all(&mut dec, &encoded);
        assert!(!decoded.is_empty(), "{who}: nothing decoded");
        assert_eq!(decoded.len(), encoded.len(), "{who}: one picture per access unit (no reordering)");
        for (d, e) in decoded.iter().zip(&encoded) {
            assert_exact(d, source_index(e), &who);
        }
        println!(
            "{who}: {} frames, {} bytes, decoded through {:?}",
            decoded.len(),
            encoded.iter().map(|e| e.annex_b.len()).sum::<usize>(),
            dec.mft_name()
        );
    }
}

#[test]
fn barcode_reads_back_exactly_in_automatica() {
    roundtrip_with(Preset::Auto, 16, true);
}

#[test]
fn barcode_reads_back_exactly_in_texto_nitido() {
    // Texto nítido encodes at most 30 of the 60 frames per second: half of the frames are dropped by timestamp.
    for c in candidates() {
        let mut enc = encoder(c);
        enc.configure(&config(c, Preset::Text, 60, 20_000)).expect("configure");
        assert_eq!(enc.effective().map(|(fps, _)| fps), Some(30), "{}: coded rate", c.name);
        assert_eq!(enc.effective().map(|(_, kbps)| kbps), Some(40_000), "{}: doubled bitrate", c.name);
        let encoded = encode_all(&mut enc, 24, &[]);
        assert!((11..=13).contains(&encoded.len()), "{}: {} frames of 24 at 30 fps", c.name, encoded.len());
        let mut dec = MfH264Decoder::new(DecoderOptions { adapter: c.adapter, ..Default::default() });
        let decoded = decode_all(&mut dec, &encoded);
        assert_eq!(decoded.len(), encoded.len(), "{}", c.name);
        for (d, e) in decoded.iter().zip(&encoded) {
            assert_exact(d, source_index(e), &format!("{} (Texto nítido)", c.name));
        }
        // The kept frames are every other one.
        let kept: Vec<usize> = encoded.iter().map(source_index).collect();
        assert!(kept.windows(2).all(|w| w[1] - w[0] == 2), "{}: kept {kept:?}", c.name);
    }
}

#[test]
fn barcode_reads_back_exactly_in_movimento_fluido() {
    roundtrip_with(Preset::Motion, 16, true);
}

#[test]
fn a_forced_keyframe_arrives_within_one_frame() {
    for c in candidates() {
        let who = &c.name;
        let mut enc = encoder(c);
        enc.configure(&config(c, Preset::Auto, 60, 20_000)).expect("configure");
        let encoded = encode_all(&mut enc, 14, &[7]);
        assert_eq!(encoded.len(), 14, "{who}");
        let keys: Vec<usize> = encoded.iter().filter(|e| e.keyframe).map(source_index).collect();
        assert_eq!(keys.first(), Some(&0), "{who}: first frame is a keyframe");
        assert!(keys.iter().any(|&i| i == 7 || i == 8), "{who}: keyframes at {keys:?}, forced at 7");
        let late: Vec<usize> = keys.iter().copied().filter(|&i| i != 0 && i != 7 && i != 8).collect();
        assert!(late.is_empty(), "{who}: unexpected keyframes at {late:?} (infinite GOP)");
        let at7 = encoded.iter().find(|e| source_index(e) == 7).expect("frame 7");
        println!("{who}: forced at 7, keyframes at {keys:?}, frame 7 keyframe: {}", at7.keyframe);
        // The forced keyframe repeats the parameter sets.
        for e in encoded.iter().filter(|e| e.keyframe) {
            assert!(annexb::contains(&e.annex_b, annexb::NAL_SPS), "{who}: SPS on every keyframe");
        }
    }
}

fn mean_bytes(frames: &[EncodedFrame]) -> f64 {
    frames.iter().map(|e| e.annex_b.len() as f64).sum::<f64>() / frames.len().max(1) as f64
}

#[test]
fn a_bitrate_change_takes_effect() {
    for c in candidates() {
        let who = &c.name;
        let mut enc = encoder(c);
        enc.configure(&config(c, Preset::Auto, 60, 30_000)).expect("configure");
        let mut all = Vec::new();
        for i in 0..96 {
            all.extend(enc.encode(&frame(i % 64), false).expect("encode"));
        }
        let high = mean_bytes(&all[32..]);
        enc.set_rate(2_000, 60).expect("set_rate");
        let mut low_frames = Vec::new();
        for i in 0..96 {
            low_frames.extend(enc.encode(&frame(i % 64), false).expect("encode"));
        }
        if c.hardware {
            // Followed live: no rebuild, so no new SPS/PPS and no keyframe.
            assert!(
                !low_frames.iter().any(|e| e.config_change || e.keyframe),
                "{who}: the rate change rebuilt the encoder"
            );
        }
        let low = mean_bytes(&low_frames[48..]);
        let kbps = |bytes: f64| bytes * 8.0 * 60.0 / 1000.0;
        println!(
            "{who}: 30000 kbps asked -> {:.0} kbps measured; 2000 kbps asked -> {:.0} kbps measured",
            kbps(high),
            kbps(low)
        );
        let limit = if c.hardware { 0.5 } else { 0.7 };
        assert!(low < high * limit, "{who}: {high:.0} bytes per frame before, {low:.0} after");
        // And back up.
        enc.set_rate(30_000, 60).expect("set_rate up");
        let mut up_frames = Vec::new();
        for i in 0..96 {
            up_frames.extend(enc.encode(&frame(i % 64), false).expect("encode"));
        }
        let up = mean_bytes(&up_frames[48..]);
        assert!(up > low * 2.0, "{who}: {low:.0} bytes per frame low, {up:.0} after raising");
    }
}

/// The host's rate control raises the target by 30 % a second from 20 Mbps to the 80 Mbps ceiling while the link is
/// clean (swapdesk-video `RateConfig`: `ramp`): a hardware encoder follows the steps live, with no new SPS/PPS and
/// no keyframe (the NVIDIA MFT used to rebuild at every step). One rebuild in the whole ramp is what an MFT whose
/// peak cannot change while it streams needs (Quick Sync: `rate_limits`, set by `configure` alone): the mean passes
/// the peak it was configured with once, on the way from 20 to 80 Mbps.
#[test]
fn the_rising_rate_of_a_session_is_followed_live_on_hardware() {
    for c in candidates().iter().filter(|c| c.hardware) {
        let who = &c.name;
        let mut enc = encoder(c);
        enc.configure(&config(c, Preset::Auto, 60, 20_000)).expect("configure");
        let mut first = Vec::new();
        for i in 0..32 {
            first.extend(enc.encode(&frame(i % 64), false).expect("encode"));
        }
        let start = mean_bytes(&first[8..]);
        let mut kbps = 20_000u32;
        let mut later = Vec::new();
        for step in 0..8 {
            kbps = (kbps * 13 / 10).min(80_000);
            enc.set_rate(kbps, 60).expect("set_rate");
            for i in 0..16 {
                later.extend(enc.encode(&frame((step * 16 + i) % 64), false).expect("encode"));
            }
        }
        let rebuilds = later.iter().filter(|e| e.config_change || e.keyframe).count();
        assert!(rebuilds <= 1, "{who}: the rate steps rebuilt the encoder {rebuilds} times");
        let end = mean_bytes(&later[later.len() - 12..]);
        println!(
            "{who}: 20000 -> {kbps} kbps in 8 steps, {start:.0} -> {end:.0} bytes per frame, {rebuilds} rebuild(s)"
        );
        assert!(end > start * 1.15, "{who}: {start:.0} bytes per frame at 20 Mbps, {end:.0} at {kbps} kbps");
    }
}

#[test]
fn the_software_mft_works_when_hardware_is_excluded() {
    let c = software_candidate();
    let mut enc = swapdesk_codec_win::MfH264Encoder::new(swapdesk_codec_win::EncoderOptions {
        choice: swapdesk_codec_win::MftChoice::SoftwareOnly,
        ..Default::default()
    });
    let info = enc.configure(&config(&c, Preset::Text, 60, 12_000)).expect("software configure");
    assert!(!info.hardware, "{info:?}");
    assert_eq!(info.profile, swapdesk_proto::H264Profile::High);
    let summary = enc.mft().expect("mft").clone();
    assert!(!summary.hardware && !summary.asynchronous && !summary.d3d_input, "{summary:?}");
    let encoded = encode_all(&mut enc, 8, &[]);
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter: c.adapter, dxva: false, ..Default::default() });
    let decoded = decode_all(&mut dec, &encoded);
    assert_eq!(decoded.len(), encoded.len());
    for (d, e) in decoded.iter().zip(&encoded) {
        assert_exact(d, source_index(e), "software encoder, software decoder");
    }
}

#[test]
fn the_decoder_recovers_after_a_lost_frame_once_a_keyframe_arrives() {
    for c in candidates() {
        let who = &c.name;
        let mut enc = encoder(c);
        enc.configure(&config(c, Preset::Auto, 60, 20_000)).expect("configure");
        let encoded = encode_all(&mut enc, 30, &[20]);
        assert_eq!(encoded.len(), 30, "{who}");
        assert!(source_index(&encoded[20]) == 20 && encoded[20].keyframe, "{who}: keyframe at 20");

        // Case 1: the session sees the gap, calls reset, and drops frames until the keyframe.
        let mut dec = MfH264Decoder::new(DecoderOptions { adapter: c.adapter, ..Default::default() });
        for (id, e) in encoded.iter().enumerate().take(10) {
            let d = dec.decode_settled(&header(e, id as u32), &e.annex_b).expect("decode").expect("picture");
            assert_exact(&d, id, who);
        }
        // Frame 10 is lost.
        dec.reset();
        assert!(dec.waiting_for_keyframe());
        for (id, e) in encoded.iter().enumerate().take(20).skip(11) {
            assert!(
                dec.decode_settled(&header(e, id as u32), &e.annex_b).expect("decode").is_none(),
                "{who}: frame {id} before the keyframe"
            );
        }
        for (id, e) in encoded.iter().enumerate().skip(20) {
            let d = dec
                .decode_settled(&header(e, id as u32), &e.annex_b)
                .expect("decode")
                .unwrap_or_else(|| panic!("{who}: picture {id}"));
            assert_exact(&d, id, who);
        }

        // Case 2: nobody calls reset; the decoder notices the hole in the frame ids itself.
        let mut dec = MfH264Decoder::new(DecoderOptions { adapter: c.adapter, ..Default::default() });
        for id in (0..10).chain(11..30) {
            let e = &encoded[id];
            let got = dec.decode_settled(&header(e, id as u32), &e.annex_b).expect("decode");
            if (11..20).contains(&id) {
                assert!(got.is_none(), "{who}: frame {id} after the gap shown");
            } else {
                assert_exact(&got.unwrap_or_else(|| panic!("{who}: picture {id}")), id, who);
            }
        }
    }
}

#[test]
fn intra_refresh_is_used_where_the_mft_takes_it() {
    use swapdesk_codec_win::{EncoderOptions, MftChoice};
    for c in candidates() {
        let who = &c.name;
        let mut enc = swapdesk_codec_win::MfH264Encoder::new(EncoderOptions {
            choice: MftChoice::Clsid(c.clsid.clone()),
            intra_refresh_frames: Some(30),
            ..Default::default()
        });
        enc.configure(&config(c, Preset::Auto, 60, 20_000)).expect("configure");
        let outcome = enc.settings().iter().find(|s| s.name == "AVEncVideoGradualIntraRefresh").cloned();
        println!("{who}: intra refresh outcome {outcome:?}");
        let Some(outcome) = outcome else { continue };
        if !outcome.applied {
            assert!(!outcome.supported || outcome.hresult.is_some(), "{who}");
            continue;
        }
        let encoded = encode_all(&mut enc, 64, &[]);
        assert_eq!(encoded.len(), 64, "{who}");
        let keys: Vec<usize> = encoded.iter().filter(|e| e.keyframe).map(source_index).collect();
        assert_eq!(keys, vec![0], "{who}: intra refresh replaces periodic keyframes, got keyframes at {keys:?}");
        let mut dec = MfH264Decoder::new(DecoderOptions { adapter: c.adapter, ..Default::default() });
        let decoded = decode_all(&mut dec, &encoded);
        assert_eq!(decoded.len(), 64, "{who}");
        for (d, e) in decoded.iter().zip(&encoded) {
            assert_exact(d, source_index(e), who);
        }
    }
}

#[test]
fn gpu_textures_go_in_and_nv12_textures_come_out() {
    use swapdesk_codec_win::{DecoderOutput, EncoderOptions, MftChoice, TextureUploader};
    use swapdesk_platform::Pixels;
    use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

    for c in candidates() {
        let who = &c.name;
        let uploader = TextureUploader::new(c.adapter).expect("uploader");
        let mut enc = swapdesk_codec_win::MfH264Encoder::new(EncoderOptions {
            choice: MftChoice::Clsid(c.clsid.clone()),
            device: Some(uploader.device()),
            ..Default::default()
        });
        enc.configure(&config(c, Preset::Auto, 60, 20_000)).expect("configure on the capture device");
        let mut encoded = Vec::new();
        for i in 0..8 {
            let gpu_frame = uploader.upload(&frame(i)).expect("upload");
            encoded.extend(enc.encode(&gpu_frame, false).expect("encode GPU frame"));
        }
        encoded.extend(enc.flush().expect("flush"));
        assert_eq!(encoded.len(), 8, "{who}");

        // CPU readback decode: the picture is exact.
        let mut dec = MfH264Decoder::new(DecoderOptions { device: Some(uploader.device()), ..Default::default() });
        let decoded = decode_all(&mut dec, &encoded);
        assert_eq!(decoded.len(), 8, "{who}");
        for (d, e) in decoded.iter().zip(&encoded) {
            assert_exact(d, source_index(e), who);
        }

        // GPU output: an NV12 texture of exactly the picture size on the same device.
        let mut dec = MfH264Decoder::new(DecoderOptions {
            output: DecoderOutput::Gpu,
            device: Some(uploader.device()),
            ..Default::default()
        });
        let mut seen = 0;
        for (id, e) in encoded.iter().enumerate() {
            let d = dec.decode_settled(&header(e, id as u32), &e.annex_b).expect("decode").expect("picture");
            let Pixels::Gpu(surface) = &d.pixels else { panic!("{who}: expected a GPU surface") };
            let tex = surface.0.downcast_ref::<ID3D11Texture2D>().expect("an ID3D11Texture2D");
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: GetDesc only fills the struct.
            unsafe { tex.GetDesc(&mut desc) };
            assert_eq!((desc.Width, desc.Height, desc.Format), (W, H, DXGI_FORMAT_NV12), "{who}");
            seen += 1;
        }
        assert_eq!(seen, 8);

        // A frame from another device is refused with a clear error, not a crash.
        let other = TextureUploader::new(c.adapter).expect("second uploader");
        let foreign = other.upload(&frame(0)).expect("upload");
        let e = enc.encode(&foreign, false).expect_err("texture of another device");
        assert_eq!(e.kind, ErrorKind::Unsupported, "{who}: {e}");
    }
}

#[test]
fn the_encoder_refuses_bad_configurations_and_unconfigured_use() {
    let c = software_candidate();
    let mut enc = encoder(&c);
    let mut cfg = config(&c, Preset::Auto, 60, 8_000);
    cfg.width = 1921;
    assert_eq!(enc.configure(&cfg).expect_err("odd width").kind, ErrorKind::InvalidInput);
    cfg.width = W;
    cfg.fps = 0;
    assert_eq!(enc.configure(&cfg).expect_err("zero fps").kind, ErrorKind::InvalidInput);
    let mut fresh = encoder(&c);
    assert_eq!(fresh.encode(&frame(0), false).expect_err("not configured").kind, ErrorKind::Closed);
    assert_eq!(fresh.set_rate(1000, 30).expect_err("not configured").kind, ErrorKind::Closed);
    assert!(fresh.flush().expect("flush of nothing").is_empty());
    let _ = MonoMicros(0);
}
