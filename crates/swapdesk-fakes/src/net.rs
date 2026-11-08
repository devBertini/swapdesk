use std::net::Ipv4Addr;
use std::sync::Mutex;

use swapdesk_platform::{IfKind, NetCategory, NetInterface, NetInterfaces, Result};

use crate::lock;

/// A scripted interface list for the LAN guard.
pub struct FakeNetInterfaces(Mutex<Vec<NetInterface>>);

fn iface(
    index: u32,
    name: &str,
    kind: IfKind,
    mac: Option<[u8; 6]>,
    ip: Option<(Ipv4Addr, u8)>,
    category: Option<NetCategory>,
) -> NetInterface {
    NetInterface {
        index,
        name: name.to_string(),
        kind,
        up: true,
        mac,
        ipv4: ip.into_iter().collect(),
        category,
        inbound_blocked: Some(false),
    }
}

impl FakeNetInterfaces {
    pub fn new(list: Vec<NetInterface>) -> Self {
        Self(Mutex::new(list))
    }

    /// A Windows PC with every kind the guard must tell apart: Ethernet, Wi-Fi, Hyper-V vEthernet, WSL, a VPN, a
    /// tunnel and loopback.
    pub fn windows_typical() -> Self {
        Self::new(vec![
            iface(1, "Loopback Pseudo-Interface 1", IfKind::Loopback, None, Some((Ipv4Addr::LOCALHOST, 8)), None),
            iface(
                4,
                "Ethernet",
                IfKind::Ethernet,
                Some([0x3C, 0x52, 0x82, 0x11, 0x22, 0x33]),
                Some((Ipv4Addr::new(192, 168, 1, 20), 24)),
                Some(NetCategory::Private),
            ),
            iface(
                7,
                "Wi-Fi",
                IfKind::WiFi,
                Some([0x3C, 0x52, 0x82, 0x44, 0x55, 0x66]),
                Some((Ipv4Addr::new(192, 168, 1, 21), 24)),
                Some(NetCategory::Public),
            ),
            iface(
                12,
                "vEthernet (Default Switch)",
                IfKind::Virtual,
                Some([0x00, 0x15, 0x5D, 0x01, 0x02, 0x03]),
                Some((Ipv4Addr::new(172, 24, 32, 1), 20)),
                Some(NetCategory::Private),
            ),
            iface(
                15,
                "vEthernet (WSL)",
                IfKind::Virtual,
                Some([0x00, 0x15, 0x5D, 0x0A, 0x0B, 0x0C]),
                Some((Ipv4Addr::new(172, 29, 0, 1), 20)),
                Some(NetCategory::Private),
            ),
            iface(20, "Work VPN", IfKind::Vpn, None, Some((Ipv4Addr::new(10, 8, 0, 6), 24)), Some(NetCategory::Domain)),
            iface(22, "Teredo Tunneling Pseudo-Interface", IfKind::Tunnel, None, None, None),
        ])
    }

    pub fn set(&self, list: Vec<NetInterface>) {
        *lock(&self.0) = list;
    }
}

impl NetInterfaces for FakeNetInterfaces {
    fn list(&self) -> Result<Vec<NetInterface>> {
        Ok(lock(&self.0).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typical_list_has_every_kind_and_can_be_replaced() {
        let n = FakeNetInterfaces::windows_typical();
        let list = n.list().unwrap();
        for kind in [IfKind::Ethernet, IfKind::WiFi, IfKind::Virtual, IfKind::Vpn, IfKind::Tunnel, IfKind::Loopback] {
            assert!(list.iter().any(|i| i.kind == kind), "{kind:?}");
        }
        assert!(list.iter().any(|i| i.name.starts_with("vEthernet (WSL)")));
        assert_eq!(list.iter().filter(|i| i.kind == IfKind::Virtual).count(), 2);
        let mut down = list[1].clone();
        down.up = false;
        n.set(vec![down]);
        let list = n.list().unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].up);
    }
}
