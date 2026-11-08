//! SPKI and device ID (PROTOCOL.md 2.1).

use ring::digest;

/// `SubjectPublicKeyInfo` of an Ed25519 key, RFC 8410 form: 12 bytes of header and the 32-byte key.
pub const SPKI_LEN: usize = 44;
const SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

pub type Spki = [u8; SPKI_LEN];
pub type DeviceId = [u8; 16];

/// The SPKI of a raw Ed25519 public key.
pub fn spki_from_public_key(public: &[u8; 32]) -> Spki {
    let mut spki = [0u8; SPKI_LEN];
    spki[..12].copy_from_slice(&SPKI_PREFIX);
    spki[12..].copy_from_slice(public);
    spki
}

/// `Some(spki)` only for exactly the 44-byte Ed25519 form; anything else is not a device key.
pub fn parse_spki(bytes: &[u8]) -> Option<Spki> {
    let spki: Spki = bytes.try_into().ok()?;
    (spki[..12] == SPKI_PREFIX).then_some(spki)
}

/// The raw 32-byte Ed25519 key inside an SPKI.
pub fn public_key_of(spki: &Spki) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&spki[12..]);
    out
}

/// The first 16 bytes of SHA-256(SPKI).
pub fn device_id(spki: &[u8]) -> DeviceId {
    let d = digest::digest(&digest::SHA256, spki);
    let mut id = [0u8; 16];
    id.copy_from_slice(&d.as_ref()[..16]);
    id
}

/// 32 lowercase hex characters, as shown in mDNS TXT `fp` and logs.
pub fn device_id_hex(id: &DeviceId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spki_parse_accepts_only_the_ed25519_form() {
        let spki = spki_from_public_key(&[7u8; 32]);
        assert_eq!(parse_spki(&spki), Some(spki));
        assert_eq!(public_key_of(&spki), [7u8; 32]);
        assert_eq!(parse_spki(&spki[..43]), None);
        let mut long = spki.to_vec();
        long.push(0);
        assert_eq!(parse_spki(&long), None);
        let mut other_oid = spki;
        other_oid[8] ^= 1;
        assert_eq!(parse_spki(&other_oid), None);
    }

    #[test]
    fn device_id_is_the_sha256_prefix() {
        let spki = spki_from_public_key(&[1u8; 32]);
        assert_eq!(device_id(&spki)[..], sha256(&spki)[..16]);
        assert_eq!(device_id_hex(&device_id(&spki)).len(), 32);
    }
}
