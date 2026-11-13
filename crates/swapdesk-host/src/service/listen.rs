//! The session endpoint's sockets, one per approved interface address (PROTOCOL.md 1), following the network.
//!
//! [`ServiceCore::start`](super::ServiceCore::start) binds them once; [`Listeners::follow`] runs on every network change
//! ([`super::ServiceCore::network_changed`]). An address that is no longer approved stops taking new connections and
//! its accept loop ends; the sessions already on it are left alone (they end by themselves if the address is gone). A
//! new approved address gets an endpoint of its own on the session port. An address that is still approved is not
//! touched, so neither are its sessions. Without this a machine whose DHCP address changed went on listening on the
//! old address only, and nobody could reach it until the program restarted (seen on a Mac, 09/11).

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

pub(crate) struct Listeners {
    port: u16,
    identity: Arc<Identity>,
    trust: Arc<TrustHandle>,
    gate: Arc<Gate>,
    shared: Arc<Shared>,
    list: Mutex<Vec<Listener>>,
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
                self.shared
                    .log(Level::Info, format_args!("session endpoint off {addr}: the address is not approved now"));
            }
        }
        let mut opened = false;
        for ip in &open {
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

    /// Closes every endpoint (the service is stopping).
    pub fn close_all(&self) {
        for l in lock(&self.list).drain(..) {
            l.accept.abort();
            l.endpoint.close(quinn::VarInt::from_u32(0), b"service stopped");
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
}
