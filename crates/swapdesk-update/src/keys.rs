//! The release keys this build trusts (`docs/UPDATES.md` 4.4): the public halves of the signing key A and of the
//! rotation key B, compiled in. A manifest signed by either verifies.
//!
//! The keys were made once with `swapdesk-release-sign keygen` (slice U2, 13/11/2026): A signs the releases, B only
//! replaces A. Tests use keys of their own, passed to [`crate::manifest::verify_with`].

/// An Ed25519 public key (32 bytes).
pub type PublicKey = [u8; 32];

/// The keys of the releases, from the key ceremony of slice U2.
pub const RELEASE_KEYS: &[PublicKey] = &[
    // A, the signing key (made 13/11/2026; its private half under DPAPI on the owner's PC and one offline copy)
    [
        0xb3, 0x5b, 0xb4, 0xc7, 0xe0, 0xf1, 0xca, 0xbf, 0xb2, 0x49, 0x5b, 0x06, 0x7a, 0x6a, 0x9e, 0x97, 0x5a, 0x8b, 0x8b,
        0x3d, 0x81, 0x84, 0x4b, 0xa5, 0x63, 0xf6, 0x03, 0xf2, 0xc9, 0x66, 0x4d, 0xd2,
    ],
    // B, the rotation key (offline only)
    [
        0xf1, 0xca, 0x8e, 0x67, 0x4e, 0x6f, 0x66, 0xb4, 0xb2, 0xec, 0xa1, 0x44, 0x8c, 0x2f, 0xec, 0x3b, 0x71, 0xa4, 0x3a,
        0xd1, 0x5a, 0xda, 0x6f, 0x5a, 0x3a, 0x54, 0xd5, 0x07, 0x26, 0xc4, 0xe7, 0x2a,
    ],
];

/// A key's id in the signature file: the first 8 bytes of the SHA-256 of the public key.
pub fn key_id(key: &PublicKey) -> [u8; 8] {
    let d = crate::sha256(key);
    let mut id = [0u8; 8];
    id.copy_from_slice(&d[..8]);
    id
}
