//! Stage W1.15 (L2 of the 2026-11-08 evaluation): the Media Foundation encoder asks for a peak bitrate and a three-frame
//! buffer next to the mean, on configure and on a live bitrate change, and runs on when an MFT refuses either.
#![cfg(windows)]

mod common;

use common::{candidates, config, encoder, frame};
use swapdesk_codec_win::MfH264Encoder;
use swapdesk_platform::VideoEncoder;
use swapdesk_proto::Preset;

/// The value the encoder asked for under `name` and whether the MFT took it.
fn asked(enc: &MfH264Encoder, name: &str, who: &str) -> (u32, bool) {
    let o = enc.settings().iter().find(|o| o.name == name).unwrap_or_else(|| panic!("{who}: no {name} outcome"));
    (o.value.parse().unwrap_or_else(|_| panic!("{who}: {name} value {:?}", o.value)), o.applied)
}

#[test]
fn peak_bitrate_and_buffer_follow_the_mean_on_configure_and_on_a_live_change() {
    for c in candidates() {
        let who = format!("{} ({})", c.name, if c.hardware { "hardware" } else { "software" });
        // Auto runs a VBR mode and Motion runs CBR where the MFT takes it; an MFT without CBR falls back to VBR (peak 2x).
        for preset in [Preset::Auto, Preset::Motion] {
            let mut enc = encoder(c);
            enc.configure(&config(c, preset, 60, 20_000)).unwrap_or_else(|e| panic!("{who}: configure: {e}"));
            let (max, max_ok) = asked(&enc, "AVEncCommonMaxBitRate", &who);
            let (buf, buf_ok) = asked(&enc, "AVEncCommonBufferSize", &who);
            assert!([20_000_000, 40_000_000].contains(&max), "{who} {preset:?}: peak {max}");
            if preset == Preset::Auto {
                assert!(
                    max == 40_000_000 || enc.rate_modes().iter().all(|(m, ok)| !ok || *m == 0),
                    "{who}: VBR peak is 2x"
                );
            }
            assert_eq!(buf, 1_000_000, "{who} {preset:?}: three frames of 20 Mbit/s at 60 fps");
            println!("{who} {preset:?}: peak {max} (taken {max_ok}), buffer {buf} (taken {buf_ok})");
            for i in 0..4 {
                enc.encode(&frame(i), false).unwrap_or_else(|e| panic!("{who}: encode {i}: {e}"));
            }
            enc.set_rate(10_000, 60).unwrap_or_else(|e| panic!("{who}: set_rate: {e}"));
            let (max2, max2_ok) = asked(&enc, "AVEncCommonMaxBitRate", &who);
            let (buf2, buf2_ok) = asked(&enc, "AVEncCommonBufferSize", &who);
            // A lower mean under the peak is a live change that leaves the limits alone (Quick Sync fails on a new buffer
            // size and emits a keyframe on a new peak); an MFT rebuilt for the change gets the limits of the new mean.
            assert!(
                [10_000_000, 20_000_000].contains(&max2) || max2 == max,
                "{who} {preset:?}: peak after the change {max2}"
            );
            assert!([500_000, 1_000_000].contains(&buf2), "{who} {preset:?}: buffer after the change {buf2}");
            println!(
                "{who} {preset:?} after set_rate(10000): peak {max2} (taken {max2_ok}), buffer {buf2} (taken {buf2_ok})"
            );
        }
    }
}
