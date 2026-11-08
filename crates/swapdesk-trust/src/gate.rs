//! The post-handshake gate (PROTOCOL.md 2.4): after every handshake and before any stream, the peer's key is read
//! from the connection and looked up again. The gate does not trust that a verifier ran.

use rustls::pki_types::CertificateDer;
use swapdesk_proto::ErrorCode;

use crate::error::{Result, TrustError};
use crate::ids::{DeviceId, Spki, parse_spki};
use crate::store::PinLookup;

fn close_with(conn: &quinn::Connection, code: ErrorCode) {
    conn.close(quinn::VarInt::from_u32(u32::from(code.tag())), b"");
}

/// The peer's key as the TLS handshake delivered it: one raw public key of exactly the Ed25519 form.
pub fn peer_spki(conn: &quinn::Connection) -> Option<Spki> {
    let identity = conn.peer_identity()?;
    let certs = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    match certs.as_slice() {
        [one] => parse_spki(one.as_ref()),
        _ => None,
    }
}

/// Session endpoint, server side: the client's key must be pinned. Closes with `UNKNOWN_DEVICE` otherwise.
pub fn gate_session_server(conn: &quinn::Connection, trust: &dyn PinLookup) -> Result<DeviceId> {
    let Some(spki) = peer_spki(conn) else {
        close_with(conn, ErrorCode::UnknownDevice);
        return Err(TrustError::UnknownDevice);
    };
    match trust.pinned(&spki) {
        Some(id) => Ok(id),
        None => {
            close_with(conn, ErrorCode::UnknownDevice);
            Err(TrustError::UnknownDevice)
        }
    }
}

/// Session endpoint, client side: the server's key must be the device it meant to reach.
pub fn gate_session_client(conn: &quinn::Connection, expected: &Spki) -> Result<()> {
    match peer_spki(conn) {
        Some(spki) if &spki == expected => Ok(()),
        _ => {
            close_with(conn, ErrorCode::UnknownDevice);
            Err(TrustError::UnknownDevice)
        }
    }
}

/// Pairing endpoint, either side: the key is recorded for section 3. Closes with `UNKNOWN_DEVICE` when the
/// handshake delivered no well-formed key.
pub fn gate_pairing(conn: &quinn::Connection) -> Result<Spki> {
    peer_spki(conn).ok_or_else(|| {
        close_with(conn, ErrorCode::UnknownDevice);
        TrustError::UnknownDevice
    })
}
