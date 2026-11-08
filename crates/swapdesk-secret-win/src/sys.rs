//! Thin, checked wrappers over the Win32 calls this crate needs. Every `unsafe` block says what it relies on.

use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;
use std::ptr::{null, null_mut};

use swapdesk_platform::{ErrorKind, PlatformError};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetNamedSecurityInfoW,
    GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FlushFileBuffers, GetFileInformationByHandle,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING, READ_CONTROL, ReadFile, WRITE_DAC,
    WriteFile,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

pub fn os_error(op: &'static str, code: u32) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: Some(i64::from(code)), detail: format!("Win32 error {code}") }
}

pub fn last_error(op: &'static str) -> PlatformError {
    // SAFETY: GetLastError has no preconditions.
    os_error(op, unsafe { GetLastError() })
}

pub fn invalid(op: &'static str, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind: ErrorKind::InvalidInput, op, os_code: None, detail: detail.into() }
}

/// A null-terminated UTF-16 copy of `s`; an interior NUL is refused.
pub fn wide(s: &OsStr) -> Result<Vec<u16>, PlatformError> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    if v.contains(&0) {
        return Err(invalid("wide", "string holds a NUL"));
    }
    v.push(0);
    Ok(v)
}

fn from_wide_ptr(p: *const u16) -> OsString {
    // SAFETY: callers pass a pointer returned by a Win32 call as a null-terminated wide string.
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    // SAFETY: `len` elements before the terminator were just read.
    OsString::from_wide(unsafe { std::slice::from_raw_parts(p, len) })
}

/// Overwrites `bytes` with zeros in a way the optimizer may not drop (volatile writes, then a fence): key material
/// is wiped with this before its memory is freed or reused.
pub fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference to one byte.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
}

/// A Win32 `HANDLE` closed on drop.
pub struct Handle(pub HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful CreateFileW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Memory allocated by the system and released with `LocalFree`.
pub struct LocalMem(pub *mut c_void);

impl Drop for LocalMem {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from a Win32 call documented to need LocalFree and is freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Opens a file or directory without following a reparse point at its last component.
pub fn open_no_follow(
    path: &Path,
    access: u32,
    creation: u32,
    extra_flags: u32,
    sa: *const SECURITY_ATTRIBUTES,
) -> Result<Handle, PlatformError> {
    let w = wide(path.as_os_str())?;
    // SAFETY: `w` is a null-terminated wide string that outlives the call; `sa` is null or points to a live struct.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            sa,
            creation,
            FILE_FLAG_OPEN_REPARSE_POINT | extra_flags,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE { Err(last_error("CreateFileW")) } else { Ok(Handle(h)) }
}

#[cfg(test)]
pub fn open_dir_for_security(path: &Path, write_dac: bool) -> Result<Handle, PlatformError> {
    let access = FILE_READ_ATTRIBUTES | READ_CONTROL | if write_dac { WRITE_DAC } else { 0 };
    open_no_follow(path, access, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, null())
}

/// The one handle `ensure_folder` judges and fixes a folder through (finding 6): attributes, owner and DACL to
/// read, DACL to write, nothing that reads or changes data, so it never conflicts with another opener's sharing.
pub fn open_dir_for_judging(path: &Path) -> Result<Handle, PlatformError> {
    open_no_follow(
        path,
        READ_CONTROL | WRITE_DAC | FILE_READ_ATTRIBUTES,
        OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS,
        null(),
    )
}

/// Opens only the attributes of a name without following a link, to compare identities.
pub fn open_attributes_only(path: &Path) -> Result<Handle, PlatformError> {
    open_no_follow(path, FILE_READ_ATTRIBUTES, OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS, null())
}

/// Volume serial number and file index: what says two handles name the same object.
pub fn file_id(h: &Handle) -> Result<(u32, u64), PlatformError> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data; zeroed is a valid initial value.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: the handle is open and `info` is a live out-parameter.
    if unsafe { GetFileInformationByHandle(h.0, &mut info) } == 0 {
        return Err(last_error("GetFileInformationByHandle"));
    }
    Ok((info.dwVolumeSerialNumber, (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
}

/// `(is_directory, is_reparse_point)` of the object the handle names (the link itself, not its target).
pub fn kind_of(h: &Handle) -> Result<(bool, bool), PlatformError> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is plain data; zeroed is a valid initial value.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: the handle is open and `info` is a live out-parameter.
    if unsafe { GetFileInformationByHandle(h.0, &mut info) } == 0 {
        return Err(last_error("GetFileInformationByHandle"));
    }
    Ok((
        info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    ))
}

pub fn read_all(h: &Handle) -> Result<Vec<u8>, PlatformError> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut got = 0u32;
        // SAFETY: the handle is open for reading; `buf` and `got` are live and `buf.len()` fits in u32.
        let ok = unsafe { ReadFile(h.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut got, null_mut()) };
        if ok == 0 {
            return Err(last_error("ReadFile"));
        }
        if got == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..got as usize]);
        if out.len() > 16 * 1024 * 1024 {
            return Err(invalid("ReadFile", "file over 16 MiB"));
        }
    }
}

