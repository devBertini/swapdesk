//! What this Mac gives the explorer's wider reach (`docs/FILES-UI.md` 3.2, 7.4, 8.1): the mounted volumes with an
//! identity of their own, the user's well-known folders, the protected folders and the Trash. macOS only; plain
//! functions that the app's `SystemDisk` adaptor and the interface's local disk call.
//!
//! Volumes: `-[NSFileManager mountedVolumeURLsIncludingResourceValuesForKeys:options:]` with
//! `NSVolumeEnumerationSkipHiddenVolumes`; network volumes (`NSURLVolumeIsLocalKey` false) are left out in 1.1 (8.2).
//!
//! The Trash: `-[NSFileManager trashItemAtURL:resultingItemURL:error:]`, which "returns YES if the item at url was
//! successfully moved to the trash, or NO if the item was not moved to the trash" (Foundation `NSFileManager.h`, macOS
//! 10.8, read in the SDK 12/11/2026): an item it cannot move stays where it is, so nothing is ever destroyed.

use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{
    NSArray, NSDictionary, NSFileManager, NSNumber, NSSearchPathDirectory, NSSearchPathDomainMask, NSString, NSURL,
    NSURLVolumeAvailableCapacityKey, NSURLVolumeIsEjectableKey, NSURLVolumeIsLocalKey, NSURLVolumeIsReadOnlyKey,
    NSURLVolumeIsRemovableKey, NSURLVolumeLocalizedNameKey, NSURLVolumeTotalCapacityKey, NSURLVolumeUUIDStringKey,
    NSVolumeEnumerationOptions,
};
use swapdesk_proto::{PlaceKind, VolumeKind};

pub use crate::fs::free_bytes;

/// `NSFeatureUnsupportedError`: the volume has no Trash (Foundation `FoundationErrors.h`).
const FEATURE_UNSUPPORTED: isize = 3328;
/// `NSFileNoSuchFileError` and `NSFileReadNoSuchFileError`.
const NO_SUCH_FILE: [isize; 2] = [4, 260];
/// `NSFileWriteNoPermissionError`.
const NO_PERMISSION: isize = 513;

/// A volume mounted now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskVolume {
    /// The volume UUID, else the mount path: what makes the volume itself across a removal.
    pub identity: String,
    /// `/`, `/Volumes/Backup`.
    pub root: PathBuf,
    pub kind: VolumeKind,
    pub label: String,
    /// Always `None` on the Mac.
    pub letter: Option<u8>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Local writable volumes; the Trash itself still refuses safely where it has none.
    pub has_bin: bool,
}

/// Why `trash` did not move the items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashError {
    /// The volume has no Trash.
    NoBin,
    NotFound,
    NotPermitted,
    Failed,
}

type Values = Retained<NSDictionary<NSString, AnyObject>>;

fn number(values: &Values, key: &NSString) -> Option<Retained<NSNumber>> {
    values.objectForKey(key)?.downcast::<NSNumber>().ok()
}

fn flag(values: &Values, key: &NSString) -> Option<bool> {
    number(values, key).map(|n| n.boolValue())
}

fn text(values: &Values, key: &NSString) -> Option<String> {
    Some(values.objectForKey(key)?.downcast::<NSString>().ok()?.to_string())
}

fn path_of(url: &NSURL) -> Option<PathBuf> {
    Some(PathBuf::from(url.path()?.to_string()))
}

