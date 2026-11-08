//! Classification of a macOS network interface into the guard's `IfKind`. Pure: the facts come from `getifaddrs` and
//! SystemConfiguration, so the rules are tested on any host.
//!
//! An interface is Ethernet or Wi-Fi only when its BSD name is a plain `en<N>` and SystemConfiguration says its
//! interface type is Ethernet or IEEE 802.11, and it is a broadcast interface that is not point-to-point. Everything the
//! OS creates in software is refused by name first (`utun*` and other VPN or tunnel devices, `bridge*`, `vmnet*`,
//! `awdl*` and `llw*` (Apple Wireless Direct Link and low-latency WLAN), `anpi*`, `ap*` (the Wi-Fi access point of
//! Internet Sharing), virtual NICs). An `en<N>` that SystemConfiguration does not list, or lists with another type
//! (Thunderbolt Bridge members, Bluetooth PAN, FireWire), is refused as software: fail closed.

use swapdesk_platform::IfKind;

/// `IFF_UP`, `IFF_BROADCAST`, `IFF_LOOPBACK`, `IFF_POINTOPOINT`, `IFF_RUNNING` from `<net/if.h>` (the same values on
/// every BSD, written out so this module builds on any host).
pub const IFF_UP: u32 = 0x1;
pub const IFF_BROADCAST: u32 = 0x2;
pub const IFF_LOOPBACK: u32 = 0x8;
pub const IFF_POINTOPOINT: u32 = 0x10;
pub const IFF_RUNNING: u32 = 0x40;

/// The SystemConfiguration interface type, reduced to what the guard cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScKind {
    /// `kSCNetworkInterfaceTypeEthernet`.
    Ethernet,
    /// `kSCNetworkInterfaceTypeIEEE80211`.
    Wifi,
    /// Any other type SystemConfiguration lists (Bluetooth, FireWire, Bond, VLAN, ...).
    Other,
}

/// What macOS reports about one interface.
#[derive(Debug, Clone, Copy)]
pub struct InterfaceFacts<'a> {
    /// The BSD name, e.g. `en0`.
    pub name: &'a str,
    /// SystemConfiguration's type for this BSD name; `None` when it does not list the interface.
    pub sc: Option<ScKind>,
    /// `ifa_flags` of `getifaddrs`.
    pub flags: u32,
}

