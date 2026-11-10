//! The local named pipes of contract C2.1: byte mode, an explicit DACL built from SIDs, first-instance creation so a
//! squatted name is detected, remote clients refused, and the server-side checks of C2.3.
//!
//! Primary sources read on 2026-11-08 (they settle the flags C2.1 marked UNPROVEN):
//! - `CreateNamedPipeW`: <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea>
//!   (the one page for the A and W forms). `FILE_FLAG_FIRST_PIPE_INSTANCE`: "creation of the first instance succeeds,
//!   but creation of the next instance fails with `ERROR_ACCESS_DENIED`" (so that code is the squat signal; the page
//!   gives the same code for mismatched type/instance-count/timeout, which this crate never varies). `PIPE_REJECT_REMOTE_CLIENTS`
//!   (0x8): "Connections from remote clients are automatically rejected." A null security descriptor gives
//!   LocalSystem, administrators and the creator owner full control **and Everyone read**, so the DACL here is always explicit.
//!   `FILE_FLAG_OVERLAPPED` allows "simultaneous read and write operations on the same pipe handle".
//! - `CreateFileW`: <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew>. `SECURITY_SQOS_PRESENT`
//!   must be set for the SQOS flags to count; `SECURITY_IDENTIFICATION` "impersonates a client at the Identification
//!   impersonation level" (the server can identify, never act as, the user). Without `SECURITY_SQOS_PRESENT` the
//!   default is Impersonation level.
//! - `GetNamedPipeServerProcessId`: <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeserverprocessid>
//!   (Windows Vista and later; the handle "must be created by the CreateNamedPipe function"), and its twin
//!   `GetNamedPipeClientProcessId` <https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeclientprocessid>.
//! - `ImpersonateNamedPipeClient`: <https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-impersonatenamedpipeclient>.
//!   It impersonates "the security context of the **last message read from the pipe**", so the server must have read
//!   at least the client's `Hello` before [`PipeConn::identify_client`]. If it fails the thread is not impersonating
//!   and the caller must stop (the page's warning); this crate returns the error and never continues.
//!
//! # I/O model
//! The pipe handle is opened `FILE_FLAG_OVERLAPPED` and every `read`/`write`/`accept` is one overlapped operation that
//! waits on a per-[`PipeConn`] event. A synchronous handle would serialize a blocked `ReadFile` and a `WriteFile` on
//! the same handle (the writer thread would stall behind the reader), so one handle is shared by an `Arc` between
//! [`PipeConn::try_clone`] copies and [`PipeConn::split`] halves, and each copy owns its own event. A timeout or
//! [`PipeConn::cancel_io`] cancels the pending operation with `CancelIoEx` and waits for it to finish, so the stack
//! `OVERLAPPED` is never touched after the call returns. A peer that closes ends a read with `Ok(0)`.
//!
//! # Limit
//! `GetNamedPipeClientProcessId` names the process that opened the handle; a handle duplicated into another process
//! keeps naming the opener. The DACL and the impersonated SID are what bind the connection to a user, the PID check
//! binds it to the process the service launched.

use std::ffi::OsStr;
use std::io;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::time::{Duration, Instant};

use swapdesk_platform::{ErrorKind, PlatformError};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    ImpersonateNamedPipeClient, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
    WaitNamedPipeW,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, INFINITE, OpenThreadToken, ResetEvent, WaitForSingleObject,
};

use crate::acl::{AceInfo, dacl_of_kernel_handle};
use crate::launch::HelperRole;
use crate::sys::{
    LocalMem, OwnedHandle, current_user_sid, last_error, os_error, token_groups, token_is_elevated, token_user_sid,
    wide,
};

/// Every Swapdesk pipe lives under this namespace prefix.
pub const PIPE_PREFIX: &str = r"\\.\pipe\swapdesk\";

/// SYSTEM's well-known SID.
pub const SYSTEM_SID: &str = "S-1-5-18";
/// The built-in Administrators group.
pub const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

