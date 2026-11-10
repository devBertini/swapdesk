//! DNS-SD discovery (F-FIND-01, F-FIND-02): announce and browse `_swapdesk._udp` with `mdns-sd`, on the interfaces the
//! guard approves and nowhere else.
//!
//! - The daemon is told to ignore every interface, then to use only the approved ones (IPv4, by index). The service
//!   is registered for those interfaces and those addresses only.
//! - `mdns-sd`'s own interface re-check is off (`set_ip_check_interval(0)`, research dependencies.md 1.4): the caller
//!   learns of a network change from its `NetInterfaces` watcher and calls [`Discovery::refresh`].
//! - When the approved addresses change, the daemon is replaced by a fresh one. With the re-check off, `mdns-sd` never
//!   forgets an address the machine lost (disabling an interface acts only on the addresses the OS lists now), and while
//!   it still holds an IPv4 address for an interface it does not join the mDNS group through that interface's new one:
//!   after a DHCP change it went on answering nothing and hearing nothing (seen on a Mac, 09/11). A fresh daemon has no
//!   such memory. The peers the old one knew are reported lost, and the new one finds them again with its first query.
//! - A resolved service is turned into a [`Peer`] only if its TXT record decodes, it is not this machine, and every
//!   address the guard keeps is on-link in the subnet of the approved interface the answer arrived on.
//! - The mDNS sockets are `mdns-sd`'s: TTL 255, never changed here.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mdns_sd::{IfKind as MdnsIfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use swapdesk_platform::{NetInterface, NetInterfaces, PlatformError};
use swapdesk_proto::SESSION_PORT;

use crate::guard::{ApprovedAddr, PeerLink, approved_addrs, check_discovered};
use crate::txt::{FINGERPRINT_BYTES, Txt, TxtError, fingerprint_hex, minor_entry, read_minor};

/// The DNS-SD service type (SPEC F-FIND-01).
pub const SERVICE_TYPE: &str = "_swapdesk._udp.local.";
/// The standard mDNS port; tests use another one so they never meet a real machine's traffic.
pub const MDNS_PORT: u16 = 5353;
/// Distinct peers (by fingerprint) with an event waiting for the reader, and distinct service names remembered as
/// found. Anyone on the LAN can announce as many names as it likes, so both are bounded: a name or a peer past the
/// bound is ignored (and counted) until room appears. Far above the number of machines of any home or office LAN.
pub const MAX_PEERS: usize = 256;
/// How long the worker keeps the interface list it read for a resolved service.
const LIST_TTL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// UDP port the mDNS daemon binds (5353 in production).
    pub mdns_port: u16,
    /// The session endpoint port announced in the SRV record.
    pub service_port: u16,
    /// The protocol minor put in each own record as the `minor` key (none for 0).
    pub minor: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self { mdns_port: MDNS_PORT, service_port: SESSION_PORT, minor: swapdesk_proto::PROTO_MINOR }
    }
}

/// A device found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub txt: Txt,
    /// The session endpoint port from the SRV record.
    pub port: u16,
    /// The protocol minor of its record (`minor` key; 0 when absent or bad: a 1.0 peer).
    pub minor: u16,
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
    /// The daemon of the addresses in `applied`; `None` only when starting a new one failed (the next refresh retries).
    engine: Option<Engine>,
}

