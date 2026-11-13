//! Holding the installed files an update replaces (docs/UPDATES.md 6.2 step 5; the security review of 13/11).
//!
//! The defect it closes: any local account can open an installed program for read with `FILE_SHARE_READ` only and keep
//! the handle. A rename over that file (`MoveFileExW` with replace) then fails with error 5 for as long as the handle
//! lives, and an apply that fails half-way leaves the service stopped.
//!
//! The rule of Win32 sharing that makes a hold possible: a handle opened with `DELETE` access refuses every later open
//! whose share mode lacks `FILE_SHARE_DELETE`. So before the first file changes, [`Held::claim`] opens each file to be
//! replaced with `DELETE` access: a program that already holds one in a way that blocks the rename makes the claim
//! fail, with nothing touched, and one that tries afterwards is refused by the system (error 32). The new file is then
//! put in place through its own handle, opened the same way ([`Held::replace_through`]): `SetFileInformationByHandle`
//! with `FileRenameInfoEx`, replace and POSIX semantics, which keeps the hold on the new file across the rename, so
//! there is no moment in which another process can grab a target.
//!
//! Two points the review's sketch (share mode 0, rename through the held handle) could not keep: our own handles
//! share everything, because a share mode of 0 would refuse the next handle of ours and the rename over the file; and
//! the replace uses POSIX semantics, because the classic replace fails with error 5 while any handle on the target
//! is open, ours included (found by trying it: the classic form failed with the hold in place, this one does not).
//! What the access check lets nobody do (write to a file in Program Files) is not widened by the share mode.
//!
//! Sources read on 2026-11-13: `SetFileInformationByHandle` and `FILE_RENAME_INFO`
//! <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setfileinformationbyhandle>,
//! <https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info>, the flags of
//! `FileRenameInfoEx` (`FILE_RENAME_POSIX_SEMANTICS` "allow replacing a file even if there are existing handles to it";
//! NTFS, Windows 10 1709 or later) <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information>; the Restart Manager
//! `RmGetList` <https://learn.microsoft.com/en-us/windows/win32/api/restartmanager/nf-restartmanager-rmgetlist>.

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_LOCK_VIOLATION, ERROR_NOT_SAME_DEVICE, ERROR_SHARING_VIOLATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileRenameInfoEx, SetFileInformationByHandle,
};
use windows_sys::Win32::System::RestartManager::{
    RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources, RmStartSession,
};

use super::{INSTALLED, copy_through};
use crate::sys::wide;

const DELETE: u32 = 0x0001_0000;
/// What our own handles let others do: everything the ACL allows, since the `DELETE` access we hold already forces
/// every other opener to share delete, and a handle of ours must not conflict with another of ours.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// `FILE_RENAME_FLAG_REPLACE_IF_EXISTS` and `FILE_RENAME_FLAG_POSIX_SEMANTICS` of `FileRenameInfoEx`.
const RENAME_REPLACE_IF_EXISTS: u32 = 0x1;
const RENAME_POSIX_SEMANTICS: u32 = 0x2;
/// The suffix of the copy made inside the install folder when the swap folder is on another volume.
const SAME_VOLUME_COPY: &str = "update-new";

/// The installed files an update holds while it replaces them, and then the new files, by name.
#[derive(Default)]
pub struct Held {
    files: Vec<(String, File)>,
    retry: Duration,
}

impl Held {
    /// Holds each installed file that `from` also carries (the files the apply will replace), retrying for `retry`
    /// while a scanner has one open. An `Err` names the file and, when the Restart Manager can say, the program that
    /// holds it; nothing in the install folder has changed by then.
    pub fn claim(install: &Path, from: &Path, retry: Duration) -> Result<Self, String> {
        let mut held = Self { files: Vec::new(), retry };
        for name in INSTALLED {
            let target = install.join(name);
            if !from.join(name).is_file() || std::fs::symlink_metadata(&target).is_err() {
                continue;
            }
            let file = open_held(&target, retry).map_err(|e| held_message(name, &target, &e))?;
            held.files.push((name.to_string(), file));
        }
        Ok(held)
    }

    /// Puts `copy` over `dst` through its own handle and keeps the hold on the new file. `Err` when the replace did
    /// not happen (`dst` is then as it was).
    pub fn replace_through(&mut self, name: &str, copy: &Path, dst: &Path) -> Result<(), String> {
        let fail = |e: std::io::Error| format!("moving {} to {}: {e}", copy.display(), dst.display());
        let start = Instant::now();
        let new = loop {
            match rename_over(copy, dst) {
                Ok(file) => break file,
                // The swap folder is on another volume than the install folder: a copy made beside the target
                // (removed again if it does not make it) is renamed instead.
                Err(e) if e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE as i32) => {
                    let beside = dst.with_file_name(format!("{name}.{SAME_VOLUME_COPY}"));
                    copy_through(copy, &beside).map_err(fail)?;
                    match rename_over(&beside, dst) {
                        Ok(file) => break file,
                        Err(e) => {
                            let _ = std::fs::remove_file(&beside);
                            return Err(fail(e));
                        }
                    }
                }
                Err(e) if is_held(&e) && start.elapsed() < self.retry => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(fail(e)),
            }
        };
        // The new file's data and the rename are on the disk before the next file moves.
        new.sync_all().map_err(|e| format!("flushing {}: {e}", dst.display()))?;
        match self.files.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = new,
            None => self.files.push((name.to_string(), new)),
        }
        Ok(())
    }

    /// Lets go of every file.
    pub fn release(&mut self) {
        self.files.clear();
    }

    /// The names held, in the order claimed.
    pub fn names(&self) -> Vec<&str> {
        self.files.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// Whether an error says another handle is in the way.
fn is_held(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error().map(|c| c as u32),
        Some(ERROR_SHARING_VIOLATION | ERROR_ACCESS_DENIED | ERROR_LOCK_VIOLATION)
    )
}

