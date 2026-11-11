//! The grants sync outside a session, this machine's side that dials (`docs/GRANTS-SYNC.md` 4.3, 4.4, 4.6, slice S6).
//!
//! * **When.** 1 s after this machine's user changed what a device may do here (each further change moves it); when
//!   a paired device appears on the network, or its record changes (port, minor); when the core starts. Of two
//!   machines that see each other, the one whose device ID is lower dials at once and the other only 3 s later, and
//!   only if no sync completed meanwhile.
//! * **With whom.** A paired device on the network, known to speak minor 1 (what it said over an authenticated
//!   connection wins over its record's hint), that never closed a sync as a violation since its record last came
//!   back, whose handshake did not refuse this machine's key, and with which no live session already carries
//!   `StoredGrants`. A device of minor 0 is never dialled.
//! * **Limits.** One sync in flight per device, four in all.
//! * **Order.** A value the peer pushed (its own sync, or `StoredGrants` in a session) after a sync of this side
//!   started is newer than that sync's answer, which is then dropped.

use std::collections::HashMap;
use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use swapdesk_platform::Level;
use swapdesk_trust::{DeviceId, device_id_hex};
use tokio::time::Instant;

use super::{Hub, lock};
use crate::service::SyncError;

/// How long after a change of the grants the push waits (each further change moves it).
pub(super) const PUSH_DELAY: Duration = Duration::from_millis(1000);
/// How long the machine with the higher device ID waits before it dials a device that appeared.
pub(super) const TIE_DELAY: Duration = Duration::from_secs(3);
/// Syncs in flight at once.
const MAX_IN_FLIGHT: usize = 4;

#[derive(Debug, Default)]
struct Slot {
    /// A sync ended as a violation: the device does not speak minor 1, whatever its record says.
    refused: bool,
    /// Its handshake refused this machine's key: no automatic sync until the user acts on it or the core restarts.
    unknown_key: bool,
    /// This machine's grants for it changed at this time and no sync has delivered them yet.
    changed_at: Option<Instant>,
    in_flight: bool,
    last_ok: Option<Instant>,
    /// When a value the peer pushed was last applied.
    peer_applied_at: Option<Instant>,
    /// What discovery showed last (address, session port, minor); `None` while it is away.
    seen: Option<(Option<SocketAddrV4>, u16)>,
}

#[derive(Debug, Default)]
pub(super) struct SyncState {
    per: HashMap<DeviceId, Slot>,
    in_flight: usize,
    /// Syncs this machine started, and how many of them were answered (for the tests and the trace).
    dialled: u64,
    answered: u64,
}

impl SyncState {
    pub fn dialled(&self) -> u64 {
        self.dialled
    }

    pub fn answered(&self) -> u64 {
        self.answered
    }

    /// Whether the handshake of `device` refused this machine's key (`None`: never synced).
    #[cfg(test)]
    pub fn per_unknown_key(&self, device: &DeviceId) -> Option<bool> {
        self.per.get(device).map(|s| s.unknown_key)
    }

    /// A sync of `device` ended as a violation: its known minor is 0 until its record comes back.
    pub fn refused(&self, device: &DeviceId) -> bool {
        self.per.get(device).is_some_and(|s| s.refused)
    }
}

impl Hub {
    /// This machine's user changed what `device` may do here: the push goes 1 s after the last change.
    pub(super) fn sync_after_change(self: &Arc<Self>, device: DeviceId) {
        let now = Instant::now();
        lock(&self.sync).per.entry(device).or_default().changed_at = Some(now);
        self.sync_later(device, PUSH_DELAY, Some(now));
    }

    /// The user acted on `device` (Connect, Pair): a key it refused may be pinned again now.
    pub(super) fn sync_user_acted(&self, device: &DeviceId) {
        if let Some(s) = lock(&self.sync).per.get_mut(device) {
            s.unknown_key = false;
        }
    }

    /// The peer pushed its value (a sync it started, or `StoredGrants` in a session): later than any answer to a sync
    /// this side started before now.
    pub(super) fn sync_peer_pushed(&self, device: DeviceId) {
        lock(&self.sync).per.entry(device).or_default().peer_applied_at = Some(Instant::now());
    }

    /// Discovery changed, or the core started: every paired device that appeared or whose record changed is synced
    /// (the lower device ID at once, the other 3 s later).
    pub(super) fn sync_presence_changed(self: &Arc<Self>) {
        let peers = self.p.presence.peers();
        let mine = self.p.identity.device_id();
        let mut due = Vec::new();
        {
            let mut st = lock(&self.sync);
            let present: Vec<DeviceId> = peers.iter().map(|p| p.device_id).collect();
            for slot in st.per.iter_mut().filter(|(id, _)| !present.contains(id)).map(|(_, s)| s) {
                slot.seen = None;
            }
            for p in &peers {
                if self.p.trust.read(|s| s.find_by_id(&p.device_id).is_none()) {
                    continue;
                }
                let now_seen = (p.addrs.first().map(|ip| SocketAddrV4::new(*ip, p.session_port)), p.proto_minor);
                let slot = st.per.entry(p.device_id).or_default();
                if slot.seen == Some(now_seen) {
                    continue;
                }
                if slot.seen.is_none() {
                    // Back on the network: what it answered before counts again only if it says so again.
                    slot.refused = false;
                }
                slot.seen = Some(now_seen);
                due.push(p.device_id);
            }
        }
        for device in due {
            if mine < device {
                self.try_sync(device);
            } else {
                self.sync_later(device, TIE_DELAY, None);
            }
        }
    }

