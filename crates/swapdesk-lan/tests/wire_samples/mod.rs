//! The sample table of the 1.0 wire items that live in `swapdesk-lan`: the DNS-SD TXT record, the Wake-on-LAN
//! magic packet and the directed-broadcast address (PROTOCOL.md, "Wire fixtures of 1.0").
//!
//! Like the table of `swapdesk-proto`, this one file compiles unchanged in the tree of tag `v1.0.0`, where
//! `generate.rs` writes the fixtures, and in the current tree, where `wire_1_0.rs` checks them. Values are fixed
//! here: no randomness, no clock. It uses only the public API that exists at that tag.

#![allow(dead_code)]

use std::net::Ipv4Addr;

use swapdesk_lan::discovery::{MDNS_PORT, SERVICE_TYPE};
use swapdesk_lan::txt::{MAX_ENTRIES, MAX_ENTRY_BYTES, MAX_NAME_BYTES, Txt};
use swapdesk_lan::wol::{MAGIC_PACKET_LEN, WOL_PORT, directed_broadcast, magic_packet};
use swapdesk_proto::Os;

/// A wire item of this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// The TXT record as it goes on the wire (RFC 6763 6.1): each entry is one length byte and `key=value`.
    Txt(Txt),
    /// The 102-byte magic packet of a MAC address.
    MagicPacket([u8; 6]),
    /// The directed-broadcast address of `addr/prefix`: the address (4 octets), the prefix (1 byte), then the
    /// broadcast address (4 octets).
    DirectedBroadcast { addr: [u8; 4], prefix: u8 },
    /// A constant both sides must agree on, as its bytes.
    Constant(Vec<u8>),
}

fn txt_wire(txt: &Txt) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (key, value) in txt.encode().map_err(|e| e.to_string())? {
        let entry = format!("{key}={value}");
        out.push(u8::try_from(entry.len()).map_err(|_| "TXT entry over 255 bytes".to_string())?);
        out.extend_from_slice(entry.as_bytes());
    }
    Ok(out)
}

fn txt_from_wire(bytes: &[u8]) -> Result<Txt, String> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rest = bytes;
    while let Some((&len, tail)) = rest.split_first() {
        let len = usize::from(len);
        if tail.len() < len {
            return Err("TXT entry runs past the record".into());
        }
        let (entry, tail) = tail.split_at(len);
        let eq = entry.iter().position(|b| *b == b'=').ok_or("TXT entry without '='")?;
        let key = std::str::from_utf8(&entry[..eq]).map_err(|e| e.to_string())?.to_string();
        entries.push((key, entry[eq + 1..].to_vec()));
        rest = tail;
    }
    let borrowed: Vec<(&str, &[u8])> = entries.iter().map(|(k, v)| (k.as_str(), v.as_slice())).collect();
    Txt::decode(&borrowed).map_err(|e| e.to_string())
}

impl Item {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        match self {
            Item::Txt(t) => txt_wire(t),
            Item::MagicPacket(mac) => Ok(magic_packet(*mac).to_vec()),
            Item::DirectedBroadcast { addr, prefix } => {
                let mut out = addr.to_vec();
                out.push(*prefix);
                out.extend_from_slice(&directed_broadcast(Ipv4Addr::from(*addr), *prefix).octets());
                Ok(out)
            }
            Item::Constant(b) => Ok(b.clone()),
        }
    }

    /// Reads `bytes` the way a receiver does, into an item of the same kind as `self`.
    pub fn decode_like(&self, bytes: &[u8]) -> Result<Item, String> {
        Ok(match self {
            Item::Txt(_) => Item::Txt(txt_from_wire(bytes)?),
            Item::MagicPacket(_) => {
                if bytes.len() != MAGIC_PACKET_LEN {
                    return Err(format!("magic packet of {} bytes", bytes.len()));
                }
                if bytes[..6] != [0xFF; 6] {
                    return Err("magic packet does not start with six 0xFF".into());
                }
                let mac: [u8; 6] = bytes[6..12].try_into().map_err(|_| "short")?;
                if bytes[6..].chunks(6).any(|c| c != mac) {
                    return Err("the MAC is not repeated 16 times".into());
                }
                Item::MagicPacket(mac)
            }
            Item::DirectedBroadcast { .. } => {
                let [a, b, c, d, prefix, e, f, g, h]: [u8; 9] = bytes.try_into().map_err(|_| "9 bytes expected")?;
                // The receiver recomputes the broadcast address, and the fixture must agree.
                let want = directed_broadcast(Ipv4Addr::new(a, b, c, d), prefix).octets();
                if [e, f, g, h] != want {
                    return Err(format!("broadcast address {:?}, wanted {want:?}", [e, f, g, h]));
                }
                Item::DirectedBroadcast { addr: [a, b, c, d], prefix }
            }
            Item::Constant(_) => Item::Constant(bytes.to_vec()),
        })
    }

    pub fn family(&self) -> &'static str {
        match self {
            Item::Txt(_) => "dns-sd",
            Item::MagicPacket(_) | Item::DirectedBroadcast { .. } => "wake-on-lan",
            Item::Constant(_) => "constant",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Item::Txt(_) => "TXT record of _swapdesk._udp".into(),
            Item::MagicPacket(_) => "magic packet".into(),
            Item::DirectedBroadcast { prefix, .. } => format!("broadcast address, prefix {prefix}"),
            Item::Constant(b) => format!("constant, {} byte(s)", b.len()),
        }
    }
}

