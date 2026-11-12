//! File-system helpers of the Mac glue: the free space of a volume and the System Settings pane that grants access to
//! folders (F-FILE-03). macOS only.

use std::ffi::CString;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_temp_folder_has_a_volume_with_space() {
        assert!(free_bytes(&std::env::temp_dir()).is_some_and(|b| b > 0));
    }

    #[test]
    fn a_missing_folder_is_unknown() {
        assert_eq!(free_bytes(Path::new("/swapdesk/no/such/folder/b1")), None);
    }
}
