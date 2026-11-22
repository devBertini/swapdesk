//! The trust store in memory, persisted through a `SecretStore` (`trust.bin`, C5.2) on every change.

use std::sync::{Arc, Mutex, MutexGuard};

use swapdesk_platform::{ErrorKind, PlatformError, SecretStore};
use swapdesk_proto::Wire;
use tokio::sync::Notify;

use crate::blocklist::{BLOCKED_NAME, BlockList, BlockedDevice};
use crate::error::{Result, TrustError};
use crate::ids::DeviceId;
use crate::records::{TRUST_NAME, TrustStore};

/// Answers "is this key pinned?" for the verifiers and the gate, by looking at the store as it is now.
pub trait PinLookup: Send + Sync {
    fn pinned(&self, spki: &[u8]) -> Option<DeviceId>;
}

/// The trust store shared by the verifiers, the gate and the pairing code.
///
/// Readers (every handshake's verifier) only ever take `store` for the instant of a copy or a lookup. Writers are
/// serialized by `writer`, which is held across the disk write: the change is built on a copy, written, and only
/// then published, so a slow disk stalls other writers but never a session handshake.
pub struct TrustHandle {
    store: Mutex<TrustStore>,
    writer: Mutex<()>,
    secrets: Arc<dyn SecretStore>,
    pairing: Mutex<Slot>,
    /// The devices blocked by MAC (D65), persisted in `blocked.bin`; changes are written under `writer` too.
    blocks: Mutex<BlockList>,
}

/// The machine's one pairing slot: which attempt holds it, and whether that attempt is an initiator that would give
/// the slot to its own peer's crossing attempt.
#[derive(Default)]
struct Slot {
    /// The ticket of the guard that holds the slot.
    holder: Option<u64>,
    next_ticket: u64,
    offer: Option<Offer>,
}

/// An initiator holding the slot has sent its `PairHello` to `peer` and has no answer yet. Both users pressed
/// "Parear" on each other's card at once when that same device's own `PairHello` reaches this machine's responder:
/// one of the two ceremonies must give way, or both fail `BUSY` (review of 22/11/2026, A3).
struct Offer {
    peer: DeviceId,
    /// The peer already refused this initiator `BUSY`: it is not giving way, so this side does, whatever the IDs.
    refused: bool,
    /// Fired when the responder took the slot over.
    gave_way: Arc<Notify>,
}

/// What the responder may do with a `PairHello` from `peer` while the slot is taken.
pub enum Crossing {
    /// The responder holds the slot now: it was free after all, or this machine's initiator dialling `peer` gave way.
    Held(PairingGuard),
    /// This machine's initiator is dialling `peer` and keeps the slot: the peer is the one to give way.
    Kept,
    /// Another attempt holds the slot.
    Busy,
}

/// The one "pairing in progress" slot of this machine, shared by the initiator and the responder role. Held for
/// the whole attempt; dropping it frees the slot, unless the slot was taken over from it.
pub struct PairingGuard {
    handle: Arc<TrustHandle>,
    ticket: u64,
}

impl Drop for PairingGuard {
    fn drop(&mut self) {
        let mut slot = guard(&self.handle.pairing);
        if slot.holder == Some(self.ticket) {
            slot.holder = None;
            slot.offer = None;
        }
    }
}

impl PairingGuard {
    /// Whether this guard still holds the slot (an initiator's slot can be taken over by its peer's crossing attempt).
    pub fn holds(&self) -> bool {
        guard(&self.handle.pairing).holder == Some(self.ticket)
    }

    /// This initiator is about to send its `PairHello` to `peer`: until [`Self::withdraw_offer`], that same device's
    /// crossing attempt may take the slot over ([`TrustHandle::cross_pairing`]). The returned notification fires
    /// when it did.
    pub fn offer_to(&self, peer: DeviceId) -> Arc<Notify> {
        let gave_way = Arc::new(Notify::new());
        let mut slot = guard(&self.handle.pairing);
        if slot.holder == Some(self.ticket) {
            slot.offer = Some(Offer { peer, refused: false, gave_way: Arc::clone(&gave_way) });
        }
        gave_way
    }

    /// The peer refused this initiator `BUSY`: if its own attempt arrives now, this side gives way whatever the IDs.
    pub fn refused_by_peer(&self) {
        let mut slot = guard(&self.handle.pairing);
        if slot.holder == Some(self.ticket)
            && let Some(offer) = slot.offer.as_mut()
        {
            offer.refused = true;
        }
    }

