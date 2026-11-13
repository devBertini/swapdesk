//! The sample table of the 1.0 identity and pairing values that `swapdesk-trust` computes: the SPKI of a device
//! key, the device ID, and the hashes of the pairing exchange (PROTOCOL.md 2.1 and 3, "Wire fixtures of 1.0").
//!
//! Like the other tables, this one file compiles unchanged in the tree of tag `v1.0.0`, where `generate.rs`
//! writes the fixtures, and in the current tree, where `wire_1_0.rs` checks them. Values are fixed here: no
//! randomness, no clock. It uses only the public API that exists at that tag.

#![allow(dead_code)]

use swapdesk_proto::{CommitmentInput, EXPORTER_LEN, NONCE_LEN, SPKI_LEN, Sas, SasInput};
use swapdesk_trust::ids::{device_id, device_id_hex, parse_spki, public_key_of, sha256, spki_from_public_key};

/// A value this crate derives, as the bytes both peers must derive alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A device key: its SPKI (44 bytes), its device ID (16 bytes) and the 32 lowercase hex characters of the ID.
    Identity { public_key: [u8; 32] },
    /// The pairing exchange of two devices: `commit = SHA-256(commit preimage)` (32 bytes), `s = SHA-256(SAS
    /// preimage)` (32 bytes), the six digits as a `u32` little-endian, and their text "482 913".
    PairingHashes {
        spki_i: [u8; SPKI_LEN],
        spki_r: [u8; SPKI_LEN],
        nonce_i: [u8; NONCE_LEN],
        nonce_r: [u8; NONCE_LEN],
        ek: [u8; EXPORTER_LEN],
    },
    /// SHA-256 of a byte string (the known-answer input is in the sample).
    Sha256 { input: Vec<u8> },
}

fn pairing_bytes(
    spki_i: &[u8; SPKI_LEN],
    spki_r: &[u8; SPKI_LEN],
    ni: &[u8; 32],
    nr: &[u8; 32],
    ek: &[u8; 32],
) -> Vec<u8> {
    let commit = sha256(&CommitmentInput { spki_i, nonce_i: ni }.preimage());
    let s = sha256(&SasInput { ek, spki_i, spki_r, nonce_i: ni, nonce_r: nr }.preimage());
    let sas = Sas::from_digest(&s);
    let mut out = commit.to_vec();
    out.extend_from_slice(&s);
    out.extend_from_slice(&sas.value().to_le_bytes());
    out.extend_from_slice(sas.to_string().as_bytes());
    out
}

impl Item {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        match self {
            Item::Identity { public_key } => {
                let spki = spki_from_public_key(public_key);
                let mut out = spki.to_vec();
                let id = device_id(&spki);
                out.extend_from_slice(&id);
                out.extend_from_slice(device_id_hex(&id).as_bytes());
                Ok(out)
            }
            Item::PairingHashes { spki_i, spki_r, nonce_i, nonce_r, ek } => {
                Ok(pairing_bytes(spki_i, spki_r, nonce_i, nonce_r, ek))
            }
            Item::Sha256 { input } => Ok(sha256(input).to_vec()),
        }
    }

    /// Reads `bytes` the way a receiver does. A device key is read back from its SPKI and the ID is verified; a
    /// hash cannot be read back, so the receiver recomputes it from the values it holds and the fixture must agree.
    pub fn decode_like(&self, bytes: &[u8]) -> Result<Item, String> {
        match self {
            Item::Identity { .. } => {
                if bytes.len() != SPKI_LEN + 16 + 32 {
                    return Err(format!("{} bytes", bytes.len()));
                }
                let spki = parse_spki(&bytes[..SPKI_LEN]).ok_or("not an Ed25519 SPKI")?;
                let id = device_id(&spki);
                if bytes[SPKI_LEN..SPKI_LEN + 16] != id {
                    return Err("the device ID is not the SHA-256 prefix of the SPKI".into());
                }
                if bytes[SPKI_LEN + 16..] != *device_id_hex(&id).as_bytes() {
                    return Err("the hex text is not the device ID".into());
                }
                Ok(Item::Identity { public_key: public_key_of(&spki) })
            }
            Item::PairingHashes { .. } | Item::Sha256 { .. } => {
                if self.encode()? == bytes {
                    Ok(self.clone())
                } else {
                    Err("the hashes differ".into())
                }
            }
        }
    }

    pub fn family(&self) -> &'static str {
        match self {
            Item::Identity { .. } => "identity",
            Item::PairingHashes { .. } | Item::Sha256 { .. } => "pairing-math",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Item::Identity { .. } => "SPKI, device ID, device ID hex".into(),
            Item::PairingHashes { .. } => "commit hash, SAS hash, six digits".into(),
            Item::Sha256 { input } => format!("SHA-256 of {} byte(s)", input.len()),
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

pub fn samples() -> Vec<Sample> {
    let mut v: Vec<Sample> = Vec::new();
    let mut add = |name: &str, item: Item| v.push(Sample { name: name.into(), item });

    add("identity_ascending_key", Item::Identity { public_key: ascending(0x01) });
    add("identity_zero_key", Item::Identity { public_key: [0; 32] });
    add("identity_ff_key", Item::Identity { public_key: [0xFF; 32] });
    // The public key of RFC 8032 section 7.1, test 1 (secret key 9d61b19d...): a real Ed25519 key.
    add(
        "identity_rfc8032_test1_key",
        Item::Identity {
            public_key: [
                0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a, 0x0e,
                0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
            ],
        },
    );

    add(
        "pairing_hashes_ascending",
        Item::PairingHashes {
            spki_i: ascending(0x80),
            spki_r: ascending(0xC0),
            nonce_i: ascending(0x70),
            nonce_r: ascending(0x50),
            ek: ascending(0x01),
        },
    );
    add(
        "pairing_hashes_zero",
        Item::PairingHashes {
            spki_i: [0; SPKI_LEN],
            spki_r: [0; SPKI_LEN],
            nonce_i: [0; NONCE_LEN],
            nonce_r: [0; NONCE_LEN],
            ek: [0; EXPORTER_LEN],
        },
    );
    add(
        "pairing_hashes_same_devices_swapped_roles",
        Item::PairingHashes {
            spki_i: ascending(0xC0),
            spki_r: ascending(0x80),
            nonce_i: ascending(0x50),
            nonce_r: ascending(0x70),
            ek: ascending(0x01),
        },
    );

    add("sha256_empty", Item::Sha256 { input: vec![] });
    add("sha256_abc", Item::Sha256 { input: b"abc".to_vec() });
    add("sha256_commit_label", Item::Sha256 { input: b"swapdesk commit v1".to_vec() });
    v
}
