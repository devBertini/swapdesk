//! `MacSecretStore`: the `SecretStore` over keychain items plus the C5 folder.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use swapdesk_platform::{ErrorKind, PlatformError, Result, SecretStore};

use crate::folder::ensure_folder;
use crate::keychain::{self, Target};
use crate::location::{KeychainLocation, SecretLocation, check_name};

/// One generic-password item per secret name: service = `location.service`, account = the name, as given ("identity.bin").
/// The record's bytes are the item's value, exactly as the callers encode them (C5.1).
pub struct MacSecretStore {
    target: Backing,
    service: String,
    folder: PathBuf,
}

enum Backing {
    Keychain(Target),
    /// Files in this folder; legacy items are looked up once in the login keychain.
    Files(PathBuf),
}

impl MacSecretStore {
    /// Opens the store at `location`: makes the C5 folder (0700, checked owner). The keychain is not touched until the
    /// first call, so opening never raises a keychain prompt.
    pub fn open(location: &SecretLocation) -> Result<Self> {
        let folder = ensure_folder(&location.folder, location.folder_owner)?;
        let target = match &location.keychain {
            KeychainLocation::Login => Backing::Keychain(Target::Login),
            KeychainLocation::File(path) => Backing::Keychain(Target::File(c_path(path)?)),
            KeychainLocation::Files => {
                Backing::Files(ensure_folder(&folder.join(SECRETS_FOLDER), location.folder_owner)?)
            }
        };
        Ok(Self { target, service: location.service.clone(), folder })
    }

    /// The C5 folder, for the audit and trace logs.
    pub fn dir(&self) -> &Path {
        &self.folder
    }

    /// The service attribute of this store's items.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// Deletes every item of this store's service (uninstall, and the cleanup of a test's unique service).
    pub fn delete_all(&self) -> Result<()> {
        match &self.target {
            Backing::Keychain(t) => keychain::remove(t, &self.service, None),
            Backing::Files(dir) => {
                for entry in std::fs::read_dir(dir).map_err(|e| io_err("delete_all", dir, &e))? {
                    let path = entry.map_err(|e| io_err("delete_all", dir, &e))?.path();
                    std::fs::remove_file(&path).map_err(|e| io_err("delete_all", &path, &e))?;
                }
                Ok(())
            }
        }
    }
}

/// The subfolder of the C5 folder that holds the secret files.
pub const SECRETS_FOLDER: &str = "secrets";

fn io_err(op: &'static str, path: &Path, e: &std::io::Error) -> PlatformError {
    PlatformError {
        kind: ErrorKind::Os,
        op,
        os_code: e.raw_os_error().map(i64::from),
        detail: format!("{}: {e}", path.display()),
    }
}

fn read_file(dir: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    let path = dir.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("SecretStore::load", &path, &e)),
    }
}

/// Writes `<name>.tmp` with mode 0600, syncs it and renames it over `<name>`: a crash leaves the old or the new value.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| io_err("SecretStore::store", &tmp, &e))?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| io_err("SecretStore::store", &tmp, &e))?;
    std::fs::rename(&tmp, &path).map_err(|e| io_err("SecretStore::store", &path, &e))
}

/// Creates a throwaway keychain file for development and tests, protected by `password` and unlocked; it is not in the
/// user's search list, so it needs no GUI session. macOS locks such a keychain after five minutes without use: every
/// call of this process on it unlocks it again with the password, without a prompt. Pair it with
/// [`delete_keychain_file`].
pub fn create_keychain_file(path: &Path, password: &str) -> Result<()> {
    keychain::create_file(&c_path(path)?, password)
}

/// Locks a keychain file, as macOS does by itself after five minutes without use (for tests).
pub fn lock_keychain_file(path: &Path) -> Result<()> {
    keychain::lock_file(&c_path(path)?)
}

/// Deletes a keychain file made by [`create_keychain_file`].
pub fn delete_keychain_file(path: &Path) -> Result<()> {
    keychain::delete_file(&c_path(path)?)
}

fn c_path(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| PlatformError {
        kind: ErrorKind::InvalidInput,
        op: "MacSecretStore",
        os_code: None,
        detail: format!("{} holds a NUL byte", path.display()),
    })
}

impl SecretStore for MacSecretStore {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>> {
        check_name(name)?;
        match &self.target {
            Backing::Keychain(t) => keychain::get(t, &self.service, name),
            Backing::Files(dir) => {
                if let Some(bytes) = read_file(dir, name)? {
                    return Ok(Some(bytes));
                }
                // D31 migration: a secret written by an earlier version is still a login-keychain item. It is read once
                // (macOS may ask once) and kept as a file from then on; the item is left in place.
                match keychain::get(&Target::Login, &self.service, name) {
                    Ok(Some(bytes)) => {
                        write_file(dir, name, &bytes)?;
                        Ok(Some(bytes))
                    }
                    // Nothing there, or no usable login keychain (an SSH session, a denied prompt): nothing to migrate.
                    Ok(None) | Err(_) => Ok(None),
                }
            }
        }
    }

    fn store(&self, name: &str, bytes: &[u8]) -> Result<()> {
        check_name(name)?;
        match &self.target {
            Backing::Keychain(t) => keychain::set(t, &self.service, name, bytes),
            Backing::Files(dir) => write_file(dir, name, bytes),
        }
    }

    fn delete(&self, name: &str) -> Result<()> {
        check_name(name)?;
        match &self.target {
            Backing::Keychain(t) => keychain::remove(t, &self.service, Some(name)),
            Backing::Files(dir) => match std::fs::remove_file(dir.join(name)) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err("SecretStore::delete", &dir.join(name), &e)),
            },
        }
    }
}
