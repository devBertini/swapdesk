//! The registry feature crates plug into: swap, terminal, files and extras each register a [`ChannelHandler`] for the
//! message types and stream kinds they own. The session core checks direction, permission and caps first; a handler
//! only ever sees what is allowed.

use std::collections::HashMap;
use std::sync::Arc;

use quinn::{RecvStream, SendStream};
use swapdesk_proto::{Desktop, Message, PermissionSet, StreamKind};

use crate::ctx::{SessionCtx, SessionEnd};
use crate::policy::is_core_type;

/// A stream a handler was given: the kind is already read. `send` is `Some` for a bidirectional stream (terminal).
pub struct IncomingStream {
    pub kind: StreamKind,
    pub recv: RecvStream,
    pub send: Option<SendStream>,
}

/// A feature's side of a session. Every method runs on the session's state machine task: it must not block or wait.
/// Anything slow goes into a task started with [`SessionCtx::spawn`] or [`SessionCtx::spawn_gated`], which end
/// with the session (and, for `spawn_gated`, with the permission).
pub trait ChannelHandler: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// The message type IDs this handler receives (`Message::type_id`). Core types cannot be claimed.
    fn messages(&self) -> &'static [u16] {
        &[]
    }

    /// The stream kinds this handler receives.
    fn streams(&self) -> &'static [StreamKind] {
        &[]
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message);

    /// The default drops the stream, which stops it.
    fn on_stream(&self, _ctx: &SessionCtx, _stream: IncomingStream) {}

    /// The session's grants changed from `old` to `now`. What `old` held and `now` does not (compare with
    /// `contains`) was revoked: anything the handler runs for it must stop within 1 s (F-PERM-02); tasks started with
    /// `spawn_gated` already have.
    fn on_permissions(&self, _ctx: &SessionCtx, _old: PermissionSet, _now: PermissionSet) {}

    /// The host's desktop changed or the pause flag moved (`HostState`).
    fn on_state(&self, _ctx: &SessionCtx, _desktop: Desktop, _paused: bool) {}

    /// The session was granted and is live.
    fn on_start(&self, _ctx: &SessionCtx) {}

    /// The session ended; a handler drops its per-session state.
    fn on_end(&self, _ctx: &SessionCtx, _end: &SessionEnd) {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// A core message type (handshake, keepalive, grants, `HostState`, errors, pairing).
    Reserved(u16),
    DuplicateMessage(u16),
    DuplicateStream(StreamKind),
    /// A type ID the protocol does not define.
    UnknownMessage(u16),
}

#[derive(Clone, Default)]
pub struct Registry {
    messages: HashMap<u16, Arc<dyn ChannelHandler>>,
    streams: HashMap<StreamKind, Arc<dyn ChannelHandler>>,
    handlers: Vec<Arc<dyn ChannelHandler>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler for everything it declares. Nothing is registered when any entry is refused.
    pub fn register(&mut self, handler: Arc<dyn ChannelHandler>) -> Result<(), RegisterError> {
        for &id in handler.messages() {
            if is_core_type(id) {
                return Err(RegisterError::Reserved(id));
            }
            if !Message::is_known_type(u64::from(id)) {
                return Err(RegisterError::UnknownMessage(id));
            }
            if self.messages.contains_key(&id) {
                return Err(RegisterError::DuplicateMessage(id));
            }
        }
        for &kind in handler.streams() {
            if self.streams.contains_key(&kind) {
                return Err(RegisterError::DuplicateStream(kind));
            }
        }
        for &id in handler.messages() {
            self.messages.insert(id, Arc::clone(&handler));
        }
        for &kind in handler.streams() {
            self.streams.insert(kind, Arc::clone(&handler));
        }
        self.handlers.push(handler);
        Ok(())
    }

    pub(crate) fn for_message(&self, type_id: u16) -> Option<&Arc<dyn ChannelHandler>> {
        self.messages.get(&type_id)
    }

    pub(crate) fn for_stream(&self, kind: StreamKind) -> Option<&Arc<dyn ChannelHandler>> {
        self.streams.get(&kind)
    }

    pub(crate) fn handlers(&self) -> &[Arc<dyn ChannelHandler>] {
        &self.handlers
    }
}
