//! The Windows side of the update applier (`docs/UPDATES.md` 6.2 to 6.4): the [`Machine`] of
//! `swapdesk_update::apply` over the SCM, the install folder and the protected update folder.
//!
//! * The service is stopped and started through the SCM ([`ServiceControl`]; tests use a fake).
//! * The sweep ends every Swapdesk program whose image is inside the install folder (the rule of `install.ps1` item
//!   1), after a grace for the interfaces told `UpdateRestarting` to exit by themselves; nothing else is touched.
//! * Before the first file changes, every installed file the update replaces is held (opened with `DELETE` access, see
//!   [`lock`]): a program that keeps one open makes the apply stop with nothing touched, and none can grab one after.
//!   Each new file is then put in place through its own handle (`SetFileInformationByHandle`, `FileRenameInfo` with
//!   `ReplaceIfExists`, flushed to the disk), so the hold carries over to it; it is reset to inherit the install
//!   folder's ACL and each program is checked with the install policy. The install folder holds a complete set at every
//!   moment.
//! * The old programs are copied into `<update>\rollback\<old>\` first; the journal is `<update>\journal`, each line
//!   written through.
//!
//! Sources read on 2026-11-11 and 2026-11-13: `SetNamedSecurityInfoW` with `UNPROTECTED_DACL_SECURITY_INFORMATION`
//! <https://learn.microsoft.com/en-us/windows/win32/api/aclapi/nf-aclapi-setnamedsecurityinfow> (the object inherits
//! the inheritable entries of its parent; an empty DACL so leaves only those); the rest in [`lock`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use swapdesk_update::Version;
use swapdesk_update::apply::{Journal, Machine};
use swapdesk_update::package::Package;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, DACL_SECURITY_INFORMATION, InitializeAcl, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::System::ProcessStatus::K32EnumProcesses;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};

use crate::launch::InstallPolicy;
use crate::sys::wide;

/// The programs of an install (`install.ps1` `$Required`).
pub const PROGRAMS: [&str; 3] = ["Swapdesk.exe", "swapdesk-service.exe", "swapdesk-helper.exe"];
/// Every name the install folder may hold (`$Required` and `$Optional`): a package carries nothing else.
pub const INSTALLED: [&str; 5] =
    ["Swapdesk.exe", "swapdesk-service.exe", "swapdesk-helper.exe", "LICENSE-MIT", "LICENSE-APACHE"];
/// How long a claim or a move is retried while another process (a scanner) holds the file (UPDATES.md 6.2 step 5).
pub const MOVE_RETRY: Duration = Duration::from_secs(10);
/// How long the interfaces told `UpdateRestarting` have to exit before the sweep ends them.
pub const SWEEP_GRACE: Duration = Duration::from_secs(10);
/// How long the SCM has to stop the service before its process is ended.
pub const STOP_WAIT: Duration = Duration::from_secs(30);

/// The names inside the update folder.
pub const JOURNAL: &str = "journal";
pub const STAGING: &str = "staging";
pub const ROLLBACK: &str = "rollback";
pub const APPLIER: &str = "applier";
/// Where a file is copied before it is moved into place: the staged and backed-up copies stay whole.
pub const SWAP: &str = "swap";

/// Stopping and starting the service (the SCM; a fake in tests).
pub trait ServiceControl {
    fn stop(&mut self) -> Result<(), String>;
    fn start(&mut self) -> Result<(), String>;
    /// Whether the service is anything but stopped.
    fn running(&mut self) -> bool;
}

/// The real service, through the SCM.
pub struct Scm {
    pub name: String,
}

impl ServiceControl for Scm {
    fn stop(&mut self) -> Result<(), String> {
        crate::service::stop_service(&self.name, STOP_WAIT).map_err(|e| e.to_string())
    }

    fn start(&mut self) -> Result<(), String> {
        crate::service::start_service(&self.name).map_err(|e| e.to_string())
    }

    fn running(&mut self) -> bool {
        crate::service::query_service(&self.name).is_ok_and(|s| s.state != 1)
    }
}

/// The folders an apply works in.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The install folder (`%ProgramFiles%\Swapdesk`).
    pub install: PathBuf,
    /// The protected update folder (`%ProgramData%\Swapdesk\update`): the journal, staging, rollback and applier.
    pub update: PathBuf,
}

