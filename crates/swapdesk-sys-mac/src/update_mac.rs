//! The Mac side of the update (`docs/UPDATES.md` 7): the checks before a swap, the swap itself and the watcher.
//!
//! * [`same_signer`]: the staged bundle must satisfy the designated requirement of the running one
//!   (`SecStaticCodeCreateWithPath`, `SecCodeCopyDesignatedRequirement`, `SecStaticCodeCheckValidity` with strict
//!   validation of every architecture and nested code). A bundle signed by another identity, or altered, is refused.
//! * [`check_location`]: a translocated app (`AppTranslocation`) and a parent folder this user cannot write are
//!   refused with their reason (7.2 step 3).
//! * [`stage_bundle`] writes the package's `Swapdesk.app/...` entries beside the running bundle; [`swap`] is
//!   `renamex_np(RENAME_SWAP)`, which exchanges two paths on one volume.
//! * [`MacWatcher`] is the [`Watcher`] of `swapdesk_update::watch`.
//!
//! Sources read on 2026-11-11: `SecStaticCodeCheckValidity`
//! <https://developer.apple.com/documentation/security/secstaticcodecheckvalidity(_:_:_:)>,
//! `SecCodeCopyDesignatedRequirement`
//! <https://developer.apple.com/documentation/security/seccodecopydesignatedrequirement(_:_:_:)>, `renamex_np(2)` (man
//! page on the Mac: "RENAME_SWAP: On file systems that support it, it will cause the source and target to be
//! atomically swapped").

use std::ffi::{CString, c_void};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use swapdesk_update::Version;
use swapdesk_update::apply::Journal;
use swapdesk_update::package::Package;
use swapdesk_update::watch::Watcher;

type CFTypeRef = *const c_void;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecStaticCodeCreateWithPath(path: CFTypeRef, flags: u32, code: *mut CFTypeRef) -> i32;
    fn SecCodeCopyDesignatedRequirement(code: CFTypeRef, flags: u32, requirement: *mut CFTypeRef) -> i32;
    fn SecStaticCodeCheckValidity(code: CFTypeRef, flags: u32, requirement: CFTypeRef) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFTypeRef,
        buffer: *const u8,
        length: isize,
        is_directory: u8,
    ) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

/// kSecCSCheckAllArchitectures | kSecCSCheckNestedCode | kSecCSStrictValidate.
const STRICT: u32 = 1 | 8 | 16;
/// errSecCSReqFailed: the code is valid but does not satisfy the requirement.
const REQ_FAILED: i32 = -67050;

/// A Core Foundation object released on drop.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the object came from a Create or Copy call and is released once.
            unsafe { CFRelease(self.0) };
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignError {
    /// The code at that path could not be read as signed code (OSStatus).
    Unreadable(i32),
    /// The reference has no designated requirement to compare with.
    NoRequirement(i32),
    /// Signed by another identity (the requirement is not met).
    OtherIdentity,
    /// Altered or badly signed (OSStatus of the strict check).
    Invalid(i32),
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(s) => write!(f, "not readable as signed code (OSStatus {s})"),
            Self::NoRequirement(s) => write!(f, "the running app has no designated requirement (OSStatus {s})"),
            Self::OtherIdentity => write!(f, "signed by another identity"),
            Self::Invalid(s) => write!(f, "its signature does not check (OSStatus {s})"),
        }
    }
}

fn static_code(path: &Path) -> Result<Cf, i32> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: `bytes` is live for the call; a null allocator is the default one.
    let url = Cf(unsafe {
        CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 0)
    });
    if url.0.is_null() {
        return Err(-1);
    }
    let mut code: CFTypeRef = std::ptr::null();
    // SAFETY: `url` is a live CFURL; `code` receives a +1 reference on success.
    let status = unsafe { SecStaticCodeCreateWithPath(url.0, 0, &mut code) };
    if status != 0 || code.is_null() { Err(status) } else { Ok(Cf(code)) }
}

