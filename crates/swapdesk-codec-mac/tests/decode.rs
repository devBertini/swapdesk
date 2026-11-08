//! The VideoToolbox decoder against two small H.264 Annex B streams made with x264 (`tests/fixtures`, Constrained
//! Baseline, no B-frames, an IDR every 4th frame, SPS/PPS repeated before each IDR, 8 frames each):
//!
//! - `a_160x96.h264`: 160x96, left half red, right half black.
//! - `b_128x90.h264`: 128x90 (the height is not a multiple of 16, so the SPS crops), top half blue, bottom half black.
//!
//! They were made with `ffmpeg -f lavfi -i "color=c=black:s=160x96:r=30,drawbox=x=0:y=0:w=80:h=96:color=red:t=fill"
//! -frames:v 8 -c:v libx264 -profile:v baseline -bf 0 -g 4 -x264-params "keyint=4:min-keyint=4:scenecut=0:repeat-headers=1:slices=1:aud=0"
//! -f h264` (and the matching command for stream B). Run on the MacBook with
//! `scripts/mac/remote.sh test -p swapdesk-codec-mac`.
#![cfg(target_os = "macos")]

use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth};
use swapdesk_codec_mac::annexb::{self, access_units};
use swapdesk_codec_mac::{DecoderOptions, DecoderOutput, VtH264Decoder};
use swapdesk_platform::{DecodedFrame, ErrorKind, Pixels, VideoDecoder};
use swapdesk_proto::{Codec, VideoHeader};

