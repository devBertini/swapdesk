//! The system of each paired device that a build before protocol minor 3 does not know (a phone), in `systems.bin`
//! beside `trust.bin`.
//!
//! A file of its own, not a value of `trust.bin`: a build of 1.0 or 1.1 (the one the updater restores when a new
//! version does not start) reads `trust.bin` strictly and knows two systems, so one record with a third would cost it
//! every pairing. `trust.bin` therefore says [`stored`] for such a device, and the system it really has is kept
//! here, in a file that build never opens. Without this file (lost, or written by an older build after a forget)
//! the device reads as a computer of [`stored`]'s system until it says its own again, over a connection its pinned
//! key authenticates.
//!
//! The file changes around `trust.bin`, never in an order a failed write could leave wrong (`TrustHandle::try_update`):
//! what a change adds is written before `trust.bin`, what it removes is taken out after.

use swapdesk_proto::codec::Result;
use swapdesk_proto::{CodecError, Os, Reader, Wire, Writer};

use crate::ids::DeviceId;
use crate::records::{MAX_DEVICES, TrustStore};

/// The systems' blob in the `SecretStore`.
pub const SYSTEMS_NAME: &str = "systems.bin";
/// Format of `systems.bin`.
pub const SYSTEMS_FORMAT: u16 = 1;

/// What `trust.bin` says for a device of `os`: the system itself when every build knows it, the first system of
/// 1.0 otherwise.
pub const fn stored(os: Os) -> Os {
    if os.min_minor() == 0 { os } else { Os::Windows }
}

/// `systems.bin`: the devices whose system `trust.bin` cannot say, each with its system's wire tag. The tag is kept
/// as a number, so an entry of a system this build does not know is carried along instead of failing the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerSystems {
    pub entries: Vec<(DeviceId, u8)>,
}

impl PeerSystems {
    /// The entries `store` needs: one per device whose system is not what `trust.bin` says for it.
    pub fn of(store: &TrustStore) -> Self {
        Self {
            entries: store
                .devices
                .iter()
                .filter(|d| stored(d.os).tag() != d.os.tag())
                .map(|d| (d.device_id, d.os.tag()))
                .collect(),
        }
        .sorted()
    }

    /// The entries in one order, so two lists that say the same compare equal.
    pub fn sorted(mut self) -> Self {
        self.entries.sort_unstable();
        self
    }

    /// What `systems.bin` should hold for `store`: [`Self::of`] it, plus the entries of `disk` (what the file holds
    /// now) whose system this build does not know, for devices still paired. A later build wrote those: this one
    /// carries them, so its own writes do not cost such a device its system.
    pub fn wanted(store: &TrustStore, disk: Option<&Self>) -> Self {
        let mut out = Self::of(store);
        for (id, tag) in disk.map(|d| d.entries.as_slice()).unwrap_or_default() {
            let carried = Os::from_tag(*tag).is_none()
                && store.devices.iter().any(|d| &d.device_id == id)
                && !out.entries.iter().any(|(have, _)| have == id);
            if carried {
                out.entries.push((*id, *tag));
            }
        }
        out.sorted()
    }

    /// `self` plus the entries of `held` for the devices of `paired` that `self` has none for: what the file holds
    /// between the two writes of a change, so that no failed write costs a device that is still paired its system.
    pub fn keeping(&self, held: &Self, paired: &[DeviceId]) -> Self {
        let mut out = self.clone();
        for (id, tag) in &held.entries {
            if paired.contains(id) && !out.entries.iter().any(|(have, _)| have == id) {
                out.entries.push((*id, *tag));
            }
        }
        out.sorted()
    }

    /// Gives each device of `store` that has an entry its real system. An entry of a device that is not paired, or
    /// of a system this build does not know, changes nothing.
    pub fn apply(&self, store: &mut TrustStore) {
        for (id, tag) in &self.entries {
            if let (Some(d), Some(os)) = (store.devices.iter_mut().find(|d| &d.device_id == id), Os::from_tag(*tag)) {
                d.os = os;
            }
        }
    }
}

impl Wire for PeerSystems {
    const MIN_SIZE: usize = 2 + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_u16(SYSTEMS_FORMAT);
        if self.entries.len() > MAX_DEVICES {
            return Err(CodecError::InvalidValue("peer systems count"));
        }
        w.put_u8(self.entries.len() as u8);
        for (id, tag) in &self.entries {
            w.put_bytes(id);
            w.put_u8(*tag);
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        if r.u16()? != SYSTEMS_FORMAT {
            return Err(CodecError::InvalidValue("peer systems format"));
        }
        let count = usize::from(r.u8()?);
        if count > MAX_DEVICES {
            return Err(CodecError::InvalidValue("peer systems count"));
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let id: DeviceId = r.array()?;
            entries.push((id, r.u8()?));
        }
        Ok(Self { entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_build_knows_the_systems_of_1_0_and_a_phone_is_stored_as_the_first_of_them() {
        assert_eq!(stored(Os::Windows), Os::Windows);
        assert_eq!(stored(Os::MacOs), Os::MacOs);
        assert_eq!(stored(Os::Android), Os::Windows);
    }

    #[test]
    fn the_file_round_trips_and_carries_a_system_this_build_does_not_know() {
        let list = PeerSystems { entries: vec![([7; 16], Os::Android.tag()), ([9; 16], 200)] };
        let bytes = list.encode().unwrap();
        assert_eq!(PeerSystems::decode(&bytes).unwrap(), list);
        assert!(PeerSystems::decode(&[9, 9]).is_err(), "another format is refused");
    }
}