pub struct Sample {
    pub name: String,
    pub item: Item,
}

pub fn file_name(name: &str) -> String {
    format!("{name}.hex")
}

/// 16 bytes per line, lowercase, one final newline.
pub fn to_hex_text(bytes: &[u8]) -> String {
    let mut out = String::new();
    for line in bytes.chunks(16) {
        for b in line {
            out.push_str(&format!("{b:02x}"));
        }
        out.push('\n');
    }
    out
}

pub fn from_hex_text(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(format!("not a lowercase hex digit: {:?}", c as char)),
    };
    digits.chunks(2).map(|p| Ok(nibble(p[0])? << 4 | nibble(p[1])?)).collect()
}

pub fn index_text(samples: &[Sample]) -> String {
    let mut out = String::from("# name\tfamily\twhat\n");
    for s in samples {
        out.push_str(&format!("{}\t{}\t{}\n", s.name, s.item.family(), s.item.describe()));
    }
    out
}

fn ascending<const N: usize>(start: u8) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = start.wrapping_add(i as u8);
    }
    out
}

fn txt(name: &str, os: Os, fingerprint: [u8; 16], pairing: bool) -> Txt {
    Txt { name: name.into(), os, fingerprint, pairing }
}

pub fn samples() -> Vec<Sample> {
    let mut v: Vec<Sample> = Vec::new();
    let mut add = |name: &str, item: Item| v.push(Sample { name: name.into(), item });

    for (os, os_name) in [(Os::Windows, "windows"), (Os::MacOs, "macos")] {
        for (pairing, mode) in [(false, "idle"), (true, "pairing")] {
            add(&format!("txt_{os_name}_{mode}"), Item::Txt(txt("Notebook da sala", os, ascending(0xA0), pairing)));
        }
    }
    add("txt_name_multibyte", Item::Txt(txt("Máquina do João 日本 😀", Os::MacOs, [0xAB; 16], false)));
    add("txt_name_64_bytes", Item::Txt(txt(&"n".repeat(MAX_NAME_BYTES), Os::Windows, [0x00; 16], true)));
    add("txt_name_one_byte", Item::Txt(txt("x", Os::Windows, [0xFF; 16], false)));
    add("txt_fingerprint_all_digits", Item::Txt(txt("Mesa", Os::MacOs, ascending(0x00), false)));

    for (name, mac) in [
        ("laa", [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]),
        ("typical", [0x00, 0x1B, 0x44, 0x11, 0x3A, 0xB7]),
        ("ascending", ascending::<6>(0x10)),
        ("zero", [0; 6]),
        ("broadcast", [0xFF; 6]),
    ] {
        add(&format!("wol_magic_packet_{name}"), Item::MagicPacket(mac));
    }
    // Host bits all ones; the prefixes of a home LAN, a direct cable, and the edges.
    for (name, addr, prefix) in [
        ("24", [192, 168, 1, 20], 24),
        ("16", [10, 1, 2, 3], 16),
        ("25", [192, 168, 1, 200], 25),
        ("30", [169, 254, 10, 5], 30),
        ("8", [10, 0, 0, 1], 8),
        ("32", [192, 168, 1, 20], 32),
    ] {
        add(&format!("wol_broadcast_prefix_{name}"), Item::DirectedBroadcast { addr, prefix });
    }

    add("const_service_type", Item::Constant(SERVICE_TYPE.as_bytes().to_vec()));
    add("const_mdns_port", Item::Constant(MDNS_PORT.to_le_bytes().to_vec()));
    add("const_wol_port", Item::Constant(WOL_PORT.to_le_bytes().to_vec()));
    add("const_magic_packet_len", Item::Constant((MAGIC_PACKET_LEN as u64).to_le_bytes().to_vec()));
    add("const_max_entries", Item::Constant((MAX_ENTRIES as u64).to_le_bytes().to_vec()));
    add("const_max_entry_bytes", Item::Constant((MAX_ENTRY_BYTES as u64).to_le_bytes().to_vec()));
    add("const_max_name_bytes", Item::Constant((MAX_NAME_BYTES as u64).to_le_bytes().to_vec()));
    v
}
