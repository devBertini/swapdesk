//! What the hub keeps on disk besides the trust store: the machine settings record (`settings.bin`, C5.1) and the
//! audit log (`audit-sessions.log` and `audit-network.log`, C5.2), both over plain traits so tests run in memory.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use swapdesk_platform::{Level, SecretStore, TraceSink};
use swapdesk_proto::codec::Result as CResult;
use swapdesk_proto::{CodecError, Reader, Wire, Writer};
use swapdesk_trust::records::FORMAT;
use swapdesk_trust::{AuditEntry, AuditEvent, AuditSink, DeviceRef, MemoryAudit, decode_log};

use crate::service::{DEFAULT_PAIR_PORT, DEFAULT_SESSION_PORT};

/// Name of the settings blob in the `SecretStore` (`settings.bin`, C5.2).
pub const SETTINGS_NAME: &str = "settings.bin";
/// Longest machine name (C5.1).
pub const MAX_MACHINE_NAME: usize = 64;

/// `MachineSettings` of C5.1: the fields the service owns. The per-user fields of C6 `Settings` live in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSettings {
    pub name: String,
    pub service_enabled: bool,
    pub lock_on_end: bool,
    pub keep_awake: bool,
    pub port: u16,
    pub pair_port: u16,
}

impl MachineSettings {
    /// What a machine starts with: the given name, the service on, no lock on end, awake during a session.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            service_enabled: true,
            lock_on_end: false,
            keep_awake: true,
            port: DEFAULT_SESSION_PORT,
            pair_port: DEFAULT_PAIR_PORT,
        }
    }
}

impl Wire for MachineSettings {
    const MIN_SIZE: usize = 2 + 1 + 3 + 4;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        w.put_u16(FORMAT);
        w.put_str(&self.name, MAX_MACHINE_NAME)?;
        w.put_bool(self.service_enabled);
        w.put_bool(self.lock_on_end);
        w.put_bool(self.keep_awake);
        w.put_u16(self.port);
        w.put_u16(self.pair_port);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        if r.u16()? != FORMAT {
            return Err(CodecError::InvalidValue("settings format"));
        }
        let s = Self {
            name: r.str(MAX_MACHINE_NAME)?,
            service_enabled: r.bool()?,
            lock_on_end: r.bool()?,
            keep_awake: r.bool()?,
            port: r.u16()?,
            pair_port: r.u16()?,
        };
        if !valid_name(&s.name) {
            return Err(CodecError::InvalidValue("machine name"));
        }
        Ok(s)
    }
}

/// A machine name the UI may set: not blank, at most 64 bytes, no control characters.
pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= MAX_MACHINE_NAME && !name.chars().any(char::is_control)
}

/// Loads `settings.bin`, or the defaults with `default_name` when there is none. A blob that does not decode is an
/// error: the service must not silently replace what it cannot read.
pub fn load_settings(secrets: &dyn SecretStore, default_name: &str) -> Result<MachineSettings, String> {
    match secrets.load(SETTINGS_NAME).map_err(|e| e.to_string())? {
        Some(bytes) => MachineSettings::decode(&bytes).map_err(|e| format!("settings.bin: {e}")),
        None => Ok(MachineSettings::new(default_name)),
    }
}

/// Writes `settings.bin`.
pub fn store_settings(secrets: &dyn SecretStore, settings: &MachineSettings) -> Result<(), String> {
    let bytes = settings.encode().map_err(|e| e.to_string())?;
    secrets.store(SETTINGS_NAME, &bytes).map_err(|e| e.to_string())
}

// ---------- ACCOUNTS.md 14.2: what each account keeps (Windows) ----------

/// `accounts.bin`: the accounts of this machine and their port slots.
pub const ACCOUNTS_NAME: &str = "accounts.bin";
/// `account.bin` in an account's folder.
pub const ACCOUNT_SETTINGS_NAME: &str = "account.bin";
/// `peers.bin` in an account's folder.
pub const PEER_PORTS_NAME: &str = "peers.bin";
/// At most this many accounts (slots 0 to 31).
pub const MAX_ACCOUNTS: usize = 32;
/// At most this many last-seen ports per account.
pub const MAX_PEER_PORTS: usize = 64;
/// The longest SID (the `str(184)` of C2.4).
pub const MAX_SID: usize = 184;

/// One account of the machine: its SID, its slot (session port `47820 + 2k`, pairing port `47821 + 2k`), when it
/// was first seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountEntry {
    pub sid: String,
    pub slot: u8,
    pub created_ms: u64,
}

