//! A handler that hands the messages it owns to an async consumer. The video control messages (`KeyframeRequest`,
//! `ReceiverReport`, `VideoSettings`, `SelectDisplay`) belong to the video pipeline and the host service, which
//! read them from a channel instead of implementing a handler of their own.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use swapdesk_proto::Message;
use tokio::sync::mpsc;

use crate::ctx::SessionCtx;
use crate::registry::ChannelHandler;

/// Message types of the host's video pipeline (viewer to host): `SelectDisplay`, `VideoSettings`, `KeyframeRequest`,
/// `ReceiverReport`, `AudioControl`, `SendSpecial`.
pub const VIDEO_CONTROL_TYPES: &[u16] = &[0x0102, 0x0103, 0x0104, 0x0105, 0x0107, 0x0109];

/// A message and the session it came from.
pub struct Forwarded {
    pub session: SessionCtx,
    pub msg: Message,
}

impl Forwarded {
    /// Whether the session still allows the message: it may have waited in the channel while the host's user took the
    /// permission away or the session ended. A consumer checks it before it acts (F-PERM-02).
    pub fn still_permitted(&self) -> bool {
        self.session.is_active()
            && crate::policy::permitted(self.session.granted(), self.session.requested(), &self.msg)
    }
}

pub struct ForwardHandler {
    name: &'static str,
    types: &'static [u16],
    tx: mpsc::Sender<Forwarded>,
    dropped: AtomicU64,
}

impl ForwardHandler {
    /// A handler for `types` and the channel its messages come out of. A consumer that falls behind loses the
    /// excess (counted in [`ForwardHandler::dropped`]) instead of holding the session back.
    pub fn new(name: &'static str, types: &'static [u16], capacity: usize) -> (Arc<Self>, mpsc::Receiver<Forwarded>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (Arc::new(Self { name, types, tx, dropped: AtomicU64::new(0) }), rx)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }
}

impl ChannelHandler for ForwardHandler {
    fn name(&self) -> &'static str {
        self.name
    }

    fn messages(&self) -> &'static [u16] {
        self.types
    }

    fn on_message(&self, ctx: &SessionCtx, msg: Message) {
        if self.tx.try_send(Forwarded { session: ctx.clone(), msg }).is_err() {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}
