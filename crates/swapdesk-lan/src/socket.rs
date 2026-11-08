//! Socket setup for the QUIC endpoints (F-FIND-02: "When a QUIC packet is sent, Swapdesk shall send it with IP TTL 1").
//!
//! Only QUIC sockets get TTL 1, set with std's `UdpSocket::set_ttl` and read back. mDNS sockets belong to `mdns-sd`,
//! which sends with TTL 255 (link-local multicast, RFC 6762); nothing in this crate touches them.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};

use swapdesk_platform::NetInterface;

use crate::guard::{ApprovedAddr, approved_addrs};

/// IP TTL of every QUIC datagram: one hop, never past the router.
pub const QUIC_TTL: u32 = 1;

#[derive(Debug)]
pub enum SocketError {
    /// The address is not a usable address of an approved interface (virtual, VPN, loopback, 0.0.0.0, unknown).
    NotApproved(Ipv4Addr),
    Io(io::Error),
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotApproved(a) => write!(f, "{a} is not an address of an approved Ethernet or Wi-Fi interface"),
            Self::Io(e) => write!(f, "socket error: {e}"),
        }
    }
}

impl std::error::Error for SocketError {}

impl From<io::Error> for SocketError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Binds a UDP socket and sets IP TTL 1, then reads the value back and fails if the system did not keep it. No guard
/// check: the caller picks the address (tests bind loopback). Production code uses [`bind_quic_approved`].
pub fn bind_quic_socket(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::V4(addr))?;
    socket.set_ttl(QUIC_TTL)?;
    let got = socket.ttl()?;
    if got != QUIC_TTL {
        return Err(io::Error::other(format!("IP TTL reads back {got}, expected {QUIC_TTL}")));
    }
    Ok(socket)
}

/// Binds a QUIC socket to one approved interface address. The address must be in the approved list of `list`
/// (so 0.0.0.0, loopback and virtual or VPN addresses fail) and the socket has TTL 1.
pub fn bind_quic_approved(list: &[NetInterface], local: SocketAddrV4) -> Result<UdpSocket, SocketError> {
    if !approved_addrs(list).iter().any(|a| a.addr == *local.ip()) {
        return Err(SocketError::NotApproved(*local.ip()));
    }
    Ok(bind_quic_socket(local)?)
}

/// One QUIC socket on `port` for every approved address (PROTOCOL.md section 1: one endpoint per approved interface
/// address). A failure on one address does not stop the others; each result says which address it is for.
pub fn bind_quic_all_approved(list: &[NetInterface], port: u16) -> Vec<(ApprovedAddr, io::Result<UdpSocket>)> {
    approved_addrs(list)
        .into_iter()
        .map(|a| {
            let socket = bind_quic_socket(SocketAddrV4::new(a.addr, port));
            (a, socket)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_fakes::FakeNetInterfaces;
    use swapdesk_platform::NetInterfaces;

    #[test]
    fn a_quic_socket_has_ttl_1_read_back_from_the_system() {
        let s = bind_quic_socket(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert_eq!(s.ttl().unwrap(), 1);
    }

    #[test]
    fn setting_ttl_1_on_a_quic_socket_leaves_other_sockets_alone() {
        let other = UdpSocket::bind("127.0.0.1:0").unwrap();
        let before = other.ttl().unwrap();
        assert_ne!(before, 1, "the system default is not 1");
        let quic = bind_quic_socket(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert_eq!(quic.ttl().unwrap(), 1);
        assert_eq!(other.ttl().unwrap(), before);
    }

    #[test]
    fn the_guarded_bind_refuses_unapproved_addresses_before_binding() {
        let list = FakeNetInterfaces::windows_typical().list().unwrap();
        for ip in [
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(172, 29, 0, 1),
            Ipv4Addr::new(10, 8, 0, 6),
            Ipv4Addr::new(192, 168, 1, 99),
        ] {
            let r = bind_quic_approved(&list, SocketAddrV4::new(ip, 0));
            assert!(matches!(r, Err(SocketError::NotApproved(a)) if a == ip), "{ip}");
        }
    }

    #[test]
    fn all_approved_addresses_get_one_entry_each() {
        let list = FakeNetInterfaces::windows_typical().list().unwrap();
        // The fake addresses are not on this PC, so each bind fails, but there is one attempt per approved address.
        let results = bind_quic_all_approved(&list, 0);
        let names: Vec<&str> = results.iter().map(|(a, _)| a.if_name.as_str()).collect();
        assert_eq!(names, ["Ethernet", "Wi-Fi"]);
    }
}
