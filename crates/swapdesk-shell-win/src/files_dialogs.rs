//! Native dialogs of the file panel (stage 44): which files to send, which folder to receive into. They are the
//! system's own "Open" dialog (C6: own file pickers), run by the user's process, so the paths they return are the
//! user's own choice and go no further than the UI backend (it hands the service tokens, never paths).

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
    SIGDN_FILESYSPATH,
};
use windows::core::HSTRING;

/// The path of a shell item, if it is a file-system one.
///
/// # Safety
/// `item` must be a live shell item.
unsafe fn path_of(item: &IShellItem) -> Option<PathBuf> {
    // SAFETY: the caller's item is live; the returned string is freed after it is copied.
    unsafe {
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0 as *const _));
        path
    }
}

fn dialog(owner: HWND, title: &str, folders: bool, multiple: bool) -> Option<Vec<PathBuf>> {
    // SAFETY: plain COM calls on a dialog created here; every string it returns is freed after it is copied.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetTitle(&HSTRING::from(title)).ok()?;
        let mut options = dialog.GetOptions().ok()? | FOS_FORCEFILESYSTEM;
        if folders {
            options |= FOS_PICKFOLDERS;
        }
        if multiple {
            options |= FOS_ALLOWMULTISELECT;
        }
        dialog.SetOptions(options).ok()?;
        dialog.Show(Some(owner)).ok()?;
        let results = dialog.GetResults().ok()?;
        let count = results.GetCount().ok()?;
        let mut paths = Vec::new();
        for i in 0..count {
            if let Ok(item) = results.GetItemAt(i)
                && let Some(p) = path_of(&item)
            {
                paths.push(p);
            }
        }
        (!paths.is_empty()).then_some(paths)
    }
}

/// Which files to send; empty when the user closes the dialog.
pub fn pick_files(owner: HWND, title: &str) -> Vec<PathBuf> {
    dialog(owner, title, false, true).unwrap_or_default()
}

/// The folder to receive files into.
pub fn pick_folder(owner: HWND, title: &str) -> Option<PathBuf> {
    dialog(owner, title, true, false).and_then(|mut v| v.pop())
}

/// The folder files are received into until the user picks another: this user's Downloads folder, if there is one.
pub fn default_receive_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("USERPROFILE")?);
    let downloads = home.join("Downloads");
    downloads.is_dir().then_some(downloads)
}
