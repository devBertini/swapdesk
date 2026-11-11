//! A file name a peer offers (swapdesk-files `validate_name`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    if let Ok(name) = std::str::from_utf8(data) {
        let _ = swapdesk_files::name::validate_name(name);
    }
});
