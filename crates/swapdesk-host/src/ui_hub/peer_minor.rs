//! Which protocol minor each paired device speaks (PROTOCOL.md 1), as far as this machine knows.
//!
//! Two sources, of different weight:
//!
//! * the **hint**: the `minor` key of the device's discovery record. Anybody on the network can publish a record,
//!   so it only decides what is asked, never what is trusted;
//! * what the device **said** in an authenticated exchange (the `HelloAck` of a session with its pinned key). It
//!   wins over the hint for as long as the record stays what it was when the device said it.
//!
//! The viewer asks for entry without a prompt only toward a host known to be of minor 1 or newer: a 1.0 host that
//! stored `UNATTENDED` for this device would let it in at once with machine-level input, which the 1.0 interface
//! never asked for. In memory only: after a restart the hint decides again until the next session.

use std::collections::HashMap;

use swapdesk_trust::DeviceId;

/// What a device said, and the hint its record carried then (`None`: it was not on the network).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Said {
    minor: u16,
    hint: Option<u16>,
}

#[derive(Debug, Default)]
pub(super) struct PeerMinors {
    said: HashMap<DeviceId, Said>,
}

impl PeerMinors {
    /// The minor to count on for `device`, whose record now carries `hint` (`None`: not on the network).
    ///
    /// * What the device said holds while its record is the one it had then, also against a hint that says more:
    ///   a host that answered minor 0 under a record of minor 1 is a 1.0 host behind a forged or stale record.
    /// * A record that changed since is a device that changed (an update, a downgrade): its hint decides again.
    /// * A device that never said anything is as new as its record says, and of minor 0 when it has none.
    pub fn known(&self, device: &DeviceId, hint: Option<u16>) -> u16 {
        match (self.said.get(device), hint) {
            (Some(said), Some(now)) if said.hint != Some(now) => now,
            (Some(said), _) => said.minor,
            (None, hint) => hint.unwrap_or(0),
        }
    }

    /// `device` announced `minor` over a connection its pinned key authenticated, while its record carried `hint`.
    /// Returns true when it says less than the record did: worth one line in the trace.
    pub fn said(&mut self, device: DeviceId, minor: u16, hint: Option<u16>) -> bool {
        self.said.insert(device, Said { minor, hint });
        hint.is_some_and(|h| minor < h)
    }

    /// The device was forgotten: what it said goes with its pairing.
    pub fn forget(&mut self, device: &DeviceId) {
        self.said.remove(device);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: DeviceId = [1; 16];
    const B: DeviceId = [2; 16];

    #[test]
    fn a_device_that_never_spoke_is_as_new_as_its_record_and_of_minor_0_without_one() {
        let m = PeerMinors::default();
        assert_eq!(m.known(&A, None), 0, "not on the network, never seen: 1.0 until proven otherwise");
        assert_eq!(m.known(&A, Some(0)), 0, "a record without the key is a 1.0 record");
        assert_eq!(m.known(&A, Some(1)), 1);
        assert_eq!(m.known(&A, Some(7)), 7);
    }

    #[test]
    fn what_a_device_said_wins_over_the_hint_while_its_record_stays_the_same() {
        let mut m = PeerMinors::default();
        // A record without the key, but the host answered minor 1 (a record read before the host updated).
        assert!(!m.said(A, 1, Some(0)));
        assert_eq!(m.known(&A, Some(0)), 1);
        // Reached by a typed address, no record at all: what it said is all there is.
        assert!(!m.said(B, 1, None));
        assert_eq!(m.known(&B, None), 1);
        // It leaves the network: what it said is kept.
        assert_eq!(m.known(&A, None), 1);
    }

    #[test]
    fn a_minor_0_helloack_after_a_minor_1_hint_records_minor_0_until_the_record_changes() {
        let mut m = PeerMinors::default();
        assert_eq!(m.known(&A, Some(1)), 1, "the hint alone: the viewer asks");
        // The session's authenticated HelloAck says 0: a 1.0 host behind a forged or stale record.
        assert!(m.said(A, 0, Some(1)), "the disagreement is reported once, for the trace");
        assert_eq!(m.known(&A, Some(1)), 0, "the same record no longer counts");
        assert_eq!(m.known(&A, None), 0);
        // The record changes (the host really updated, and its record says so anew): the hint decides again.
        assert_eq!(m.known(&A, Some(2)), 2);
        // An agreeing answer is no disagreement.
        assert!(!m.said(A, 2, Some(2)));
        assert_eq!(m.known(&A, Some(2)), 2);
    }

    #[test]
    fn a_record_that_went_back_to_minor_0_is_believed_over_what_was_said_before() {
        let mut m = PeerMinors::default();
        m.said(A, 1, Some(1));
        assert_eq!(m.known(&A, Some(1)), 1);
        // The host was put back on 1.0: its record lost the key. Asking it for unattended entry now would be the
        // 1.0 entry with machine-level input, so the record wins.
        assert_eq!(m.known(&A, Some(0)), 0);
    }

    #[test]
    fn forgetting_a_device_forgets_what_it_said() {
        let mut m = PeerMinors::default();
        m.said(A, 1, None);
        m.forget(&A);
        assert_eq!(m.known(&A, None), 0);
    }
}
