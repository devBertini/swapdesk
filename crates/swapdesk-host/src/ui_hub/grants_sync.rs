//! What the other side stored for a paired device, as this machine learns it (GRANTS-SYNC.md 3.4): the column
//! "Este computador pode, no X" of Permissões.
//!
//! * The value arrives from the peer (`StoredGrants` in a session, either role). It is kept as `grants_there` of the
//!   device's record, and only when it differs from what is kept.
//! * The trust store is written at most once per second per device; a value that arrives inside that second waits
//!   for its end, and the latest value wins. A peer that repeats the message cannot turn it into a stream of disk
//!   writes.
//! * After a write the interfaces hear `DevicesChanged`, whose `DeviceView.grants_there` carries it.
//! * Nothing else reads it: it never widens or narrows a session, never decides a swap, a prompt or an `Elevate`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use swapdesk_platform::Level;
use swapdesk_proto::PermissionSet;
use swapdesk_trust::{DeviceId, device_id_hex};
use tokio::time::Instant;

use super::{Hub, lock};

/// The shortest time between two writes of one device's value.
pub(super) const WRITE_GAP: Duration = Duration::from_secs(1);

/// The writes of the peers' values, per device.
#[derive(Default)]
pub(super) struct PeerWrites {
    per: HashMap<DeviceId, Slot>,
}

#[derive(Default)]
struct Slot {
    /// When the value of this device was last written.
    last_write: Option<Instant>,
    /// A value that arrived inside the gap: written at its end.
    pending: Option<PermissionSet>,
    /// A task waits for the end of the gap.
    timer: bool,
}

impl PeerWrites {
    /// The device was forgotten: what waited for it goes.
    pub fn forget(&mut self, device: &DeviceId) {
        self.per.remove(device);
    }
}

impl Hub {
    /// The peer `device` pushed what its side stored for this machine (display only): `StoredGrants` in a session,
    /// or a grants sync it started.
    pub(super) fn apply_peer_grants(self: &Arc<Self>, device: DeviceId, grants: PermissionSet) {
        self.sync_peer_pushed(device);
        self.store_peer_grants(device, grants);
    }

    /// Keeps `grants` as what `device` allows here (the coalesced write below).
    pub(super) fn store_peer_grants(self: &Arc<Self>, device: DeviceId, grants: PermissionSet) {
        let now = Instant::now();
        let due = {
            let mut writes = lock(&self.peer_writes);
            let slot = writes.per.entry(device).or_default();
            match slot.last_write {
                Some(t) if now < t + WRITE_GAP => {
                    slot.pending = Some(grants);
                    if slot.timer {
                        return;
                    }
                    slot.timer = true;
                    Some(t + WRITE_GAP)
                }
                _ => {
                    // A value equal to what is kept costs no write and leaves the gap where it was.
                    if self.kept_there(&device) == Some(Some(grants)) {
                        return;
                    }
                    slot.last_write = Some(now);
                    None
                }
            }
        };
        match due {
            None => self.write_peer_grants(&device, grants),
            Some(at) => {
                let Some(rt) = self.rt() else { return };
                let me = Arc::clone(self);
                rt.spawn(async move {
                    tokio::time::sleep_until(at).await;
                    me.flush_peer_grants(device);
                });
            }
        }
    }

    /// The end of the gap: the value that waited is written.
    fn flush_peer_grants(&self, device: DeviceId) {
        let grants = {
            let mut writes = lock(&self.peer_writes);
            let Some(slot) = writes.per.get_mut(&device) else { return };
            slot.timer = false;
            let grants = slot.pending.take();
            if grants.is_some() {
                slot.last_write = Some(Instant::now());
            }
            grants
        };
        if let Some(grants) = grants {
            self.write_peer_grants(&device, grants);
        }
    }

    /// `Some(grants_there)` of the device's record, `None` when it is not paired.
    fn kept_there(&self, device: &DeviceId) -> Option<Option<PermissionSet>> {
        self.p.trust.read(|s| s.find_by_id(device).map(|d| d.grants_there))
    }

    /// Compares and writes; a device that is no longer paired gets nothing.
    fn write_peer_grants(&self, device: &DeviceId, grants: PermissionSet) {
        match self.kept_there(device) {
            Some(kept) if kept != Some(grants) => {}
            _ => return,
        }
        let written = self.p.trust.update(|s| {
            s.devices.iter_mut().find(|d| d.device_id == *device).map(|d| d.grants_there = Some(grants)).is_some()
        });
        match written {
            Ok(true) => self.devices_changed(),
            Ok(false) => {}
            Err(e) => {
                self.log(Level::Warn, format_args!("what {} allows here was not written: {e}", device_id_hex(device)))
            }
        }
    }
}
