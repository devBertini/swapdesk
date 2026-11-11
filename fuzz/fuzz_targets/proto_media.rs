//! Media headers a peer sends: video, audio, cursor (swapdesk-proto `Wire::decode`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    use swapdesk_proto::Wire;
    let _ = swapdesk_proto::VideoHeader::decode(data);
    let _ = swapdesk_proto::AudioHeader::decode(data);
    let _ = swapdesk_proto::CursorShapeHeader::decode(data);
    let _ = swapdesk_proto::CursorShape::decode(data);
});
