//! The phone's networks as the guard needs them (docs/ANDROID.md 4, F-AND-02).
//!
//! Java listens to `ConnectivityManager` and hands over, at every change, the whole list of connected networks as one
//! small buffer ([`decode`]). [`Networks`] keeps the newest list and answers the two questions the core asks:
//! which interfaces exist (`NetInterfaces`, read by the LAN guard of `swapdesk-lan`, which approves physical Wi-Fi and
//! Ethernet only) and which Android network a socket bound to a given address belongs to ([`Networks::handle_for`]).
//!
//! The rule that makes a network one the guard may approve is here and nowhere else: a network counts as Wi-Fi or
//! Ethernet only when it has that transport and is not a VPN; mobile data and everything else is `Other`, which the
//! guard never approves (it fails closed on what it does not know).
//!
//! Plain Rust: built and tested on every host.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Mutex;

use swapdesk_lan::guard::approved_addrs;
use swapdesk_platform::{IfKind, NetInterface, NetInterfaces};

/// The transports of a network, as bits Java sets from `NetworkCapabilities.hasTransport`.
pub mod transport {
    pub const WIFI: u8 = 1;
    pub const ETHERNET: u8 = 2;
    pub const CELLULAR: u8 = 4;
    pub const VPN: u8 = 8;
    /// Any transport this file does not name (Bluetooth, USB, a satellite link).
    pub const OTHER: u8 = 16;
}

/// Most networks a buffer may hold, and most addresses of one network: a phone has a handful.
pub const MAX_NETWORKS: usize = 16;
pub const MAX_ADDRS: usize = 8;
/// Longest interface name, in bytes (Linux allows 15).
pub const MAX_NAME: usize = 32;

/// What Java reports of one connected network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkFacts {
    /// `Network.getNetworkHandle()`, the NDK's `net_handle_t`.
    pub handle: u64,
    /// The interface's index (`NetworkInterface.getIndex()`), 0 when Java could not read it.
    pub index: u32,
    /// [`transport`] bits.
    pub transports: u8,
    /// `LinkProperties.getInterfaceName()`.
    pub name: String,
    /// The IPv4 link addresses with their prefix lengths.
    pub ipv4: Vec<(Ipv4Addr, u8)>,
}

impl NetworkFacts {
    /// What kind of interface the guard sees. A VPN is a VPN whatever it runs over; only then Wi-Fi and Ethernet.
    pub fn kind(&self) -> IfKind {
        if self.transports & transport::VPN != 0 {
            IfKind::Vpn
        } else if self.transports & transport::WIFI != 0 {
            IfKind::WiFi
        } else if self.transports & transport::ETHERNET != 0 {
            IfKind::Ethernet
        } else {
            IfKind::Other
        }
    }

    fn interface(&self) -> NetInterface {
        NetInterface {
            index: self.index,
            name: self.name.clone(),
            kind: self.kind(),
            // Java reports connected networks only.
            up: true,
            // Android hides hardware addresses from apps.
            mac: None,
            ipv4: self.ipv4.clone(),
            category: None,
            inbound_blocked: None,
        }
    }
}

/// Why a buffer of network facts was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactsError {
    /// The buffer ends before what it announces.
    Truncated,
    /// A count or a length is over its limit, a prefix is over 32, or bytes are left over.
    Invalid,
    /// An interface name is not UTF-8.
    Name,
}

