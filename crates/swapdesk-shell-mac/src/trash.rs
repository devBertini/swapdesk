//! This computer's Trash and free space for the explorer (`docs/FILES-UI.md` 3.2, slice 2a), given to
//! `swapdesk_ui::local_disk` at start; both run on the interface backend's worker thread.
//!
//! The Trash: `-[NSFileManager trashItemAtURL:resultingItemURL:error:]`, which "returns YES if the item at url was
//! successfully moved to the trash, or NO if the item was not moved to the trash" (Foundation `NSFileManager.h`, macOS
//! 10.8, read in the SDK 12/11/2026): an item it cannot move stays where it is, so nothing is ever destroyed. Free
//! space: `attributesOfFileSystemForPath:error:` and `NSFileSystemFreeSize`.

use std::path::{Path, PathBuf};

use objc2_foundation::{NSFileManager, NSFileSystemFreeSize, NSNumber, NSString, NSURL};
use swapdesk_ui::ApiCode;
use swapdesk_ui::local_disk::LocalOs;

/// `NSFeatureUnsupportedError`: the volume has no Trash (Foundation `FoundationErrors.h`).
const FEATURE_UNSUPPORTED: isize = 3328;
/// `NSFileNoSuchFileError` and `NSFileReadNoSuchFileError`.
const NO_SUCH_FILE: [isize; 2] = [4, 260];
/// `NSFileWriteNoPermissionError`.
const NO_PERMISSION: isize = 513;

pub fn local_os() -> LocalOs {
    LocalOs { trash, free_bytes }
}

fn free_bytes(path: &Path) -> Option<u64> {
    let manager = NSFileManager::defaultManager();
    let attrs = manager.attributesOfFileSystemForPath_error(&NSString::from_str(path.to_str()?)).ok()?;
    // SAFETY: the key is Foundation's constant.
    let value = attrs.objectForKey(unsafe { NSFileSystemFreeSize })?;
    let number = value.downcast::<NSNumber>().ok()?;
    Some(number.unsignedLongLongValue())
}

/// Moves `paths` to the Trash. `Refused` when a volume has no Trash; an item that could not be moved stays.
fn trash(paths: &[PathBuf]) -> Result<(), ApiCode> {
    let manager = NSFileManager::defaultManager();
    for p in paths {
        let text = p.to_str().ok_or(ApiCode::Invalid)?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(text));
        if let Err(e) = manager.trashItemAtURL_resultingItemURL_error(&url, None) {
            return Err(match e.code() {
                FEATURE_UNSUPPORTED => ApiCode::Refused,
                c if NO_SUCH_FILE.contains(&c) => ApiCode::NotFound,
                NO_PERMISSION => ApiCode::NotPermitted,
                _ => ApiCode::Internal,
            });
        }
    }
    Ok(())
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
