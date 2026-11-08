//! The Wake-on-LAN packet of F-WAKE-01 (PROTOCOL.md section 6): the magic packet and the broadcast address of the
//! approved local interface whose subnet holds the sleeping device's last address. Sending (TTL 1, UDP port 9, the
//! 60 s wait) belongs to `swapdesk-extras`; the broadcast address never comes from the peer.

use std::net::Ipv4Addr;

use swapdesk_platform::NetInterface;

use crate::guard::{PeerLink, Refusal, find_on_link};

/// UDP destination port of the magic packet (PROTOCOL.md: port 9, a GUESS that stage 22 confirms).
pub const WOL_PORT: u16 = 9;
/// 6 bytes of 0xFF, then the MAC 16 times.
pub const MAGIC_PACKET_LEN: usize = 6 + 6 * 16;

/// The magic packet for one MAC address.
pub fn magic_packet(mac: [u8; 6]) -> [u8; MAGIC_PACKET_LEN] {
    let mut p = [0xFF; MAGIC_PACKET_LEN];
    for chunk in p[6..].as_chunks_mut::<6>().0 {
        *chunk = mac;
    }
    p
}

/// The directed-broadcast address of `addr/prefix`: the host bits all ones.
pub fn directed_broadcast(addr: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let host_bits = if prefix >= 32 { 0 } else { u32::MAX >> prefix };
    Ipv4Addr::from(u32::from(addr) | host_bits)
}

/// Where to send the magic packet: from `local` (an address of the approved interface `if_index`) to `broadcast`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeRoute {
    pub if_index: u32,
    pub if_name: String,
    pub local: Ipv4Addr,
    pub broadcast: Ipv4Addr,
}

/// The route for a device last seen at `last_addr`: the approved interface whose subnet holds it. Refused when the
/// address is off-link, own, loopback or only on a virtual, VPN or tunnel interface.
pub fn wake_route(list: &[NetInterface], last_addr: Ipv4Addr) -> Result<WakeRoute, Refusal> {
    let PeerLink { if_index, if_name, local, prefix, .. } = find_on_link(list, last_addr)?;
    Ok(WakeRoute { if_index, if_name, local, broadcast: directed_broadcast(local, prefix) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_fakes::FakeNetInterfaces;
    use swapdesk_platform::{IfKind, NetInterfaces};

    #[test]
    fn magic_packet_is_six_ff_then_the_mac_sixteen_times() {
        let mac = [0x3C, 0x52, 0x82, 0x44, 0x55, 0x66];
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac, "repetition {i}");
        }
        // A known vector, byte for byte.
        let mut expect = vec![0xFF; 6];
        for _ in 0..16 {
            expect.extend_from_slice(&[0x3C, 0x52, 0x82, 0x44, 0x55, 0x66]);
        }
        assert_eq!(p.as_slice(), expect.as_slice());
        assert_eq!(magic_packet([0; 6])[6..], [0u8; 96]);
    }

    #[test]
    fn broadcast_addresses() {
        let b = |a: [u8; 4], p| directed_broadcast(Ipv4Addr::from(a), p);
        assert_eq!(b([192, 168, 1, 21], 24), Ipv4Addr::new(192, 168, 1, 255));
        assert_eq!(b([10, 0, 0, 5], 8), Ipv4Addr::new(10, 255, 255, 255));
        assert_eq!(b([172, 16, 5, 9], 20), Ipv4Addr::new(172, 16, 15, 255));
        assert_eq!(b([192, 168, 1, 130], 25), Ipv4Addr::new(192, 168, 1, 255));
        assert_eq!(b([192, 168, 1, 100], 26), Ipv4Addr::new(192, 168, 1, 127));
        assert_eq!(b([192, 168, 1, 100], 30), Ipv4Addr::new(192, 168, 1, 103));
    }

    #[test]
    fn route_picks_the_approved_interface_that_holds_the_last_address() {
        let list = FakeNetInterfaces::windows_typical().list().unwrap();
        let r = wake_route(&list, Ipv4Addr::new(192, 168, 1, 50)).unwrap();
        assert_eq!(r.broadcast, Ipv4Addr::new(192, 168, 1, 255));
        assert_eq!(r.if_name, "Ethernet");

        // Two approved interfaces on different subnets: the one that holds the address wins.
        let mut list = list;
        list.iter_mut().find(|i| i.name == "Wi-Fi").unwrap().ipv4 = vec![(Ipv4Addr::new(10, 1, 0, 8), 16)];
        let r = wake_route(&list, Ipv4Addr::new(10, 1, 9, 9)).unwrap();
        assert_eq!((r.if_name.as_str(), r.broadcast), ("Wi-Fi", Ipv4Addr::new(10, 1, 255, 255)));
    }

    #[test]
    fn route_refuses_off_link_own_loopback_and_virtual_addresses() {
        let list = FakeNetInterfaces::windows_typical().list().unwrap();
        assert_eq!(wake_route(&list, Ipv4Addr::new(192, 168, 7, 7)), Err(Refusal::OffLink));
        assert_eq!(wake_route(&list, Ipv4Addr::new(192, 168, 1, 21)), Err(Refusal::OwnAddress));
        assert_eq!(wake_route(&list, Ipv4Addr::LOCALHOST), Err(Refusal::Loopback));
        assert_eq!(
            wake_route(&list, Ipv4Addr::new(172, 29, 0, 9)),
            Err(Refusal::InterfaceNotApproved(IfKind::Virtual))
        );
        assert_eq!(wake_route(&list, Ipv4Addr::new(10, 8, 0, 9)), Err(Refusal::InterfaceNotApproved(IfKind::Vpn)));
    }
}
