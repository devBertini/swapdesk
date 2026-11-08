//! Clipboard sync (F-CLIP-01, PROTOCOL.md 7.5 and the `ClipboardSync` / `ClipboardSkipped` messages).
//!
//! [`ClipEngine`] is the logic over a `ClipboardProvider`, with no I/O: it polls the provider's change counter, decides
//! what goes out (text and PNG within the 7.5 limits; concealed content never; an item is only sent when both sides
//! switched sync on), and applies what comes in (the newest `seq` wins; what it writes does not echo back).
//! [`ClipboardHandler`] plugs the engine into a session: one unidirectional stream (kind 0x04) per item.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use swapdesk_platform::{ClipContent, ClipRead, ClipboardProvider, PlatformError};
use swapdesk_proto::{
    CLIPBOARD_HEADER_LEN, ClipKind, ClipSkip, ClipboardHeader, ClipboardItem, ClipboardPayload, ClipboardSkipped,
    ClipboardSync, MAX_CLIPBOARD_PNG, MAX_CLIPBOARD_TEXT, Message, PermissionSet, StreamKind, Wire,
};
use swapdesk_session::{ChannelHandler, IncomingStream, SessionCtx, SessionEnd};

use crate::{ExtrasEvent, ExtrasSink, lock};

/// How often the provider's change counter is looked at (the 1 s budget of F-CLIP-01 leaves room).
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// A clipboard stream that does not finish in this time is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// What a poll found to tell the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    Item(ClipboardItem),
    Skipped(ClipSkip),
}

pub struct ClipEngine<P> {
    provider: P,
    last_count: Option<u64>,
    out_seq: u32,
    in_seq: Option<u32>,
    local_on: bool,
    peer_on: bool,
}

impl<P: ClipboardProvider> ClipEngine<P> {
    /// Sync starts off on the peer's side until it says otherwise; the content already on the clipboard is not sent.
    pub fn new(mut provider: P, local_on: bool) -> Self {
        let last_count = provider.change_count().ok();
        Self { provider, last_count, out_seq: 0, in_seq: None, local_on, peer_on: false }
    }

    pub fn set_local(&mut self, on: bool) {
        self.local_on = on;
    }

    pub fn local_on(&self) -> bool {
        self.local_on
    }

    pub fn set_peer(&mut self, on: bool) {
        self.peer_on = on;
    }

    /// A new session: the peer's `seq` starts over and what is on the clipboard now stays local.
    pub fn new_session(&mut self) {
        self.in_seq = None;
        self.peer_on = false;
        self.last_count = self.provider.change_count().ok();
    }

    /// Looks at the clipboard once. A change while sync is off on either side is consumed without being sent.
    pub fn poll(&mut self) -> Option<Outgoing> {
        let count = self.provider.change_count().ok()?;
        if self.last_count == Some(count) {
            return None;
        }
        let first_look = self.last_count.is_none();
        self.last_count = Some(count);
        // The provider was out of reach until now (a helper or a UI not attached yet): what is on the clipboard at the
        // first look was there before the session and stays local.
        if first_look {
            return None;
        }
        if !(self.local_on && self.peer_on) {
            return None;
        }
        match self.provider.read().ok()? {
            ClipRead::Empty => None,
            ClipRead::Concealed => Some(Outgoing::Skipped(ClipSkip::Concealed)),
            ClipRead::Unsupported => Some(Outgoing::Skipped(ClipSkip::Unsupported)),
            ClipRead::Content(content) => {
                let payload = match content {
                    ClipContent::Text(t) if t.len() > MAX_CLIPBOARD_TEXT as usize => {
                        return Some(Outgoing::Skipped(ClipSkip::TooLarge));
                    }
                    ClipContent::Png(p) if p.len() > MAX_CLIPBOARD_PNG as usize => {
                        return Some(Outgoing::Skipped(ClipSkip::TooLarge));
                    }
                    ClipContent::Text(t) => ClipboardPayload::Text(t),
                    ClipContent::Png(p) => ClipboardPayload::Png(p),
                };
                self.out_seq = self.out_seq.wrapping_add(1);
                Some(Outgoing::Item(ClipboardItem { seq: self.out_seq, payload }))
            }
        }
    }

    /// Writes an item from the peer. `Ok(false)` when it is stale (not newer than the last one) or sync is off here.
    pub fn apply(&mut self, item: &ClipboardItem) -> Result<bool, PlatformError> {
        if !self.local_on {
            return Ok(false);
        }
        if let Some(last) = self.in_seq
            && (item.seq.wrapping_sub(last) as i32) <= 0
        {
            return Ok(false);
        }
        let content = match &item.payload {
            ClipboardPayload::Text(t) => ClipContent::Text(t.clone()),
            ClipboardPayload::Png(p) => ClipContent::Png(p.clone()),
        };
        self.provider.write(&content)?;
        self.in_seq = Some(item.seq);
        // What we just wrote is not a local copy: do not send it back.
        self.last_count = self.provider.change_count().ok();
        Ok(true)
    }
}

