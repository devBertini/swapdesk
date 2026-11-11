//! The terminal output a host sends, fed to the viewer's terminal (swapdesk-termview `Term::feed`).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let _ = swapdesk_term::parse_term_header(data);
    let mut term = swapdesk_termview::Term::new(80, 24, 200);
    term.feed(data);
});
