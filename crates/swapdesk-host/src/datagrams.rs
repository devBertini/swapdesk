//! What a listening socket lets quinn see (F5 of the 13/11 security review).
//!
//! quinn answers some datagrams by itself, before the application sees an `Incoming`: Version Negotiation, a stateless
//! reset, a Retry. The accept loop's checks (the service's `Gate`, the pairing filter) all come after that, so a
//! blocked device or a neighbour on an interface the owner did not approve still got an answer. These filters run on
//! the endpoint's socket ([`swapdesk_trust::DatagramFilter`]) and drop such a datagram silently.
//!
//! They run for every datagram of every live session on a runtime worker: memory reads under short locks, no OS call.
//!
//! What they cannot do: a device blocked by its MAC is known only through the neighbour table's cache
//! ([`blocked_by_mac`]); a first datagram from a fresh IP whose MAC the OS has not learned yet gets through, and the
//! accept loop then ignores the connection once the table knows the MAC.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use swapdesk_lan::guard::check_incoming;
use swapdesk_platform::{NeighborTable, NetInterface, NetInterfaces};
use swapdesk_trust::{DatagramFilter, TrustHandle};

use crate::blocking::{NoNeighbors, blocked_by_mac};
use crate::netcache::Snapshot;

/// How long the neighbour table's answer for one source address is kept: reading it is an OS call.
const MAC_VERDICT_TTL: Duration = Duration::from_secs(1);
/// Source addresses whose MAC verdict is kept; past it the table is emptied (a flood of sources costs reads, not
/// memory).
const MAX_VERDICTS: usize = 512;

/// Whether a source address is blocked (by address, or by the MAC behind it), for every datagram: the address list is
/// read each time (it is memory), the MAC through the neighbour table once per [`MAC_VERDICT_TTL`] per source.
pub struct SourceBlocks {
    trust: Arc<TrustHandle>,
    macs: Mutex<HashMap<IpAddr, (Instant, bool)>>,
}

impl SourceBlocks {
    pub fn new(trust: Arc<TrustHandle>) -> Self {
        Self { trust, macs: Mutex::new(HashMap::new()) }
    }

    pub fn blocked(&self, neighbors: &dyn NeighborTable, ip: IpAddr) -> bool {
        if self.trust.read(|s| s.is_blocked(ip)) {
            return true;
        }
        if !self.trust.any_blocked_mac() {
            return false;
        }
        let now = Instant::now();
        if let Some((at, verdict)) = self.macs.lock().unwrap_or_else(PoisonError::into_inner).get(&ip)
            && now.saturating_duration_since(*at) < MAC_VERDICT_TTL
        {
            return *verdict;
        }
        let verdict = blocked_by_mac(&self.trust, neighbors, ip);
        let mut macs = self.macs.lock().unwrap_or_else(PoisonError::into_inner);
        if macs.len() >= MAX_VERDICTS {
            macs.clear();
        }
        macs.insert(ip, (now, verdict));
        verdict
    }
}

/// The session endpoint's socket filter: the checks of the accept loop's `Gate::judge` and `is_blocked`, as far as
/// they can be made without an `Incoming`.
pub struct SessionDatagrams {
    nets: Snapshot,
    blocks: SourceBlocks,
    neighbors: Arc<dyn NeighborTable>,
    allow_loopback: bool,
}

impl SessionDatagrams {
    pub fn new(
        interfaces: Arc<dyn NetInterfaces>,
        trust: Arc<TrustHandle>,
        neighbors: Arc<dyn NeighborTable>,
        allow_loopback: bool,
    ) -> Self {
        Self { nets: Snapshot::new(interfaces), blocks: SourceBlocks::new(trust), neighbors, allow_loopback }
    }

