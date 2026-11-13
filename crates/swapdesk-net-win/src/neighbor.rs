//! `NeighborTable` for Windows (D65): the peer's MAC from the IP Helper neighbour table. The interface comes from
//! `GetBestInterfaceEx`, the entry from `GetIpNetEntry2`; when the entry is missing or has no usable address,
//! `ResolveIpNetEntry2` asks the network (ARP for IPv4, Neighbour Discovery for IPv6).
//!
//! Sources (read 2026-11-12): <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-getipnetentry2>,
//! <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/nf-netioapi-resolveipnetentry2> (flushes the entry
//! on the interface, sends ARP/ND, `Address` plus `InterfaceLuid` or `InterfaceIndex` must be set) and
//! <https://learn.microsoft.com/en-us/windows/win32/api/netioapi/ns-netioapi-mib_ipnet_row2>.

use core::net::IpAddr;

use swapdesk_platform::NeighborTable;
use windows_sys::Win32::Foundation::NO_ERROR;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetBestInterfaceEx, GetIpNetEntry2, MIB_IPNET_ROW2, ResolveIpNetEntry2,
};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, NlnsDelay, NlnsPermanent, NlnsProbe, NlnsReachable, NlnsStale, SOCKADDR, SOCKADDR_INET,
};

/// `NeighborTable` over the live Windows neighbour table.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsNeighborTable;

impl NeighborTable for WindowsNeighborTable {
    /// `GetIpNetEntry2` only: no network traffic.
    fn cached_mac(&self, peer: IpAddr) -> Option<[u8; 6]> {
        lookup(peer, false)
    }

    /// The cached entry, else `ResolveIpNetEntry2` (sends ARP or Neighbour Solicitation and waits for the answer).
    fn resolve_mac(&self, peer: IpAddr) -> Option<[u8; 6]> {
        lookup(peer, true)
    }
}

fn lookup(peer: IpAddr, resolve: bool) -> Option<[u8; 6]> {
    {
        if peer.is_loopback() || peer.is_unspecified() || peer.is_multicast() {
            return None;
        }
        let address = sockaddr_inet(peer);
        let mut if_index = 0u32;
        // SAFETY: `SOCKADDR_INET` starts with the address family and is at least as large as the sockaddr the call
        // reads for that family; `if_index` is a valid out pointer.
        let rc = unsafe { GetBestInterfaceEx(std::ptr::from_ref(&address).cast::<SOCKADDR>(), &mut if_index) };
        if rc != NO_ERROR || if_index == 0 {
            return None;
        }
        // SAFETY: MIB_IPNET_ROW2 is plain data for which all-zero bytes are a valid value.
        let mut row: MIB_IPNET_ROW2 = unsafe { std::mem::zeroed() };
        row.Address = address;
        row.InterfaceIndex = if_index;
        // SAFETY: `row` is a valid MIB_IPNET_ROW2 with Address and InterfaceIndex set, as the call requires.
        let found = unsafe { GetIpNetEntry2(&mut row) } == NO_ERROR;
        if found && let Some(mac) = usable_mac(row.State, row.PhysicalAddressLength, &row.PhysicalAddress) {
            return Some(mac);
        }
        if !resolve {
            return None;
        }
        // SAFETY: as above.
        let mut row: MIB_IPNET_ROW2 = unsafe { std::mem::zeroed() };
        row.Address = address;
        row.InterfaceIndex = if_index;
        // SAFETY: `row` is valid as above; a null source address lets Windows pick the interface, and `row` receives
        // the result.
        if unsafe { ResolveIpNetEntry2(&mut row, std::ptr::null()) } != NO_ERROR {
            return None;
        }
        usable_mac(row.State, row.PhysicalAddressLength, &row.PhysicalAddress)
    }
}

fn sockaddr_inet(peer: IpAddr) -> SOCKADDR_INET {
    // SAFETY: SOCKADDR_INET is plain data for which all-zero bytes are a valid value.
    let mut sa: SOCKADDR_INET = unsafe { std::mem::zeroed() };
    match peer {
        IpAddr::V4(v4) => {
            // Writing a union field is safe; the family is set with the address.
            sa.Ipv4.sin_family = AF_INET;
            sa.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes(v4.octets());
        }
        IpAddr::V6(v6) => {
            sa.Ipv6.sin6_family = AF_INET6;
            sa.Ipv6.sin6_addr.u.Byte = v6.octets();
        }
    }
    sa
}