/// `FILE_ALL_ACCESS`.
const OWNER_ROW_MASK: u32 = 0x001F_01FF;
/// What the user's row grants: `FILE_GENERIC_READ | FILE_GENERIC_WRITE` without `FILE_APPEND_DATA`, which on a pipe
/// is `FILE_CREATE_PIPE_INSTANCE`. The user can talk through the pipe but cannot create a second instance of it.
/// (Reading of C2.1's "read/write": UNPROVEN end to end until the service runs under SCM; the dev test proves the
/// client open succeeds with exactly this mask.)
pub const USER_ROW_MASK: u32 = 0x0012_019B;

const BUFFER_BYTES: u32 = 64 * 1024;

/// `TOKEN_GROUPS` attribute bits (`SE_GROUP_ENABLED`, `SE_GROUP_USE_FOR_DENY_ONLY`).
const SE_GROUP_ENABLED: u32 = 0x4;
const SE_GROUP_USE_FOR_DENY_ONLY: u32 = 0x10;

// ---------- errors ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeError {
    /// The first-instance create failed with access denied: another process already owns the name (C2.1).
    Squatted {
        name: String,
    },
    /// The pipe's server process is not the one the caller expected.
    ServerPidMismatch {
        expected: u32,
        actual: u32,
    },
    /// A timeout elapsed.
    Timeout,
    Os(PlatformError),
}

impl PipeError {
    pub fn is_squatted(&self) -> bool {
        matches!(self, PipeError::Squatted { .. })
    }
}

impl core::fmt::Display for PipeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PipeError::Squatted { name } => write!(f, "pipe name {name} is already owned by another process"),
            PipeError::ServerPidMismatch { expected, actual } => {
                write!(f, "pipe server is process {actual}, expected {expected}")
            }
            PipeError::Timeout => write!(f, "pipe operation timed out"),
            PipeError::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PipeError {}

impl From<PlatformError> for PipeError {
    fn from(e: PlatformError) -> Self {
        PipeError::Os(e)
    }
}

impl From<PipeError> for PlatformError {
    fn from(e: PipeError) -> Self {
        match e {
            PipeError::Os(e) => e,
            PipeError::Timeout => {
                PlatformError { kind: ErrorKind::Timeout, op: "pipe", os_code: None, detail: String::new() }
            }
            other => PlatformError { kind: ErrorKind::Os, op: "pipe", os_code: None, detail: other.to_string() },
        }
    }
}

// ---------- names ----------

/// A pipe name is the part after [`PIPE_PREFIX`]: letters, digits, `-` and `_` only, so no name can climb out of the namespace.
fn check_name(name: &str) -> Result<(), PipeError> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok { Ok(()) } else { Err(crate::sys::invalid("pipe name", format!("{name:?} is not a plain pipe name")).into()) }
}

fn full_path(name: &str) -> String {
    format!("{PIPE_PREFIX}{name}")
}

/// `ui-<session id>` (C2.1).
pub fn ui_pipe_name(session_id: u32) -> String {
    format!("ui-{session_id}")
}

/// `helper-<role>-<session id>-<128-bit random hex>` (C2.1); the hex part is [`random_hex_128`].
pub fn helper_pipe_name(role: HelperRole, session_id: u32, random_hex: &str) -> String {
    format!("helper-{}-{session_id}-{random_hex}", role.as_str())
}

