//! H.264 Annex B a peer sends: NAL scan and the SPS parser (swapdesk-codec-win `annexb.rs`, pure Rust).
#![no_main]

#[path = "../../crates/swapdesk-codec-win/src/annexb.rs"]
#[allow(dead_code)]
mod annexb;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = annexb::scan(data);
    let _ = annexb::parameter_sets(data);
    let _ = annexb::is_idr(data);
    let _ = annexb::sps_profile_level(data);
    let _ = annexb::parse_sps(data);
});
