use core::fmt;

use swapdesk_platform::PlatformError;
use swapdesk_proto::{CodecError, ErrorCode};

/// Everything `swapdesk-trust` can fail with. No variant carries a secret.
#[derive(Debug)]
pub enum TrustError {
    /// A record or message did not decode or encode.
    Codec(CodecError),
    /// The `SecretStore` failed.
    Secret(PlatformError),
    /// A key, a signature or the random source failed.
    Crypto(&'static str),
    /// TLS or QUIC setup, handshake or I/O failed.
    Tls(String),
    /// A peer or a local check broke the protocol; the code is what the peer is told.
    Protocol(ErrorCode),
    /// The peer ended the attempt with `PairResult { ok: false, code }`.
    Refused(ErrorCode),
    /// Pairing is locked on this machine for `remaining_ms` more.
    Locked { remaining_ms: u64 },
    /// A step took longer than its limit.
    Timeout(&'static str),
    /// A user said the codes differ, or the peer did.
    SasRejected,
    /// Pairing mode is not open on this machine.
    NotInPairingMode,
    /// The peer's key is not (or no longer) in the trust store.
    UnknownDevice,
}

impl TrustError {
    /// The code a peer is told (PROTOCOL.md 8) and the QUIC application close code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Codec(e) => e.code(),
            Self::Secret(_) => ErrorCode::IoFailed,
            Self::Crypto(_) | Self::Tls(_) => ErrorCode::Internal,
            Self::Protocol(c) | Self::Refused(c) => *c,
            Self::Locked { .. } => ErrorCode::PairingLocked,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::SasRejected => ErrorCode::SasRejected,
            Self::NotInPairingMode => ErrorCode::NotPermitted,
            Self::UnknownDevice => ErrorCode::UnknownDevice,
        }
    }
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codec(e) => write!(f, "codec: {e}"),
            Self::Secret(e) => write!(f, "secret store: {e}"),
            Self::Crypto(what) => write!(f, "crypto: {what}"),
            Self::Tls(what) => write!(f, "tls: {what}"),
            Self::Protocol(c) => write!(f, "protocol violation ({c:?})"),
            Self::Refused(c) => write!(f, "refused by the peer ({c:?})"),
            Self::Locked { remaining_ms } => write!(f, "pairing locked for {remaining_ms} ms"),
            Self::Timeout(what) => write!(f, "timeout: {what}"),
            Self::SasRejected => f.write_str("the codes did not match"),
            Self::NotInPairingMode => f.write_str("not in pairing mode"),
            Self::UnknownDevice => f.write_str("unknown device"),
        }
    }
}

impl std::error::Error for TrustError {}

impl From<CodecError> for TrustError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

impl From<PlatformError> for TrustError {
    fn from(e: PlatformError) -> Self {
        Self::Secret(e)
    }
}

pub type Result<T> = core::result::Result<T, TrustError>;