/// The pure filter: a neighbour state that carries a resolved address (reachable, stale, permanent, and the probe and
/// delay steps a stale entry passes through), a 6-byte address and not all zero.
fn usable_mac(state: i32, len: u32, bytes: &[u8]) -> Option<[u8; 6]> {
    const RESOLVED: [i32; 5] = [NlnsReachable, NlnsStale, NlnsPermanent, NlnsProbe, NlnsDelay];
    if !RESOLVED.contains(&state) || len != 6 {
        return None;
    }
    let mac: [u8; 6] = bytes.get(..6)?.try_into().ok()?;
    (mac != [0u8; 6]).then_some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_UNSPEC, NlnsIncomplete, NlnsUnreachable, SOCKADDR_IN};

    const MAC: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];

    #[test]
    fn filter_accepts_resolved_states() {
        let mut bytes = [0u8; 32];
        bytes[..6].copy_from_slice(&MAC);
        for s in [NlnsReachable, NlnsStale, NlnsPermanent, NlnsProbe, NlnsDelay] {
            assert_eq!(usable_mac(s, 6, &bytes), Some(MAC), "state {s}");
        }
    }

    #[test]
    fn filter_refuses_unresolved_states_bad_length_and_zero() {
        let mut bytes = [0u8; 32];
        bytes[..6].copy_from_slice(&MAC);
        assert_eq!(usable_mac(NlnsUnreachable, 6, &bytes), None);
        assert_eq!(usable_mac(NlnsIncomplete, 6, &bytes), None);
        assert_eq!(usable_mac(NlnsReachable, 0, &bytes), None);
        assert_eq!(usable_mac(NlnsReachable, 8, &bytes), None);
        assert_eq!(usable_mac(NlnsReachable, 6, &[0u8; 32]), None);
        assert_eq!(usable_mac(NlnsReachable, 6, &MAC[..3]), None);
    }

    #[test]
    fn loopback_and_unspecified_are_none() {
        let t = WindowsNeighborTable;
        assert_eq!(t.resolve_mac("127.0.0.1".parse().unwrap()), None);
        assert_eq!(t.resolve_mac("::1".parse().unwrap()), None);
        assert_eq!(t.resolve_mac("0.0.0.0".parse().unwrap()), None);
        assert_eq!(t.cached_mac("127.0.0.1".parse().unwrap()), None);
        assert_eq!(t.cached_mac("0.0.0.0".parse().unwrap()), None);
        assert_eq!(t.cached_mac("::1".parse().unwrap()), None);
    }

    /// IPv4 gateways of every adapter (GetAdaptersAddresses with gateways included).
    fn ipv4_gateways() -> Vec<std::net::Ipv4Addr> {
        let flags = GAA_FLAG_INCLUDE_GATEWAYS
            | GAA_FLAG_SKIP_UNICAST
            | GAA_FLAG_SKIP_ANYCAST
            | GAA_FLAG_SKIP_MULTICAST
            | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 32 * 1024;
        let mut buf: Vec<u64> = Vec::new();
        for _ in 0..5 {
            buf.resize((size as usize).div_ceil(8), 0);
            // SAFETY: `buf` holds at least `size` bytes, 8-byte aligned; `size` is updated by the call.
            let rc = unsafe {
                GetAdaptersAddresses(
                    u32::from(AF_UNSPEC),
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                    &mut size,
                )
            };
            if rc == NO_ERROR {
                break;
            }
        }
        let mut out = Vec::new();
        let mut cur = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !cur.is_null() {
            // SAFETY: `cur` walks the linked list inside `buf`, which outlives the loop.
            let a = unsafe { &*cur };
            let mut g = a.FirstGatewayAddress;
            while !g.is_null() {
                // SAFETY: the gateway list lives in the same buffer.
                let ga = unsafe { &*g };
                let sa = ga.Address.lpSockaddr;
                // SAFETY: when non-null, `lpSockaddr` points at a sockaddr inside the buffer.
                if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                    // SAFETY: the family is AF_INET, so the sockaddr is a SOCKADDR_IN.
                    let raw = unsafe { (*sa.cast::<SOCKADDR_IN>()).sin_addr.S_un.S_addr };
                    out.push(std::net::Ipv4Addr::from(raw.to_ne_bytes()));
                }
                g = ga.Next;
            }
            cur = a.Next;
        }
        out
    }

    #[test]
    fn default_gateway_mac_resolves() {
        let gateways = ipv4_gateways();
        if gateways.is_empty() {
            eprintln!("skipped: no IPv4 default route on this host");
            return;
        }
        let t = WindowsNeighborTable;
        let found: Vec<_> = gateways.iter().filter_map(|g| t.resolve_mac(IpAddr::V4(*g)).map(|m| (*g, m))).collect();
        eprintln!("gateways {gateways:?} -> {found:?}");
        assert!(!found.is_empty(), "no gateway MAC resolved from {gateways:?}");
    }
}