    /// True when quinn may see a datagram from `remote` that arrived on `local`: not blocked, and on-link in the subnet
    /// of the approved interface that owns `local` (or loopback to loopback in the development build). Without a known
    /// arrival address the accept loop could only refuse it, so it is dropped.
    pub fn allows(&self, remote: IpAddr, local: Option<IpAddr>) -> bool {
        if self.blocks.blocked(&*self.neighbors, remote) {
            return false;
        }
        let Some(local) = local else { return false };
        if self.allow_loopback && remote.is_loopback() && local.is_loopback() {
            return true;
        }
        self.nets.with(|list| check_incoming(list, local, remote).is_ok())
    }

    pub fn into_filter(self: Arc<Self>) -> DatagramFilter {
        Arc::new(move |remote, local| self.allows(remote, local))
    }
}

/// The pairing endpoint's socket filter: blocked sources, then the same on-link check as its `IncomingFilter`, against
/// the list the hub keeps.
pub fn pairing_datagrams(
    nets: Arc<Mutex<Vec<NetInterface>>>,
    trust: Arc<TrustHandle>,
    neighbors: Arc<OnceLock<Arc<dyn NeighborTable>>>,
    allow_loopback: bool,
) -> DatagramFilter {
    let blocks = SourceBlocks::new(trust);
    Arc::new(move |remote, local| {
        let blocked = match neighbors.get() {
            Some(n) => blocks.blocked(n.as_ref(), remote),
            None => blocks.blocked(&NoNeighbors, remote),
        };
        if blocked {
            return false;
        }
        let Some(local) = local else { return false };
        if allow_loopback && remote.is_loopback() && local.is_loopback() {
            return true;
        }
        check_incoming(&nets.lock().unwrap_or_else(PoisonError::into_inner), local, remote).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use swapdesk_fakes::MemorySecretStore;
    use swapdesk_platform::{IfKind, NetCategory, PlatformError, Result};
    use swapdesk_trust::BlockedDevice;

    use super::*;

    const LOCAL: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 10);

    fn lan() -> NetInterface {
        NetInterface {
            index: 7,
            name: "Ethernet".into(),
            kind: IfKind::Ethernet,
            up: true,
            mac: Some([2, 0, 0, 0, 0, 1]),
            ipv4: vec![(LOCAL, 24)],
            category: Some(NetCategory::Private),
            inbound_blocked: None,
        }
    }

    struct Fixed(Vec<NetInterface>);
    impl NetInterfaces for Fixed {
        fn list(&self) -> Result<Vec<NetInterface>> {
            Ok(self.0.clone())
        }
    }

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

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
    }

    fn setup(macs: HashMap<IpAddr, [u8; 6]>) -> (Arc<TrustHandle>, Arc<Table>, SessionDatagrams) {
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let table = Arc::new(Table { macs, lookups: AtomicUsize::new(0) });
        let guard = SessionDatagrams::new(Arc::new(Fixed(vec![lan()])), Arc::clone(&trust), table.clone(), false);
        (trust, table, guard)
    }

    #[test]
    fn a_neighbour_on_the_approved_subnet_passes_and_every_other_source_is_dropped() {
        let (_trust, _table, guard) = setup(HashMap::new());
        let local = Some(IpAddr::V4(LOCAL));
        assert!(guard.allows(ip(20), local), "a neighbour in the subnet of the approved interface");
        assert!(!guard.allows(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), local), "off-link");
        assert!(!guard.allows(IpAddr::V4(Ipv4Addr::LOCALHOST), local), "loopback");
        assert!(!guard.allows(ip(10), local), "this machine's own address");
        assert!(!guard.allows(ip(20), Some(ip(99))), "arrived on an address of no interface");
        assert!(!guard.allows(ip(20), None), "no arrival address");
    }

    #[test]
    fn a_blocked_address_is_dropped_and_a_blocked_mac_follows_the_device_to_a_new_address() {
        let mac = [2, 0x11, 0x22, 0x33, 0x44, 0x55];
        let (trust, table, guard) = setup(HashMap::from([(ip(80), mac)]));
        let local = Some(IpAddr::V4(LOCAL));
        assert!(guard.allows(ip(80), local));
        assert_eq!(table.lookups.load(Ordering::SeqCst), 0, "no neighbour lookup while nothing is blocked by MAC");
        let blocked = BlockedDevice { mac, last_ipv4: [192, 168, 1, 57], name: String::new(), blocked_at_ms: 0 };
        trust.block_mac(blocked).unwrap();
        assert!(!guard.allows(ip(80), local), "the blocked device under another address (DHCP)");
        for _ in 0..50 {
            assert!(!guard.allows(ip(80), local));
        }
        assert_eq!(table.lookups.load(Ordering::SeqCst), 1, "the neighbour table is read once per source per second");
        assert!(guard.allows(ip(81), local), "a neighbour whose MAC is not blocked");
    }

    #[test]
    fn the_pairing_filter_drops_blocked_sources_and_whatever_is_off_the_hub_s_links() {
        let mac = [2, 0x11, 0x22, 0x33, 0x44, 0x55];
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let table: Arc<dyn NeighborTable> =
            Arc::new(Table { macs: HashMap::from([(ip(80), mac)]), lookups: AtomicUsize::new(0) });
        let neighbors = Arc::new(OnceLock::new());
        let nets = Arc::new(Mutex::new(vec![lan()]));
        let keep = pairing_datagrams(nets, Arc::clone(&trust), Arc::clone(&neighbors), false);
        let local = Some(IpAddr::V4(LOCAL));
        assert!(keep(ip(20), local));
        assert!(!keep(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), local), "off-link");
        assert!(!keep(ip(20), None), "no arrival address");
        trust.update(|s| s.blocked.push([192, 168, 1, 21])).unwrap();
        assert!(!keep(ip(21), local), "a blocked address");
        trust
            .block_mac(BlockedDevice { mac, last_ipv4: [192, 168, 1, 57], name: String::new(), blocked_at_ms: 0 })
            .unwrap();
        assert!(keep(ip(80), local), "the neighbour table is not set yet: only the address list applies");
        neighbors.set(table).ok().unwrap();
        // The verdict given while no neighbour table was set (not blocked) is kept for a second, then read again.
        std::thread::sleep(MAC_VERDICT_TTL + Duration::from_millis(50));
        assert!(!keep(ip(80), local), "the blocked MAC under a new address");
    }

    #[test]
    fn the_development_build_lets_loopback_reach_loopback_only() {
        let trust = TrustHandle::open(Arc::new(MemorySecretStore::new())).unwrap();
        let table = Arc::new(Table { macs: HashMap::new(), lookups: AtomicUsize::new(0) });
        let guard = SessionDatagrams::new(Arc::new(Fixed(Vec::new())), trust, table, true);
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(guard.allows(lo, Some(lo)));
        assert!(!guard.allows(ip(20), Some(lo)));
    }

    #[test]
    fn the_snapshot_never_waits_for_the_os_and_refreshes_behind_the_caller() {
        struct Slow(AtomicUsize);
        impl NetInterfaces for Slow {
            fn list(&self) -> Result<Vec<NetInterface>> {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                if n > 0 {
                    std::thread::sleep(Duration::from_millis(400));
                    return Err(PlatformError {
                        kind: swapdesk_platform::ErrorKind::Os,
                        op: "test",
                        os_code: None,
                        detail: "down".into(),
                    });
                }
                Ok(vec![lan()])
            }
        }
        let slow = Arc::new(Slow(AtomicUsize::new(0)));
        let snap = Snapshot::new(slow.clone());
        std::thread::sleep(crate::netcache::TTL + Duration::from_millis(50));
        let started = Instant::now();
        assert_eq!(snap.with(<[NetInterface]>::len), 1, "the older copy answers while the refresh runs");
        assert!(started.elapsed() < Duration::from_millis(200), "the caller waited for the OS");
        for _ in 0..20 {
            snap.with(<[NetInterface]>::len);
        }
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(slow.0.load(Ordering::SeqCst), 2, "one refresh at a time; a failed one leaves the copy");
        assert_eq!(snap.with(<[NetInterface]>::len), 1);
    }
}
