//! Clipboard sync (F-CLIP-01, PROTOCOL.md 7.5 and the `ClipboardSync` / `ClipboardSkipped` messages).
//!
//! [`ClipEngine`] is the logic over a `ClipboardProvider`, with no I/O: it polls the provider's change counter, decides
//! what goes out (text and PNG within the 7.5 limits; concealed content never; an item is only sent when both sides
//! switched sync on; what the clipboard holds when sync becomes active is offered once), and applies what comes in (the newest `seq` wins; what it writes does not echo back).
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
    /// Sync just became active on both sides: the next poll offers what the clipboard holds, changed or not.
    offer_pending: bool,
}

impl<P: ClipboardProvider> ClipEngine<P> {
    /// Sync starts off on the peer's side until it says otherwise; the content already on the clipboard is offered
    /// once when both sides have sync on.
    pub fn new(mut provider: P, local_on: bool) -> Self {
        let last_count = provider.change_count().ok();
        Self { provider, last_count, out_seq: 0, in_seq: None, local_on, peer_on: false, offer_pending: false }
    }

    fn active(&self) -> bool {
        self.local_on && self.peer_on
    }

    pub fn set_local(&mut self, on: bool) {
        let was = self.active();
        self.local_on = on;
        self.offer_pending |= !was && self.active();
    }

    pub fn local_on(&self) -> bool {
        self.local_on
    }

    pub fn set_peer(&mut self, on: bool) {
        let was = self.active();
        self.peer_on = on;
        self.offer_pending |= !was && self.active();
    }

    /// A new session: the peer's `seq` starts over. What is on the clipboard now is offered once sync is on at both
    /// sides (the peer's `ClipboardSync` arrives after this).
    pub fn new_session(&mut self) {
        self.in_seq = None;
        self.peer_on = false;
        self.offer_pending = false;
        self.last_count = self.provider.change_count().ok();
    }

