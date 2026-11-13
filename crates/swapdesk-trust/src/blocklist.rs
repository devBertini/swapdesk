//! The devices blocked by their link-layer (MAC) address (D65), in `blocked.bin` beside `trust.bin`.
//!
//! A file of its own, not a field of `trust.bin`: an older build (the one the updater restores when a new version does
//! not start) reads `trust.bin` strictly, so a new field there would cost it every pairing. It never opens this file.

use swapdesk_proto::codec::Result;
use swapdesk_proto::{CodecError, Reader, Wire, Writer};

/// The blocked devices' blob in the `SecretStore`.
pub const BLOCKED_NAME: &str = "blocked.bin";
/// Format of `blocked.bin`.
pub const BLOCKED_FORMAT: u16 = 1;
pub const MAX_BLOCKED_DEVICES: usize = 256;
const MAX_BLOCKED_NAME: usize = 64;

/// A device refused before any pairing card, prompt or alert: its MAC, and what was seen of it when it was blocked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedDevice {
    pub mac: [u8; 6],
    /// The IPv4 address it came from when it was blocked (shown in Ajustes; never used to refuse).
    pub last_ipv4: [u8; 4],
    /// Its name as it announced it, empty when it never did.
    pub name: String,
    /// Unix milliseconds.
    pub blocked_at_ms: u64,
}

impl Wire for BlockedDevice {
    const MIN_SIZE: usize = 6 + 4 + 2 + 8;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_bytes(&self.mac);
        w.put_bytes(&self.last_ipv4);
        w.put_str(&self.name, MAX_BLOCKED_NAME)?;
        w.put_u64(self.blocked_at_ms);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        let mut mac = [0u8; 6];
        for b in &mut mac {
            *b = r.u8()?;
        }
        let mut last_ipv4 = [0u8; 4];
        for b in &mut last_ipv4 {
            *b = r.u8()?;
        }
        Ok(Self { mac, last_ipv4, name: r.str(MAX_BLOCKED_NAME)?, blocked_at_ms: r.u64()? })
    }
}

/// `blocked.bin`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockList {
    pub devices: Vec<BlockedDevice>,
}

impl BlockList {
    pub fn contains(&self, mac: &[u8; 6]) -> bool {
        self.devices.iter().any(|d| &d.mac == mac)
    }

    /// Adds `device`, or refreshes what was seen of a MAC already blocked. False when the list is full.
    pub fn block(&mut self, device: BlockedDevice) -> bool {
        if let Some(d) = self.devices.iter_mut().find(|d| d.mac == device.mac) {
            *d = device;
            return true;
        }
        if self.devices.len() >= MAX_BLOCKED_DEVICES {
            return false;
        }
        self.devices.push(device);
        true
    }

    /// Removes a MAC; true if it was there.
    pub fn unblock(&mut self, mac: &[u8; 6]) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| &d.mac != mac);
        self.devices.len() != before
    }
}

impl Wire for BlockList {
    const MIN_SIZE: usize = 2 + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_u16(BLOCKED_FORMAT);
        self.devices.put(w, MAX_BLOCKED_DEVICES)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        if r.u16()? != BLOCKED_FORMAT {
            return Err(CodecError::InvalidValue("blocked list format"));
        }
        Ok(Self { devices: <Vec<BlockedDevice>>::get(r, MAX_BLOCKED_DEVICES)? })
    }
}

/// `aa:bb:cc:dd:ee:ff`.
pub fn mac_text(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The MAC of `aa:bb:cc:dd:ee:ff` (either case, `-` also accepted), `None` for anything else.
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (b, p) in mac.iter_mut().zip(parts) {
        if p.len() != 2 {
            return None;
        }
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    Some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(last: u8) -> BlockedDevice {
        BlockedDevice {
            mac: [0x02, 0, 0, 0, 0, last],
            last_ipv4: [192, 168, 0, last],
            name: format!("pc-{last}"),
            blocked_at_ms: 1_000,
        }
    }

    #[test]
    fn a_list_round_trips_and_a_truncation_or_a_trailing_byte_is_refused() {
        let mut list = BlockList::default();
        assert!(list.block(device(1)) && list.block(device(2)));
        let bytes = list.encode().unwrap();
        assert_eq!(BlockList::decode(&bytes).unwrap(), list);
        for n in 0..bytes.len() {
            assert!(BlockList::decode(&bytes[..n]).is_err(), "truncated at {n}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(BlockList::decode(&longer).is_err());
    }

    #[test]
    fn blocking_a_mac_again_refreshes_it_and_unblocking_removes_it() {
        let mut list = BlockList::default();
        list.block(device(1));
        let mut again = device(1);
        again.last_ipv4 = [192, 168, 0, 77];
        list.block(again.clone());
        assert_eq!(list.devices, vec![again]);
        assert!(list.contains(&device(1).mac) && !list.contains(&device(2).mac));
        assert!(list.unblock(&device(1).mac) && !list.unblock(&device(1).mac));
        assert!(list.devices.is_empty());
    }

    #[test]
    fn the_list_is_capped() {
        let mut list = BlockList::default();
        for i in 0..MAX_BLOCKED_DEVICES {
            let mut d = device(0);
            d.mac = [0x02, 0, 0, 0, (i >> 8) as u8, i as u8];
            assert!(list.block(d));
        }
        let mut one_more = device(0);
        one_more.mac = [0x02, 1, 1, 1, 1, 1];
        assert!(!list.block(one_more));
    }

    #[test]
    fn a_mac_is_written_and_read_in_the_usual_notation() {
        let mac = [0xaa, 0xbb, 0x0c, 0x01, 0xee, 0xff];
        assert_eq!(mac_text(&mac), "aa:bb:0c:01:ee:ff");
        assert_eq!(parse_mac("AA-BB-0C-01-EE-FF"), Some(mac));
        assert_eq!(parse_mac("aa:bb:0c:01:ee"), None);
        assert_eq!(parse_mac("aa:bb:0c:01:ee:fff"), None);
        assert_eq!(parse_mac("aa:bb:0c:01:ee:zz"), None);
    }
}
