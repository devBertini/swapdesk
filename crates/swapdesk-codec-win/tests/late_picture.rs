//! The decoder never waits for a picture that is not out yet: `decode` returns at once and the picture comes on a later call (stage W1.1, C3).
#![cfg(windows)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use swapdesk_codec_win::{DecoderOptions, MfH264Decoder};
use swapdesk_platform::{DecodedFrame, EncodedFrame, VideoDecoder, VideoEncoder};
use swapdesk_proto::Preset;

/// A call that ends without a picture must not have waited for one.
const PROMPT: Duration = Duration::from_millis(2);

fn stream() -> Vec<EncodedFrame> {
    let c = software_candidate();
    let mut enc = encoder(&c);
    enc.configure(&config(&c, Preset::Auto, 60, 20_000)).expect("configure");
    let mut out = Vec::new();
    for i in 0..20 {
        out.extend(enc.encode(&frame(i), false).expect("encode"));
    }
    out.extend(enc.flush().expect("flush"));
    out
}

#[test]
fn decode_returns_at_once_without_a_picture_and_poll_collects_it_later() {
    let encoded = stream();
    let n = encoded.len();
    let mut dec = MfH264Decoder::new(DecoderOptions::default());
    let mut pictures: Vec<DecodedFrame> = Vec::new();
    let mut empty_calls = 0;
    for (id, e) in encoded.iter().enumerate() {
        let t = Instant::now();
        let got = dec.decode(&header(e, id as u32), &e.annex_b).expect("decode");
        let took = t.elapsed();
        match got {
            Some(d) => pictures.push(d),
            None => {
                empty_calls += 1;
                assert!(took < PROMPT, "frame {id}: decode returned nothing after {took:?}");
                assert!(dec.owes_picture(), "frame {id}: nothing returned and nothing owed");
            }
        }
    }
    // The last frame before a pause: whatever is still owed comes out through poll, bounded in time.
    let deadline = Instant::now() + Duration::from_secs(2);
    while dec.owes_picture() && Instant::now() < deadline {
        let t = Instant::now();
        if let Some(d) = dec.poll().expect("poll") {
            pictures.push(d);
        }
        assert!(t.elapsed() < Duration::from_millis(50), "poll blocked");
        std::thread::yield_now();
    }
    assert!(!dec.owes_picture(), "a picture stayed owed after 2 s");
    assert!(!pictures.is_empty(), "no picture at all out of {n} frames");
    let last = pictures.last().expect("a picture");
    assert_eq!(
        last.frame_id as usize,
        n - 1,
        "the last picture is the last frame (empty calls: {empty_calls}, late: {})",
        dec.late_pictures()
    );
    println!(
        "frames {n}, pictures {}, empty decode calls {empty_calls}, late pictures {}",
        pictures.len(),
        dec.late_pictures()
    );
}
