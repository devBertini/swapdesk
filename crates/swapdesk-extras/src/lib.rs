//! The session extras of stage 22: clipboard sync (F-CLIP-01), chat (F-CHAT-01), the Wake-on-LAN sender (F-WAKE-01)
//! and the reconnect policy (F-NET-01). No `unsafe`.
//!
//! * [`clipboard::ClipboardHandler`] and [`chat::ChatHandler`] are `swapdesk_session::ChannelHandler`s: the service
//!   registers them in the `Registry` it gives to the host and to the viewer.
//! * [`wake`] sends the magic packet through a [`wake::WakeSocket`] (the real one is TTL 1 and bound to the approved
//!   interface) and waits at most 60 s for the device to appear.
//! * [`reconnect`] decides when a lost session is retried and drives the retries through a dial closure.
//!
//! Neither clipboard content nor chat text is ever logged or put in an event: [`ExtrasEvent`] carries sizes and ids.

pub mod chat;
pub mod clipboard;
pub mod reconnect;
pub mod wake;

use std::sync::{Mutex, MutexGuard};

use swapdesk_proto::{ClipKind, ClipSkip};

/// What the UI hears of the extras. Never the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtrasEvent {
    /// An item was not sent: by this side (`from_peer` false) or the peer told us.
    ClipboardSkipped { from_peer: bool, reason: ClipSkip },
    /// An item from the peer was written to the local clipboard.
    ClipboardReceived { kind: ClipKind, len: usize },
    /// A chat message arrived.
    ChatReceived { msg_id: u32 },
    /// The peer read our messages up to this id.
    ChatReadByPeer { up_to: u32 },
}

pub trait ExtrasSink: Send + Sync + 'static {
    fn event(&self, event: ExtrasEvent);
}

/// A sink that drops everything.
pub struct NullSink;

impl ExtrasSink for NullSink {
    fn event(&self, _event: ExtrasEvent) {}
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
