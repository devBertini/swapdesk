//! The release keys this build trusts (`docs/UPDATES.md` 4.4): the public halves of the signing key A and of the
//! rotation key B, compiled in. A manifest signed by either verifies.
//!
//! The keys are made once by the owner with `swapdesk-release-sign` (slice U2); until that ceremony this list is
//! empty and no manifest verifies, so a build without keys can never install anything. Tests use keys of their own,
//! passed to [`crate::manifest::verify_with`].

/// An Ed25519 public key (32 bytes).
pub type PublicKey = [u8; 32];

/// The keys of the releases. Filled by the key ceremony of slice U2.
pub const RELEASE_KEYS: &[PublicKey] = &[];

/// A key's id in the signature file: the first 8 bytes of the SHA-256 of the public key.
pub fn key_id(key: &PublicKey) -> [u8; 8] {
    let d = crate::sha256(key);
    let mut id = [0u8; 8];
    id.copy_from_slice(&d[..8]);
    id
}
