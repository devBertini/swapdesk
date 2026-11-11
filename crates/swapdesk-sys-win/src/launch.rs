//! Helper launch (C2.1): the production path starts `swapdesk-helper.exe` in the console session with the right
//! token; the development path (`swapdesk-service --console`, C2.4) starts it as the current user.
//!
//! The production path needs the service's own SYSTEM token (`SE_TCB_NAME` for `WTSQueryUserToken`, the
//! right to set a token's session id) and so cannot run unelevated: it is compiled and reviewed here, and
//! exercised only when the service is installed (the owner's UAC click).
//!
//! UNPROVEN, settled when the service runs: which token `WTSQueryUserToken` returns for an administrator (C2.1).
//! [`launch_helper`] asks the token (`TokenElevationType`) and takes the linked limited token when it got the full one;
//! [`ChildProcess::user_token_elevation_type`] reports what it saw so the service can log the fact.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::Duration;

use swapdesk_platform::{ErrorKind, PlatformError};
use windows_sys::Win32::Foundation::{HANDLE, STILL_ACTIVE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_LINKED_TOKEN, TOKEN_QUERY, TokenLinkedToken, TokenPrimary,
    TokenSessionId,
};
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW, GetExitCodeProcess,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

use crate::acl::{ACCESS_ALLOWED_ACE_TYPE, INHERIT_ONLY_ACE, security_of_path};
use crate::env::EnvBlock;
use crate::privileges::restrict_system_helper_token;
use crate::sys::{
    OwnedHandle, Result, invalid, last_error, normalize_sid, open_process_token, token_elevation_type, token_info,
    token_is_elevated, token_user_sid, wide,
};

const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
/// `TokenElevationType` values.
const ELEVATION_DEFAULT: u32 = 1;
const ELEVATION_FULL: u32 = 2;
const ELEVATION_LIMITED: u32 = 3;
/// `WTSGetActiveConsoleSessionId` when no session is attached to the console.
pub const NO_CONSOLE_SESSION: u32 = 0xFFFF_FFFF;
/// The desktop every helper starts on (windows.md 5d recipe).
const HELPER_DESKTOP: &str = r"winsta0\default";

/// Which helper (C2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelperRole {
    /// SYSTEM token moved to the console session.
    System,
    /// The logged-on user's limited token.
    User,
}

impl HelperRole {
    /// The word in `--role <x>` and in the helper pipe name.
    pub fn as_str(self) -> &'static str {
        match self {
            HelperRole::System => "system",
            HelperRole::User => "user",
        }
    }
}

// ---------- restart policy ----------

/// Restart delays 1, 2, 4, 8, 16, 30, 30 ... seconds (C2.1). Pure: the caller sleeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartBackoff {
    failures: u32,
}

impl RestartBackoff {
    pub const FIRST: Duration = Duration::from_secs(1);
    pub const MAX: Duration = Duration::from_secs(30);
    /// A helper that ran this long counts as healthy: the next failure starts again at 1 s. (A choice, not in
    /// C2.1: without it one crash a day would one day wait the full 30 s.)
    pub const STABLE_AFTER: Duration = Duration::from_secs(60);

    pub fn new() -> Self {
        Self { failures: 0 }
    }

    /// The delay before the next start, and counts one more failure.
    pub fn next_delay(&mut self) -> Duration {
        let secs = 1u64.checked_shl(self.failures).unwrap_or(u64::MAX);
        self.failures = self.failures.saturating_add(1).min(63);
        Duration::from_secs(secs).min(Self::MAX)
    }

    /// The helper ran fine for a while (or a fresh console session started): forget the failures.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// The helper exited after running `ran_for`: resets first when that was long enough, then returns the next delay.
    pub fn after_exit(&mut self, ran_for: Duration) -> Duration {
        if ran_for >= Self::STABLE_AFTER {
            self.reset();
        }
        self.next_delay()
    }
}

impl Default for RestartBackoff {
    fn default() -> Self {
        Self::new()
    }
}

// ---------- command line ----------

/// One argument quoted for `CommandLineToArgvW` / the C runtime.
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// `"<exe>" arg arg ...`.
pub fn build_command_line(exe: &Path, args: &[String]) -> String {
    let mut line = quote_arg(&exe.to_string_lossy());
    for a in args {
        line.push(' ');
        line.push_str(&quote_arg(a));
    }
    line
}

