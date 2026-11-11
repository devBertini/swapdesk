//! The release manifest and update package (swapdesk-update; the manifest is parsed only after its signature, but the parser must hold anyway).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = swapdesk_update::manifest::parse(data);
    let _ = swapdesk_update::package::Package::read(data);
});
