//! The trust store records of C5.1, encoded with the PROTOCOL.md 9.2 codec rules (`format: u16` = 1 first).

use swapdesk_proto::codec::Result;
use swapdesk_proto::{CodecError, Os, PermissionSet, Reader, Wire, Writer};

use crate::ids::{DeviceId, SPKI_LEN, Spki, device_id, parse_spki};

pub use crate::identity::FORMAT;

pub const MAX_DEVICES: usize = 64;
pub const MAX_BLOCKED: usize = 256;
pub const MAX_FAILURES: usize = 8;
/// Name of the trust store blob in the `SecretStore` (`trust.bin`, C5.2).
pub const TRUST_NAME: &str = "trust.bin";

/// Who granted `UNATTENDED`: a Windows SID string or a macOS uid, with the account name (D13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRef {
    pub id: String,
    pub name: String,
}

impl Wire for UserRef {
    const MIN_SIZE: usize = 2;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_str(&self.id, 128)?;
        w.put_str(&self.name, 64)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        Ok(Self { id: r.str(128)?, name: r.str(64)? })
    }
}

/// One paired device. `grants_here` is authoritative and **empty right after pairing**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub device_id: DeviceId,
    pub spki: Spki,
    pub name: String,
    pub os: Os,
    pub os_version: String,
    pub paired_ms: u64,
    pub last_seen_ms: u64,
    pub last_addrs: Vec<[u8; 4]>,
    pub wol_macs: Vec<[u8; 6]>,
    pub grants_here: PermissionSet,
    pub unattended_granted_by: Option<UserRef>,
    pub grants_there: Option<PermissionSet>,
}

impl PairedDevice {
    /// A device as it is right after a pairing ceremony: no grants, no Wake-on-LAN addresses.
    pub fn new_paired(
        spki: Spki,
        name: String,
        os: Os,
        os_version: String,
        now_ms: u64,
        addr: Option<[u8; 4]>,
    ) -> Self {
        Self {
            device_id: device_id(&spki),
            spki,
            name,
            os,
            os_version,
            paired_ms: now_ms,
            last_seen_ms: now_ms,
            last_addrs: addr.into_iter().collect(),
            wol_macs: Vec::new(),
            grants_here: PermissionSet::EMPTY,
            unattended_granted_by: None,
            grants_there: None,
        }
    }
}

impl Wire for PairedDevice {
    const MIN_SIZE: usize = 16 + SPKI_LEN + 1 + 1 + 1 + 8 + 8 + 1 + 1 + 2 + 1 + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        self.device_id.put(w, 0)?;
        self.spki.put(w, 0)?;
        w.put_str(&self.name, 64)?;
        self.os.put(w, 0)?;
        w.put_str(&self.os_version, 32)?;
        w.put_u64(self.paired_ms);
        w.put_u64(self.last_seen_ms);
        self.last_addrs.put(w, 4)?;
        self.wol_macs.put(w, 8)?;
        self.grants_here.put(w, 0)?;
        self.unattended_granted_by.put(w, 0)?;
        self.grants_there.put(w, 0)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let d = Self {
            device_id: r.array()?,
            spki: r.array()?,
            name: r.str(64)?,
            os: Os::get(r, 0)?,
            os_version: r.str(32)?,
            paired_ms: r.u64()?,
            last_seen_ms: r.u64()?,
            last_addrs: <Vec<[u8; 4]>>::get(r, 4)?,
            wol_macs: <Vec<[u8; 6]>>::get(r, 8)?,
            grants_here: PermissionSet::get(r, 0)?,
            unattended_granted_by: <Option<UserRef>>::get(r, 0)?,
            grants_there: <Option<PermissionSet>>::get(r, 0)?,
        };
        if parse_spki(&d.spki).is_none() {
            return Err(CodecError::InvalidValue("paired device spki"));
        }
        if device_id(&d.spki) != d.device_id {
            return Err(CodecError::InvalidValue("paired device id does not match its key"));
        }
        Ok(d)
    }
}

/// Failed pairing attempts and the lockout they cause (PROTOCOL.md 3). Logic in `lockout.rs`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lockout {
    /// Unix ms of recent failed attempts.
    pub failures: Vec<u64>,
    /// An attempt in progress past step 2, counted as a failure until it pins.
    pub provisional: Option<u64>,
    pub locked_until_ms: u64,
    pub lockouts_24h: u8,
}

impl Wire for Lockout {
    const MIN_SIZE: usize = 1 + 1 + 8 + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        self.failures.put(w, MAX_FAILURES)?;
        self.provisional.put(w, 0)?;
        w.put_u64(self.locked_until_ms);
        w.put_u8(self.lockouts_24h);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        Ok(Self {
            failures: <Vec<u64>>::get(r, MAX_FAILURES)?,
            provisional: <Option<u64>>::get(r, 0)?,
            locked_until_ms: r.u64()?,
            lockouts_24h: r.u8()?,
        })
    }
}