/// The volumes to publish: local, not hidden; network volumes left out.
pub fn volumes() -> Vec<DiskVolume> {
    let manager = NSFileManager::defaultManager();
    // SAFETY: the keys are Foundation's constants.
    let keys = unsafe {
        NSArray::from_slice(&[
            NSURLVolumeUUIDStringKey,
            NSURLVolumeLocalizedNameKey,
            NSURLVolumeTotalCapacityKey,
            NSURLVolumeAvailableCapacityKey,
            NSURLVolumeIsLocalKey,
            NSURLVolumeIsRemovableKey,
            NSURLVolumeIsEjectableKey,
            NSURLVolumeIsReadOnlyKey,
        ])
    };
    let Some(urls) = manager.mountedVolumeURLsIncludingResourceValuesForKeys_options(
        Some(&keys),
        NSVolumeEnumerationOptions::SkipHiddenVolumes,
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for url in urls.iter() {
        let (Some(root), Ok(values)) = (path_of(&url), url.resourceValuesForKeys_error(&keys)) else {
            continue;
        };
        // SAFETY: Foundation's constants.
        let (uuid, name, total, free, local, removable, ejectable, read_only) = unsafe {
            (
                text(&values, NSURLVolumeUUIDStringKey),
                text(&values, NSURLVolumeLocalizedNameKey),
                number(&values, NSURLVolumeTotalCapacityKey),
                number(&values, NSURLVolumeAvailableCapacityKey),
                flag(&values, NSURLVolumeIsLocalKey),
                flag(&values, NSURLVolumeIsRemovableKey),
                flag(&values, NSURLVolumeIsEjectableKey),
                flag(&values, NSURLVolumeIsReadOnlyKey),
            )
        };
        let local = local.unwrap_or(true);
        if !local {
            continue;
        }
        let kind = if root == Path::new("/") {
            VolumeKind::System
        } else if removable.unwrap_or(false) || ejectable.unwrap_or(false) {
            VolumeKind::Removable
        } else {
            VolumeKind::Fixed
        };
        out.push(DiskVolume {
            identity: uuid.filter(|u| !u.is_empty()).unwrap_or_else(|| root.to_string_lossy().into_owned()),
            label: name.unwrap_or_default(),
            kind,
            letter: None,
            total_bytes: total.map_or(0, |n| n.unsignedLongLongValue()),
            free_bytes: free.map_or(0, |n| n.unsignedLongLongValue()),
            has_bin: !read_only.unwrap_or(false),
            root,
        });
    }
    out
}

/// The user's well-known folders that exist on this Mac (Movies is the Videos place). `Received` is the service's own
/// folder, not listed here.
pub fn places() -> Vec<(PlaceKind, PathBuf)> {
    let manager = NSFileManager::defaultManager();
    [
        (PlaceKind::Desktop, NSSearchPathDirectory::DesktopDirectory),
        (PlaceKind::Documents, NSSearchPathDirectory::DocumentDirectory),
        (PlaceKind::Downloads, NSSearchPathDirectory::DownloadsDirectory),
        (PlaceKind::Pictures, NSSearchPathDirectory::PicturesDirectory),
        (PlaceKind::Music, NSSearchPathDirectory::MusicDirectory),
        (PlaceKind::Videos, NSSearchPathDirectory::MoviesDirectory),
    ]
    .into_iter()
    .filter_map(|(kind, dir)| {
        let urls = manager.URLsForDirectory_inDomains(dir, NSSearchPathDomainMask::UserDomainMask);
        let url = urls.iter().next()?;
        let path = path_of(&url)?;
        path.is_dir().then_some((kind, path))
    })
    .collect()
}

/// Where every change is refused: the system's own folders and the root of the system volume.
pub fn protected() -> Vec<PathBuf> {
    ["/System", "/Library", "/Applications", "/usr", "/bin", "/sbin", "/private", "/"]
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Moves `paths` to the Trash. `NoBin` when a volume has none; an item that could not be moved stays where it is.
pub fn trash(paths: &[PathBuf]) -> Result<(), TrashError> {
    let manager = NSFileManager::defaultManager();
    for p in paths {
        let text = p.to_str().ok_or(TrashError::Failed)?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(text));
        if let Err(e) = manager.trashItemAtURL_resultingItemURL_error(&url, None) {
            return Err(match e.code() {
                FEATURE_UNSUPPORTED => TrashError::NoBin,
                c if NO_SUCH_FILE.contains(&c) => TrashError::NotFound,
                NO_PERMISSION => TrashError::NotPermitted,
                _ => TrashError::Failed,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_the_system_volume_with_an_identity() {
        let vols = volumes();
        let sys = vols.iter().find(|v| v.root == Path::new("/")).expect("the root volume");
        assert_eq!(sys.kind, VolumeKind::System);
        assert!(!sys.identity.is_empty());
        assert!(sys.total_bytes > 0 && sys.letter.is_none());
        assert!(vols.iter().all(|v| v.kind != VolumeKind::Network));
    }

    #[test]
    fn the_places_include_documents_and_all_exist() {
        let places = places();
        assert!(places.iter().any(|(k, _)| *k == PlaceKind::Documents));
        assert!(places.iter().all(|(k, p)| *k != PlaceKind::Received && p.is_dir()));
    }

    #[test]
    fn the_protected_folders_include_the_system_and_the_root() {
        let prot = protected();
        assert!(prot.contains(&PathBuf::from("/System")) && prot.contains(&PathBuf::from("/")));
    }

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
        assert_eq!(trash(&[dir.join("gone.txt")]), Err(TrashError::NotFound));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