/// 128 random bits from the system RNG (`BCryptGenRandom`), as 32 lowercase hex digits.
pub fn random_hex_128() -> Result<String, PlatformError> {
    let mut bytes = [0u8; 16];
    // SAFETY: a null algorithm handle with BCRYPT_USE_SYSTEM_PREFERRED_RNG is the documented form; `bytes` is 16 live bytes.
    let status =
        unsafe { BCryptGenRandom(null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status != 0 {
        return Err(os_error("BCryptGenRandom", status as u32));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

// ---------- the DACL ----------

/// Whose SID sits in the first (full control) row: SYSTEM in production, the current user in development mode
/// (`swapdesk-service --console`, C2.4), where nothing runs as SYSTEM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeOwner {
    System,
    CurrentUser,
}

/// The pipe's DACL: row one is the owner with full control, row two (when `user_sid` is set) the session's user with
/// [`USER_ROW_MASK`]. Nobody else has an entry, and the DACL is protected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeAcl {
    pub owner: PipeOwner,
    /// `S-1-5-21-...` of the user; `None` for a pipe only the owner may open (helper role system).
    pub user_sid: Option<String>,
}

impl PipeAcl {
    /// Production UI pipe or user-helper pipe: SYSTEM and that user.
    pub fn system_and_user(user_sid: impl Into<String>) -> Self {
        Self { owner: PipeOwner::System, user_sid: Some(user_sid.into()) }
    }

    /// Production system-helper pipe: SYSTEM only.
    pub fn system_only() -> Self {
        Self { owner: PipeOwner::System, user_sid: None }
    }

    /// Development mode: the current user takes SYSTEM's place; the user row is the current user too, so the shape is the same.
    pub fn dev() -> Result<Self, PlatformError> {
        Ok(Self { owner: PipeOwner::CurrentUser, user_sid: Some(current_user_sid()?) })
    }

    /// Development-mode helper pipe: the current user only.
    pub fn dev_only() -> Self {
        Self { owner: PipeOwner::CurrentUser, user_sid: None }
    }

    /// The owner row's SID.
    pub fn owner_sid(&self) -> Result<String, PlatformError> {
        match self.owner {
            PipeOwner::System => Ok(SYSTEM_SID.to_string()),
            PipeOwner::CurrentUser => current_user_sid(),
        }
    }

    /// The descriptor as SDDL: `D:P(A;;0x1f01ff;;;<owner>)(A;;0x12019b;;;<user>)`.
    pub fn to_sddl(&self) -> Result<String, PlatformError> {
        let owner = self.owner_sid()?;
        let owner = crate::sys::normalize_sid(&owner)?;
        let mut s = format!("D:P(A;;0x{OWNER_ROW_MASK:x};;;{owner})");
        if let Some(user) = &self.user_sid {
            let user = crate::sys::normalize_sid(user)?;
            s.push_str(&format!("(A;;0x{USER_ROW_MASK:x};;;{user})"));
        }
        Ok(s)
    }
}

/// A pipe to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeSpec {
    /// Name after [`PIPE_PREFIX`].
    pub name: String,
    pub acl: PipeAcl,
    /// 1 for helper pipes (one instance only, C2.1); a few for the UI pipe so a reconnect never waits for the old instance.
    pub max_instances: u32,
}

impl PipeSpec {
    /// `ui-<session id>`: several instances, so the next client can connect while the previous one is closing.
    pub fn ui(session_id: u32, acl: PipeAcl) -> Self {
        Self { name: ui_pipe_name(session_id), acl, max_instances: 4 }
    }

    /// A helper pipe: one instance, ever.
    pub fn helper(name: String, acl: PipeAcl) -> Self {
        Self { name, acl, max_instances: 1 }
    }
}

// ---------- overlapped operations ----------

enum Io {
    Done(u32),
    Failed(u32),
    TimedOut,
}

/// Runs one overlapped operation to its end. `start` is handed the `OVERLAPPED` and returns the call's BOOL.
/// On a timeout the operation is cancelled and waited for, so `OVERLAPPED` outlives the kernel's use of it.
fn run_overlapped(
    handle: HANDLE,
    event: HANDLE,
    timeout: Option<Duration>,
    start: impl FnOnce(*mut OVERLAPPED) -> i32,
) -> Io {
    // SAFETY: OVERLAPPED is plain data; zeroed is a valid initial value.
    let mut ov: OVERLAPPED = unsafe { core::mem::zeroed() };
    ov.hEvent = event;
    // SAFETY: `event` is a live manual-reset event owned by the caller, used by one operation at a time.
    unsafe { ResetEvent(event) };
    let mut timed_out = false;
    if start(&mut ov) == 0 {
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        if code != ERROR_IO_PENDING {
            return Io::Failed(code);
        }
        let wait_ms = timeout.map_or(INFINITE, |d| d.as_millis().min(u128::from(INFINITE - 1)) as u32);
        // SAFETY: `event` is a live event handle.
        if unsafe { WaitForSingleObject(event, wait_ms) } != WAIT_OBJECT_0 {
            timed_out = true;
            // SAFETY: `handle` and `ov` are the pair the operation was started with; cancelling an operation
            // that has just completed is harmless (the result below says which happened).
            unsafe { CancelIoEx(handle, &ov) };
        }
    }
    let mut n = 0u32;
    // SAFETY: `ov` is the operation's own structure; waiting (the event is signalled, or the cancel is under way)
    // guarantees the kernel is done with it when this returns.
    if unsafe { GetOverlappedResult(handle, &ov, &mut n, 1) } != 0 {
        return Io::Done(n);
    }
    // SAFETY: no preconditions.
    let code = unsafe { GetLastError() };
    if timed_out && code == ERROR_OPERATION_ABORTED { Io::TimedOut } else { Io::Failed(code) }
}

fn io_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn new_event() -> Result<OwnedHandle, PlatformError> {
    // SAFETY: a manual-reset, initially non-signalled, unnamed event with default security.
    let h = unsafe { CreateEventW(null(), 1, 0, null()) };
    if h.is_null() { Err(last_error("CreateEventW")) } else { Ok(OwnedHandle(h)) }
}

// ---------- the connected pipe ----------

struct Shared {
    handle: OwnedHandle,
}

/// One connected end of a pipe (server or client side). Implements blocking `Read` and `Write`; see the module
/// documentation for the overlapped design. `try_clone` and `split` give a second owner for another thread.
pub struct PipeConn {
    shared: Arc<Shared>,
    event: OwnedHandle,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
}

impl core::fmt::Debug for PipeConn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PipeConn").finish_non_exhaustive()
    }
}

