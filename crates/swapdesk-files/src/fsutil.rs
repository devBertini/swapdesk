//! Small file-system helpers: the reparse-point check and the open calls that never follow one.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

/// True when `meta` describes a symbolic link, junction or any other reparse point (`FILE_ATTRIBUTE_REPARSE_POINT`
/// on Windows, a symlink elsewhere). Takes the metadata of the entry itself (`symlink_metadata`), never of its target.
pub fn is_reparse(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0 || meta.file_type().is_symlink()
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

/// Something exists at `path` (a dangling link counts).
pub fn exists_nofollow(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn no_follow(opts: &mut OpenOptions) {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the link itself, so the check below sees it.
        opts.custom_flags(0x0020_0000);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW on macOS.
        opts.custom_flags(0x0100);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = opts;
    }
}

/// What `open_rw_nofollow` does when nothing, or something, is at the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Create {
    /// The file must exist.
    No,
    /// The file must not exist yet.
    New,
    /// Open the file, creating it when missing.
    OrOpen,
}

/// Opens a regular file for reading and writing without following a reparse point. `Create::New` fails when
/// anything is there already. The opened handle is checked again, so a link swapped in after the caller's own check
/// is refused as well.
pub fn open_rw_nofollow(path: &Path, create: Create) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true);
    match create {
        Create::No => {}
        Create::New => {
            opts.create_new(true);
        }
        Create::OrOpen => {
            opts.create(true);
        }
    }
    no_follow(&mut opts);
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    if is_reparse(&meta) || !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}

/// Opens a regular file for reading only (the sending side: the user's own file, links are read through).
pub fn open_read(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(file)
}