/// Whether the code at `staged` is validly signed and meets the designated requirement of the code at `reference`
/// (the running bundle: same identifier, same signing identity).
pub fn same_signer(staged: &Path, reference: &Path) -> Result<(), SignError> {
    let reference = static_code(reference).map_err(SignError::Unreadable)?;
    let mut requirement: CFTypeRef = std::ptr::null();
    // SAFETY: `reference` is a live SecStaticCode; `requirement` receives a +1 reference on success.
    let status = unsafe { SecCodeCopyDesignatedRequirement(reference.0, 0, &mut requirement) };
    if status != 0 || requirement.is_null() {
        return Err(SignError::NoRequirement(status));
    }
    let requirement = Cf(requirement);
    let staged = static_code(staged).map_err(SignError::Unreadable)?;
    // SAFETY: both are live objects of the right types.
    match unsafe { SecStaticCodeCheckValidity(staged.0, STRICT, requirement.0) } {
        0 => Ok(()),
        REQ_FAILED => Err(SignError::OtherIdentity),
        s => Err(SignError::Invalid(s)),
    }
}

/// Why a bundle cannot be replaced where it is (7.2 step 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationRefusal {
    /// Gatekeeper runs it from a read-only copy: "Mova o Swapdesk para Aplicativos".
    Translocated,
    /// This user cannot write its parent folder: "Só um administrador deste Mac pode substituir o Swapdesk".
    NotWritable,
}

/// Whether the bundle at `bundle` can be swapped by this user.
pub fn check_location(bundle: &Path) -> Result<(), LocationRefusal> {
    if bundle.components().any(|c| c.as_os_str() == "AppTranslocation") {
        return Err(LocationRefusal::Translocated);
    }
    let parent = bundle.parent().ok_or(LocationRefusal::NotWritable)?;
    let c = CString::new(parent.as_os_str().as_bytes()).map_err(|_| LocationRefusal::NotWritable)?;
    // SAFETY: `c` is a NUL-terminated path alive for the call.
    if unsafe { libc::access(c.as_ptr(), libc::W_OK) } != 0 {
        return Err(LocationRefusal::NotWritable);
    }
    Ok(())
}

/// The `.app` folder that holds `exe` (`<X>.app/Contents/MacOS/<name>`).
pub fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// Where the new bundle is staged beside the running one (same volume): `<parent>/.Swapdesk-<new>.app`. After the swap
/// it holds the old bundle.
pub fn hidden_path(running: &Path, new: &Version) -> PathBuf {
    running.with_file_name(format!(".Swapdesk-{new}.app"))
}

