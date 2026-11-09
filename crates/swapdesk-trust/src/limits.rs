//! Per-address limits of the two endpoints (PROTOCOL.md 1 and 3), judged on every `quinn::Incoming` before any
//! handshake is spent on it.
//!
//! * Session endpoint ([`SessionLimits`]): every `Incoming` whose address is not validated gets `retry()` before
//!   anything else: a forged source address cannot answer it, so a forger costs one packet, takes no place under
//!   any limit and no handshake, and cannot use up the real peer's places. Only validated connections count: at most
//!   [`MAX_PER_ADDRESS`] at once from one remote address, the next ignored. Each admitted connection holds an
//!   [`AddressSlot`] until it ends, so the table only holds addresses with live connections and cannot grow past the
//!   handshake cap and the live sessions.
//! * Pairing endpoint ([`PairingRate`]): one connection attempt per source address per [`PAIRING_INTERVAL`]. An
//!   unvalidated address gets `retry()` first and costs the address nothing (a forged `Initial` must not burn the
//!   real initiator's turn). A validated one is checked against the address's last turn, and the turn is recorded
//!   only once the service has actually admitted the attempt (past its in-flight cap); every later attempt inside
//!   the interval is ignored. Nothing here writes an audit entry: an ignored attempt leaves no trace.
//!
//! The decisions are pure functions of the address, whether it is validated and the time, so the tests drive them with
//! a fake clock; [`SessionLimits::judge`] and [`PairingRate::judge`] read those three from the `Incoming`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use quinn::Incoming;

use crate::endpoint::Decision;

/// Connections at once from one remote address on the session endpoint (PROTOCOL.md 1, GUESS).
pub const MAX_PER_ADDRESS: usize = 4;
/// One pairing connection attempt per source address per this long (PROTOCOL.md 3, GUESS).
pub const PAIRING_INTERVAL: Duration = Duration::from_secs(30);
/// Source addresses the pairing table remembers; past it new addresses are ignored until entries expire (fail closed).
const MAX_PAIRING_ENTRIES: usize = 4096;

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- session endpoint ----------

/// What to do with one `Incoming` of the session endpoint.
#[derive(Debug)]
pub enum Admission {
    /// Go on; keep the slot until the connection ends.
    Admit(AddressSlot),
    /// The address is not validated: send `retry()`. Nothing was counted and no place was taken.
    Retry,
    /// The address is validated and already holds [`MAX_PER_ADDRESS`] connections: send nothing.
    Ignore,
}

/// Connections now live (or being handshaken) per remote address.
#[derive(Clone, Default)]
pub struct SessionLimits {
    live: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

/// One connection's place under its address's limit, given back when dropped.
pub struct AddressSlot {
    live: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl core::fmt::Debug for AddressSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSlot").field("ip", &self.ip).finish()
    }
}

impl Drop for AddressSlot {
    fn drop(&mut self) {
        let mut live = guard(&self.live);
        if let Some(n) = live.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                live.remove(&self.ip);
            }
        }
    }
}

impl SessionLimits {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judges a connection from `ip`; `validated` is `Incoming::remote_address_validated()`. An address that is not
    /// validated is retried and counts for nothing, whatever the table holds: forged `Initial`s from a victim's
    /// address must not fill the victim's places.
    pub fn admit(&self, ip: IpAddr, validated: bool) -> Admission {
        if !validated {
            return Admission::Retry;
        }
        let mut live = guard(&self.live);
        let n = live.entry(ip).or_insert(0);
        if *n >= MAX_PER_ADDRESS {
            return Admission::Ignore;
        }
        *n += 1;
        Admission::Admit(AddressSlot { live: Arc::clone(&self.live), ip })
    }

    /// [`Self::admit`] for an `Incoming`.
    pub fn judge(&self, incoming: &Incoming) -> Admission {
        self.admit(incoming.remote_address().ip(), incoming.remote_address_validated())
    }

    /// Connections counted for `ip` now.
    pub fn live_for(&self, ip: IpAddr) -> usize {
        guard(&self.live).get(&ip).copied().unwrap_or(0)
    }

    /// Addresses with at least one connection (the table's size).
    pub fn tracked(&self) -> usize {
        guard(&self.live).len()
    }
}

/// Answers an `Incoming` that [`SessionLimits`] turned away, the way quinn wants it: `retry()` (an `Incoming` that
/// cannot be retried, because it already was, is ignored) or `ignore()`.
pub fn turn_away(incoming: Incoming, retry: bool) {
    if retry {
        if let Err(e) = incoming.retry() {
            e.into_incoming().ignore();
        }
    } else {
        incoming.ignore();
    }
}

