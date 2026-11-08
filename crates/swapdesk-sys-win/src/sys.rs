//! Thin, checked wrappers shared by the modules. Every `unsafe` block says what it relies on.

use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::ptr::null_mut;

use swapdesk_platform::{ErrorKind, PlatformError};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSID, SID_AND_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_GROUPS, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenElevationType, TokenGroups, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub type Result<T> = core::result::Result<T, PlatformError>;

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
pub fn wide(s: &OsStr) -> Result<Vec<u16>> {
    let mut v: Vec<u16> = s.encode_wide().collect();
    if v.contains(&0) {
        return Err(invalid("wide", "string holds a NUL"));
    }
    v.push(0);
    Ok(v)
}

/// The text of a null-terminated wide string.
///
/// # Safety
/// `p` must point to a null-terminated UTF-16 string that stays valid for the call.
pub unsafe fn from_wide_ptr(p: *const u16) -> String {
    // SAFETY: the caller guarantees a null-terminated string, so every index up to the terminator is readable.
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    // SAFETY: `len` elements before the terminator were just read.
    OsString::from_wide(unsafe { std::slice::from_raw_parts(p, len) }).to_string_lossy().into_owned()
}

/// A Win32 `HANDLE` closed on drop.
pub struct OwnedHandle(pub HANDLE);

// SAFETY: a kernel handle is an index into the process handle table; any thread may use or close it.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; the calls made through `&OwnedHandle` are the thread-safe kernel calls.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from a successful Win32 call and is closed once.
            unsafe { CloseHandle(self.0) };
        }
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

/// A SID kept alive for as long as a security descriptor or ACL built from it is used.
pub struct Sid {
    mem: LocalMem,
}

impl Sid {
    /// Parses `S-1-5-...` or an SDDL alias ("SY", "BA").
    pub fn parse(text: &str) -> Result<Self> {
        let w = wide(OsStr::new(text))?;
        let mut psid: PSID = null_mut();
        // SAFETY: `w` is a null-terminated wide string; the result is freed by LocalMem.
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut psid) } == 0 {
            return Err(last_error("ConvertStringSidToSidW"));
        }
        Ok(Self { mem: LocalMem(psid) })
    }

    pub fn as_ptr(&self) -> PSID {
        self.mem.0
    }
}

/// `S-1-5-...` text of a SID.
pub fn sid_to_string(psid: PSID) -> Result<String> {
    let mut out: *mut u16 = null_mut();
    // SAFETY: `psid` is a valid SID; the result is freed by LocalMem.
    if unsafe { ConvertSidToStringSidW(psid, &mut out) } == 0 {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    let _free = LocalMem(out.cast());
    // SAFETY: the call above returned a null-terminated string.
    Ok(unsafe { from_wide_ptr(out) })
}

/// `S-1-5-...` for a SID string that may be an SDDL alias.
pub fn normalize_sid(text: &str) -> Result<String> {
    sid_to_string(Sid::parse(text)?.as_ptr())
}

/// Opens the current process token.
pub fn open_process_token(access: u32) -> Result<OwnedHandle> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo-handle of the current process is valid; `token` is a live out-parameter.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token) } == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    Ok(OwnedHandle(token))
}

/// A zeroed buffer aligned for the structures `GetTokenInformation` writes (8-byte aligned).
pub struct TokenInfo(Vec<u64>);

impl TokenInfo {
    pub fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr().cast()
    }
}

/// `GetTokenInformation` with the size query done first.
pub fn token_info(token: HANDLE, class: i32) -> Result<TokenInfo> {
    let mut needed = 0u32;
    // SAFETY: a size query with a null buffer is the documented way to learn the needed length.
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(last_error("GetTokenInformation"));
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes and the caller's handle is a token opened with TOKEN_QUERY.
    if unsafe { GetTokenInformation(token, class, buf.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
        return Err(last_error("GetTokenInformation"));
    }
    Ok(TokenInfo(buf))
}

/// The user SID of a token.
pub fn token_user_sid(token: HANDLE) -> Result<String> {
    let info = token_info(token, TokenUser)?;
    // SAFETY: the buffer holds a TOKEN_USER written by the call above, 8-byte aligned.
    let user = unsafe { &*info.as_ptr().cast::<TOKEN_USER>() };
    sid_to_string(user.User.Sid)
}

/// Every group of a token as `(sid, attributes)`.
pub fn token_groups(token: HANDLE) -> Result<Vec<(String, u32)>> {
    let info = token_info(token, TokenGroups)?;
    // SAFETY: the buffer holds a TOKEN_GROUPS written by the call above, 8-byte aligned.
    let groups = unsafe { &*info.as_ptr().cast::<TOKEN_GROUPS>() };
    let first: *const SID_AND_ATTRIBUTES = groups.Groups.as_ptr();
    let mut out = Vec::with_capacity(groups.GroupCount as usize);
    for i in 0..groups.GroupCount as usize {
        // SAFETY: the kernel wrote `GroupCount` entries starting at `Groups`, all inside the buffer.
        let g = unsafe { &*first.add(i) };
        out.push((sid_to_string(g.Sid)?, g.Attributes));
    }
    Ok(out)
}

/// `TokenIsElevated` of a token.
pub fn token_is_elevated(token: HANDLE) -> Result<bool> {
    let info = token_info(token, TokenElevation)?;
    // SAFETY: the buffer holds a TOKEN_ELEVATION written by the call above.
    Ok(unsafe { &*info.as_ptr().cast::<TOKEN_ELEVATION>() }.TokenIsElevated != 0)
}

/// `TokenElevationType` of a token: 1 default, 2 full (elevated, has a linked limited token), 3 limited.
pub fn token_elevation_type(token: HANDLE) -> Result<u32> {
    let info = token_info(token, TokenElevationType)?;
    // SAFETY: the buffer holds a TOKEN_ELEVATION_TYPE (an i32 enum) written by the call above.
    Ok(unsafe { *info.as_ptr().cast::<i32>() } as u32)
}

/// The SID of the user this process runs as, as `S-1-5-21-...`.
pub fn current_user_sid() -> Result<String> {
    let token = open_process_token(TOKEN_QUERY)?;
    token_user_sid(token.0)
}
