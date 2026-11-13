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
//!
//! Nothing may put a dialog on the host's desktop in the usual cases (13/11/2026 review): the user's own Recycle Bin
//! settings for the volume are read first (`BinSettings`: the bin is off, or the item is bigger than the bin holds) and
//! an item the shell would delete for good is refused with `NoBin` before the shell is asked; and a progress sink
//! (`IFileOperationProgressSink::PreDeleteItem`, "performs caller-implemented actions before the delete process for
//! each item begins"; an error there cancels the step and every one pending, learn.microsoft.com/windows/win32/api/
//! shobjidl_core/nf-shobjidl_core-ifileoperationprogresssink-predeleteitem, read 13/11/2026) refuses a step whose flags
//! lack `TSF_DELETE_RECYCLE_IF_POSSIBLE` (`TRANSFER_SOURCE_FLAGS`, 0x80, same site, `ne-shobjidl_core-
//! _transfer_source_flags`, read 13/11/2026). Measured on this PC the flags of a 3 TB item and of a 1 byte item were
//! the same, so the sink alone does not see "too big": the settings do. The nuke warning remains for what neither
//! foresaw, and the helper runs a trash on a thread of its own so that a warning waiting for an answer holds no other
//! disk call.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{E_ABORT, E_ACCESSDENIED};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE,
    FOLDERID_CommonStartup, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music,
    FOLDERID_Pictures, FOLDERID_ProgramData, FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Startup,
    FOLDERID_Videos, FOLDERID_Windows, FileOperation, IFileOperation, IFileOperationProgressSink,
    IFileOperationProgressSink_Impl, IShellItem, KNOWN_FOLDER_FLAG, SHCreateItemFromParsingName, SHGetKnownFolderPath,
    TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows::core::{GUID, HRESULT, PCWSTR, Ref};
use windows_core::implement;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW,
};

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

/// Where every change is refused: the Windows folder, the program folders, ProgramData, the system drive's root, the
/// two Startup folders (the user's and the common one), where a program is run at the next logon, and the folders of
/// the user's PowerShell profiles and modules (Windows PowerShell and PowerShell 7), whose code runs when a shell
/// starts (D66). The PowerShell folders are listed even when absent, so they cannot be made.
pub fn protected() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [
        &FOLDERID_Windows,
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX86,
        &FOLDERID_ProgramData,
        &FOLDERID_Startup,
        &FOLDERID_CommonStartup,
    ]
    .into_iter()
    .filter_map(known_folder)
    .collect();
    if let Some(root) = out.first().and_then(|w| w.ancestors().last().map(Path::to_path_buf)) {
        out.push(root);
    }
    if let Some(documents) = known_folder(&FOLDERID_Documents) {
        out.extend(POWERSHELL_FOLDERS.iter().map(|f| documents.join(f)));
    }
    out
}

/// The user's PowerShell folders under Documents: profiles and modules load from them when a shell starts.
const POWERSHELL_FOLDERS: [&str; 2] = ["WindowsPowerShell", "PowerShell"];

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

