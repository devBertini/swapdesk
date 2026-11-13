//! File-system helpers of the Mac glue: the free space of a volume, the System Settings pane that grants access to
//! folders (F-FILE-03), the rename that never replaces and the quarantine mark of a downloaded file. macOS only.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Where the user grants a folder to the app: System Settings, Privacy & Security, Files & Folders (Downloads,
/// Documents, Desktop, removable and network volumes). A volume or folder that pane does not list needs Full Disk
/// Access (`Privacy_AllFiles`). Open it with `open <url>` or `NSWorkspace`.
pub const FILES_AND_FOLDERS_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders";
/// The Full Disk Access pane.
pub const FULL_DISK_ACCESS_PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// Free bytes the calling user can use on the volume that holds `path` (`statvfs`: blocks available to a non-root
/// user times the fragment size), `None` when the system cannot say.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is plain data for which all-zero is a valid value.
    let mut s: libc::statvfs = unsafe { core::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path that outlives the call and `s` is a valid out pointer.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    if rc != 0 {
        return None;
    }
    // The field types differ by platform (u32 or u64): widen both.
    #[allow(clippy::useless_conversion, clippy::unnecessary_cast)]
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64))
}

fn c_path(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the path"))
}

/// Renames `from` to `to` (a file, a folder or a link, itself and never its target) unless something is at `to`: then
/// it fails with `AlreadyExists` and changes nothing. `renamex_np` with `RENAME_EXCL` makes the check and the move one
/// step; a move to another volume fails with `CrossesDevices`. A volume that does not know `RENAME_EXCL` (`ENOTSUP`)
/// gets a look first and a plain rename, the best it allows.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (c_path(from)?, c_path(to)?);
    // SAFETY: both are NUL-terminated paths that outlive the call; `RENAME_EXCL` is a documented flag of `renamex_np`.
    let rc = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(libc::ENOTSUP) {
        return Err(err);
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(from, to)
}

/// The value of the quarantine attribute for a file this program saved from another machine: flags `0081` (the file was
/// downloaded; not yet approved by the user), the time in hexadecimal seconds, and the name of the agent that saved it.
fn quarantine_value(agent: &str, now_secs: u64) -> String {
    format!("0081;{now_secs:x};{agent};")
}

/// Marks `path` as downloaded, the way a browser does: the `com.apple.quarantine` extended attribute, which makes
/// Gatekeeper check a program or script opened from it. The caller treats a failure as non-fatal.
pub fn mark_quarantined(path: &Path, agent: &str) -> io::Result<()> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let value = quarantine_value(agent, now);
    let p = c_path(path)?;
    let name = CString::new("com.apple.quarantine").map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `p` and `name` are NUL-terminated and `value` is a buffer of the length passed; all outlive the call.
    // `XATTR_NOFOLLOW` marks the entry itself, never what a link points to.
    let rc = unsafe {
        libc::setxattr(p.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, libc::XATTR_NOFOLLOW)
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// The quarantine attribute of `path`, `None` when it has none.
pub fn quarantine_of(path: &Path) -> Option<String> {
    let p = c_path(path).ok()?;
    let name = CString::new("com.apple.quarantine").ok()?;
    let mut buf = [0u8; 256];
    // SAFETY: `p` and `name` are NUL-terminated; `buf` is a writable buffer of the length passed.
    let n = unsafe {
        libc::getxattr(p.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW)
    };
    usize::try_from(n).ok().map(|n| String::from_utf8_lossy(&buf[..n.min(buf.len())]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("swapdesk-sysmac-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn rename_noreplace_moves_and_never_replaces() {
        let d = dir("rename");
        std::fs::write(d.join("a"), b"a").unwrap();
        std::fs::write(d.join("b"), b"b").unwrap();
        assert_eq!(rename_noreplace(&d.join("a"), &d.join("b")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(d.join("a")).unwrap(), b"a");
        assert_eq!(std::fs::read(d.join("b")).unwrap(), b"b");
        rename_noreplace(&d.join("a"), &d.join("c")).unwrap();
        assert!(!d.join("a").exists() && d.join("c").exists());
        std::fs::create_dir(d.join("dir1")).unwrap();
        std::fs::create_dir(d.join("dir2")).unwrap();
        assert!(rename_noreplace(&d.join("dir1"), &d.join("dir2")).is_err());
        assert_eq!(rename_noreplace(&d.join("missing"), &d.join("z")).unwrap_err().kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_marked_as_downloaded_carries_the_quarantine_attribute() {
        let d = dir("quarantine");
        std::fs::write(d.join("a.bin"), b"x").unwrap();
        assert_eq!(quarantine_of(&d.join("a.bin")), None);
        mark_quarantined(&d.join("a.bin"), "Swapdesk").unwrap();
        let v = quarantine_of(&d.join("a.bin")).expect("the attribute");
        assert!(v.starts_with("0081;") && v.ends_with(";Swapdesk;"), "{v}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_quarantine_value_has_flags_time_and_agent() {
        assert_eq!(quarantine_value("Swapdesk", 255), "0081;ff;Swapdesk;");
    }

    #[test]
    fn the_temp_folder_has_a_volume_with_space() {
        assert!(free_bytes(&std::env::temp_dir()).is_some_and(|b| b > 0));
    }

    #[test]
    fn a_missing_folder_is_unknown() {
        assert_eq!(free_bytes(Path::new("/swapdesk/no/such/folder/b1")), None);
    }
}
