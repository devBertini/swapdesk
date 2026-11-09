//! Colour of the pictures a Windows host sends, decoded by VideoToolbox. The fixtures `tests/fixtures/*_patches.h264` are
//! six flat sRGB patches side by side (red, green, blue, greys 16, 128 and 235), 1280x720, two frames, encoded on the
//! Windows PC by NVENC (`nvenc`, which writes a BT.709 colour description in the VUI), the Quick Sync MFT (`qsv`, a VUI
//! with no colour description) and the Windows software MFT (`mfsoft`, no colour VUI), with
//! `cargo run -p swapdesk-codec-win --example color_fixtures -- crates/swapdesk-codec-mac/tests/fixtures`.
//!
//! What macOS colour-manages by is the colour attachments of the decoded `CVPixelBuffer` (the renderer's format
//! description is made from the buffer, `CMVideoFormatDescriptionCreateForImageBuffer`), so every host encoder must
//! come out with the same attachments and the same RGB. Measured on 2026-11-09: at HD sizes VideoToolbox takes a stream
//! without a colour description as BT.709 too; below HD (384x192) as SMPTE C / BT.601, so the BT.709 samples of the
//! MFTs decoded red as (233, 0, 2) there. Every Swapdesk display is HD or larger.
#![cfg(target_os = "macos")]

use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_video::{
    CVPixelBuffer, kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrixKey,
};
use swapdesk_codec_mac::annexb::{self, access_units};
use swapdesk_codec_mac::{DecoderOptions, DecoderOutput, VtH264Decoder};
use swapdesk_platform::{DecodedFrame, Pixels, VideoDecoder};
use swapdesk_proto::{Codec, VideoHeader};

const W: usize = 1280;
const H: usize = 720;
const PATCHES: [[u8; 3]; 6] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [16, 16, 16], [128, 128, 128], [235, 235, 235]];
const STREAMS: [(&str, &[u8]); 3] = [
    ("nvenc", include_bytes!("fixtures/nvenc_patches.h264")),
    ("qsv", include_bytes!("fixtures/qsv_patches.h264")),
    ("mfsoft", include_bytes!("fixtures/mfsoft_patches.h264")),
];

fn decode(stream: &[u8], output: DecoderOutput) -> Vec<DecodedFrame> {
    let mut dec = VtH264Decoder::new(DecoderOptions { output });
    let mut out = Vec::new();
    for (i, r) in access_units(stream).into_iter().enumerate() {
        let unit = &stream[r];
        let header = VideoHeader {
            keyframe: annexb::is_idr(unit),
            config: annexb::parameter_sets(unit).is_some(),
            discontinuity: false,
            display_id: 0,
            frame_id: i as u32,
            capture_us: 1_000_000 + i as u64 * 16_667,
            encode_us: 100,
            width: W as u16,
            height: H as u16,
            codec: Codec::H264,
            payload_len: unit.len() as u32,
        };
        out.extend(dec.decode(&header, unit).expect("decode"));
    }
    out
}

fn attachment(buffer: &CVPixelBuffer, key: &CFString) -> Option<String> {
    // SAFETY: a copy of one attachment of a live buffer; the mode pointer may be null.
    let value = unsafe { buffer.attachment(key, std::ptr::null_mut()) }?;
    Some(value.downcast_ref::<CFString>().map_or_else(|| "(not a string)".to_string(), |s| s.to_string()))
}

/// (primaries, transfer, matrix) attached to the first decoded picture.
fn tags(stream: &[u8]) -> [Option<String>; 3] {
    let frames = decode(stream, DecoderOutput::Gpu);
    let Pixels::Gpu(surface) = &frames[0].pixels else { panic!("GPU output expected") };
    let buffer = surface.0.downcast_ref::<CFRetained<CVPixelBuffer>>().expect("a CVPixelBuffer");
    // SAFETY: the statics are the framework's own.
    let keys =
        unsafe { [kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrixKey] };
    keys.map(|k| attachment(buffer, k))
}

/// The RGB at the centre of each patch of the last decoded picture (VideoToolbox converts to BGRA).
fn patch_colours(stream: &[u8]) -> Vec<[u8; 3]> {
    let frames = decode(stream, DecoderOutput::CpuBgra);
    let last = frames.last().expect("a picture");
    let Pixels::Cpu { stride, data } = &last.pixels else { panic!("CPU output expected") };
    let patch_w = W / PATCHES.len();
    (0..PATCHES.len())
        .map(|i| {
            let o = (H / 2) * *stride as usize + (i * patch_w + patch_w / 2) * 4;
            [data[o + 2], data[o + 1], data[o]]
        })
        .collect()
}

#[test]
fn every_host_encoder_decodes_with_the_same_colour_attachments() {
    let want = [Some("ITU_R_709_2".to_string()), Some("ITU_R_709_2".to_string()), Some("ITU_R_709_2".to_string())];
    for (name, stream) in STREAMS {
        let t = tags(stream);
        println!("{name}: primaries {:?}, transfer {:?}, matrix {:?}", t[0], t[1], t[2]);
        assert_eq!(t, want, "{name}: the colour attachments of a Swapdesk picture");
    }
}

#[test]
fn every_host_encoder_decodes_to_the_source_colours() {
    for (name, stream) in STREAMS {
        let got = patch_colours(stream);
        println!("{name}: {got:?}");
        for (want, got) in PATCHES.iter().zip(&got) {
            let off = want.iter().zip(got).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
            assert!(off <= 6, "{name}: {want:?} came back as {got:?}");
        }
    }
}