impl PipeConn {
    fn from_handle(handle: OwnedHandle) -> Result<Self, PlatformError> {
        Ok(Self { shared: Arc::new(Shared { handle }), event: new_event()?, read_timeout: None, write_timeout: None })
    }

    fn raw(&self) -> HANDLE {
        self.shared.handle.0
    }

    /// Another owner of the same pipe handle with its own event: one thread reads on one copy while another writes on the other.
    pub fn try_clone(&self) -> Result<PipeConn, PlatformError> {
        Ok(Self {
            shared: Arc::clone(&self.shared),
            event: new_event()?,
            read_timeout: self.read_timeout,
            write_timeout: self.write_timeout,
        })
    }

    /// A read half and a write half over the same handle. The pipe closes when both are dropped.
    pub fn split(self) -> Result<(PipeReader, PipeWriter), PlatformError> {
        let writer = self.try_clone()?;
        Ok((PipeReader(self), PipeWriter(writer)))
    }

    /// Reads wait at most this long (`None`: forever); a timed-out read fails with `io::ErrorKind::TimedOut`.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    /// Writes wait at most this long (`None`: forever).
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    /// Cancels every pending read and write on the pipe, from any thread: a blocked reader returns an error
    /// (`ERROR_OPERATION_ABORTED`). This is how a shutdown unblocks the reader thread.
    pub fn cancel_io(&self) {
        // SAFETY: the handle is open (held by `self`); a null OVERLAPPED cancels all I/O the process issued on it.
        unsafe { CancelIoEx(self.raw(), null()) };
    }

    /// PID of the process that opened the client end (`GetNamedPipeClientProcessId`); server-side handles.
    pub fn client_pid(&self) -> Result<u32, PlatformError> {
        let mut pid = 0u32;
        // SAFETY: the handle is a pipe handle; `pid` is a live out-parameter.
        if unsafe { GetNamedPipeClientProcessId(self.raw(), &mut pid) } == 0 {
            return Err(last_error("GetNamedPipeClientProcessId"));
        }
        Ok(pid)
    }

    /// Session id of the client process (`ProcessIdToSessionId` of [`client_pid`](Self::client_pid)).
    pub fn client_session_id(&self) -> Result<u32, PlatformError> {
        session_of_pid(self.client_pid()?)
    }

    /// PID of the process that created the pipe server instance (`GetNamedPipeServerProcessId`).
    pub fn server_pid(&self) -> Result<u32, PlatformError> {
        let mut pid = 0u32;
        // SAFETY: the handle is a pipe handle; `pid` is a live out-parameter.
        if unsafe { GetNamedPipeServerProcessId(self.raw(), &mut pid) } == 0 {
            return Err(last_error("GetNamedPipeServerProcessId"));
        }
        Ok(pid)
    }