/// The clipboard feature of a machine. One clipboard serves the one live session.
pub struct ClipboardHandler<P> {
    engine: Arc<Mutex<ClipEngine<P>>>,
    sink: Arc<dyn ExtrasSink>,
}

impl<P: ClipboardProvider + Send + 'static> ClipboardHandler<P> {
    pub fn new(provider: P, sync_on: bool, sink: Arc<dyn ExtrasSink>) -> Arc<Self> {
        Arc::new(Self { engine: Arc::new(Mutex::new(ClipEngine::new(provider, sync_on))), sink })
    }

    /// The "Copiar e colar" switch: this side's own flag; the peer is told.
    pub fn set_enabled(&self, ctx: &SessionCtx, on: bool) {
        lock(&self.engine).set_local(on);
        let _ = ctx.try_send(ClipboardSync { enabled: on });
    }

    pub fn enabled(&self) -> bool {
        lock(&self.engine).local_on()
    }
}

impl<P: ClipboardProvider + Send + 'static> ChannelHandler for ClipboardHandler<P> {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn messages(&self) -> &'static [u16] {
        &[0x0303, 0x0304]
    }

    fn streams(&self) -> &'static [StreamKind] {
        &[StreamKind::Clipboard]
    }

    fn on_message(&self, _ctx: &SessionCtx, msg: Message) {
        match msg {
            Message::ClipboardSync(m) => lock(&self.engine).set_peer(m.enabled),
            Message::ClipboardSkipped(m) => {
                self.sink.event(ExtrasEvent::ClipboardSkipped { from_peer: true, reason: m.reason })
            }
            _ => {}
        }
    }

    fn on_start(&self, ctx: &SessionCtx) {
        let on = {
            let mut e = lock(&self.engine);
            e.new_session();
            e.local_on()
        };
        let _ = ctx.try_send(ClipboardSync { enabled: on });
        let (engine, sink, task_ctx) = (Arc::clone(&self.engine), Arc::clone(&self.sink), ctx.clone());
        ctx.spawn_gated(PermissionSet::CLIPBOARD, poll_loop(task_ctx, engine, sink));
    }

    fn on_stream(&self, ctx: &SessionCtx, stream: IncomingStream) {
        let (engine, sink) = (Arc::clone(&self.engine), Arc::clone(&self.sink));
        ctx.spawn_gated(PermissionSet::CLIPBOARD, async move {
            let read = tokio::time::timeout(READ_TIMEOUT, read_item(stream)).await;
            let Ok(Some(item)) = read else { return };
            let (kind, len) = match &item.payload {
                ClipboardPayload::Text(t) => (ClipKind::Text, t.len()),
                ClipboardPayload::Png(p) => (ClipKind::Png, p.len()),
            };
            let applied = tokio::task::spawn_blocking(move || lock(&engine).apply(&item)).await;
            if matches!(applied, Ok(Ok(true))) {
                sink.event(ExtrasEvent::ClipboardReceived { kind, len });
            }
        });
    }

    fn on_end(&self, _ctx: &SessionCtx, _end: &SessionEnd) {
        lock(&self.engine).set_peer(false);
    }
}

/// Reads one item: the header is checked (limits) before the body is read.
async fn read_item(mut stream: IncomingStream) -> Option<ClipboardItem> {
    let mut head = [0u8; CLIPBOARD_HEADER_LEN];
    stream.recv.read_exact(&mut head).await.ok()?;
    let header = ClipboardHeader::decode(&head).ok()?;
    let mut buf = head.to_vec();
    buf.resize(CLIPBOARD_HEADER_LEN + header.len as usize, 0);
    stream.recv.read_exact(&mut buf[CLIPBOARD_HEADER_LEN..]).await.ok()?;
    ClipboardItem::decode(&buf).ok()
}

