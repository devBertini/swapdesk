//! File-system helpers of the Windows glue: the free space of a volume (F-FILE-03).

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

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