    /// The peer committed to a nonce: from here the ceremony is this initiator's and cannot be taken over. False when
    /// it was taken over first.
    pub fn withdraw_offer(&self) -> bool {
        let mut slot = guard(&self.handle.pairing);
        if slot.holder != Some(self.ticket) {
            return false;
        }
        slot.offer = None;
        true
    }
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl TrustHandle {
    /// Loads `trust.bin`, or starts empty when there is none. A blob that does not decode is an error: the
    /// service must not silently replace a store it cannot read.
    pub fn open(secrets: Arc<dyn SecretStore>) -> Result<Arc<Self>> {
        let store = match secrets.load(TRUST_NAME)? {
            Some(bytes) => TrustStore::decode(&bytes)?,
            None => TrustStore::default(),
        };
        // A `blocked.bin` that cannot be read starts an empty list (the next block writes a good one): losing the
        // list costs a device's block, while refusing to start would cost every pairing and every session.
        let blocks =
            secrets.load(BLOCKED_NAME).ok().flatten().and_then(|b| BlockList::decode(&b).ok()).unwrap_or_default();
        Ok(Arc::new(Self {
            store: Mutex::new(store),
            writer: Mutex::new(()),
            secrets,
            pairing: Mutex::new(Slot::default()),
            blocks: Mutex::new(blocks),
        }))
    }

    pub fn read<R>(&self, f: impl FnOnce(&TrustStore) -> R) -> R {
        f(&guard(&self.store))
    }

    /// Applies `f` and writes the store. If the write fails the change is undone and the error returned.
    pub fn update<R>(&self, f: impl FnOnce(&mut TrustStore) -> R) -> Result<R> {
        self.try_update(|s| Ok(f(s)))
    }

    /// Like [`Self::update`], for a change that may refuse: an `Err` from `f` leaves the store as it was and writes
    /// nothing, so a change that validates half-way never persists the half it already applied.
    pub fn try_update<R>(&self, f: impl FnOnce(&mut TrustStore) -> Result<R>) -> Result<R> {
        let _writing = guard(&self.writer);
        // Nobody else changes the store while `_writing` is held, so this copy is the current state.
        let mut next = guard(&self.store).clone();
        let out = f(&mut next)?;
        let bytes = next.encode().map_err(TrustError::Codec)?;
        self.secrets.store(TRUST_NAME, &bytes).map_err(TrustError::Secret)?;
        *guard(&self.store) = next;
        Ok(out)
    }

    /// [`Self::update`] for an async caller: the change and the write (`trust.bin` goes through DPAPI and the disk) run
    /// on a blocking thread, never on one of the runtime's few workers.
    pub async fn update_blocking<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut TrustStore) -> R + Send + 'static,
    ) -> Result<R> {
        self.try_update_blocking(move |s| Ok(f(s))).await
    }

    /// [`Self::try_update`] for an async caller, on a blocking thread (see [`Self::update_blocking`]).
    pub async fn try_update_blocking<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut TrustStore) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.try_update(f)).await.unwrap_or_else(|e| {
            Err(TrustError::Secret(PlatformError {
                kind: ErrorKind::Os,
                op: "TrustHandle::update",
                os_code: None,
                detail: format!("the write task ended early: {e}"),
            }))
        })
    }

    /// The devices blocked by MAC, as they are now.
    pub fn blocked_devices(&self) -> Vec<BlockedDevice> {
        guard(&self.blocks).devices.clone()
    }

    /// Whether any device is blocked by MAC: a connection is looked up in the neighbour table only then.
    pub fn any_blocked_mac(&self) -> bool {
        !guard(&self.blocks).devices.is_empty()
    }

    pub fn is_blocked_mac(&self, mac: &[u8; 6]) -> bool {
        guard(&self.blocks).contains(mac)
    }

    /// Blocks a device by its MAC and writes `blocked.bin`; false when the list is full. A failed write changes
    /// nothing.
    pub fn block_mac(&self, device: BlockedDevice) -> Result<bool> {
        self.change_blocks(|b| b.block(device))
    }

    /// Unblocks a MAC and writes `blocked.bin`; true if it was blocked.
    pub fn unblock_mac(&self, mac: &[u8; 6]) -> Result<bool> {
        self.change_blocks(|b| b.unblock(mac))
    }

    fn change_blocks(&self, f: impl FnOnce(&mut BlockList) -> bool) -> Result<bool> {
        let _writing = guard(&self.writer);
        let mut next = guard(&self.blocks).clone();
        let out = f(&mut next);
        let bytes = next.encode().map_err(TrustError::Codec)?;
        self.secrets.store(BLOCKED_NAME, &bytes).map_err(TrustError::Secret)?;
        *guard(&self.blocks) = next;
        Ok(out)
    }

    pub fn snapshot(&self) -> TrustStore {
        self.read(Clone::clone)
    }

    /// Takes the machine's pairing slot, or `None` when an attempt (either role) already holds it.
    pub fn try_begin_pairing(self: &Arc<Self>) -> Option<PairingGuard> {
        let mut slot = guard(&self.pairing);
        if slot.holder.is_some() {
            return None;
        }
        Some(self.hand_slot(&mut slot))
    }

    fn hand_slot(self: &Arc<Self>, slot: &mut Slot) -> PairingGuard {
        let ticket = slot.next_ticket;
        slot.next_ticket = slot.next_ticket.wrapping_add(1);
        slot.holder = Some(ticket);
        slot.offer = None;
        PairingGuard { handle: Arc::clone(self), ticket }
    }

    /// The responder's way to the slot, for a `PairHello` from `peer` (its key proven by the handshake). A free slot
    /// is taken. When the slot is this machine's initiator's, dialling that same device and still without an answer,
    /// both users pressed "Parear" at once: the side with the higher device ID gives way (both sides compare the same
    /// two IDs, so exactly one does), and so does a side the peer already refused `BUSY` (a peer of 1.0 or 1.1.0 never
    /// gives way). Nothing of the ceremony's checks is skipped: the responder that takes over runs every one of them.
    pub fn cross_pairing(self: &Arc<Self>, peer: DeviceId, local: DeviceId) -> Crossing {
        let mut slot = guard(&self.pairing);
        if slot.holder.is_none() {
            return Crossing::Held(self.hand_slot(&mut slot));
        }
        let Some(offer) = slot.offer.as_ref().filter(|o| o.peer == peer) else { return Crossing::Busy };
        if !(local > peer || offer.refused) {
            return Crossing::Kept;
        }
        let gave_way = Arc::clone(&offer.gave_way);
        let taken = self.hand_slot(&mut slot);
        drop(slot);
        // A permit is stored when the initiator is not waiting yet: it is never lost.
        gave_way.notify_one();
        Crossing::Held(taken)
    }

    /// Milliseconds the pairing lockout has left at `now` (wall-clock), for a check that refuses while locked. A record
    /// a clock set back left in the future is rebased and written first (`Lockout::rebase`), so what is left counts
    /// down from here instead of staying at the lockout's length until the clock catches up (review of 22/11/2026,
    /// A10). Should that write fail the answer is still bounded by the lockout's length.
    pub fn pairing_locked_for(&self, now: u64) -> Option<u64> {
        if self.read(|s| s.lockout.needs_rebase(now)) {
            let _ = self.update(|s| s.lockout.rebase(now));
        }
        self.read(|s| s.lockout.remaining_ms(now))
    }

    /// [`Self::pairing_locked_for`] for an async caller: the write, when there is one, runs on a blocking thread.
    pub async fn pairing_locked_for_async(self: &Arc<Self>, now: u64) -> Option<u64> {
        if self.read(|s| s.lockout.needs_rebase(now)) {
            let _ = self.update_blocking(move |s| s.lockout.rebase(now)).await;
        }
        self.read(|s| s.lockout.remaining_ms(now))
    }

    /// True while an attempt holds the slot.
    pub fn pairing_in_progress(&self) -> bool {
        guard(&self.pairing).holder.is_some()
    }
}