/// One `mdns-sd` daemon and the worker that reads its browse channel.
struct Engine {
    daemon: ServiceDaemon,
    registered: Option<String>,
    worker: Option<JoinHandle<()>>,
    /// Set before the last daemon stops: its worker then closes the event queue instead of reporting its peers lost.
    last: Arc<AtomicBool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn fingerprint_of(event: &DiscoveryEvent) -> [u8; FINGERPRINT_BYTES] {
    match event {
        DiscoveryEvent::Found(p) => p.txt.fingerprint,
        DiscoveryEvent::Lost { fingerprint } => *fingerprint,
    }
}

#[derive(Default)]
struct Pending {
    /// Fingerprints in the order their first waiting event came.
    order: VecDeque<[u8; FINGERPRINT_BYTES]>,
    /// The newest event of each: "the newest wins", so a peer that flaps while nobody reads costs one place.
    latest: HashMap<[u8; FINGERPRINT_BYTES], DiscoveryEvent>,
    /// The worker ended: once `order` is empty the reader gets `None` at once.
    closed: bool,
    /// Events of new peers refused for lack of room.
    refused: u64,
}

/// The events between the worker thread and the reader: bounded by [`MAX_PEERS`] and coalesced per peer, and the worker
/// never waits for the reader (it would then never see the daemon shut down).
#[derive(Default)]
struct Events {
    pending: Mutex<Pending>,
    ready: Condvar,
}

impl Events {
    fn push(&self, event: DiscoveryEvent) {
        let fp = fingerprint_of(&event);
        let mut p = lock(&self.pending);
        if let Some(slot) = p.latest.get_mut(&fp) {
            *slot = event;
        } else if p.latest.len() >= MAX_PEERS {
            p.refused += 1;
            return;
        } else {
            p.order.push_back(fp);
            p.latest.insert(fp, event);
        }
        drop(p);
        self.ready.notify_one();
    }

    fn close(&self) {
        lock(&self.pending).closed = true;
        self.ready.notify_all();
    }

