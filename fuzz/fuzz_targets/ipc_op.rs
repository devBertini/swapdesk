//! A reply body the interface reads, by op number (swapdesk-ipc `ReplyBody::decode`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let op = u16::from_le_bytes([data[0], data[1]]);
    let _ = swapdesk_ipc::ReplyBody::decode(op, &data[2..]);
});
