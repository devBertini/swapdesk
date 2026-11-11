//! `SecretStore` for Windows (contracts C3 and C5.2): every secret is a file in a protected folder, wrapped with
//! DPAPI at machine scope, written to a temporary file in the same folder, flushed and renamed.
//!
//! The DACL on the folder is what stops other local users (machine-scope DPAPI does not). Reparse points are never
//! followed: not in the folder, not in a file.
//!
//! This crate is Windows only and may use `unsafe`; every block states what it relies on.
#![cfg(windows)]

mod dpapi;
mod folder;
mod sys;

use std::path::{Path, PathBuf};

use swapdesk_platform::{Level, Result, SecretStore, TraceSink};

pub use dpapi::{protect, protect_user, unprotect, unprotect_user};
pub use folder::{FolderReport, FolderSpec, dacl_matches, ensure_folder};
pub use sys::{FolderSecurity, current_user_sid, normalize_sid, read_security};

/// The folder's name inside `%ProgramData%`.
pub const FOLDER_NAME: &str = "Swapdesk";

/// DPAPI entropy of a blob: `swapdesk <stem> v1`, which for `identity.bin` is "swapdesk identity v1" (C5.2).
fn entropy_for(name: &str) -> Vec<u8> {
    let stem = name.split('.').next().unwrap_or(name);
    format!("swapdesk {stem} v1").into_bytes()
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A secret's name is one plain file name: letters, digits, `.`, `_` and `-`, not starting with a dot, not a device
/// name, not the name of a temporary file.
fn check_name(name: &str) -> Result<()> {
    let plain = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.ends_with(".tmp")
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if plain && !RESERVED.contains(&stem.as_str()) {
        Ok(())
    } else {
        Err(sys::invalid("SecretStore", format!("{name:?} is not a valid secret name")))
    }
}

/// The folder under the protected root that holds one folder per account (ACCOUNTS.md 4.1).
pub const ACCOUNTS_FOLDER: &str = "accounts";
/// The longest SID an account folder may be named after (the `str(184)` of C2.4).
pub const MAX_SID: usize = 184;

/// A SID that may name an account folder: `S-1-` followed by digits and dashes only, at most [`MAX_SID`] bytes, so
/// it can never name another path.
pub fn account_folder_ok(sid: &str) -> bool {
    sid.len() <= MAX_SID
        && sid.strip_prefix("S-1-").is_some_and(|rest| {
            !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-') && !rest.contains("--")
        })
        && !sid.ends_with('-')
}

/// The DPAPI-backed store in the protected folder.
pub struct WinSecretStore {
    dir: PathBuf,
    /// Owners (as `S-1-5-...`) a file may have and still be read: the spec's trusted owners.
    trusted_owners: Vec<String>,
    /// The folder rules this store was opened with; an account's folder gets the same.
    spec: FolderSpec,
}

impl WinSecretStore {
    /// Opens `%ProgramData%\Swapdesk`, creating it as C5.2 says. Run by the service (SYSTEM).
    pub fn open(trace: Option<&dyn TraceSink>) -> Result<Self> {
        let program_data =
            std::env::var_os("ProgramData").ok_or_else(|| sys::invalid("open", "%ProgramData% is not set"))?;
        Self::open_in(&FolderSpec::production(), Path::new(&program_data), trace)
    }

    /// The same with an injected parent folder and spec (tests use a temp folder and the current user).
    pub fn open_in(spec: &FolderSpec, parent: &Path, trace: Option<&dyn TraceSink>) -> Result<Self> {
        let report = ensure_folder(spec, parent, FOLDER_NAME, trace)?;
        if let (Some(t), Some(aside)) = (trace, &report.moved_aside) {
            t.log(
                Level::Warn,
                "secret",
                format_args!("alert: an untrusted {FOLDER_NAME} folder was set aside as {}", aside.display()),
            );
        }
        let trusted_owners = spec.trusted_owners.iter().map(|o| sys::normalize_sid(o)).collect::<Result<Vec<_>>>()?;
        Ok(Self { dir: report.path, trusted_owners, spec: spec.clone() })
    }

    /// The store of one account (ACCOUNTS.md 4.1): `<root>\accounts\<sid>\`, each folder made, kept or moved aside
    /// by the rules of the root ([`ensure_folder`]). Its files have the root's names and DPAPI entropies.
    pub fn open_account(&self, sid: &str, trace: Option<&dyn TraceSink>) -> Result<Self> {
        if !account_folder_ok(sid) {
            return Err(sys::invalid("open_account", "not a SID that may name a folder"));
        }
        let accounts = ensure_folder(&self.spec, &self.dir, ACCOUNTS_FOLDER, trace)?;
        let report = ensure_folder(&self.spec, &accounts.path, sid, trace)?;
        if let (Some(t), Some(aside)) = (trace, &report.moved_aside) {
            t.log(
                Level::Warn,
                "secret",
                format_args!("alert: an untrusted account folder was set aside as {}", aside.display()),
            );
        }
        Ok(Self { dir: report.path, trusted_owners: self.trusted_owners.clone(), spec: self.spec.clone() })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_of(&self, name: &str) -> Result<PathBuf> {
        check_name(name)?;
        Ok(self.dir.join(name))
    }
}

impl SecretStore for WinSecretStore {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path_of(name)?;
        let Some(h) = sys::open_file_for_read(&path)? else { return Ok(None) };
        // The DACL keeps other users from writing here; a file owned by anyone else was not made by the service.
        let owner = sys::owner_of(&h)?;
        if !self.trusted_owners.contains(&owner) {
            return Err(sys::invalid(
                "SecretStore::load",
                format!("{name:?} is owned by {owner}, which is not a trusted owner"),
            ));
        }
        let blob = sys::read_all(&h)?;
        Ok(Some(unprotect(&blob, &entropy_for(name))?))
    }

    fn store(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_of(name)?;
        let tmp = self.dir.join(format!("{name}.tmp"));
        let blob = protect(bytes, &entropy_for(name))?;
        // A stale temporary file (or a link planted under its name) is removed itself, never followed.
        match sys::delete_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.os_code == Some(2) => {}
            Err(e) => return Err(e),
        }
        let h = sys::create_new_file(&tmp)?;
        let written = sys::write_all(&h, &blob).and_then(|()| sys::flush(&h));
        drop(h);
        if let Err(e) = written {
            let _ = sys::delete_file(&tmp);
            return Err(e);
        }
        sys::rename(&tmp, &path, true).inspect_err(|_| {
            let _ = sys::delete_file(&tmp);
        })
    }

    fn delete(&self, name: &str) -> Result<()> {
        let path = self.path_of(name)?;
        match sys::delete_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.os_code == Some(2) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests;
