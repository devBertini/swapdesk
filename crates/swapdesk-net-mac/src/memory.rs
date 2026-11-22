//! The SystemConfiguration type of each interface, kept across reads. Pure: it builds and is tested on every host.
//!
//! `SCNetworkInterfaceCopyAll` can come back without an interface that `getifaddrs` still lists, for a moment (while
//! the configuration daemon rebuilds its list after a network notice). Classified on that read alone, the Wi-Fi `en0`
//! became `Virtual` (an `en` that SystemConfiguration does not list fails closed), the guard approved no address, and
//! the session endpoint's listener of an address that was still there closed: a live session went silent (review of
//! 22/11/2026, I1).
//!
//! So a read that does not list an interface keeps the type SystemConfiguration gave that same interface before. The
//! same interface means the same BSD name and the same link-layer address (a different card plugged in as the same
//! `en<N>`, or a Wi-Fi private address, is a new interface and is not remembered). Only the type is kept: the flags,
//! the link state and the addresses always come from the fresh `getifaddrs`, so an address that left is still dropped
//! and a link that is down is still refused. A read that lists the interface with another type wins at once. The memory
//! is bounded from the first read that missed the interface: after [`MISSING_LIMIT`] without SystemConfiguration
//! listing it, it fails closed again. An interface `getifaddrs` no longer lists is forgotten.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::classify::ScKind;

/// How long an interface may stay unlisted by SystemConfiguration and keep the type it had (counted from the first read
/// that missed it).
pub const MISSING_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
struct Known {
    mac: [u8; 6],
    kind: ScKind,
    /// The first read since the last listing that did not list it.
    missing_since: Option<Instant>,
}

/// The last SystemConfiguration type of each interface, by BSD name and link-layer address.
#[derive(Debug, Default)]
pub struct ScMemory {
    known: HashMap<String, Known>,
}

/// What one read says about one interface.
#[derive(Debug, Clone, Copy)]
pub struct Seen<'a> {
    /// The BSD name `getifaddrs` gave.
    pub name: &'a str,
    /// The link-layer address `getifaddrs` gave, if any.
    pub mac: Option<[u8; 6]>,
    /// SystemConfiguration's type in this read; `None` when this read does not list the interface.
    pub listed: Option<ScKind>,
}