// ---------- the child ----------

/// A started process; its handle closes on drop (the process keeps running: call [`kill`](Self::kill) to end it).
pub struct ChildProcess {
    process: OwnedHandle,
    pid: u32,
    /// `TokenElevationType` of the token `WTSQueryUserToken` returned (role user, production path only).
    user_token_elevation_type: Option<u32>,
}

impl core::fmt::Debug for ChildProcess {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ChildProcess").field("pid", &self.pid).finish_non_exhaustive()
    }
}

impl ChildProcess {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// What `WTSQueryUserToken` handed over (1 default, 2 full, 3 limited), for role user in production; `None` otherwise.
    pub fn user_token_elevation_type(&self) -> Option<u32> {
        self.user_token_elevation_type
    }

    /// The exit code once the process has ended, `None` while it runs.
    pub fn try_wait(&self) -> Result<Option<u32>> {
        let mut code = 0u32;
        // SAFETY: the process handle has PROCESS_QUERY_LIMITED_INFORMATION (CreateProcess gives full access); `code` is live.
        if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        // A process that really exits with 259 is read as running until the handle's wait says otherwise.
        if code == STILL_ACTIVE as u32 && !self.wait_signalled(Duration::ZERO)? {
            return Ok(None);
        }
        Ok(Some(code))
    }

    /// Waits up to `timeout` for the process to end; its exit code, or `None` on timeout.
    pub fn wait(&self, timeout: Duration) -> Result<Option<u32>> {
        if !self.wait_signalled(timeout)? {
            return Ok(None);
        }
        self.try_wait()
    }

    fn wait_signalled(&self, timeout: Duration) -> Result<bool> {
        // SAFETY: the process handle is open.
        match unsafe { WaitForSingleObject(self.process.0, timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32) } {
            WAIT_OBJECT_0 => Ok(true),
            0x102 /* WAIT_TIMEOUT */ => Ok(false),
            _ => Err(last_error("WaitForSingleObject")),
        }
    }

    /// Ends the process (exit code 1).
    pub fn kill(&self) -> Result<()> {
        // SAFETY: the process handle has PROCESS_TERMINATE (CreateProcess gives full access).
        if unsafe { TerminateProcess(self.process.0, 1) } == 0 { Err(last_error("TerminateProcess")) } else { Ok(()) }
    }
}

fn from_process_information(pi: &PROCESS_INFORMATION, elevation: Option<u32>) -> ChildProcess {
    drop(OwnedHandle(pi.hThread));
    ChildProcess { process: OwnedHandle(pi.hProcess), pid: pi.dwProcessId, user_token_elevation_type: elevation }
}

// ---------- development path ----------

/// Development mode: `CreateProcessW` as the current user, on the current desktop, no console window. The
/// `--role system` helper started this way has no secure-desktop reach (C2.4).
pub fn launch_helper_dev(exe: &Path, args: &[String]) -> Result<ChildProcess> {
    let app = wide(exe.as_os_str())?;
    let mut cmd = wide(OsStr::new(&build_command_line(exe, args)))?;
    // SAFETY: STARTUPINFOW and PROCESS_INFORMATION are plain data; zeroed is valid, `cb` is set below.
    let mut si: STARTUPINFOW = unsafe { core::mem::zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    // SAFETY: as above.
    let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: `app` and `cmd` are null-terminated wide strings (`cmd` is writable, as the call requires); `si` and `pi` are live.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            null(),
            null(),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(last_error("CreateProcessW"));
    }
    Ok(from_process_information(&pi, None))
}

// ---------- production path ----------

/// The active console session, `None` when nobody is attached to the console (`0xFFFFFFFF`; C2.1 retries every 500 ms).
pub fn active_console_session() -> Option<u32> {
    // SAFETY: no preconditions.
    let id = unsafe { WTSGetActiveConsoleSessionId() };
    if id == NO_CONSOLE_SESSION { None } else { Some(id) }
}

/// What to do with the token `WTSQueryUserToken` returned, from its `TokenElevationType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTokenChoice {
    /// Default (no split token) or already limited: use it.
    UseAsIs,
    /// Full (elevated): take the linked limited token instead.
    UseLinked,
    /// A value this code does not know.
    Refuse,
}