impl Layout {
    pub fn journal(&self) -> PathBuf {
        self.update.join(JOURNAL)
    }

    pub fn staging(&self) -> PathBuf {
        self.update.join(STAGING)
    }

    pub fn rollback(&self, old: &Version) -> PathBuf {
        self.update.join(ROLLBACK).join(old.to_string())
    }

    pub fn applier(&self) -> PathBuf {
        self.update.join(APPLIER)
    }

    pub fn swap(&self) -> PathBuf {
        self.update.join(SWAP)
    }
}

/// Whether every program is in `dir`.
pub fn complete(dir: &Path) -> bool {
    PROGRAMS.iter().all(|p| dir.join(p).is_file())
}

/// Appends `entry` to the journal at `path`, written through to the disk before it returns.
pub fn append_journal(path: &Path, entry: &Journal) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(format!("{}\n", entry.to_line()).as_bytes())?;
    f.sync_all()
}

/// The journal's last line, `None` when it is missing or reads as nothing.
pub fn read_journal(path: &Path) -> Option<Journal> {
    Journal::last_of(&std::fs::read_to_string(path).ok()?)
}

/// Writes the package's files into a fresh `staging` folder, each written through. Refused (and the folder removed)
/// when the package carries a name that is not one of [`INSTALLED`] or lacks one of the [`PROGRAMS`].
pub fn stage(package: &Package, staging: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(staging);
    let missing = PROGRAMS.iter().find(|p| package.get(p).is_none());
    if let Some(p) = missing {
        return Err(format!("the package has no {p}"));
    }
    if let Some(e) = package.entries.iter().find(|e| !INSTALLED.contains(&e.path.as_str())) {
        return Err(format!("the package carries {}, which an install never holds", e.path));
    }
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(staging)?;
        for e in &package.entries {
            let mut f = std::fs::File::create(staging.join(&e.path))?;
            f.write_all(&e.bytes)?;
            f.sync_all()?;
        }
        Ok(())
    };
    write().map_err(|e| {
        let _ = std::fs::remove_dir_all(staging);
        format!("staging: {e}")
    })
}

/// The machine of an apply on Windows.
pub struct WinMachine {
    pub layout: Layout,
    pub control: Box<dyn ServiceControl>,
    /// What every moved program must pass (`InstallPolicy::production()`; tests give a temporary root).
    pub policy: InstallPolicy,
    /// The sweep's grace ([`SWEEP_GRACE`]; tests use none).
    pub grace: Duration,
    /// How often [`Machine::wait_commit`] reads the journal.
    pub poll: Duration,
    /// How long a file held by a scanner is retried before the apply is refused ([`MOVE_RETRY`]).
    pub retry: Duration,
    /// The files held (installed first, then the new ones) from [`Machine::claim_targets`] until the commit.
    pub held: Option<lock::Held>,
}

impl WinMachine {
    /// The machine of an apply with the production timings: the sweep's grace, the commit poll and the retry for a file
    /// a scanner holds.
    pub fn new(layout: Layout, control: Box<dyn ServiceControl>, policy: InstallPolicy) -> Self {
        Self {
            layout,
            control,
            policy,
            grace: SWEEP_GRACE,
            poll: Duration::from_millis(250),
            retry: MOVE_RETRY,
            held: None,
        }
    }

    /// Holds the installed files `from` will replace (see [`lock`]); the new hold is taken before the old one is let
    /// go, so the files are never free in between.
    fn claim(&mut self, from: &Path) -> Result<(), String> {
        let held = lock::Held::claim(&self.layout.install, from, self.retry)?;
        self.held = Some(held);
        Ok(())
    }

