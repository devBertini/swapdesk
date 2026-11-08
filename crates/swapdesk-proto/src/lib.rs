//! Wire protocol of Swapdesk, major version 1: the types, limits and codec of `docs/PROTOCOL.md`.
//! Std only, no I/O and no cryptography: hashing and TLS belong to the callers, which receive the byte strings
//! to hash from [`pairing`].
//!
//! The codec is hand-written (section 9.2): [`codec::Reader`] and [`codec::Writer`] hold the primitives and
//! [`codec::Wire`] gives every type `encode`, `decode` (rejects trailing bytes) and `decode_prefix`.

// Outside tests an unwrap is a crash of the service: return the error or use `expect` with the reason.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

#[macro_use]
pub mod codec;
pub mod control;
pub mod media;
pub mod pairing;
pub mod types;

pub use codec::{CodecError, Reader, Wire, Writer};
pub use control::*;
pub use media::*;
pub use pairing::*;
pub use types::*;

/// ALPN identifier negotiated on every session QUIC connection; the suffix is the protocol major version.
pub const ALPN: &[u8] = b"swapdesk/1";

/// Protocol major version carried in the handshake.
pub const PROTOCOL_VERSION: u16 = 1;

/// Protocol minor version sent in `Hello` and `HelloAck` (section 1). A peer sends a message type or enum value
/// introduced in minor N only when the other side's minor is at least N.
pub const PROTO_MINOR: u16 = 0;

/// UDP port of the session endpoint (section 1; a GUESS, configurable).
pub const SESSION_PORT: u16 = 47820;

/// UDP port of the pairing endpoint (section 1; a GUESS, configurable).
pub const PAIRING_PORT: u16 = 47821;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpn_carries_the_protocol_version() {
        let expected = format!("swapdesk/{PROTOCOL_VERSION}");
        assert_eq!(ALPN, expected.as_bytes());
        let expected = format!("swapdesk-pair/{PROTOCOL_VERSION}");
        assert_eq!(PAIR_ALPN, expected.as_bytes());
    }
}
