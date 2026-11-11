//! The update manager (UPDATES.md 2.2, 10.2, 13): the schedule, the switch, the check and the download, as a state
//! machine the service (Windows) or the app (Mac) drives and the interfaces show through [`UpdateView`]. OS-free:
//! the HTTP client, the protected folder and the clock come in from outside, so every rule is tested with fakes.
//!
//! What it never does: install by itself (an install is a person's click, owner decision 1), check from a development
//! instance, check with the switch off, or write anywhere but its folder.

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use crate::http::{HttpError, HttpGet};
use crate::keys::PublicKey;
use crate::manifest::{FileEntry, MAX_MANIFEST, MAX_SIGNATURE, verify_with};
use crate::policy::{Offer, offer};
use crate::{UpdateError, Version};

/// The two files every check reads (UPDATES.md 2.1, option A).
pub const MANIFEST_NAME: &str = "swapdesk-update.txt";
pub const SIGNATURE_NAME: &str = "swapdesk-update.txt.sig";

/// The first check comes this long after the start, at least and at most (UPDATES.md 2.2): the caller picks the
/// delay at random in this range, so a power cut does not line up every machine of a house.
pub const FIRST_DELAY_MS: (u64, u64) = (2 * 60_000, 10 * 60_000);
/// Then every 24 hours, plus or minus one (the caller picks the jitter).
pub const PERIOD_MS: u64 = 24 * 3_600_000;
pub const PERIOD_JITTER_MS: u64 = 3_600_000;

/// What persists across restarts (the service keeps it in its folder; the Mac app in its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// "Procurar atualizações automaticamente" (owner decision 2: on by default).
    pub check_on: bool,
    pub last_check_ms: Option<u64>,
    /// "Agora não" for this version: the card stays hidden for it (Ajustes keeps showing it).
    pub dismissed: Option<Version>,
    /// Versions installed here that did not commit: not offered again by themselves (4.5).
    pub rolled_back: Vec<Version>,
}

impl Default for Saved {
    fn default() -> Self {
        Self { check_on: true, last_check_ms: None, dismissed: None, rolled_back: Vec::new() }
    }
}

impl Saved {
    /// The file's text: one `key value` per line (the service keeps it in its protected folder).
    pub fn to_text(&self) -> String {
        let mut t = format!("swapdesk-update-state 1\ncheck {}\n", u8::from(self.check_on));
        if let Some(ms) = self.last_check_ms {
            t.push_str(&format!("last {ms}\n"));
        }
        if let Some(v) = &self.dismissed {
            t.push_str(&format!("dismissed {v}\n"));
        }
        for v in &self.rolled_back {
            t.push_str(&format!("rolled-back {v}\n"));
        }
        t
    }

    /// Reads [`Saved::to_text`]; what does not read gives the defaults (the switch on), never an error.
    pub fn from_text(text: &str) -> Saved {
        let mut lines = text.lines();
        if lines.next() != Some("swapdesk-update-state 1") {
            return Saved::default();
        }
        let mut s = Saved::default();
        for line in lines {
            match line.split_once(' ') {
                Some(("check", v)) => s.check_on = v != "0",
                Some(("last", v)) => s.last_check_ms = v.parse().ok(),
                Some(("dismissed", v)) => s.dismissed = Version::parse(v).ok(),
                Some(("rolled-back", v)) => s.rolled_back.extend(Version::parse(v).ok()),
                _ => {}
            }
        }
        s
    }
}

/// Where [`Saved`] lives.
pub trait SavedStore: Send + Sync {
    fn load(&self) -> Saved;
    fn save(&self, saved: &Saved);
}

/// The protected update folder (Windows: only SYSTEM and Administrators open it; UPDATES.md 2.2, 6.3).
pub trait UpdateFolder: Send + Sync {
    /// A new file named `name` in the folder, replacing one of that name.
    fn create(&self, name: &str) -> std::io::Result<Box<dyn Write + Send>>;
    /// Removes `name` from the folder; no error when it is not there.
    fn remove(&self, name: &str);
    /// The full path of `name`, as the applier is given it.
    fn path(&self, name: &str) -> std::path::PathBuf;
}

