//! Chat (F-CHAT-01, `ChatMessage` and `ChatRead`): messages live in memory only, with their read state, at most 500
//! per session, and vanish when the session ends. [`ChatLog`] is the state; [`ChatHandler`] plugs it into a session.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use swapdesk_proto::{ChatMessage, ChatRead, Message};
use swapdesk_session::{ChannelHandler, SendError, SessionCtx, SessionEnd};

use crate::{ExtrasEvent, ExtrasSink, lock};

/// Messages kept per session (F-CHAT-01; the session's own cap refuses more, this is a second guard).
pub const MAX_MESSAGES: usize = 500;
/// `ChatMessage.text` limit in bytes (PROTOCOL.md 5.3).
pub const MAX_TEXT: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatEntry {
    pub msg_id: u32,
    pub text: String,
    pub sent_at_ms: u64,
    /// Sent by this side.
    pub mine: bool,
    /// Mine: the peer read it. Theirs: this side's user read it.
    pub read: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatError {
    Empty,
    TooLong,
    /// The session's 500-message cap.
    Busy,
    /// The session is gone.
    Closed,
}

#[derive(Default)]
pub struct ChatLog {
    entries: VecDeque<ChatEntry>,
    last_id: u32,
}

impl ChatLog {
    pub fn entries(&self) -> Vec<ChatEntry> {
        self.entries.iter().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.last_id = 0;
    }

    fn push(&mut self, e: ChatEntry) {
        if self.entries.len() >= MAX_MESSAGES {
            self.entries.pop_front();
        }
        self.entries.push_back(e);
    }

    /// Reserves the next id and records the message as sent.
    fn push_outgoing(&mut self, text: String, now_ms: u64) -> u32 {
        self.last_id = self.last_id.wrapping_add(1);
        let msg_id = self.last_id;
        self.push(ChatEntry { msg_id, text, sent_at_ms: now_ms, mine: true, read: false });
        msg_id
    }

    fn remove_outgoing(&mut self, msg_id: u32) {
        self.entries.retain(|e| !(e.mine && e.msg_id == msg_id));
    }

    /// Records a message from the peer; a repeated id is ignored.
    pub fn push_incoming(&mut self, m: &ChatMessage) -> bool {
        if self.entries.iter().any(|e| !e.mine && e.msg_id == m.msg_id) {
            return false;
        }
        self.push(ChatEntry {
            msg_id: m.msg_id,
            text: m.text.clone(),
            sent_at_ms: m.sent_at_ms,
            mine: false,
            read: false,
        });
        true
    }

    /// The peer read our messages up to `up_to`.
    pub fn read_by_peer(&mut self, up_to: u32) {
        self.entries.iter_mut().filter(|e| e.mine && e.msg_id <= up_to).for_each(|e| e.read = true);
    }

    /// This side's user read the peer's messages: returns the highest id now read, to send as `ChatRead`, when
    /// anything was unread.
    pub fn mark_all_read(&mut self) -> Option<u32> {
        let mut top = None;
        for e in self.entries.iter_mut().filter(|e| !e.mine && !e.read) {
            e.read = true;
            top = top.max(Some(e.msg_id));
        }
        top
    }

    pub fn unread(&self) -> usize {
        self.entries.iter().filter(|e| !e.mine && !e.read).count()
    }
}

pub struct ChatHandler {
    log: Mutex<ChatLog>,
    sink: Arc<dyn ExtrasSink>,
}

impl ChatHandler {
    pub fn new(sink: Arc<dyn ExtrasSink>) -> Arc<Self> {
        Arc::new(Self { log: Mutex::new(ChatLog::default()), sink })
    }

    /// Sends a message (Enter in the chat box). `now_ms` is the Unix time in ms. Returns the message id.
    pub async fn send(&self, ctx: &SessionCtx, text: &str, now_ms: u64) -> Result<u32, ChatError> {
        if text.trim().is_empty() {
            return Err(ChatError::Empty);
        }
        if text.len() > MAX_TEXT {
            return Err(ChatError::TooLong);
        }
        let msg_id = lock(&self.log).push_outgoing(text.to_owned(), now_ms);
        match ctx.send(ChatMessage { msg_id, text: text.to_owned(), sent_at_ms: now_ms }).await {
            Ok(()) => Ok(msg_id),
            Err(e) => {
                lock(&self.log).remove_outgoing(msg_id);
                Err(if e == SendError::Busy { ChatError::Busy } else { ChatError::Closed })
            }
        }
    }

    /// The chat panel is open and showing everything: tells the peer.
    pub async fn mark_read(&self, ctx: &SessionCtx) {
        let top = lock(&self.log).mark_all_read();
        if let Some(up_to) = top {
            let _ = ctx.send(ChatRead { up_to }).await;
        }
    }

    pub fn entries(&self) -> Vec<ChatEntry> {
        lock(&self.log).entries()
    }

    pub fn unread(&self) -> usize {
        lock(&self.log).unread()
    }
}

impl ChannelHandler for ChatHandler {
    fn name(&self) -> &'static str {
        "chat"
    }

    fn messages(&self) -> &'static [u16] {
        &[0x0301, 0x0302]
    }

    fn on_message(&self, _ctx: &SessionCtx, msg: Message) {
        match msg {
            Message::ChatMessage(m) => {
                if lock(&self.log).push_incoming(&m) {
                    self.sink.event(ExtrasEvent::ChatReceived { msg_id: m.msg_id });
                }
            }
            Message::ChatRead(ChatRead { up_to }) => {
                lock(&self.log).read_by_peer(up_to);
                self.sink.event(ExtrasEvent::ChatReadByPeer { up_to });
            }
            _ => {}
        }
    }

    fn on_start(&self, _ctx: &SessionCtx) {
        lock(&self.log).clear();
    }

    fn on_end(&self, _ctx: &SessionCtx, _end: &SessionEnd) {
        lock(&self.log).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incoming(id: u32) -> ChatMessage {
        ChatMessage { msg_id: id, text: format!("m{id}"), sent_at_ms: 1 }
    }

    #[test]
    fn read_state_both_ways_and_duplicates() {
        let mut log = ChatLog::default();
        let a = log.push_outgoing("hi".into(), 10);
        let b = log.push_outgoing("there".into(), 11);
        assert_eq!((a, b), (1, 2));
        assert!(log.push_incoming(&incoming(7)));
        assert!(!log.push_incoming(&incoming(7)));
        assert_eq!(log.unread(), 1);
        log.read_by_peer(1);
        let e = log.entries();
        assert_eq!((e[0].read, e[1].read), (true, false));
        assert_eq!(log.mark_all_read(), Some(7));
        assert_eq!((log.unread(), log.mark_all_read()), (0, None));
    }

    #[test]
    fn at_most_500_messages_are_kept() {
        let mut log = ChatLog::default();
        for i in 1..=600 {
            log.push_incoming(&incoming(i));
        }
        let e = log.entries();
        assert_eq!((e.len(), e[0].msg_id), (500, 101));
    }
}
