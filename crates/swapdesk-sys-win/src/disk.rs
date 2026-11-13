//! What this computer's operating system gives the explorer's wider reach (`docs/FILES-UI.md` 3.2, 7.4, 8.1): the
//! mounted volumes with an identity of their own, the user's well-known folders, the protected folders and the
//! Recycle Bin. Plain functions; the helper's `SystemDisk` adaptor and the interface's local disk call them.
//!
//! Network volumes (`DRIVE_REMOTE`) are left out in 1.1 (8.2: LAN-only stays about the two paired machines).
//!
//! The bin: `IFileOperation` with `FOFX_RECYCLEONDELETE` ("send it to the Recycle Bin rather than permanently
//! deleting it") and `FOF_WANTNUKEWARNING` ("send a warning if a file or folder is being destroyed during a delete
//! operation rather than recycled"), learn.microsoft.com/windows/win32/api/shobjidl_core/
//! nf-shobjidl_core-ifileoperation-setoperationflags, read 12/11/2026. Microsoft documents recycling only "if
//! possible" (`TSF_DELETE_RECYCLE_IF_POSSIBLE`) and no way to ask a volume first, so a place that is not a fixed disk is
//! refused before anything happens (`NoBin`), and the nuke warning stays as the last guard: nothing is ever destroyed
//! in silence.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE,
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures, FOLDERID_ProgramData,
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Videos, FOLDERID_Windows, FileOperation, IFileOperation,
    IShellItem, KNOWN_FOLDER_FLAG, SHCreateItemFromParsingName, SHGetKnownFolderPath,
};
use windows::core::{GUID, PCWSTR};

pub use crate::fs::free_bytes;
use swapdesk_proto::{PlaceKind, VolumeKind};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_CDROM: u32 = 5;
const DRIVE_RAMDISK: u32 = 6;

/// A volume mounted now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskVolume {
    /// The `\\?\Volume{GUID}\` path, else the serial number in hex: what makes the volume itself across a removal.
    pub identity: String,
    /// `D:\`.
    pub root: PathBuf,
    pub kind: VolumeKind,
    pub label: String,
    /// `b'A'..=b'Z'`.
    pub letter: Option<u8>,
    pub total_bytes: u64,
    pub free_bytes: u64,
    /// Only fixed disks have a Recycle Bin that `trash` will use.
    pub has_bin: bool,
}

/// Why `trash` did not move the items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashError {
    /// A place has no bin (not a fixed disk), or the system's warning that an item would be destroyed was declined.
    NoBin,
    NotFound,
    NotPermitted,
    Failed,
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

fn text_of(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end]).to_string_lossy().into_owned()
}

