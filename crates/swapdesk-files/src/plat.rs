//! What the disk work asks of the operating system beyond `std`: renaming and copying that never replace (the system
//! makes the check and the change one step, so two things arriving under one name cannot both win), and the mark a
//! file carries when it came from another machine. Windows and the Mac call their glue crates; any other system gets
//! the closest `std` can do, with the gap said where it is.

use std::io;
use std::path::Path;

/// Renames `from` to `to` unless something is at `to` (`AlreadyExists`, nothing changed); a move to another volume is
/// `CrossesDevices`. A file, a folder or a link is moved as itself.
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        swapdesk_sys_win::fs::rename_noreplace(from, to)
    }
    #[cfg(target_os = "macos")]
    {
        swapdesk_sys_mac::fs::rename_noreplace(from, to)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // No portable no-replace rename in `std`: a look first, then the rename (a race can still replace here).
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        std::fs::rename(from, to)
    }
}

/// Copies the file `from` to `to` unless something is at `to` (`AlreadyExists`, nothing changed).
pub(crate) fn copy_file_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        swapdesk_sys_win::fs::copy_noreplace(from, to)
    }
    #[cfg(not(windows))]
    {
        use std::fs::{File, OpenOptions};
        let mut src = File::open(from)?;
        let meta = src.metadata()?;
        // `create_new` is `O_EXCL`: the system refuses an existing target, a dangling link included.
        let mut dest = OpenOptions::new().write(true).create_new(true).open(to)?;
        let copied = io::copy(&mut src, &mut dest).and_then(|_| {
            dest.set_permissions(meta.permissions())?;
            if let Ok(mtime) = meta.modified() {
                let _ = dest.set_modified(mtime);
            }
            dest.sync_all()
        });
        if copied.is_err() {
            // The file is ours (it did not exist a moment ago): a half copy is not left behind.
            drop(dest);
            let _ = std::fs::remove_file(to);
        }
        copied
    }
}

/// The name of this program in the quarantine record of a file it saved (macOS).
#[cfg(target_os = "macos")]
const QUARANTINE_AGENT: &str = "Swapdesk";

/// Marks the file at `path` as one that came from another machine, the way a browser marks a download: on Windows the
/// `Zone.Identifier` stream with `ZoneId=3` (the Internet zone, so SmartScreen and Office's Protected View look at
/// it), on the Mac the `com.apple.quarantine` attribute (Gatekeeper). It stays with the file when it is renamed, so
/// the mark goes on before the last rename and there is no moment when the finished file is unmarked. A volume with
/// no streams (FAT) or attributes cannot carry it: the caller treats an error as non-fatal.
pub(crate) fn mark_downloaded(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::io::Write;
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(std::path::PathBuf::from(stream))?;
        f.write_all(b"[ZoneTransfer]\r\nZoneId=3\r\n")
    }
    #[cfg(target_os = "macos")]
    {
        swapdesk_sys_mac::fs::mark_quarantined(path, QUARANTINE_AGENT)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        Ok(())
    }
}

/// Whether `path` carries the mark of [`mark_downloaded`] (the test side of it).
#[cfg(all(test, any(windows, target_os = "macos")))]
pub(crate) fn is_marked_downloaded(path: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        std::fs::read_to_string(std::path::PathBuf::from(stream)).is_ok_and(|t| t.contains("ZoneId=3"))
    }
    #[cfg(target_os = "macos")]
    {
        swapdesk_sys_mac::fs::quarantine_of(path).is_some_and(|v| v.starts_with("0081;"))
    }
}
