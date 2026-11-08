//! DNS-SD discovery (F-FIND-01, F-FIND-02): announce and browse `_swapdesk._udp` with `mdns-sd`, on the interfaces the
//! guard approves and nowhere else.
//!
//! - The daemon is told to ignore every interface, then to use only the approved ones (IPv4, by index). The service
//!   is registered for those interfaces and those addresses only.
//! - `mdns-sd`'s own interface re-check is off (`set_ip_check_interval(0)`, research dependencies.md 1.4): the caller
//!   learns of a network change from its `NetInterfaces` watcher and calls [`Discovery::refresh`].
//! - A resolved service is turned into a [`Peer`] only if its TXT record decodes, it is not this machine, and every
//!   address the guard keeps is on-link in the subnet of the approved interface the answer arrived on.
//! - The mDNS sockets are `mdns-sd`'s: TTL 255, never changed here.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use mdns_sd::{IfKind as MdnsIfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use swapdesk_platform::{NetInterfaces, PlatformError};
use swapdesk_proto::SESSION_PORT;

use crate::guard::{ApprovedAddr, PeerLink, approved_addrs, check_discovered};
use crate::txt::{FINGERPRINT_BYTES, Txt, TxtError, fingerprint_hex};

/// The DNS-SD service type (SPEC F-FIND-01).
pub const SERVICE_TYPE: &str = "_swapdesk._udp.local.";
/// The standard mDNS port; tests use another one so they never meet a real machine's traffic.
pub const MDNS_PORT: u16 = 5353;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// UDP port the mDNS daemon binds (5353 in production).
    pub mdns_port: u16,
    /// The session endpoint port announced in the SRV record.
    pub service_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self { mdns_port: MDNS_PORT, service_port: SESSION_PORT }
    }
}

/// A device found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub txt: Txt,
    /// The session endpoint port from the SRV record.
    pub port: u16,
    /// The addresses that passed the guard, each with the approved interface that reaches it.
    pub links: Vec<PeerLink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    /// A peer appeared or its record changed (for example `pair=1` came or went). Keyed by `txt.fingerprint`.
    Found(Peer),
    /// The peer sent a goodbye or its record expired.
    Lost { fingerprint: [u8; FINGERPRINT_BYTES] },
}

