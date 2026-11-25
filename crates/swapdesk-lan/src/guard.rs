//! The LAN guard (F-FIND-02, F-FIND-03, PROTOCOL.md section 1): which interfaces Swapdesk may use and which peers it
//! may talk to. Pure functions over a snapshot of `NetInterface`s, so the same rules run on the fakes and on the
//! real list.
//!
//! Rules, all IPv4 (v1). Any network (D93, owner 25/11/2026: Swapdesk works on whatever network the person is on):
//! - An interface is approved when it is up and is not loopback: Ethernet, Wi-Fi, a VPN, a virtual adapter, a
//!   tunnel, a phone's mobile data.
//! - An address of an approved interface is usable when it is a unicast address with a prefix of 1 to 32 bits (a /0
//!   would make everything "on-link"; a VPN or mobile data often has a /32, and its peers are all behind it). Link-local 169.254/16 is allowed: two
//!   machines joined by one cable have no other address.
//! - A peer is accepted on any network, on-link or through routers, when it is not loopback, unspecified, multicast,
//!   broadcast, one of this machine's own addresses, or the network or broadcast address of a subnet it is on-link
//!   in. The on-link test still serves Wake-on-LAN (a broadcast reaches only the link) and the choice of interface.

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

/// True when the interface itself may carry Swapdesk traffic: up and not loopback (D93).
pub fn is_approved(i: &NetInterface) -> bool {
    i.up && i.kind != IfKind::Loopback
}

fn usable_local(addr: Ipv4Addr, prefix: u8) -> bool {
    !(addr.is_loopback() || addr.is_unspecified() || addr.is_multicast() || addr.is_broadcast())
        && (1..=32).contains(&prefix)
}