    fn next(&self, timeout: Duration) -> Option<DiscoveryEvent> {
        let end = Instant::now() + timeout;
        let mut p = lock(&self.pending);
        loop {
            if let Some(fp) = p.order.pop_front() {
                return p.latest.remove(&fp);
            }
            if p.closed {
                return None;
            }
            let left = end.checked_duration_since(Instant::now())?;
            p = self.ready.wait_timeout(p, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }
}

pub struct Discovery {
    nets: Arc<dyn NetInterfaces>,
    config: Config,
    state: Mutex<State>,
    events: Arc<Events>,
    /// Bumped by every `refresh`, so the worker drops the interface list it kept.
    generation: Arc<AtomicU64>,
    /// How many daemons were started: one at the start, one more per change of the approved addresses.
    daemons: AtomicU64,
}

fn instance_name(fp: &[u8; FINGERPRINT_BYTES]) -> String {
    fingerprint_hex(fp)
}

fn host_name(fp: &[u8; FINGERPRINT_BYTES]) -> String {
    format!("swapdesk-{}.local.", &fingerprint_hex(fp)[..12])
}

/// The properties of an own record: the 1.0 keys, then `minor` when it is 1 or more.
fn own_properties(own: &Txt, minor: u16) -> Result<Vec<(String, String)>, DiscoveryError> {
    let mut props = own.encode().map_err(DiscoveryError::Txt)?;
    props.extend(minor_entry(minor));
    Ok(props)
}

impl Discovery {
    /// Starts the daemon on the approved interfaces, announces `own` and starts browsing.
    pub fn start(nets: Arc<dyn NetInterfaces>, own: Txt, config: Config) -> Result<Self, DiscoveryError> {
        own.encode().map_err(DiscoveryError::Txt)?;
        let d = Self {
            nets,
            config,
            state: Mutex::new(State { own, applied: Vec::new(), engine: None }),
            events: Arc::new(Events::default()),
            generation: Arc::new(AtomicU64::new(0)),
            daemons: AtomicU64::new(0),
        };
        let list = d.nets.list().map_err(DiscoveryError::Platform)?;
        {
            let mut st = lock(&d.state);
            d.apply(&mut st, approved_addrs(&list))?;
        }
        Ok(d)
    }

    /// Re-reads the interfaces and, if the approved addresses changed, replaces the daemon by one bound to the new
    /// ones, which announces again. Returns whether anything changed. Call it when the network changes (and never on
    /// a timer).
    pub fn refresh(&self) -> Result<bool, DiscoveryError> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let list = self.nets.list().map_err(DiscoveryError::Platform)?;
        let approved = approved_addrs(&list);
        let mut st = lock(&self.state);
        if approved == st.applied && st.engine.is_some() {
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
        let (own, approved) = (st.own.clone(), st.applied.clone());
        match st.engine.as_mut() {
            Some(engine) => engine.register(&own, &approved, self.config.service_port, self.config.minor),
            None => Ok(()),
        }
    }

    /// The next event, or `None` after `timeout`.
    pub fn next_event(&self, timeout: Duration) -> Option<DiscoveryEvent> {
        self.events.next(timeout)
    }

    /// The approved addresses the daemon is using now.
    pub fn applied(&self) -> Vec<ApprovedAddr> {
        lock(&self.state).applied.clone()
    }

    /// How many `mdns-sd` daemons this discovery started: one at the start, one more per change of the approved
    /// addresses (diagnostics and tests).
    pub fn daemons_started(&self) -> u64 {
        self.daemons.load(Ordering::SeqCst)
    }

    /// Replaces the daemon by one bound to `approved` (with none approved it announces and browses nothing).
    fn apply(&self, st: &mut State, approved: Vec<ApprovedAddr>) -> Result<(), DiscoveryError> {
        if let Some(old) = st.engine.take() {
            // No goodbye: the record keeps its name, and the new daemon's announcement replaces the old addresses in the
            // peers' caches (cache-flush), so they see an update, not a departure.
            old.stop(false, Duration::from_secs(1));
        }
        let engine =
            Engine::start(&self.config, &st.own, &approved, Arc::clone(&self.nets), &self.events, &self.generation)?;
        self.daemons.fetch_add(1, Ordering::SeqCst);
        st.engine = Some(engine);
        st.applied = approved;
        Ok(())
    }

    /// Sends the goodbye packets and stops the daemon.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let engine = lock(&self.state).engine.take();
        match engine {
            Some(e) => e.stop(true, Duration::from_secs(2)),
            None => self.events.close(),
        }
    }
}

fn indexes_of(approved: &[ApprovedAddr]) -> Vec<u32> {
    let mut indexes: Vec<u32> = approved.iter().map(|a| a.if_index).collect();
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

impl Engine {
    fn start(
        config: &Config,
        own: &Txt,
        approved: &[ApprovedAddr],
        nets: Arc<dyn NetInterfaces>,
        events: &Arc<Events>,
        generation: &Arc<AtomicU64>,
    ) -> Result<Self, DiscoveryError> {
        let daemon = ServiceDaemon::new_with_port(config.mdns_port).map_err(mdns)?;
        let browse = match Self::configure(&daemon, approved).and_then(|()| daemon.browse(SERVICE_TYPE).map_err(mdns)) {
            Ok(b) => b,
            Err(e) => {
                let _ = daemon.shutdown();
                return Err(e);
            }
        };
        let last = Arc::new(AtomicBool::new(false));
        let own_fp = own.fingerprint;
        let spawned = {
            let (events, generation, last) = (Arc::clone(events), Arc::clone(generation), Arc::clone(&last));
            std::thread::Builder::new()
                .name("swapdesk-discovery".into())
                .spawn(move || run_worker(browse, nets, own_fp, &events, &generation, &last))
        };
        let worker = match spawned {
            Ok(w) => w,
            Err(e) => {
                let _ = daemon.shutdown();
                return Err(DiscoveryError::Mdns(format!("thread: {e}")));
            }
        };
        let mut engine = Self { daemon, registered: None, worker: Some(worker), last };
        if let Err(e) = engine.register(own, approved, config.service_port, config.minor) {
            engine.stop(false, Duration::from_secs(1));
            return Err(e);
        }
        Ok(engine)
    }

    /// Nothing is used until the guard approves it, then only the approved interfaces (IPv4, by index).
    fn configure(daemon: &ServiceDaemon, approved: &[ApprovedAddr]) -> Result<(), DiscoveryError> {
        daemon.set_ip_check_interval(0).map_err(mdns)?;
        daemon.disable_interface(MdnsIfKind::All).map_err(mdns)?;
        let indexes = indexes_of(approved);
        if !indexes.is_empty() {
            daemon
                .enable_interface(indexes.iter().map(|i| MdnsIfKind::IndexV4(*i)).collect::<Vec<_>>())
                .map_err(mdns)?;
        }
        Ok(())
    }

    fn register(&mut self, own: &Txt, approved: &[ApprovedAddr], port: u16, minor: u16) -> Result<(), DiscoveryError> {
        if approved.is_empty() {
            if let Some(full) = self.registered.take() {
                self.daemon.unregister(&full).map_err(mdns)?;
            }
            return Ok(());
        }
        let props = own_properties(own, minor)?;
        let addrs: Vec<IpAddr> = approved.iter().map(|a| IpAddr::V4(a.addr)).collect();
        let mut info = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name(&own.fingerprint),
            &host_name(&own.fingerprint),
            addrs.as_slice(),
            port,
            props.as_slice(),
        )
        .map_err(mdns)?;
        info.set_interfaces(indexes_of(approved).into_iter().map(MdnsIfKind::IndexV4).collect());
        let full = info.get_fullname().to_string();
        self.daemon.register(info).map_err(mdns)?;
        self.registered = Some(full);
        Ok(())
    }

    /// Stops the daemon and waits for its worker. `last`: the discovery is ending, so the record says goodbye and the
    /// event queue closes; otherwise a new daemon takes over and the worker reports the peers it knew as lost.
    fn stop(mut self, last: bool, wait: Duration) {
        self.last.store(last, Ordering::SeqCst);
        if last
            && let Some(full) = self.registered.take()
            && let Ok(done) = self.daemon.unregister(&full)
        {
            let _ = done.recv_timeout(wait);
        }
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(wait);
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

/// The interface list the worker uses for resolved services: read at most once per [`LIST_TTL`] and again after a
/// `refresh`, so a flood of announcements does not become a flood of adapter enumerations.
struct ListCache {
    nets: Arc<dyn NetInterfaces>,
    generation: Arc<AtomicU64>,
    kept: Option<(Instant, u64, Vec<NetInterface>)>,
}

impl ListCache {
    fn get(&mut self) -> Option<&[NetInterface]> {
        let generation = self.generation.load(Ordering::SeqCst);
        let fresh = self.kept.as_ref().is_some_and(|(at, g, _)| *g == generation && at.elapsed() < LIST_TTL);
        if !fresh {
            self.kept = self.nets.list().ok().map(|l| (Instant::now(), generation, l));
        }
        self.kept.as_ref().map(|(_, _, l)| l.as_slice())
    }
}

fn run_worker(
    browse: mdns_sd::Receiver<ServiceEvent>,
    nets: Arc<dyn NetInterfaces>,
    own_fp: [u8; FINGERPRINT_BYTES],
    events: &Events,
    generation: &Arc<AtomicU64>,
    last: &AtomicBool,
) {
    // fullname -> fingerprint of the peers reported as found, so a removal can name them.
    let mut known: HashMap<String, [u8; FINGERPRINT_BYTES]> = HashMap::new();
    let mut lists = ListCache { nets, generation: Arc::clone(generation), kept: None };
    // Blocks until an event or until the daemon is shut down (the channel closes): no timer, no polling.
    while let Ok(event) = browse.recv() {
        match event {
            ServiceEvent::ServiceResolved(rs) => {
                let entries: Vec<(&str, &[u8])> =
                    rs.txt_properties.iter().map(|p| (p.key(), p.val().unwrap_or(&[]))).collect();
                let Ok(txt) = Txt::decode(&entries) else {
                    drop_known(&mut known, &rs.fullname, events);
                    continue;
                };
                let minor = read_minor(&entries);
                if txt.fingerprint == own_fp {
                    continue;
                }
                let Some(list) = lists.get() else { continue };
                let mut links: Vec<PeerLink> = Vec::new();
                for sip in &rs.addresses {
                    let mdns_sd::ScopedIp::V4(v4) = sip else { continue };
                    for id in v4.interface_ids() {
                        if let Ok(link) = check_discovered(list, id.index, IpAddr::V4(*v4.addr()))
                            && !links.contains(&link)
                        {
                            links.push(link);
                        }
                    }
                }
                links.sort_by_key(|l| (l.if_index, u32::from(l.remote)));
                if links.is_empty() || rs.port == 0 {
                    drop_known(&mut known, &rs.fullname, events);
                    continue;
                }
                // A new name past the bound is not remembered and not reported (a LAN host can make up names).
                if known.len() >= MAX_PEERS && !known.contains_key(&rs.fullname) {
                    continue;
                }
                known.insert(rs.fullname.clone(), txt.fingerprint);
                events.push(DiscoveryEvent::Found(Peer { txt, port: rs.port, minor, links }));
            }
            ServiceEvent::ServiceRemoved(_, fullname) => drop_known(&mut known, &fullname, events),
            _ => {}
        }
    }
    if last.load(Ordering::SeqCst) {
        events.close();
    } else {
        // This daemon was replaced: what it knew is unknown until the new one finds it again.
        for (_, fingerprint) in known.drain() {
            events.push(DiscoveryEvent::Lost { fingerprint });
        }
    }
}

fn drop_known(known: &mut HashMap<String, [u8; FINGERPRINT_BYTES]>, fullname: &str, events: &Events) {
    if let Some(fingerprint) = known.remove(fullname) {
        events.push(DiscoveryEvent::Lost { fingerprint });
    }
}

/// The addresses of the approved list as plain IPs (handy for tests and logs).
pub fn addrs_of(list: &[ApprovedAddr]) -> Vec<Ipv4Addr> {
    list.iter().map(|a| a.addr).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_records_carry_minor_from_1_and_not_before() {
        let own = Txt { name: "n".into(), os: swapdesk_proto::Os::Windows, fingerprint: [1; 16], pairing: false };
        let keys = |m| own_properties(&own, m).unwrap().into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(keys(0), ["name", "os", "fp"]);
        assert_eq!(keys(1), ["name", "os", "fp", "minor"]);
    }

    fn peer(fp: u8, port: u16) -> DiscoveryEvent {
        DiscoveryEvent::Found(Peer {
            txt: Txt { name: format!("p{fp}"), os: swapdesk_proto::Os::Windows, fingerprint: [fp; 16], pairing: false },
            port,
            minor: 0,
            links: Vec::new(),
        })
    }

    #[test]
    fn the_event_queue_has_a_bound_and_keeps_the_newest_event_of_each_peer() {
        let q = Events::default();
        // One peer that flaps while nobody reads costs one place, and the newest event is the one that is read.
        for port in 1..=1000u16 {
            q.push(peer(7, port));
        }
        q.push(DiscoveryEvent::Lost { fingerprint: [7; 16] });
        q.push(peer(7, 4242));
        assert_eq!(lock(&q.pending).latest.len(), 1);
        assert!(matches!(q.next(Duration::ZERO), Some(DiscoveryEvent::Found(p)) if p.port == 4242));
        assert!(q.next(Duration::ZERO).is_none());
        // A flood of distinct peers stops at the bound; the ones already waiting are still read, in order.
        for i in 0..(MAX_PEERS as u32 + 100) {
            let mut fp = [0u8; 16];
            fp[..4].copy_from_slice(&i.to_be_bytes());
            q.push(DiscoveryEvent::Lost { fingerprint: fp });
        }
        assert_eq!(lock(&q.pending).latest.len(), MAX_PEERS);
        assert_eq!(lock(&q.pending).refused, 100);
        let mut read = 0;
        while let Some(DiscoveryEvent::Lost { fingerprint }) = q.next(Duration::ZERO) {
            assert_eq!(u32::from_be_bytes(fingerprint[..4].try_into().unwrap()), read, "first come, first read");
            read += 1;
        }
        assert_eq!(read as usize, MAX_PEERS);
        // Room again after the reader drained it.
        q.push(peer(1, 1));
        assert!(q.next(Duration::ZERO).is_some());
    }

    #[test]
    fn a_reader_wakes_when_an_event_arrives_and_returns_at_once_when_the_worker_ended() {
        let q = Arc::new(Events::default());
        let reader = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                let began = Instant::now();
                let got = q.next(Duration::from_secs(30));
                (got, began.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        q.push(peer(3, 9));
        let (got, waited) = reader.join().unwrap();
        assert!(got.is_some());
        assert!(waited < Duration::from_secs(5), "woke on the event, not at the timeout ({waited:?})");
        let began = Instant::now();
        q.close();
        assert!(q.next(Duration::from_secs(30)).is_none());
        assert!(began.elapsed() < Duration::from_secs(1), "a closed queue does not wait");
    }

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