pub fn write_all(h: &Handle, mut bytes: &[u8]) -> Result<(), PlatformError> {
    while !bytes.is_empty() {
        let chunk = bytes.len().min(1 << 20);
        let mut put = 0u32;
        // SAFETY: the handle is open for writing; `bytes` has at least `chunk` bytes and `put` is live.
        let ok = unsafe { WriteFile(h.0, bytes.as_ptr().cast(), chunk as u32, &mut put, null_mut()) };
        if ok == 0 || put == 0 {
            return Err(last_error("WriteFile"));
        }
        bytes = &bytes[put as usize..];
    }
    Ok(())
}

pub fn flush(h: &Handle) -> Result<(), PlatformError> {
    // SAFETY: the handle is open for writing.
    if unsafe { FlushFileBuffers(h.0) } == 0 { Err(last_error("FlushFileBuffers")) } else { Ok(()) }
}

/// Creates a file that must not exist yet, without following a reparse point, for writing.
pub fn create_new_file(path: &Path) -> Result<Handle, PlatformError> {
    open_no_follow(path, GENERIC_WRITE, CREATE_NEW, 0, null())
}

/// Opens an existing file for reading, refusing a reparse point or a directory.
pub fn open_file_for_read(path: &Path) -> Result<Option<Handle>, PlatformError> {
    match open_no_follow(path, GENERIC_READ, OPEN_EXISTING, 0, null()) {
        Ok(h) => {
            let (dir, reparse) = kind_of(&h)?;
            if dir || reparse {
                return Err(invalid("open_file_for_read", "not a regular file (directory or reparse point)"));
            }
            Ok(Some(h))
        }
        Err(e) if e.os_code == Some(2) || e.os_code == Some(3) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Removes a name. A reparse point is removed itself; its target is not touched.
pub fn delete_file(path: &Path) -> Result<(), PlatformError> {
    let w = wide(path.as_os_str())?;
    // SAFETY: `w` is a null-terminated wide string that outlives the call.
    if unsafe { DeleteFileW(w.as_ptr()) } == 0 { Err(last_error("DeleteFileW")) } else { Ok(()) }
}

/// Renames `from` to `to`; with `replace`, an existing `to` is replaced (a link there is replaced, not followed).
pub fn rename(from: &Path, to: &Path, replace: bool) -> Result<(), PlatformError> {
    let (f, t) = (wide(from.as_os_str())?, wide(to.as_os_str())?);
    let flags = MOVEFILE_WRITE_THROUGH | if replace { MOVEFILE_REPLACE_EXISTING } else { 0 };
    // SAFETY: both are null-terminated wide strings that outlive the call.
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), flags) } == 0 { Err(last_error("MoveFileExW")) } else { Ok(()) }
}

/// A security descriptor parsed from SDDL, freed on drop.
pub struct SecurityDescriptor {
    sd: PSECURITY_DESCRIPTOR,
    _owner: LocalMem,
}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> Result<Self, PlatformError> {
        let w = wide(OsStr::new(sddl))?;
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `w` is a null-terminated wide string; `sd` is a live out-parameter; the result is freed by LocalMem.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        };
        if ok == 0 {
            return Err(last_error("ConvertStringSecurityDescriptorToSecurityDescriptorW"));
        }
        Ok(Self { sd, _owner: LocalMem(sd) })
    }

    pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.sd
    }

    /// The DACL inside the descriptor.
    pub fn dacl(&self) -> Result<*mut ACL, PlatformError> {
        let (mut present, mut defaulted, mut dacl) = (0i32, 0i32, null_mut());
        // SAFETY: `self.sd` is a valid descriptor; the out-parameters are live. The ACL stays valid as long as `self`.
        if unsafe { GetSecurityDescriptorDacl(self.sd, &mut present, &mut dacl, &mut defaulted) } == 0 {
            return Err(last_error("GetSecurityDescriptorDacl"));
        }
        if present == 0 || dacl.is_null() {
            return Err(invalid("GetSecurityDescriptorDacl", "the descriptor has no DACL"));
        }
        Ok(dacl)
    }
}

/// Creates a directory with this descriptor from the first instant (no window with a weaker DACL). `Ok(false)`
/// means the name already exists.
pub fn create_dir_with(path: &Path, sd: &SecurityDescriptor) -> Result<bool, PlatformError> {
    let w = wide(path.as_os_str())?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: `w` and `sa` are live for the call; the descriptor is valid.
    if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } != 0 {
        return Ok(true);
    }
    // SAFETY: no preconditions.
    match unsafe { GetLastError() } {
        183 /* ERROR_ALREADY_EXISTS */ | 80 /* ERROR_FILE_EXISTS */ => Ok(false),
        code => Err(os_error("CreateDirectoryW", code)),
    }
}

