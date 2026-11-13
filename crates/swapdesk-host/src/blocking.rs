//! Devices blocked by their MAC address (D65): the check the session gate and the pairing filter share.

use std::net::IpAddr;

use swapdesk_platform::NeighborTable;
use swapdesk_trust::TrustHandle;

/// A neighbour table that knows nobody: the default of a core and a hub until the service gives them the system's.
pub struct NoNeighbors;

impl NeighborTable for NoNeighbors {
    fn cached_mac(&self, _peer: IpAddr) -> Option<[u8; 6]> {
        None
    }
}

/// Whether the device behind `ip` is blocked by its MAC. The neighbour table is read only when some MAC is blocked,
/// and only as it is (no network traffic): this runs for every incoming connection.
pub fn blocked_by_mac(trust: &TrustHandle, neighbors: &dyn NeighborTable, ip: IpAddr) -> bool {
    trust.any_blocked_mac() && neighbors.cached_mac(ip).is_some_and(|mac| trust.is_blocked_mac(&mac))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use swapdesk_fakes::MemorySecretStore;
    use swapdesk_trust::BlockedDevice;

    use super::*;

    struct Table {
        macs: HashMap<IpAddr, [u8; 6]>,
        lookups: AtomicUsize,
    }

    impl NeighborTable for Table {
        fn cached_mac(&self, peer: IpAddr) -> Option<[u8; 6]> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            self.macs.get(&peer).copied()
        }
    }

    #[test]
    fn a_blocked_mac_is_refused_whatever_address_it_comes_from_and_nothing_is_looked_up_with_an_empty_list() {
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let (old, new) = (IpAddr::V4(Ipv4Addr::new(192, 168, 0, 57)), IpAddr::V4(Ipv4Addr::new(192, 168, 0, 80)));
        let mac = [2, 0x11, 0x22, 0x33, 0x44, 0x55];
        let table = Table { macs: HashMap::from([(new, mac)]), lookups: AtomicUsize::new(0) };
        assert!(!blocked_by_mac(&trust, &table, new));
        assert_eq!(table.lookups.load(Ordering::SeqCst), 0, "no lookup while nothing is blocked");

        let d = BlockedDevice { mac, last_ipv4: [192, 168, 0, 57], name: String::new(), blocked_at_ms: 0 };
        trust.block_mac(d).unwrap();
        assert!(blocked_by_mac(&trust, &table, new), "the same device under a new address (DHCP) is still refused");
        assert!(!blocked_by_mac(&trust, &table, old), "the old address, now someone else's, is not");
        assert!(!blocked_by_mac(&trust, &NoNeighbors, new), "no MAC known: not refused");
    }
}
