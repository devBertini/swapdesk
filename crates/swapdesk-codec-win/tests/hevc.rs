//! HEVC on Windows (D39, F-VIEW-07): the hardware encoders (NVENC and Media Foundation) honour `EncoderConfig::codec`,
//! the decoder follows `VideoHeader::codec`, and the probe says what this PC can do. The round trips skip with a message
//! on a PC without HEVC hardware (encoder or decoder).
#![cfg(windows)]

mod common;

use common::*;
use swapdesk_codec_win::{
    DecoderOptions, EncoderOptions, MfH264Decoder, MfH264Encoder, MftChoice, NvencEncoder, NvencOptions, VENDOR_NVIDIA,
    annexb, hevc_probe, hevc_support, list_adapters,
};
use swapdesk_fakes::BarcodeReader;
use swapdesk_platform::{Codec, EncodedFrame, EncoderConfig, ErrorKind, VideoEncoder};
use swapdesk_proto::Preset;

fn hevc_config(adapter: swapdesk_platform::AdapterId) -> EncoderConfig {
    EncoderConfig { width: W, height: H, fps: 60, kbps: 20_000, preset: Preset::Auto, adapter, codec: Codec::Hevc }
}

#[test]
fn hevc_probe_reports_what_this_pc_can_do() {
    let probe = hevc_probe();
    println!("HEVC probe: {probe:#?}");
    let (enc, dec) = hevc_support();
    assert_eq!((enc, dec), (probe.encode_hw(), probe.decode_hw()));
    // The answer is cached: a second call is the same value.
    assert_eq!(hevc_support(), (enc, dec));
}

#[test]
fn hevc_from_the_software_encoder_is_unsupported() {
    let adapter = first_hardware_adapter();
    let mut enc = MfH264Encoder::new(EncoderOptions { choice: MftChoice::SoftwareOnly, ..Default::default() });
    let e = enc.configure(&hevc_config(adapter)).expect_err("the software MFT never encodes HEVC (D39)");
    assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
}

#[test]
fn hevc_by_nvenc_on_a_non_nvidia_adapter_is_unsupported() {
    let Some(other) =
        list_adapters().expect("adapters").into_iter().find(|a| !a.software && a.vendor_id != VENDOR_NVIDIA)
    else {
        println!("skipped: this PC has no non-NVIDIA hardware adapter");
        return;
    };
    let mut enc = NvencEncoder::new(NvencOptions::default());
    let e = enc.configure(&hevc_config(other.id)).expect_err("NVENC needs an NVIDIA adapter");
    assert_eq!(e.kind, ErrorKind::Unsupported, "{e}");
}

/// Encodes 24 frames, returns every encoded frame (flushed).
fn encode_stream(enc: &mut dyn VideoEncoder, n: usize) -> Vec<EncodedFrame> {
    let mut out = Vec::new();
    for i in 0..n {
        out.extend(enc.encode(&frame(i), false).unwrap_or_else(|e| panic!("encode {i}: {e}")));
    }
    out.extend(enc.flush().expect("flush"));
    out
}

/// Checks the HEVC stream shape, decodes it and reads the barcode of every picture back.
fn check_round_trip(who: &str, encoded: &[EncodedFrame], adapter: swapdesk_platform::AdapterId) {
    assert!(!encoded.is_empty(), "{who}: no output");
    assert!(encoded[0].keyframe && encoded[0].config_change, "{who}: first frame is a keyframe with config");
    for e in encoded {
        assert!(e.annex_b.starts_with(&[0, 0, 0, 1]) || e.annex_b.starts_with(&[0, 0, 1]), "{who}: Annex B start code");
        if e.keyframe {
            for (name, kind) in
                [("VPS", annexb::hevc::NAL_VPS), ("SPS", annexb::hevc::NAL_SPS), ("PPS", annexb::hevc::NAL_PPS)]
            {
                assert!(annexb::hevc::contains(&e.annex_b, kind), "{who}: keyframe carries {name}");
            }
            assert!(annexb::hevc::is_idr(&e.annex_b), "{who}: keyframe holds an IDR picture");
        }
    }
    let mut dec = MfH264Decoder::new(DecoderOptions { adapter, ..Default::default() });
    let mut pictures = 0;
    for (id, e) in encoded.iter().enumerate() {
        let header = header_of(e, id as u32, Codec::Hevc);
        if let Some(d) =
            dec.decode_settled(&header, &e.annex_b).unwrap_or_else(|err| panic!("{who}: decode {id}: {err}"))
        {
            let reading = BarcodeReader::new()
                .read_decoded(&d)
                .unwrap_or_else(|| panic!("{who}: barcode unreadable on frame {}", d.frame_id));
            let index = (0..64).find(|&i| captured_at(i) == e.captured_at).expect("known timestamp");
            assert_eq!(reading.counter, counter(index), "{who}: counter of frame {}", d.frame_id);
            assert_eq!((d.width, d.height), (W, H), "{who}: size");
            pictures += 1;
        }
    }
    assert!(pictures >= encoded.len() - 1, "{who}: {pictures} pictures out of {} frames", encoded.len());
}

#[test]
fn hevc_round_trip_through_every_hardware_encoder() {
    let probe = hevc_probe();
    if !probe.encode_hw() || !probe.decode_hw() {
        println!("skipped: this PC has no hardware HEVC encoder and decoder together: {probe:#?}");
        return;
    }
    let mut ran = 0;
    for adapter in list_adapters().expect("adapters").into_iter().filter(|a| !a.software) {
        let cfg = hevc_config(adapter.id);
        if adapter.vendor_id == VENDOR_NVIDIA {
            let mut nv = NvencEncoder::new(NvencOptions::default());
            if let Ok(info) = nv.configure(&cfg) {
                assert!(info.hardware && info.name.contains("HEVC"), "{info:?}");
                let encoded = encode_stream(&mut nv, 24);
                check_round_trip(&format!("NVENC HEVC on {}", adapter.name), &encoded, adapter.id);
                ran += 1;
            }
        }
        let mut mf = MfH264Encoder::new(EncoderOptions { choice: MftChoice::HardwareOnly, ..Default::default() });
        if let Ok(info) = mf.configure(&cfg) {
            assert!(info.hardware, "{info:?}");
            let encoded = encode_stream(&mut mf, 24);
            check_round_trip(&format!("{} on {}", info.name, adapter.name), &encoded, adapter.id);
            ran += 1;
        }
    }
    assert!(ran > 0, "the probe found a hardware encoder but none configured: {probe:#?}");
}