/// Sets a protected DACL on an open directory handle (no path is resolved again).
pub fn set_dacl(h: &Handle, dacl: *mut ACL) -> Result<(), PlatformError> {
    // SAFETY: the handle has WRITE_DAC; `dacl` points into a descriptor that outlives the call.
    let err = unsafe {
        SetSecurityInfo(
            h.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null_mut(),
        )
    };
    if err == 0 { Ok(()) } else { Err(os_error("SetSecurityInfo", err)) }
}

/// `S-1-5-...` for a SID string that may be an SDDL alias such as "SY" or "BA".
pub fn normalize_sid(sid: &str) -> Result<String, PlatformError> {
    let w = wide(OsStr::new(sid))?;
    let mut psid: PSID = null_mut();
    // SAFETY: `w` is a null-terminated wide string; the result is freed by LocalMem.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut psid) } == 0 {
        return Err(last_error("ConvertStringSidToSidW"));
    }
    let _free = LocalMem(psid);
    sid_to_string(psid)
}

fn sid_to_string(psid: PSID) -> Result<String, PlatformError> {
    let mut out: *mut u16 = null_mut();
    // SAFETY: `psid` is a valid SID; the result is freed by LocalMem.
    if unsafe { ConvertSidToStringSidW(psid, &mut out) } == 0 {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    let _free = LocalMem(out.cast());
    Ok(from_wide_ptr(out).to_string_lossy().into_owned())
}

/// The SID of the user this process runs as, as `S-1-5-21-...`.
pub fn current_user_sid() -> Result<String, PlatformError> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo-handle of the current process is valid; `token` is a live out-parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let token = Handle(token);
    let mut needed = 0u32;
    // SAFETY: a size query with a null buffer is the documented way to learn the needed length.
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed) };
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` has `needed` bytes and the handle is a token opened with TOKEN_QUERY.
    if unsafe { GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
        return Err(last_error("GetTokenInformation"));
    }
    // SAFETY: the buffer now holds a TOKEN_USER (aligned for the pointer inside by Vec<u8>'s allocation on x86-64 and arm64 Windows).
    let user = unsafe { core::ptr::read_unaligned(buf.as_ptr().cast::<TOKEN_USER>()) };
    sid_to_string(user.User.Sid)
}

/// What a folder's security looks like now, read back from the file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderSecurity {
    /// Owner as `S-1-5-...`.
    pub owner: String,
    /// The DACL as SDDL, for example `D:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)`.
    pub dacl_sddl: String,
}

/// Reads owner and DACL with `GetNamedSecurityInfoW`.
pub fn read_security(path: &Path) -> Result<FolderSecurity, PlatformError> {
    let w = wide(path.as_os_str())?;
    let (mut owner, mut dacl, mut sd): (PSID, *mut ACL, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut(), null_mut());
    // SAFETY: `w` is a null-terminated wide string; the out-parameters are live; `sd` is freed by LocalMem.
    let err = unsafe {
        GetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(os_error("GetNamedSecurityInfoW", err));
    }
    let _free = LocalMem(sd);
    describe(owner, sd)
}

/// Owner and DACL of an open handle (needs READ_CONTROL), without resolving any path again.
pub fn security_of(h: &Handle) -> Result<FolderSecurity, PlatformError> {
    let (mut owner, mut dacl, mut sd): (PSID, *mut ACL, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut(), null_mut());
    // SAFETY: the handle has READ_CONTROL; the out-parameters are live; `sd` is freed by LocalMem.
    let err = unsafe {
        GetSecurityInfo(
            h.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(os_error("GetSecurityInfo", err));
    }
    let _free = LocalMem(sd);
    describe(owner, sd)
}

/// `owner` points into the self-relative descriptor `sd`, which the caller keeps alive for the call.
fn describe(owner: PSID, sd: PSECURITY_DESCRIPTOR) -> Result<FolderSecurity, PlatformError> {
    let owner_str = sid_to_string(owner)?;
    let mut text: *mut u16 = null_mut();
    // SAFETY: `sd` is a valid self-relative descriptor; the string is freed by LocalMem.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error("ConvertSecurityDescriptorToStringSecurityDescriptorW"));
    }
    let _free_text = LocalMem(text.cast());
    Ok(FolderSecurity { owner: owner_str, dacl_sddl: from_wide_ptr(text).to_string_lossy().into_owned() })
}

/// The owner of an open directory handle, as `S-1-5-...` (the handle was opened without following links).
pub fn owner_of(h: &Handle) -> Result<String, PlatformError> {
    let (mut owner, mut sd): (PSID, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
    // SAFETY: the handle has READ_CONTROL; the out-parameters are live; `sd` is freed by LocalMem.
    let err = unsafe {
        GetSecurityInfo(
            h.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        )
    };
    if err != 0 {
        return Err(os_error("GetSecurityInfo", err));
    }
    let _free = LocalMem(sd);
    sid_to_string(owner)
}
