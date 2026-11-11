//! The update core of Swapdesk (`docs/UPDATES.md` 3, 4 and 13): what a release says, how it is trusted and what is
//! offered.
//!
//! * [`Version`]: `MAJOR.MINOR.PATCH` with an optional pre-release (`1.1.0-preview.1`), compared as numbers.
//! * [`manifest`]: the release manifest and its Ed25519 signature. The manifest is trusted only when a key compiled
//!   into this build ([`keys::RELEASE_KEYS`]) signed `"swapdesk-update-manifest\n"` followed by its exact bytes.
//! * [`package`]: the `.sdpkg` update package (a magic, a count, then per entry a path, a mode, a size and the bytes),
//!   read only after its size and SHA-256 equal the manifest's.
//! * [`policy`]: never a version equal to or lower than the running one, stable only.
//!
//! No OS call, no network and no `unsafe`: the crate builds on every host. The HTTPS clients and the swap of the
//! installed files live in the platform crates.

pub mod http;
pub mod keys;
pub mod manifest;
pub mod package;
pub mod policy;
mod version;

pub use manifest::{Apply, Channel, FileEntry, Manifest};
pub use package::{Entry, Package};
pub use policy::{Offer, offer};
pub use version::Version;

/// Why an update was refused. Never carries the bytes it was refused for (they may be anything).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// Longer than the bound of its kind.
    TooLarge,
    /// Not the expected text, or a field that does not read.
    Malformed(&'static str),
    /// A key that is unknown, or repeated.
    BadKey,
    /// No compiled key has this id.
    UnknownKey,
    /// The signature does not verify.
    BadSignature,
    /// The manifest names nothing for this system.
    NoEntry,
    /// The package's size or hash is not the manifest's.
    Mismatch,
    /// A package path that is not allowed, or a duplicate.
    BadPath,
}

impl core::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("too large"),
            Self::Malformed(what) => write!(f, "malformed: {what}"),
            Self::BadKey => f.write_str("an unknown or repeated key"),
            Self::UnknownKey => f.write_str("signed by an unknown key"),
            Self::BadSignature => f.write_str("invalid update signature"),
            Self::NoEntry => f.write_str("nothing for this system"),
            Self::Mismatch => f.write_str("size or hash differs from the manifest"),
            Self::BadPath => f.write_str("a path that is not allowed"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `N` bytes from exactly `2 * N` lowercase hex digits.
pub(crate) fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let b = s.as_bytes();
    if b.len() != 2 * N {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = digit(b[2 * i])? << 4 | digit(b[2 * i + 1])?;
    }
    Some(out)
}

/// The SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}
