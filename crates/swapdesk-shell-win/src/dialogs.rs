//! Native dialogs: where to save the exported activity log.

use std::path::PathBuf;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::{
    FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FileSaveDialog, IFileSaveDialog, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, w};

/// Shows the system "Save as" dialog (own file dialog, C6) and returns the chosen path, if any.
pub fn save_log(owner: HWND, title: &str) -> Option<PathBuf> {
    // SAFETY: plain COM calls on a dialog created here; the returned string is freed after it is copied.
    unsafe {
        let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetTitle(&HSTRING::from(title)).ok()?;
        dialog.SetFileName(w!("swapdesk-atividade.csv")).ok()?;
        dialog.SetDefaultExtension(w!("csv")).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_OVERWRITEPROMPT).ok()?;
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0 as *const _));
        path
    }
}
