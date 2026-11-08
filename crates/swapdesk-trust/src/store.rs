//! The trust store in memory, persisted through a `SecretStore` (`trust.bin`, C5.2) on every change.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use swapdesk_platform::SecretStore;
use swapdesk_proto::Wire;

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
    pairing: AtomicBool,
}

/// The one "pairing in progress" slot of this machine, shared by the initiator and the responder role. Held for
/// the whole attempt; dropping it frees the slot.
pub struct PairingGuard {
    handle: Arc<TrustHandle>,
}

impl Drop for PairingGuard {
    fn drop(&mut self) {
        self.handle.pairing.store(false, Ordering::SeqCst);
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
        Ok(Arc::new(Self {
            store: Mutex::new(store),
            writer: Mutex::new(()),
            secrets,
            pairing: AtomicBool::new(false),
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

    pub fn snapshot(&self) -> TrustStore {
        self.read(Clone::clone)
    }

    /// Takes the machine's pairing slot, or `None` when an attempt (either role) already holds it.
    pub fn try_begin_pairing(self: &Arc<Self>) -> Option<PairingGuard> {
        self.pairing.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).ok()?;
        Some(PairingGuard { handle: Arc::clone(self) })
    }

    /// True while an attempt holds the slot.
    pub fn pairing_in_progress(&self) -> bool {
        self.pairing.load(Ordering::SeqCst)
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
    use swapdesk_platform::{ErrorKind, PlatformError};
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
