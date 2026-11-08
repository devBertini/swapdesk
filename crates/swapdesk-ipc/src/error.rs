//! One error type for framing, decoding and the handshake.

use core::fmt;
use std::io;

use swapdesk_proto::{CodecError, ErrorCode};

/// Why a message could not be written, read or decoded. Every variant but `Io` is a protocol error: the pipe closes.
#[derive(Debug)]
pub enum IpcError {
    /// The stream failed or ended inside a message.
    Io(io::Error),
    /// A body (or a field of it) the codec rejects, trailing bytes included.
    Codec(CodecError),
    /// A message type this version does not know.
    UnknownType(u64),
    /// A declared or produced body length above the type's limit.
    TooLarge { ty: u64, len: u64, limit: u64 },
    /// The first message of a connection was not `Hello`.
    NotHello { got: u64 },
    /// The answer to `Hello` was not `HelloAck`.
    NotHelloAck { got: u64 },
    /// `Hello` or `HelloAck` carried a version other than [`crate::IPC_VERSION`].
    UnsupportedVersion(u16),
    /// The peer sent `Bye` instead of answering.
    PeerBye(ErrorCode),
    /// The stream ended before the first message.
    Closed,
    /// `UiReply.status` is above the last `ApiCode` + 1.
    BadStatus(u16),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o: {e}"),
            Self::Codec(e) => write!(f, "codec: {e}"),
            Self::UnknownType(t) => write!(f, "unknown message type {t:#x}"),
            Self::TooLarge { ty, len, limit } => {
                write!(f, "message {ty:#x} of {len} bytes is over its limit of {limit}")
            }
            Self::NotHello { got } => write!(f, "first message {got:#x} is not Hello"),
            Self::NotHelloAck { got } => write!(f, "answer {got:#x} is not HelloAck"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported local version {v}"),
            Self::PeerBye(c) => write!(f, "peer closed with {c:?}"),
            Self::Closed => f.write_str("stream closed before the first message"),
            Self::BadStatus(s) => write!(f, "bad reply status {s}"),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Codec(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for IpcError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<CodecError> for IpcError {
    fn from(e: CodecError) -> Self {
        Self::Codec(e)
    }
}

pub type Result<T> = core::result::Result<T, IpcError>;