impl Wire for AccountEntry {
    const MIN_SIZE: usize = 1 + 1 + 8;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        w.put_str(&self.sid, MAX_SID)?;
        w.put_u8(self.slot);
        w.put_u64(self.created_ms);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        let e = Self { sid: r.str(MAX_SID)?, slot: r.u8()?, created_ms: r.u64()? };
        if usize::from(e.slot) >= MAX_ACCOUNTS {
            return Err(CodecError::InvalidValue("account slot"));
        }
        Ok(e)
    }
}

/// `AccountsIndex`: every account with a folder, each in a slot of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountsIndex {
    pub accounts: Vec<AccountEntry>,
}

impl Wire for AccountsIndex {
    const MIN_SIZE: usize = 2 + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        w.put_u16(FORMAT);
        self.accounts.put(w, MAX_ACCOUNTS)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        if r.u16()? != FORMAT {
            return Err(CodecError::InvalidValue("accounts format"));
        }
        let accounts = <Vec<AccountEntry>>::get(r, MAX_ACCOUNTS)?;
        let mut slots: Vec<u8> = accounts.iter().map(|a| a.slot).collect();
        let mut sids: Vec<&str> = accounts.iter().map(|a| a.sid.as_str()).collect();
        slots.sort_unstable();
        sids.sort_unstable();
        if slots.windows(2).any(|w| w[0] == w[1]) || sids.windows(2).any(|w| w[0] == w[1]) {
            return Err(CodecError::InvalidValue("a slot or an account twice"));
        }
        Ok(Self { accounts })
    }
}

/// `AccountSettings`: what each account sets for itself (the machine keeps the rest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSettings {
    /// The name peers see (`<computer> · <account>` by default, ACCOUNTS.md 7.2).
    pub name: String,
    /// "Receber conexões": the account's endpoint listens and its record is announced.
    pub receive: bool,
    pub lock_on_end: bool,
    pub keep_awake: bool,
}

impl AccountSettings {
    pub fn new(name: &str) -> Self {
        Self { name: name.into(), receive: true, lock_on_end: false, keep_awake: true }
    }
}

impl Wire for AccountSettings {
    const MIN_SIZE: usize = 2 + 1 + 3;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        w.put_u16(FORMAT);
        w.put_str(&self.name, MAX_MACHINE_NAME)?;
        w.put_bool(self.receive);
        w.put_bool(self.lock_on_end);
        w.put_bool(self.keep_awake);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        if r.u16()? != FORMAT {
            return Err(CodecError::InvalidValue("account settings format"));
        }
        let s =
            Self { name: r.str(MAX_MACHINE_NAME)?, receive: r.bool()?, lock_on_end: r.bool()?, keep_awake: r.bool()? };
        if !valid_name(&s.name) {
            return Err(CodecError::InvalidValue("account name"));
        }
        Ok(s)
    }
}

/// The session port a paired device was last seen on (ACCOUNTS.md 5.2), so a dial without discovery tries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPort {
    pub device_id: [u8; 16],
    pub session_port: u16,
    pub seen_ms: u64,
}

impl Wire for PeerPort {
    const MIN_SIZE: usize = 16 + 2 + 8;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        self.device_id.put(w, 0)?;
        w.put_u16(self.session_port);
        w.put_u64(self.seen_ms);
        Ok(())
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        Ok(Self { device_id: r.array()?, session_port: r.u16()?, seen_ms: r.u64()? })
    }
}

/// `PeerPorts`: one entry per paired device at most; an entry goes with its device on Forget.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerPorts {
    pub entries: Vec<PeerPort>,
}

impl Wire for PeerPorts {
    const MIN_SIZE: usize = 2 + 1;

    fn put(&self, w: &mut Writer, _lim: usize) -> CResult<()> {
        w.put_u16(FORMAT);
        self.entries.put(w, MAX_PEER_PORTS)
    }

    fn get(r: &mut Reader<'_>, _lim: usize) -> CResult<Self> {
        if r.u16()? != FORMAT {
            return Err(CodecError::InvalidValue("peer ports format"));
        }
        let entries = <Vec<PeerPort>>::get(r, MAX_PEER_PORTS)?;
        let mut ids: Vec<[u8; 16]> = entries.iter().map(|e| e.device_id).collect();
        ids.sort_unstable();
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(CodecError::InvalidValue("a device twice"));
        }
        Ok(Self { entries })
    }
}

/// Loads `peers.bin`; a missing or unreadable one is empty (it is only a hint for dialling).
pub fn load_peer_ports(secrets: &dyn SecretStore) -> PeerPorts {
    match secrets.load(PEER_PORTS_NAME) {
        Ok(Some(bytes)) => PeerPorts::decode(&bytes).unwrap_or_default(),
        _ => PeerPorts::default(),
    }
}