    /// Fails with [`PipeError::ServerPidMismatch`] unless the server is process `expected`.
    pub fn check_server_pid(&self, expected: u32) -> Result<(), PipeError> {
        let actual = self.server_pid()?;
        if actual == expected { Ok(()) } else { Err(PipeError::ServerPidMismatch { expected, actual }) }
    }

    /// The pipe's DACL read back from the kernel (needs the server handle's READ_CONTROL).
    pub fn dacl(&self) -> Result<Vec<AceInfo>, PlatformError> {
        dacl_of_kernel_handle(self.raw())
    }

    /// Server side, C2.3: impersonates the client at the identification level, reads the token's user SID,
    /// whether it is elevated, whether Administrators is an **enabled** group in it and whether Administrators is in it
    /// at all (a filtered administrator token holds it deny-only), then reverts.
    ///
    /// Call it after reading at least the client's first message: the page says `ImpersonateNamedPipeClient` uses "the
    /// last message read from the pipe". (Observed on Windows 11 10.0.26300: the call also succeeds right after connect,
    /// before any read, so a missing read is not an error to rely on.) Any error means no verdict: the caller treats the
    /// connection as unauthorized.
    pub fn identify_client(&self) -> Result<ClientIdentity, PlatformError> {
        /// Reverts on every exit path. A thread left impersonating would run the service's later work as the client,
        /// so a failed revert ends the process.
        struct Revert;
        impl Drop for Revert {
            fn drop(&mut self) {
                // SAFETY: no preconditions; reverting a thread that is not impersonating is a no-op success.
                if unsafe { RevertToSelf() } == 0 {
                    std::process::abort();
                }
            }
        }
        // SAFETY: the handle is the server end of a pipe.
        if unsafe { ImpersonateNamedPipeClient(self.raw()) } == 0 {
            return Err(last_error("ImpersonateNamedPipeClient"));
        }
        let _revert = Revert;
        let mut token: HANDLE = null_mut();
        // SAFETY: the pseudo-handle of this thread is valid; `OpenAsSelf` = TRUE opens the token with the process's own
        // rights, so the identification-level token can be queried; `token` is a live out-parameter.
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } == 0 {
            return Err(last_error("OpenThreadToken"));
        }
        let token = OwnedHandle(token);
        let user_sid = token_user_sid(token.0)?;
        let elevated = token_is_elevated(token.0)?;
        let groups = token_groups(token.0)?;
        Ok(ClientIdentity {
            user_sid,
            elevated,
            administrators_enabled: administrators_enabled(&groups),
            administrators_present: administrators_present(&groups),
        })
    }
}

/// The Administrators SID is among `groups` (`(sid, attributes)` as `token_groups` gives them) and enabled, not "deny
/// only".
pub(crate) fn administrators_enabled(groups: &[(String, u32)]) -> bool {
    groups.iter().any(|(sid, attrs)| {
        sid == ADMINISTRATORS_SID && attrs & SE_GROUP_ENABLED != 0 && attrs & SE_GROUP_USE_FOR_DENY_ONLY == 0
    })
}

/// The Administrators SID is among `groups` with any attributes: a filtered administrator token holds it deny-only.
pub(crate) fn administrators_present(groups: &[(String, u32)]) -> bool {
    groups.iter().any(|(sid, _)| sid == ADMINISTRATORS_SID)
}

/// Who a pipe client is, from its impersonated token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// `S-1-5-21-...`.
    pub user_sid: String,
    /// `TokenIsElevated`.
    pub elevated: bool,
    /// The Administrators SID is in the token and enabled (not "deny only", as in a filtered token).
    pub administrators_enabled: bool,
    /// The Administrators SID is in the token with any attributes, enabled or deny-only: the account is an administrator
    /// account, whether or not this token is the elevated one.
    pub administrators_present: bool,
}

impl ClientIdentity {
    /// C2.3: admin only if the token is elevated **and** Administrators is enabled in it.
    pub fn is_admin(&self) -> bool {
        self.elevated && self.administrators_enabled
    }
}

/// `ProcessIdToSessionId`.
pub fn session_of_pid(pid: u32) -> Result<u32, PlatformError> {
    let mut session = 0u32;
    // SAFETY: `session` is a live out-parameter.
    if unsafe { ProcessIdToSessionId(pid, &mut session) } == 0 {
        return Err(last_error("ProcessIdToSessionId"));
    }
    Ok(session)
}

