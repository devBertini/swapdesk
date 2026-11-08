//! What the hub keeps on disk besides the trust store: the machine settings record (`settings.bin`, C5.1) and the
//! audit log (`audit-sessions.log` and `audit-network.log`, C5.2), both over plain traits so tests run in memory.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

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
    ) || (matches!(entry.event, AuditEvent::PairFailed(_) | AuditEvent::PairingLocked)
        && matches!(entry.device, DeviceRef::Unknown { .. }))
}

/// The audit log in two append-only files of length-prefixed records (the `swapdesk-trust` format), with the
/// recent entries cached in memory. Rotation is by size (the old file becomes `<name>.1`); the rate limit of C5.1 is
/// not applied here (nothing in this build writes unauthenticated entries in bulk yet).
pub struct FileAuditLog {
    dir: PathBuf,
    trace: std::sync::Arc<dyn TraceSink>,
    state: Mutex<FileState>,
}

struct FileState {
    cache: Vec<AuditEntry>,
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
        Ok(Self { dir: dir.to_path_buf(), trace, state: Mutex::new(FileState { cache }) })
    }

    fn append_to(&self, name: &str, cap: u64, record: &[u8]) -> std::io::Result<()> {
        let path = self.dir.join(name);
        if fs::metadata(&path).is_ok_and(|m| m.len() >= cap) {
            let rotated = self.dir.join(format!("{name}.1"));
            // The previous rotated copy goes (no more than two generations).
            let _ = fs::remove_file(&rotated);
            fs::rename(&path, &rotated)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.write_all(record)?;
        file.flush()
    }
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

impl AuditSink for FileAuditLog {
    fn append(&self, entry: AuditEntry) {
        let Ok(record) = entry.encode_record() else { return };
        let (name, cap) =
            if is_network(&entry) { (NETWORK_FILE, NETWORK_LOG_CAP) } else { (SESSIONS_FILE, SESSIONS_LOG_CAP) };
        if let Err(e) = self.append_to(name, cap, &record) {
            self.trace.log(Level::Warn, "audit", format_args!("could not write {name}: {e}"));
        }
        let mut st = lock(&self.state);
        st.cache.push(entry);
        if st.cache.len() > CACHE {
            st.cache.remove(0);
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