/// Opens `path` for `DELETE` (the file itself, never what a link points to), sharing read and delete.
fn open_held(path: &Path, retry: Duration) -> std::io::Result<File> {
    let start = Instant::now();
    loop {
        match OpenOptions::new()
            .access_mode(DELETE)
            .share_mode(SHARE_ALL)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
        {
            Err(e) if is_held(&e) && start.elapsed() < retry => std::thread::sleep(Duration::from_millis(100)),
            other => return other,
        }
    }
}

fn held_message(name: &str, path: &Path, e: &std::io::Error) -> String {
    if !is_held(e) {
        return format!("{name} in the install folder cannot be opened to be replaced: {e}; nothing was changed");
    }
    let by = holders_of(path);
    let who = if by.is_empty() { "another program".to_string() } else { by.join(", ") };
    format!("{name} in the install folder is held open by {who} and cannot be replaced; nothing was changed")
}

/// Renames the file at `from` to `to`, replacing what is there, through a handle opened for `DELETE` that stays open
/// (and so keeps the hold on the file under its new name).
fn rename_over(from: &Path, to: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .access_mode(DELETE | GENERIC_WRITE)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(from)?;
    // The new name in the verbatim form the kernel takes: the folder resolved, the file name appended.
    let folder = to.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    let mut full = std::fs::canonicalize(folder)?;
    full.push(to.file_name().ok_or_else(|| std::io::Error::other("no file name"))?);
    let name = wide(full.as_os_str()).map_err(|e| std::io::Error::other(e.to_string()))?;
    let name = &name[..name.len() - 1];
    let bytes = std::mem::size_of_val(name);
    // The struct ends in a one-unit name: room for the whole name after the fixed part. u64 units keep it aligned.
    let size = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + bytes;
    let mut buf = vec![0u64; size.div_ceil(8) + 1];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buf` holds at least `size` bytes, aligned for FILE_RENAME_INFO; the name is copied into the space after
    // the fixed part, `FileNameLength` is its length in bytes (without a terminator), and the handle is live.
    let ok = unsafe {
        (*info).Anonymous.Flags = RENAME_REPLACE_IF_EXISTS | RENAME_POSIX_SEMANTICS;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = bytes as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(file.as_raw_handle(), FileRenameInfoEx, info.cast(), size as u32)
    };
    if ok == 0 { Err(std::io::Error::last_os_error()) } else { Ok(file) }
}

/// The programs that hold `path` open, as "name (pid N)", by the Restart Manager; empty when it cannot say.
pub fn holders_of(path: &Path) -> Vec<String> {
    let mut session = 0u32;
    let mut key = [0u16; 33];
    // SAFETY: `session` and `key` (CCH_RM_SESSION_KEY + 1 units) are valid out buffers.
    if unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) } != 0 {
        return Vec::new();
    }
    let names = (|| {
        let w = wide(path.as_os_str()).ok()?;
        let files = [w.as_ptr()];
        // SAFETY: one null-terminated name, alive for the call; no applications or services.
        if unsafe { RmRegisterResources(session, 1, files.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null()) } != 0 {
            return None;
        }
        let (mut needed, mut count, mut reasons) = (0u32, 0u32, 0u32);
        // SAFETY: the first call only asks how many entries there are (a null array and a zero count).
        unsafe { RmGetList(session, &mut needed, &mut count, std::ptr::null_mut(), &mut reasons) };
        if needed == 0 {
            return None;
        }
        // SAFETY: RM_PROCESS_INFO is plain data for which all-zero is valid.
        let mut info = vec![unsafe { std::mem::zeroed::<RM_PROCESS_INFO>() }; needed as usize];
        count = needed;
        // SAFETY: `info` holds `count` entries.
        if unsafe { RmGetList(session, &mut needed, &mut count, info.as_mut_ptr(), &mut reasons) } != 0 {
            return None;
        }
        Some(
            info.iter()
                .take(count as usize)
                .map(|p| {
                    let len = p.strAppName.iter().position(|&c| c == 0).unwrap_or(p.strAppName.len());
                    format!("{} (pid {})", String::from_utf16_lossy(&p.strAppName[..len]), p.Process.dwProcessId)
                })
                .collect::<Vec<_>>(),
        )
    })();
    // SAFETY: the session was started above and is ended once.
    unsafe { RmEndSession(session) };
    names.unwrap_or_default()
}
