//! Control frames a peer sends, pairing messages included (swapdesk-proto `Message::decode_frame`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let mut rest = data;
    // A stream of frames: each decoded frame is cut off and the next one read, as the session's reader does.
    while let Ok((msg, used)) = swapdesk_proto::Message::decode_frame(rest) {
        let _ = msg.encode_frame();
        if used == 0 || used > rest.len() {
            break;
        }
        rest = &rest[used..];
    }
});