    /// Looks at the clipboard once. A change while sync is off on either side is consumed without being sent. When
    /// sync has just become active on both sides, what the clipboard holds is offered like a copy, except that a type
    /// that is never synced says nothing.
    pub fn poll(&mut self) -> Option<Outgoing> {
        let count = self.provider.change_count().ok()?;
        let offer = self.offer_pending && self.active();
        if self.last_count == Some(count) && !offer {
            return None;
        }
        let first_look = self.last_count.is_none();
        self.last_count = Some(count);
        // The provider was out of reach until now (a helper or a UI not attached yet): what is on the clipboard at the
        // first look was there before the session; only the offer at the start sends it.
        if first_look && !offer {
            return None;
        }
        if !self.active() {
            return None;
        }
        self.offer_pending = false;
        match self.provider.read().ok()? {
            ClipRead::Empty => None,
            ClipRead::Unsupported if offer => None,
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
        // The clipboard now holds the peer's item: offering it back would be an echo.
        self.offer_pending = false;
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

    fn session_engine(fake: &FakeClipboard, local_on: bool) -> ClipEngine<FakeClipboard> {
        let mut e = ClipEngine::new(fake.clone(), local_on);
        e.new_session();
        e
    }

    #[test]
    fn what_was_copied_before_the_session_is_offered_when_it_starts() {
        let fake = FakeClipboard::new();
        fake.copy_externally(text("before"), false);
        let mut e = session_engine(&fake, true);
        assert_eq!(e.poll(), None, "the peer has not said it is on yet");
        e.set_peer(true);
        assert_eq!(
            e.poll(),
            Some(Outgoing::Item(ClipboardItem { seq: 1, payload: ClipboardPayload::Text("before".into()) }))
        );
        assert_eq!(e.poll(), None, "offered once");
        fake.copy_externally(ClipContent::Png(vec![1]), false);
        assert!(matches!(e.poll(), Some(Outgoing::Item(ClipboardItem { seq: 2, .. }))), "later copies go on as before");
        // The same limits and notices as a copy during the session.
        let big = FakeClipboard::new();
        big.copy_externally(text(&"x".repeat(MAX_CLIPBOARD_TEXT as usize + 1)), false);
        let mut e = session_engine(&big, true);
        e.set_peer(true);
        assert_eq!(e.poll(), Some(Outgoing::Skipped(ClipSkip::TooLarge)));
        assert_eq!(e.poll(), None);
        let secret = FakeClipboard::new();
        secret.copy_externally(text("hunter2"), true);
        let mut e = session_engine(&secret, true);
        e.set_peer(true);
        assert_eq!(e.poll(), Some(Outgoing::Skipped(ClipSkip::Concealed)));
    }

    #[test]
    fn nothing_is_offered_at_the_start_without_the_permission_or_with_the_sync_off() {
        let fake = FakeClipboard::new();
        fake.copy_externally(text("before"), false);
        let mut here_off = session_engine(&fake, false);
        here_off.set_peer(true);
        assert_eq!(here_off.poll(), None, "sync off here");
        let mut peer_off = session_engine(&fake, true);
        peer_off.set_peer(false);
        assert_eq!(peer_off.poll(), None, "sync off at the peer");
        // The permission is the handler's gate (the poll loop is spawned gated): without it poll is never called.
        // The service test with a host that does not grant CLIPBOARD proves that end to end.
        let mut next_session = session_engine(&fake, true);
        next_session.set_peer(true);
        next_session.new_session();
        assert_eq!(next_session.poll(), None, "a new session offers again only when the peer says it is on");
    }

    #[test]
    fn turning_the_sync_on_offers_the_current_clipboard_once() {
        let fake = FakeClipboard::new();
        let mut e = session_engine(&fake, false);
        e.set_peer(true);
        fake.copy_externally(text("while off"), false);
        assert_eq!(e.poll(), None);
        e.set_local(true);
        assert!(matches!(e.poll(), Some(Outgoing::Item(ClipboardItem { seq: 1, .. }))));
        assert_eq!(e.poll(), None);
        e.set_local(true);
        assert_eq!(e.poll(), None, "setting it on again while on offers nothing");
        e.set_local(false);
        e.set_local(true);
        assert!(matches!(e.poll(), Some(Outgoing::Item(ClipboardItem { seq: 2, .. }))), "off then on is a new turn-on");
    }

    #[test]
    fn an_empty_or_unsupported_clipboard_offers_nothing_at_the_start() {
        let empty = FakeClipboard::new();
        let mut e = session_engine(&empty, true);
        e.set_peer(true);
        assert_eq!(e.poll(), None);
        let other = FakeClipboard::new();
        other.copy_unsupported();
        let mut e = session_engine(&other, true);
        e.set_peer(true);
        assert_eq!(e.poll(), None, "no notice for a type that is never synced");
        other.copy_unsupported();
        assert_eq!(e.poll(), Some(Outgoing::Skipped(ClipSkip::Unsupported)), "a copy during the session still says so");
    }

    #[test]
    fn what_just_arrived_from_the_peer_is_not_offered_back_at_the_start() {
        let fake = FakeClipboard::new();
        fake.copy_externally(text("mine"), false);
        let mut e = session_engine(&fake, true);
        e.set_peer(true);
        let item = ClipboardItem { seq: 1, payload: ClipboardPayload::Text("theirs".into()) };
        assert_eq!(e.apply(&item), Ok(true));
        assert_eq!(e.poll(), None, "the clipboard holds the peer's item now");
    }

    mod over_a_session {
        use super::*;
        use swapdesk_fakes::{FakeClock, MemorySecretStore};
        use swapdesk_platform::MonoMicros;
        use swapdesk_proto::{Capabilities, KbKind, KeyboardInfo, Os};
        use swapdesk_session::{
            HostConfig, HostService, HostUi, PromptAnswer, PromptRequest, Registry, ViewerConfig, ViewerSession,
            memory_pair,
        };
        use swapdesk_trust::{PairedDevice, TrustHandle, device_id};
        use tokio::sync::oneshot;

        struct Ui(PermissionSet);

        impl HostUi for Ui {
            fn somebody_present(&self) -> bool {
                true
            }
            fn prompt(&self, _request: PromptRequest) -> oneshot::Receiver<PromptAnswer> {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(PromptAnswer::Accept(self.0));
                rx
            }
            fn consent(&self, _request: swapdesk_session::ConsentRequest) -> oneshot::Receiver<bool> {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(true);
                rx
            }
        }

        /// A peer that knows only what 1.0 knew: the two clipboard messages and the clipboard stream. It switches
        /// its own sync on at the start and keeps the messages it received. (The in-memory link of the session
        /// layer carries no unidirectional streams: an offered item is proved by the wire check below and by the
        /// service tests over QUIC.)
        #[derive(Default)]
        struct OldPeer {
            messages: Mutex<Vec<Message>>,
        }

        impl ChannelHandler for OldPeer {
            fn name(&self) -> &'static str {
                "clipboard-1.0"
            }
            fn messages(&self) -> &'static [u16] {
                &[0x0303, 0x0304]
            }
            fn streams(&self) -> &'static [StreamKind] {
                &[StreamKind::Clipboard]
            }
            fn on_message(&self, _ctx: &SessionCtx, msg: Message) {
                lock(&self.messages).push(msg);
            }
            fn on_start(&self, ctx: &SessionCtx) {
                let _ = ctx.try_send(ClipboardSync { enabled: true });
            }
            fn on_stream(&self, _ctx: &SessionCtx, _stream: IncomingStream) {}
        }