impl io::Read for PipeConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(1 << 30) as u32;
        let h = self.raw();
        // SAFETY (closure): `buf` is live for the whole operation (run_overlapped waits for it to end) and `len` fits it.
        let r = run_overlapped(h, self.event.0, self.read_timeout, |ov| unsafe {
            ReadFile(h, buf.as_mut_ptr().cast(), len, null_mut(), ov)
        });
        match r {
            Io::Done(n) => Ok(n as usize),
            Io::TimedOut => Err(io::Error::new(io::ErrorKind::TimedOut, "pipe read timed out")),
            Io::Failed(c) if c == ERROR_BROKEN_PIPE || c == ERROR_PIPE_NOT_CONNECTED || c == ERROR_NO_DATA => Ok(0),
            Io::Failed(c) => Err(io_error(c)),
        }
    }
}

impl io::Write for PipeConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(1 << 30) as u32;
        let h = self.raw();
        // SAFETY (closure): `buf` is live for the whole operation and `len` fits it.
        let r = run_overlapped(h, self.event.0, self.write_timeout, |ov| unsafe {
            WriteFile(h, buf.as_ptr().cast(), len, null_mut(), ov)
        });
        match r {
            Io::Done(n) => Ok(n as usize),
            Io::TimedOut => Err(io::Error::new(io::ErrorKind::TimedOut, "pipe write timed out")),
            Io::Failed(c) if c == ERROR_BROKEN_PIPE || c == ERROR_NO_DATA || c == ERROR_PIPE_NOT_CONNECTED => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe closed by the peer"))
            }
            Io::Failed(c) => Err(io_error(c)),
        }
    }

    /// Nothing is buffered here, and `FlushFileBuffers` on a pipe would wait for the peer to drain it.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The read half of a split pipe.
#[derive(Debug)]
pub struct PipeReader(PipeConn);

/// The write half of a split pipe.
#[derive(Debug)]
pub struct PipeWriter(PipeConn);

impl PipeReader {
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.0.set_read_timeout(timeout);
    }
    /// See [`PipeConn::cancel_io`].
    pub fn cancel_io(&self) {
        self.0.cancel_io();
    }
}

impl PipeWriter {
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.0.set_write_timeout(timeout);
    }
    /// See [`PipeConn::cancel_io`].
    pub fn cancel_io(&self) {
        self.0.cancel_io();
    }
}

impl io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl io::Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------- server side ----------

/// A pipe name owned by this process, waiting for clients. Created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so
/// [`bind`](Self::bind) fails with [`PipeError::Squatted`] when the name is taken.
pub struct PipeListener {
    spec: PipeSpec,
    sddl: Vec<u16>,
    /// The instance waiting for the next client; `None` once a single-instance pipe has been accepted.
    pending: Option<OwnedHandle>,
}