/// How the manager is set up.
#[derive(Debug, Clone)]
pub struct Setup {
    pub running: Version,
    /// `windows-x64` or `macos-arm64`.
    pub target: String,
    /// `https://github.com/devBertini/swapdesk/releases` in production; a loopback server in tests.
    pub base: String,
    /// The release keys (`keys::RELEASE_KEYS` in production).
    pub keys: Vec<PublicKey>,
    /// A development instance (`--instance`, `--console`, `--scratch`): never checks, never installs.
    pub dev: bool,
    /// The first check's delay, picked by the caller within [`FIRST_DELAY_MS`].
    pub first_delay_ms: u64,
}

/// Why a check did not give an answer (silent by itself; "Procurar agora" says it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckFailure {
    /// No answer from GitHub (no network, a proxy, TLS).
    Network,
    /// GitHub asked to wait (403 or 429).
    RateLimited,
    /// The manifest's signature, or the manifest, does not verify.
    Signature,
}

/// Why an install did not happen, or stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallFailure {
    /// Asked from an interface that is not the console session's.
    NotConsole,
    /// A session is live and the person has not confirmed ending it (10.3).
    NeedsConfirmation,
    /// Not the version on offer (a newer check replaced it, or none).
    NotOffered,
    /// Another install runs.
    Busy,
    /// The download failed (network, cancel, a size beyond the manifest's).
    Download,
    /// The package's size or hash is not the signed manifest's.
    Mismatch,
    /// The folder could not be written.
    Disk,
    /// Cancelled ("Cancelar").
    Cancelled,
}

/// Where the manager is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Checking,
    /// A newer version. `manual`: it needs the installer; the interface links to its release page.
    Offered {
        version: Version,
        manual: bool,
    },
    Downloading {
        version: Version,
        done: u64,
        total: u64,
    },
    Verifying {
        version: Version,
    },
    /// Downloaded and verified: handed to the applier (UPDATES.md 6, slice U5).
    Ready {
        version: Version,
    },
    Failed(InstallFailure),
}

/// What the interfaces show (UPDATES.md 10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateView {
    pub running: Version,
    pub phase: Phase,
    pub check_on: bool,
    pub last_check_ms: Option<u64>,
    /// The card is hidden for this version.
    pub dismissed: Option<Version>,
    /// The last "Procurar agora" that failed, and why.
    pub last_failure: Option<CheckFailure>,
}

/// What an install request knows of the asking interface and the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallAsk {
    pub from_console: bool,
    pub live_sessions: usize,
    /// "Encerrar e atualizar" was pressed.
    pub end_sessions: bool,
}

struct Inner {
    phase: Phase,
    saved: Saved,
    next_check_ms: Option<u64>,
    last_failure: Option<CheckFailure>,
    /// The entry of the version on offer (from the verified manifest).
    entry: Option<FileEntry>,
}

/// The manager. Cheap to share; every call is short except a check and a download, which the caller runs on a thread
/// of its own.
pub struct UpdateManager {
    setup: Setup,
    http: Arc<dyn HttpGet>,
    store: Arc<dyn SavedStore>,
    folder: Arc<dyn UpdateFolder>,
    inner: Mutex<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A writer that also hashes what it writes.
struct Hashing<'a> {
    out: &'a mut dyn Write,
    digest: ring::digest::Context,
}

impl Write for Hashing<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.out.write(buf)?;
        self.digest.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

impl UpdateManager {
    /// `now_ms`: the start; the first check is due `setup.first_delay_ms` later.
    pub fn new(
        setup: Setup,
        http: Arc<dyn HttpGet>,
        store: Arc<dyn SavedStore>,
        folder: Arc<dyn UpdateFolder>,
        now_ms: u64,
    ) -> Self {
        let saved = store.load();
        let next = (!setup.dev).then(|| now_ms + setup.first_delay_ms);
        Self {
            setup,
            http,
            store,
            folder,
            inner: Mutex::new(Inner {
                phase: Phase::Idle,
                saved,
                next_check_ms: next,
                last_failure: None,
                entry: None,
            }),
        }
    }