        fn capabilities() -> Capabilities {
            Capabilities {
                video_decode: vec![],
                audio: vec![],
                max_datagram: 1200,
                keyboard: KeyboardInfo { kind: KbKind::Ansi, layout: "us".into() },
                features: 0,
            }
        }

        /// Runs a session whose viewer has a `ClipboardHandler` over `fake` and whose host is an `OldPeer` granting
        /// `grants`; returns the messages the old peer saw after the session had time to offer.
        fn run(grants: PermissionSet, fake: &FakeClipboard) -> Vec<Message> {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async {
                let clock = Arc::new(FakeClock::new(MonoMicros(1)));
                let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
                let identity = swapdesk_trust::Identity::generate(0).unwrap();
                let id = device_id(identity.spki());
                trust
                    .update(|s| {
                        let mut d = PairedDevice::new_paired(
                            *identity.spki(),
                            "viewer".into(),
                            Os::MacOs,
                            "15".into(),
                            1,
                            None,
                        );
                        d.grants_here = grants;
                        s.devices.push(d);
                    })
                    .unwrap();
                let peer = Arc::new(OldPeer::default());
                let mut hr = Registry::new();
                hr.register(peer.clone()).unwrap();
                let host = HostService::new(
                    HostConfig::new("host-pc", Os::Windows, "11 24H2", "0.1.0", capabilities()),
                    trust,
                    Arc::new(Ui(grants)),
                    hr,
                    clock.clone(),
                );
                let handler = ClipboardHandler::new(fake.clone(), true, Arc::new(crate::NullSink));
                let mut vr = Registry::new();
                vr.register(handler).unwrap();
                let mut cfg = ViewerConfig::new("viewer-mac", Os::MacOs, "15.1", "0.1.0", capabilities(), clock);
                cfg.requested = grants;
                cfg.registry = vr;
                let (host_io, viewer_io) = memory_pair();
                let h = host.clone();
                tokio::spawn(async move { h.serve_io(host_io, None, id).await });
                let session = ViewerSession::open_io(viewer_io, None, Some([9; 16]), cfg).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1200)).await;
                let seen = lock(&peer.messages).clone();
                session.disconnect().await;
                seen
            })
        }

        #[test]
        fn a_one_zero_peer_sees_only_the_messages_it_knows_when_the_offer_at_the_start_is_skipped() {
            let fake = FakeClipboard::new();
            fake.copy_externally(text("hunter2"), true);
            let seen = run(PermissionSet::CONTROL.union(PermissionSet::CLIPBOARD), &fake);
            assert_eq!(
                seen,
                vec![
                    Message::ClipboardSync(ClipboardSync { enabled: true }),
                    Message::ClipboardSkipped(ClipboardSkipped { reason: ClipSkip::Concealed }),
                ]
            );
        }

        #[test]
        fn a_session_without_the_permission_offers_nothing() {
            let fake = FakeClipboard::new();
            fake.copy_externally(text("hunter2"), true);
            assert_eq!(run(PermissionSet::CONTROL, &fake), vec![], "not even the notice reaches the peer");
        }
    }

    #[test]
    fn the_offer_at_the_start_is_an_item_in_the_wire_format_of_1_0() {
        let fake = FakeClipboard::new();
        fake.copy_externally(text("before"), false);
        let mut e = session_engine(&fake, true);
        e.set_peer(true);
        let Some(Outgoing::Item(item)) = e.poll() else { panic!("an item is offered") };
        let bytes = item.encode().unwrap();
        assert_eq!(ClipboardItem::decode(&bytes), Ok(item), "the same record a copy during the session sends");
    }

    #[test]
    fn the_first_look_after_the_provider_was_out_of_reach_only_sets_the_baseline() {
        let fake = FakeClipboard::new();
        fake.copy_externally(text("before the session"), false);
        let mut e = ClipEngine::new(fake.clone(), true);
        e.set_peer(true);
        // As if the provider had failed at `new` and at `new_session`, with the offer at the start already made.
        e.last_count = None;
        e.offer_pending = false;
        assert_eq!(e.poll(), None, "the earlier copy is not sent");
        fake.copy_externally(text("during"), false);
        assert!(matches!(e.poll(), Some(Outgoing::Item(_))));
    }

    #[test]
    fn nothing_is_sent_unless_both_sides_have_sync_on_and_a_copy_made_while_off_is_not_queued() {
        let fake = FakeClipboard::new();
        let mut e = ClipEngine::new(fake.clone(), true);
        fake.copy_externally(text("while the peer is off"), false);
        assert_eq!(e.poll(), None);
        e.set_peer(true);
        e.offer_pending = false;
        assert_eq!(e.poll(), None, "a copy made while the peer was off was consumed, not queued");
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