/// Writes `peers.bin`.
pub fn store_peer_ports(secrets: &dyn SecretStore, ports: &PeerPorts) -> Result<(), String> {
    let bytes = ports.encode().map_err(|e| e.to_string())?;
    secrets.store(PEER_PORTS_NAME, &bytes).map_err(|e| e.to_string())
}

/// Loads `account.bin`, or the defaults named `default_name` when it is missing or does not read (with one trace
/// line: an account's own choices are not worth refusing to serve it, ACCOUNTS.md 14.2).
pub fn load_account_settings(secrets: &dyn SecretStore, default_name: &str, trace: &dyn TraceSink) -> AccountSettings {
    match secrets.load(ACCOUNT_SETTINGS_NAME) {
        Ok(Some(bytes)) => AccountSettings::decode(&bytes).unwrap_or_else(|e| {
            trace.log(Level::Warn, "store", format_args!("account.bin does not read ({e}): the defaults apply"));
            AccountSettings::new(default_name)
        }),
        Ok(None) => AccountSettings::new(default_name),
        Err(e) => {
            trace.log(Level::Warn, "store", format_args!("account.bin cannot be read ({e}): the defaults apply"));
            AccountSettings::new(default_name)
        }
    }
}

/// The audit log as the hub reads it: an appending sink plus the recent entries, newest first.
pub trait AuditLog: AuditSink {
    /// At most `max` entries, newest first.
    fn recent(&self, max: usize) -> Vec<AuditEntry>;
}