pub fn choose_user_token(elevation_type: u32) -> UserTokenChoice {
    match elevation_type {
        ELEVATION_DEFAULT | ELEVATION_LIMITED => UserTokenChoice::UseAsIs,
        ELEVATION_FULL => UserTokenChoice::UseLinked,
        _ => UserTokenChoice::Refuse,
    }
}

fn duplicate_primary(token: HANDLE) -> Result<OwnedHandle> {
    let mut out: HANDLE = null_mut();
    // SAFETY: `token` is a token handle with TOKEN_DUPLICATE; `out` is a live out-parameter.
    if unsafe { DuplicateTokenEx(token, MAXIMUM_ALLOWED, null(), SecurityImpersonation, TokenPrimary, &mut out) } == 0 {
        return Err(last_error("DuplicateTokenEx"));
    }
    Ok(OwnedHandle(out))
}

/// The SID (`S-1-5-...`) of the user logged on in `session_id`, for the DACL of that session's UI pipe and user
/// helper pipe (C2.1). Production only: `WTSQueryUserToken` needs the SYSTEM service's `SE_TCB_NAME`, and fails with
/// `ERROR_NO_TOKEN` when nobody is logged on in that session.
pub fn session_user_sid(session_id: u32) -> Result<String> {
    let mut raw: HANDLE = null_mut();
    // SAFETY: `raw` is a live out-parameter; the call refuses without SE_TCB_NAME.
    if unsafe { WTSQueryUserToken(session_id, &mut raw) } == 0 {
        return Err(last_error("WTSQueryUserToken"));
    }
    let token = OwnedHandle(raw);
    token_user_sid(token.0)
}

/// The logged-on user's limited primary token for `session_id`, and the elevation type of what the OS first gave.
pub(crate) fn user_limited_token(session_id: u32) -> Result<(OwnedHandle, u32)> {
    let mut raw: HANDLE = null_mut();
    // SAFETY: the caller is the SYSTEM service holding SE_TCB_NAME (the call refuses otherwise); `raw` is a live out-parameter.
    if unsafe { WTSQueryUserToken(session_id, &mut raw) } == 0 {
        return Err(last_error("WTSQueryUserToken"));
    }
    let queried = OwnedHandle(raw);
    let seen = token_elevation_type(queried.0)?;
    let primary = match choose_user_token(seen) {
        UserTokenChoice::UseAsIs => duplicate_primary(queried.0)?,
        UserTokenChoice::UseLinked => {
            let info = token_info(queried.0, TokenLinkedToken)?;
            // SAFETY: the buffer holds a TOKEN_LINKED_TOKEN written by GetTokenInformation, 8-byte aligned.
            let linked = OwnedHandle(unsafe { &*info.as_ptr().cast::<TOKEN_LINKED_TOKEN>() }.LinkedToken);
            duplicate_primary(linked.0)?
        }
        UserTokenChoice::Refuse => return Err(invalid("launch_helper", format!("unknown TokenElevationType {seen}"))),
    };
    // The helper must run at medium integrity (D11): a token that is still elevated is never used.
    if token_is_elevated(primary.0)? {
        return Err(PlatformError {
            kind: ErrorKind::Unsupported,
            op: "launch_helper",
            os_code: None,
            detail: "the user's token is elevated and has no limited token (UAC off?); refusing to run the user helper elevated".into(),
        });
    }
    Ok((primary, seen))
}

/// The service's own token as a primary token moved to `session_id`, with every privilege the system helper does not
/// need deleted (B5, [`crate::privileges::SYSTEM_HELPER_KEPT_PRIVILEGES`]). Order: the session id is set first, on the
/// full duplicate (that call needs `SeTcbPrivilege` in the caller, which is the service, and TOKEN_ADJUST_SESSIONID on
/// the handle), then the privileges are deleted from a copy, which is checked to still be in `session_id`.
fn system_token_in_session(session_id: u32) -> Result<OwnedHandle> {
    let own = open_process_token(
        TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT | TOKEN_ADJUST_SESSIONID,
    )?;
    let dup = duplicate_primary(own.0)?;
    // SAFETY: `dup` has TOKEN_ADJUST_SESSIONID (MAXIMUM_ALLOWED on a duplicate of such a token); the value is a u32 of the stated size.
    let ok =
        unsafe { SetTokenInformation(dup.0, TokenSessionId, (&raw const session_id).cast(), size_of::<u32>() as u32) };
    if ok == 0 {
        return Err(last_error("SetTokenInformation(TokenSessionId)"));
    }
    restrict_system_helper_token(dup.0, session_id)
}

