//! The LAN guard (F-FIND-02, F-FIND-03, PROTOCOL.md section 1): which interfaces Swapdesk may use and which peers it
//! may talk to. Pure functions over a snapshot of `NetInterface`s, so the same rules run on the fakes and on the
//! real list.
//!
//! Rules, all IPv4 (v1):
//! - An interface is approved when it is up and its kind is physical Ethernet or Wi-Fi. Virtual (Hyper-V, WSL,
//!   Docker), VPN, tunnel, loopback and unknown kinds are never approved.
//! - An address of an approved interface is usable when it is a unicast address with a prefix of 1 to 30 bits (a /0
//!   would make everything "on-link"; /31 and /32 hold no ordinary peer). Link-local 169.254/16 is allowed: two
//!   machines joined by one cable have no other address.
//! - A peer is accepted only when it is on-link in the subnet of the approved interface it arrived on, is not that
//!   subnet's network or broadcast address, and is not loopback, unspecified, multicast, broadcast or one of this
//!   machine's own addresses (on any interface, approved or not).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr};

use swapdesk_platform::{IfKind, NetInterface};

/// An address of an approved interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedAddr {
    pub if_index: u32,
    pub if_name: String,
    pub kind: IfKind,
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl ApprovedAddr {
    pub fn netmask(&self) -> Ipv4Addr {
        mask(self.prefix)
    }
    /// The directed-broadcast address of the subnet (used by Wake-on-LAN).
    pub fn broadcast(&self) -> Ipv4Addr {
        crate::wol::directed_broadcast(self.addr, self.prefix)
    }
}

/// A peer that passed the guard, with the approved interface address that reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerLink {
    pub if_index: u32,
    pub if_name: String,
    pub local: Ipv4Addr,
    pub prefix: u8,
    pub remote: Ipv4Addr,
}

/// Why the guard refused. The UI maps each variant to its own text (pt-BR and en); `Display` is the English log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not an IPv4 address (IPv4 only in v1).
    NotIpv4,
    Unspecified,
    Loopback,
    Multicast,
    /// 255.255.255.255, or the network or directed-broadcast address of the subnet.
    Broadcast,
    /// One of this machine's own addresses.
    OwnAddress,
    /// The local address is not on any interface of this machine.
    UnknownLocalAddress,
    /// The interface is virtual, a VPN, a tunnel, loopback or of unknown kind (or down).
    InterfaceNotApproved(IfKind),
    /// Not in the subnet of the approved interface.
    OffLink,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotIpv4 => f.write_str("not an IPv4 address"),
            Self::Unspecified => f.write_str("the unspecified address 0.0.0.0"),
            Self::Loopback => f.write_str("a loopback address"),
            Self::Multicast => f.write_str("a multicast address"),
            Self::Broadcast => f.write_str("a broadcast or subnet network address"),
            Self::OwnAddress => f.write_str("this machine's own address"),
            Self::UnknownLocalAddress => f.write_str("the local address is not on any interface of this machine"),
            Self::InterfaceNotApproved(kind) => {
                write!(f, "the interface is not a physical Ethernet or Wi-Fi one ({kind:?})")
            }
            Self::OffLink => f.write_str("not on the local network (off-link)"),
        }
    }
}

impl std::error::Error for Refusal {}

/// Why a typed address was refused (F-FIND-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualError {
    /// Not a dotted IPv4 address (a name, a port, an IPv6 address, an empty string).
    Syntax,
    Refused(Refusal),
}

impl fmt::Display for ManualError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax => f.write_str("not a valid IPv4 address (use four numbers like 192.168.1.20)"),
            Self::Refused(r) => write!(f, "refused: {r}"),
        }
    }
}

impl std::error::Error for ManualError {}

fn mask(prefix: u8) -> Ipv4Addr {
    let p = u32::from(prefix.min(32));
    Ipv4Addr::from(if p == 0 { 0 } else { u32::MAX << (32 - p) })
}

/// True when `ip` is inside `net/prefix`.
pub fn in_subnet(ip: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    let m = u32::from(mask(prefix));
    u32::from(ip) & m == u32::from(net) & m
}

