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
use swapdesk_trust::{Identity, TrustHandle, open_session_endpoint};
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
        Self { port, identity, trust, gate, shared, list: Mutex::new(Vec::new()) }
    }

    /// Starts accepting on `endpoint`.
    pub fn adopt(&self, endpoint: Endpoint, extra: bool) -> std::io::Result<SocketAddr> {
        let local = endpoint.local_addr()?;
        let accept = self.shared.rt.spawn(accept_loop(Arc::clone(&self.gate), endpoint.clone())).abort_handle();
        lock(&self.list).push(Listener { local, endpoint, extra, accept });
        Ok(local)
    }

    /// The addresses the session endpoint listens on now.
    pub fn addrs(&self) -> Vec<SocketAddr> {
        lock(&self.list).iter().map(|l| l.local).collect()
    }

    /// Follows the approved addresses of `list`. Returns whether anything changed.
    pub fn follow(&self, list: &[NetInterface]) -> bool {
        let mut approved: Vec<Ipv4Addr> = approved_addrs(list).iter().map(|a| a.addr).collect();
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
                    open_session_endpoint(s, &self.identity, self.trust.clone()).map_err(|e| e.to_string())
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