impl ScMemory {
    /// The type to classify each interface of one read with (in the order given), at `now`. Interfaces of earlier reads
    /// that are not in `seen` are forgotten.
    pub fn settle(&mut self, seen: &[Seen<'_>], now: Instant) -> Vec<Option<ScKind>> {
        self.known.retain(|name, _| seen.iter().any(|s| s.name == name));
        seen.iter().map(|s| self.one(s, now)).collect()
    }

    fn one(&mut self, s: &Seen<'_>, now: Instant) -> Option<ScKind> {
        match (s.listed, s.mac) {
            (Some(kind), Some(mac)) => {
                self.known.insert(s.name.to_string(), Known { mac, kind, missing_since: None });
                Some(kind)
            }
            (Some(kind), None) => {
                // Nothing to tell this interface from another one with the same name later: nothing is kept.
                self.known.remove(s.name);
                Some(kind)
            }
            (None, mac) => {
                let known = self.known.get_mut(s.name)?;
                if mac != Some(known.mac) {
                    self.known.remove(s.name);
                    return None;
                }
                let since = *known.missing_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= MISSING_LIMIT {
                    return None;
                }
                Some(known.kind)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{IFF_BROADCAST, IFF_RUNNING, IFF_UP, InterfaceFacts, classify};
    use swapdesk_platform::IfKind;

    const MAC: [u8; 6] = [0x3c, 0x22, 0xfb, 0x01, 0x02, 0x03];
    const OTHER_MAC: [u8; 6] = [0x3c, 0x22, 0xfb, 0x0a, 0x0b, 0x0c];
    const LAN: u32 = IFF_UP | IFF_BROADCAST | IFF_RUNNING;

    fn en0(listed: Option<ScKind>) -> Seen<'static> {
        Seen { name: "en0", mac: Some(MAC), listed }
    }

    /// The kind the guard sees for `en0` after the reads, one after the other, `step` apart.
    fn kinds(memory: &mut ScMemory, reads: &[Seen<'_>], start: Instant, step: Duration) -> Vec<IfKind> {
        reads
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let at = start + step * u32::try_from(i).unwrap_or(u32::MAX);
                let sc = memory.settle(std::slice::from_ref(s), at)[0];
                classify(&InterfaceFacts { name: s.name, sc, flags: LAN })
            })
            .collect()
    }

    /// Review of 22/11/2026, I1: one read of SystemConfiguration that did not list the Wi-Fi made `en0` Virtual, the
    /// guard approved no address and the session endpoint's listener of an address that was still there closed.
    #[test]
    fn a_read_that_does_not_list_the_wifi_keeps_the_type_it_had() {
        let mut memory = ScMemory::default();
        let got = kinds(
            &mut memory,
            &[en0(Some(ScKind::Wifi)), en0(None), en0(None), en0(Some(ScKind::Wifi))],
            Instant::now(),
            Duration::from_millis(300),
        );
        assert_eq!(got, [IfKind::WiFi, IfKind::WiFi, IfKind::WiFi, IfKind::WiFi]);
    }

    /// Review of 22/11/2026, I1: the memory never approves an interface SystemConfiguration never called Ethernet or
    /// Wi-Fi.
    #[test]
    fn an_en_interface_never_listed_still_fails_closed() {
        let mut memory = ScMemory::default();
        let got = kinds(&mut memory, &[en0(None), en0(None)], Instant::now(), Duration::from_secs(1));
        assert_eq!(got, [IfKind::Virtual, IfKind::Virtual]);
        // Listed with another type: that type is what is kept.
        let got = kinds(&mut memory, &[en0(Some(ScKind::Other)), en0(None)], Instant::now(), Duration::from_secs(1));
        assert_eq!(got, [IfKind::Virtual, IfKind::Virtual]);
    }

    /// Review of 22/11/2026, I1: a different card under the same name, or no link-layer address to tell it by, is not
    /// the interface that was listed.
    #[test]
    fn another_link_layer_address_under_the_same_name_is_not_remembered() {
        let mut memory = ScMemory::default();
        let now = Instant::now();
        memory.settle(&[en0(Some(ScKind::Ethernet))], now);
        let other = Seen { name: "en0", mac: Some(OTHER_MAC), listed: None };
        assert_eq!(memory.settle(&[other], now), [None]);
        // And the old card is forgotten: it does not come back with the first address either.
        assert_eq!(memory.settle(&[en0(None)], now), [None]);
        memory.settle(&[en0(Some(ScKind::Ethernet))], now);
        let no_mac = Seen { name: "en0", mac: None, listed: None };
        assert_eq!(memory.settle(&[no_mac], now), [None]);
    }

    /// Review of 22/11/2026, I1: the memory is bounded from the first read that missed the interface, however long the
    /// interface had been listed before.
    #[test]
    fn an_interface_left_unlisted_past_the_limit_fails_closed() {
        let mut memory = ScMemory::default();
        let start = Instant::now();
        memory.settle(&[en0(Some(ScKind::Wifi))], start);
        // Minutes pass between notices on a quiet host: the bound starts with the read that missed it.
        let first_miss = start + Duration::from_secs(600);
        assert_eq!(memory.settle(&[en0(None)], first_miss), [Some(ScKind::Wifi)]);
        let just_before = first_miss + MISSING_LIMIT - Duration::from_millis(1);
        assert_eq!(memory.settle(&[en0(None)], just_before), [Some(ScKind::Wifi)]);
        assert_eq!(memory.settle(&[en0(None)], first_miss + MISSING_LIMIT), [None]);
        // Listed again: the bound starts over at the next miss.
        let back = first_miss + MISSING_LIMIT + Duration::from_secs(1);
        assert_eq!(memory.settle(&[en0(Some(ScKind::Wifi))], back), [Some(ScKind::Wifi)]);
        assert_eq!(memory.settle(&[en0(None)], back + Duration::from_secs(1)), [Some(ScKind::Wifi)]);
    }

    /// Review of 22/11/2026, I1: an interface that left `getifaddrs` is forgotten, and the memory does not keep a link
    /// up or an address there: those come from the fresh read only.
    #[test]
    fn an_interface_that_left_is_forgotten_and_its_state_is_never_kept() {
        let mut memory = ScMemory::default();
        let now = Instant::now();
        memory.settle(&[en0(Some(ScKind::Wifi))], now);
        // A read without en0 at all (the card went away).
        let en1 = Seen { name: "en1", mac: Some(OTHER_MAC), listed: Some(ScKind::Ethernet) };
        assert_eq!(memory.settle(&[en1], now), [Some(ScKind::Ethernet)]);
        assert_eq!(memory.settle(&[en0(None)], now), [None]);
        // The type is all that is kept: a link that is not running is refused by the guard whatever the type.
        memory.settle(&[en0(Some(ScKind::Wifi))], now);
        let sc = memory.settle(&[en0(None)], now)[0];
        let down = classify(&InterfaceFacts { name: "en0", sc, flags: IFF_UP | IFF_BROADCAST });
        assert_eq!(down, IfKind::WiFi);
        assert!(!crate::classify::is_up(IFF_UP | IFF_BROADCAST), "the guard refuses an interface that is not up");
    }
}