    /// Puts every installed name found in `from` into the install folder: a copy in the swap folder is moved over the
    /// installed file (so `from` stays whole for a second try), reset to the folder's ACL and, for a program, checked
    /// with the install policy. Before each program the service is stopped again and the folder swept: the SCM's
    /// recovery actions may have restarted a service that ended badly, and its image would hold the file.
    fn put_from(&mut self, from: &Path) -> Result<(), String> {
        if self.held.is_none() {
            self.claim(from)?;
        }
        let swap = self.layout.swap();
        std::fs::create_dir_all(&swap).map_err(|e| format!("the swap folder: {e}"))?;
        for name in INSTALLED {
            let src = from.join(name);
            if !src.is_file() {
                continue;
            }
            if PROGRAMS.contains(&name) {
                if self.control.running() {
                    self.control.stop()?;
                }
                sweep(&self.layout.install, Duration::ZERO)?;
            }
            let copy = swap.join(name);
            copy_through(&src, &copy).map_err(|e| format!("copying {name}: {e}"))?;
            let dst = self.layout.install.join(name);
            let held = self.held.get_or_insert_with(Default::default);
            held.replace_through(name, &copy, &dst)?;
            inherit_parent_acl(&dst)?;
            if PROGRAMS.contains(&name) {
                self.policy.verify(&dst).map_err(|e| format!("{name}: {e}"))?;
            }
        }
        Ok(())
    }
}

/// Copies `src` to `dst` and flushes it to the disk.
fn copy_through(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst)?;
    // Flushing needs write access to the handle.
    std::fs::OpenOptions::new().write(true).open(dst)?.sync_all()
}

impl Machine for WinMachine {
    fn journal(&mut self, entry: &Journal) -> Result<(), String> {
        append_journal(&self.layout.journal(), entry).map_err(|e| format!("journal: {e}"))
    }

    fn stop_service(&mut self) -> Result<(), String> {
        self.control.stop()
    }

    fn sweep(&mut self) -> Result<(), String> {
        sweep(&self.layout.install, self.grace)
    }

    fn claim_targets(&mut self) -> Result<(), String> {
        self.claim(&self.layout.staging())
    }

    fn install_complete(&mut self) -> bool {
        complete(&self.layout.install)
    }

    fn back_up(&mut self, old: &Version) -> Result<(), String> {
        let dir = self.layout.rollback(old);
        // A copy made by an earlier try holds the files from before any was replaced: it is the one to keep. The
        // folder gets its name only once complete (made under another name, then renamed).
        if complete(&dir) {
            return Ok(());
        }
        let partial = dir.with_extension("partial");
        let copy = || -> std::io::Result<()> {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&partial);
            std::fs::create_dir_all(&partial)?;
            for name in INSTALLED {
                let src = self.layout.install.join(name);
                if src.is_file() {
                    copy_through(&src, &partial.join(name))?;
                }
            }
            std::fs::rename(&partial, &dir)
        };
        copy().map_err(|e| format!("back up: {e}"))
    }

    fn put_new(&mut self) -> Result<(), String> {
        self.put_from(&self.layout.staging())
    }

    fn restore(&mut self, old: &Version) -> Result<(), String> {
        let dir = self.layout.rollback(old);
        if !complete(&dir) {
            return Err(format!("the rollback folder {} is incomplete", dir.display()));
        }
        // The files the old set carries are held (the new files' hold is let go once these are), then put back.
        self.claim(&dir)?;
        self.put_from(&dir)
    }

    fn start_service(&mut self) -> Result<(), String> {
        self.control.start()
    }

    fn wait_commit(&mut self, new: &Version, limit: Duration) -> bool {
        let start = Instant::now();
        let want = Journal::Committed { new: new.clone() };
        loop {
            if read_journal(&self.layout.journal()).as_ref() == Some(&want) {
                return true;
            }
            // A service that stopped again will not commit (UPDATES.md 6.4); the first seconds are its start.
            if start.elapsed() >= limit || (start.elapsed() > Duration::from_secs(5) && !self.control.running()) {
                return false;
            }
            std::thread::sleep(self.poll);
        }
    }

    fn staged(&mut self) -> bool {
        complete(&self.layout.staging())
    }

    fn clean_staging(&mut self) {
        if let Some(held) = &mut self.held {
            held.release();
        }
        let _ = std::fs::remove_dir_all(self.layout.staging());
        let _ = std::fs::remove_dir_all(self.layout.swap());
    }
}

