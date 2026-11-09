//! Where the macOS secrets live. Pure: the same type describes the stage 23 user-level store and the stage 24 root store.

use std::path::{Path, PathBuf};

use swapdesk_platform::{ErrorKind, PlatformError, Result};

/// The keychain item's service attribute: "Swapdesk" (C5.2).
pub const SERVICE_NAME: &str = "Swapdesk";
/// The folder's name inside `Application Support`.
pub const FOLDER_NAME: &str = "Swapdesk";

/// Which keychain holds the items.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeychainLocation {
    /// The logged-in user's login keychain (the default keychain of the user's search list). Stage 23.
    Login,
    /// One keychain file by its path. Stage 24 uses `/Library/Keychains/System.keychain` for the root daemon (the
    /// folder and owner change with it, see [`FolderOwner`]); tests use a throwaway keychain, which also works where the
    /// login keychain cannot be reached (an SSH session has no GUI session to unlock it).
    File(PathBuf),
    /// No keychain: each secret is a file of its own (mode 0600) in the `secrets` subfolder (0700) of the C5 folder,
    /// readable by this user's processes only. A legacy item still in the login keychain is read once and copied
    /// there (one keychain prompt at most, ever). D31: the login keychain ties an item's "Always Allow" to the exact
    /// binary of a self-signed app, so every rebuild or update asked again and blocked the app at start.
    Files,
}

/// Who must own the C5 folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderOwner {
    /// The user this process runs as (stage 23).
    CurrentUser,
    /// root, as C5.2 says of the daemon's folder (stage 24).
    Root,
}

/// The place of a store: the keychain, the item's service name and the C5 folder. Opening a store from another
/// `SecretLocation` is all stage 24 changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretLocation {
    pub keychain: KeychainLocation,
    /// The service attribute of every item of this store ([`SERVICE_NAME`] in production; tests use a unique one so
    /// they never touch the real items).
    pub service: String,
    /// The C5 folder (audit and trace logs): created 0700, owned by `folder_owner`, never a symlink.
    pub folder: PathBuf,
    pub folder_owner: FolderOwner,
}

impl SecretLocation {
    /// The location of the logged-in user's app: secrets as files in `~/Library/Application Support/Swapdesk/secrets`
    /// ([`KeychainLocation::Files`], D31), service "Swapdesk" for the legacy login-keychain items it migrates.
    /// `home` is the user's home folder.
    pub fn login(home: &Path) -> Self {
        Self {
            keychain: KeychainLocation::Files,
            service: SERVICE_NAME.to_string(),
            folder: home.join("Library").join("Application Support").join(FOLDER_NAME),
            folder_owner: FolderOwner::CurrentUser,
        }
    }

    /// The same with another service name and folder: tests and development instances.
    #[must_use]
    pub fn with(mut self, service: impl Into<String>, folder: impl Into<PathBuf>) -> Self {
        self.service = service.into();
        self.folder = folder.into();
        self
    }
}

const RESERVED_SUFFIX: &str = ".tmp";

/// A secret's name is one plain file-like name: letters, digits, `.`, `_` and `-`, at most 64 bytes, not starting or
/// ending with a dot. The same rule as the Windows store, so a name valid on one OS is valid on the other.
pub fn check_name(name: &str) -> Result<()> {
    let plain = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.ends_with(RESERVED_SUFFIX)
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if plain {
        Ok(())
    } else {
        Err(PlatformError {
            kind: ErrorKind::InvalidInput,
            op: "SecretStore",
            os_code: None,
            detail: format!("{name:?} is not a valid secret name"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_location_is_the_users_application_support_folder() {
        let l = SecretLocation::login(Path::new("/Users/someone"));
        assert_eq!(l.keychain, KeychainLocation::Files);
        assert_eq!(l.service, "Swapdesk");
        assert_eq!(l.folder, Path::new("/Users/someone/Library/Application Support/Swapdesk"));
        assert_eq!(l.folder_owner, FolderOwner::CurrentUser);
    }

    #[test]
    fn with_replaces_the_service_and_the_folder_only() {
        let l = SecretLocation::login(Path::new("/h")).with("swapdesk-test-1", "/tmp/x");
        assert_eq!((l.service.as_str(), l.folder.as_path()), ("swapdesk-test-1", Path::new("/tmp/x")));
        assert_eq!(l.keychain, KeychainLocation::Files);
    }

    #[test]
    fn names_follow_the_windows_rule() {
        for ok in ["identity.bin", "trust.bin", "settings.bin", "a", "A_b-c.d"] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "trailing.", "a/b", "a\\b", "x.tmp", "sp ace", "ü", &"n".repeat(65)] {
            assert_eq!(check_name(bad).unwrap_err().kind, ErrorKind::InvalidInput, "{bad:?}");
        }
    }
}