    pub fn view(&self) -> UpdateView {
        let i = lock(&self.inner);
        UpdateView {
            running: self.setup.running.clone(),
            phase: i.phase.clone(),
            check_on: i.saved.check_on,
            last_check_ms: i.saved.last_check_ms,
            dismissed: i.saved.dismissed.clone(),
            last_failure: i.last_failure,
        }
    }

    /// When the next scheduled check is due (`None`: never: a development instance or the switch off).
    pub fn next_check_ms(&self) -> Option<u64> {
        let i = lock(&self.inner);
        if self.setup.dev || !i.saved.check_on { None } else { i.next_check_ms }
    }

    /// The scheduled check, when due: silent when it fails, and the next one a period later. `jitter_ms` within
    /// ±[`PERIOD_JITTER_MS`], picked by the caller. `true` when a check ran.
    pub fn tick(&self, now_ms: u64, jitter_ms: i64) -> bool {
        if self.next_check_ms().is_none_or(|t| now_ms < t) {
            return false;
        }
        let _ = self.check(now_ms, jitter_ms);
        true
    }

    /// "Procurar agora" (and the scheduled check): reads the signed manifest and offers what it allows.
    pub fn check(&self, now_ms: u64, jitter_ms: i64) -> Result<(), CheckFailure> {
        if self.setup.dev {
            return Ok(());
        }
        let before = {
            let mut i = lock(&self.inner);
            if matches!(i.phase, Phase::Downloading { .. } | Phase::Verifying { .. } | Phase::Ready { .. }) {
                return Ok(());
            }
            let before = std::mem::replace(&mut i.phase, Phase::Checking);
            i.next_check_ms = Some(now_ms.saturating_add_signed(PERIOD_MS as i64 + jitter_ms));
            before
        };
        let result = self.fetch_offer();
        let outcome = result.as_ref().map(|_| ()).map_err(|f| *f);
        let mut i = lock(&self.inner);
        i.saved.last_check_ms = Some(now_ms);
        match result {
            Ok(Some((version, manual, entry))) => {
                i.phase = Phase::Offered { version, manual };
                i.entry = entry;
                i.last_failure = None;
            }
            Ok(None) => {
                i.phase = Phase::Idle;
                i.entry = None;
                i.last_failure = None;
            }
            Err(f) => {
                // Silent: what was on offer stays on offer (a failed install's reason too); the next period tries
                // again.
                if i.phase == Phase::Checking {
                    i.phase = before;
                }
                i.last_failure = Some(f);
            }
        }
        self.store.save(&i.saved);
        outcome
    }

    fn fetch(&self, name: &str, max: usize) -> Result<Vec<u8>, CheckFailure> {
        let url = format!("{}/latest/download/{name}", self.setup.base);
        let mut out = Vec::new();
        let never = AtomicBool::new(false);
        self.http.get(&url, max as u64, &mut out, &mut |_| {}, &never).map_err(|e| match e {
            HttpError::Status(403 | 429) => CheckFailure::RateLimited,
            HttpError::TooLarge => CheckFailure::Signature,
            _ => CheckFailure::Network,
        })?;
        Ok(out)
    }

    #[allow(clippy::type_complexity)] // the version, whether it is manual, and its entry
    fn fetch_offer(&self) -> Result<Option<(Version, bool, Option<FileEntry>)>, CheckFailure> {
        let manifest = self.fetch(MANIFEST_NAME, MAX_MANIFEST)?;
        let signature = self.fetch(SIGNATURE_NAME, MAX_SIGNATURE)?;
        let m = verify_with(&manifest, &signature, &self.setup.keys).map_err(|_| CheckFailure::Signature)?;
        let rolled_back = lock(&self.inner).saved.rolled_back.clone();
        match offer(&m, &self.setup.running, &self.setup.target, &rolled_back) {
            Ok(Offer::Nothing) => Ok(None),
            Ok(Offer::Install { version, entry }) => Ok(Some((version.clone(), false, Some(entry.clone())))),
            Ok(Offer::Manual { version }) => Ok(Some((version.clone(), true, None))),
            Err(UpdateError::NoEntry) => Ok(None),
            Err(_) => Err(CheckFailure::Signature),
        }
    }