// ---------- pairing endpoint ----------

/// When each source address last made a pairing attempt.
#[derive(Default)]
pub struct PairingRate {
    last: Mutex<HashMap<IpAddr, Instant>>,
}

impl PairingRate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Judges an attempt from `ip` at `now`, recording nothing. `Retry` for an address not validated yet, `Ignore`
    /// inside [`PAIRING_INTERVAL`] of the address's last recorded turn (or when the table is full), otherwise
    /// `Accept`. The caller records the turn with [`Self::record`] once it has admitted the attempt.
    pub fn decide(&self, ip: IpAddr, validated: bool, now: Instant) -> Decision {
        if !validated {
            return Decision::Retry;
        }
        let mut last = guard(&self.last);
        if last.len() >= MAX_PAIRING_ENTRIES / 2 {
            last.retain(|_, t| now.saturating_duration_since(*t) < PAIRING_INTERVAL);
        }
        match last.get(&ip) {
            Some(t) if now.saturating_duration_since(*t) < PAIRING_INTERVAL => Decision::Ignore,
            None if last.len() >= MAX_PAIRING_ENTRIES => Decision::Ignore,
            _ => Decision::Accept,
        }
    }

    /// Records `ip`'s turn at `now`: an attempt the service admitted.
    pub fn record(&self, ip: IpAddr, now: Instant) {
        let mut last = guard(&self.last);
        if last.contains_key(&ip) || last.len() < MAX_PAIRING_ENTRIES {
            last.insert(ip, now);
        }
    }

    /// [`Self::decide`] for an `Incoming`, now: the `filter` of a `PairingService`.
    pub fn judge(&self, incoming: &Incoming) -> Decision {
        self.decide(incoming.remote_address().ip(), incoming.remote_address_validated(), Instant::now())
    }

    /// [`Self::record`] for an `Incoming`, now: the `admitted` hook of a `PairingService`.
    pub fn note(&self, incoming: &Incoming) {
        self.record(incoming.remote_address().ip(), Instant::now());
    }

    /// Source addresses remembered now.
    pub fn tracked(&self) -> usize {
        guard(&self.last).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, last))
    }

    #[test]
    fn the_fifth_validated_connection_from_one_address_is_ignored() {
        let limits = SessionLimits::new();
        let slots: Vec<_> = (0..MAX_PER_ADDRESS)
            .map(|_| match limits.admit(ip(7), true) {
                Admission::Admit(s) => s,
                other => panic!("under the limit: {other:?}"),
            })
            .collect();
        assert_eq!(limits.live_for(ip(7)), MAX_PER_ADDRESS);
        assert!(matches!(limits.admit(ip(7), true), Admission::Ignore), "over the limit, address validated");
        assert!(matches!(limits.admit(ip(7), false), Admission::Retry), "an unvalidated one is retried first, always");
        assert_eq!(limits.live_for(ip(7)), MAX_PER_ADDRESS, "a turned-away connection takes no place");
        // Another address is not affected.
        assert!(matches!(limits.admit(ip(8), true), Admission::Admit(_)));
        // A place freed by a finished connection is usable again.
        drop(slots);
        assert!(matches!(limits.admit(ip(7), true), Admission::Admit(_)));
    }

    #[test]
    fn unvalidated_attempts_from_an_address_never_take_a_place_and_never_stop_its_real_connections() {
        let limits = SessionLimits::new();
        // An attacker forges the viewer's address: any number of Initials, none validated.
        for _ in 0..100 {
            assert!(matches!(limits.admit(ip(5), false), Admission::Retry));
        }
        assert_eq!(limits.live_for(ip(5)), 0, "forged Initials hold no place");
        assert_eq!(limits.tracked(), 0, "and leave nothing in the table");
        // The real viewer, from the same address, is served: up to the limit, with a validated address.
        let slots: Vec<_> = (0..MAX_PER_ADDRESS)
            .map(|n| match limits.admit(ip(5), true) {
                Admission::Admit(s) => s,
                other => panic!("validated connection {n}: {other:?}"),
            })
            .collect();
        assert_eq!(slots.len(), MAX_PER_ADDRESS);
    }

    #[test]
    fn the_table_holds_only_addresses_with_a_live_connection() {
        let limits = SessionLimits::new();
        let a = limits.admit(ip(1), true);
        let b = limits.admit(ip(2), true);
        assert_eq!(limits.tracked(), 2);
        drop(a);
        assert_eq!(limits.tracked(), 1);
        drop(b);
        assert_eq!(limits.tracked(), 0);
        // Turned-away attempts leave nothing behind either (a flood from many addresses cannot grow the table).
        let held: Vec<_> = (0..MAX_PER_ADDRESS).map(|_| limits.admit(ip(3), true)).collect();
        for _ in 0..100 {
            let _ = limits.admit(ip(3), true);
            let _ = limits.admit(ip(4), false);
        }
        assert_eq!(limits.tracked(), 1);
        drop(held);
        assert_eq!(limits.tracked(), 0);
    }

    #[test]
    fn a_second_pairing_attempt_inside_thirty_seconds_is_ignored_and_one_after_it_is_not() {
        let rate = PairingRate::new();
        let t0 = Instant::now();
        // The service admits every attempt the filter accepts here, and records its turn.
        let attempt = |ip: IpAddr, at: Instant| {
            let d = rate.decide(ip, true, at);
            if d == Decision::Accept {
                rate.record(ip, at);
            }
            d
        };
        assert_eq!(attempt(ip(5), t0), Decision::Accept);
        assert_eq!(attempt(ip(5), t0 + Duration::from_secs(1)), Decision::Ignore);
        assert_eq!(attempt(ip(5), t0 + PAIRING_INTERVAL - Duration::from_millis(1)), Decision::Ignore);
        assert_eq!(attempt(ip(6), t0 + Duration::from_secs(2)), Decision::Accept, "another source");
        assert_eq!(attempt(ip(5), t0 + PAIRING_INTERVAL), Decision::Accept, "30 s later");
        // An ignored attempt does not push the next turn away.
        assert_eq!(attempt(ip(5), t0 + PAIRING_INTERVAL + Duration::from_secs(29)), Decision::Ignore);
        assert_eq!(attempt(ip(5), t0 + 2 * PAIRING_INTERVAL), Decision::Accept);
    }

    #[test]
    fn a_turn_is_spent_only_when_the_service_records_it() {
        let rate = PairingRate::new();
        let t0 = Instant::now();
        // Accepted by the filter but dropped by the service's in-flight cap: nothing recorded, so the same source may
        // try again at once.
        assert_eq!(rate.decide(ip(5), true, t0), Decision::Accept);
        assert_eq!(rate.decide(ip(5), true, t0 + Duration::from_secs(1)), Decision::Accept);
        rate.record(ip(5), t0 + Duration::from_secs(1));
        assert_eq!(rate.decide(ip(5), true, t0 + Duration::from_secs(2)), Decision::Ignore);
    }

    #[test]
    fn an_unvalidated_pairing_source_is_retried_and_costs_its_turn_nothing() {
        let rate = PairingRate::new();
        let t0 = Instant::now();
        for _ in 0..50 {
            assert_eq!(rate.decide(ip(9), false, t0), Decision::Retry, "a forged Initial gets a retry");
        }
        assert_eq!(rate.tracked(), 0, "and leaves no entry");
        assert_eq!(rate.decide(ip(9), true, t0), Decision::Accept, "the real source still has its turn");
        assert_eq!(rate.tracked(), 0, "deciding records nothing");
    }

    #[test]
    fn the_pairing_table_is_pruned_by_time_and_bounded() {
        let rate = PairingRate::new();
        let t0 = Instant::now();
        let addr = |i: usize| IpAddr::V4(Ipv4Addr::from(u32::try_from(i).unwrap() + 0x0a00_0000));
        for i in 0..MAX_PAIRING_ENTRIES {
            assert_eq!(rate.decide(addr(i), true, t0), Decision::Accept);
            rate.record(addr(i), t0);
        }
        assert_eq!(rate.tracked(), MAX_PAIRING_ENTRIES);
        // Full and all fresh: a new source is ignored rather than the table growing.
        assert_eq!(rate.decide(addr(MAX_PAIRING_ENTRIES), true, t0 + Duration::from_secs(1)), Decision::Ignore);
        assert_eq!(rate.tracked(), MAX_PAIRING_ENTRIES);
        // Thirty seconds on every entry has expired and is dropped.
        assert_eq!(rate.decide(addr(MAX_PAIRING_ENTRIES), true, t0 + PAIRING_INTERVAL), Decision::Accept);
        rate.record(addr(MAX_PAIRING_ENTRIES), t0 + PAIRING_INTERVAL);
        assert_eq!(rate.tracked(), 1);
    }
}