const STREAM_A: &[u8] = include_bytes!("fixtures/a_160x96.h264");
const STREAM_B: &[u8] = include_bytes!("fixtures/b_128x90.h264");

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`.
const NV12_VIDEO_RANGE: u32 = 0x3432_3076;

/// The access units of a stream.
fn units(stream: &'static [u8]) -> Vec<&'static [u8]> {
    access_units(stream).into_iter().map(|r| &stream[r]).collect()
}

fn header(frame_id: u32, unit: &[u8], size: (u16, u16)) -> VideoHeader {
    VideoHeader {
        keyframe: annexb::is_idr(unit),
        config: annexb::parameter_sets(unit).is_some(),
        discontinuity: false,
        display_id: 3,
        frame_id,
        capture_us: 1_000_000 + u64::from(frame_id) * 33_333,
        encode_us: 100,
        width: size.0,
        height: size.1,
        codec: Codec::H264,
        payload_len: unit.len() as u32,
    }
}

fn decoder(output: DecoderOutput) -> VtH264Decoder {
    VtH264Decoder::new(DecoderOptions { output })
}

fn bgra(frame: &DecodedFrame) -> (&[u8], usize) {
    match &frame.pixels {
        Pixels::Cpu { stride, data } => (data.as_slice(), *stride as usize),
        Pixels::Gpu(_) => panic!("expected CPU pixels"),
    }
}

fn pixel(frame: &DecodedFrame, x: usize, y: usize) -> [u8; 3] {
    let (data, stride) = bgra(frame);
    let o = y * stride + x * 4;
    [data[o + 2], data[o + 1], data[o]] // R, G, B
}

fn assert_mostly(pixel: [u8; 3], red: bool, green: bool, blue: bool, what: &str) {
    let [r, g, b] = pixel;
    let hi = |v: u8, on: bool| if on { v >= 170 } else { v <= 70 };
    assert!(
        hi(r, red) && hi(g, green) && hi(b, blue),
        "{what}: rgb {pixel:?}, wanted high r/g/b = {red}/{green}/{blue}"
    );
}

fn decode_all(dec: &mut VtH264Decoder, stream: &'static [u8], first_id: u32, size: (u16, u16)) -> Vec<DecodedFrame> {
    units(stream)
        .into_iter()
        .enumerate()
        .map(|(i, unit)| {
            dec.decode(&header(first_id + i as u32, unit, size), unit)
                .expect("decode")
                .unwrap_or_else(|| panic!("picture {i}"))
        })
        .collect()
}

#[test]
fn the_fixtures_are_what_the_tests_assume() {
    for stream in [STREAM_A, STREAM_B] {
        let u = units(stream);
        assert_eq!(u.len(), 8);
        let keys: Vec<usize> = u.iter().enumerate().filter(|(_, x)| annexb::is_idr(x)).map(|(i, _)| i).collect();
        assert_eq!(keys, [0, 4]);
        assert!(annexb::parameter_sets(u[0]).is_some() && annexb::parameter_sets(u[4]).is_some());
    }
}

#[test]
fn a_known_stream_decodes_to_one_picture_per_access_unit() {
    let mut dec = decoder(DecoderOutput::Gpu);
    assert!(dec.waiting_for_keyframe());
    let frames = decode_all(&mut dec, STREAM_A, 0, (160, 96));
    assert_eq!(frames.len(), 8);
    assert!(!dec.waiting_for_keyframe());
    assert_eq!(dec.sessions_created(), 1);
    for (i, f) in frames.iter().enumerate() {
        assert_eq!((f.width, f.height), (160, 96), "frame {i}");
        assert_eq!(f.frame_id, i as u32);
        assert_eq!(f.display.0, 3);
        assert_eq!(
            f.captured_at.0,
            1_000_000 + i as u64 * 33_333,
            "the header's capture time travels with the picture"
        );
        let Pixels::Gpu(surface) = &f.pixels else { panic!("GPU output expected") };
        let buffer = surface.0.downcast_ref::<CFRetained<CVPixelBuffer>>().expect("a CVPixelBuffer");
        assert_eq!((CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer)), (160, 96));
        assert_eq!(CVPixelBufferGetPixelFormatType(buffer), NV12_VIDEO_RANGE);
    }
    assert!(dec.last_decode_us() > 0);
}

#[test]
fn the_pictures_have_the_colours_and_the_orientation_of_the_source() {
    let mut dec = decoder(DecoderOutput::CpuBgra);
    let frames = decode_all(&mut dec, STREAM_A, 0, (160, 96));
    for (i, f) in frames.iter().enumerate() {
        let (data, stride) = bgra(f);
        assert_eq!(stride, 160 * 4);
        assert_eq!(data.len(), 160 * 4 * 96);
        assert_mostly(pixel(f, 10, 48), true, false, false, &format!("frame {i} left half"));
        assert_mostly(pixel(f, 150, 48), false, false, false, &format!("frame {i} right half"));
        assert_mostly(pixel(f, 10, 2), true, false, false, &format!("frame {i} top left"));
        assert_mostly(pixel(f, 10, 93), true, false, false, &format!("frame {i} bottom left"));
    }
}

#[test]
fn a_picture_is_cropped_to_the_size_in_the_sps() {
    let mut dec = decoder(DecoderOutput::CpuBgra);
    let frames = decode_all(&mut dec, STREAM_B, 0, (128, 90));
    for (i, f) in frames.iter().enumerate() {
        assert_eq!((f.width, f.height), (128, 90), "frame {i}");
        assert_eq!(bgra(f).0.len(), 128 * 4 * 90);
        assert_mostly(pixel(f, 10, 10), false, false, true, &format!("frame {i} top"));
        assert_mostly(pixel(f, 10, 80), false, false, false, &format!("frame {i} bottom"));
    }
}

#[test]
fn a_new_size_in_a_keyframe_makes_a_new_session() {
    let mut dec = decoder(DecoderOutput::Gpu);
    let a = decode_all(&mut dec, STREAM_A, 0, (160, 96));
    assert!(a.iter().all(|f| (f.width, f.height) == (160, 96)));
    assert_eq!(dec.sessions_created(), 1);
    // Stream B follows A with the next frame ids; its first unit is a keyframe with its own SPS/PPS.
    let b = decode_all(&mut dec, STREAM_B, 8, (128, 90));
    assert!(b.iter().all(|f| (f.width, f.height) == (128, 90)));
    assert_eq!(dec.sessions_created(), 2, "one session per parameter-set change, not per keyframe");
}

#[test]
fn repeated_identical_parameter_sets_keep_the_session() {
    let mut dec = decoder(DecoderOutput::Gpu);
    // Units 0 and 4 both carry the SPS/PPS; the second keyframe must not rebuild the session.
    decode_all(&mut dec, STREAM_A, 0, (160, 96));
    assert_eq!(dec.sessions_created(), 1);
}

#[test]
fn a_gap_in_the_frame_ids_waits_for_the_next_keyframe() {
    let u = units(STREAM_A);
    let mut dec = decoder(DecoderOutput::Gpu);
    for id in 0..2 {
        assert!(
            dec.decode(&header(id, u[id as usize], (160, 96)), u[id as usize]).expect("decode").is_some(),
            "picture {id}"
        );
    }
    // Frame 2 is lost: frame 3 arrives with a hole before it.
    let got = dec.decode(&header(3, u[3], (160, 96)), u[3]).expect("decode");
    assert!(got.is_none(), "the picture after a hole would be wrong");
    assert!(dec.waiting_for_keyframe(), "the receiver is told to ask for a keyframe");
    // Every non-keyframe is dropped until the keyframe (unit 4); then pictures flow again.
    assert!(dec.decode(&header(4, u[4], (160, 96)), u[4]).expect("decode").is_some(), "keyframe");
    assert!(!dec.waiting_for_keyframe());
    for id in 5..8 {
        let f = dec
            .decode(&header(id, u[id as usize], (160, 96)), u[id as usize])
            .expect("decode")
            .unwrap_or_else(|| panic!("picture {id}"));
        assert_eq!(f.frame_id, id);
    }
}

#[test]
fn reset_drops_everything_until_a_keyframe_and_keeps_the_session() {
    let u = units(STREAM_A);
    let mut dec = decoder(DecoderOutput::Gpu);
    dec.decode(&header(0, u[0], (160, 96)), u[0]).expect("decode").expect("picture");
    dec.decode(&header(1, u[1], (160, 96)), u[1]).expect("decode").expect("picture");
    dec.reset();
    assert!(dec.waiting_for_keyframe());
    for id in 2..4 {
        assert!(
            dec.decode(&header(id, u[id as usize], (160, 96)), u[id as usize]).expect("decode").is_none(),
            "frame {id} before the keyframe"
        );
    }
    for id in 4..8 {
        assert!(
            dec.decode(&header(id, u[id as usize], (160, 96)), u[id as usize]).expect("decode").is_some(),
            "picture {id}"
        );
    }
    assert_eq!(dec.sessions_created(), 1, "reset keeps the session");
}

#[test]
fn a_discontinuity_flag_on_a_delta_frame_waits_for_a_keyframe() {
    let u = units(STREAM_A);
    let mut dec = decoder(DecoderOutput::Gpu);
    dec.decode(&header(0, u[0], (160, 96)), u[0]).expect("decode").expect("picture");
    let mut h = header(1, u[1], (160, 96));
    h.discontinuity = true;
    assert!(dec.decode(&h, u[1]).expect("decode").is_none());
    assert!(dec.waiting_for_keyframe());
}

#[test]
fn nothing_but_a_keyframe_starts_a_stream() {
    let u = units(STREAM_A);
    let mut dec = decoder(DecoderOutput::Gpu);
    for id in 1..4 {
        assert!(
            dec.decode(&header(id, u[id as usize], (160, 96)), u[id as usize]).expect("decode").is_none(),
            "delta frame {id} first"
        );
    }
    assert!(dec.waiting_for_keyframe());
    assert_eq!(dec.sessions_created(), 0, "no session before a keyframe");
    assert!(dec.decode(&header(4, u[4], (160, 96)), u[4]).expect("decode").is_some());
}

#[test]
fn a_keyframe_without_parameter_sets_is_refused_until_some_were_seen() {
    let u = units(STREAM_A);
    // Unit 0 without its SPS/PPS: from the first IDR slice on.
    let idr_at = annexb::scan(u[0]).into_iter().find(|n| n.kind == annexb::NAL_IDR).expect("idr").start;
    let bare = &u[0][idr_at..];
    let mut dec = decoder(DecoderOutput::Gpu);
    let Err(e) = dec.decode(&header(0, bare, (160, 96)), bare) else { panic!("no SPS/PPS yet: an error was expected") };
    assert_eq!(e.kind, ErrorKind::InvalidInput);
    assert!(dec.waiting_for_keyframe());

    // After a stream with parameter sets, a later keyframe without them decodes with the remembered ones.
    let mut dec = decoder(DecoderOutput::Gpu);
    dec.decode(&header(0, u[0], (160, 96)), u[0]).expect("decode").expect("picture");
    let idr_at = annexb::scan(u[4]).into_iter().find(|n| n.kind == annexb::NAL_IDR).expect("idr").start;
    let bare = &u[4][idr_at..];
    dec.reset();
    let f = dec.decode(&header(4, bare, (160, 96)), bare).expect("decode").expect("picture");
    assert_eq!((f.width, f.height), (160, 96));
    assert_eq!(dec.sessions_created(), 1);
}

#[test]
fn damaged_data_never_panics_and_the_next_keyframe_recovers() {
    let u = units(STREAM_A);
    let mut dec = decoder(DecoderOutput::Gpu);
    dec.decode(&header(0, u[0], (160, 96)), u[0]).expect("decode").expect("picture");
    // A delta frame whose slice is garbage after the NAL header.
    let mut bad = vec![0, 0, 0, 1, 0x41];
    bad.extend((0..40u8).map(|i| i.wrapping_mul(37).wrapping_add(11)));
    let outcome = dec.decode(&header(1, &bad, (160, 96)), &bad);
    // VideoToolbox may conceal it or fail it; either way the decoder is not left in a state it cannot leave.
    let _ = outcome;
    for id in 2..4 {
        let _ = dec.decode(&header(id, u[id as usize], (160, 96)), u[id as usize]);
    }
    let f = dec.decode(&header(4, u[4], (160, 96)), u[4]).expect("keyframe decodes").expect("picture");
    assert_eq!((f.width, f.height), (160, 96));
    assert!(!dec.waiting_for_keyframe());
}

#[test]
fn a_config_only_unit_gives_no_picture() {
    let u = units(STREAM_A);
    let params_only = &u[0][..annexb::scan(u[0]).into_iter().find(|n| n.kind == annexb::NAL_IDR).expect("idr").start];
    let mut dec = decoder(DecoderOutput::Gpu);
    let mut h = header(0, params_only, (160, 96));
    h.keyframe = true;
    assert!(dec.decode(&h, params_only).expect("decode").is_none());
    assert!(dec.waiting_for_keyframe(), "parameter sets alone are not a keyframe picture");
    assert_eq!(dec.sessions_created(), 0);
    // The remembered sets serve the keyframe that follows without them.
    let idr_at = params_only.len();
    let bare = &u[0][idr_at..];
    let f = dec.decode(&header(1, bare, (160, 96)), bare).expect("decode").expect("picture");
    assert_eq!((f.width, f.height), (160, 96));
}

#[test]
fn the_decoder_is_usable_through_dyn() {
    let u = units(STREAM_A);
    let mut boxed: Box<dyn VideoDecoder> = Box::new(VtH264Decoder::default());
    let got = boxed.decode(&header(0, u[0], (160, 96)), u[0]).expect("decode");
    assert!(got.is_some());
    boxed.reset();
}
