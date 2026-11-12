//! Access to the receiving folder (F-FILE-03, third rule): macOS keeps Downloads, Documents, Desktop and removable or
//! network volumes behind a per-app grant ("Files and Folders", or "Full Disk Access"). Without it the system answers
//! `EPERM` ("Operation not permitted") to a read or a create inside the folder, which the engine can only report as a
//! failed write. This module turns that answer into a state the Mac can show: which folder, and that the user must
//! grant access. Rust maps both `EPERM` and `EACCES` to `ErrorKind::PermissionDenied`, so the check is the same on
//! every system (a Windows folder the user may not write is `Denied` as well).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// What the receiving folder lets this process do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveAccess {
    /// The folder can be read.
    Granted,
    /// The system refused (`EPERM` or `EACCES`): on macOS the grant is missing.
    Denied,
    /// The folder is not there (it is created on first use under the default receive folder).
    Missing,
    /// Any other failure.
    Failed,
}

/// Classifies the error of a read or create inside the receiving folder.
pub fn classify_error(e: &io::Error) -> ReceiveAccess {
    match e.kind() {
        io::ErrorKind::PermissionDenied => ReceiveAccess::Denied,
        io::ErrorKind::NotFound => ReceiveAccess::Missing,
        _ => ReceiveAccess::Failed,
    }
}

/// Reads `dir` once to learn whether this process may use it.
pub fn check_folder(dir: &Path) -> ReceiveAccess {
    match std::fs::read_dir(dir) {
        Ok(_) => ReceiveAccess::Granted,
        Err(e) => classify_error(&e),
    }
}

/// The folder a disk operation was last refused in, until an operation there works again. The disk's owner (the Mac
/// helper) keeps one and the app asks it which folder to name in "give Swapdesk access to {folder}".
#[derive(Debug, Default)]
pub struct AccessState {
    denied: Mutex<Option<PathBuf>>,
}

impl AccessState {
    fn lock(&self) -> MutexGuard<'_, Option<PathBuf>> {
        self.denied.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// An operation in `dir` failed with `e`: remembered when it was a refusal.
    pub fn note_error(&self, dir: &Path, e: &io::Error) {
        if classify_error(e) == ReceiveAccess::Denied {
            *self.lock() = Some(dir.to_path_buf());
        }
    }

    /// An operation in `dir` worked: a refusal remembered for it is over.
    pub fn note_ok(&self, dir: &Path) {
        let mut g = self.lock();
        if g.as_deref() == Some(dir) {
            *g = None;
        }
    }

    /// The folder that refused this process, if one did and none has worked since.
    pub fn denied(&self) -> Option<PathBuf> {
        self.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_classified() {
        assert_eq!(classify_error(&io::Error::from(io::ErrorKind::PermissionDenied)), ReceiveAccess::Denied);
        assert_eq!(classify_error(&io::Error::from(io::ErrorKind::NotFound)), ReceiveAccess::Missing);
        assert_eq!(classify_error(&io::Error::from(io::ErrorKind::Other)), ReceiveAccess::Failed);
    }

    #[test]
    fn a_refusal_is_remembered_until_the_folder_works() {
        let s = AccessState::default();
        let d = Path::new("Downloads/Swapdesk");
        s.note_error(d, &io::Error::from(io::ErrorKind::Other));
        assert_eq!(s.denied(), None);
        s.note_error(d, &io::Error::from(io::ErrorKind::PermissionDenied));
        assert_eq!(s.denied().as_deref(), Some(d));
        s.note_ok(Path::new("elsewhere"));
        assert!(s.denied().is_some());
        s.note_ok(d);
        assert_eq!(s.denied(), None);
    }

    #[test]
    fn a_readable_folder_is_granted_and_a_missing_one_is_missing() {
        let dir = std::env::temp_dir();
        assert_eq!(check_folder(&dir), ReceiveAccess::Granted);
        assert_eq!(check_folder(&dir.join("swapdesk-no-such-folder-b1")), ReceiveAccess::Missing);
    }
}
