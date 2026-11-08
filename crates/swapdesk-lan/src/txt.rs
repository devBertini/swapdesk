//! The DNS-SD TXT record of `_swapdesk._udp` (PROTOCOL.md 2.1 and 3): `name`, `os`, `fp` (the device ID as 32 lowercase
//! hex characters) and `pair=1` while the machine is in pairing mode. Anyone on the LAN can read it, so it holds no
//! secret, and a record read from the network is checked strictly: it is attacker-controlled input.

use std::fmt;

use swapdesk_proto::Os;

/// `name` is a `str(64)` of the protocol: at most 64 bytes of UTF-8.
pub const MAX_NAME_BYTES: usize = 64;
/// A TXT string is one length byte plus its content (RFC 6763 6.1).
pub const MAX_ENTRY_BYTES: usize = 255;
/// More entries than this in a received record is refused (ours has 3 or 4).
pub const MAX_ENTRIES: usize = 16;
pub const FINGERPRINT_BYTES: usize = 16;

pub const KEY_NAME: &str = "name";
pub const KEY_OS: &str = "os";
pub const KEY_FP: &str = "fp";
pub const KEY_PAIR: &str = "pair";

/// A received value, when its key was present.
type Value<'a> = Option<&'a [u8]>;

/// What a device announces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Txt {
    pub name: String,
    pub os: Os,
    /// The device ID: the first 16 bytes of SHA-256(SPKI).
    pub fingerprint: [u8; FINGERPRINT_BYTES],
    pub pairing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxtError {
    TooManyEntries,
    EntryTooLong(String),
    DuplicateKey(String),
    Missing(&'static str),
    BadName,
    BadOs,
    BadFingerprint,
    BadPairFlag,
}

impl fmt::Display for TxtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEntries => write!(f, "more than {MAX_ENTRIES} TXT entries"),
            Self::EntryTooLong(k) => write!(f, "TXT entry {k:?} is longer than {MAX_ENTRY_BYTES} bytes"),
            Self::DuplicateKey(k) => write!(f, "TXT key {k:?} appears twice"),
            Self::Missing(k) => write!(f, "TXT key {k:?} is missing"),
            Self::BadName => {
                write!(f, "TXT name is empty, over {MAX_NAME_BYTES} bytes, not UTF-8 or has control characters")
            }
            Self::BadOs => f.write_str("TXT os is neither windows nor macos"),
            Self::BadFingerprint => f.write_str("TXT fp is not 32 lowercase hex characters"),
            Self::BadPairFlag => f.write_str("TXT pair is not 1"),
        }
    }
}

impl std::error::Error for TxtError {}

pub fn os_str(os: Os) -> &'static str {
    match os {
        Os::Windows => "windows",
        Os::MacOs => "macos",
    }
}

pub fn fingerprint_hex(fp: &[u8; FINGERPRINT_BYTES]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_fingerprint(s: &[u8]) -> Option<[u8; FINGERPRINT_BYTES]> {
    if s.len() != FINGERPRINT_BYTES * 2 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; FINGERPRINT_BYTES];
    for (i, b) in out.iter_mut().enumerate() {
        *b = nibble(s[i * 2])? << 4 | nibble(s[i * 2 + 1])?;
    }
    Some(out)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
}

impl Txt {
    /// The properties to publish. Fails when the name breaks its limits (the caller shortens a machine name before).
    pub fn encode(&self) -> Result<Vec<(String, String)>, TxtError> {
        if !valid_name(&self.name) {
            return Err(TxtError::BadName);
        }
        let mut v = vec![
            (KEY_NAME.to_string(), self.name.clone()),
            (KEY_OS.to_string(), os_str(self.os).to_string()),
            (KEY_FP.to_string(), fingerprint_hex(&self.fingerprint)),
        ];
        if self.pairing {
            v.push((KEY_PAIR.to_string(), "1".to_string()));
        }
        for (k, val) in &v {
            if k.len() + 1 + val.len() > MAX_ENTRY_BYTES {
                return Err(TxtError::EntryTooLong(k.clone()));
            }
        }
        Ok(v)
    }

