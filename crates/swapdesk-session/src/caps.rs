//! Caps per session (PROTOCOL.md 5.3 and the defaults of DECISIONS "Re-pairing"): 4 terminals, 8 transfers, 500
//! chat messages. The session counts what flows through it; a request over a cap is answered `BUSY` and nothing is
//! done.

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
            Message::ChatMessage(_) => {
                if self.chat >= caps.max_chat {
                    return CapVerdict::Busy;
                }
                self.chat += 1;
            }
            _ => self.release(msg),
        }
        CapVerdict::Ok
    }

    /// A message of ours goes out: counts it where it frees or uses a slot. `Busy` only for chat past its cap.
    pub fn on_outgoing(&mut self, caps: &Caps, msg: &Message) -> CapVerdict {
        match msg {
            Message::ChatMessage(_) => {
                if self.chat >= caps.max_chat {
                    return CapVerdict::Busy;
                }
                self.chat += 1;
            }
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
    fn the_501st_chat_message_is_refused() {
        let caps = Caps::default();
        let mut t = CapTracker::default();
        for id in 0..250 {
            assert_eq!(t.on_incoming(&caps, &chat(id)), CapVerdict::Ok);
            assert_eq!(t.on_outgoing(&caps, &chat(id)), CapVerdict::Ok);
        }
        assert_eq!(t.chat(), 500);
        assert_eq!(t.on_incoming(&caps, &chat(999)), CapVerdict::Busy);
        assert_eq!(t.on_outgoing(&caps, &chat(999)), CapVerdict::Busy);
        assert_eq!(t.chat(), 500);
    }
}