/// An executable that passed [`InstallPolicy::verify`]; production launch takes only this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedExe(PathBuf);

impl VerifiedExe {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Production launch: the helper starts in console session `session_id` on `winsta0\default`, with the SYSTEM token
/// moved to that session (role system) or the user's limited token (role user), and that token's environment.
/// The caller got `session_id` from [`active_console_session`] (it also names the helper pipe) and has checked the exe.
pub fn launch_helper(role: HelperRole, exe: &VerifiedExe, args: &[String], session_id: u32) -> Result<ChildProcess> {
    let (token, elevation) = match role {
        HelperRole::System => (system_token_in_session(session_id)?, None),
        HelperRole::User => {
            let (t, seen) = user_limited_token(session_id)?;
            (t, Some(seen))
        }
    };
    let env = EnvBlock::for_token(token.0)?;
    let app = wide(exe.path().as_os_str())?;
    let mut cmd = wide(OsStr::new(&build_command_line(exe.path(), args)))?;
    let mut desktop = wide(OsStr::new(HELPER_DESKTOP))?;
    let dir = exe.path().parent().map(|p| wide(p.as_os_str())).transpose()?;
    // SAFETY: plain data; zeroed is valid.
    let mut si: STARTUPINFOW = unsafe { core::mem::zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desktop.as_mut_ptr();
    // SAFETY: plain data.
    let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    // SAFETY: `token` is a primary token for the target session; `app`, `cmd` (writable), `desktop`, `dir` are null-terminated
    // wide strings that outlive the call; `env` is a Unicode environment block (CREATE_UNICODE_ENVIRONMENT); `si`, `pi` are live.
    let ok = unsafe {
        CreateProcessAsUserW(
            token.0,
            app.as_ptr(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            env.as_ptr(),
            dir.as_ref().map_or(null(), |d| d.as_ptr()),
            &si,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(last_error("CreateProcessAsUserW"));
    }
    Ok(from_process_information(&pi, elevation))
}

// ---------- install location (F-SYS-01) ----------

/// The TrustedInstaller service SID.
pub const TRUSTED_INSTALLER_SID: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// Access bits that let a principal change the object or its contents: write data / add file, append / add subfolder,
/// write EA, delete child, write attributes, delete, write DAC, write owner, generic write, generic all, maximum allowed.
const WRITE_BITS: u32 =
    0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x1_0000 | 0x4_0000 | 0x8_0000 | 0x4000_0000 | 0x1000_0000 | MAXIMUM_ALLOWED;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    /// The object grants write-like access to a principal that is not trusted.
    Writable {
        path: PathBuf,
        sid: String,
        mask: u32,
    },
    /// The object's owner can rewrite its DACL.
    OwnerNotTrusted {
        path: PathBuf,
        owner: String,
    },
    /// An allow entry of a type this check does not parse; refused rather than guessed.
    UnsupportedAce {
        path: PathBuf,
        ace_type: u8,
    },
    /// The exe is not under the install root.
    OutsideRoot {
        path: PathBuf,
    },
    /// The exe is not a regular file.
    NotAFile {
        path: PathBuf,
    },
    Os(PlatformError),
}

impl core::fmt::Display for InstallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InstallError::Writable { path, sid, mask } => {
                write!(f, "{} grants write access (0x{mask:x}) to {sid}", path.display())
            }
            InstallError::OwnerNotTrusted { path, owner } => write!(f, "{} is owned by {owner}", path.display()),
            InstallError::UnsupportedAce { path, ace_type } => write!(
                f,
                "{} has an allow entry of type {ace_type} that the install check does not parse",
                path.display()
            ),
            InstallError::OutsideRoot { path } => write!(f, "{} is outside the install root", path.display()),
            InstallError::NotAFile { path } => write!(f, "{} is not a regular file", path.display()),
            InstallError::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<PlatformError> for InstallError {
    fn from(e: PlatformError) -> Self {
        InstallError::Os(e)
    }
}

/// Fails unless nobody outside `trusted_sids` can write to `path` (a file or folder): no allow entry that applies to
/// the object itself grants a write-like bit to another SID, and the owner (who can always rewrite the DACL) is trusted.
/// Deny entries are ignored (the check is stricter without them); inherit-only entries do not apply to the object.
pub fn check_no_other_writers(path: &Path, trusted_sids: &[String]) -> core::result::Result<(), InstallError> {
    let trusted: Vec<String> = trusted_sids.iter().map(|s| normalize_sid(s)).collect::<Result<_>>()?;
    let (owner, aces) = security_of_path(path)?;
    if !trusted.contains(&owner) {
        return Err(InstallError::OwnerNotTrusted { path: path.to_path_buf(), owner });
    }
    for ace in aces {
        if ace.flags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match (ace.ace_type, ace.sid) {
            (ACCESS_ALLOWED_ACE_TYPE, Some(sid)) => {
                if ace.mask & WRITE_BITS != 0 && !trusted.contains(&sid) {
                    return Err(InstallError::Writable { path: path.to_path_buf(), sid, mask: ace.mask });
                }
            }
            // ACCESS_ALLOWED_COMPOUND, _OBJECT, _CALLBACK, _CALLBACK_OBJECT.
            (t @ (4 | 5 | 9 | 11), _) => {
                return Err(InstallError::UnsupportedAce { path: path.to_path_buf(), ace_type: t });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Where helpers may be started from and who may hold write access there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPolicy {
    /// The exe must be inside this folder (after links are resolved).
    pub root: PathBuf,
    pub trusted_sids: Vec<String>,
}

/// The Program Files folder as the system knows it (`SHGetKnownFolderPath(FOLDERID_ProgramFiles)`).
pub fn program_files() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, SHGetKnownFolderPath};
    let mut path: windows_sys::core::PWSTR = null_mut();
    // SAFETY: the GUID is a static of windows-sys; `path` receives a buffer the call allocates, freed below with
    // `CoTaskMemFree` whatever the result (the documented contract, also on failure).
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, 0, null_mut(), &mut path) };
    let out = if hr == 0 && !path.is_null() {
        // SAFETY: on success `path` is a NUL-terminated UTF-16 string owned until the free.
        let len = (0..).take_while(|&i| unsafe { *path.add(i) } != 0).count();
        // SAFETY: `len` units were just read from the same live buffer.
        let wide = unsafe { std::slice::from_raw_parts(path, len) };
        Ok(PathBuf::from(std::ffi::OsString::from_wide(wide)))
    } else {
        Err(invalid("InstallPolicy::production", format!("SHGetKnownFolderPath failed: {hr:#x}")))
    };
    // SAFETY: the buffer came from `SHGetKnownFolderPath`; null is allowed.
    unsafe { CoTaskMemFree(path.cast()) };
    out
}

impl InstallPolicy {
    /// C2.1: under the Program Files folder, writable only by SYSTEM, Administrators and TrustedInstaller. The
    /// folder comes from the system (`FOLDERID_ProgramFiles`), not from `%ProgramFiles%`, which a process of the same
    /// user could set for a program it starts (review of the elevation, finding 7).
    pub fn production() -> Result<Self> {
        Ok(Self {
            root: program_files()?,
            trusted_sids: vec!["S-1-5-18".into(), "S-1-5-32-544".into(), TRUSTED_INSTALLER_SID.into()],
        })
    }

    /// Resolves links, requires the exe to be a regular file under the root, and checks the DACL and owner of the exe and of its folder.
    /// (The check and the launch are two steps; the trusted principals are the only ones that could swap the file between them.)
    pub fn verify(&self, exe: &Path) -> core::result::Result<VerifiedExe, InstallError> {
        let io_err = |e: std::io::Error| {
            InstallError::Os(PlatformError {
                kind: ErrorKind::Os,
                op: "canonicalize",
                os_code: e.raw_os_error().map(i64::from),
                detail: e.to_string(),
            })
        };
        let exe = std::fs::canonicalize(exe).map_err(io_err)?;
        let root = std::fs::canonicalize(&self.root).map_err(io_err)?;
        if !exe.starts_with(&root) {
            return Err(InstallError::OutsideRoot { path: exe });
        }
        if !std::fs::metadata(&exe).map_err(io_err)?.is_file() {
            return Err(InstallError::NotAFile { path: exe });
        }
        let folder = exe.parent().ok_or_else(|| InstallError::OutsideRoot { path: exe.clone() })?;
        check_no_other_writers(folder, &self.trusted_sids)?;
        check_no_other_writers(&exe, &self.trusted_sids)?;
        Ok(VerifiedExe(exe))
    }
}
