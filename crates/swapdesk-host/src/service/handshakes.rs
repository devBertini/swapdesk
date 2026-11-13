//! The handshakes in flight on the session endpoints (PROTOCOL.md 1, F4 of the 13/11 security review).
//!
//! A handshake is the one stage where the peer is not yet authenticated (mutual TLS: it ends when the peer's pinned
//! certificate is checked). A neighbour that starts handshakes and goes quiet holds a place for as long as the stage
//! may last, so the places are rationed so that no few addresses can take them all:
//!
//! * at most [`PER_ADDRESS`] handshakes in flight per source address: a newcomer from an address that already holds
//!   that many takes the place of its oldest (the address only ever displaces itself). Not one: a viewer legitimately
//!   dials twice at once (the session and the grants sync, GRANTS-SYNC.md), and the test of a reconnect showed the
//!   second one displacing the first when the limit was one;
//! * at most [`crate::service::MAX_HANDSHAKES`] on the machine: when the cap is full, the oldest handshake in flight is
//!   dropped and the newcomer takes its place, rather than the newcomer being ignored;
//! * and the stage itself is short ([`swapdesk_trust::ACCEPT_HANDSHAKE_LIMIT`]).
//!
//! A place is held only through the handshake: once the peer is authenticated the connection is a session and counts
//! under the per-address session limit alone.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::oneshot;

/// Handshakes in flight at once from one source address: a session and a grants sync.
pub const PER_ADDRESS: usize = 2;

/// The places, oldest first.
#[derive(Default)]
pub struct Handshakes {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    list: VecDeque<Entry>,
}

struct Entry {
    id: u64,
    ip: IpAddr,
    /// Dropped when the entry is displaced: the place's receiver then resolves.
    _evict: oneshot::Sender<()>,
}

/// One handshake's place, given back when dropped.
pub struct Place {
    owner: Arc<Handshakes>,
    id: u64,
    evicted: oneshot::Receiver<()>,
}

impl Handshakes {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn guard(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Handshakes in flight now.
    pub fn in_flight(&self) -> usize {
        self.guard().list.len()
    }

    /// A place for a handshake from `ip`, displacing what stands in its way: the oldest handshake of that address while
    /// it holds `per_address`, and the oldest of all while `cap` places are taken. The displaced handshakes'
    /// [`Place::evicted`] resolve.
    pub fn enter(self: &Arc<Self>, ip: IpAddr, per_address: usize, cap: usize) -> Place {
        let (tx, evicted) = oneshot::channel();
        let mut inner = self.guard();
        while inner.list.iter().filter(|e| e.ip == ip).count() >= per_address.max(1) {
            if let Some(oldest) = inner.list.iter().position(|e| e.ip == ip) {
                inner.list.remove(oldest);
            }
        }
        while inner.list.len() >= cap.max(1) {
            inner.list.pop_front();
        }
        let id = inner.next_id;
        inner.next_id += 1;
        inner.list.push_back(Entry { id, ip, _evict: tx });
        Place { owner: Arc::clone(self), id, evicted }
    }
}

impl Place {
    /// Resolves when a newer handshake displaced this one: the handshake is then abandoned.
    pub async fn evicted(&mut self) {
        // The sender is only ever dropped (by the displacement, or with the entry), never sent on.
        let _ = (&mut self.evicted).await;
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.owner.guard().list.retain(|e| e.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::net::Ipv4Addr;

    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, n))
    }

    fn displaced(place: &mut Place) -> bool {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        std::pin::pin!(place.evicted()).poll(&mut cx).is_ready()
    }

    #[test]
    fn an_address_holds_its_share_and_a_newcomer_from_it_takes_the_oldest_of_its_own() {
        let hs = Handshakes::new();
        let mut first = hs.enter(ip(1), 2, 16);
        let mut other = hs.enter(ip(2), 2, 16);
        let mut second = hs.enter(ip(1), 2, 16);
        assert_eq!(hs.in_flight(), 3, "a session and a sync from one address are both let in");
        assert!(!displaced(&mut first) && !displaced(&mut second));
        let mut third = hs.enter(ip(1), 2, 16);
        assert_eq!(hs.in_flight(), 3);
        assert!(displaced(&mut first), "the address's oldest handshake gave way");
        assert!(!displaced(&mut other), "another address is not touched");
        assert!(!displaced(&mut second) && !displaced(&mut third));
        drop(first);
        assert_eq!(hs.in_flight(), 3, "the displaced place's drop frees nothing twice");
    }

    #[test]
    fn a_full_cap_drops_the_oldest_handshake_and_not_the_newcomer() {
        let hs = Handshakes::new();
        let mut places: Vec<Place> = (1..=4).map(|n| hs.enter(ip(n), 2, 4)).collect();
        assert_eq!(hs.in_flight(), 4);
        let newcomer = hs.enter(ip(9), 2, 4);
        assert_eq!(hs.in_flight(), 4, "the cap holds");
        assert!(displaced(&mut places[0]), "the oldest went");
        for p in &mut places[1..] {
            assert!(!displaced(p), "the others stay");
        }
        drop(newcomer);
        assert_eq!(hs.in_flight(), 3);
        places.clear();
        assert_eq!(hs.in_flight(), 0);
    }
}
