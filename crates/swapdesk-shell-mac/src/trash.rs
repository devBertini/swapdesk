//! This computer's Trash and free space for the explorer (`docs/FILES-UI.md` 3.2, slice 2a), given to
//! `swapdesk_ui::local_disk` at start; both run on the interface backend's worker thread.
//!
//! A thin adaptor: the Trash (`trashItemAtURL`) and the free space live in `swapdesk_sys_mac::disk`; this maps its
//! `TrashError` to the window's `ApiCode`.

use std::path::{Path, PathBuf};

use swapdesk_sys_mac::disk::{self, TrashError};
use swapdesk_ui::ApiCode;
use swapdesk_ui::local_disk::LocalOs;

pub fn local_os() -> LocalOs {
    LocalOs { trash, free_bytes }
}

fn free_bytes(path: &Path) -> Option<u64> {
    disk::free_bytes(path)
}

/// Moves `paths` to the Trash. `Refused` when a volume has no Trash; an item that could not be moved stays.
fn trash(paths: &[PathBuf]) -> Result<(), ApiCode> {
    disk::trash(paths).map_err(|e| match e {
        TrashError::NoBin => ApiCode::Refused,
        TrashError::NotFound => ApiCode::NotFound,
        TrashError::NotPermitted => ApiCode::NotPermitted,
        TrashError::Failed => ApiCode::Internal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_in_the_home_folder_goes_to_the_trash_and_the_disk_has_free_space() {
        let home = std::env::var("HOME").expect("HOME");
        let dir = PathBuf::from(home).join(format!(".swapdesk-trash-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lixo.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(free_bytes(&dir).is_some_and(|b| b > 0));
        trash(std::slice::from_ref(&file)).expect("to the Trash");
        assert!(!file.exists(), "the file left its folder for the Trash");
        assert_eq!(trash(&[dir.join("gone.txt")]), Err(ApiCode::NotFound));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