fn kind_is_physical(kind: IfKind) -> bool {
    matches!(kind, IfKind::Ethernet | IfKind::WiFi)
}

/// True when the interface itself may carry Swapdesk traffic (physical Ethernet or Wi-Fi, up).
pub fn is_approved(i: &NetInterface) -> bool {
    i.up && kind_is_physical(i.kind)
}

fn usable_local(addr: Ipv4Addr, prefix: u8) -> bool {
    !(addr.is_loopback() || addr.is_unspecified() || addr.is_multicast() || addr.is_broadcast())
        && (1..=30).contains(&prefix)
}

/// Every usable address of every approved interface.
pub fn approved_addrs(list: &[NetInterface]) -> Vec<ApprovedAddr> {
    list.iter()
        .filter(|i| is_approved(i))
        .flat_map(|i| {
            i.ipv4.iter().filter(|(a, p)| usable_local(*a, *p)).map(|(a, p)| ApprovedAddr {
                if_index: i.index,
                if_name: i.name.clone(),
                kind: i.kind,
                addr: *a,
                prefix: *p,
            })
        })
        .collect()
}

/// The indexes of the approved interfaces that have at least one usable address (what mDNS binds to).
pub fn approved_indexes(list: &[NetInterface]) -> Vec<u32> {
    let mut v: Vec<u32> = approved_addrs(list).iter().map(|a| a.if_index).collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn is_own(list: &[NetInterface], ip: Ipv4Addr) -> bool {
    list.iter().any(|i| i.ipv4.iter().any(|(a, _)| *a == ip))
}

/// Address-class checks that need no interface.
fn check_class(list: &[NetInterface], remote: Ipv4Addr) -> Result<(), Refusal> {
    if remote.is_unspecified() {
        return Err(Refusal::Unspecified);
    }
    if remote.is_loopback() {
        return Err(Refusal::Loopback);
    }
    if remote.is_multicast() {
        return Err(Refusal::Multicast);
    }
    if remote.is_broadcast() {
        return Err(Refusal::Broadcast);
    }
    if is_own(list, remote) {
        return Err(Refusal::OwnAddress);
    }
    Ok(())
}

fn check_subnet(a: &ApprovedAddr, remote: Ipv4Addr) -> Result<(), Refusal> {
    if !in_subnet(remote, a.addr, a.prefix) {
        return Err(Refusal::OffLink);
    }
    let net = Ipv4Addr::from(u32::from(a.addr) & u32::from(a.netmask()));
    if remote == net || remote == a.broadcast() {
        return Err(Refusal::Broadcast);
    }
    Ok(())
}

fn link(a: &ApprovedAddr, remote: Ipv4Addr) -> PeerLink {
    PeerLink { if_index: a.if_index, if_name: a.if_name.clone(), local: a.addr, prefix: a.prefix, remote }
}

fn v4(ip: IpAddr) -> Result<Ipv4Addr, Refusal> {
    match ip {
        IpAddr::V4(a) => Ok(a),
        IpAddr::V6(_) => Err(Refusal::NotIpv4),
    }
}

/// The incoming-connection check (before the TLS handshake): `local_ip` is quinn's `Incoming::local_ip()`, `remote`
/// the peer's source address. The peer must be on-link in the subnet of the approved interface that owns `local_ip`.
pub fn check_incoming(list: &[NetInterface], local_ip: IpAddr, remote: IpAddr) -> Result<PeerLink, Refusal> {
    let remote = v4(remote)?;
    let local = v4(local_ip)?;
    check_class(list, remote)?;
    let owner = list.iter().find(|i| i.ipv4.iter().any(|(a, _)| *a == local)).ok_or(Refusal::UnknownLocalAddress)?;
    if !is_approved(owner) {
        return Err(Refusal::InterfaceNotApproved(if owner.up { owner.kind } else { IfKind::Other }));
    }
    let addr = approved_addrs(list)
        .into_iter()
        .find(|a| a.if_index == owner.index && a.addr == local)
        .ok_or(Refusal::InterfaceNotApproved(owner.kind))?;
    check_subnet(&addr, remote)?;
    Ok(link(&addr, remote))
}

/// The check for a peer found by mDNS: `arrival_if_index` is the interface the answer arrived on. The peer must be
/// on-link in a subnet of that approved interface.
pub fn check_discovered(list: &[NetInterface], arrival_if_index: u32, remote: IpAddr) -> Result<PeerLink, Refusal> {
    let remote = v4(remote)?;
    check_class(list, remote)?;
    let iface = list.iter().find(|i| i.index == arrival_if_index).ok_or(Refusal::UnknownLocalAddress)?;
    if !is_approved(iface) {
        return Err(Refusal::InterfaceNotApproved(if iface.up { iface.kind } else { IfKind::Other }));
    }
    let mut result = Err(Refusal::OffLink);
    for a in approved_addrs(list).iter().filter(|a| a.if_index == arrival_if_index) {
        match check_subnet(a, remote) {
            Ok(()) => return Ok(link(a, remote)),
            Err(Refusal::OffLink) => {}
            Err(e) => result = Err(e),
        }
    }
    result
}

/// The approved interface whose subnet holds `remote`, whatever interface the packet arrived on (used for a typed
/// address and for choosing the Wake-on-LAN broadcast).
pub fn find_on_link(list: &[NetInterface], remote: Ipv4Addr) -> Result<PeerLink, Refusal> {
    check_class(list, remote)?;
    let mut result = Err(Refusal::OffLink);
    for a in approved_addrs(list) {
        match check_subnet(&a, remote) {
            Ok(()) => return Ok(link(&a, remote)),
            Err(Refusal::OffLink) => {}
            Err(e) => result = Err(e),
        }
    }
    if result == Err(Refusal::OffLink) {
        // Say why when the address belongs to a network Swapdesk deliberately does not use.
        for i in list.iter().filter(|i| !is_approved(i)) {
            if i.ipv4.iter().any(|(a, p)| (1..=30).contains(p) && !a.is_loopback() && in_subnet(remote, *a, *p)) {
                return Err(Refusal::InterfaceNotApproved(if i.up { i.kind } else { IfKind::Other }));
            }
        }
    }
    result
}

/// F-FIND-03: validates an address typed by the user. IPv4 literal only (no name, no port); surrounding spaces are
/// ignored.
pub fn validate_manual(list: &[NetInterface], text: &str) -> Result<PeerLink, ManualError> {
    let ip: Ipv4Addr = text.trim().parse().map_err(|_| ManualError::Syntax)?;
    find_on_link(list, ip).map_err(ManualError::Refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_fakes::FakeNetInterfaces;
    use swapdesk_platform::NetInterfaces;

    fn typical() -> Vec<NetInterface> {
        FakeNetInterfaces::windows_typical().list().unwrap()
    }

    fn ip(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn only_physical_ethernet_and_wifi_are_approved() {
        let list = typical();
        let addrs = approved_addrs(&list);
        let names: Vec<&str> = addrs.iter().map(|a| a.if_name.as_str()).collect();
        assert_eq!(names, ["Ethernet", "Wi-Fi"]);
        assert_eq!(approved_indexes(&list), [4, 7]);
        for i in &list {
            let expect = matches!(i.name.as_str(), "Ethernet" | "Wi-Fi");
            assert_eq!(is_approved(i), expect, "{}", i.name);
        }
    }

    #[test]
    fn a_down_interface_is_not_approved() {
        let mut list = typical();
        list.iter_mut().find(|i| i.name == "Wi-Fi").unwrap().up = false;
        assert_eq!(approved_indexes(&list), [4]);
    }

    #[test]
    fn accepts_on_link_peers_on_wifi_and_ethernet() {
        let list = typical();
        let wifi = check_incoming(&list, ip(192, 168, 1, 21), ip(192, 168, 1, 50)).unwrap();
        assert_eq!((wifi.if_index, wifi.if_name.as_str(), wifi.prefix), (7, "Wi-Fi", 24));
        let eth = check_incoming(&list, ip(192, 168, 1, 20), ip(192, 168, 1, 99)).unwrap();
        assert_eq!(eth.if_name, "Ethernet");
        let found = check_discovered(&list, 7, ip(192, 168, 1, 77)).unwrap();
        assert_eq!(found.local, Ipv4Addr::new(192, 168, 1, 21));
    }

    #[test]
    fn refuses_connections_that_arrived_on_virtual_vpn_and_tunnel_interfaces() {
        let list = typical();
        // vEthernet (WSL) 172.29.0.1/20, Default Switch 172.24.32.1/20, VPN 10.8.0.6/24.
        let cases = [
            (ip(172, 29, 0, 1), ip(172, 29, 0, 7), IfKind::Virtual),
            (ip(172, 24, 32, 1), ip(172, 24, 32, 9), IfKind::Virtual),
            (ip(10, 8, 0, 6), ip(10, 8, 0, 20), IfKind::Vpn),
        ];
        for (local, remote, kind) in cases {
            assert_eq!(check_incoming(&list, local, remote), Err(Refusal::InterfaceNotApproved(kind)), "{local}");
        }
        assert_eq!(check_discovered(&list, 15, ip(172, 29, 0, 7)), Err(Refusal::InterfaceNotApproved(IfKind::Virtual)));
        assert_eq!(check_discovered(&list, 20, ip(10, 8, 0, 20)), Err(Refusal::InterfaceNotApproved(IfKind::Vpn)));
        // The tunnel has no IPv4 address at all, and index 22 is no approved interface.
        assert_eq!(
            check_discovered(&list, 22, ip(192, 168, 1, 50)),
            Err(Refusal::InterfaceNotApproved(IfKind::Tunnel))
        );
    }

    #[test]
    fn refuses_off_link_peers() {
        let list = typical();
        assert_eq!(check_incoming(&list, ip(192, 168, 1, 21), ip(192, 168, 2, 5)), Err(Refusal::OffLink));
        assert_eq!(check_incoming(&list, ip(192, 168, 1, 21), ip(8, 8, 8, 8)), Err(Refusal::OffLink));
        // Arrived on Wi-Fi (192.168.1.0/24) but the peer is in another private range.
        assert_eq!(check_discovered(&list, 7, ip(10, 0, 0, 5)), Err(Refusal::OffLink));
    }

    #[test]
    fn refuses_own_loopback_and_special_addresses() {
        let list = typical();
        let local = ip(192, 168, 1, 21);
        // Own addresses on any interface, approved or not.
        assert_eq!(check_incoming(&list, local, ip(192, 168, 1, 21)), Err(Refusal::OwnAddress));
        assert_eq!(check_incoming(&list, local, ip(192, 168, 1, 20)), Err(Refusal::OwnAddress));
        assert_eq!(check_incoming(&list, local, ip(172, 29, 0, 1)), Err(Refusal::OwnAddress));
        assert_eq!(check_incoming(&list, local, ip(127, 0, 0, 1)), Err(Refusal::Loopback));
        assert_eq!(check_incoming(&list, local, ip(127, 5, 5, 5)), Err(Refusal::Loopback));
        assert_eq!(check_incoming(&list, local, ip(0, 0, 0, 0)), Err(Refusal::Unspecified));
        assert_eq!(check_incoming(&list, local, ip(224, 0, 0, 251)), Err(Refusal::Multicast));
        assert_eq!(check_incoming(&list, local, ip(255, 255, 255, 255)), Err(Refusal::Broadcast));
        assert_eq!(check_incoming(&list, local, ip(192, 168, 1, 255)), Err(Refusal::Broadcast));
        assert_eq!(check_incoming(&list, local, ip(192, 168, 1, 0)), Err(Refusal::Broadcast));
        // The local address must exist on this machine, and a loopback listener is not approved.
        assert_eq!(check_incoming(&list, ip(192, 168, 1, 99), ip(192, 168, 1, 50)), Err(Refusal::UnknownLocalAddress));
        assert_eq!(
            check_incoming(&list, ip(127, 0, 0, 1), ip(192, 168, 1, 50)),
            Err(Refusal::InterfaceNotApproved(IfKind::Loopback))
        );
    }

    #[test]
    fn refuses_ipv6() {
        let list = typical();
        let v6: IpAddr = "fe80::1".parse().unwrap();
        assert_eq!(check_incoming(&list, ip(192, 168, 1, 21), v6), Err(Refusal::NotIpv4));
        assert_eq!(check_incoming(&list, v6, ip(192, 168, 1, 50)), Err(Refusal::NotIpv4));
        assert_eq!(check_discovered(&list, 7, v6), Err(Refusal::NotIpv4));
    }

    #[test]
    fn a_zero_or_host_prefix_never_makes_everything_on_link() {
        let mut list = typical();
        let wifi = list.iter_mut().find(|i| i.name == "Wi-Fi").unwrap();
        wifi.ipv4 = vec![(Ipv4Addr::new(192, 168, 1, 21), 0)];
        assert!(approved_addrs(&list).iter().all(|a| a.if_name != "Wi-Fi"));
        assert!(check_incoming(&list, ip(192, 168, 1, 21), ip(8, 8, 8, 8)).is_err());
        let wifi = list.iter_mut().find(|i| i.name == "Wi-Fi").unwrap();
        wifi.ipv4 = vec![(Ipv4Addr::new(192, 168, 1, 21), 32)];
        assert!(approved_addrs(&list).iter().all(|a| a.if_name != "Wi-Fi"));
    }

    #[test]
    fn link_local_cable_pair_is_allowed() {
        let mut list = typical();
        let eth = list.iter_mut().find(|i| i.name == "Ethernet").unwrap();
        eth.ipv4 = vec![(Ipv4Addr::new(169, 254, 10, 1), 16)];
        assert!(check_incoming(&list, ip(169, 254, 10, 1), ip(169, 254, 77, 3)).is_ok());
    }

    #[test]
    fn netmask_and_broadcast_follow_the_prefix() {
        let a = |addr: [u8; 4], prefix| ApprovedAddr {
            if_index: 1,
            if_name: "x".into(),
            kind: IfKind::Ethernet,
            addr: Ipv4Addr::from(addr),
            prefix,
        };
        assert_eq!(a([192, 168, 1, 21], 24).broadcast(), Ipv4Addr::new(192, 168, 1, 255));
        assert_eq!(a([192, 168, 1, 21], 24).netmask(), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(a([10, 1, 2, 3], 8).broadcast(), Ipv4Addr::new(10, 255, 255, 255));
        assert_eq!(a([172, 16, 5, 9], 20).broadcast(), Ipv4Addr::new(172, 16, 15, 255));
        assert_eq!(a([192, 168, 1, 130], 25).broadcast(), Ipv4Addr::new(192, 168, 1, 255));
        assert_eq!(a([192, 168, 1, 100], 26).broadcast(), Ipv4Addr::new(192, 168, 1, 127));
    }

    #[test]
    fn manual_address_validation() {
        let list = typical();
        assert_eq!(validate_manual(&list, " 192.168.1.50 ").unwrap().remote, Ipv4Addr::new(192, 168, 1, 50));
        let refused = |s: &str| validate_manual(&list, s).unwrap_err();
        assert_eq!(refused("192.168.2.50"), ManualError::Refused(Refusal::OffLink));
        assert_eq!(refused("8.8.8.8"), ManualError::Refused(Refusal::OffLink));
        assert_eq!(refused("127.0.0.1"), ManualError::Refused(Refusal::Loopback));
        assert_eq!(refused("192.168.1.21"), ManualError::Refused(Refusal::OwnAddress));
        assert_eq!(refused("192.168.1.20"), ManualError::Refused(Refusal::OwnAddress));
        assert_eq!(refused("0.0.0.0"), ManualError::Refused(Refusal::Unspecified));
        // An address inside the WSL or VPN subnet is refused with the reason.
        assert_eq!(refused("172.29.0.8"), ManualError::Refused(Refusal::InterfaceNotApproved(IfKind::Virtual)));
        assert_eq!(refused("10.8.0.9"), ManualError::Refused(Refusal::InterfaceNotApproved(IfKind::Vpn)));
        for bad in [
            "",
            "   ",
            "localhost",
            "192.168.1",
            "192.168.1.50:47820",
            "192.168.1.256",
            "::1",
            "192.168.001.050",
            "1.2.3.4.5",
        ] {
            assert_eq!(refused(bad), ManualError::Syntax, "{bad:?}");
        }
        assert!(refused("192.168.2.50").to_string().contains("off-link"));
    }
}