    /// Reads a received record. Unknown keys are ignored (a later minor may add some); a duplicate key, a missing
    /// required key or any bad value refuses the whole record.
    pub fn decode(entries: &[(&str, &[u8])]) -> Result<Self, TxtError> {
        if entries.len() > MAX_ENTRIES {
            return Err(TxtError::TooManyEntries);
        }
        let mut seen: Vec<String> = Vec::with_capacity(entries.len());
        let (mut name, mut os, mut fp, mut pair): (Value, Value, Value, Value) = (None, None, None, None);
        for (key, val) in entries {
            if key.len() + 1 + val.len() > MAX_ENTRY_BYTES {
                return Err(TxtError::EntryTooLong((*key).to_string()));
            }
            // RFC 6763 6.4: keys are case-insensitive.
            let lower = key.to_ascii_lowercase();
            if seen.contains(&lower) {
                return Err(TxtError::DuplicateKey(lower));
            }
            match lower.as_str() {
                KEY_NAME => name = Some(val),
                KEY_OS => os = Some(val),
                KEY_FP => fp = Some(val),
                KEY_PAIR => pair = Some(val),
                _ => {}
            }
            seen.push(lower);
        }
        let name = std::str::from_utf8(name.ok_or(TxtError::Missing(KEY_NAME))?).map_err(|_| TxtError::BadName)?;
        if !valid_name(name) {
            return Err(TxtError::BadName);
        }
        let os = match os.ok_or(TxtError::Missing(KEY_OS))? {
            b"windows" => Os::Windows,
            b"macos" => Os::MacOs,
            _ => return Err(TxtError::BadOs),
        };
        let fingerprint = parse_fingerprint(fp.ok_or(TxtError::Missing(KEY_FP))?).ok_or(TxtError::BadFingerprint)?;
        let pairing = match pair {
            None => false,
            Some(b"1") => true,
            Some(_) => return Err(TxtError::BadPairFlag),
        };
        Ok(Self { name: name.to_string(), os, fingerprint, pairing })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Txt {
        Txt { name: "Notebook da sala".into(), os: Os::Windows, fingerprint: [0xAB; 16], pairing: false }
    }

    fn as_entries(v: &[(String, String)]) -> Vec<(&str, &[u8])> {
        v.iter().map(|(k, val)| (k.as_str(), val.as_bytes())).collect()
    }

    #[test]
    fn round_trips_with_and_without_the_pair_flag() {
        for pairing in [false, true] {
            for os in [Os::Windows, Os::MacOs] {
                let t = Txt { pairing, os, ..sample() };
                let enc = t.encode().unwrap();
                assert_eq!(enc.len(), if pairing { 4 } else { 3 });
                assert_eq!(Txt::decode(&as_entries(&enc)).unwrap(), t);
            }
        }
    }

    #[test]
    fn the_fingerprint_is_32_lowercase_hex_characters() {
        let enc = sample().encode().unwrap();
        assert_eq!(enc[2], ("fp".to_string(), "ab".repeat(16)));
        let mut fp = [0u8; 16];
        fp[0] = 0x01;
        fp[15] = 0xF0;
        assert_eq!(fingerprint_hex(&fp), format!("01{}f0", "00".repeat(14)));
    }

    #[test]
    fn the_record_holds_nothing_but_name_os_fp_and_pair() {
        let enc = Txt { pairing: true, ..sample() }.encode().unwrap();
        let keys: Vec<&str> = enc.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["name", "os", "fp", "pair"]);
    }

