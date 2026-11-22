//! The session endpoint's sockets, one per approved interface address (PROTOCOL.md 1), following the network.
//!
//! [`ServiceCore::start`](super::ServiceCore::start) binds them once; [`Listeners::follow`] runs on every network change
//! ([`super::ServiceCore::network_changed`]). An address that is no longer approved stops taking new connections and
//! its accept loop ends; the sessions already on it are left alone (they end by themselves if the address is gone). A
//! new approved address gets an endpoint of its own on the session port. An address that is still approved is not
//! touched, so neither are its sessions. Without this a machine whose DHCP address changed went on listening on the
//! old address only, and nobody could reach it until the program restarted (seen on a Mac, 09/11). A closed listener's
//! endpoint is kept while its sessions run (they hold its socket): when the address is approved again that endpoint
//! takes new connections again, since a new socket on the address cannot be bound until they end.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};

use quinn::Endpoint;
use swapdesk_lan::guard::approved_addrs;
use swapdesk_lan::socket::bind_quic_socket;
use swapdesk_platform::{Level, NetInterface};
use swapdesk_trust::{Identity, TrustHandle, open_session_endpoint_filtered};
use tokio::task::AbortHandle;

use super::{Gate, Shared, accept_loop, lock};

struct Listener {
    local: SocketAddr,
    endpoint: Endpoint,
    /// A development address (`listen_extra`): it is not an interface address and never follows the network.
    extra: bool,
    accept: AbortHandle,
}

/// The endpoint of a listener that closed while sessions still ran on it (see [`Listeners::follow`]).
struct Parked {
    local: SocketAddr,
    endpoint: Endpoint,
    /// Tells this entry from a later one of the same address.
    id: u64,
    /// The task that lets the endpoint go when its last session ends.
    release: AbortHandle,
}

pub(crate) struct Listeners {
    port: u16,
    identity: Arc<Identity>,
    trust: Arc<TrustHandle>,
    gate: Arc<Gate>,
    shared: Arc<Shared>,
    list: Mutex<Vec<Listener>>,
    /// Endpoints of closed listeners whose sessions still run, kept until they end.
    parked: Arc<Mutex<Vec<Parked>>>,
    parked_ids: std::sync::atomic::AtomicU64,
    /// The approved addresses are listened on (an account that receives no connections listens nowhere).
    receiving: std::sync::atomic::AtomicBool,
    /// The port is one exclusive socket on the wildcard address (ACCOUNTS.md 5.5): never closed by a network change,
    /// and "not receiving" stops it taking connections instead.
    wildcard: std::sync::atomic::AtomicBool,
    /// A second handle of the wildcard socket (ACCOUNTS.md 6, review of S3/S4 L7): while the account receives
    /// nothing the endpoint is moved to a socket nobody reaches, so the slot answers no datagram (quinn would answer an
    /// Initial with a stateless reset), and this handle keeps the port held.
    held: Mutex<Option<std::net::UdpSocket>>,
}

/// What a network change does to the listeners: which listening addresses close, which approved addresses open.
/// Development addresses never close; an approved address already listened on (as a development address too) is not
/// opened twice.
pub(crate) fn plan(listening: &[(SocketAddr, bool)], approved: &[Ipv4Addr]) -> (Vec<SocketAddr>, Vec<Ipv4Addr>) {
    let close = listening
        .iter()
        .filter(|(a, extra)| !extra && !matches!(a.ip(), IpAddr::V4(ip) if approved.contains(&ip)))
        .map(|(a, _)| *a)
        .collect();
    let mut open: Vec<Ipv4Addr> = Vec::new();
    for ip in approved {
        if !open.contains(ip) && !listening.iter().any(|(a, _)| a.ip() == IpAddr::V4(*ip)) {
            open.push(*ip);
        }
    }
    (close, open)
}

/// Of the addresses `plan` opens, those a kept endpoint (`parked`, closed while its sessions still ran) takes again,
/// and those that get a new socket.
pub(crate) fn split_open(open: &[Ipv4Addr], parked: &[SocketAddr], port: u16) -> (Vec<SocketAddr>, Vec<Ipv4Addr>) {
    let mut revive = Vec::new();
    let mut bind = Vec::new();
    for ip in open {
        let at = SocketAddr::V4(SocketAddrV4::new(*ip, port));
        if parked.contains(&at) { revive.push(at) } else { bind.push(*ip) }
    }
    (revive, bind)
}