/// What the user's settings say about the Recycle Bin of one volume (the values Explorer keeps for it under
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{volume GUID}`, and the policy
/// `NoRecycleFiles`; the shell's own decision, which no documented call reports ahead of the delete).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BinSettings {
    /// "Don't move files to the Recycle Bin. Remove files immediately when deleted" (or the policy that says so).
    pub nuke_on_delete: bool,
    /// The most the bin of this volume holds, in megabytes; `None` when the value is not there.
    pub max_capacity_mb: Option<u32>,
}

/// Whether the shell would delete an item of `bytes` bytes for good, given the settings: the bin is off, or the item
/// is bigger than the bin can hold.
fn bin_refuses(settings: BinSettings, bytes: u64) -> bool {
    settings.nuke_on_delete || settings.max_capacity_mb.is_some_and(|mb| bytes > u64::from(mb) * 1_048_576)
}

/// The bytes of the file or folder at `p`, at most `limit` + 1 (the walk stops once it passes `limit`); links count
/// nothing, since the bin takes the link itself.
fn tree_size(p: &Path, limit: u64) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return 0 };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            total = total.saturating_add(tree_size(&e.path(), limit.saturating_sub(total)));
            if total > limit {
                break;
            }
        }
    }
    total
}

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A DWORD of the registry, `None` when it is not there or not a DWORD.
fn reg_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide_str(key), wide_str(value));
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated wide strings; `data` and `size` are live and `size` is the buffer's size;
    // the flag restricts the read to a DWORD, and a missing or differently typed value is an error code.
    let rc = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            core::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (rc == 0).then_some(data)
}

/// The `{GUID}` of the volume that holds `path`, as the registry names it.
fn volume_guid(path: &Path) -> Option<String> {
    let w = wide(path);
    let mut root = [0u16; 261];
    // SAFETY: `w` is NUL-terminated; `root` is a writable buffer of the length passed.
    unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut root) }.ok()?;
    let mut name = [0u16; 64];
    // SAFETY: `root` holds the NUL-terminated volume path the call wrote; `name` is a writable buffer.
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root.as_ptr()), &mut name) }.ok()?;
    let text = text_of(&name);
    let (start, end) = (text.find('{')?, text.find('}')?);
    (start < end).then(|| text[start..=end].to_string())
}

/// The bin settings of the volume that holds `path`.
fn bin_settings_of(path: &Path) -> BinSettings {
    const POLICY_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    let policy_off = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .any(|root| reg_dword(root, POLICY_KEY, "NoRecycleFiles").is_some_and(|v| v != 0));
    let Some(guid) = volume_guid(path) else {
        return BinSettings { nuke_on_delete: policy_off, max_capacity_mb: None };
    };
    let key = format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}");
    BinSettings {
        nuke_on_delete: policy_off || reg_dword(HKEY_CURRENT_USER, &key, "NukeOnDelete").is_some_and(|v| v != 0),
        max_capacity_mb: reg_dword(HKEY_CURRENT_USER, &key, "MaxCapacity"),
    }
}

/// Sends `paths` to the Recycle Bin, never deleting for good and never following a link. `NoBin` when a place is not
/// a fixed disk, when the settings say the shell would delete an item for good (the bin is off for its volume, or the
/// item is bigger than the bin holds), or when the shell's own steps say so; in every one of these nothing is deleted
/// and no dialog is shown. The shell's nuke warning remains the last guard for a case none of this foresaw.
pub fn trash(paths: &[PathBuf]) -> Result<(), TrashError> {
    trash_with(paths, on_fixed_disk, bin_settings_of, |paths| {
        // SAFETY: COM for the calling thread; balanced by `CoUninitialize` below when it succeeded.
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let result = recycle(paths);
        if initialized {
            // SAFETY: balances the successful `CoInitializeEx` above, on the same thread.
            unsafe { CoUninitialize() };
        }
        result
    })
}

/// [`trash`] over its three questions, so a test can answer them: is this place a fixed disk, what do the settings of
/// its volume say, and the shell's own run (which a test replaces with one that must never be reached).
fn trash_with(
    paths: &[PathBuf],
    fixed: impl Fn(&Path) -> bool,
    settings_of: impl Fn(&Path) -> BinSettings,
    run: impl FnOnce(&[PathBuf]) -> Result<(), TrashError>,
) -> Result<(), TrashError> {
    if paths.iter().any(|p| !fixed(p)) {
        return Err(TrashError::NoBin);
    }
    for p in paths {
        let settings = settings_of(p);
        if settings.nuke_on_delete {
            return Err(TrashError::NoBin);
        }
        if let Some(mb) = settings.max_capacity_mb {
            let limit = u64::from(mb) * 1_048_576;
            if bin_refuses(settings, tree_size(p, limit)) {
                return Err(TrashError::NoBin);
            }
        }
    }
    run(paths)
}

/// The progress sink that stands between the shell and a delete: it hears each step before it begins
/// (`PreDeleteItem`, whose flags say whether the step recycles, `TSF_DELETE_RECYCLE_IF_POSSIBLE`) and after it ends
/// (`PostDeleteItem`, whose `psiNewlyCreated` is the item in the bin, or null "if the item was fully deleted"), and
/// refuses a step that would not recycle, which cancels it and every one after it.
#[implement(IFileOperationProgressSink)]
struct RecycleGuard {
    /// A step came without the recycle flag and was refused: nothing was deleted.
    refused: Arc<AtomicBool>,
    /// A step ended with the item gone and not in the bin (the shell deleted it for good despite the guard).
    destroyed: Arc<AtomicBool>,
    /// The flags of every `PreDeleteItem`, for the tests.
    seen: Arc<Mutex<Vec<u32>>>,
}

/// Whether the shell flags for a delete step say it goes to the bin (`TSF_DELETE_RECYCLE_IF_POSSIBLE`, 0x80).
fn step_recycles(flags: u32) -> bool {
    flags & (TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32) != 0
}

impl IFileOperationProgressSink_Impl for RecycleGuard_Impl {
    fn StartOperations(&self) -> windows_core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _result: HRESULT) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _flags: u32, _item: Ref<IShellItem>, _new_name: &PCWSTR) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _to: Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _to: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _to: Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _to: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreDeleteItem(&self, flags: u32, _item: Ref<IShellItem>) -> windows_core::Result<()> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(flags);
        }
        if !step_recycles(flags) {
            self.refused.store(true, Ordering::SeqCst);
            // An error here cancels this delete and every operation still pending (the PreDeleteItem page).
            return Err(E_ABORT.into());
        }
        Ok(())
    }
    fn PostDeleteItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        result: HRESULT,
        created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        if result.is_ok() && created.is_null() {
            self.destroyed.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn PreNewItem(&self, _flags: u32, _to: Ref<IShellItem>, _new_name: &PCWSTR) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _flags: u32,
        _to: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _template: &PCWSTR,
        _attributes: u32,
        _result: HRESULT,
        _item: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _total: u32, _so_far: u32) -> windows_core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }
}

/// What a recycle run found out, besides the result of the shell itself.
#[derive(Default)]
struct Outcome {
    refused: Arc<AtomicBool>,
    destroyed: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<u32>>>,
}

fn recycle(paths: &[PathBuf]) -> Result<(), TrashError> {
    recycle_with(paths, &Outcome::default())
}

fn recycle_with(paths: &[PathBuf], outcome: &Outcome) -> Result<(), TrashError> {
    // SAFETY: a plain COM object creation on a thread with COM initialized.
    let op: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }.map_err(|_| TrashError::Failed)?;
    // No confirmation, no progress and no error box, and the first error stops the lot. `FOF_WANTNUKEWARNING` stays:
    // an item the bin cannot take (too big for it, say) would otherwise be deleted for good with the answer "yes" to
    // every question. What the shell can be told beforehand is told first (`bin_refuses`, the guard's `PreDeleteItem`),
    // so the warning is the last guard and not the usual way out.
    let flags = FOFX_RECYCLEONDELETE
        | FOF_WANTNUKEWARNING
        | FOF_NOCONFIRMATION
        | FOF_SILENT
        | FOF_NOERRORUI
        | FOFX_EARLYFAILURE;
    // SAFETY: `op` is a valid IFileOperation; the flags are documented values.
    unsafe { op.SetOperationFlags(flags) }.map_err(|_| TrashError::Failed)?;
    let guard: IFileOperationProgressSink = RecycleGuard {
        refused: Arc::clone(&outcome.refused),
        destroyed: Arc::clone(&outcome.destroyed),
        seen: Arc::clone(&outcome.seen),
    }
    .into();
    // SAFETY: `guard` is a valid sink that lives until the `Unadvise` below.
    let cookie = unsafe { op.Advise(&guard) }.map_err(|_| TrashError::Failed)?;
    let result = queue_and_run(&op, paths);
    // SAFETY: `cookie` came from the `Advise` above, on the same object.
    let _ = unsafe { op.Unadvise(cookie) };
    // SAFETY: a plain query after the operations.
    let aborted = unsafe { op.GetAnyOperationsAborted() }.map(|b| b.as_bool()).unwrap_or(true);
    if outcome.refused.load(Ordering::SeqCst) {
        // A step that would not recycle was refused before it began: nothing was deleted.
        return Err(TrashError::NoBin);
    }
    if outcome.destroyed.load(Ordering::SeqCst) {
        return Err(TrashError::Failed);
    }
    match (result, aborted) {
        (Ok(()), false) => Ok(()),
        (Err(TrashRun::NotFound), _) => Err(TrashError::NotFound),
        (Err(TrashRun::Failed), _) => Err(TrashError::Failed),
        (Err(TrashRun::Denied), _) => Err(TrashError::NotPermitted),
        // An item could not be recycled and the run stopped on it.
        (Ok(()), true) => Err(TrashError::NoBin),
    }
}

enum TrashRun {
    NotFound,
    Failed,
    Denied,
}

fn queue_and_run(op: &IFileOperation, paths: &[PathBuf]) -> Result<(), TrashRun> {
    for p in paths {
        let w = wide(p);
        // SAFETY: `w` is NUL-terminated and outlives the call. The item is the path itself, not a link's target.
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }.map_err(|_| TrashRun::NotFound)?;
        // SAFETY: `item` is a valid shell item; no per-item sink (the guard hears every item).
        unsafe { op.DeleteItem(&item, None) }.map_err(|_| TrashRun::Failed)?;
    }
    // SAFETY: runs the queued deletes.
    match unsafe { op.PerformOperations() } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == E_ACCESSDENIED => Err(TrashRun::Denied),
        // Refused by the guard, or an item could not be recycled: the caller reads the guard's flags first.
        Err(_) => Ok(()),
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
    fn the_protected_folders_include_both_startup_folders() {
        // What a program put there runs at the next logon: the user's Startup folder and the common one.
        let prot = protected();
        for id in [&FOLDERID_Startup, &FOLDERID_CommonStartup] {
            let dir = known_folder(id).expect("the shell knows the folder");
            assert!(prot.contains(&dir), "{dir:?} is protected");
        }
        assert_ne!(known_folder(&FOLDERID_Startup), known_folder(&FOLDERID_CommonStartup));
        // The PowerShell profiles and modules of the user, listed even before they exist (D66).
        let documents = known_folder(&FOLDERID_Documents).expect("the shell knows Documents");
        for f in ["WindowsPowerShell", "PowerShell"] {
            assert!(prot.contains(&documents.join(f)), "{f} is protected");
        }
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

    fn temp_file(tag: &str, len: u64) -> PathBuf {
        let p = std::env::temp_dir().join(format!("swapdesk-bin-{tag}-{}.bin", std::process::id()));
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(len).unwrap();
        p
    }

    fn never_run(_: &[PathBuf]) -> Result<(), TrashError> {
        panic!("the shell must not be asked: it would show its warning or delete for good");
    }

    #[test]
    fn a_volume_with_the_bin_off_is_refused_before_the_shell_is_asked() {
        let file = temp_file("binoff", 10);
        let off = BinSettings { nuke_on_delete: true, max_capacity_mb: Some(1000) };
        assert_eq!(trash_with(std::slice::from_ref(&file), |_| true, |_| off, never_run), Err(TrashError::NoBin));
        assert!(file.exists(), "nothing was deleted");
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn an_item_bigger_than_the_bin_holds_is_refused_before_the_shell_is_asked() {
        let big = temp_file("big", 3 * 1_048_576);
        let two_mb = BinSettings { nuke_on_delete: false, max_capacity_mb: Some(2) };
        assert_eq!(trash_with(std::slice::from_ref(&big), |_| true, |_| two_mb, never_run), Err(TrashError::NoBin));
        // A folder counts what is in it, however deep.
        let dir = std::env::temp_dir().join(format!("swapdesk-bin-tree-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a").join("b")).unwrap();
        std::fs::write(dir.join("a").join("b").join("x.bin"), vec![0u8; 3 * 1_048_576]).unwrap();
        assert_eq!(trash_with(std::slice::from_ref(&dir), |_| true, |_| two_mb, never_run), Err(TrashError::NoBin));
        assert!(big.exists() && dir.join("a").join("b").join("x.bin").exists(), "nothing was deleted");
        // What fits goes on to the shell; so does a volume whose capacity is not known.
        let reached = std::cell::Cell::new(0);
        let run = |_: &[PathBuf]| {
            reached.set(reached.get() + 1);
            Ok(())
        };
        let ten_mb = BinSettings { nuke_on_delete: false, max_capacity_mb: Some(10) };
        assert_eq!(trash_with(std::slice::from_ref(&big), |_| true, |_| ten_mb, run), Ok(()));
        assert_eq!(trash_with(std::slice::from_ref(&big), |_| true, |_| BinSettings::default(), run), Ok(()));
        assert_eq!(reached.get(), 2);
        let _ = std::fs::remove_file(&big);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bin_settings_of_the_temp_volume_can_be_read() {
        let guid = volume_guid(&std::env::temp_dir()).expect("the volume has a GUID name");
        assert!(guid.starts_with('{') && guid.ends_with('}') && guid.len() == 38, "{guid}");
        // Whatever the machine says, reading it never fails.
        let settings = bin_settings_of(&std::env::temp_dir());
        eprintln!("bin settings of the temp volume: {settings:?}");
        assert!(!bin_refuses(BinSettings { nuke_on_delete: false, max_capacity_mb: None }, u64::MAX));
        assert!(bin_refuses(BinSettings { nuke_on_delete: true, max_capacity_mb: None }, 0));
        assert!(bin_refuses(BinSettings { nuke_on_delete: false, max_capacity_mb: Some(1) }, 1_048_577));
        assert!(!bin_refuses(BinSettings { nuke_on_delete: false, max_capacity_mb: Some(1) }, 1_048_576));
    }

    #[test]
    fn the_guard_refuses_a_delete_step_that_does_not_recycle_and_lets_a_recycling_one_go() {
        // The shell tells each step's flags before it begins: 0x80 (`TSF_DELETE_RECYCLE_IF_POSSIBLE`) is a recycle.
        assert!(step_recycles(0x282) && step_recycles(0x80));
        assert!(!step_recycles(0x202) && !step_recycles(0));
        let file = temp_file("guard", 1);
        // SAFETY: COM for this test thread, balanced below.
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let w = wide(&file);
        // SAFETY: `w` is NUL-terminated and outlives the call.
        let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }.unwrap();
        let outcome = Outcome::default();
        let sink: IFileOperationProgressSink = RecycleGuard {
            refused: Arc::clone(&outcome.refused),
            destroyed: Arc::clone(&outcome.destroyed),
            seen: Arc::clone(&outcome.seen),
        }
        .into();
        // SAFETY: `sink` and `item` are valid objects; these are plain calls of the interface.
        unsafe {
            assert!(sink.PreDeleteItem(0x282, &item).is_ok());
            assert!(!outcome.refused.load(Ordering::SeqCst));
            let refusal = sink.PreDeleteItem(0x202, &item).unwrap_err();
            assert_eq!(refusal.code(), E_ABORT, "an error here cancels the step and every one after it");
        }
        assert!(outcome.refused.load(Ordering::SeqCst));
        assert_eq!(*outcome.seen.lock().unwrap(), [0x282, 0x202]);
        // A step that ends with nothing in the bin and the item gone is noted.
        // SAFETY: as above; no newly created item stands for "fully deleted".
        unsafe { sink.PostDeleteItem(0x82, &item, HRESULT(0), None::<&IShellItem>).unwrap() };
        assert!(outcome.destroyed.load(Ordering::SeqCst));
        drop((sink, item));
        if initialized {
            // SAFETY: balances the successful `CoInitializeEx` above.
            unsafe { CoUninitialize() };
        }
        assert!(file.exists(), "the guard never deletes anything itself");
        let _ = std::fs::remove_file(&file);
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