impl PinLookup for TrustHandle {
    fn pinned(&self, spki: &[u8]) -> Option<DeviceId> {
        self.read(|s| s.find_by_spki(spki).map(|d| d.device_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::spki_from_public_key;
    use crate::records::PairedDevice;
    use swapdesk_fakes::MemorySecretStore;
    use swapdesk_proto::Os;

    fn dev(seed: u8) -> PairedDevice {
        PairedDevice::new_paired(spki_from_public_key(&[seed; 32]), "pc".into(), Os::MacOs, "15".into(), 1, None)
    }

    #[test]
    fn changes_persist_and_reload() {
        let secrets = Arc::new(MemorySecretStore::new());
        let h = TrustHandle::open(secrets.clone()).unwrap();
        h.update(|s| s.devices.push(dev(1))).unwrap();
        assert!(h.pinned(&spki_from_public_key(&[1; 32])).is_some());
        let again = TrustHandle::open(secrets).unwrap();
        assert_eq!(again.snapshot(), h.snapshot());
    }

    /// D65: the blocked MACs live in `blocked.bin`, apart from `trust.bin` (an older build restored by the updater
    /// reads `trust.bin` strictly), and survive a restart; an unreadable `blocked.bin` starts empty.
    #[test]
    fn blocked_macs_persist_in_their_own_file() {
        let secrets = Arc::new(MemorySecretStore::new());
        let h = TrustHandle::open(secrets.clone()).unwrap();
        let trust_before = secrets.load(TRUST_NAME).unwrap();
        let d =
            BlockedDevice { mac: [2, 0, 0, 0, 0, 9], last_ipv4: [192, 168, 0, 9], name: "x".into(), blocked_at_ms: 5 };
        assert!(h.block_mac(d.clone()).unwrap());
        assert_eq!(secrets.load(TRUST_NAME).unwrap(), trust_before, "trust.bin is not touched");
        let again = TrustHandle::open(secrets.clone()).unwrap();
        assert!(again.is_blocked_mac(&d.mac) && again.any_blocked_mac());
        assert_eq!(again.blocked_devices(), vec![d.clone()]);
        assert!(again.unblock_mac(&d.mac).unwrap());
        assert!(!TrustHandle::open(secrets.clone()).unwrap().any_blocked_mac());

        secrets.store(BLOCKED_NAME, &[9, 9]).unwrap();
        let h = TrustHandle::open(secrets).expect("an unreadable blocked.bin never stops the service");
        assert!(!h.any_blocked_mac());
    }

    #[test]
    fn a_corrupt_blob_is_an_error_not_an_empty_store() {
        let secrets = Arc::new(MemorySecretStore::new());
        secrets.store(TRUST_NAME, &[9, 9, 9]).unwrap();
        assert!(TrustHandle::open(secrets).is_err());
    }

    struct FailingStore;
    impl SecretStore for FailingStore {
        fn load(&self, _: &str) -> swapdesk_platform::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn store(&self, _: &str, _: &[u8]) -> swapdesk_platform::Result<()> {
            Err(PlatformError { kind: ErrorKind::Os, op: "test", os_code: None, detail: "disk full".into() })
        }
        fn delete(&self, _: &str) -> swapdesk_platform::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_write_undoes_the_change() {
        let h = TrustHandle::open(Arc::new(FailingStore)).unwrap();
        assert!(h.update(|s| s.devices.push(dev(1))).is_err());
        assert!(h.read(|s| s.devices.is_empty()));
    }

    /// A store whose `store` call stops until the test lets it go, to hold a write "on the disk".
    struct SlowStore {
        entered: Mutex<std::sync::mpsc::Sender<()>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl SecretStore for SlowStore {
        fn load(&self, _: &str) -> swapdesk_platform::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn store(&self, _: &str, _: &[u8]) -> swapdesk_platform::Result<()> {
            guard(&self.entered).send(()).unwrap();
            let _ = guard(&self.release).recv_timeout(std::time::Duration::from_secs(10));
            Ok(())
        }
        fn delete(&self, _: &str) -> swapdesk_platform::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_verifier_lookup_is_not_blocked_by_a_slow_store_write() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let h =
            TrustHandle::open(Arc::new(SlowStore { entered: Mutex::new(entered_tx), release: Mutex::new(release_rx) }))
                .unwrap();
        let writer = {
            let h = Arc::clone(&h);
            std::thread::spawn(move || h.update(|s| s.devices.push(dev(1))).is_ok())
        };
        entered_rx.recv_timeout(Duration::from_secs(5)).expect("the write reached the store");

        // The write is on the "disk" now: a session handshake asks the verifier whether a key is pinned.
        let (tx, rx) = mpsc::channel();
        let reader = {
            let h = Arc::clone(&h);
            std::thread::spawn(move || tx.send(h.pinned(&spki_from_public_key(&[2; 32]))).unwrap())
        };
        let looked_up = rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        assert!(writer.join().unwrap(), "the write itself succeeds");
        reader.join().unwrap();
        assert!(looked_up.is_ok(), "a lookup waited for the disk write");
        assert!(h.pinned(&spki_from_public_key(&[1; 32])).is_some(), "the change is published once it is written");
    }

    #[test]
    fn an_async_update_runs_on_a_blocking_thread_and_leaves_the_runtime_free() {
        use std::time::Duration;
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let h =
            TrustHandle::open(Arc::new(SlowStore { entered: Mutex::new(entered_tx), release: Mutex::new(release_rx) }))
                .unwrap();
        // One thread runs every task: if the write ran on it, the timer below could not fire while the "disk" is stopped.
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let writer_thread = std::thread::current().id();
        let seen_on = Arc::new(Mutex::new(None));
        let done = rt.block_on(async {
            let write = {
                let (h, seen_on) = (Arc::clone(&h), Arc::clone(&seen_on));
                tokio::spawn(async move {
                    h.update_blocking(move |s| {
                        *guard(&seen_on) = Some(std::thread::current().id());
                        s.devices.push(dev(1));
                    })
                    .await
                })
            };
            tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(5)).unwrap())
                .await
                .unwrap();
            // The write is stopped in the store. The only worker is free: a timer fires, and the task is not finished.
            tokio::time::timeout(Duration::from_millis(300), std::future::pending::<()>()).await.unwrap_err();
            assert!(!write.is_finished());
            release_tx.send(()).unwrap();
            write.await.unwrap().is_ok()
        });
        assert!(done, "the write itself succeeds");
        let ran_on = guard(&seen_on).expect("the closure ran");
        assert_ne!(ran_on, writer_thread);
        assert!(h.pinned(&spki_from_public_key(&[1; 32])).is_some());
    }

    #[test]
    fn concurrent_updates_all_land_and_the_disk_ends_equal_to_memory() {
        let secrets = Arc::new(MemorySecretStore::new());
        let h = TrustHandle::open(secrets.clone()).unwrap();
        let workers: Vec<_> = (0..8u8)
            .map(|w| {
                let h = Arc::clone(&h);
                std::thread::spawn(move || {
                    for i in 0..8u8 {
                        h.update(|s| s.devices.push(dev(w * 8 + i + 1))).unwrap();
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(h.read(|s| s.devices.len()), 64);
        assert_eq!(TrustHandle::open(secrets).unwrap().snapshot(), h.snapshot());
    }

    #[test]
    fn try_update_rolls_back_without_writing_when_the_change_is_refused() {
        let secrets = Arc::new(MemorySecretStore::new());
        let h = TrustHandle::open(secrets.clone()).unwrap();
        h.update(|s| s.devices.push(dev(1))).unwrap();
        let on_disk = secrets.load(TRUST_NAME).unwrap();
        let err = h.try_update(|s| {
            s.devices.clear();
            Err::<(), _>(TrustError::NotInPairingMode)
        });
        assert!(matches!(err, Err(TrustError::NotInPairingMode)));
        assert_eq!(h.read(|s| s.devices.len()), 1, "a refused change leaves the store as it was");
        assert_eq!(secrets.load(TRUST_NAME).unwrap(), on_disk, "and writes nothing");
        assert_eq!(h.try_update(|s| Ok(s.devices.len())).unwrap(), 1);
    }

    /// Review of 22/11/2026, A10: the check that refuses a locked pairing writes the rebased lockout, so after a clock
    /// set back the time left runs out one lockout length later instead of when the clock catches up.
    #[test]
    fn the_lockout_check_writes_a_record_the_clock_left_in_the_future() {
        use crate::lockout::BASE_LOCK_MS;
        let secrets = Arc::new(MemorySecretStore::new());
        let h = TrustHandle::open(secrets.clone()).unwrap();
        let t: u64 = 1_800_000_000_000;
        h.update(|s| {
            for i in 0..3 {
                s.lockout.begin(t + i).unwrap();
                s.lockout.fail(t + i);
            }
        })
        .unwrap();
        let back = t - 365 * 24 * 60 * 60 * 1000;
        assert_eq!(h.pairing_locked_for(back), Some(BASE_LOCK_MS));
        let on_disk = TrustHandle::open(secrets).unwrap().snapshot().lockout;
        assert_eq!(on_disk.locked_until_ms, back + BASE_LOCK_MS, "the rebased end is on disk");
        assert_eq!(h.pairing_locked_for(back + 1000), Some(BASE_LOCK_MS - 1000), "and counts down");
        assert_eq!(h.pairing_locked_for(back + BASE_LOCK_MS), None);
    }

    #[test]
    fn one_pairing_at_a_time_across_every_role() {
        let h = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let first = h.try_begin_pairing().expect("the slot is free");
        assert!(h.pairing_in_progress());
        assert!(h.try_begin_pairing().is_none(), "a second attempt, in either role, finds it taken");
        drop(first);
        assert!(!h.pairing_in_progress());
        assert!(h.try_begin_pairing().is_some());
    }
}