/// The BSD name without its unit number: `utun12` is `utun`, `en0` is `en`.
pub fn base_name(name: &str) -> &str {
    name.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// Interface families that carry a VPN.
const VPN_BASES: &[&str] = &["utun", "ipsec", "ppp", "tun", "tap"];
/// Interface families that are plain tunnels.
const TUNNEL_BASES: &[&str] = &["gif", "stf"];
/// Interface families that are software, wireless peer-to-peer or hypervisor devices.
const VIRTUAL_BASES: &[&str] =
    &["awdl", "llw", "anpi", "ap", "bridge", "vmnet", "vmenet", "vnic", "feth", "vboxnet", "veth", "docker", "gpd"];

pub fn classify(f: &InterfaceFacts<'_>) -> IfKind {
    let base = base_name(f.name);
    if f.flags & IFF_LOOPBACK != 0 || base == "lo" {
        return IfKind::Loopback;
    }
    if VPN_BASES.contains(&base) {
        return IfKind::Vpn;
    }
    if TUNNEL_BASES.contains(&base) {
        return IfKind::Tunnel;
    }
    if VIRTUAL_BASES.contains(&base) {
        return IfKind::Virtual;
    }
    // A point-to-point link is a tunnel whatever its name, and a physical LAN interface broadcasts.
    if f.flags & IFF_POINTOPOINT != 0 {
        return IfKind::Tunnel;
    }
    if base != "en" {
        return IfKind::Other;
    }
    if f.flags & IFF_BROADCAST == 0 {
        return IfKind::Virtual;
    }
    match f.sc {
        Some(ScKind::Ethernet) => IfKind::Ethernet,
        Some(ScKind::Wifi) => IfKind::WiFi,
        // An `en` interface SystemConfiguration does not call Ethernet or Wi-Fi is software.
        Some(ScKind::Other) | None => IfKind::Virtual,
    }
}

/// Link state: up and running.
pub fn is_up(flags: u32) -> bool {
    flags & (IFF_UP | IFF_RUNNING) == (IFF_UP | IFF_RUNNING)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: u32 = IFF_UP | IFF_BROADCAST | IFF_RUNNING;

    fn kind(name: &str, sc: Option<ScKind>, flags: u32) -> IfKind {
        classify(&InterfaceFacts { name, sc, flags })
    }

    #[test]
    fn physical_interfaces_are_ethernet_and_wifi() {
        assert_eq!(kind("en0", Some(ScKind::Wifi), LAN), IfKind::WiFi);
        assert_eq!(kind("en7", Some(ScKind::Ethernet), LAN), IfKind::Ethernet);
        assert_eq!(kind("en12", Some(ScKind::Ethernet), LAN), IfKind::Ethernet);
    }

    #[test]
    fn software_interfaces_are_refused_by_name_even_with_an_ethernet_type() {
        let ether = Some(ScKind::Ethernet);
        assert_eq!(kind("utun4", ether, LAN | IFF_POINTOPOINT), IfKind::Vpn);
        assert_eq!(kind("ipsec0", ether, LAN), IfKind::Vpn);
        assert_eq!(kind("ppp0", ether, LAN), IfKind::Vpn);
        assert_eq!(kind("bridge100", ether, LAN), IfKind::Virtual);
        assert_eq!(kind("vmnet8", ether, LAN), IfKind::Virtual);
        assert_eq!(kind("vmenet0", ether, LAN), IfKind::Virtual);
        assert_eq!(kind("awdl0", Some(ScKind::Wifi), LAN), IfKind::Virtual);
        assert_eq!(kind("llw0", Some(ScKind::Wifi), LAN), IfKind::Virtual);
        assert_eq!(kind("anpi0", ether, LAN), IfKind::Virtual);
        assert_eq!(kind("ap1", Some(ScKind::Wifi), LAN), IfKind::Virtual);
        assert_eq!(kind("gif0", None, IFF_POINTOPOINT), IfKind::Tunnel);
        assert_eq!(kind("stf0", None, 0), IfKind::Tunnel);
    }

    #[test]
    fn loopback_and_unknown_families() {
        assert_eq!(kind("lo0", None, IFF_UP | IFF_LOOPBACK | IFF_RUNNING), IfKind::Loopback);
        assert_eq!(kind("pktap0", None, IFF_UP), IfKind::Other);
        assert_eq!(kind("XHC20", None, 0), IfKind::Other);
    }

    #[test]
    fn an_en_interface_that_is_not_listed_or_not_ethernet_or_wifi_fails_closed() {
        assert_eq!(kind("en1", None, LAN), IfKind::Virtual);
        assert_eq!(kind("en2", Some(ScKind::Other), LAN), IfKind::Virtual);
        // No broadcast capability: not a LAN interface.
        assert_eq!(kind("en3", Some(ScKind::Ethernet), IFF_UP | IFF_RUNNING), IfKind::Virtual);
    }

    #[test]
    fn a_point_to_point_link_is_never_physical() {
        assert_eq!(kind("en5", Some(ScKind::Ethernet), LAN | IFF_POINTOPOINT), IfKind::Tunnel);
    }

    #[test]
    fn only_the_two_physical_kinds_are_approved() {
        // The unit-number rule: `ap` is not a prefix match, so a hypothetical `apple0` is Other, not Virtual by luck.
        assert_eq!(base_name("utun12"), "utun");
        assert_eq!(base_name("en0"), "en");
        assert_eq!(kind("apple0", Some(ScKind::Ethernet), LAN), IfKind::Other);
        assert_eq!(kind("enx0", Some(ScKind::Ethernet), LAN), IfKind::Other);
    }

    #[test]
    fn link_state_needs_up_and_running() {
        assert!(is_up(LAN));
        assert!(!is_up(IFF_UP | IFF_BROADCAST));
        assert!(!is_up(IFF_BROADCAST | IFF_RUNNING));
    }
}