#[derive(Debug)]
pub enum DiscoveryError {
    Mdns(String),
    Txt(TxtError),
    Platform(PlatformError),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mdns(e) => write!(f, "mDNS: {e}"),
            Self::Txt(e) => write!(f, "TXT: {e}"),
            Self::Platform(e) => write!(f, "interfaces: {e}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

fn mdns(e: mdns_sd::Error) -> DiscoveryError {
    DiscoveryError::Mdns(e.to_string())
}

struct State {
    own: Txt,
    /// The approved addresses the daemon is bound to now (empty: nothing is announced or browsed).
    applied: Vec<ApprovedAddr>,
    registered: Option<String>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct Discovery {
    daemon: ServiceDaemon,
    nets: Arc<dyn NetInterfaces>,
    config: Config,
    state: Mutex<State>,
    events: Mutex<Receiver<DiscoveryEvent>>,
    worker: Option<JoinHandle<()>>,
}

fn instance_name(fp: &[u8; FINGERPRINT_BYTES]) -> String {
    fingerprint_hex(fp)
}

fn host_name(fp: &[u8; FINGERPRINT_BYTES]) -> String {
    format!("swapdesk-{}.local.", &fingerprint_hex(fp)[..12])
}

impl Discovery {
    /// Starts the daemon on the approved interfaces, announces `own` and starts browsing.
    pub fn start(nets: Arc<dyn NetInterfaces>, own: Txt, config: Config) -> Result<Self, DiscoveryError> {
        own.encode().map_err(DiscoveryError::Txt)?;
        let daemon = ServiceDaemon::new_with_port(config.mdns_port).map_err(mdns)?;
        daemon.set_ip_check_interval(0).map_err(mdns)?;
        // Nothing is used until the guard approves it.
        daemon.disable_interface(MdnsIfKind::All).map_err(mdns)?;

        let browse = daemon.browse(SERVICE_TYPE).map_err(mdns)?;
        let (tx, rx) = mpsc::channel();
        let worker_nets = Arc::clone(&nets);
        let own_fp = own.fingerprint;
        let worker = std::thread::Builder::new()
            .name("swapdesk-discovery".into())
            .spawn(move || run_worker(browse, worker_nets, own_fp, tx))
            .map_err(|e| DiscoveryError::Mdns(format!("thread: {e}")))?;

        let d = Self {
            daemon,
            nets,
            config,
            state: Mutex::new(State { own, applied: Vec::new(), registered: None }),
            events: Mutex::new(rx),
            worker: Some(worker),
        };
        d.refresh()?;
        Ok(d)
    }

    /// Re-reads the interfaces and, if the approved addresses changed, rebinds the daemon and re-announces. Returns
    /// whether anything changed. Call it when the network changes (and never on a timer).
    pub fn refresh(&self) -> Result<bool, DiscoveryError> {
        let list = self.nets.list().map_err(DiscoveryError::Platform)?;
        let approved = approved_addrs(&list);
        let mut st = lock(&self.state);
        if approved == st.applied {
            return Ok(false);
        }
        self.apply(&mut st, approved)?;
        Ok(true)
    }

    /// Adds or removes `pair=1` in the announcement (PROTOCOL.md 3, pairing mode).
    pub fn set_pairing(&self, on: bool) -> Result<(), DiscoveryError> {
        let mut st = lock(&self.state);
        if st.own.pairing == on {
            return Ok(());
        }
        st.own.pairing = on;
        let approved = st.applied.clone();
        self.register(&mut st, &approved)
    }

    /// The next event, or `None` after `timeout`.
    pub fn next_event(&self, timeout: Duration) -> Option<DiscoveryEvent> {
        lock(&self.events).recv_timeout(timeout).ok()
    }

    /// The approved addresses the daemon is using now.
    pub fn applied(&self) -> Vec<ApprovedAddr> {
        lock(&self.state).applied.clone()
    }

    fn apply(&self, st: &mut State, approved: Vec<ApprovedAddr>) -> Result<(), DiscoveryError> {
        self.daemon.disable_interface(MdnsIfKind::All).map_err(mdns)?;
        let mut indexes: Vec<u32> = approved.iter().map(|a| a.if_index).collect();
        indexes.sort_unstable();
        indexes.dedup();
        if !indexes.is_empty() {
            self.daemon
                .enable_interface(indexes.iter().map(|i| MdnsIfKind::IndexV4(*i)).collect::<Vec<_>>())
                .map_err(mdns)?;
        }
        self.register(st, &approved)?;
        st.applied = approved;
        Ok(())
    }

    fn register(&self, st: &mut State, approved: &[ApprovedAddr]) -> Result<(), DiscoveryError> {
        if approved.is_empty() {
            if let Some(full) = st.registered.take() {
                self.daemon.unregister(&full).map_err(mdns)?;
            }
            return Ok(());
        }
        let props = st.own.encode().map_err(DiscoveryError::Txt)?;
        let addrs: Vec<IpAddr> = approved.iter().map(|a| IpAddr::V4(a.addr)).collect();
        let mut info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name(&st.own.fingerprint),
            &host_name(&st.own.fingerprint),
            addrs.as_slice(),
            self.config.service_port,
            props.as_slice(),
        )
        .map_err(mdns)?;
        let mut indexes: Vec<u32> = approved.iter().map(|a| a.if_index).collect();
        indexes.sort_unstable();
        indexes.dedup();
        info.set_interfaces(indexes.into_iter().map(MdnsIfKind::IndexV4).collect());
        let full = info.get_fullname().to_string();
        self.daemon.register(info).map_err(mdns)?;
        st.registered = Some(full);
        Ok(())
    }

    /// Sends the goodbye packets and stops the daemon.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(full) = lock(&self.state).registered.take()
            && let Ok(done) = self.daemon.unregister(&full)
        {
            let _ = done.recv_timeout(Duration::from_secs(2));
        }
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(Duration::from_secs(2));
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_worker(
    browse: mdns_sd::Receiver<ServiceEvent>,
    nets: Arc<dyn NetInterfaces>,
    own_fp: [u8; FINGERPRINT_BYTES],
    tx: Sender<DiscoveryEvent>,
) {
    // fullname -> fingerprint of the peers reported as found, so a removal can name them.
    let mut known: HashMap<String, [u8; FINGERPRINT_BYTES]> = HashMap::new();
    // Blocks until an event or until the daemon is shut down (the channel closes): no timer, no polling.
    while let Ok(event) = browse.recv() {
        match event {
            ServiceEvent::ServiceResolved(rs) => {
                let list = match nets.list() {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                let entries: Vec<(&str, &[u8])> =
                    rs.txt_properties.iter().map(|p| (p.key(), p.val().unwrap_or(&[]))).collect();
                let Ok(txt) = Txt::decode(&entries) else {
                    drop_known(&mut known, &rs.fullname, &tx);
                    continue;
                };
                if txt.fingerprint == own_fp {
                    continue;
                }
                let mut links: Vec<PeerLink> = Vec::new();
                for sip in &rs.addresses {
                    let mdns_sd::ScopedIp::V4(v4) = sip else { continue };
                    for id in v4.interface_ids() {
                        if let Ok(link) = check_discovered(&list, id.index, IpAddr::V4(*v4.addr()))
                            && !links.contains(&link)
                        {
                            links.push(link);
                        }
                    }
                }
                links.sort_by_key(|l| (l.if_index, u32::from(l.remote)));
                if links.is_empty() || rs.port == 0 {
                    drop_known(&mut known, &rs.fullname, &tx);
                    continue;
                }
                known.insert(rs.fullname.clone(), txt.fingerprint);
                if tx.send(DiscoveryEvent::Found(Peer { txt, port: rs.port, links })).is_err() {
                    return;
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) if drop_known_checked(&mut known, &fullname, &tx).is_err() => {
                return;
            }
            _ => {}
        }
    }
}

fn drop_known(known: &mut HashMap<String, [u8; FINGERPRINT_BYTES]>, fullname: &str, tx: &Sender<DiscoveryEvent>) {
    let _ = drop_known_checked(known, fullname, tx);
}

fn drop_known_checked(
    known: &mut HashMap<String, [u8; FINGERPRINT_BYTES]>,
    fullname: &str,
    tx: &Sender<DiscoveryEvent>,
) -> Result<(), ()> {
    if let Some(fingerprint) = known.remove(fullname) {
        tx.send(DiscoveryEvent::Lost { fingerprint }).map_err(|_| ())?;
    }
    Ok(())
}

/// The addresses of the approved list as plain IPs (handy for tests and logs).
pub fn addrs_of(list: &[ApprovedAddr]) -> Vec<Ipv4Addr> {
    list.iter().map(|a| a.addr).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_and_host_names_come_from_the_fingerprint() {
        let fp = [0xAB; 16];
        assert_eq!(instance_name(&fp), "ab".repeat(16));
        assert_eq!(host_name(&fp), "swapdesk-abababababab.local.");
        // A DNS label is at most 63 bytes.
        assert!(instance_name(&fp).len() <= 63);
    }

    #[test]
    fn this_module_never_sets_a_ttl() {
        // mDNS keeps the TTL 255 that mdns-sd sets; only socket.rs sets IP TTL 1, on QUIC sockets.
        let src = include_str!("discovery.rs");
        let code: String = src.split("#[cfg(test)]").next().unwrap().to_string();
        for forbidden in ["set_ttl", "set_multicast_ttl", "IP_TTL", "IP_MULTICAST_TTL", "ttl("] {
            assert!(!code.contains(forbidden), "discovery.rs must not touch TTL: found {forbidden}");
        }
    }
}
