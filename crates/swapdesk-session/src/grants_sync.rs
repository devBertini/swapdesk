//! The grants sync connection, the side that answers (`docs/GRANTS-SYNC.md` 4.2, 4.3, 4.6): a connection to the session
//! endpoint whose first control message is `GrantsSync` instead of `Hello`.
//!
//! * It answers with what this host stored for the authenticated device (`grants_here` of the record the gate found,
//!   never a device the message names) and reports the device's own value as [`HostEvent::PeerGrants`] and its
//!   protocol minor as [`HostEvent::PeerMinor`].
//! * It never prompts, never takes a place in the live table (so a live session does not make it busy, and it never
//!   counts as one), emits no `SessionStarted` or `Ended` and writes no audit entry.
//! * One message each way. A second message, a stream or a datagram is `PROTOCOL_VIOLATION`; so is a `GrantsSync`
//!   that says minor 0 (no build of minor 0 can send it). The client closes; this side closes 2 s after its answer.
//! * Per device: one sync in flight and a budget of 6 a minute with a burst of 3; past it the connection closes
//!   `BUSY` right after the first message, unanswered and unapplied.

use std::collections::HashMap;
use std::time::Duration;

use quinn::Connection;
use swapdesk_proto::{ErrorCode, GrantsSync, Message, PROTO_MINOR};
use swapdesk_trust::DeviceId;
use tokio::time::Instant;

use crate::ctx::{EndKind, SessionEnd};
use crate::host::{HostEvent, HostService};
use crate::link::ControlIo;
use crate::util::lock;

/// How long this side waits for the client to close after its answer.
pub const LINGER: Duration = Duration::from_secs(2);
/// Syncs a device may start in a burst, and how many it earns back a minute.
pub const BURST: f64 = 3.0;
pub const PER_MINUTE: f64 = 6.0;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
    in_flight: bool,
}

/// The budget of every device that syncs with this host.
#[derive(Debug, Default)]
pub(crate) struct SyncBudget {
    per: HashMap<DeviceId, Bucket>,
}

impl SyncBudget {
    /// Takes one sync for `device`, or `false` when it is over its budget or already has one in flight.
    fn take(&mut self, device: DeviceId, now: Instant) -> bool {
        let b = self.per.entry(device).or_insert(Bucket { tokens: BURST, at: now, in_flight: false });
        let earned = now.saturating_duration_since(b.at).as_secs_f64() * PER_MINUTE / 60.0;
        b.tokens = (b.tokens + earned).min(BURST);
        b.at = now;
        if b.in_flight || b.tokens < 1.0 {
            return false;
        }
        b.tokens -= 1.0;
        b.in_flight = true;
        true
    }

    fn done(&mut self, device: &DeviceId) {
        if let Some(b) = self.per.get_mut(device) {
            b.in_flight = false;
        }
    }
}

/// Ends the connection with `code` (the application close of QUIC; the memory link just closes its stream).
async fn close(io: &ControlIo, conn: Option<&Connection>, code: ErrorCode) {
    io.out.close(Duration::from_millis(500)).await;
    if let Some(c) = conn {
        c.close(crate::host::app_code(code), b"");
    }
}

/// Answers the sync `first` that `device` opened the connection with.
pub(crate) async fn answer(
    host: &HostService,
    mut io: ControlIo,
    conn: Option<Connection>,
    device: DeviceId,
    first: GrantsSync,
) -> SessionEnd {
    let inner = &host.inner;
    if first.proto_minor == 0 {
        close(&io, conn.as_ref(), ErrorCode::ProtocolViolation).await;
        return SessionEnd { kind: EndKind::Violation, code: ErrorCode::ProtocolViolation };
    }
    if !lock(&inner.sync_budget).take(device, Instant::now()) {
        close(&io, conn.as_ref(), ErrorCode::Busy).await;
        return SessionEnd { kind: EndKind::Refused, code: ErrorCode::Busy };
    }
    let end = exchange(host, &mut io, conn.as_ref(), device, first).await;
    lock(&inner.sync_budget).done(&device);
    end
}

async fn exchange(
    host: &HostService,
    io: &mut ControlIo,
    conn: Option<&Connection>,
    device: DeviceId,
    first: GrantsSync,
) -> SessionEnd {
    let inner = &host.inner;
    // The device comes from the gate, never from the message.
    let Some(stored) = inner.stored(&device) else {
        close(io, conn, ErrorCode::NotPermitted).await;
        return SessionEnd { kind: EndKind::Refused, code: ErrorCode::NotPermitted };
    };
    io.out.set_peer_minor(first.proto_minor);
    if io.out.send(GrantsSync { proto_minor: PROTO_MINOR, grants: stored.grants_here }).await.is_err() {
        return SessionEnd { kind: EndKind::Lost, code: ErrorCode::NoError };
    }
    inner.ui.event(HostEvent::PeerMinor { device, minor: first.proto_minor });
    inner.ui.event(HostEvent::PeerGrants { device, grants: first.grants });

    // Nothing else may come: the client closes, or this side does after `LINGER`.
    let more = async {
        match conn {
            Some(c) => {
                use crate::util::{Either, select2};
                // Any of them is "more"; a closed stream or connection is not.
                let streams = select2(c.accept_bi(), c.accept_uni());
                let other = select2(streams, c.read_datagram());
                match select2(io.rx.recv(), other).await {
                    Either::Left(m) => m.is_some(),
                    Either::Right(Either::Left(Either::Left(s))) => s.is_ok(),
                    Either::Right(Either::Left(Either::Right(s))) => s.is_ok(),
                    Either::Right(Either::Right(d)) => d.is_ok(),
                }
            }
            None => io.rx.recv().await.is_some(),
        }
    };
    match tokio::time::timeout(LINGER, more).await {
        Ok(true) => {
            close(io, conn, ErrorCode::ProtocolViolation).await;
            SessionEnd { kind: EndKind::Violation, code: ErrorCode::ProtocolViolation }
        }
        Ok(false) => SessionEnd { kind: EndKind::PeerClosed, code: ErrorCode::NoError },
        Err(_) => {
            close(io, conn, ErrorCode::NoError).await;
            SessionEnd { kind: EndKind::LocalBye, code: ErrorCode::NoError }
        }
    }
}

/// Whether `msg` opens a grants sync connection.
pub(crate) fn as_sync(msg: &Message) -> Option<GrantsSync> {
    match msg {
        Message::GrantsSync(s) => Some(s.clone()),
        _ => None,
    }
}
