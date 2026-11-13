//! IPC messages between the interface, the helpers and the service (swapdesk-ipc `decode_message`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = swapdesk_ipc::decode_message(data);
});
