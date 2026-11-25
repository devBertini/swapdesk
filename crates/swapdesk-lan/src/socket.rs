//! Socket setup for the QUIC endpoints. Since D93 every QUIC datagram is sent with TTL 64, so it crosses routers
//! (F-FIND-02 sent it with TTL 1 before).
//!
//! Only QUIC sockets get their TTL here, set with std's `UdpSocket::set_ttl` and read back. mDNS sockets belong to `mdns-sd`,
//! which sends with TTL 255 (link-local multicast, RFC 6762); nothing in this crate touches them.
//!
//! A platform may add one step to every QUIC socket, right after it is bound ([`set_after_bind`]). Android needs it:
//! there a socket is routed by the network it is bound to, not by its address, so a socket on the Wi-Fi address
//! leaves through mobile data unless it is bound to the Wi-Fi network (docs/ANDROID.md 4). On Android a missing
//! step fails the bind: no socket is better than one on the wrong network.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::OnceLock;

use swapdesk_platform::NetInterface;

use crate::guard::{ApprovedAddr, approved_addrs};

/// IP TTL of every QUIC datagram: the systems' usual default, so a datagram crosses the routers to a peer on another
/// network (D93; it was 1, one hop, before).
pub const QUIC_TTL: u32 = 64;

/// A QUIC socket for a dial to `route`, from its local address.
pub fn bind_quic_for(route: &crate::guard::TypedRoute) -> io::Result<UdpSocket> {
    bind_quic_socket(SocketAddrV4::new(route.local, 0))
}

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

/// The platform's step on a QUIC socket that was just bound and given its TTL: an error fails the bind.
pub type AfterBind = Box<dyn Fn(&UdpSocket) -> io::Result<()> + Send + Sync>;

static AFTER_BIND: OnceLock<AfterBind> = OnceLock::new();

/// Installs the platform's step for every QUIC socket bound from now on. Once, at start: a second call installs
/// nothing and returns `false`.
pub fn set_after_bind(step: AfterBind) -> bool {
    AFTER_BIND.set(step).is_ok()
}

/// Binds a UDP socket and sets the IP TTL (QUIC_TTL), then reads the value back and fails if the system did not keep it; then
/// the platform's step, if one is installed. No guard check: the caller picks the address (tests bind loopback).
/// Production code uses [`bind_quic_approved`].
pub fn bind_quic_socket(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    bind_with(addr, AFTER_BIND.get(), cfg!(target_os = "android"))
}

/// [`bind_quic_socket`] with the step named: `required` makes a missing step an error.
fn bind_with(addr: SocketAddrV4, step: Option<&AfterBind>, required: bool) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::V4(addr))?;
    socket.set_ttl(QUIC_TTL)?;
    let got = socket.ttl()?;
    if got != QUIC_TTL {
        return Err(io::Error::other(format!("IP TTL reads back {got}, expected {QUIC_TTL}")));
    }
    match step {
        Some(step) => step(&socket)?,
        None if required => {
            return Err(io::Error::other(
                "this platform binds every QUIC socket to a network, and no step is installed",
            ));
        }
        None => {}
    }
    Ok(socket)
}

/// Binds a QUIC socket to one approved interface address. The address must be in the approved list of `list`
/// (so 0.0.0.0, loopback and virtual or VPN addresses fail) and the socket has the QUIC TTL.
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
    fn a_quic_socket_has_its_ttl_read_back_from_the_system() {
        let s = bind_quic_socket(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert_eq!(s.ttl().unwrap(), QUIC_TTL);
    }

    /// docs/ANDROID.md 4: the platform's step runs on the socket once it has its TTL, an error of the step fails the
    /// bind, and where the step is required a missing one fails it too (no socket rather than one on any network).
    #[test]
    fn the_platforms_step_runs_after_the_ttl_and_fails_the_bind_closed() {
        let loopback = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = std::sync::Arc::clone(&seen);
        let step: AfterBind = Box::new(move |s| {
            record.lock().unwrap().push((s.ttl()?, s.local_addr()?.ip().is_loopback()));
            Ok(())
        });
        assert!(bind_with(loopback, Some(&step), true).is_ok());
        assert_eq!(*seen.lock().unwrap(), vec![(QUIC_TTL, true)], "once, on the bound socket, TTL already set");

        let refuse: AfterBind = Box::new(|_| Err(io::Error::other("no approved network")));
        assert_eq!(bind_with(loopback, Some(&refuse), false).unwrap_err().to_string(), "no approved network");
        assert!(bind_with(loopback, None, true).is_err(), "required and missing");
        assert!(bind_with(loopback, None, false).is_ok(), "the desktops need none");
    }

    #[test]
    fn setting_the_ttl_on_a_quic_socket_leaves_other_sockets_alone() {
        let other = UdpSocket::bind("127.0.0.1:0").unwrap();
        let before = other.ttl().unwrap();
        assert_ne!(before, 1, "the system default is not 1");
        let quic = bind_quic_socket(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert_eq!(quic.ttl().unwrap(), QUIC_TTL);
        assert_eq!(other.ttl().unwrap(), before);
    }

    #[test]
    fn the_guarded_bind_refuses_unapproved_addresses_before_binding() {
        let list = FakeNetInterfaces::windows_typical().list().unwrap();
        for ip in [Ipv4Addr::UNSPECIFIED, Ipv4Addr::LOCALHOST, Ipv4Addr::new(192, 168, 1, 99)] {
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
        assert_eq!(&names[..2], ["Ethernet", "Wi-Fi"]);
        assert_eq!(names.len(), approved_addrs(&list).len());
    }
}
