//! File-system helpers of the Windows glue: the free space of a volume (F-FILE-03) and the two calls that never replace
//! what is already at the destination (the file server's "never replace" rule, decided by the system in one step).

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    COPY_FILE_FAIL_IF_EXISTS, CopyFileExW, GetDiskFreeSpaceExW, MoveFileExW,
};

/// A NUL-terminated UTF-16 copy of `p`; `InvalidInput` when it holds a NUL.
fn wide(p: &Path) -> io::Result<Vec<u16>> {
    let mut w: Vec<u16> = p.as_os_str().encode_wide().collect();
    if w.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "a NUL in the path"));
    }
    w.push(0);
    Ok(w)
}

/// Renames `from` to `to` (a file, a folder or a link, itself and never its target) unless something is at `to`: then
/// it fails with `AlreadyExists` and changes nothing. `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, so the system
/// makes the check and the move one step; with no `MOVEFILE_COPY_ALLOWED` a move to another volume fails with
/// `CrossesDevices` instead of copying.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (wide(from)?, wide(to)?);
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call; flags 0 is the documented "no replace, no
    // copy" move.
    let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Copies the file `from` to `to` unless something is at `to`: then it fails with `AlreadyExists` and changes nothing
/// (`CopyFileExW` with `COPY_FILE_FAIL_IF_EXISTS`; the alternate streams and attributes come along, as with a copy in
/// the shell).
pub fn copy_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (wide(from)?, wide(to)?);
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call; no progress routine, no data, no cancel
    // flag (all documented as optional).
    let ok = unsafe {
        CopyFileExW(
            from.as_ptr(),
            to.as_ptr(),
            None,
            core::ptr::null(),
            core::ptr::null_mut(),
            COPY_FILE_FAIL_IF_EXISTS,
        )
    };
    if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Free bytes the calling user can use on the volume that holds `path` (a folder; quotas count), `None` when the
/// system cannot say (the path does not exist, the volume is offline).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return None;
    }
    wide.push(0);
    let mut available: u64 = 0;
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call; `available` is a valid out pointer and
    // the two other out pointers are documented as optional (null).
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, core::ptr::null_mut(), core::ptr::null_mut()) };
    (ok != 0).then_some(available)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("swapdesk-syswin-fs-{tag}-{}", std::process::id()));
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
        // A folder onto a folder and a file onto a folder are refused as well.
        std::fs::create_dir(d.join("dir1")).unwrap();
        std::fs::create_dir(d.join("dir2")).unwrap();
        assert!(rename_noreplace(&d.join("dir1"), &d.join("dir2")).is_err());
        assert!(rename_noreplace(&d.join("c"), &d.join("dir2")).is_err());
        assert!(d.join("dir1").is_dir() && d.join("c").is_file());
        assert_eq!(rename_noreplace(&d.join("missing"), &d.join("z")).unwrap_err().kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_noreplace_refuses_a_nul_in_a_path() {
        assert_eq!(
            rename_noreplace(Path::new("C:/a\0b"), Path::new("C:/c")).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn copy_noreplace_copies_and_never_replaces() {
        let d = dir("copy");
        std::fs::write(d.join("a"), b"from a").unwrap();
        std::fs::write(d.join("b"), b"keep me").unwrap();
        assert_eq!(copy_noreplace(&d.join("a"), &d.join("b")).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(d.join("b")).unwrap(), b"keep me");
        copy_noreplace(&d.join("a"), &d.join("c")).unwrap();
        assert_eq!(std::fs::read(d.join("c")).unwrap(), b"from a");
        assert_eq!(std::fs::read(d.join("a")).unwrap(), b"from a");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_temp_folder_has_a_volume_with_space() {
        let free = free_bytes(&std::env::temp_dir()).expect("free space of the temp volume");
        assert!(free > 0);
    }

    #[test]
    fn a_missing_folder_is_unknown() {
        assert_eq!(free_bytes(Path::new(r"Z:/swapdesk/no/such/folder/b1")), None);
    }

    #[test]
    fn a_nul_in_the_path_is_refused() {
        assert_eq!(free_bytes(Path::new("C:/a\0b")), None);
    }
}
