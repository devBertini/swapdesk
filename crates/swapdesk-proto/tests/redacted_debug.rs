//! CONTRACTS.md C2.3: tokens, nonces, chat text, clipboard content and input records never reach a log line,
//! so their `Debug` output must not hold them.

use swapdesk_proto::*;

fn hex_marker() -> [u8; 16] {
    [0xAB; 16]
}

fn assert_hidden(dbg: &str, secrets: &[&str]) {
    for s in secrets {
        assert!(!dbg.contains(s), "{s:?} leaked in {dbg}");
    }
    assert!(dbg.contains("redacted"), "no redaction marker in {dbg}");
}

#[test]
fn tokens_nonces_and_bodies_are_redacted_in_debug() {
    // A [0xAB; 16] array prints as "171, 171, ..." under the derived Debug.
    let byte = "171";
    let resume = Resume { session_id: 9, token: hex_marker() };
    assert_hidden(&format!("{resume:?}"), &[byte]);
    assert!(format!("{resume:?}").contains("session_id: 9"));

    let accept = SwapAccept { request_id: 1, token: hex_marker() };
    assert_hidden(&format!("{accept:?}"), &[byte]);

    let chat = ChatMessage { msg_id: 1, text: "segredo-do-chat".into(), sent_at_ms: 5 };
    assert_hidden(&format!("{chat:?}"), &["segredo-do-chat"]);

    let reveal = PairReveal { nonce_i: [0xAB; 32] };
    assert_hidden(&format!("{reveal:?}"), &[byte]);

    let clip = ClipboardPayload::Text("senha123".into());
    assert_hidden(&format!("{clip:?}"), &["senha123"]);

    let text = InputEvent::Text("hunter2".into());
    assert_hidden(&format!("{text:?}"), &["hunter2"]);

    let key = InputEvent::Key {
        usage_page: USAGE_PAGE_KEYBOARD,
        usage: 0x2A_u16 * 3 + 1,
        down: true,
        modifiers: 0,
        locks: 0,
    };
    assert_hidden(&format!("{key:?}"), &["127", "usage:"]);
}
