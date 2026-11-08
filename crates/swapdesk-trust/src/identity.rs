//! The device key (PROTOCOL.md 2.1) and its `Identity` record (C5.1).

use core::fmt;

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use swapdesk_platform::SecretStore;
use swapdesk_proto::{CodecError, Reader, Wire, Writer};

use crate::error::{Result, TrustError};
use crate::ids::{DeviceId, SPKI_LEN, Spki, device_id, parse_spki, spki_from_public_key};
use crate::wipe::{Secret, wipe};

/// Record format number (first `u16` of every C5 record).
pub const FORMAT: u16 = 1;
/// Longest PKCS#8 document accepted when decoding (ring writes 85 bytes).
const MAX_PKCS8: usize = 128;
/// Name of the identity blob in the `SecretStore` (`identity.bin`, C5.2).
pub const IDENTITY_NAME: &str = "identity.bin";

/// The long-term Ed25519 device key. The private part never leaves this struct except in its record.
#[derive(Clone)]
pub struct Identity {
    pkcs8: Vec<u8>,
    spki: Spki,
    created_ms: u64,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &crate::ids::device_id_hex(&self.device_id()))
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A new key from the system random source.
    pub fn generate(created_ms: u64) -> Result<Self> {
        let pkcs8 =
            Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| TrustError::Crypto("key generation"))?;
        Self::from_pkcs8(pkcs8.as_ref().to_vec(), created_ms)
    }

    pub fn from_pkcs8(mut pkcs8: Vec<u8>, created_ms: u64) -> Result<Self> {
        let pair = match Ed25519KeyPair::from_pkcs8(&pkcs8) {
            Ok(pair) => pair,
            Err(_) => {
                wipe(&mut pkcs8);
                return Err(TrustError::Crypto("key parse"));
            }
        };
        let mut public = [0u8; 32];
        public.copy_from_slice(pair.public_key().as_ref());
        Ok(Self { pkcs8, spki: spki_from_public_key(&public), created_ms })
    }

    pub fn spki(&self) -> &Spki {
        &self.spki
    }

    pub fn device_id(&self) -> DeviceId {
        device_id(&self.spki)
    }

    pub fn created_ms(&self) -> u64 {
        self.created_ms
    }

    /// The PKCS#8 document, for the TLS signing key only.
    pub(crate) fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// Loads `identity.bin` through the store, or makes and stores a new key. The record's bytes (they hold the
    /// private key) are wiped as soon as they are decoded or written.
    pub fn load_or_create(store: &dyn SecretStore, now_ms: u64) -> Result<Self> {
        if let Some(bytes) = store.load(IDENTITY_NAME)? {
            let bytes = Secret(bytes);
            return Ok(Self::decode(&bytes)?);
        }
        let id = Self::generate(now_ms)?;
        let record = Secret(id.encode()?);
        store.store(IDENTITY_NAME, &record)?;
        Ok(id)
    }

    /// Zeroes the private key bytes of this value (every clone owns its own copy and is wiped on its own drop).
    fn wipe_key(&mut self) {
        wipe(&mut self.pkcs8);
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.wipe_key();
    }
}

impl Wire for Identity {
    const MIN_SIZE: usize = 2 + 1 + SPKI_LEN + 8;

    fn put(&self, w: &mut Writer, _lim: usize) -> swapdesk_proto::codec::Result<()> {
        w.put_u16(FORMAT);
        w.put_varint(self.pkcs8.len() as u64)?;
        w.put_bytes(&self.pkcs8);
        w.put_bytes(&self.spki);
        w.put_u64(self.created_ms);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> swapdesk_proto::codec::Result<Self> {
        if r.u16()? != FORMAT {
            return Err(CodecError::InvalidValue("identity format"));
        }
        let len = r.varint()?;
        if len > MAX_PKCS8 as u64 {
            return Err(CodecError::OverLimit { what: "pkcs8 length", limit: MAX_PKCS8 as u64 });
        }
        let pkcs8 = r.take(len as usize)?.to_vec();
        let spki: Spki = r.array()?;
        let created_ms = r.u64()?;
        if parse_spki(&spki).is_none() {
            return Err(CodecError::InvalidValue("identity spki"));
        }
        let id = Self::from_pkcs8(pkcs8, created_ms).map_err(|_| CodecError::InvalidValue("identity key"))?;
        if id.spki != spki {
            return Err(CodecError::InvalidValue("identity spki does not match the key"));
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_fakes::MemorySecretStore;

    #[test]
    fn record_round_trips_and_spki_is_44_bytes() {
        let id = Identity::generate(1234).unwrap();
        assert_eq!(id.spki().len(), 44);
        let bytes = id.encode().unwrap();
        let back = Identity::decode(&bytes).unwrap();
        assert_eq!(back.spki(), id.spki());
        assert_eq!(back.created_ms(), 1234);
        assert_eq!(back.encode().unwrap(), bytes);
    }

    #[test]
    fn a_record_whose_spki_does_not_match_its_key_is_rejected() {
        let a = Identity::generate(1).unwrap();
        let b = Identity::generate(1).unwrap();
        let mut bytes = a.encode().unwrap();
        let n = bytes.len();
        bytes[n - 8 - SPKI_LEN..n - 8].copy_from_slice(b.spki());
        assert!(Identity::decode(&bytes).is_err());
        let mut trailing = a.encode().unwrap();
        trailing.push(0);
        assert_eq!(Identity::decode(&trailing).unwrap_err(), CodecError::TrailingBytes);
    }

    #[test]
    fn load_or_create_keeps_the_same_key() {
        let store = MemorySecretStore::new();
        let first = Identity::load_or_create(&store, 5).unwrap();
        let second = Identity::load_or_create(&store, 99).unwrap();
        assert_eq!(first.spki(), second.spki());
        assert_eq!(second.created_ms(), 5);
        assert_eq!(store.names(), vec![IDENTITY_NAME.to_string()]);
    }

    #[test]
    fn wiping_zeroes_the_private_key_bytes_of_that_value_only() {
        let mut id = Identity::generate(1).unwrap();
        let copy = id.clone();
        assert!(id.pkcs8.iter().any(|&b| b != 0));
        id.wipe_key();
        assert!(id.pkcs8.iter().all(|&b| b == 0), "the private key stayed in memory after the wipe");
        assert!(copy.pkcs8.iter().any(|&b| b != 0), "a clone keeps its own bytes until its own drop");
    }

    #[test]
    fn a_key_that_does_not_parse_is_an_error() {
        assert!(Identity::from_pkcs8(vec![1, 2, 3], 0).is_err());
    }

    #[test]
    fn debug_never_shows_the_private_key() {
        let id = Identity::generate(1).unwrap();
        let shown = format!("{id:?}");
        assert!(!shown.contains("pkcs8"));
    }
}
