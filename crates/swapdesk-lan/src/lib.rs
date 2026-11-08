//! The LAN side of Swapdesk (contract C3 `NetInterfaces`, `docs/PROTOCOL.md` section 1, flows F-FIND-01 to F-FIND-03 and
//! the packet of F-WAKE-01). No `unsafe`.
//!
//! - [`guard`]: approved interfaces (physical Ethernet and Wi-Fi only), on-link peer checks, manual-address validation.
//! - [`txt`]: the DNS-SD TXT record (name, OS, device fingerprint, pairing flag), with strict limits.
//! - [`discovery`]: announce and browse `_swapdesk._udp` with `mdns-sd`, bound to approved interfaces only.
//! - [`socket`]: QUIC UDP sockets with IP TTL 1. mDNS keeps the TTL 255 that `mdns-sd` sets: this crate never touches
//!   a multicast socket.
//! - [`wol`]: the magic packet and the directed-broadcast address of the approved interface.

pub mod discovery;
pub mod guard;
pub mod socket;
pub mod txt;
pub mod wol;