impl Listeners {
    pub fn new(
        port: u16,
        identity: Arc<Identity>,
        trust: Arc<TrustHandle>,
        gate: Arc<Gate>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            port,
            identity,
            trust,
            gate,
            shared,
            list: Mutex::new(Vec::new()),
            parked: Arc::new(Mutex::new(Vec::new())),
            parked_ids: std::sync::atomic::AtomicU64::new(0),
            receiving: std::sync::atomic::AtomicBool::new(true),
            wildcard: std::sync::atomic::AtomicBool::new(false),
            held: Mutex::new(None),
        }
    }

    /// Starts accepting on `endpoint`.
    pub fn adopt(&self, endpoint: Endpoint, extra: bool) -> std::io::Result<SocketAddr> {
        let local = endpoint.local_addr()?;
        let accept = self.shared.rt.spawn(accept_loop(Arc::clone(&self.gate), endpoint.clone())).abort_handle();
        lock(&self.list).push(Listener { local, endpoint, extra, accept });
        Ok(local)
    }

    /// Whether the approved addresses are listened on (an account without pairings does not). On the wildcard
    /// socket it decides whether new connections are taken; the socket itself stays bound.
    pub fn set_receiving(&self, on: bool) {
        self.receiving.store(on, std::sync::atomic::Ordering::SeqCst);
        if !self.wildcard.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let config = if on {
            match swapdesk_trust::tls::session_server_config(&self.identity, self.trust.clone()) {
                Ok(c) => Some(c),
                Err(e) => {
                    self.shared.log(Level::Warn, format_args!("the session endpoint cannot take connections: {e}"));
                    None
                }
            }
        } else {
            None
        };
        let held = lock(&self.held);
        // `rebind` registers the socket with the core's runtime: callers are often outside it (the hub's threads).
        let _runtime = self.shared.rt.enter();
        for l in lock(&self.list).iter().filter(|l| l.local.ip().is_unspecified()) {
            l.endpoint.set_server_config(config.clone());
            // Receiving: the endpoint reads the slot's port again; not: it reads a socket nobody reaches. quinn keeps
            // reading the socket a rebind replaced (until traffic comes on the new one), so it moves twice: the slot's
            // socket then leaves quinn, and the held handle alone keeps the port.
            if held.is_none() {
                continue;
            }
            let moves = if on { 1 } else { 2 };
            for _ in 0..moves {
                let socket = if on {
                    held.as_ref().map_or_else(|| Err(std::io::Error::other("no held socket")), |h| h.try_clone())
                } else {
                    std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                };
                if let Err(e) = socket.and_then(|s| l.endpoint.rebind(s)) {
                    self.shared.log(Level::Warn, format_args!("the session endpoint could not move: {e}"));
                }
            }
        }
    }

    /// The second handle of the wildcard socket, held for the life of the core (see `held`).
    pub fn set_held(&self, socket: std::net::UdpSocket) {
        *lock(&self.held) = Some(socket);
    }

    pub fn set_wildcard(&self, on: bool) {
        self.wildcard.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn receiving(&self) -> bool {
        self.receiving.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The addresses the session endpoint listens on now.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        lock(&self.list).iter().map(|l| l.local).collect()
    }

    /// Follows the approved addresses of `list`. Returns whether anything changed.
    pub fn follow(&self, list: &[NetInterface]) -> bool {
        // The wildcard socket covers every address; the guard judges each arrival address.
        if self.wildcard.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        // An account that receives no connections listens on no approved address (ACCOUNTS.md 6).
        let mut approved: Vec<Ipv4Addr> = if self.receiving.load(std::sync::atomic::Ordering::SeqCst) {
            approved_addrs(list).iter().map(|a| a.addr).collect()
        } else {
            Vec::new()
        };
        approved.dedup();
        let mut listeners = lock(&self.list);
        let listening: Vec<(SocketAddr, bool)> = listeners.iter().map(|l| (l.local, l.extra)).collect();
        let (close, open) = plan(&listening, &approved);
        for addr in &close {
            if let Some(i) = listeners.iter().position(|l| l.local == *addr) {
                let l = listeners.remove(i);
                // No new connections; the live ones keep the endpoint running until they end.
                l.endpoint.set_server_config(None);
                l.accept.abort();
                self.park(l.local, l.endpoint);
                self.shared
                    .log(Level::Info, format_args!("session endpoint off {addr}: the address is not approved now"));
            }
        }
        let mut opened = false;
        let parked: Vec<SocketAddr> = lock(&self.parked).iter().map(|p| p.local).collect();
        let (revive, mut bind) = split_open(&open, &parked, self.port);
        for at in revive {
            // Its last session may have ended since: the socket is free again, and the address gets a new one.
            let Some(endpoint) = self.unpark(at) else {
                if let IpAddr::V4(ip) = at.ip() {
                    bind.push(ip);
                }
                continue;
            };
            match swapdesk_trust::tls::session_server_config(&self.identity, self.trust.clone()) {
                Ok(config) => {
                    endpoint.set_server_config(Some(config));
                    let accept =
                        self.shared.rt.spawn(accept_loop(Arc::clone(&self.gate), endpoint.clone())).abort_handle();
                    listeners.push(Listener { local: at, endpoint, extra: false, accept });
                    self.shared.log(Level::Info, format_args!("session endpoint on {at} again (its sessions kept it)"));
                    opened = true;
                }
                Err(e) => {
                    self.shared.log(Level::Warn, format_args!("cannot listen on {at} again: {e}"));
                    self.park(at, endpoint);
                }
            }
        }
        for ip in &bind {
            let at = SocketAddrV4::new(*ip, self.port);
            let endpoint = {
                let _enter = self.shared.rt.enter();
                bind_quic_socket(at).map_err(|e| e.to_string()).and_then(|s| {
                    open_session_endpoint_filtered(
                        s,
                        &self.identity,
                        self.trust.clone(),
                        Arc::clone(&self.gate.datagrams).into_filter(),
                    )
                    .map_err(|e| e.to_string())
                })
            };
            match endpoint.and_then(|e| e.local_addr().map(|a| (e, a)).map_err(|e| e.to_string())) {
                Ok((endpoint, local)) => {
                    let accept =
                        self.shared.rt.spawn(accept_loop(Arc::clone(&self.gate), endpoint.clone())).abort_handle();
                    listeners.push(Listener { local, endpoint, extra: false, accept });
                    self.shared.log(Level::Info, format_args!("session endpoint on {local}"));
                    opened = true;
                }
                Err(e) => self.shared.log(Level::Warn, format_args!("cannot listen on {at}: {e}")),
            }
        }
        !close.is_empty() || opened
    }

    /// Keeps the endpoint of a closed listener while sessions still run on it. Their endpoint holds the address's
    /// socket, so a new bind of the address fails until they end ("cannot listen on ..."): when the address is approved
    /// again, this endpoint takes it again instead (review of 22/11/2026, I1). It is let go when its last session ends,
    /// as before, so an address that is not listened on answers nothing once its sessions are over.
    fn park(&self, local: SocketAddr, endpoint: Endpoint) {
        let id = self.parked_ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut parked = lock(&self.parked);
        // The lock is held while the task starts, so the task's removal comes after the entry is in.
        let release = {
            let (slot, waiting) = (Arc::clone(&self.parked), endpoint.clone());
            self.shared
                .rt
                .spawn(async move {
                    waiting.wait_idle().await;
                    lock(&slot).retain(|p| p.id != id);
                })
                .abort_handle()
        };
        parked.retain(|p| {
            let other = p.local != local;
            if !other {
                p.release.abort();
            }
            other
        });
        parked.push(Parked { local, endpoint, id, release });
    }

    /// The kept endpoint of `at`, when its sessions still run.
    fn unpark(&self, at: SocketAddr) -> Option<Endpoint> {
        let mut parked = lock(&self.parked);
        let i = parked.iter().position(|p| p.local == at)?;
        let p = parked.remove(i);
        p.release.abort();
        Some(p.endpoint)
    }

    /// Closes every endpoint (the service is stopping).
    pub fn close_all(&self) {
        for l in lock(&self.list).drain(..) {
            l.accept.abort();
            l.endpoint.close(quinn::VarInt::from_u32(0), b"service stopped");
        }
        for p in lock(&self.parked).drain(..) {
            p.release.abort();
            p.endpoint.close(quinn::VarInt::from_u32(0), b"service stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::from((ip, port))
    }

    #[test]
    fn a_changed_address_closes_the_old_listener_opens_the_new_and_leaves_the_rest() {
        let listening =
            [(at([192, 168, 1, 2], 47820), false), (at([10, 0, 0, 5], 47820), false), (at([127, 0, 0, 1], 5000), true)];
        let approved = [Ipv4Addr::new(192, 168, 1, 3), Ipv4Addr::new(10, 0, 0, 5)];
        let (close, open) = plan(&listening, &approved);
        assert_eq!(close, vec![at([192, 168, 1, 2], 47820)], "only the address that went");
        assert_eq!(open, vec![Ipv4Addr::new(192, 168, 1, 3)], "only the address that came");
        // Nothing changed: nothing to do; no approved address at all: every interface listener closes, the
        // development one stays.
        assert_eq!(plan(&listening[1..], &[Ipv4Addr::new(10, 0, 0, 5)]), (Vec::new(), Vec::new()));
        let (close, open) = plan(&listening, &[]);
        assert_eq!((close.len(), open.len()), (2, 0));
        // An approved address listened on as a development address is not bound twice.
        let (_, open) = plan(&[(at([192, 168, 1, 3], 5000), true)], &[Ipv4Addr::new(192, 168, 1, 3)]);
        assert!(open.is_empty());
    }

    /// Review of 22/11/2026, I1: a listener closed while a session still ran on it kept its socket bound through that
    /// session's endpoint, so when the address came back the new bind failed ("cannot listen on ...") and nothing
    /// listened on the address again until a later notice. The kept endpoint now takes the address again.
    #[test]
    fn an_address_that_comes_back_while_its_old_endpoint_lives_takes_that_endpoint_again() {
        let back = Ipv4Addr::new(192, 168, 1, 2);
        let new = Ipv4Addr::new(192, 168, 1, 7);
        let parked = [at([192, 168, 1, 2], 47820), at([10, 0, 0, 5], 47820)];
        let (revive, bind) = split_open(&[back, new], &parked, 47820);
        assert_eq!(revive, vec![at([192, 168, 1, 2], 47820)], "the address whose endpoint is kept");
        assert_eq!(bind, vec![new], "an address nothing holds gets a socket of its own");
        // A kept endpoint of another port (another instance's) is not this listener's.
        let (revive, bind) = split_open(&[back], &[at([192, 168, 1, 2], 47830)], 47820);
        assert!(revive.is_empty());
        assert_eq!(bind, vec![back]);
    }
}
