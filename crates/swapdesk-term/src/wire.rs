//! The terminal stream's own header and the mapping of platform failures onto protocol error codes.

use swapdesk_platform::{ErrorKind, PlatformError};
use swapdesk_proto::ErrorCode;

/// Bytes after the stream kind (0x10): `term_id: u32` little-endian (PROTOCOL.md 4); raw PTY bytes follow.
pub const TERM_HEADER_LEN: usize = 4;

/// The header the viewer writes right after the stream kind.
pub fn term_header(term_id: u32) -> [u8; TERM_HEADER_LEN] {
    term_id.to_le_bytes()
}

/// The terminal id of a stream whose kind was already read; `None` while fewer than 4 bytes arrived.
pub fn parse_term_header(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(..TERM_HEADER_LEN)?.try_into().ok()?))
}

/// The protocol error for a platform failure; the detail never goes on the wire.
pub fn code_for(e: &PlatformError) -> ErrorCode {
    match e.kind {
        ErrorKind::PermissionDenied(_) | ErrorKind::Unsupported => ErrorCode::NotPermitted,
        ErrorKind::Unavailable | ErrorKind::AccessLost => ErrorCode::Unavailable,
        ErrorKind::Timeout => ErrorCode::Timeout,
        ErrorKind::InvalidInput => ErrorCode::ProtocolViolation,
        ErrorKind::Closed => ErrorCode::NotFound,
        _ => ErrorCode::IoFailed,
    }
}