    #[test]
    fn name_limits() {
        let max = "n".repeat(MAX_NAME_BYTES);
        assert!(Txt { name: max.clone(), ..sample() }.encode().is_ok());
        assert_eq!(Txt { name: format!("{max}x"), ..sample() }.encode(), Err(TxtError::BadName));
        assert_eq!(Txt { name: String::new(), ..sample() }.encode(), Err(TxtError::BadName));
        assert_eq!(Txt { name: "a\0b".into(), ..sample() }.encode(), Err(TxtError::BadName));
        assert_eq!(Txt { name: "a\nb".into(), ..sample() }.encode(), Err(TxtError::BadName));
        // Bytes, not characters: 33 two-byte characters are 66 bytes.
        assert_eq!(Txt { name: "é".repeat(33), ..sample() }.encode(), Err(TxtError::BadName));
        assert!(Txt { name: "é".repeat(32), ..sample() }.encode().is_ok());
    }

    #[test]
    fn decode_refuses_bad_values() {
        let fp = "ab".repeat(16);
        let ok = |name: &'static [u8], os: &'static [u8], fp: &'static [u8]| [("name", name), ("os", os), ("fp", fp)];
        assert!(Txt::decode(&ok(b"x", b"macos", fp.as_bytes().to_vec().leak())).is_ok());
        assert_eq!(Txt::decode(&ok(b"x", b"linux", b"")), Err(TxtError::BadOs));
        assert_eq!(Txt::decode(&ok(b"", b"macos", b"")), Err(TxtError::BadName));
        assert_eq!(Txt::decode(&ok(b"\xff\xfe", b"macos", b"")), Err(TxtError::BadName));
        assert_eq!(Txt::decode(&ok(&[b'a'; 65], b"macos", b"")), Err(TxtError::BadName));
        for bad in ["", "ab", &"AB".repeat(16), &"ab".repeat(17), &"zz".repeat(16), &"a".repeat(31)] {
            let leaked: &'static [u8] = bad.to_string().leak().as_bytes();
            assert_eq!(Txt::decode(&ok(b"x", b"macos", leaked)), Err(TxtError::BadFingerprint), "{bad:?}");
        }
    }

    #[test]
    fn decode_refuses_missing_duplicate_oversized_and_too_many_entries() {
        let fp = "ab".repeat(16);
        let base: Vec<(&str, &[u8])> = vec![("name", b"x"), ("os", b"macos"), ("fp", fp.as_bytes())];
        assert!(Txt::decode(&base).is_ok());
        assert_eq!(Txt::decode(&base[1..]), Err(TxtError::Missing("name")));
        assert_eq!(Txt::decode(&[base[0], base[2]]), Err(TxtError::Missing("os")));
        assert_eq!(Txt::decode(&base[..2]), Err(TxtError::Missing("fp")));

        let mut dup = base.clone();
        dup.push(("NAME", b"y"));
        assert_eq!(Txt::decode(&dup), Err(TxtError::DuplicateKey("name".into())));

        let big = vec![b'a'; 252];
        let mut long = base.clone();
        long[0] = ("name", &big); // "name" + "=" + 252 = 257 bytes
        assert_eq!(Txt::decode(&long), Err(TxtError::EntryTooLong("name".into())));

        let mut many = base.clone();
        for _ in 0..MAX_ENTRIES {
            many.push(("x", b"1"));
        }
        assert_eq!(Txt::decode(&many), Err(TxtError::TooManyEntries));
    }

    #[test]
    fn decode_is_strict_about_the_pair_flag_and_ignores_unknown_keys() {
        let fp = "ab".repeat(16);
        let mut v: Vec<(&str, &[u8])> =
            vec![("name", b"x"), ("os", b"macos"), ("fp", fp.as_bytes()), ("future", b"anything")];
        assert!(!Txt::decode(&v).unwrap().pairing);
        v.push(("pair", b"1"));
        assert!(Txt::decode(&v).unwrap().pairing);
        v.pop();
        for bad in [&b"0"[..], b"", b"true", b"11"] {
            let mut w = v.clone();
            w.push(("pair", bad));
            assert_eq!(Txt::decode(&w), Err(TxtError::BadPairFlag), "{bad:?}");
        }
    }
}