    /// The switch in Ajustes. Off: nothing is sent from now on.
    pub fn set_check(&self, on: bool) {
        let mut i = lock(&self.inner);
        i.saved.check_on = on;
        self.store.save(&i.saved);
    }

    /// "Agora não": the card hides for `version`.
    pub fn dismiss(&self, version: &Version) {
        let mut i = lock(&self.inner);
        i.saved.dismissed = Some(version.clone());
        self.store.save(&i.saved);
    }

    /// The applier reported that `version` did not commit (UPDATES.md 6.4): it is not offered again by itself.
    pub fn rolled_back(&self, version: &Version) {
        let mut i = lock(&self.inner);
        if !i.saved.rolled_back.contains(version) {
            i.saved.rolled_back.push(version.clone());
        }
        if matches!(&i.phase, Phase::Offered { version: v, .. } if v == version) {
            i.phase = Phase::Idle;
        }
        self.store.save(&i.saved);
    }

    /// "Atualizar agora" (UPDATES.md 10.2, 10.3): refused from an interface outside the console session, and with a
    /// live session unless "Encerrar e atualizar" was pressed; then downloads the package into the update folder and
    /// checks its size and hash against the signed manifest. On any failure the partial file is deleted. `Ok`: the
    /// verified package's path, for the applier.
    pub fn install(
        &self,
        version: &Version,
        ask: InstallAsk,
        cancel: &AtomicBool,
    ) -> Result<std::path::PathBuf, InstallFailure> {
        if self.setup.dev {
            return Err(InstallFailure::NotOffered);
        }
        if !ask.from_console {
            return Err(InstallFailure::NotConsole);
        }
        if ask.live_sessions > 0 && !ask.end_sessions {
            return Err(InstallFailure::NeedsConfirmation);
        }
        let entry = {
            let mut i = lock(&self.inner);
            match &i.phase {
                Phase::Downloading { .. } | Phase::Verifying { .. } => return Err(InstallFailure::Busy),
                Phase::Offered { version: v, manual: false } if v == version => {}
                _ => return Err(InstallFailure::NotOffered),
            }
            let Some(entry) = i.entry.clone() else { return Err(InstallFailure::NotOffered) };
            i.phase = Phase::Downloading { version: version.clone(), done: 0, total: entry.size };
            entry
        };
        let result = self.download(version, &entry, cancel);
        let mut i = lock(&self.inner);
        match &result {
            Ok(_) => i.phase = Phase::Ready { version: version.clone() },
            Err(f) => {
                self.folder.remove(&entry.name);
                i.phase = Phase::Failed(*f);
            }
        }
        result
    }

    fn download(
        &self,
        version: &Version,
        entry: &FileEntry,
        cancel: &AtomicBool,
    ) -> Result<std::path::PathBuf, InstallFailure> {
        let url = format!("{}/download/v{version}/{}", self.setup.base, entry.name);
        let mut file = self.folder.create(&entry.name).map_err(|_| InstallFailure::Disk)?;
        let mut hashing = Hashing { out: &mut *file, digest: ring::digest::Context::new(&ring::digest::SHA256) };
        let mut written = 0u64;
        let got = self.http.get(
            &url,
            entry.size,
            &mut hashing,
            &mut |done| {
                written = done;
                if let Phase::Downloading { done: d, .. } = &mut lock(&self.inner).phase {
                    *d = done;
                }
            },
            cancel,
        );
        let digest = hashing.digest.finish();
        file.flush().map_err(|_| InstallFailure::Disk)?;
        drop(file);
        match got {
            Ok(()) => {}
            Err(HttpError::Cancelled) => return Err(InstallFailure::Cancelled),
            Err(_) => return Err(InstallFailure::Download),
        }
        lock(&self.inner).phase = Phase::Verifying { version: version.clone() };
        if written != entry.size || digest.as_ref() != entry.sha256 {
            return Err(InstallFailure::Mismatch);
        }
        Ok(self.folder.path(&entry.name))
    }
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
