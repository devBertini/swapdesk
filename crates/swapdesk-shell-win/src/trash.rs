//! This computer's bin and free space for the explorer (`docs/FILES-UI.md` 3.2, slice 2a), given to
//! `swapdesk_ui::local_disk` at start; both run on the interface backend's worker thread, never the UI thread.
//!
//! The bin: `IFileOperation` with `FOFX_RECYCLEONDELETE` ("send it to the Recycle Bin rather than permanently
//! deleting it") and `FOF_WANTNUKEWARNING` ("send a warning if a file or folder is being destroyed during a delete
//! operation rather than recycled"), learn.microsoft.com/windows/win32/api/shobjidl_core/
//! nf-shobjidl_core-ifileoperation-setoperationflags, read 12/11/2026. Microsoft documents recycling only "if
//! possible" (`TSF_DELETE_RECYCLE_IF_POSSIBLE`) and no way to ask a volume first, so a place that is not a fixed disk is
//! refused before anything happens (`Refused`: the window says the place has no bin), and the nuke warning stays as the
//! last guard: nothing is ever destroyed in silence.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use swapdesk_ui::ApiCode;
use swapdesk_ui::local_disk::LocalOs;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumePathNameW};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE,
    FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
};
use windows::core::PCWSTR;

/// `DRIVE_FIXED` of `GetDriveTypeW`.
const DRIVE_FIXED: u32 = 3;

pub fn local_os() -> LocalOs {
    LocalOs { trash, free_bytes }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// The free space the user may use on the volume of `path`.
fn free_bytes(path: &Path) -> Option<u64> {
    let w = wide(path);
    let mut free = 0u64;
    // SAFETY: `w` is NUL-terminated and outlives the call; `free` is a valid out pointer.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None) }.ok()?;
    Some(free)
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

/// Sends `paths` to the Recycle Bin. `Refused` when a place has no bin (nothing is deleted) or the system's warning
/// that an item would be destroyed was declined.
fn trash(paths: &[PathBuf]) -> Result<(), ApiCode> {
    if paths.iter().any(|p| !on_fixed_disk(p)) {
        return Err(ApiCode::Refused);
    }
    // SAFETY: COM for this worker thread; balanced by `CoUninitialize` below when it succeeded.
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = recycle(paths);
    if initialized {
        // SAFETY: balances the successful `CoInitializeEx` above, on the same thread.
        unsafe { CoUninitialize() };
    }
    result
}

fn recycle(paths: &[PathBuf]) -> Result<(), ApiCode> {
    // SAFETY: a plain COM object creation on a thread with COM initialized.
    let op: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }.map_err(|_| ApiCode::Internal)?;
    let flags = FOFX_RECYCLEONDELETE
        | FOF_WANTNUKEWARNING
        | FOF_NOCONFIRMATION
        | FOF_SILENT
        | FOF_NOERRORUI
        | FOFX_EARLYFAILURE;
    // SAFETY: `op` is a valid IFileOperation; the flags are documented values.
    unsafe { op.SetOperationFlags(flags) }.map_err(|_| ApiCode::Internal)?;
    for p in paths {
        let w = wide(p);
        // SAFETY: `w` is NUL-terminated and outlives the call.
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }.map_err(|_| ApiCode::NotFound)?;
        // SAFETY: `item` is a valid shell item; no progress sink.
        unsafe { op.DeleteItem(&item, None) }.map_err(|_| ApiCode::Internal)?;
    }
    // SAFETY: runs the queued deletes.
    let performed = unsafe { op.PerformOperations() };
    // SAFETY: a plain query after the operations.
    let aborted = unsafe { op.GetAnyOperationsAborted() }.map(|b| b.as_bool()).unwrap_or(true);
    match (performed, aborted) {
        (Ok(()), false) => Ok(()),
        // The warning that an item would be destroyed was declined, or an item could not be recycled.
        _ => Err(ApiCode::Refused),
    }
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
        assert!(on_fixed_disk(&file), "the temp folder is on a fixed disk");
        assert!(free_bytes(&dir).is_some_and(|b| b > 0));
        trash(std::slice::from_ref(&file)).expect("to the Recycle Bin");
        assert!(!file.exists(), "the file left its folder for the bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_place_that_is_not_a_fixed_disk_is_refused_and_nothing_is_deleted() {
        // A UNC path to this machine's administrative share is a network place: no Recycle Bin.
        let unc = PathBuf::from(r"\\localhost\C$\Windows\win.ini");
        assert!(!on_fixed_disk(&unc) || !unc.exists());
        if !on_fixed_disk(&unc) {
            assert_eq!(trash(std::slice::from_ref(&unc)), Err(ApiCode::Refused));
        }
    }
}