    /// Tries a sync with `device` after `delay`. `change`: the push of the change made then (a later change moved
    /// it, so this one does nothing); `None`: an appearance, skipped when a sync completed meanwhile.
    fn sync_later(self: &Arc<Self>, device: DeviceId, delay: Duration, change: Option<Instant>) {
        let Some(rt) = self.rt() else { return };
        let me = Arc::clone(self);
        let asked = Instant::now();
        rt.spawn(async move {
            tokio::time::sleep(delay).await;
            let skip = {
                let st = lock(&me.sync);
                let slot = st.per.get(&device);
                match change {
                    Some(at) => slot.and_then(|s| s.changed_at).is_some_and(|c| c > at),
                    // A sync completed meanwhile, ours or one the peer started (its value then arrived here).
                    None => slot.is_some_and(|s| {
                        s.last_ok.is_some_and(|ok| ok >= asked) || s.peer_applied_at.is_some_and(|t| t >= asked)
                    }),
                }
            };
            if !skip {
                me.try_sync(device);
            }
        });
    }

    /// Whether a live session with `device` already carries `StoredGrants` (its peer speaks minor 1).
    fn session_carries(&self, device: &DeviceId) -> bool {
        if let Some(core) = self.core() {
            let host = core.host();
            if host
                .sessions()
                .iter()
                .filter(|s| &s.device == device)
                .any(|s| host.session(s.session_id).is_some_and(|c| c.peer_minor() >= 1))
            {
                return true;
            }
        }
        lock(&self.conns)
            .iter()
            .any(|c| lock(&c.viewers).values().any(|v| &v.device == device && v.handle.ctx().peer_minor() >= 1))
    }

    /// The address to sync `device` at, when it may be synced now.
    fn sync_target(&self, device: &DeviceId) -> Option<SocketAddr> {
        if self.p.trust.read(|s| s.find_by_id(device).is_none()) {
            return None;
        }
        let hint = self.minor_hint(device);
        if hint.is_none() || self.known_minor(device, hint) < 1 || self.session_carries(device) {
            return None;
        }
        self.address_of(device).map(SocketAddr::V4)
    }

    /// Starts a sync with `device` now, if it may be synced and has none in flight.
    pub(super) fn try_sync(self: &Arc<Self>, device: DeviceId) {
        let Some(rt) = self.rt() else { return };
        let Some(core) = self.core() else { return };
        {
            let st = lock(&self.sync);
            if st.per.get(&device).is_some_and(|s| s.in_flight || s.unknown_key || s.refused) {
                return;
            }
            if st.in_flight >= MAX_IN_FLIGHT {
                drop(st);
                self.sync_later(device, Duration::from_millis(500), None);
                return;
            }
        }
        let Some(addr) = self.sync_target(&device) else { return };
        let started = Instant::now();
        {
            let mut st = lock(&self.sync);
            let slot = st.per.entry(device).or_default();
            if slot.in_flight {
                return;
            }
            slot.in_flight = true;
            st.in_flight += 1;
            st.dialled += 1;
        }
        let me = Arc::clone(self);
        rt.spawn(async move {
            let result = core.sync_grants_async(addr, device).await;
            me.sync_finished(device, started, result);
        });
    }

    pub(super) fn sync_finished(
        self: &Arc<Self>,
        device: DeviceId,
        started: Instant,
        result: Result<swapdesk_proto::GrantsSync, SyncError>,
    ) {
        let hex = device_id_hex(&device);
        let (apply, again) = {
            let mut st = lock(&self.sync);
            st.in_flight = st.in_flight.saturating_sub(1);
            if result.is_ok() {
                st.answered += 1;
            }
            let slot = st.per.entry(device).or_default();
            slot.in_flight = false;
            match &result {
                Ok(answer) => {
                    slot.last_ok = Some(Instant::now());
                    let delivered = slot.changed_at.is_none_or(|c| c <= started);
                    if delivered {
                        slot.changed_at = None;
                    }
                    // A value the peer pushed after this sync started is newer than its answer.
                    let newer = slot.peer_applied_at.is_some_and(|t| t > started);
                    ((!newer).then_some(answer.grants), !delivered)
                }
                Err(SyncError::Violation) => {
                    slot.refused = true;
                    (None, false)
                }
                Err(SyncError::UnknownKey) => {
                    slot.unknown_key = true;
                    (None, false)
                }
                Err(_) => (None, false),
            }
        };
        match result {
            Ok(answer) => {
                let hint = self.minor_hint(&device);
                lock(&self.minors).said(device, answer.proto_minor, hint);
                let (answered, dialled) = {
                    let st = lock(&self.sync);
                    (st.answered(), st.dialled())
                };
                self.log(Level::Debug, format_args!("grants synced with {hex} ({answered} of {dialled} answered)"));
                if let Some(grants) = apply {
                    self.store_peer_grants(device, grants);
                }
                if again {
                    self.sync_later(device, PUSH_DELAY, None);
                }
            }
            Err(SyncError::Violation) => self.log(
                Level::Info,
                format_args!("{hex} closed a grants sync as a violation: not dialled again until it reappears"),
            ),
            Err(SyncError::UnknownKey) => self.log(
                Level::Info,
                format_args!("{hex} refused this machine's key: no automatic grants sync until it is acted on"),
            ),
            Err(e) => self.log(Level::Debug, format_args!("a grants sync with {hex} gave no answer: {e:?}")),
        }
    }
}
