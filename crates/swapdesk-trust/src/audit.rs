//! The audit log records of C5.1 and their file format: length-prefixed `AuditEntry` records, appended.
//! Files, rotation and the unauthenticated-entry rate limit belong to the service; this module fixes the bytes.

use std::sync::{Mutex, MutexGuard};

use swapdesk_proto::codec::Result;
use swapdesk_proto::{Action, CodecError, Desktop, ErrorCode, Feature, GrantMode, PermissionSet, Reader, Wire, Writer};

use crate::ids::DeviceId;
use crate::records::UserRef;

/// Largest encoded entry accepted when reading a log (the biggest real one is under 200 bytes).
pub const MAX_ENTRY_LEN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Incoming,
    Outgoing,
}

impl Wire for Direction {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_u8(match self {
            Self::Incoming => 0,
            Self::Outgoing => 1,
        });
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        match r.u8()? {
            0 => Ok(Self::Incoming),
            1 => Ok(Self::Outgoing),
            tag => Err(CodecError::UnknownTag { what: "Direction", tag: u64::from(tag) }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRef {
    Known(DeviceId),
    Unknown { addr: [u8; 4], device_id: Option<DeviceId> },
}

impl Wire for DeviceRef {
    const MIN_SIZE: usize = 17;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        match self {
            Self::Known(id) => {
                w.put_u8(0);
                id.put(w, 0)
            }
            Self::Unknown { addr, device_id } => {
                w.put_u8(1);
                addr.put(w, 0)?;
                device_id.put(w, 0)
            }
        }
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        match r.u8()? {
            0 => Ok(Self::Known(r.array()?)),
            1 => Ok(Self::Unknown { addr: r.array()?, device_id: <Option<DeviceId>>::get(r, 0)? }),
            tag => Err(CodecError::UnknownTag { what: "DeviceRef", tag: u64::from(tag) }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    Paired,
    PairFailed(ErrorCode),
    PairingLocked,
    UnknownDeviceRejected,
    Suppressed {
        count: u32,
        kind: u8,
    },
    SessionStarted {
        direction: Direction,
        mode: GrantMode,
        granted: PermissionSet,
        desktop: Desktop,
    },
    SessionEnded(ErrorCode),
    Swapped,
    ConsentAsked {
        feature: Feature,
        answer: ErrorCode,
    },
    FilesSent {
        count: u32,
        bytes: u64,
    },
    FilesReceived {
        count: u32,
        bytes: u64,
    },
    TerminalOpened {
        admin: bool,
    },
    ActionRun(Action),
    GrantsChanged {
        from: PermissionSet,
        to: PermissionSet,
        by: UserRef,
    },
    Forgotten,
    AddressBlocked([u8; 4]),
    /// Addition to C5.1 (DECISIONS: re-pairing logs "chave substituída"): the ceremony replaced this device's key.
    KeyReplaced {
        old: DeviceId,
    },
    /// Addition to C5.1 (the "`PairFailed` before step 2" of its rate limit): a pairing attempt that ended before the
    /// responder committed to a nonce, so before any failure was counted (busy, no pairing mode, a bad first message,
    /// a stall). It is the kind an unauthenticated peer can produce at will, and the one the audit rate limit covers;
    /// `PairFailed` is reserved for attempts that reached step 2.
    PairRefused(ErrorCode),
    /// ACCOUNTS.md 14.2, tag 18: the session of the entry before it was elevated (`UNATTENDED` and a stored
    /// `ADMIN_TERMINAL` added to it). Written only to the per-account logs, which 1.0 never reads.
    Elevated,
    /// FILES-UI.md 8.3, tag 19: one change message of the explorer done on this computer's files, counts and kinds
    /// only (never a name or path). `op`: 0 new folder, 1 rename, 2 move, 3 to the bin. `count`: the names done.
    /// `area`: 0 the home folder, 1 elsewhere on the system volume, 2 another volume.
    FilesChanged {
        op: u8,
        count: u32,
        area: u8,
    },
    /// FILES-UI.md 8.3, tag 20: the viewer reached a new kind of volume (bit 0 the system volume outside the home
    /// folder, 1 fixed, 2 removable, 3 network, 4 optical or image); `kinds` holds every bit reached so far in the
    /// session.
    FilesReach {
        kinds: u8,
    },
}

impl Wire for AuditEvent {
    const MIN_SIZE: usize = 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        match self {
            Self::Paired => w.put_u8(0),
            Self::PairFailed(c) => {
                w.put_u8(1);
                c.put(w, 0)?;
            }
            Self::PairingLocked => w.put_u8(2),
            Self::UnknownDeviceRejected => w.put_u8(3),
            Self::Suppressed { count, kind } => {
                w.put_u8(4);
                w.put_u32(*count);
                w.put_u8(*kind);
            }
            Self::SessionStarted { direction, mode, granted, desktop } => {
                w.put_u8(5);
                direction.put(w, 0)?;
                mode.put(w, 0)?;
                granted.put(w, 0)?;
                desktop.put(w, 0)?;
            }
            Self::SessionEnded(c) => {
                w.put_u8(6);
                c.put(w, 0)?;
            }
            Self::Swapped => w.put_u8(7),
            Self::ConsentAsked { feature, answer } => {
                w.put_u8(8);
                feature.put(w, 0)?;
                answer.put(w, 0)?;
            }
            Self::FilesSent { count, bytes } => {
                w.put_u8(9);
                w.put_u32(*count);
                w.put_u64(*bytes);
            }
            Self::FilesReceived { count, bytes } => {
                w.put_u8(10);
                w.put_u32(*count);
                w.put_u64(*bytes);
            }
            Self::TerminalOpened { admin } => {
                w.put_u8(11);
                w.put_bool(*admin);
            }
            Self::ActionRun(a) => {
                w.put_u8(12);
                a.put(w, 0)?;
            }
            Self::GrantsChanged { from, to, by } => {
                w.put_u8(13);
                from.put(w, 0)?;
                to.put(w, 0)?;
                by.put(w, 0)?;
            }
            Self::Forgotten => w.put_u8(14),
            Self::AddressBlocked(addr) => {
                w.put_u8(15);
                addr.put(w, 0)?;
            }
            Self::KeyReplaced { old } => {
                w.put_u8(16);
                old.put(w, 0)?;
            }
            Self::PairRefused(c) => {
                w.put_u8(17);
                c.put(w, 0)?;
            }
            Self::Elevated => w.put_u8(18),
            Self::FilesChanged { op, count, area } => {
                w.put_u8(19);
                w.put_u8(*op);
                w.put_u32(*count);
                w.put_u8(*area);
            }
            Self::FilesReach { kinds } => {
                w.put_u8(20);
                w.put_u8(*kinds);
            }
        }
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        Ok(match r.u8()? {
            0 => Self::Paired,
            1 => Self::PairFailed(ErrorCode::get(r, 0)?),
            2 => Self::PairingLocked,
            3 => Self::UnknownDeviceRejected,
            4 => Self::Suppressed { count: r.u32()?, kind: r.u8()? },
            5 => Self::SessionStarted {
                direction: Direction::get(r, 0)?,
                mode: GrantMode::get(r, 0)?,
                granted: PermissionSet::get(r, 0)?,
                desktop: Desktop::get(r, 0)?,
            },
            6 => Self::SessionEnded(ErrorCode::get(r, 0)?),
            7 => Self::Swapped,
            8 => Self::ConsentAsked { feature: Feature::get(r, 0)?, answer: ErrorCode::get(r, 0)? },
            9 => Self::FilesSent { count: r.u32()?, bytes: r.u64()? },
            10 => Self::FilesReceived { count: r.u32()?, bytes: r.u64()? },
            11 => Self::TerminalOpened { admin: r.bool()? },
            12 => Self::ActionRun(Action::get(r, 0)?),
            13 => Self::GrantsChanged {
                from: PermissionSet::get(r, 0)?,
                to: PermissionSet::get(r, 0)?,
                by: UserRef::get(r, 0)?,
            },
            14 => Self::Forgotten,
            15 => Self::AddressBlocked(r.array()?),
            16 => Self::KeyReplaced { old: r.array()? },
            17 => Self::PairRefused(ErrorCode::get(r, 0)?),
            18 => Self::Elevated,
            19 => Self::FilesChanged { op: r.u8()?, count: r.u32()?, area: r.u8()? },
            20 => Self::FilesReach { kinds: r.u8()? },
            tag => return Err(CodecError::UnknownTag { what: "AuditEvent", tag: u64::from(tag) }),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub time_ms: u64,
    pub device: DeviceRef,
    pub event: AuditEvent,
    pub duration_s: Option<u32>,
}

impl Wire for AuditEntry {
    const MIN_SIZE: usize = 8 + DeviceRef::MIN_SIZE + 1 + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> Result<()> {
        w.put_u64(self.time_ms);
        self.device.put(w, 0)?;
        self.event.put(w, 0)?;
        self.duration_s.put(w, 0)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> Result<Self> {
        Ok(Self {
            time_ms: r.u64()?,
            device: DeviceRef::get(r, 0)?,
            event: AuditEvent::get(r, 0)?,
            duration_s: <Option<u32>>::get(r, 0)?,
        })
    }
}

impl AuditEntry {
    /// One record of the log file: `varint length`, then the entry.
    pub fn encode_record(&self) -> Result<Vec<u8>> {
        let body = self.encode()?;
        let mut w = Writer::new();
        w.put_varint(body.len() as u64)?;
        w.put_bytes(&body);
        Ok(w.into_bytes())
    }
}

/// What reading a log file gave: the entries, and whether the file ended in the middle of a record (a write the
/// crash cut off; the caller truncates the file to `valid_len`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRead {
    pub entries: Vec<AuditEntry>,
    pub valid_len: usize,
    pub truncated_tail: bool,
}

/// Reads a whole log file. A record that is complete but does not decode is an error (the file is damaged); an
/// incomplete last record is reported, not an error.
pub fn decode_log(bytes: &[u8]) -> Result<LogRead> {
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let mut r = Reader::new(&bytes[pos..]);
        let len = match r.varint() {
            Ok(len) => len,
            Err(CodecError::Truncated) => return Ok(LogRead { entries, valid_len: pos, truncated_tail: true }),
            Err(e) => return Err(e),
        };
        if len > MAX_ENTRY_LEN as u64 {
            return Err(CodecError::OverLimit { what: "audit entry length", limit: MAX_ENTRY_LEN as u64 });
        }
        let body = match r.take(len as usize) {
            Ok(b) => b,
            Err(CodecError::Truncated) => return Ok(LogRead { entries, valid_len: pos, truncated_tail: true }),
            Err(e) => return Err(e),
        };
        entries.push(AuditEntry::decode(body)?);
        pos += r.position();
    }
    Ok(LogRead { entries, valid_len: pos, truncated_tail: false })
}

/// Where the pairing code writes what happened. The service appends to the C5 files; tests use [`MemoryAudit`].
pub trait AuditSink: Send + Sync {
    fn append(&self, entry: AuditEntry);
}

/// An in-memory log in the file format.
#[derive(Default)]
pub struct MemoryAudit(Mutex<Vec<u8>>);

impl MemoryAudit {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> MutexGuard<'_, Vec<u8>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.guard().clone()
    }

    pub fn entries(&self) -> Vec<AuditEntry> {
        decode_log(&self.guard()).map(|r| r.entries).unwrap_or_default()
    }
}

impl AuditSink for MemoryAudit {
    fn append(&self, entry: AuditEntry) {
        if let Ok(rec) = entry.encode_record() {
            self.guard().extend_from_slice(&rec);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_event() -> Vec<AuditEvent> {
        vec![
            AuditEvent::Paired,
            AuditEvent::PairFailed(ErrorCode::SasRejected),
            AuditEvent::PairingLocked,
            AuditEvent::UnknownDeviceRejected,
            AuditEvent::Suppressed { count: 7, kind: 3 },
            AuditEvent::SessionStarted {
                direction: Direction::Incoming,
                mode: GrantMode::Prompted,
                granted: PermissionSet::CONTROL,
                desktop: Desktop::Default,
            },
            AuditEvent::SessionEnded(ErrorCode::NoError),
            AuditEvent::Swapped,
            AuditEvent::ConsentAsked { feature: Feature::Files, answer: ErrorCode::Declined },
            AuditEvent::FilesSent { count: 2, bytes: 1 << 40 },
            AuditEvent::FilesReceived { count: 1, bytes: 5 },
            AuditEvent::TerminalOpened { admin: true },
            AuditEvent::ActionRun(Action::Lock),
            AuditEvent::GrantsChanged {
                from: PermissionSet::EMPTY,
                to: PermissionSet::UNATTENDED,
                by: UserRef { id: "501".into(), name: "alex".into() },
            },
            AuditEvent::Forgotten,
            AuditEvent::AddressBlocked([10, 0, 0, 5]),
            AuditEvent::KeyReplaced { old: [4; 16] },
            AuditEvent::PairRefused(ErrorCode::Busy),
        ]
    }

    #[test]
    fn every_event_round_trips_through_the_log_format() {
        let mut file = Vec::new();
        let mut want = Vec::new();
        for (i, event) in every_event().into_iter().enumerate() {
            let device = if i % 2 == 0 {
                DeviceRef::Known([i as u8; 16])
            } else {
                DeviceRef::Unknown { addr: [192, 168, 1, i as u8], device_id: (i % 3 == 0).then_some([9; 16]) }
            };
            let e =
                AuditEntry { time_ms: 1_000 + i as u64, device, event, duration_s: (i % 2 == 1).then_some(i as u32) };
            file.extend_from_slice(&e.encode_record().unwrap());
            want.push(e);
        }
        let read = decode_log(&file).unwrap();
        assert_eq!(read.entries, want);
        assert_eq!(read.valid_len, file.len());
        assert!(!read.truncated_tail);
    }

    /// ACCOUNTS.md 15 S2: tag 18 `Elevated` reads back, and a log of 1.0 events (none carries it) still reads as it
    /// did.
    #[test]
    fn audit_tag_18_round_trips_and_a_1_0_log_still_reads() {
        let elevated =
            AuditEntry { time_ms: 7, device: DeviceRef::Known([3; 16]), event: AuditEvent::Elevated, duration_s: None };
        let rec = elevated.encode_record().unwrap();
        assert!(rec.contains(&18), "the tag is 18");
        let mut file = Vec::new();
        let old: Vec<AuditEntry> = every_event()
            .into_iter()
            .map(|event| AuditEntry { time_ms: 1, device: DeviceRef::Known([1; 16]), event, duration_s: None })
            .collect();
        for e in &old {
            file.extend_from_slice(&e.encode_record().unwrap());
        }
        assert_eq!(decode_log(&file).unwrap().entries, old, "a 1.0 log");
        file.extend_from_slice(&rec);
        let read = decode_log(&file).unwrap();
        assert_eq!(read.entries.last(), Some(&elevated));
        assert_eq!(read.entries.len(), old.len() + 1);
    }

    /// FILES-UI.md 8.3: tags 19 and 20 carry counts and kinds only, and a log without them still reads.
    #[test]
    fn audit_tags_19_and_20_carry_counts_and_kinds_only() {
        let changed = AuditEvent::FilesChanged { op: 2, count: 5, area: 1 };
        let reach = AuditEvent::FilesReach { kinds: 0b1_0100 };
        let mut file = Vec::new();
        for event in [changed.clone(), reach.clone()] {
            let e = AuditEntry { time_ms: 9, device: DeviceRef::Known([2; 16]), event, duration_s: None };
            file.extend_from_slice(&e.encode_record().unwrap());
        }
        let read = decode_log(&file).unwrap();
        assert_eq!(read.entries.iter().map(|e| e.event.clone()).collect::<Vec<_>>(), [changed, reach]);
        let tags: Vec<u8> = read.entries.iter().map(|e| e.event.encode().unwrap()[0]).collect();
        assert_eq!(tags, [19, 20]);
        // Nothing but the kind byte, the count and the area byte travels: a record has no room for a name.
        assert_eq!(AuditEvent::FilesChanged { op: 0, count: 0, area: 0 }.encode().unwrap().len(), 1 + 1 + 4 + 1);
        assert_eq!(AuditEvent::FilesReach { kinds: 0 }.encode().unwrap().len(), 2);
    }

    #[test]
    fn a_cut_last_record_is_reported_and_a_damaged_one_is_an_error() {
        let e =
            AuditEntry { time_ms: 1, device: DeviceRef::Known([1; 16]), event: AuditEvent::Paired, duration_s: None };
        let rec = e.encode_record().unwrap();
        let mut file = rec.clone();
        file.extend_from_slice(&rec[..rec.len() - 3]);
        let read = decode_log(&file).unwrap();
        assert_eq!(read.entries.len(), 1);
        assert!(read.truncated_tail);
        assert_eq!(read.valid_len, rec.len());

        let mut bad = rec.clone();
        let last = bad.len() - 1;
        bad[last] = 7; // duration_s option tag
        assert!(decode_log(&bad).is_err());
        assert!(decode_log(&[0xFF; 9]).is_err());
        assert!(decode_log(&[0x44, 0x01]).is_err(), "declared length over the limit");
    }
}
