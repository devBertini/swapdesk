//! Who is on the network right now, as the device list needs it: a small trait over the DNS-SD discovery of
//! `swapdesk-lan`, so tests script the answer.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_lan::discovery::{Config, Discovery, DiscoveryError, DiscoveryEvent, Peer};
use swapdesk_lan::txt::Txt;
use swapdesk_platform::NetInterfaces;
use swapdesk_proto::Os;
use swapdesk_trust::DeviceId;

/// One device that announced itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresencePeer {
    pub device_id: DeviceId,
    pub name: String,
    pub os: Os,
    /// The peer's pairing mode is open (`pair=1` in its record).
    pub pairing_open: bool,
    /// Addresses that passed the LAN guard, best first.
    pub addrs: Vec<Ipv4Addr>,
    /// The session endpoint port of the peer (its SRV record).
    pub session_port: u16,
    /// The peer's pairing endpoint port. The record does not carry it: discovery assumes the next port of the
    /// session endpoint (PROTOCOL.md 1: 47820 and 47821).
    pub pair_port: u16,
    /// The protocol minor of its record (`minor` TXT key, GRANTS-SYNC.md 4.5); 0 when absent: a 1.0 peer.
    pub proto_minor: u16,
}

/// Presence of the other devices and the announcement of this one.
pub trait Presence: Send + Sync {
    /// The devices seen now.
    fn peers(&self) -> Vec<PresencePeer>;
    /// Adds or removes `pair=1` in this machine's announcement.
    fn set_pairing(&self, on: bool);
    /// The network changed: re-read the interfaces.
    fn refresh(&self);
    /// `f` runs (on any thread) whenever the set of peers changes.
    fn set_listener(&self, f: Arc<dyn Fn() + Send + Sync>);
    /// Announces this account's record, or withdraws it while the others are still browsed (ACCOUNTS.md 6: an
    /// account that receives no connections announces nothing).
    fn set_announced(&self, _on: bool) {}
}

/// No discovery: nobody is ever seen.
pub struct NoPresence;

impl Presence for NoPresence {
    fn peers(&self) -> Vec<PresencePeer> {
        Vec::new()
    }
    fn set_pairing(&self, _on: bool) {}
    fn refresh(&self) {}
    fn set_listener(&self, _f: Arc<dyn Fn() + Send + Sync>) {}
}

type Listener = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`Presence`] over `swapdesk_lan::Discovery` (mDNS on the approved interfaces only).
pub struct DnsSdPresence {
    discovery: Arc<Discovery>,
    peers: Arc<Mutex<HashMap<DeviceId, PresencePeer>>>,
    listener: Listener,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DnsSdPresence {
    /// Announces `own` and starts browsing.
    pub fn start(nets: Arc<dyn NetInterfaces>, own: Txt, config: Config) -> Result<Self, DiscoveryError> {
        let discovery = Arc::new(Discovery::start(nets, own, config)?);
        let peers: Arc<Mutex<HashMap<DeviceId, PresencePeer>>> = Arc::default();
        let listener: Listener = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (discovery, peers, listener, stop) =
                (Arc::clone(&discovery), Arc::clone(&peers), Arc::clone(&listener), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("swapdesk-presence".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let Some(event) = discovery.next_event(Duration::from_millis(500)) else { continue };
                        let changed = {
                            let mut map = lock(&peers);
                            match event {
                                DiscoveryEvent::Found(p) => {
                                    let peer = presence_peer(&p);
                                    found(&mut map, peer)
                                }
                                DiscoveryEvent::Lost { fingerprint } => map.remove(&fingerprint).is_some(),
                            }
                        };
                        if changed && let Some(f) = lock(&listener).clone() {
                            f();
                        }
                    }
                })
                .map_err(|e| DiscoveryError::Mdns(format!("thread: {e}")))?
        };
        Ok(Self { discovery, peers, listener, stop, worker: Some(worker) })
    }
}

/// What the device list keeps of a peer discovery found.
fn presence_peer(p: &Peer) -> PresencePeer {
    PresencePeer {
        device_id: p.txt.fingerprint,
        name: p.txt.name.clone(),
        os: p.txt.os,
        pairing_open: p.txt.pairing,
        addrs: p.links.iter().map(|l| l.remote).collect(),
        session_port: p.port,
        pair_port: p.port.wrapping_add(1),
        proto_minor: p.minor,
    }
}

/// A peer announced itself: it replaces its own old record and any other device's record at one of its addresses and
/// its port. On a LAN an IPv4 address is one machine at a time, so an older record there is a machine that is gone
/// (Swapdesk reinstalled with a new key, or another computer that took the address) and must not stand for that
/// address: "Adicionar por endereço" would pick it and pairing would fail against the real key. `true` when the set
/// changed.
pub(crate) fn found(map: &mut HashMap<DeviceId, PresencePeer>, peer: PresencePeer) -> bool {
    let before = map.len();
    map.retain(|id, q| {
        id == &peer.device_id || q.session_port != peer.session_port || !q.addrs.iter().any(|a| peer.addrs.contains(a))
    });
    let replaced = map.len() != before;
    let new = map.insert(peer.device_id, peer.clone()).as_ref() != Some(&peer);
    replaced || new
}

