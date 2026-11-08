//! The parsed `keys.tsv` and its lookups.

use std::collections::HashMap;
use std::sync::OnceLock;

const DATA: &str = include_str!("../data/keys.tsv");

/// Flags column of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(u8);

impl Flags {
    pub const ISO_SWAP: Flags = Flags(1);
    pub const ALTGR: Flags = Flags(2);
    pub const NUMPAD: Flags = Flags(4);
    pub const MEDIA: Flags = Flags(8);
    pub const JIS: Flags = Flags(16);
    pub const ABNT2: Flags = Flags(32);

    pub fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// The physical keyboard of a Mac host. Only the ISO type changes codes: its two `iso_swap` keys exchange `kVK_ANSI_Grave`
/// and `kVK_ISO_Section` (C4 rule 2; the swap itself is UNPROVEN until probe P9 on the MacBook).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KbKind {
    Ansi,
    Iso,
    Jis,
}

impl KbKind {
    pub const ALL: [KbKind; 3] = [KbKind::Ansi, KbKind::Iso, KbKind::Jis];

    fn index(self) -> usize {
        match self {
            KbKind::Ansi => 0,
            KbKind::Iso => 1,
            KbKind::Jis => 2,
        }
    }
}

/// One row of `keys.tsv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub page: u16,
    pub usage: u16,
    pub name: &'static str,
    /// Set-1 make code; extended keys are `0xE0xx`.
    pub win_scan: Option<u16>,
    pub win_vk: Option<u16>,
    /// `kVK_*` of an ANSI Mac keyboard.
    pub mac_kvk: Option<u16>,
    pub flags: Flags,
    pub note: &'static str,
}

struct Tables {
    rows: Vec<KeyInfo>,
    by_hid: HashMap<(u16, u16), usize>,
    by_win: HashMap<u16, usize>,
    /// Per `KbKind`: the kVK of each row (after the ISO swap).
    mac_of_row: [Vec<Option<u16>>; 3],
    by_mac: [HashMap<u16, usize>; 3],
}

fn hex(field: &str, line: usize, what: &str) -> Option<u16> {
    if field == "-" {
        return None;
    }
    let digits =
        field.strip_prefix("0x").unwrap_or_else(|| panic!("keys.tsv line {line}: {what} `{field}` is not hex"));
    Some(
        u16::from_str_radix(digits, 16).unwrap_or_else(|_| panic!("keys.tsv line {line}: {what} `{field}` is not hex")),
    )
}

fn parse_flags(field: &str, line: usize) -> Flags {
    let mut bits = 0;
    if field != "-" {
        for f in field.split(',') {
            bits |= match f {
                "iso_swap" => Flags::ISO_SWAP.0,
                "altgr" => Flags::ALTGR.0,
                "numpad" => Flags::NUMPAD.0,
                "media" => Flags::MEDIA.0,
                "jis" => Flags::JIS.0,
                "abnt2" => Flags::ABNT2.0,
                other => panic!("keys.tsv line {line}: unknown flag `{other}`"),
            };
        }
    }
    Flags(bits)
}

fn parse(data: &'static str) -> Tables {
    let mut rows = Vec::new();
    let mut header_seen = false;
    for (n, line) in data.lines().enumerate() {
        let line_no = n + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !header_seen {
            header_seen = true;
            assert!(line.starts_with("page\tusage\tname\t"), "keys.tsv line {line_no}: header expected");
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        assert!(f.len() == 8, "keys.tsv line {line_no}: 8 columns expected, found {}", f.len());
        rows.push(KeyInfo {
            page: hex(f[0], line_no, "page").unwrap_or(0),
            usage: hex(f[1], line_no, "usage").unwrap_or(0),
            name: f[2],
            win_scan: hex(f[3], line_no, "win_scan"),
            win_vk: hex(f[4], line_no, "win_vk"),
            mac_kvk: hex(f[5], line_no, "mac_kvk"),
            flags: parse_flags(f[6], line_no),
            note: f[7],
        });
    }
    let mut by_hid = HashMap::new();
    let mut by_win = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        by_hid.insert((r.page, r.usage), i);
        if let Some(s) = r.win_scan {
            by_win.entry(s).or_insert(i);
        }
    }
    let swap_rows: Vec<usize> =
        rows.iter().enumerate().filter(|(_, r)| r.flags.contains(Flags::ISO_SWAP)).map(|(i, _)| i).collect();
    let mut mac_of_row: [Vec<Option<u16>>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for kind in KbKind::ALL {
        let mut col: Vec<Option<u16>> = rows.iter().map(|r| r.mac_kvk).collect();
        if kind == KbKind::Iso && swap_rows.len() == 2 {
            col.swap(swap_rows[0], swap_rows[1]);
        }
        mac_of_row[kind.index()] = col;
    }
    let mut by_mac: [HashMap<u16, usize>; 3] = [HashMap::new(), HashMap::new(), HashMap::new()];
    for kind in KbKind::ALL {
        for (i, kvk) in mac_of_row[kind.index()].iter().enumerate() {
            if let Some(k) = kvk {
                by_mac[kind.index()].entry(*k).or_insert(i);
            }
        }
    }
    Tables { rows, by_hid, by_win, mac_of_row, by_mac }
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| parse(DATA))
}

/// Every row of the table, in file order.
pub fn entries() -> &'static [KeyInfo] {
    &tables().rows
}

/// Keyboard-page volume keys (`0x07` `0x7F`, `0x80`, `0x81`) are the consumer-page Mute, Volume Up and Volume Down:
/// every lookup reads them there, so one key has one identity on the wire.
pub fn canonical(page: u16, usage: u16) -> (u16, u16) {
    match (page, usage) {
        (0x07, 0x7F) => (0x0C, 0xE2),
        (0x07, 0x80) => (0x0C, 0xE9),
        (0x07, 0x81) => (0x0C, 0xEA),
        other => other,
    }
}

pub fn key_info(page: u16, usage: u16) -> Option<&'static KeyInfo> {
    let t = tables();
    let key = canonical(page, usage);
    t.by_hid.get(&key).map(|&i| &t.rows[i])
}

/// Windows set-1 scan code of a HID key (`0xE0xx` when extended).
pub fn hid_to_win(page: u16, usage: u16) -> Option<u16> {
    key_info(page, usage).and_then(|r| r.win_scan)
}

/// The HID key of a Windows scan code (`0xE0xx` when extended).
pub fn win_to_hid(scan: u16) -> Option<(u16, u16)> {
    let t = tables();
    t.by_win.get(&scan).map(|&i| (t.rows[i].page, t.rows[i].usage))
}

/// Windows virtual-key code of a HID key (informative).
pub fn win_vk(page: u16, usage: u16) -> Option<u16> {
    key_info(page, usage).and_then(|r| r.win_vk)
}

/// `kVK_*` of a HID key on a Mac with this keyboard type.
pub fn hid_to_mac(page: u16, usage: u16, kb: KbKind) -> Option<u16> {
    let t = tables();
    let i = *t.by_hid.get(&canonical(page, usage))?;
    t.mac_of_row[kb.index()][i]
}

/// The HID key of a `kVK_*` code from a Mac with this keyboard type.
pub fn mac_to_hid(kvk: u16, kb: KbKind) -> Option<(u16, u16)> {
    let t = tables();
    t.by_mac[kb.index()].get(&kvk).map(|&i| (t.rows[i].page, t.rows[i].usage))
}

/// Splits a table scan code into the make code and the extended (`E0`) flag.
pub fn scan_parts(scan: u16) -> (u8, bool) {
    ((scan & 0xFF) as u8, scan & 0xFF00 == 0xE000)
}