/// Exchanges `a` and `b` (`renamex_np` with `RENAME_SWAP`).
pub fn swap(a: &Path, b: &Path) -> std::io::Result<()> {
    let ca = CString::new(a.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    let cb = CString::new(b.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: both are NUL-terminated paths alive for the call.
    if unsafe { libc::renamex_np(ca.as_ptr(), cb.as_ptr(), libc::RENAME_SWAP) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The journal's name inside the updater's folder.
pub const JOURNAL: &str = "journal";

/// The prefix every entry of a Mac package carries.
pub const BUNDLE_PREFIX: &str = "Swapdesk.app/";

/// Writes the package's bundle into `dest` (a fresh folder), each file with its mode. Refused when an entry lies
/// outside `Swapdesk.app/` (the package's paths are already checked: relative, no `..`).
pub fn stage_bundle(package: &Package, dest: &Path) -> Result<(), String> {
    let _ = std::fs::remove_dir_all(dest);
    if let Some(e) = package.entries.iter().find(|e| !e.path.starts_with(BUNDLE_PREFIX)) {
        return Err(format!("the package carries {}, outside the app", e.path));
    }
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(dest)?;
        for e in &package.entries {
            let path = dest.join(&e.path[BUNDLE_PREFIX.len()..]);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::File::create(&path)?;
            f.write_all(&e.bytes)?;
            f.sync_all()?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(u32::from(e.mode)))?;
        }
        Ok(())
    };
    write().map_err(|e| {
        let _ = std::fs::remove_dir_all(dest);
        format!("staging the bundle: {e}")
    })
}

/// Appends `entry` to the journal at `path`, flushed before it returns.
pub fn append_journal(path: &Path, entry: &Journal) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(format!("{}\n", entry.to_line()).as_bytes())?;
    f.sync_all()
}

/// The journal's last line.
pub fn read_journal(path: &Path) -> Option<Journal> {
    Journal::last_of(&std::fs::read_to_string(path).ok()?)
}

/// The processes whose executable is `exe`.
pub fn pids_of(exe: &Path) -> Vec<i32> {
    let mut pids = vec![0i32; 8192];
    // SAFETY: the buffer holds `pids.len()` ints; the call returns how many it wrote.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
    if n <= 0 {
        return Vec::new();
    }
    pids.truncate(n as usize);
    let want = exe.as_os_str().as_bytes();
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter(|&pid| {
            let mut buf = [0u8; 4096];
            // SAFETY: `buf` holds 4096 bytes (PROC_PIDPATHINFO_MAXSIZE).
            let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
            len > 0 && &buf[..len as usize] == want
        })
        .collect()
}

/// The executable of a bundle: the one file in `Contents/MacOS`.
pub fn executable_of(bundle: &Path) -> Option<PathBuf> {
    let mut files = std::fs::read_dir(bundle.join("Contents").join("MacOS")).ok()?.flatten();
    let first = files.next()?.path();
    files.next().is_none().then_some(first)
}

/// The watcher on a Mac.
pub struct MacWatcher {
    /// The running path (`/Applications/Swapdesk.app`).
    pub running: PathBuf,
    /// The hidden path the old bundle went to.
    pub hidden: PathBuf,
    /// The app that started the watcher.
    pub pid: i32,
    pub journal: PathBuf,
    pub poll: Duration,
}

impl Watcher for MacWatcher {
    fn wait_quit(&mut self, limit: Duration) -> bool {
        let start = Instant::now();
        loop {
            // SAFETY: signal 0 only checks that the process exists.
            if unsafe { libc::kill(self.pid, 0) } != 0 {
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(self.poll);
        }
    }

    fn open(&mut self) -> Result<(), String> {
        let status = std::process::Command::new("/usr/bin/open")
            .arg(&self.running)
            .status()
            .map_err(|e| format!("open: {e}"))?;
        if status.success() { Ok(()) } else { Err(format!("open ended with {status}")) }
    }

    fn wait_commit(&mut self, new: &Version, limit: Duration) -> bool {
        let start = Instant::now();
        let want = Journal::Committed { new: new.clone() };
        loop {
            if read_journal(&self.journal).as_ref() == Some(&want) {
                return true;
            }
            if start.elapsed() >= limit {
                return false;
            }
            std::thread::sleep(self.poll);
        }
    }

    fn end_running(&mut self) {
        let Some(exe) = executable_of(&self.running) else { return };
        for pid in pids_of(&exe) {
            // SAFETY: plain signals to a process found by its executable path.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        let until = Instant::now() + Duration::from_secs(5);
        while !pids_of(&exe).is_empty() && Instant::now() < until {
            std::thread::sleep(self.poll);
        }
        for pid in pids_of(&exe) {
            // SAFETY: as above.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    fn swap_back(&mut self) -> Result<(), String> {
        swap(&self.hidden, &self.running).map_err(|e| e.to_string())
    }

    fn remove_hidden(&mut self) {
        let _ = std::fs::remove_dir_all(&self.hidden);
    }

    fn journal(&mut self, entry: &Journal) -> Result<(), String> {
        append_journal(&self.journal, entry).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
#[path = "update_mac_tests.rs"]
mod tests;
