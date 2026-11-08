//! Trust of Swapdesk: the device identity, TLS 1.3 with pinned keys, the pairing endpoint and protocol (compared
//! codes), the trust store, the lockout and the audit log records. PROTOCOL.md 2 and 3, contract C5.
//!
//! No `unsafe`. Sockets, the LAN guard and the files on disk belong to other crates: this one takes a
//! `std::net::UdpSocket` and a `SecretStore`.

// Outside tests an unwrap is a crash of the service: return the error or use `expect` with the reason.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

pub mod audit;
pub mod clock;
pub mod endpoint;
pub mod error;
pub mod gate;
pub mod identity;
pub mod ids;
pub mod lockout;
pub mod mode;
pub mod pairing;
pub mod records;
pub mod store;
pub mod tls;
pub mod wipe;

pub use audit::{AuditEntry, AuditEvent, AuditSink, DeviceRef, Direction, MemoryAudit, decode_log};
pub use clock::{ClockWall, SystemWallClock, WallClock};
pub use endpoint::{
    DEFAULT_MAX_IN_FLIGHT, Decision, IncomingFilter, PairingService, ServeOptions, accept_session,
    connect_and_initiate, connect_session, endpoint_from_socket, open_pairing_endpoint, open_session_endpoint,
};
pub use error::{Result, TrustError};
pub use gate::{gate_pairing, gate_session_client, gate_session_server, peer_spki};
pub use identity::Identity;
pub use ids::{DeviceId, SPKI_LEN, Spki, device_id, device_id_hex, parse_spki};
pub use mode::{ModeOptions, PAIRING_MODE_MICROS, PairingMode};
pub use pairing::{
    InitiateOptions, LocalInfo, PairOutcome, PairUi, PairingContext, PeerInfo, Timeouts, commitment, compute_sas,
    initiate, respond,
};
pub use records::{Lockout, PairedDevice, TrustStore, UserRef};
pub use store::{PairingGuard, PinLookup, TrustHandle};
