//! This computer's bin and free space for the explorer (`docs/FILES-UI.md` 3.2, slice 2a), given to
//! `swapdesk_ui::local_disk` at start; both run on the interface backend's worker thread, never the UI thread.
//!
//! A thin adaptor: the Recycle Bin (`IFileOperation`, fixed disks only) and the free space live in
//! `swapdesk_sys_win::disk`; this maps its `TrashError` to the window's `ApiCode`.

use std::path::{Path, PathBuf};

use swapdesk_sys_win::disk::{self, TrashError};
use swapdesk_ui::ApiCode;
use swapdesk_ui::local_disk::LocalOs;

pub fn local_os() -> LocalOs {
    LocalOs { trash, free_bytes }
}

fn free_bytes(path: &Path) -> Option<u64> {
    disk::free_bytes(path)
}

/// Sends `paths` to the Recycle Bin. `Refused` when a place has no bin (nothing is deleted) or the system's warning
/// that an item would be destroyed was declined.
fn trash(paths: &[PathBuf]) -> Result<(), ApiCode> {
    disk::trash(paths).map_err(|e| match e {
        TrashError::NoBin | TrashError::Failed => ApiCode::Refused,
        TrashError::NotFound => ApiCode::NotFound,
        TrashError::NotPermitted => ApiCode::NotPermitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_on_the_system_disk_goes_to_the_bin_and_the_disk_has_free_space() {
        let dir = std::env::temp_dir().join(format!("swapdesk-trash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lixo.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(free_bytes(&dir).is_some_and(|b| b > 0));
        trash(std::slice::from_ref(&file)).expect("to the Recycle Bin");
        assert!(!file.exists(), "the file left its folder for the bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_maps_to_not_found_and_a_non_fixed_place_to_refused() {
        let gone = std::env::temp_dir().join("swapdesk-lane-test-shell-gone.txt");
        assert_eq!(trash(&[gone]), Err(ApiCode::NotFound));
        // A UNC path to this machine's administrative share is a network place: no Recycle Bin.
        let unc = PathBuf::from(r"\\localhost\C$\Windows\win.ini");
        if unc.exists() {
            assert_eq!(trash(std::slice::from_ref(&unc)), Err(ApiCode::Refused));
        }
    }
}
