//! The chat proof across machines (F-XCHAT-01): a message each way through a live session, with its read state.
//!
//! The viewer side sends and receives through its UI backend (`ChatSend`, `ChatMarkRead`, `UiEvent::Chat`) exactly as
//! the chat drawer does; the host's scripted user ([`crate::host`]) reads what its interface received and sends when
//! told. The orchestrator carries the two across the machines through files in the run folder:
//!
//! 1. this side sends an accented text and writes it to `chat-viewer-text`; the host's script logs
//!    `HOST_CHAT_GOT <text>` (what its interface received) and marks it read; the orchestrator writes that text into
//!    `chat-host-got`. This side compares it with what it sent and waits for the read state (`ChatRead`) to come back;
//! 2. this side picks a reply text and writes it to `chat-host-say-text`; the orchestrator copies it into the host's
//!    `chat-host-say` file, which makes the host's script send it; this side waits for it as a `UiEvent::Chat` that is
//!    not its own, then marks it read (the host's script logs `HOST_CHAT_READ_BY_PEER`).
//!
//! Markers: `CHAT_TO_HOST_OK`, `CHAT_READ_BY_HOST_OK`, `CHAT_FROM_HOST_OK`.

use std::path::Path;
use std::time::Duration;

use swapdesk_ui::{Command, Reply, UiEvent};

use crate::pump::{Pump, wait_file};

const REPLY: Duration = Duration::from_secs(10);

/// The text this side sends: accented (precomposed, as typed), with a nonce so an old message is never taken for it.
pub fn viewer_text(nonce: u128) -> String {
    format!("Ol\u{e1}, \u{e9} o espectador {nonce}: a\u{e7}\u{e3}o, cora\u{e7}\u{e3}o, n\u{e3}o \u{fc}")
}

/// The text the host is made to send back.
pub fn host_text(nonce: u128) -> String {
    format!("Resposta do anfitri\u{e3}o {nonce}: j\u{e1}, at\u{e9}, voc\u{ea} \u{2014} ok")
}

/// Runs the two steps; `Err` names the first that failed.
pub fn chat_round(
    pump: &mut Pump<'_>,
    session_id: u64,
    dir: &Path,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<(), String> {
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());

    // 1. A message to the host, which must show it and mark it read.
    let text = viewer_text(nonce);
    std::fs::write(dir.join("chat-viewer-text"), &text).map_err(|e| e.to_string())?;
    let Reply::Chat(sent) = pump.ask(Command::ChatSend { session_id, text: text.clone() }, REPLY)? else {
        return Err("ChatSend did not reply the stored message".into());
    };
    if !sent.mine || sent.text != text {
        return Err("the stored message is not the one sent".into());
    }
    let got = wait_file(&dir.join("chat-host-got"), "what the host's interface received", timeout)?;
    if got != text {
        return Err(format!("the host's interface received {got:?}, not {text:?}"));
    }
    log("CHAT_TO_HOST_OK");
    let msg_id = sent.msg_id;
    pump.wait("the host to read the message", timeout, |e| match e {
        UiEvent::ChatRead { session_id: s, up_to } if *s == session_id && *up_to >= msg_id => Some(()),
        _ => None,
    })?;
    log("CHAT_READ_BY_HOST_OK");

    // 2. A message from the host, which the orchestrator makes it send.
    let reply = host_text(nonce);
    std::fs::write(dir.join("chat-host-say-text"), &reply).map_err(|e| e.to_string())?;
    let (id, shown) = pump.wait("the host's message", timeout, |e| match e {
        UiEvent::Chat { session_id: s, message } if *s == session_id && !message.mine && message.text == reply => {
            Some((message.msg_id, message.read))
        }
        _ => None,
    })?;
    log(&format!("CHAT_FROM_HOST_OK read_on_arrival={shown}"));
    pump.ask(Command::ChatMarkRead { session_id, up_to: id }, REPLY)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_texts_are_accented_distinct_and_have_no_edge_whitespace() {
        for t in [viewer_text(1), host_text(1)] {
            assert!(!t.is_ascii(), "accented: {t}");
            assert_eq!(t, t.trim(), "wait_file trims what it reads");
            assert!(!t.contains('\n'));
        }
        assert_ne!(viewer_text(1), host_text(1));
        assert_ne!(viewer_text(1), viewer_text(2));
    }
}
