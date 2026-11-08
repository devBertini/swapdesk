//! The folder rules of C5.2 for macOS: the folder is created by this crate with mode 0700, owned by the expected user
//! (the current user in stage 23, root in stage 24), and a pre-existing one that is a symlink, is not a directory, or has
//! another owner is refused, never used and never followed.

use std::fs::{DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use swapdesk_platform::{ErrorKind, PlatformError, Result};

use crate::location::FolderOwner;

fn fail(kind: ErrorKind, op: &'static str, os_code: Option<i64>, detail: String) -> PlatformError {
    PlatformError { kind, op, os_code, detail }
}

fn io(op: &'static str, path: &Path, e: &std::io::Error) -> PlatformError {
    fail(ErrorKind::Os, op, e.raw_os_error().map(i64::from), format!("{}: {e}", path.display()))
}

fn expected_uid(owner: FolderOwner) -> u32 {
    match owner {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        FolderOwner::CurrentUser => unsafe { libc::geteuid() },
        FolderOwner::Root => 0,
    }
}

/// Makes sure `path` is a directory owned by `owner` with mode 0700 and returns it. Creates missing folders (every one
/// created gets 0700). An existing one owned by someone else, a symlink or a file is an error; an existing one owned
/// correctly but with a looser mode is tightened to 0700.
pub fn ensure_folder(path: &Path, owner: FolderOwner) -> Result<PathBuf> {
    let uid = expected_uid(owner);
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(fail(
                    ErrorKind::InvalidInput,
                    "ensure_folder",
                    None,
                    format!("{} is a symlink", path.display()),
                ));
            }
            if !meta.is_dir() {
                return Err(fail(
                    ErrorKind::InvalidInput,
                    "ensure_folder",
                    None,
                    format!("{} is not a folder", path.display()),
                ));
            }
            if meta.uid() != uid {
                return Err(fail(
                    ErrorKind::InvalidInput,
                    "ensure_folder",
                    Some(i64::from(meta.uid())),
                    format!("{} is owned by uid {}, expected {uid}", path.display(), meta.uid()),
                ));
            }
            if meta.mode() & 0o777 != 0o700 {
                std::fs::set_permissions(path, Permissions::from_mode(0o700)).map_err(|e| io("chmod", path, &e))?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            DirBuilder::new().recursive(true).mode(0o700).create(path).map_err(|e| io("create_dir", path, &e))?;
            // Re-read what was created: it must be ours, in case a parent was swapped meanwhile.
            let meta = std::fs::symlink_metadata(path).map_err(|e| io("stat", path, &e))?;
            if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != uid {
                return Err(fail(
                    ErrorKind::InvalidInput,
                    "ensure_folder",
                    None,
                    format!("{} changed while it was created", path.display()),
                ));
            }
        }
        Err(e) => return Err(io("stat", path, &e)),
    }
    Ok(path.to_path_buf())
}