async fn poll_loop<P: ClipboardProvider + Send + 'static>(
    ctx: SessionCtx,
    engine: Arc<Mutex<ClipEngine<P>>>,
    sink: Arc<dyn ExtrasSink>,
) {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    loop {
        tick.tick().await;
        let e = Arc::clone(&engine);
        let Ok(out) = tokio::task::spawn_blocking(move || lock(&e).poll()).await else { return };
        match out {
            None => {}
            Some(Outgoing::Skipped(reason)) => {
                sink.event(ExtrasEvent::ClipboardSkipped { from_peer: false, reason });
                let _ = ctx.try_send(ClipboardSkipped { reason });
            }
            Some(Outgoing::Item(item)) => {
                let Ok(bytes) = item.encode() else { continue };
                let Ok(mut send) = ctx.open_uni(StreamKind::Clipboard).await else { continue };
                if send.write_all(&bytes).await.is_ok() {
                    let _ = send.finish();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_fakes::FakeClipboard;

    fn text(s: &str) -> ClipContent {
        ClipContent::Text(s.into())
    }

    fn engine(fake: &FakeClipboard) -> ClipEngine<FakeClipboard> {
        let mut e = ClipEngine::new(fake.clone(), true);
        e.set_peer(true);
        e
    }

    #[test]
    fn a_local_copy_goes_out_once_with_a_rising_seq() {
        let fake = FakeClipboard::new();
        let mut e = engine(&fake);
        assert_eq!(e.poll(), None);
        fake.copy_externally(text("a"), false);
        assert_eq!(
            e.poll(),
            Some(Outgoing::Item(ClipboardItem { seq: 1, payload: ClipboardPayload::Text("a".into()) }))
        );
        assert_eq!(e.poll(), None, "unchanged counter: nothing");
        fake.copy_externally(ClipContent::Png(vec![1, 2]), false);
        assert!(matches!(e.poll(), Some(Outgoing::Item(ClipboardItem { seq: 2, payload: ClipboardPayload::Png(_) }))));
    }

    #[test]
    fn concealed_unsupported_and_oversize_are_skipped_never_sent() {
        let fake = FakeClipboard::new();
        let mut e = engine(&fake);
        fake.copy_externally(text("hunter2"), true);
        assert_eq!(e.poll(), Some(Outgoing::Skipped(ClipSkip::Concealed)));
        fake.copy_externally(text(&"x".repeat(MAX_CLIPBOARD_TEXT as usize + 1)), false);
        assert_eq!(e.poll(), Some(Outgoing::Skipped(ClipSkip::TooLarge)));
        fake.copy_externally(ClipContent::Png(vec![0; MAX_CLIPBOARD_PNG as usize + 1]), false);
        assert_eq!(e.poll(), Some(Outgoing::Skipped(ClipSkip::TooLarge)));
        fake.copy_unsupported();
        assert_eq!(e.poll(), Some(Outgoing::Skipped(ClipSkip::Unsupported)));
    }

    #[test]
    fn the_first_look_after_the_provider_was_out_of_reach_only_sets_the_baseline() {
        let fake = FakeClipboard::new();
        fake.copy_externally(text("before the session"), false);
        let mut e = ClipEngine::new(fake.clone(), true);
        e.set_peer(true);
        // As if the provider had failed at `new` and at `new_session`.
        e.last_count = None;
        assert_eq!(e.poll(), None, "the earlier copy is not sent");
        fake.copy_externally(text("during"), false);
        assert!(matches!(e.poll(), Some(Outgoing::Item(_))));
    }

    #[test]
    fn nothing_is_sent_unless_both_sides_have_sync_on_and_old_content_stays_local() {
        let fake = FakeClipboard::new();
        let mut e = ClipEngine::new(fake.clone(), true);
        fake.copy_externally(text("while the peer is off"), false);
        assert_eq!(e.poll(), None);
        e.set_peer(true);
        assert_eq!(e.poll(), None, "the earlier copy was consumed, not queued");
        e.set_local(false);
        fake.copy_externally(text("while we are off"), false);
        assert_eq!(e.poll(), None);
    }

    #[test]
    fn an_incoming_item_is_written_once_newest_seq_wins_and_does_not_echo() {
        let fake = FakeClipboard::new();
        let mut e = engine(&fake);
        let item = |seq, s: &str| ClipboardItem { seq, payload: ClipboardPayload::Text(s.into()) };
        assert_eq!(e.apply(&item(5, "five")), Ok(true));
        assert_eq!(e.apply(&item(4, "stale")), Ok(false));
        assert_eq!(e.apply(&item(5, "dup")), Ok(false));
        assert_eq!(e.apply(&item(6, "six")), Ok(true));
        assert_eq!(fake.writes(), vec![text("five"), text("six")]);
        assert_eq!(e.poll(), None, "what we wrote is not sent back");
        // The seq wraps: 0 after u32::MAX is newer.
        e.new_session();
        assert_eq!(e.apply(&item(u32::MAX, "max")), Ok(true));
        assert_eq!(e.apply(&item(0, "zero")), Ok(true));
    }
}