impl std::fmt::Display for FactsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "the network facts end early",
            Self::Invalid => "the network facts hold a value over its limit",
            Self::Name => "an interface name is not UTF-8",
        })
    }
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], FactsError> {
        if self.0.len() < n {
            return Err(FactsError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, FactsError> {
        Ok(self.take(1)?[0])
    }
}

/// Decodes what Java wrote (`Networks.java`), every number little-endian:
///
/// ```text
/// u8 count, then per network:
///   u64 handle, u32 index, u8 transports, u8 name length, the name (UTF-8),
///   u8 address count, then per address: 4 bytes of IPv4 address, u8 prefix length
/// ```
pub fn decode(bytes: &[u8]) -> Result<Vec<NetworkFacts>, FactsError> {
    let mut c = Cursor(bytes);
    let count = usize::from(c.u8()?);
    if count > MAX_NETWORKS {
        return Err(FactsError::Invalid);
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let handle = u64::from_le_bytes(c.take(8)?.try_into().map_err(|_| FactsError::Truncated)?);
        let index = u32::from_le_bytes(c.take(4)?.try_into().map_err(|_| FactsError::Truncated)?);
        let transports = c.u8()?;
        let name_len = usize::from(c.u8()?);
        if name_len > MAX_NAME {
            return Err(FactsError::Invalid);
        }
        let name = std::str::from_utf8(c.take(name_len)?).map_err(|_| FactsError::Name)?.to_owned();
        let addrs = usize::from(c.u8()?);
        if addrs > MAX_ADDRS {
            return Err(FactsError::Invalid);
        }
        let mut ipv4 = Vec::with_capacity(addrs);
        for _ in 0..addrs {
            let a = c.take(4)?;
            let prefix = c.u8()?;
            if prefix > 32 {
                return Err(FactsError::Invalid);
            }
            ipv4.push((Ipv4Addr::new(a[0], a[1], a[2], a[3]), prefix));
        }
        out.push(NetworkFacts { handle, index, transports, name, ipv4 });
    }
    if !c.0.is_empty() {
        return Err(FactsError::Invalid);
    }
    Ok(out)
}

/// The phone's networks as Java last reported them. Empty until the first report: with no network known, the guard
/// approves nothing.
#[derive(Default)]
pub struct Networks(Mutex<Vec<NetworkFacts>>);

impl Networks {
    pub fn new() -> Self {
        Self::default()
    }

    fn facts(&self) -> std::sync::MutexGuard<'_, Vec<NetworkFacts>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces the list with Java's newest. Returns whether anything changed.
    pub fn set(&self, facts: Vec<NetworkFacts>) -> bool {
        let mut held = self.facts();
        if *held == facts {
            return false;
        }
        *held = facts;
        true
    }

    /// The networks as reported.
    pub fn snapshot(&self) -> Vec<NetworkFacts> {
        self.facts().clone()
    }

    fn interfaces(&self) -> Vec<NetInterface> {
        self.facts().iter().map(NetworkFacts::interface).collect()
    }

    /// The Android network a socket bound to `local` belongs to: the one whose interface the guard approves and that
    /// holds that address. `None`: the address is on no approved network (mobile data, a VPN, an address the phone
    /// no longer has), and a socket on it must not be used.
    pub fn handle_for(&self, local: Ipv4Addr) -> Option<u64> {
        let facts = self.facts();
        let list: Vec<NetInterface> = facts.iter().map(NetworkFacts::interface).collect();
        let owner = approved_addrs(&list).into_iter().find(|a| a.addr == local)?;
        facts.iter().find(|f| f.name == owner.if_name && f.index == owner.if_index).map(|f| f.handle)
    }

    /// What to do with a QUIC socket bound to `local` (the step of `swapdesk_lan::socket::set_after_bind`): bind it
    /// to `Ok(Some(handle))`; leave it alone for `Ok(None)`, a loopback socket, which never leaves the phone; refuse
    /// it for `Err`, an address on no approved network.
    pub fn binding_for(&self, local: SocketAddr) -> Result<Option<u64>, &'static str> {
        let SocketAddr::V4(local) = local else { return Err("a QUIC socket is IPv4") };
        if local.ip().is_loopback() {
            return Ok(None);
        }
        self.handle_for(*local.ip()).map(Some).ok_or("the socket's address is on no approved network")
    }

    /// The network the whole process is bound to, for the sockets of code that cannot be handed one (the discovery
    /// library): the first approved network, Wi-Fi before Ethernet. `None` when no network is approved.
    pub fn process_network(&self) -> Option<u64> {
        let facts = self.facts();
        let list: Vec<NetInterface> = facts.iter().map(NetworkFacts::interface).collect();
        let approved = approved_addrs(&list);
        let pick = approved.iter().find(|a| a.kind == IfKind::WiFi).or_else(|| approved.first())?;
        facts.iter().find(|f| f.name == pick.if_name && f.index == pick.if_index).map(|f| f.handle)
    }
}

impl NetInterfaces for Networks {
    fn list(&self) -> swapdesk_platform::Result<Vec<NetInterface>> {
        Ok(self.interfaces())
    }

    /// The list is already in memory: handing it over reads nothing.
    fn warm_copy(&self) -> Option<Vec<NetInterface>> {
        Some(self.interfaces())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_lan::guard::approved_addrs;

    fn net(handle: u64, index: u32, transports: u8, name: &str, ipv4: &[(Ipv4Addr, u8)]) -> NetworkFacts {
        NetworkFacts { handle, index, transports, name: name.into(), ipv4: ipv4.to_vec() }
    }

    fn encode(list: &[NetworkFacts]) -> Vec<u8> {
        let mut b = vec![list.len() as u8];
        for n in list {
            b.extend_from_slice(&n.handle.to_le_bytes());
            b.extend_from_slice(&n.index.to_le_bytes());
            b.push(n.transports);
            b.push(n.name.len() as u8);
            b.extend_from_slice(n.name.as_bytes());
            b.push(n.ipv4.len() as u8);
            for (a, p) in &n.ipv4 {
                b.extend_from_slice(&a.octets());
                b.push(*p);
            }
        }
        b
    }

    const WIFI: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 23);
    const CELL: Ipv4Addr = Ipv4Addr::new(10, 44, 7, 2);
    const VPN: Ipv4Addr = Ipv4Addr::new(10, 8, 0, 6);

    /// A phone at home: Wi-Fi, mobile data, and a VPN app running over the Wi-Fi.
    fn phone_at_home() -> Vec<NetworkFacts> {
        vec![
            net(0x64_0000_0001, 21, transport::CELLULAR, "rmnet_data0", &[(CELL, 30)]),
            net(0x65_0000_0001, 23, transport::WIFI, "wlan0", &[(WIFI, 24)]),
            net(0x66_0000_0001, 31, transport::VPN | transport::WIFI, "tun0", &[(VPN, 32)]),
        ]
    }

    /// F-AND-02: only a Wi-Fi or Ethernet network that is not a VPN is approved; mobile data never is, and a VPN is a
    /// VPN whatever it runs over.
    #[test]
    fn only_wifi_and_ethernet_that_are_not_a_vpn_are_approved() {
        let networks = Networks::new();
        assert!(approved_addrs(&networks.list().unwrap()).is_empty(), "before the first report nothing is approved");
        assert!(networks.set(phone_at_home()));
        let approved = approved_addrs(&networks.list().unwrap());
        assert_eq!(approved.len(), 1);
        assert_eq!((approved[0].if_name.as_str(), approved[0].addr, approved[0].prefix), ("wlan0", WIFI, 24));

        let kinds: Vec<IfKind> = phone_at_home().iter().map(NetworkFacts::kind).collect();
        assert_eq!(kinds, [IfKind::Other, IfKind::WiFi, IfKind::Vpn]);
        assert_eq!(net(1, 1, transport::ETHERNET, "eth0", &[]).kind(), IfKind::Ethernet);
        assert_eq!(net(1, 1, transport::OTHER, "bt-pan", &[]).kind(), IfKind::Other);
        assert_eq!(net(1, 1, transport::VPN | transport::ETHERNET, "tun1", &[]).kind(), IfKind::Vpn);
        assert_eq!(net(1, 1, 0, "unknown", &[]).kind(), IfKind::Other, "no transport named: not approved");
    }

    /// A socket is bound to the network that holds its address, and only when the guard approves that network.
    #[test]
    fn a_socket_is_bound_to_the_approved_network_that_holds_its_address() {
        let networks = Networks::new();
        networks.set(phone_at_home());
        assert_eq!(networks.handle_for(WIFI), Some(0x65_0000_0001));
        assert_eq!(networks.handle_for(CELL), None, "mobile data");
        assert_eq!(networks.handle_for(VPN), None, "a VPN");
        assert_eq!(networks.handle_for(Ipv4Addr::new(192, 168, 1, 99)), None, "not an address of this phone");
        assert_eq!(networks.process_network(), Some(0x65_0000_0001));
    }

    /// The step on every QUIC socket: the Wi-Fi socket is bound to the Wi-Fi network, a loopback socket is left
    /// alone, and a socket on any other address is refused (the wildcard address too: it names no network).
    #[test]
    fn a_quic_socket_is_bound_left_alone_or_refused_by_its_address() {
        let networks = Networks::new();
        networks.set(phone_at_home());
        let at = |ip: Ipv4Addr| SocketAddr::from((ip, 50000));
        assert_eq!(networks.binding_for(at(WIFI)), Ok(Some(0x65_0000_0001)));
        assert_eq!(networks.binding_for(at(Ipv4Addr::LOCALHOST)), Ok(None));
        for refused in [CELL, VPN, Ipv4Addr::UNSPECIFIED] {
            assert!(networks.binding_for(at(refused)).is_err(), "{refused}");
        }
        assert!(networks.binding_for("[::1]:50000".parse().unwrap()).is_err());
    }

    /// When Wi-Fi drops and the phone moves to mobile data, no network is approved (docs/ANDROID.md 4).
    #[test]
    fn with_wifi_gone_nothing_is_approved() {
        let networks = Networks::new();
        networks.set(phone_at_home());
        let only_cell: Vec<NetworkFacts> = phone_at_home().into_iter().filter(|n| n.name == "rmnet_data0").collect();
        assert!(networks.set(only_cell.clone()));
        assert!(!networks.set(only_cell), "the same list again is no change");
        assert!(approved_addrs(&networks.list().unwrap()).is_empty());
        assert_eq!((networks.handle_for(WIFI), networks.process_network()), (None, None));
    }

    /// With Wi-Fi and Ethernet both approved, each socket goes to its own network and the process to the Wi-Fi.
    #[test]
    fn two_approved_networks_keep_their_own_sockets() {
        let cable = Ipv4Addr::new(192, 168, 7, 4);
        let networks = Networks::new();
        networks.set(vec![
            net(7, 40, transport::ETHERNET, "eth0", &[(cable, 24)]),
            net(9, 23, transport::WIFI, "wlan0", &[(WIFI, 24)]),
        ]);
        assert_eq!((networks.handle_for(cable), networks.handle_for(WIFI)), (Some(7), Some(9)));
        assert_eq!(networks.process_network(), Some(9));
    }

    #[test]
    fn what_java_writes_decodes_and_anything_else_is_refused() {
        let list = phone_at_home();
        let bytes = encode(&list);
        assert_eq!(decode(&bytes).unwrap(), list);
        assert_eq!(decode(&[0]).unwrap(), Vec::new());
        // Every shorter buffer ends early, and a longer one has bytes left over.
        for cut in 0..bytes.len() {
            assert_eq!(decode(&bytes[..cut]), Err(FactsError::Truncated), "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode(&longer), Err(FactsError::Invalid));
        // Limits: the count, a name's length, a prefix.
        assert_eq!(decode(&[MAX_NETWORKS as u8 + 1]), Err(FactsError::Invalid));
        let mut bad_prefix = encode(&[net(1, 1, transport::WIFI, "wlan0", &[(WIFI, 24)])]);
        *bad_prefix.last_mut().unwrap() = 33;
        assert_eq!(decode(&bad_prefix), Err(FactsError::Invalid));
        let mut bad_name = encode(&[net(1, 1, transport::WIFI, "wl", &[])]);
        let at = 1 + 8 + 4 + 1 + 1;
        bad_name[at] = 0xFF;
        assert_eq!(decode(&bad_name), Err(FactsError::Name));
    }
}