impl AuditLog for MemoryAudit {
    fn recent(&self, max: usize) -> Vec<AuditEntry> {
        let mut all = self.entries();
        all.reverse();
        all.truncate(max);
        all
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The size at which `audit-sessions.log` rotates (C5.2, GUESS default).
pub const SESSIONS_LOG_CAP: u64 = 10 * 1024 * 1024;
/// The size at which `audit-network.log` rotates.
pub const NETWORK_LOG_CAP: u64 = 2 * 1024 * 1024;
/// Entries older than this are not read back (C5.2: 90 days).
pub const MAX_AGE_MS: u64 = 90 * 24 * 60 * 60 * 1000;
/// Entries kept in memory for the Activity screen and the last-session lines.
const CACHE: usize = 5000;

const SESSIONS_FILE: &str = "audit-sessions.log";
const NETWORK_FILE: &str = "audit-network.log";

/// The unauthenticated kinds go to the network log (C5.2), everything else to the sessions log.
fn is_network(entry: &AuditEntry) -> bool {
    matches!(
        entry.event,
        AuditEvent::UnknownDeviceRejected | AuditEvent::Suppressed { .. } | AuditEvent::AddressBlocked(_)
    ) || (matches!(entry.event, AuditEvent::PairFailed(_) | AuditEvent::PairRefused(_) | AuditEvent::PairingLocked)
        && matches!(entry.device, DeviceRef::Unknown { .. }))
}

/// One unauthenticated entry per remote address per minute (C5.1).
const PER_ADDRESS_MS: u64 = 60 * 1000;
/// At most this many unauthenticated entries in an hour, in total (C5.1).
const PER_HOUR: u32 = 60;
/// The window the hourly total and the `Suppressed` summary are counted over.
const WINDOW_MS: u64 = 60 * 60 * 1000;
/// Remote addresses remembered for the per-minute rule; past it, new addresses are suppressed until entries expire.
const MAX_ADDRESSES: usize = 4096;
/// `Suppressed { kind }` of the summary this limiter writes: entries of several kinds, counted together.
pub const SUPPRESSED_MIXED: u8 = 0;

/// The remote address an entry caused by an unauthenticated peer is about (C5.1: `UnknownDeviceRejected`, a
/// `PairFailed` before step 2 (written as `PairRefused`) and blocked addresses); `None` for every other entry. A
/// `PairFailed` (the attempt reached step 2 and counted against the lockout) and a `PairingLocked` are not limited:
/// the lockout and the pairing rate already bound them, and they are what the owner must see.
fn unauthenticated_addr(entry: &AuditEntry) -> Option<[u8; 4]> {
    match (&entry.event, &entry.device) {
        (AuditEvent::AddressBlocked(addr), _) => Some(*addr),
        (AuditEvent::UnknownDeviceRejected | AuditEvent::PairRefused(_), DeviceRef::Unknown { addr, .. }) => {
            Some(*addr)
        }
        _ => None,
    }
}

/// True when `then` is less than a minute before `now`. A `then` after `now` (the wall clock stepped back) is not
/// recent: it must not hold an address suppressed until the clock catches up.
fn within_a_minute(then: u64, now: u64) -> bool {
    now.checked_sub(then).is_some_and(|d| d < PER_ADDRESS_MS)
}

/// The rate limit of C5.1: entries caused by unauthenticated peers are written at most once per address per minute
/// and 60 per hour in total; the rest are counted and written as one `Suppressed` entry per hour. Time is the
/// entries' own `time_ms`, so a test drives it without a clock. Without it a peer on the LAN fills the 2 MiB network
/// log (and rotates the real history out) with connections that never authenticate.
#[derive(Default)]
struct UnauthRate {
    last_by_addr: std::collections::HashMap<[u8; 4], u64>,
    window_start_ms: u64,
    written_in_window: u32,
    suppressed: u32,
    /// The time of the last entry seen, for the summary written when the log is dropped.
    last_seen_ms: u64,
}

/// What the limiter decided for one entry: the `Suppressed` summary of the hour that just ended (written first), and
/// whether the entry itself is written.
struct Verdict {
    summary: Option<AuditEntry>,
    write: bool,
}

impl UnauthRate {
    /// The `Suppressed` entry for what was dropped since the last one, and the count starts again.
    fn take_summary(&mut self, now: u64) -> Option<AuditEntry> {
        let count = std::mem::take(&mut self.suppressed);
        (count > 0).then_some(AuditEntry {
            time_ms: now,
            device: DeviceRef::Unknown { addr: [0; 4], device_id: None },
            event: AuditEvent::Suppressed { count, kind: SUPPRESSED_MIXED },
            duration_s: None,
        })
    }

    fn judge(&mut self, entry: &AuditEntry) -> Verdict {
        let now = entry.time_ms;
        self.last_seen_ms = now;
        let addr = unauthenticated_addr(entry);
        // The hour is over, or the wall clock stepped back before it began: the window starts again here. The summary
        // of what the old window dropped goes out with the next write of any kind, not only an unauthenticated one.
        let rolled = now >= self.window_start_ms.saturating_add(WINDOW_MS) || now < self.window_start_ms;
        let mut summary = None;
        if rolled && (addr.is_some() || self.suppressed > 0) {
            summary = self.take_summary(now);
            self.window_start_ms = now;
            self.written_in_window = 0;
        }
        let Some(addr) = addr else { return Verdict { summary, write: true } };
        if self.last_by_addr.len() >= MAX_ADDRESSES / 2 {
            self.last_by_addr.retain(|_, t| within_a_minute(*t, now));
        }
        let recent = self.last_by_addr.get(&addr).is_some_and(|t| within_a_minute(*t, now));
        let table_full = !self.last_by_addr.contains_key(&addr) && self.last_by_addr.len() >= MAX_ADDRESSES;
        if recent || table_full || self.written_in_window >= PER_HOUR {
            self.suppressed = self.suppressed.saturating_add(1);
            return Verdict { summary, write: false };
        }
        self.last_by_addr.insert(addr, now);
        self.written_in_window += 1;
        Verdict { summary, write: true }
    }
}

/// The audit log in two append-only files of length-prefixed records (the `swapdesk-trust` format), with the
/// recent entries cached in memory. Rotation is by size (the old file becomes `<name>.1`); the unauthenticated
/// entries go through the rate limit of C5.1 first, so a peer that never authenticates cannot fill the network log.
///
/// `append` never touches the disk: the rate limit and the cache are memory, and the file work (`metadata`, `rename`,
/// `open`, `write`) is done by one writer thread fed through a bounded channel, so a runtime worker that records an
/// entry (a session ending, a pairing failing) never waits for the disk. The channel holds [`WRITE_QUEUE`] records;
/// a caller that finds it full waits for room (the bound is the backpressure, nothing is lost and nothing grows).
/// Dropping the log writes what the limiter still owes and waits for the writer, so the files are complete when the
/// service stops; [`flush`](Self::flush) is the same wait without the drop.
pub struct FileAuditLog {
    dir: PathBuf,
    trace: std::sync::Arc<dyn TraceSink>,
    state: Mutex<FileState>,
    writes: Mutex<Option<SyncSender<Job>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

/// Records waiting for the writer thread before a caller waits.
const WRITE_QUEUE: usize = 1024;

enum Job {
    Record {
        name: &'static str,
        cap: u64,
        record: Vec<u8>,
    },
    /// Answered once every record queued before it is written.
    Flush(SyncSender<()>),
}

struct FileState {
    cache: Vec<AuditEntry>,
    rate: UnauthRate,
}

impl FileAuditLog {
    /// Opens (creating) the log in `dir`, which must exist, and reads the recent entries of both files and of their
    /// rotated copies. A file that does not decode is set aside as `<name>.damaged-<now_ms>`; one that ends in the
    /// middle of a record is cut back to its last whole record.
    pub fn open(dir: &Path, now_ms: u64, trace: std::sync::Arc<dyn TraceSink>) -> std::io::Result<Self> {
        let mut cache = Vec::new();
        for name in [SESSIONS_FILE, NETWORK_FILE] {
            for file in [format!("{name}.1"), name.to_string()] {
                cache.extend(read_file(&dir.join(&file), now_ms, &*trace));
            }
        }
        cache.retain(|e| e.time_ms.saturating_add(MAX_AGE_MS) >= now_ms);
        cache.sort_by_key(|e| e.time_ms);
        if cache.len() > CACHE {
            cache.drain(..cache.len() - CACHE);
        }
        let (tx, rx) = sync_channel::<Job>(WRITE_QUEUE);
        let writer = {
            let (dir, trace) = (dir.to_path_buf(), std::sync::Arc::clone(&trace));
            std::thread::Builder::new().name("audit-writer".into()).spawn(move || {
                while let Ok(job) = rx.recv() {
                    match job {
                        Job::Record { name, cap, record } => {
                            if let Err(e) = append_to(&dir, name, cap, &record) {
                                trace.log(Level::Warn, "audit", format_args!("could not write {name}: {e}"));
                            }
                        }
                        Job::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })?
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            trace,
            state: Mutex::new(FileState { cache, rate: UnauthRate::default() }),
            writes: Mutex::new(Some(tx)),
            writer: Mutex::new(Some(writer)),
        })
    }

    /// Waits until every record appended before this call is in its file.
    pub fn flush(&self) {
        let (done_tx, done_rx) = sync_channel(1);
        if self.queue(Job::Flush(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }

    /// Hands a job to the writer; the job comes back when the writer is gone (the log is being dropped, or its thread
    /// died).
    fn queue(&self, job: Job) -> Result<(), Job> {
        // Cloned so the lock is not held while a full channel makes the caller wait.
        let tx = self.writes.lock().unwrap_or_else(PoisonError::into_inner).clone();
        match tx {
            Some(tx) => tx.send(job).map_err(|e| e.0),
            None => Err(job),
        }
    }
}

/// One record into `name` under `dir`, rotating the file first when it reached `cap`.
fn append_to(dir: &Path, name: &str, cap: u64, record: &[u8]) -> std::io::Result<()> {
    let path = dir.join(name);
    if fs::metadata(&path).is_ok_and(|m| m.len() >= cap) {
        let rotated = dir.join(format!("{name}.1"));
        // The previous rotated copy goes (no more than two generations).
        let _ = fs::remove_file(&rotated);
        fs::rename(&path, &rotated)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    file.write_all(record)?;
    file.flush()
}

fn read_file(path: &Path, now_ms: u64, trace: &dyn TraceSink) -> Vec<AuditEntry> {
    let Ok(bytes) = fs::read(path) else { return Vec::new() };
    match decode_log(&bytes) {
        Ok(read) => {
            if read.truncated_tail
                && let Ok(file) = OpenOptions::new().write(true).open(path)
            {
                let _ = file.set_len(read.valid_len as u64);
            }
            read.entries
        }
        Err(e) => {
            let aside = path.with_extension(format!("log.damaged-{now_ms}"));
            trace.log(
                Level::Warn,
                "audit",
                format_args!("{} does not decode ({e}); set aside as {}", path.display(), aside.display()),
            );
            let _ = fs::rename(path, aside);
            Vec::new()
        }
    }
}

impl FileAuditLog {
    /// Writes one entry to its file and the cache, with no rate limit.
    fn write_entry(&self, st: &mut FileState, entry: AuditEntry) {
        let Ok(record) = entry.encode_record() else { return };
        let (name, cap) =
            if is_network(&entry) { (NETWORK_FILE, NETWORK_LOG_CAP) } else { (SESSIONS_FILE, SESSIONS_LOG_CAP) };
        // The writer thread does the file work. Gone (it never is, but a panic in it would be): write here instead.
        if let Err(Job::Record { record, .. }) = self.queue(Job::Record { name, cap, record })
            && let Err(e) = append_to(&self.dir, name, cap, &record)
        {
            self.trace.log(Level::Warn, "audit", format_args!("could not write {name}: {e}"));
        }
        st.cache.push(entry);
        if st.cache.len() > CACHE {
            st.cache.remove(0);
        }
    }
}

impl AuditSink for FileAuditLog {
    fn append(&self, entry: AuditEntry) {
        let mut st = lock(&self.state);
        let Verdict { summary, write } = st.rate.judge(&entry);
        if let Some(summary) = summary {
            self.write_entry(&mut st, summary);
        }
        if write {
            self.write_entry(&mut st, entry);
        }
    }
}

/// What was suppressed and not yet summarised is written when the log is dropped (the service stops), at the time of
/// the last entry seen.
impl Drop for FileAuditLog {
    fn drop(&mut self) {
        {
            let mut st = lock(&self.state);
            let now = st.rate.last_seen_ms;
            if let Some(summary) = st.rate.take_summary(now) {
                self.write_entry(&mut st, summary);
            }
        }
        // Close the channel, then wait for the writer to finish what is queued.
        drop(self.writes.lock().unwrap_or_else(PoisonError::into_inner).take());
        if let Some(writer) = self.writer.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = writer.join();
        }
    }
}

impl AuditLog for FileAuditLog {
    fn recent(&self, max: usize) -> Vec<AuditEntry> {
        lock(&self.state).cache.iter().rev().take(max).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_fakes::{LogCapture, MemorySecretStore};

    #[test]
    fn settings_round_trip_and_defaults() {
        let secrets = MemorySecretStore::new();
        let s = load_settings(&secrets, "pc").unwrap();
        assert_eq!(s, MachineSettings::new("pc"));
        let mut changed = s.clone();
        changed.name = "sala".into();
        changed.lock_on_end = true;
        store_settings(&secrets, &changed).unwrap();
        assert_eq!(load_settings(&secrets, "pc").unwrap(), changed);
        secrets.store(SETTINGS_NAME, &[9, 9]).unwrap();
        assert!(load_settings(&secrets, "pc").is_err());
    }

    #[test]
    fn names_are_checked() {
        assert!(valid_name("Sala"));
        assert!(!valid_name("  "));
        assert!(!valid_name("a\nb"));
        assert!(!valid_name(&"x".repeat(65)));
    }

    fn rejected(addr: u8, at_ms: u64) -> AuditEntry {
        AuditEntry {
            time_ms: at_ms,
            device: DeviceRef::Unknown { addr: [10, 0, 0, addr], device_id: None },
            event: AuditEvent::UnknownDeviceRejected,
            duration_s: None,
        }
    }

    #[test]
    fn unauthenticated_entries_are_one_per_address_per_minute_and_sixty_an_hour_then_one_summary() {
        let mut rate = UnauthRate::default();
        let t0 = 1_000_000u64;
        let written = |rate: &mut UnauthRate, e: AuditEntry| rate.judge(&e).write;
        // One address: the first is written, the rest of that minute are not, the next minute is.
        assert!(written(&mut rate, rejected(1, t0)));
        assert!(!written(&mut rate, rejected(1, t0 + 1_000)));
        assert!(!written(&mut rate, rejected(1, t0 + 59_999)));
        assert!(written(&mut rate, rejected(1, t0 + 60_000)));
        // Another address has its own minute.
        assert!(written(&mut rate, rejected(2, t0 + 61_000)));
        // Entries of authenticated peers are never limited.
        let paired = AuditEntry {
            time_ms: t0 + 62_000,
            device: DeviceRef::Known([1; 16]),
            event: AuditEvent::PairFailed(swapdesk_proto::ErrorCode::SasRejected),
            duration_s: None,
        };
        for _ in 0..5 {
            assert!(written(&mut rate, paired.clone()));
        }
        // 60 an hour in total: 3 are written so far, so 57 more addresses fill it and the 58th is suppressed.
        let mut fresh = 0u8;
        let mut hourly = 3u32;
        let at = t0 + 2 * 60_000;
        while hourly < PER_HOUR {
            fresh += 1;
            assert!(written(&mut rate, rejected(100 + fresh, at)), "entry {hourly}");
            hourly += 1;
        }
        assert!(!written(&mut rate, rejected(200, at)), "the 61st unauthenticated entry of the hour");
        assert!(!written(&mut rate, rejected(201, at)));
        // The first entry after the hour carries the summary of what was dropped: the two repeats inside a minute
        // (t0+1s, t0+59.999s) and the two over the hourly total.
        let verdict = rate.judge(&rejected(1, t0 + WINDOW_MS));
        assert!(verdict.write, "the new hour starts afresh");
        let summary = verdict.summary.expect("a summary of the suppressed entries");
        assert_eq!(summary.event, AuditEvent::Suppressed { count: 4, kind: SUPPRESSED_MIXED });
        assert_eq!(summary.time_ms, t0 + WINDOW_MS);
        // And the count starts again: no second summary.
        assert!(rate.judge(&rejected(3, t0 + WINDOW_MS + 61_000)).summary.is_none());
    }

    fn refused(addr: u8, at_ms: u64) -> AuditEntry {
        AuditEntry {
            time_ms: at_ms,
            device: DeviceRef::Unknown { addr: [10, 0, 0, addr], device_id: None },
            event: AuditEvent::PairRefused(swapdesk_proto::ErrorCode::Busy),
            duration_s: None,
        }
    }

    #[test]
    fn only_a_pairing_refused_before_step_2_is_limited_a_failed_or_locked_one_is_always_written() {
        let mut rate = UnauthRate::default();
        let t0 = 5_000_000u64;
        // A throwaway attempt from the address, refused before step 2 ...
        assert!(rate.judge(&refused(4, t0)).write);
        assert!(!rate.judge(&refused(4, t0 + 5_000)).write, "a second one inside the minute is suppressed");
        // ... then, 30 s later from the same address, a failure after step 2 and a lockout: both are written.
        let unknown = |event, at_ms| AuditEntry {
            time_ms: at_ms,
            device: DeviceRef::Unknown { addr: [10, 0, 0, 4], device_id: Some([7; 16]) },
            event,
            duration_s: None,
        };
        let failed = unknown(AuditEvent::PairFailed(swapdesk_proto::ErrorCode::SasRejected), t0 + 30_000);
        let locked = unknown(AuditEvent::PairingLocked, t0 + 30_500);
        assert!(rate.judge(&failed).write, "a SasRejected after step 2 is written");
        assert!(rate.judge(&locked).write, "a PairingLocked is written");
        let mismatch = unknown(AuditEvent::PairFailed(swapdesk_proto::ErrorCode::CommitMismatch), t0 + 31_000);
        assert!(rate.judge(&mismatch).write, "and so is a CommitMismatch right after");
        // They neither count against the hourly total nor against the address's minute.
        assert_eq!(rate.written_in_window, 1);
    }

    #[test]
    fn a_wall_clock_that_steps_back_does_not_freeze_the_limiter() {
        let mut rate = UnauthRate::default();
        let t0 = 10 * WINDOW_MS;
        assert!(rate.judge(&rejected(1, t0)).write);
        assert!(!rate.judge(&rejected(1, t0 + 1_000)).write);
        // The clock is set back two hours: the window starts again, and the address's old entry is not "recent".
        let back = t0 - 2 * WINDOW_MS;
        let verdict = rate.judge(&rejected(1, back));
        assert!(verdict.write, "an entry after a step back is judged afresh");
        assert_eq!(
            verdict.summary.expect("what the old window dropped").event,
            AuditEvent::Suppressed { count: 1, kind: SUPPRESSED_MIXED }
        );
        assert!(!rate.judge(&rejected(1, back + 1_000)).write, "and the minute rule works from the new time");
        assert!(rate.judge(&rejected(1, back + PER_ADDRESS_MS)).write);
        assert_eq!(rate.written_in_window, 2);
    }

    #[test]
    fn the_summary_goes_out_with_the_next_write_of_any_kind_and_when_the_log_is_dropped() {
        let mut rate = UnauthRate::default();
        let t0 = 10 * WINDOW_MS;
        assert!(rate.judge(&rejected(1, t0)).write);
        for i in 1..=3 {
            assert!(!rate.judge(&rejected(1, t0 + i * 1_000)).write);
        }
        // An hour later the next entry is a session entry of a paired device: the summary comes with it.
        let paired = AuditEntry {
            time_ms: t0 + WINDOW_MS,
            device: DeviceRef::Known([1; 16]),
            event: AuditEvent::Paired,
            duration_s: None,
        };
        let verdict = rate.judge(&paired);
        assert!(verdict.write);
        assert_eq!(
            verdict.summary.expect("the hour's summary").event,
            AuditEvent::Suppressed { count: 3, kind: SUPPRESSED_MIXED }
        );
        assert!(rate.judge(&paired).summary.is_none(), "written once");

        // On the file log: what is suppressed is written when the log is dropped.
        let dir = std::env::temp_dir().join(format!("swapdesk-audit-drop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let trace: std::sync::Arc<dyn TraceSink> = std::sync::Arc::new(LogCapture::new());
        let log = FileAuditLog::open(&dir, 10_000, trace.clone()).unwrap();
        log.append(rejected(1, 20_000));
        log.append(rejected(1, 21_000));
        log.append(rejected(1, 22_000));
        assert_eq!(log.recent(10).len(), 1);
        drop(log); // the drop writes the summary and waits for the writer thread
        let again = FileAuditLog::open(&dir, 30_000, trace).unwrap();
        let events: Vec<_> = again.recent(10).into_iter().map(|e| e.event).collect();
        assert_eq!(
            events,
            [AuditEvent::Suppressed { count: 2, kind: SUPPRESSED_MIXED }, AuditEvent::UnknownDeviceRejected]
        );
        drop(again);
        for name in [SESSIONS_FILE, NETWORK_FILE] {
            let _ = fs::remove_file(dir.join(name));
        }
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn a_flood_of_unauthenticated_entries_does_not_grow_the_network_log() {
        let dir = std::env::temp_dir().join(format!("swapdesk-audit-flood-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let trace: std::sync::Arc<dyn TraceSink> = std::sync::Arc::new(LogCapture::new());
        let log = FileAuditLog::open(&dir, 10_000, trace).unwrap();
        let one = rejected(9, 20_000).encode_record().unwrap().len() as u64;
        // 5000 attempts in one second from one address, and 5000 more from 5000 addresses.
        for i in 0..5000u64 {
            log.append(rejected(9, 20_000 + i / 10));
        }
        for i in 0..5000u64 {
            let a = [10, (i >> 8) as u8, i as u8, 1];
            log.append(AuditEntry {
                time_ms: 21_000 + i / 10,
                device: DeviceRef::Unknown { addr: a, device_id: None },
                event: AuditEvent::UnknownDeviceRejected,
                duration_s: None,
            });
        }
        log.flush();
        let size = fs::metadata(dir.join(NETWORK_FILE)).unwrap().len();
        assert!(size <= PER_HOUR as u64 * one + 64, "the network log grew to {size} bytes ({one} per entry)");
        assert!(log.recent(10_000).len() <= PER_HOUR as usize);
        drop(log);
        for name in [SESSIONS_FILE, NETWORK_FILE] {
            let _ = fs::remove_file(dir.join(name));
        }
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn a_burst_larger_than_the_writer_queue_loses_nothing_keeps_order_and_the_cache_is_ahead_of_the_disk() {
        let dir = std::env::temp_dir().join(format!("swapdesk-audit-burst-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let trace: std::sync::Arc<dyn TraceSink> = std::sync::Arc::new(LogCapture::new());
        let log = FileAuditLog::open(&dir, 10_000, trace).unwrap();
        let total = WRITE_QUEUE * 3 + 7;
        for i in 0..total as u64 {
            log.append(AuditEntry {
                time_ms: 20_000 + i,
                device: DeviceRef::Known([1; 16]),
                event: AuditEvent::Paired,
                duration_s: None,
            });
        }
        // The cache answers `recent` at once, with no wait for the files.
        assert_eq!(log.recent(1)[0].time_ms, 20_000 + total as u64 - 1);
        log.flush();
        let bytes = fs::read(dir.join(SESSIONS_FILE)).unwrap();
        let read = decode_log(&bytes).unwrap();
        assert_eq!(read.entries.len(), total, "every entry reached the file");
        assert!(read.entries.windows(2).all(|w| w[0].time_ms < w[1].time_ms), "in the order they were appended");
        drop(log);
        for name in [SESSIONS_FILE, NETWORK_FILE] {
            let _ = fs::remove_file(dir.join(name));
        }
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn the_file_log_keeps_entries_across_a_reopen_and_routes_by_kind() {
        let dir = std::env::temp_dir().join(format!("swapdesk-audit-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let trace: std::sync::Arc<dyn TraceSink> = std::sync::Arc::new(LogCapture::new());
        let log = FileAuditLog::open(&dir, 10_000, trace.clone()).unwrap();
        let known = AuditEntry {
            time_ms: 9_000,
            device: DeviceRef::Known([1; 16]),
            event: AuditEvent::Paired,
            duration_s: None,
        };
        let unknown = AuditEntry {
            time_ms: 9_500,
            device: DeviceRef::Unknown { addr: [10, 0, 0, 9], device_id: None },
            event: AuditEvent::UnknownDeviceRejected,
            duration_s: None,
        };
        log.append(known.clone());
        log.append(unknown.clone());
        log.flush();
        assert!(dir.join(SESSIONS_FILE).exists() && dir.join(NETWORK_FILE).exists());
        assert_eq!(log.recent(10), vec![unknown.clone(), known.clone()], "newest first");
        drop(log);
        let again = FileAuditLog::open(&dir, 10_000, trace.clone()).unwrap();
        assert_eq!(again.recent(10), vec![unknown, known]);
        // Entries past 90 days are not read back.
        let later = FileAuditLog::open(&dir, 10_000 + MAX_AGE_MS + 1_000, trace).unwrap();
        assert!(later.recent(10).is_empty());
        for name in [SESSIONS_FILE, NETWORK_FILE] {
            fs::remove_file(dir.join(name)).unwrap();
        }
        fs::remove_dir(&dir).unwrap();
    }
}