impl PipeListener {
    /// Creates the first instance. `ERROR_ACCESS_DENIED` here means the name is squatted (C2.1).
    pub fn bind(spec: PipeSpec) -> Result<Self, PipeError> {
        check_name(&spec.name)?;
        if spec.max_instances == 0 || spec.max_instances > 254 {
            return Err(crate::sys::invalid("PipeSpec", "max_instances must be 1..=254").into());
        }
        let sddl = wide(OsStr::new(&spec.acl.to_sddl()?))?;
        let first = create_instance(&spec, &sddl, true)?;
        Ok(Self { spec, sddl, pending: Some(first) })
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    /// The DACL of the pending instance, read back (for the tests and the service's audit).
    pub fn dacl(&self) -> Result<Vec<AceInfo>, PlatformError> {
        match &self.pending {
            Some(h) => dacl_of_kernel_handle(h.0),
            None => Err(crate::sys::invalid("PipeListener::dacl", "no pending instance")),
        }
    }

    /// Waits for a client. `Err(PipeError::Timeout)` leaves the listener usable. For a multi-instance pipe the next
    /// instance is created before this returns, so the name is never unowned between clients; a single-instance
    /// pipe accepts once and then reports `Closed`.
    pub fn accept(&mut self, timeout: Option<Duration>) -> Result<PipeConn, PipeError> {
        let Some(instance) = self.pending.take() else {
            return Err(PlatformError {
                kind: ErrorKind::Closed,
                op: "PipeListener::accept",
                os_code: None,
                detail: "single-instance pipe already accepted".into(),
            }
            .into());
        };
        let event = new_event()?;
        let h = instance.0;
        let r = run_overlapped(h, event.0, timeout, |ov| {
            // SAFETY: `h` is a listening pipe instance opened overlapped; `ov` is the operation's structure.
            unsafe { ConnectNamedPipe(h, ov) }
        });
        match r {
            Io::Done(_) => {}
            // The client connected between the create and this call.
            Io::Failed(c) if c == ERROR_PIPE_CONNECTED => {}
            Io::TimedOut => {
                self.pending = Some(instance);
                return Err(PipeError::Timeout);
            }
            Io::Failed(c) => {
                self.pending = Some(instance);
                return Err(os_error("ConnectNamedPipe", c).into());
            }
        }
        if self.spec.max_instances > 1 {
            self.pending = Some(create_instance(&self.spec, &self.sddl, false)?);
        }
        Ok(PipeConn::from_handle(instance)?)
    }
}

fn create_instance(spec: &PipeSpec, sddl: &[u16], first: bool) -> Result<OwnedHandle, PipeError> {
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `sddl` is a null-terminated wide string; the descriptor is freed by LocalMem after the create call.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
    } == 0
    {
        return Err(last_error("ConvertStringSecurityDescriptorToSecurityDescriptorW").into());
    }
    let _sd_mem = LocalMem(sd);
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let path = wide(OsStr::new(&full_path(&spec.name)))?;
    let open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    // SAFETY: `path` is a null-terminated wide string; `sa` and the descriptor it points to are live for the call.
    let h = unsafe {
        CreateNamedPipeW(path.as_ptr(), open_mode, pipe_mode, spec.max_instances, BUFFER_BYTES, BUFFER_BYTES, 0, &sa)
    };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        if first && code == ERROR_ACCESS_DENIED {
            return Err(PipeError::Squatted { name: spec.name.clone() });
        }
        return Err(os_error("CreateNamedPipeW", code).into());
    }
    Ok(OwnedHandle(h))
}

// ---------- client side ----------

/// Client access mask: `FILE_GENERIC_READ | FILE_GENERIC_WRITE` without `FILE_APPEND_DATA` (equal to [`USER_ROW_MASK`]).
const CLIENT_ACCESS: u32 = USER_ROW_MASK;

/// Opens `\\.\pipe\swapdesk\<name>` as a client with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, then checks
/// that the server process is `expected_server_pid` (what the SCM reported for the service, or the service's PID
/// for a helper). Nothing is written before the check passes. A busy pipe is waited for up to `timeout`; a missing pipe
/// fails at once with `ErrorKind::Unavailable` (the server is not up).
pub fn connect(name: &str, expected_server_pid: u32, timeout: Duration) -> Result<PipeConn, PipeError> {
    check_name(name)?;
    let path = wide(OsStr::new(&full_path(name)))?;
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: `path` is a null-terminated wide string; no security attributes, no template file.
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                CLIENT_ACCESS,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            let conn = PipeConn::from_handle(OwnedHandle(h))?;
            conn.check_server_pid(expected_server_pid)?;
            return Ok(conn);
        }
        // SAFETY: no preconditions.
        let code = unsafe { GetLastError() };
        if code == ERROR_FILE_NOT_FOUND {
            return Err(PipeError::Os(PlatformError {
                kind: ErrorKind::Unavailable,
                op: "CreateFileW",
                os_code: Some(i64::from(code)),
                detail: "pipe does not exist".into(),
            }));
        }
        if code != ERROR_PIPE_BUSY {
            return Err(os_error("CreateFileW", code).into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(PipeError::Timeout);
        }
        // SAFETY: `path` is a null-terminated wide string. Failure (timeout) is retried against the deadline above.
        unsafe { WaitNamedPipeW(path.as_ptr(), left.as_millis().min(u128::from(u32::MAX - 1)) as u32) };
    }
}
