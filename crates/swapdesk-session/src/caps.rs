//! Caps per session (PROTOCOL.md 5.3 and the defaults of DECISIONS "Re-pairing"): 4 terminals, 8 transfers; chat
//! messages are counted and never refused (each side keeps the newest 500). The session counts what flows through
//! it; a request over a cap is answered `BUSY` and nothing is done.

use std::collections::HashSet;

use swapdesk_proto::Message;

#[derive(Debug, Clone, Copy)]
pub struct Caps {
    pub max_terminals: usize,
    pub max_transfers: usize,
    pub max_chat: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self { max_terminals: 4, max_transfers: 8, max_chat: 500 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapVerdict {
    Ok,
    /// Over a cap (or a duplicate id): refuse with `BUSY`.
    Busy,
}

#[derive(Debug, Default)]
pub struct CapTracker {
    terminals: HashSet<u32>,
    transfers: HashSet<u32>,
    chat: usize,
}

impl CapTracker {
    pub fn terminals(&self) -> usize {
        self.terminals.len()
    }

    pub fn transfers(&self) -> usize {
        self.transfers.len()
    }

    pub fn chat(&self) -> usize {
        self.chat
    }

    /// A message arrived from the peer: counts it, or says it is over a cap (and then counts nothing).
    pub fn on_incoming(&mut self, caps: &Caps, msg: &Message) -> CapVerdict {
        match msg {
            Message::TermOpen(m) => {
                if self.terminals.contains(&m.term_id) || self.terminals.len() >= caps.max_terminals {
                    return CapVerdict::Busy;
                }
                self.terminals.insert(m.term_id);
            }
            // A `FileOffer` that answers our `FileGet` (or follows our own offer) reuses its id.
            Message::FileOffer(m) => {
                if !self.transfers.contains(&m.transfer_id) {
                    if self.transfers.len() >= caps.max_transfers {
                        return CapVerdict::Busy;
                    }
                    self.transfers.insert(m.transfer_id);
                }
            }
            Message::FileGet(m) => {
                if self.transfers.contains(&m.transfer_id) || self.transfers.len() >= caps.max_transfers {
                    return CapVerdict::Busy;
                }
                self.transfers.insert(m.transfer_id);
            }
            // Chat is counted and never refused: `max_chat` is how many messages each side keeps (the chat's own
            // ring drops the oldest), not how many a session may ever carry. Refusing the 501st ended the chat of
            // a long session for good, silently.
            Message::ChatMessage(_) => self.chat = self.chat.saturating_add(1),
            _ => self.release(msg),
        }
        CapVerdict::Ok
    }

    /// A message of ours goes out: counts it where it frees or uses a slot. Never `Busy` today.
    pub fn on_outgoing(&mut self, _caps: &Caps, msg: &Message) -> CapVerdict {
        match msg {
            Message::ChatMessage(_) => self.chat = self.chat.saturating_add(1),
            Message::FileOffer(m) => {
                self.transfers.insert(m.transfer_id);
            }
            _ => self.release(msg),
        }
        CapVerdict::Ok
    }

    /// A `TermOpen` the cap counted was refused by its handler afterwards (D13 declined, nobody present, no helper):
    /// its id is free again, or four refusals would shut the session out of terminals.
    pub fn release_terminal(&mut self, term_id: u32) {
        self.terminals.remove(&term_id);
    }

    fn release(&mut self, msg: &Message) {
        match msg {
            Message::TermClose(m) => {
                self.terminals.remove(&m.term_id);
            }
            Message::TermExited(m) => {
                self.terminals.remove(&m.term_id);
            }
            Message::FileVerified(m) => {
                self.transfers.remove(&m.transfer_id);
            }
            Message::FileDecline(m) => {
                self.transfers.remove(&m.transfer_id);
            }
            Message::FileCancel(m) => {
                self.transfers.remove(&m.transfer_id);
            }
            // A `FileGet` or `FileOffer` answered with a non-fatal `Error` (the file cannot be opened, the other
            // user said no, the receiver's disk failed) is over: its slot is free again, or eight such answers
            // would shut the session out of transfers.
            Message::Error(e) if !e.fatal && matches!(e.about_type, 0x0401 | 0x040B) => {
                self.transfers.remove(&e.about_id);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_proto::*;

    fn open(id: u32) -> Message {
        Message::TermOpen(TermOpen { term_id: id, cols: 80, rows: 24, admin: false })
    }

    fn get(id: u32) -> Message {
        Message::FileGet(FileGet { transfer_id: id, path: "x".into() })
    }

    fn chat(id: u32) -> Message {
        Message::ChatMessage(ChatMessage { msg_id: id, text: "hi".into(), sent_at_ms: 0 })
    }

    #[test]
    fn the_fifth_terminal_is_refused_and_closing_one_frees_a_slot() {
        let caps = Caps::default();
        let mut t = CapTracker::default();
        for id in 1..=4 {
            assert_eq!(t.on_incoming(&caps, &open(id)), CapVerdict::Ok);
        }
        assert_eq!(t.on_incoming(&caps, &open(5)), CapVerdict::Busy);
        assert_eq!(t.terminals(), 4);
        assert_eq!(t.on_incoming(&caps, &open(2)), CapVerdict::Busy, "a duplicate id");
        t.on_incoming(&caps, &Message::TermClose(TermClose { term_id: 2 }));
        assert_eq!(t.on_incoming(&caps, &open(5)), CapVerdict::Ok);
        t.on_outgoing(&caps, &Message::TermExited(TermExited { term_id: 1, exit_code: None }));
        assert_eq!(t.terminals(), 3);
    }

    #[test]
    fn the_ninth_transfer_is_refused_and_finishing_one_frees_a_slot() {
        let caps = Caps::default();
        let mut t = CapTracker::default();
        for id in 1..=8 {
            assert_eq!(t.on_incoming(&caps, &get(id)), CapVerdict::Ok);
        }
        assert_eq!(t.on_incoming(&caps, &get(9)), CapVerdict::Busy);
        assert_eq!(
            t.on_incoming(
                &caps,
                &Message::FileOffer(FileOffer { transfer_id: 9, name: "a".into(), size: 1, mtime_ms: 0, dest: None })
            ),
            CapVerdict::Busy
        );
        t.on_incoming(&caps, &Message::FileVerified(FileVerified { transfer_id: 3, ok: true }));
        assert_eq!(t.on_incoming(&caps, &get(9)), CapVerdict::Ok);
        // The offer that answers a request already counted does not take a second slot.
        t.on_outgoing(&caps, &Message::FileCancel(FileCancel { transfer_id: 1 }));
        assert_eq!(t.transfers(), 7);
    }

    #[test]
    fn a_file_request_answered_with_an_error_gives_its_slot_back() {
        // Review of 22/11, F1: a `FileGet` the host could not serve kept its slot, so eight of them shut the
        // session out of transfers.
        let caps = Caps::default();
        let mut t = CapTracker::default();
        let error = |about_type: u16, about_id: u32, fatal: bool| {
            Message::Error(ErrorMessage { code: ErrorCode::NotFound, fatal, about_type, about_id })
        };
        for round in 0..3 {
            for id in 1..=8 {
                assert_eq!(t.on_incoming(&caps, &get(id)), CapVerdict::Ok, "round {round}, request {id}");
            }
            assert_eq!(t.on_incoming(&caps, &get(9)), CapVerdict::Busy);
            for id in 1..=8 {
                // The host's own answer on its way out.
                assert_eq!(t.on_outgoing(&caps, &error(0x040B, id, false)), CapVerdict::Ok);
            }
            assert_eq!(t.transfers(), 0);
        }
        // An error about anything else frees nothing.
        t.on_incoming(&caps, &get(1));
        t.on_outgoing(&caps, &error(0x0501, 1, false));
        assert_eq!(t.transfers(), 1);
        // The refusal of an offer frees the offer's slot on the side that hears it.
        t.on_incoming(&caps, &error(0x0401, 1, false));
        assert_eq!(t.transfers(), 0);
    }

    /// Review of 22/11/2026, E4: the 501st message of a session, counted over both directions, was refused, and so
    /// was every one after it: the chat of a long session stopped for good. The 500 of F-CHAT-01 is how many are
    /// kept, which the chat's own ring does.
    #[test]
    fn a_long_sessions_chat_is_never_refused_for_how_much_was_said() {
        let caps = Caps::default();
        let mut t = CapTracker::default();
        for id in 0..600 {
            assert_eq!(t.on_incoming(&caps, &chat(id)), CapVerdict::Ok);
            assert_eq!(t.on_outgoing(&caps, &chat(id)), CapVerdict::Ok);
        }
        assert_eq!(t.chat(), 1200);
    }
}