/// `trust.bin`: the paired devices, the lockout and the blocked addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustStore {
    pub devices: Vec<PairedDevice>,
    pub lockout: Lockout,
    pub blocked: Vec<[u8; 4]>,
}

impl TrustStore {
    pub fn find_by_spki(&self, spki: &[u8]) -> Option<&PairedDevice> {
        self.devices.iter().find(|d| d.spki == spki)
    }

    pub fn find_by_id(&self, id: &DeviceId) -> Option<&PairedDevice> {
        self.devices.iter().find(|d| &d.device_id == id)
    }

    /// Removes a device and its grants (F-PAIR-04); true if it was there.
    pub fn forget(&mut self, id: &DeviceId) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| &d.device_id != id);
        self.devices.len() != before
    }
}

impl Wire for TrustStore {
    const MIN_SIZE: usize = 2 + 1 + Lockout::MIN_SIZE + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_u16(FORMAT);
        self.devices.put(w, MAX_DEVICES)?;
        self.lockout.put(w, 0)?;
        self.blocked.put(w, MAX_BLOCKED)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        if r.u16()? != FORMAT {
            return Err(CodecError::InvalidValue("trust store format"));
        }
        let devices = <Vec<PairedDevice>>::get(r, MAX_DEVICES)?;
        for (i, d) in devices.iter().enumerate() {
            if devices[..i].iter().any(|e| e.device_id == d.device_id) {
                return Err(CodecError::InvalidValue("duplicate paired device"));
            }
        }
        Ok(Self { devices, lockout: Lockout::get(r, 0)?, blocked: <Vec<[u8; 4]>>::get(r, MAX_BLOCKED)? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::spki_from_public_key;

    fn device(seed: u8) -> PairedDevice {
        let spki = spki_from_public_key(&[seed; 32]);
        let mut d = PairedDevice::new_paired(
            spki,
            format!("pc-{seed}"),
            Os::Windows,
            "11".into(),
            1_000,
            Some([192, 168, 0, seed]),
        );
        d.wol_macs.push([1, 2, 3, 4, 5, seed]);
        d
    }

    #[test]
    fn a_new_device_has_empty_grants() {
        let d = device(1);
        assert!(d.grants_here.is_empty());
        assert!(d.unattended_granted_by.is_none());
        assert!(d.grants_there.is_none());
    }

    #[test]
    fn trust_store_round_trips_byte_for_byte() {
        let mut s = TrustStore::default();
        let mut d = device(1);
        d.grants_here = PermissionSet::CONTROL;
        d.unattended_granted_by = Some(UserRef { id: "S-1-5-21-1".into(), name: "claudio".into() });
        d.grants_there = Some(PermissionSet::VIEW);
        s.devices.push(d);
        s.devices.push(device(2));
        s.lockout = Lockout { failures: vec![1, 2], provisional: Some(3), locked_until_ms: 4, lockouts_24h: 1 };
        s.blocked.push([10, 0, 0, 9]);
        let bytes = s.encode().unwrap();
        let back = TrustStore::decode(&bytes).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.encode().unwrap(), bytes);
        assert_eq!(&bytes[..2], &[1, 0]);
    }

    #[test]
    fn every_truncation_and_a_trailing_byte_are_rejected() {
        let mut s = TrustStore::default();
        s.devices.push(device(1));
        let bytes = s.encode().unwrap();
        for cut in 0..bytes.len() {
            assert!(TrustStore::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut more = bytes.clone();
        more.push(0);
        assert_eq!(TrustStore::decode(&more).unwrap_err(), CodecError::TrailingBytes);
    }

    #[test]
    fn a_device_id_that_does_not_match_its_key_and_duplicates_are_rejected() {
        let mut s = TrustStore::default();
        let mut d = device(1);
        d.device_id[0] ^= 1;
        s.devices.push(d);
        assert!(TrustStore::decode(&s.encode().unwrap()).is_err());

        let mut s = TrustStore::default();
        s.devices.push(device(1));
        s.devices.push(device(1));
        assert!(TrustStore::decode(&s.encode().unwrap()).is_err());
    }

    #[test]
    fn limits_hold_when_encoding_and_decoding() {
        let mut s = TrustStore::default();
        for i in 0..=MAX_DEVICES as u8 {
            s.devices.push(device(i));
        }
        assert!(s.encode().is_err());
        // A huge declared device count with no bytes behind it costs nothing.
        let mut huge = vec![1u8, 0];
        huge.extend_from_slice(&[0xFF; 8]);
        assert!(TrustStore::decode(&huge).is_err());
    }

    #[test]
    fn forget_removes_the_device_and_its_grants() {
        let mut s = TrustStore::default();
        s.devices.push(device(1));
        let id = s.devices[0].device_id;
        assert!(s.forget(&id));
        assert!(!s.forget(&id));
        assert!(s.find_by_id(&id).is_none());
    }
}
