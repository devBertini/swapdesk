//! CONTRACTS.md C2.3: a type that holds a nonce, the pairing code, a payload body, shell output, chat text or
//! clipboard content must not `#[derive(Debug)]`: a derived `Debug` prints every byte, and one future `{:?}` in a log
//! line would leak it. Each of these types has a hand-written `Debug` that shows sizes only.
//!
//! This is a source guard, not a compile-fail test (a compile-fail check needs `trybuild`, a new crate): it reads the
//! attributes written above each type and fails when one of them derives `Debug`. A new secret-holding type is added
//! to `GUARDED`.

use std::path::Path;

/// (file under `crates/`, type name). Every type of finding B10, plus the ones the same fix covered.
const GUARDED: &[(&str, &str)] = &[
    ("swapdesk-proto/src/pairing.rs", "CommitmentInput"),
    ("swapdesk-proto/src/pairing.rs", "SasInput"),
    ("swapdesk-proto/src/pairing.rs", "Sas"),
    ("swapdesk-proto/src/media.rs", "CursorShape"),
    ("swapdesk-proto/src/media.rs", "ClipboardPayload"),
    ("swapdesk-ipc/src/redact.rs", "Payload"),
    ("swapdesk-video/src/sender.rs", "Frame"),
    ("swapdesk-video/src/receiver.rs", "ReceivedFrame"),
    ("swapdesk-host/src/service/shared.rs", "MediaEv"),
    ("swapdesk-host/src/service/helper_link.rs", "HelperEvent"),
    ("swapdesk-term/src/host.rs", "TermEvent"),
    ("swapdesk-extras/src/chat.rs", "ChatEntry"),
    ("swapdesk-platform/src/lib.rs", "ClipContent"),
    ("swapdesk-audio/src/jitter.rs", "QueuedPacket"),
    ("swapdesk-ui/src/api.rs", "ChatView"),
    ("swapdesk-ui/src/api.rs", "Command"),
    ("swapdesk-platform/src/lib.rs", "CursorShape"),
    ("swapdesk-capture-win/src/pointer.rs", "Converted"),
    ("swapdesk-input-win/src/grab.rs", "Translated"),
];

/// The attribute and doc lines directly above the declaration of `name`, nearest first.
fn attributes_above(source: &str, name: &str) -> Vec<String> {
    let lines: Vec<&str> = source.lines().collect();
    let declares = |l: &str| {
        let l = l.trim_start();
        ["pub struct ", "pub enum ", "pub(crate) struct ", "pub(crate) enum ", "struct ", "enum "]
            .iter()
            .filter_map(|p| l.strip_prefix(p))
            .any(|rest| {
                rest.strip_prefix(name)
                    .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
            })
    };
    let at = lines.iter().position(|l| declares(l)).unwrap_or_else(|| panic!("no declaration of {name}"));
    lines[..at]
        .iter()
        .rev()
        .take_while(|l| {
            let l = l.trim_start();
            l.starts_with("#[") || l.starts_with("//")
        })
        .map(|l| (*l).to_string())
        .collect()
}

#[test]
fn no_secret_holding_type_derives_debug() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates directory");
    for (file, name) in GUARDED {
        let source = std::fs::read_to_string(crates.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        let attrs = attributes_above(&source, name);
        assert!(
            !attrs.iter().any(|l| l.contains("derive(") && l.contains("Debug")),
            "{name} in {file} derives Debug: write a `Debug` that shows sizes only (swapdesk_proto::RedactedLen)"
        );
        assert!(
            source.contains(&format!("Debug for {name} ")) || source.contains(&format!("Debug for {name}<")),
            "{name} in {file} has no hand-written Debug impl"
        );
    }
}

#[test]
fn the_guard_sees_a_derive_when_there_is_one() {
    let source =
        "/// doc\n#[derive(Debug, Clone)]\npub struct Leaky {\n    a: u8,\n}\n\n#[derive(Clone)]\npub struct Safe;\n";
    assert!(attributes_above(source, "Leaky").iter().any(|l| l.contains("derive(") && l.contains("Debug")));
    assert!(!attributes_above(source, "Safe").iter().any(|l| l.contains("Debug")));
}
