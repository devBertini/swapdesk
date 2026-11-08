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
    target: Target,
    service: String,
    folder: PathBuf,
}

impl MacSecretStore {
    /// Opens the store at `location`: makes the C5 folder (0700, checked owner). The keychain is not touched until the
    /// first call, so opening never raises a keychain prompt.
    pub fn open(location: &SecretLocation) -> Result<Self> {
        let target = match &location.keychain {
            KeychainLocation::Login => Target::Login,
            KeychainLocation::File(path) => Target::File(c_path(path)?),
        };
        let folder = ensure_folder(&location.folder, location.folder_owner)?;
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
        keychain::remove(&self.target, &self.service, None)
    }
}

/// Creates a throwaway keychain file for development and tests, protected by `password` and unlocked; it is not in the
/// user's search list, so it needs no GUI session. Pair it with [`delete_keychain_file`].
pub fn create_keychain_file(path: &Path, password: &str) -> Result<()> {
    keychain::create_file(&c_path(path)?, password)
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
        keychain::get(&self.target, &self.service, name)
    }

    fn store(&self, name: &str, bytes: &[u8]) -> Result<()> {
        check_name(name)?;
        keychain::set(&self.target, &self.service, name, bytes)
    }

    fn delete(&self, name: &str) -> Result<()> {
        check_name(name)?;
        keychain::remove(&self.target, &self.service, Some(name))
    }
}