/// Drops the file's own entries: its DACL becomes the inheritable entries of its folder alone.
pub fn inherit_parent_acl(path: &Path) -> Result<(), String> {
    let w = wide(path.as_os_str()).map_err(|e| e.to_string())?;
    // An empty ACL: the header alone, 8 bytes, kept in a u32 buffer for its alignment.
    let mut buf = [0u32; 4];
    let acl = buf.as_mut_ptr().cast::<ACL>();
    // SAFETY: `buf` holds 16 bytes, more than the header InitializeAcl writes.
    if unsafe { InitializeAcl(acl, size_of::<ACL>() as u32, ACL_REVISION) } == 0 {
        return Err("InitializeAcl failed".into());
    }
    // SAFETY: `w` is a null-terminated wide string; `acl` is the initialized empty ACL; owner, group and SACL are not
    // changed (null).
    let code = unsafe {
        SetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    if code != 0 { Err(format!("resetting the ACL of {}: os error {code}", path.display())) } else { Ok(()) }
}

/// Ends every Swapdesk program whose image is directly inside `install`, after `grace` for them to exit by themselves.
/// Processes elsewhere, of other names, and this one are left alone.
pub fn sweep(install: &Path, grace: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let left = programs_inside(install);
        if left.is_empty() {
            return Ok(());
        }
        if start.elapsed() >= grace {
            end_programs_inside(install);
            let left = programs_inside(install);
            return if left.is_empty() { Ok(()) } else { Err(format!("processes {left:?} did not end")) };
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The PIDs of the Swapdesk programs running from `install` (not this process).
pub fn programs_inside(install: &Path) -> Vec<u32> {
    let folder = comparable(install);
    // SAFETY: no preconditions.
    let me = unsafe { GetCurrentProcessId() };
    all_pids()
        .into_iter()
        .filter(|&pid| pid != me && pid != 0)
        .filter(|&pid| image_path(pid).is_some_and(|image| is_program_inside(&image, &folder)))
        .collect()
}

/// Whether `image` (a full path) is one of the [`PROGRAMS`] directly inside `folder` (both compared case-blind).
fn is_program_inside(image: &str, folder: &str) -> bool {
    let image = image.to_lowercase();
    let Some(name) = image.strip_prefix(folder).and_then(|rest| rest.strip_prefix('\\')) else { return false };
    PROGRAMS.iter().any(|p| p.to_lowercase() == name)
}

/// A folder as `QueryFullProcessImageNameW` writes it: resolved, without the `\\?\` prefix or a trailing separator,
/// lower case.
fn comparable(folder: &Path) -> String {
    let p = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).trim_end_matches('\\').to_lowercase()
}

fn all_pids() -> Vec<u32> {
    let mut pids = vec![0u32; 4096];
    loop {
        let mut needed = 0u32;
        let bytes = (pids.len() * 4) as u32;
        // SAFETY: `pids` holds `bytes` bytes.
        if unsafe { K32EnumProcesses(pids.as_mut_ptr(), bytes, &mut needed) } == 0 {
            return Vec::new();
        }
        if needed < bytes {
            pids.truncate(needed as usize / 4);
            return pids;
        }
        pids.resize(pids.len() * 2, 0);
    }
}

fn image_path(pid: u32) -> Option<String> {
    // SAFETY: plain arguments; the handle is closed below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let image = image_of(h);
    // SAFETY: `h` is valid and not used after this.
    unsafe { CloseHandle(h) };
    image
}

/// The image of the process `h` (opened with at least `PROCESS_QUERY_LIMITED_INFORMATION`).
fn image_of(h: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units; `h` is a live process handle with the query right.
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// Ends each Swapdesk program inside `install`: its image is read and the process ended through one handle, so a PID
/// reused between a look and the end is never ended.
fn end_programs_inside(install: &Path) {
    let folder = comparable(install);
    // SAFETY: no preconditions.
    let me = unsafe { GetCurrentProcessId() };
    for pid in all_pids().into_iter().filter(|&pid| pid != me && pid != 0) {
        // SAFETY: plain arguments; the handle is closed below.
        let h =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid) };
        if h.is_null() {
            continue;
        }
        if image_of(h).is_some_and(|image| is_program_inside(&image, &folder)) {
            // SAFETY: `h` was opened with PROCESS_TERMINATE and PROCESS_SYNCHRONIZE.
            unsafe {
                TerminateProcess(h, 1);
                WaitForSingleObject(h, 5000);
            }
        }
        // SAFETY: `h` is valid and closed once.
        unsafe { CloseHandle(h) };
    }
}

pub mod lock;
#[cfg(test)]
mod tests;