/// Every usable address of every approved interface.
/// Ethernet and Wi-Fi first, then the rest, each in the list's order.
pub fn approved_addrs(list: &[NetInterface]) -> Vec<ApprovedAddr> {
    let mut sorted: Vec<&NetInterface> = list.iter().filter(|i| is_approved(i)).collect();
    sorted.sort_by_key(|i| !kind_is_physical(i.kind));
    sorted
        .into_iter()
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
    // A /31 holds two hosts and no broadcast (RFC 3021); a /32 holds only this machine.
    if a.prefix <= 30 && (remote == net || remote == a.broadcast()) {
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
/// the peer's source address. Any network (D93): the peer may be on-link or behind routers; `local_ip` must be an
/// address of an approved interface.
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
    match check_subnet(&addr, remote) {
        Ok(()) | Err(Refusal::OffLink) => Ok(link(&addr, remote)),
        Err(e) => Err(e),
    }
}

/// The check for a peer found by mDNS: `arrival_if_index` is the interface the answer arrived on, which must be
/// approved. Any network (D93): an announced address behind a router is taken too.
pub fn check_discovered(list: &[NetInterface], arrival_if_index: u32, remote: IpAddr) -> Result<PeerLink, Refusal> {
    let remote = v4(remote)?;
    check_class(list, remote)?;
    let iface = list.iter().find(|i| i.index == arrival_if_index).ok_or(Refusal::UnknownLocalAddress)?;
    if !is_approved(iface) {
        return Err(Refusal::InterfaceNotApproved(if iface.up { iface.kind } else { IfKind::Other }));
    }
    let on_iface: Vec<ApprovedAddr> =
        approved_addrs(list).into_iter().filter(|a| a.if_index == arrival_if_index).collect();
    for a in &on_iface {
        match check_subnet(a, remote) {
            Ok(()) => return Ok(link(a, remote)),
            Err(Refusal::OffLink) => {}
            Err(e) => return Err(e),
        }
    }
    on_iface.first().map(|a| link(a, remote)).ok_or(Refusal::InterfaceNotApproved(iface.kind))
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

/// How this machine reaches an address the person typed (D93): the approved interface address to send from, and
/// whether the peer is on-link there. An off-link peer is reached through the router, so its socket needs a TTL that
/// crosses it ([`crate::socket::ROUTED_TTL`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypedRoute {
    pub local: Ipv4Addr,
    pub remote: Ipv4Addr,
    pub on_link: bool,
}

/// The route to an address the person typed or a paired device's address (D93: the person may type any address,
/// on this network or another). Refused only what cannot be another computer: 0.0.0.0, loopback, multicast,
/// broadcast, this machine's own addresses, and the network or broadcast address of an approved subnet. On-link in
/// an approved subnet goes out of that interface; anything else out of the first approved address (the network the
/// router of this machine sits on), and an error when no approved interface has an address.
pub fn route_to_typed(list: &[NetInterface], remote: Ipv4Addr) -> Result<TypedRoute, Refusal> {
    check_class(list, remote)?;
    let approved = approved_addrs(list);
    for a in &approved {
        if in_subnet(remote, a.addr, a.prefix) {
            check_subnet(a, remote)?;
            return Ok(TypedRoute { local: a.addr, remote, on_link: true });
        }
    }
    let first = approved.first().ok_or(Refusal::UnknownLocalAddress)?;
    Ok(TypedRoute { local: first.addr, remote, on_link: false })
}

/// F-FIND-03: validates an address typed by the user. IPv4 literal only (no name, no port); surrounding spaces are
/// ignored. Any network (D93): see [`route_to_typed`].
pub fn validate_manual(list: &[NetInterface], text: &str) -> Result<TypedRoute, ManualError> {
    let ip: Ipv4Addr = text.trim().parse().map_err(|_| ManualError::Syntax)?;
    route_to_typed(list, ip).map_err(ManualError::Refused)
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
    fn every_network_but_loopback_is_approved_ethernet_and_wifi_first() {
        let list = typical();
        let addrs = approved_addrs(&list);
        let names: Vec<&str> = addrs.iter().map(|a| a.if_name.as_str()).collect();
        assert_eq!(&names[..2], ["Ethernet", "Wi-Fi"], "physical first: {names:?}");
        assert!(addrs.iter().any(|a| a.kind == IfKind::Vpn) && addrs.iter().any(|a| a.kind == IfKind::Virtual));
        for i in &list {
            assert_eq!(is_approved(i), i.up && i.kind != IfKind::Loopback, "{}", i.name);
        }
    }

    #[test]
    fn a_down_interface_is_not_approved() {
        let mut list = typical();
        list.iter_mut().find(|i| i.name == "Wi-Fi").unwrap().up = false;
        assert!(!approved_indexes(&list).contains(&7));
        assert!(approved_indexes(&list).contains(&4));
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
    fn accepts_connections_on_virtual_and_vpn_interfaces() {
        let list = typical();
        // vEthernet (WSL) 172.29.0.1/20, Default Switch 172.24.32.1/20, VPN 10.8.0.6/24: any network (D93).
        for (local, remote) in [
            (ip(172, 29, 0, 1), ip(172, 29, 0, 7)),
            (ip(172, 24, 32, 1), ip(172, 24, 32, 9)),
            (ip(10, 8, 0, 6), ip(10, 8, 0, 20)),
        ] {
            assert!(check_incoming(&list, local, remote).is_ok(), "{local}");
        }
        assert!(check_discovered(&list, 15, ip(172, 29, 0, 7)).is_ok());
        assert!(check_discovered(&list, 20, ip(10, 8, 0, 20)).is_ok());
        // The tunnel has no IPv4 address at all: nothing on it can answer.
        assert_eq!(
            check_discovered(&list, 22, ip(192, 168, 1, 50)),
            Err(Refusal::InterfaceNotApproved(IfKind::Tunnel))
        );
    }

    #[test]
    fn accepts_peers_behind_routers() {
        let list = typical();
        let routed = check_incoming(&list, ip(192, 168, 1, 21), ip(192, 168, 2, 5)).unwrap();
        assert_eq!((routed.local, routed.remote), (Ipv4Addr::new(192, 168, 1, 21), Ipv4Addr::new(192, 168, 2, 5)));
        assert!(check_incoming(&list, ip(192, 168, 1, 21), ip(8, 8, 8, 8)).is_ok());
        // Announced on Wi-Fi (192.168.1.0/24) with an address of another range.
        assert_eq!(check_discovered(&list, 7, ip(10, 0, 0, 5)).unwrap().local, Ipv4Addr::new(192, 168, 1, 21));
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
    fn a_zero_prefix_never_makes_everything_on_link_and_a_host_prefix_holds_only_this_machine() {
        let mut list = typical();
        let wifi = list.iter_mut().find(|i| i.name == "Wi-Fi").unwrap();
        wifi.ipv4 = vec![(Ipv4Addr::new(192, 168, 1, 21), 0)];
        assert!(approved_addrs(&list).iter().all(|a| a.if_name != "Wi-Fi"));
        assert!(check_incoming(&list, ip(192, 168, 1, 21), ip(8, 8, 8, 8)).is_err());
        let wifi = list.iter_mut().find(|i| i.name == "Wi-Fi").unwrap();
        wifi.ipv4 = vec![(Ipv4Addr::new(192, 168, 1, 21), 32)];
        // A /32 (a VPN, mobile data) is usable: every peer of it is behind a router.
        assert!(approved_addrs(&list).iter().any(|a| a.if_name == "Wi-Fi"));
        let routed = check_incoming(&list, ip(192, 168, 1, 21), ip(8, 8, 8, 8)).unwrap();
        assert_eq!(routed.prefix, 32);
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
        // Another network is the person's choice (D93): reached through the router, from the first approved address.
        let routed = |s: &str| validate_manual(&list, s).unwrap();
        assert!(!routed("192.168.2.50").on_link);
        assert_eq!(routed("8.8.8.8").local, approved_addrs(&list)[0].addr);
        assert!(routed("172.29.0.8").on_link, "the WSL network is a network too");
        assert!(routed("10.8.0.9").on_link, "and the VPN");
        assert!(routed(" 192.168.1.50 ").on_link);
        assert_eq!(refused("192.168.1.255"), ManualError::Refused(Refusal::Broadcast));
        assert_eq!(refused("224.0.0.251"), ManualError::Refused(Refusal::Multicast));
        assert_eq!(refused("127.0.0.1"), ManualError::Refused(Refusal::Loopback));
        assert_eq!(refused("192.168.1.21"), ManualError::Refused(Refusal::OwnAddress));
        assert_eq!(refused("192.168.1.20"), ManualError::Refused(Refusal::OwnAddress));
        assert_eq!(refused("0.0.0.0"), ManualError::Refused(Refusal::Unspecified));
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
        assert!(refused("192.168.1.21").to_string().contains("own address"));
    }
}