impl Presence for DnsSdPresence {
    fn peers(&self) -> Vec<PresencePeer> {
        let mut v: Vec<PresencePeer> = lock(&self.peers).values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.device_id.cmp(&b.device_id)));
        v
    }

    fn set_pairing(&self, on: bool) {
        // A failed re-announcement leaves the old record: the next network change retries.
        let _ = self.discovery.set_pairing(on);
    }

    fn refresh(&self) {
        let _ = self.discovery.refresh();
    }

    fn set_listener(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.listener) = Some(f);
    }

    fn set_announced(&self, on: bool) {
        // A failed re-announcement leaves the record as it was: the next network change retries.
        let _ = self.discovery.set_announced(on);
    }
}

impl Drop for DnsSdPresence {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// A scripted [`Presence`] for tests: `set` replaces the peers and calls the listener.
#[derive(Default)]
pub struct FakePresence {
    peers: Mutex<Vec<PresencePeer>>,
    listener: Listener,
    pairing: AtomicBool,
    /// `Some(on)` once the hub said whether to announce.
    announced: Mutex<Option<bool>>,
}

impl FakePresence {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn set(&self, peers: Vec<PresencePeer>) {
        *lock(&self.peers) = peers;
        if let Some(f) = lock(&self.listener).clone() {
            f();
        }
    }

    /// Whether this machine's announcement carries `pair=1`.
    pub fn pairing_announced(&self) -> bool {
        self.pairing.load(Ordering::SeqCst)
    }

    /// What the hub last said about announcing the record (`None`: it never said).
    pub fn announced(&self) -> Option<bool> {
        *lock(&self.announced)
    }
}

impl Presence for FakePresence {
    fn peers(&self) -> Vec<PresencePeer> {
        lock(&self.peers).clone()
    }
    fn set_pairing(&self, on: bool) {
        self.pairing.store(on, Ordering::SeqCst);
    }
    fn refresh(&self) {}
    fn set_listener(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.listener) = Some(f);
    }
    fn set_announced(&self, on: bool) {
        *lock(&self.announced) = Some(on);
    }
}

/// The presence of one account on a machine with several: the records of this machine's other accounts are not
/// peers (ACCOUNTS.md 6, "this machine's other accounts are not listed").
pub struct FilteredPresence {
    inner: Arc<dyn Presence>,
    own: Arc<Mutex<Vec<DeviceId>>>,
}

impl FilteredPresence {
    /// `own`: the device IDs of this machine's accounts, kept up to date by the machine as accounts appear.
    pub fn new(inner: Arc<dyn Presence>, own: Arc<Mutex<Vec<DeviceId>>>) -> Self {
        Self { inner, own }
    }
}

impl Presence for FilteredPresence {
    fn peers(&self) -> Vec<PresencePeer> {
        let own = lock(&self.own).clone();
        self.inner.peers().into_iter().filter(|p| !own.contains(&p.device_id)).collect()
    }
    fn set_pairing(&self, on: bool) {
        self.inner.set_pairing(on);
    }
    fn refresh(&self) {
        self.inner.refresh();
    }
    fn set_listener(&self, f: Arc<dyn Fn() + Send + Sync>) {
        self.inner.set_listener(f);
    }
    fn set_announced(&self, on: bool) {
        self.inner.set_announced(on);
    }
}

#[cfg(test)]
mod found_tests {
    use super::*;

    fn peer(id: u8, ip: [u8; 4], port: u16) -> PresencePeer {
        PresencePeer {
            device_id: [id; 16],
            name: format!("p{id}"),
            os: swapdesk_proto::Os::MacOs,
            pairing_open: false,
            addrs: vec![Ipv4Addr::from(ip)],
            session_port: port,
            pair_port: port + 1,
            proto_minor: 0,
        }
    }

    #[test]
    fn a_new_key_at_an_address_replaces_the_old_record_there() {
        let mut map = HashMap::new();
        assert!(found(&mut map, peer(1, [192, 168, 1, 3], 47820)));
        assert!(found(&mut map, peer(9, [192, 168, 1, 7], 47820)));
        assert!(!found(&mut map, peer(1, [192, 168, 1, 3], 47820)), "the same record again changes nothing");
        // Swapdesk reinstalled on 192.168.1.3: a new key there.
        assert!(found(&mut map, peer(2, [192, 168, 1, 3], 47820)));
        let mut ids: Vec<u8> = map.keys().map(|d| d[0]).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![2, 9], "the old record at that address is gone, the other machine stays");
        // A second instance on another port of the same machine is another endpoint: both stay.
        assert!(found(&mut map, peer(3, [192, 168, 1, 3], 50000)));
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn a_discovered_peers_minor_reaches_presence_peer_proto_minor() {
        let found_peer = |minor| Peer {
            txt: Txt { name: "m".into(), os: swapdesk_proto::Os::MacOs, fingerprint: [4; 16], pairing: false },
            port: 47820,
            minor,
            links: Vec::new(),
        };
        assert_eq!(presence_peer(&found_peer(0)).proto_minor, 0);
        assert_eq!(presence_peer(&found_peer(1)).proto_minor, 1);
        // A record that gains `minor` is a change of the set.
        let mut map = HashMap::new();
        assert!(found(&mut map, presence_peer(&found_peer(0))));
        assert!(found(&mut map, presence_peer(&found_peer(1))));
        assert!(!found(&mut map, presence_peer(&found_peer(1))));
    }

    #[test]
    fn a_scripted_peers_minor_is_what_the_fake_presence_reports() {
        let fake = FakePresence::new();
        fake.set(vec![
            PresencePeer { proto_minor: 1, ..peer(1, [192, 168, 1, 3], 47820) },
            peer(2, [192, 168, 1, 4], 47820),
        ]);
        let minors: Vec<u16> = fake.peers().iter().map(|p| p.proto_minor).collect();
        assert_eq!(minors, [1, 0]);
    }
}