/// The volumes to publish: fixed, removable, optical with a medium and RAM disks; network and unknown left out.
pub fn volumes() -> Vec<DiskVolume> {
    // SAFETY: no arguments; returns the drive bitmask.
    let mask = unsafe { GetLogicalDrives() };
    let system_letter = known_folder(&FOLDERID_Windows)
        .and_then(|p| p.to_str().and_then(|s| s.as_bytes().first().copied()))
        .map(|b| b.to_ascii_uppercase());
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = b'A' + i;
        let root = PathBuf::from(format!("{}:\\", letter as char));
        let w = wide(&root);
        // SAFETY: `w` is a NUL-terminated root path that outlives the call.
        let drive = unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) };
        let mut label = [0u16; 261];
        let mut serial = 0u32;
        // SAFETY: `w` is NUL-terminated; the buffer and the out pointer are valid for the call.
        let info =
            unsafe { GetVolumeInformationW(PCWSTR(w.as_ptr()), Some(&mut label), Some(&mut serial), None, None, None) };
        let kind = match drive {
            DRIVE_FIXED | DRIVE_RAMDISK if Some(letter) == system_letter => VolumeKind::System,
            DRIVE_FIXED | DRIVE_RAMDISK => VolumeKind::Fixed,
            DRIVE_REMOVABLE => VolumeKind::Removable,
            DRIVE_CDROM if info.is_ok() => VolumeKind::Optical,
            _ => continue,
        };
        let mut guid = [0u16; 64];
        // SAFETY: `w` is NUL-terminated; `guid` is a writable buffer of the length passed.
        let identity = match unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(w.as_ptr()), &mut guid) } {
            Ok(()) => text_of(&guid),
            Err(_) if info.is_ok() => format!("serial:{serial:08X}"),
            Err(_) => continue,
        };
        let (mut free, mut total) = (0u64, 0u64);
        // SAFETY: `w` is NUL-terminated; both out pointers are valid.
        let space = unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), Some(&mut total), None) };
        if space.is_err() {
            (free, total) = (0, 0);
        }
        out.push(DiskVolume {
            identity,
            root,
            kind,
            label: if info.is_ok() { text_of(&label) } else { String::new() },
            letter: Some(letter),
            total_bytes: total,
            free_bytes: free,
            has_bin: drive == DRIVE_FIXED,
        });
    }
    out
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` is a valid folder id; no token (the calling user); the returned string is freed below.
    let p = unsafe { SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None) }.ok()?;
    // SAFETY: `p` is the NUL-terminated string the call allocated.
    let path = PathBuf::from(OsString::from_wide(unsafe { p.as_wide() }));
    // SAFETY: allocated by the shell with the COM task allocator, freed once.
    unsafe { CoTaskMemFree(Some(p.0.cast_const().cast())) };
    Some(path)
}

/// The user's well-known folders that exist on this PC. `Received` is the service's own folder, not listed here.
pub fn places() -> Vec<(PlaceKind, PathBuf)> {
    [
        (PlaceKind::Desktop, &FOLDERID_Desktop),
        (PlaceKind::Documents, &FOLDERID_Documents),
        (PlaceKind::Downloads, &FOLDERID_Downloads),
        (PlaceKind::Pictures, &FOLDERID_Pictures),
        (PlaceKind::Music, &FOLDERID_Music),
        (PlaceKind::Videos, &FOLDERID_Videos),
    ]
    .into_iter()
    .filter_map(|(kind, id)| known_folder(id).filter(|p| p.is_dir()).map(|p| (kind, p)))
    .collect()
}

/// Where every change is refused: the Windows folder, the program folders, ProgramData and the system drive's root.
pub fn protected() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> =
        [&FOLDERID_Windows, &FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86, &FOLDERID_ProgramData]
            .into_iter()
            .filter_map(known_folder)
            .collect();
    if let Some(root) = out.first().and_then(|w| w.ancestors().last().map(Path::to_path_buf)) {
        out.push(root);
    }
    out
}

/// Whether the volume of `path` is a fixed disk, the kind that has a Recycle Bin.
fn on_fixed_disk(path: &Path) -> bool {
    let w = wide(path);
    let mut root = [0u16; 261];
    // SAFETY: `w` is NUL-terminated; `root` is a writable buffer of the length passed.
    if unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut root) }.is_err() {
        return false;
    }
    // SAFETY: `root` holds the NUL-terminated volume path the call wrote.
    let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
    kind == DRIVE_FIXED
}

/// Sends `paths` to the Recycle Bin, never deleting for good and never following a link. `NoBin` when a place is not
/// a fixed disk (nothing is touched) or the system's warning that an item would be destroyed was declined.
pub fn trash(paths: &[PathBuf]) -> Result<(), TrashError> {
    if paths.iter().any(|p| !on_fixed_disk(p)) {
        return Err(TrashError::NoBin);
    }
    // SAFETY: COM for the calling thread; balanced by `CoUninitialize` below when it succeeded.
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = recycle(paths);
    if initialized {
        // SAFETY: balances the successful `CoInitializeEx` above, on the same thread.
        unsafe { CoUninitialize() };
    }
    result
}

fn recycle(paths: &[PathBuf]) -> Result<(), TrashError> {
    // SAFETY: a plain COM object creation on a thread with COM initialized.
    let op: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }.map_err(|_| TrashError::Failed)?;
    let flags = FOFX_RECYCLEONDELETE
        | FOF_WANTNUKEWARNING
        | FOF_NOCONFIRMATION
        | FOF_SILENT
        | FOF_NOERRORUI
        | FOFX_EARLYFAILURE;
    // SAFETY: `op` is a valid IFileOperation; the flags are documented values.
    unsafe { op.SetOperationFlags(flags) }.map_err(|_| TrashError::Failed)?;
    for p in paths {
        let w = wide(p);
        // SAFETY: `w` is NUL-terminated and outlives the call. The item is the path itself, not a link's target.
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }.map_err(|_| TrashError::NotFound)?;
        // SAFETY: `item` is a valid shell item; no progress sink.
        unsafe { op.DeleteItem(&item, None) }.map_err(|_| TrashError::Failed)?;
    }
    // SAFETY: runs the queued deletes.
    let performed = unsafe { op.PerformOperations() };
    // SAFETY: a plain query after the operations.
    let aborted = unsafe { op.GetAnyOperationsAborted() }.map(|b| b.as_bool()).unwrap_or(true);
    match (performed, aborted) {
        (Ok(()), false) => Ok(()),
        (Err(e), _) if e.code() == E_ACCESSDENIED => Err(TrashError::NotPermitted),
        // The warning that an item would be destroyed was declined, or an item could not be recycled.
        _ => Err(TrashError::NoBin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_drive_is_a_volume_of_kind_system_with_a_bin_and_an_identity() {
        let vols = volumes();
        let sys = vols.iter().find(|v| v.kind == VolumeKind::System).expect("a System volume");
        assert!(sys.has_bin);
        assert!(!sys.identity.is_empty());
        assert!(sys.total_bytes > 0 && sys.letter.is_some());
        let windows = known_folder(&FOLDERID_Windows).unwrap();
        assert!(windows.starts_with(&sys.root));
        assert!(vols.iter().all(|v| v.kind != VolumeKind::Network));
    }

    #[test]
    fn the_places_include_documents_and_all_exist() {
        let places = places();
        assert!(places.iter().any(|(k, _)| *k == PlaceKind::Documents));
        assert!(places.iter().all(|(k, p)| *k != PlaceKind::Received && p.is_dir()));
    }

    #[test]
    fn the_protected_folders_include_the_windows_folder_and_the_system_root() {
        let prot = protected();
        let windows = known_folder(&FOLDERID_Windows).unwrap();
        assert!(prot.contains(&windows));
        assert!(prot.iter().any(|p| p.parent().is_none()), "the root of the system drive");
    }

    #[test]
    fn a_file_on_the_system_disk_goes_to_the_bin_and_the_disk_has_free_space() {
        let file = std::env::temp_dir().join(format!("swapdesk-lane-test-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        assert!(on_fixed_disk(&file), "the temp folder is on a fixed disk");
        assert!(free_bytes(&std::env::temp_dir()).is_some_and(|b| b > 0));
        trash(std::slice::from_ref(&file)).expect("to the Recycle Bin");
        assert!(!file.exists(), "the file left its folder for the bin");
    }

    #[test]
    fn a_missing_file_is_not_found() {
        let gone = std::env::temp_dir().join("swapdesk-lane-test-gone.txt");
        assert_eq!(trash(&[gone]), Err(TrashError::NotFound));
    }

    #[test]
    fn a_place_that_is_not_a_fixed_disk_is_refused_and_nothing_is_deleted() {
        // A UNC path to this machine's administrative share is a network place: no Recycle Bin.
        let unc = PathBuf::from(r"\\localhost\C$\Windows\win.ini");
        assert!(!on_fixed_disk(&unc) || !unc.exists());
        if !on_fixed_disk(&unc) {
            assert_eq!(trash(std::slice::from_ref(&unc)), Err(TrashError::NoBin));
        }
    }
}
