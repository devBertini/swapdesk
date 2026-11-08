//! Classification of a Windows network adapter into the guard's `IfKind`. Pure: the facts come from
//! `GetAdaptersAddresses` and `GetIfEntry2`, so the rules are tested on any host.
//!
//! The IANA `IfType` alone cannot tell a physical NIC from a virtual one: Hyper-V and WSL `vEthernet` adapters, Wi-Fi
//! Direct and most VPN drivers report `IF_TYPE_ETHERNET_CSMACD` or `IF_TYPE_IEEE80211`. So an adapter is Ethernet or
//! Wi-Fi only when Windows also says it is a hardware interface (`MIB_IF_ROW2` flag `HardwareInterface`) and no
//! virtual or VPN marker is in its name or driver description. Anything doubtful falls on the refused side.

use swapdesk_platform::IfKind;

pub const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
pub const IF_TYPE_PPP: u32 = 23;
pub const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
pub const IF_TYPE_PROP_VIRTUAL: u32 = 53;
pub const IF_TYPE_IEEE80211: u32 = 71;
pub const IF_TYPE_TUNNEL: u32 = 131;

/// What Windows reports about one adapter.
#[derive(Debug, Clone, Copy)]
pub struct AdapterFacts<'a> {
    pub if_type: u32,
    pub friendly_name: &'a str,
    pub description: &'a str,
    /// `MIB_IF_ROW2.InterfaceAndOperStatusFlags.HardwareInterface`; false when it could not be read.
    pub hardware: bool,
}

/// Names of VPN clients and drivers (matched lower-case in the friendly name and the driver description).
const VPN_MARKERS: &[&str] = &[
    "vpn",
    "tap-windows",
    "wireguard",
    "wintun",
    "openvpn",
    "pritunl",
    "tailscale",
    "zerotier",
    "nordlynx",
    "anyconnect",
    "fortinet",
    "globalprotect",
    "pangp",
    "hamachi",
    "softether",
    "proton",
    "mullvad",
    "sonicwall",
    "check point",
    "juniper",
];

/// Names of hypervisor, container and software adapters.
const VIRTUAL_MARKERS: &[&str] = &[
    "hyper-v",
    "vethernet",
    "wsl",
    "vmware",
    "virtualbox",
    "vmnet",
    "virtual",
    "npcap",
    "docker",
    "parallels",
    "loopback adapter",
    "wi-fi direct",
    "qemu",
    "kvm",
];

fn has_marker(f: &AdapterFacts<'_>, markers: &[&str]) -> bool {
    let name = f.friendly_name.to_lowercase();
    let desc = f.description.to_lowercase();
    markers.iter().any(|m| name.contains(m) || desc.contains(m))
}

pub fn classify(f: &AdapterFacts<'_>) -> IfKind {
    match f.if_type {
        IF_TYPE_SOFTWARE_LOOPBACK => return IfKind::Loopback,
        IF_TYPE_TUNNEL => return IfKind::Tunnel,
        IF_TYPE_PPP => return IfKind::Vpn,
        _ => {}
    }
    if has_marker(f, VPN_MARKERS) {
        return IfKind::Vpn;
    }
    if f.if_type == IF_TYPE_PROP_VIRTUAL || has_marker(f, VIRTUAL_MARKERS) {
        return IfKind::Virtual;
    }
    match (f.if_type, f.hardware) {
        (IF_TYPE_ETHERNET_CSMACD, true) => IfKind::Ethernet,
        (IF_TYPE_IEEE80211, true) => IfKind::WiFi,
        // An Ethernet or Wi-Fi typed adapter that Windows does not call hardware is software.
        (IF_TYPE_ETHERNET_CSMACD | IF_TYPE_IEEE80211, false) => IfKind::Virtual,
        _ => IfKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(if_type: u32, name: &str, desc: &str, hardware: bool) -> IfKind {
        classify(&AdapterFacts { if_type, friendly_name: name, description: desc, hardware })
    }

    #[test]
    fn physical_adapters_are_ethernet_and_wifi() {
        assert_eq!(kind(6, "Ethernet", "Intel(R) Ethernet Controller I225-V", true), IfKind::Ethernet);
        assert_eq!(kind(71, "Wi-Fi", "Intel(R) Wi-Fi 7 BE200 320MHz", true), IfKind::WiFi);
        assert_eq!(kind(6, "Ethernet 2", "Realtek USB GbE Family Controller", true), IfKind::Ethernet);
    }

    #[test]
    fn hyper_v_and_wsl_adapters_are_virtual_even_with_the_ethernet_type() {
        assert_eq!(kind(6, "vEthernet (Default Switch)", "Hyper-V Virtual Ethernet Adapter", false), IfKind::Virtual);
        assert_eq!(
            kind(6, "vEthernet (WSL (Hyper-V firewall))", "Hyper-V Virtual Ethernet Adapter #2", false),
            IfKind::Virtual
        );
        // Even if the hardware flag were true, the name decides.
        assert_eq!(kind(6, "vEthernet (WSL)", "Hyper-V Virtual Ethernet Adapter", true), IfKind::Virtual);
        assert_eq!(kind(6, "Ethernet 3", "VMware Virtual Ethernet Adapter for VMnet8", true), IfKind::Virtual);
        assert_eq!(
            kind(6, "VirtualBox Host-Only Network", "VirtualBox Host-Only Ethernet Adapter", false),
            IfKind::Virtual
        );
        assert_eq!(kind(6, "Ethernet 4", "Npcap Loopback Adapter", false), IfKind::Virtual);
    }

    #[test]
    fn wifi_direct_is_virtual() {
        assert_eq!(kind(71, "Conexao Local* 9", "Microsoft Wi-Fi Direct Virtual Adapter", false), IfKind::Virtual);
        assert_eq!(kind(71, "Wi-Fi 2", "Some Wi-Fi", false), IfKind::Virtual);
    }

    #[test]
    fn vpn_adapters_are_vpn() {
        assert_eq!(kind(6, "Pritunl 1", "Pritunl Client TAP", false), IfKind::Vpn);
        assert_eq!(kind(6, "Pritunl DCO 1", "Pritunl DCO", false), IfKind::Vpn);
        assert_eq!(kind(53, "WireGuard Tunnel", "WireGuard Tunnel", false), IfKind::Vpn);
        assert_eq!(kind(6, "Ethernet 5", "TAP-Windows Adapter V9", true), IfKind::Vpn);
        assert_eq!(kind(23, "Work", "WAN Miniport (IKEv2)", false), IfKind::Vpn);
        assert_eq!(kind(6, "Tailscale", "Tailscale Tunnel", false), IfKind::Vpn);
    }

    #[test]
    fn tunnels_loopback_and_unknown_types() {
        assert_eq!(
            kind(131, "Teredo Tunneling Pseudo-Interface", "Teredo Tunneling Pseudo-Interface", false),
            IfKind::Tunnel
        );
        assert_eq!(kind(24, "Loopback Pseudo-Interface 1", "Software Loopback Interface 1", false), IfKind::Loopback);
        assert_eq!(kind(53, "Something", "Proprietary", false), IfKind::Virtual);
        assert_eq!(kind(9, "Token Ring", "Token Ring", true), IfKind::Other);
        assert_eq!(
            kind(6, "Bluetooth Network Connection", "Bluetooth Device (Personal Area Network)", false),
            IfKind::Virtual
        );
    }

    #[test]
    fn a_hardware_flag_that_could_not_be_read_fails_closed() {
        assert_eq!(kind(6, "Ethernet", "Intel(R) Ethernet Controller I225-V", false), IfKind::Virtual);
    }
}
