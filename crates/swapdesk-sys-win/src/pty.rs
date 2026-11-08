//! The ConPTY `PtySpawner` (C3, D14: own code): a shell in a pseudoconsole as the logged-in user, never SYSTEM
//! (F-TERM-01), or, from the service only, a SYSTEM shell in session 0 (F-TERM-02).
//!
//! Which token starts the shell is decided before anything is created, by [`check_spawn`] (pure, unit-tested):
//!
//! * a user terminal needs a token whose user is not a service account (SYSTEM, LocalService, NetworkService) and, when
//!   the caller told the session user, is exactly that user;
//! * an administrator terminal needs the calling process to be SYSTEM in session 0, so the shell can never appear on a
//!   user's desktop.
//!
//! Ways to start a user shell: [`ConPtySpawner::current_user`] (the user helper, and the tests: the process's own token,
//! `CreateProcessW`) and [`ConPtySpawner::console_user`] (the service: the console user's limited token from
//! `WTSQueryUserToken`, `CreateProcessAsUserW` with that token's environment).
//!
//! Exit and end of output: the pseudoconsole's output pipe only breaks when the console is closed, so after the shell
//! exits `try_wait` waits a short grace (the last output is still on its way), closes the pseudoconsole on a helper
//! thread (`ClosePseudoConsole` can wait for the output to be drained on older Windows) and only then reports the exit
//! code; the reader sees the end of the stream after that. Shells and what they start live in a job object that kills
//! them when the terminal is dropped.

use std::ffi::{OsStr, c_void};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use swapdesk_platform::{ErrorKind, OsGrant, PlatformError, Pty, PtyRequest, PtySpawner};
use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Security::{LookupAccountSidW, TOKEN_QUERY, TOKEN_USER, TokenSessionId, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::GetUserProfileDirectoryW;

use crate::env::EnvBlock;
use crate::launch::user_limited_token;
use crate::sys::{OwnedHandle, Result, invalid, last_error, open_process_token, token_info, token_user_sid, wide};

/// The SID of the LocalSystem account.
pub const SID_SYSTEM: &str = "S-1-5-18";
const SERVICE_SIDS: [&str; 3] = ["S-1-5-18", "S-1-5-19", "S-1-5-20"];
/// How long after the shell exited its last output may still be in the pipe, before the pseudoconsole is closed.
const EXIT_GRACE: Duration = Duration::from_millis(150);

// ---------- the guard ----------

/// What the guard reads from a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenFacts {
    /// `S-1-5-...` of the token's user.
    pub user_sid: String,
    /// The Windows session the token belongs to (0 for the service).
    pub session_id: u32,
}

/// Why a spawn was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A user terminal with a SYSTEM, LocalService or NetworkService token.
    ServiceAccountForUser,
    /// A user terminal whose token is not the user the caller said is logged in.
    NotSessionUser,
    /// An administrator terminal from a process that is not SYSTEM.
    AdminNeedsSystem,
    /// An administrator terminal from a process on a user's session (it would run on the user's desktop).
    AdminOnUserSession,
}

impl Refusal {
    fn detail(&self) -> &'static str {
        match self {
            Refusal::ServiceAccountForUser => "a user terminal never runs as a service account",
            Refusal::NotSessionUser => "the token is not the logged-in user's",
            Refusal::AdminNeedsSystem => "the administrator terminal needs a SYSTEM process",
            Refusal::AdminOnUserSession => "the administrator terminal runs in session 0 only",
        }
    }

    fn into_error(self) -> PlatformError {
        let kind = match self {
            Refusal::AdminNeedsSystem | Refusal::AdminOnUserSession => {
                ErrorKind::PermissionDenied(OsGrant::Administrator)
            }
            Refusal::ServiceAccountForUser | Refusal::NotSessionUser => ErrorKind::Unsupported,
        };
        PlatformError { kind, op: "ConPtySpawner.spawn", os_code: None, detail: self.detail().into() }
    }
}

/// The rule of this module: may a shell be started with `token` for this request?
/// `session_user` is the SID of the user logged in at the console, when the caller knows it.
pub fn check_spawn(admin: bool, token: &TokenFacts, session_user: Option<&str>) -> core::result::Result<(), Refusal> {
    if admin {
        if token.user_sid != SID_SYSTEM {
            return Err(Refusal::AdminNeedsSystem);
        }
        if token.session_id != 0 {
            return Err(Refusal::AdminOnUserSession);
        }
        return Ok(());
    }
    if SERVICE_SIDS.contains(&token.user_sid.as_str()) {
        return Err(Refusal::ServiceAccountForUser);
    }
    match session_user {
        Some(expected) if expected != token.user_sid => Err(Refusal::NotSessionUser),
        _ => Ok(()),
    }
}

// ---------- the spawner ----------

/// The command line of the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    command_line: String,
}

impl Shell {
    /// An exact command line, e.g. `cmd.exe /k`.
    pub fn command(command_line: impl Into<String>) -> Shell {
        Shell { command_line: command_line.into() }
    }

    /// Windows PowerShell when it is installed, else the command interpreter (`%ComSpec%`).
    pub fn user_default() -> Shell {
        if let Some(root) = std::env::var_os("SystemRoot") {
            let ps = PathBuf::from(root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
            if ps.is_file() {
                return Shell { command_line: format!("\"{}\" -NoLogo", ps.display()) };
            }
        }
        Shell { command_line: std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into()) }
    }

    pub fn command_line(&self) -> &str {
        &self.command_line
    }
}

#[derive(Debug, Clone)]
enum Who {
    /// This process's own token.
    CurrentProcess,
    /// The limited token of the user logged in at this console session (needs the service's privileges).
    ConsoleUser { session_id: u32 },
}

pub struct ConPtySpawner {
    shell: Shell,
    who: Who,
    session_user: Option<String>,
}

impl ConPtySpawner {
    /// Spawns as the calling process: the user helper (a user token) or, for `admin` requests, the service (SYSTEM,
    /// session 0). `session_user` is the SID of the user logged in at the console when known; a user terminal is refused
    /// when this process's token is a different one.
    pub fn current_user(shell: Shell, session_user: Option<String>) -> ConPtySpawner {
        ConPtySpawner { shell, who: Who::CurrentProcess, session_user }
    }

    /// The service's spawner for the user terminal: each spawn takes the console user's limited primary token for
    /// `session_id` (`WTSQueryUserToken`) and starts the shell with `CreateProcessAsUserW`. Administrator requests use
    /// the service's own token. Create a new spawner when the console session changes.
    pub fn console_user(shell: Shell, session_id: u32) -> ConPtySpawner {
        ConPtySpawner { shell, who: Who::ConsoleUser { session_id }, session_user: None }
    }

    /// Spawns and keeps the concrete type (the tests read the child's token through it).
    pub fn spawn_conpty(&self, request: &PtyRequest) -> Result<ConPty> {
        if request.cols == 0 || request.rows == 0 {
            return Err(invalid("ConPtySpawner.spawn", "empty size"));
        }
        // The token that starts the shell.
        let (holder, via_user_token) = match self.who {
            Who::ConsoleUser { session_id } if !request.admin => (console_user_token(session_id)?, true),
            _ => (open_process_token(TOKEN_QUERY)?, false),
        };
        let token = holder.0;
        let facts = TokenFacts { user_sid: token_user_sid(token)?, session_id: token_session_id(token)? };
        check_spawn(request.admin, &facts, self.session_user.as_deref()).map_err(Refusal::into_error)?;
        let account = account_of(token)?;
        let cwd = if request.admin { None } else { profile_dir(token, via_user_token) };
        start(&self.shell, request, if via_user_token { Some(token) } else { None }, cwd, account)
    }
}

impl PtySpawner for ConPtySpawner {
    fn spawn(&self, request: &PtyRequest) -> Result<Box<dyn Pty>> {
        Ok(Box::new(self.spawn_conpty(request)?))
    }
}

// ---------- tokens ----------

fn token_session_id(token: HANDLE) -> Result<u32> {
    let info = token_info(token, TokenSessionId)?;
    // SAFETY: the buffer holds a DWORD written by GetTokenInformation(TokenSessionId), 8-byte aligned.
    Ok(unsafe { *info.as_ptr().cast::<u32>() })
}

/// The user name ("claudio", "SYSTEM") of a token's user.
fn account_of(token: HANDLE) -> Result<String> {
    let info = token_info(token, TokenUser)?;
    // SAFETY: the buffer holds a TOKEN_USER written by the call above, 8-byte aligned.
    let user = unsafe { &*info.as_ptr().cast::<TOKEN_USER>() };
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut name_len, mut domain_len, mut kind) = (256u32, 256u32, 0i32);
    // SAFETY: the SID is valid for the buffer's life; both buffers hold the lengths passed; null system name = this machine.
    let ok = unsafe {
        LookupAccountSidW(
            null(),
            user.User.Sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if ok == 0 {
        return Err(last_error("LookupAccountSidW"));
    }
    Ok(String::from_utf16_lossy(&name[..name_len as usize]))
}

/// The console user's limited (medium integrity) primary token, as the user helper gets it (D11): the same choice
/// and the same refusal of a still-elevated token (UAC off) as `launch_helper`.
fn console_user_token(session_id: u32) -> Result<OwnedHandle> {
    user_limited_token(session_id).map(|(token, _)| token)
}

/// The user's profile folder, the shell's starting directory.
fn profile_dir(token: HANDLE, from_token: bool) -> Option<Vec<u16>> {
    if !from_token {
        let p = std::env::var_os("USERPROFILE")?;
        return wide(&p).ok();
    }
    let mut len = 0u32;
    // SAFETY: a size query with a null buffer; `len` is a live out-parameter.
    unsafe { GetUserProfileDirectoryW(token, null_mut(), &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` UTF-16 units, the size the first call asked for.
    (unsafe { GetUserProfileDirectoryW(token, buf.as_mut_ptr(), &mut len) } != 0).then_some(buf)
}

// ---------- starting the shell ----------

/// The desktop the shell starts on (the same as the helpers', see `launch::HELPER_DESKTOP`). Without it the child
/// inherits the caller's desktop, which for the service is its non-interactive service window station.
const SHELL_DESKTOP: &str = r"winsta0\default";

struct AttrList {
    mem: Vec<u64>,
}

impl AttrList {
    fn pseudoconsole(hpc: HPCON) -> Result<AttrList> {
        let mut size = 0usize;
        // SAFETY: a size query with a null list is the documented first call; it fails with ERROR_INSUFFICIENT_BUFFER.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size) };
        if size == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        let mut list = AttrList { mem: vec![0u64; size.div_ceil(8)] };
        // SAFETY: the buffer holds `size` bytes, 8-byte aligned as the list requires.
        if unsafe { InitializeProcThreadAttributeList(list.ptr(), 1, 0, &mut size) } == 0 {
            return Err(last_error("InitializeProcThreadAttributeList"));
        }
        // SAFETY: the list was initialised for one attribute; PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE takes the HPCON value itself
        // (not a pointer to it) with its size; the pseudoconsole outlives the CreateProcess call.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list.ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const c_void,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            )
        };
        if ok == 0 {
            let e = last_error("UpdateProcThreadAttribute");
            // SAFETY: initialised above and deleted once (Drop runs on `list`, so delete here and forget the buffer state).
            unsafe { DeleteProcThreadAttributeList(list.ptr()) };
            list.mem.clear();
            return Err(e);
        }
        Ok(list)
    }

    fn ptr(&mut self) -> *mut c_void {
        self.mem.as_mut_ptr().cast()
    }
}

impl Drop for AttrList {
    fn drop(&mut self) {
        if !self.mem.is_empty() {
            // SAFETY: the list was initialised and not yet deleted.
            unsafe { DeleteProcThreadAttributeList(self.mem.as_mut_ptr().cast()) };
        }
    }
}

fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let (mut read, mut write): (HANDLE, HANDLE) = (null_mut(), null_mut());
    // SAFETY: both out-parameters are live; default security, default buffer size.
    if unsafe { CreatePipe(&mut read, &mut write, null(), 0) } == 0 {
        return Err(last_error("CreatePipe"));
    }
    Ok((OwnedHandle(read), OwnedHandle(write)))
}

/// A pseudoconsole closed on a helper thread (or never, if already closed).
struct Console {
    hpc: HPCON,
    closed: bool,
}

impl Console {
    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let h = self.hpc;
            // On Windows before 11 24H2 ClosePseudoConsole returns only after its output was read: never block a caller on it.
            std::thread::spawn(move || {
                // SAFETY: `h` came from CreatePseudoConsole and is closed once (guarded by `closed`).
                unsafe { ClosePseudoConsole(h) };
            });
        }
    }
}

fn start(
    shell: &Shell,
    request: &PtyRequest,
    user_token: Option<HANDLE>,
    cwd: Option<Vec<u16>>,
    account: String,
) -> Result<ConPty> {
    let (in_read, in_write) = pipe()?;
    let (out_read, out_write) = pipe()?;
    let size = COORD { X: request.cols.min(i16::MAX as u16) as i16, Y: request.rows.min(i16::MAX as u16) as i16 };
    let mut hpc: HPCON = 0;
    // SAFETY: `in_read` and `out_write` are open pipe ends; `hpc` is a live out-parameter; flags 0 = no cursor inheritance.
    let hr = unsafe { CreatePseudoConsole(size, in_read.0, out_write.0, 0, &mut hpc) };
    if hr < 0 {
        return Err(PlatformError {
            kind: ErrorKind::Os,
            op: "CreatePseudoConsole",
            os_code: Some(i64::from(hr)),
            detail: format!("HRESULT {hr:#x}"),
        });
    }
    let mut console = Console { hpc, closed: false };
    // The pseudoconsole holds its own copies of these two ends.
    drop(in_read);
    drop(out_write);
    let mut attrs = match AttrList::pseudoconsole(hpc) {
        Ok(a) => a,
        Err(e) => {
            console.close();
            return Err(e);
        }
    };
    let mut cmd = wide(OsStr::new(shell.command_line()))?;
    let mut desktop = wide(OsStr::new(SHELL_DESKTOP))?;
    // SAFETY: plain data; zeroed is valid, `cb` and the attribute list are set below.
    let mut si: STARTUPINFOEXW = unsafe { core::mem::zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
    // The shell must use the pseudoconsole, not this process's own standard handles (a parent with redirected stdio would
    // otherwise leak them into the child): say explicitly that it has none.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = attrs.ptr();
    // SAFETY: plain data.
    let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    // Suspended: the shell runs no instruction before it is in the kill-on-close job, so nothing it starts can be outside it.
    let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED;
    let dir = cwd.as_ref().map_or(null(), |d| d.as_ptr());
    let created = match user_token {
        Some(token) => {
            let env = match EnvBlock::for_token(token) {
                Ok(env) => env,
                Err(e) => {
                    console.close();
                    return Err(e);
                }
            };
            // SAFETY: `token` is a primary token for the user's session; `cmd` is a writable null-terminated wide string; `env` is a
            // Unicode block (CREATE_UNICODE_ENVIRONMENT) alive through the call; `si` is a STARTUPINFOEXW (EXTENDED_STARTUPINFO_PRESENT)
            // whose attribute list holds the live pseudoconsole and whose `lpDesktop` points at `desktop`; `dir` is null or
            // null-terminated; `pi` is live.
            unsafe {
                CreateProcessAsUserW(
                    token,
                    null(),
                    cmd.as_mut_ptr(),
                    null(),
                    null(),
                    0,
                    flags,
                    env.as_ptr(),
                    dir,
                    (&raw const si).cast(),
                    &mut pi,
                )
            }
        }
        // SAFETY: as above, without a token: the process starts with this process's token and environment.
        None => unsafe {
            CreateProcessW(
                null(),
                cmd.as_mut_ptr(),
                null(),
                null(),
                0,
                flags,
                null(),
                dir,
                (&raw const si).cast(),
                &mut pi,
            )
        },
    };
    if created == 0 {
        let e = last_error(if user_token.is_some() { "CreateProcessAsUserW" } else { "CreateProcessW" });
        console.close();
        return Err(e);
    }
    let process = OwnedHandle(pi.hProcess);
    let thread = OwnedHandle(pi.hThread);
    let job = match job_and_resume(&process, &thread) {
        Ok(job) => job,
        Err(e) => {
            // The shell exists but never ran: end it so nothing is left outside any job.
            // SAFETY: the process handle is open and has PROCESS_TERMINATE.
            unsafe { TerminateProcess(process.0, 1) };
            console.close();
            return Err(e);
        }
    };
    drop(thread);
    Ok(ConPty {
        console,
        job,
        process,
        pid: pi.dwProcessId,
        input: Some(in_write),
        output: Some(out_read),
        account,
        exit_seen: None,
        code: None,
    })
}

/// Puts the suspended shell in a new job that kills it and everything it started when closed, then lets it run.
/// Any failure is an error: a terminal that cannot be contained is not started.
fn job_and_resume(process: &OwnedHandle, thread: &OwnedHandle) -> Result<OwnedHandle> {
    // SAFETY: an unnamed job with default security.
    let job = OwnedHandle(unsafe { CreateJobObjectW(null(), null()) });
    if job.0.is_null() {
        return Err(last_error("CreateJobObjectW"));
    }
    // SAFETY: plain data; zeroed is valid.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `info` is the structure the information class expects, with the size passed.
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(last_error("SetInformationJobObject"));
    }
    // SAFETY: both handles are open; the process is suspended and not yet in a job of ours (nested jobs are allowed).
    if unsafe { AssignProcessToJobObject(job.0, process.0) } == 0 {
        return Err(last_error("AssignProcessToJobObject"));
    }
    // SAFETY: the thread handle is the suspended primary thread's, with THREAD_SUSPEND_RESUME (CreateProcess gives full access).
    if unsafe { ResumeThread(thread.0) } == u32::MAX {
        return Err(last_error("ResumeThread"));
    }
    Ok(job)
}

// ---------- the running terminal ----------

pub struct ConPty {
    console: Console,
    /// Kills the process tree when closed; never read outside the tests, order among the fields is irrelevant.
    #[allow(dead_code)]
    job: OwnedHandle,
    process: OwnedHandle,
    pid: u32,
    input: Option<OwnedHandle>,
    output: Option<OwnedHandle>,
    account: String,
    exit_seen: Option<Instant>,
    code: Option<i32>,
}

impl ConPty {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The user SID the shell process runs as, read from the process's own token (not from what was asked for).
    pub fn child_user_sid(&self) -> Result<String> {
        let mut token: HANDLE = null_mut();
        // SAFETY: the process handle has PROCESS_QUERY_LIMITED_INFORMATION (CreateProcess gives full access); `token` is live.
        if unsafe { windows_sys::Win32::System::Threading::OpenProcessToken(self.process.0, TOKEN_QUERY, &mut token) }
            == 0
        {
            return Err(last_error("OpenProcessToken(child)"));
        }
        let token = OwnedHandle(token);
        token_user_sid(token.0)
    }

    fn exited(&self) -> bool {
        // SAFETY: the process handle is open.
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Pty for ConPty {
    fn account(&self) -> &str {
        &self.account
    }

    fn reader(&mut self) -> Result<Box<dyn Read + Send>> {
        let h = self.output.take().ok_or_else(|| PlatformError {
            kind: ErrorKind::Closed,
            op: "ConPty.reader",
            os_code: None,
            detail: "already taken".into(),
        })?;
        Ok(Box::new(PipeEnd(h)))
    }

    fn writer(&mut self) -> Result<Box<dyn Write + Send>> {
        let h = self.input.take().ok_or_else(|| PlatformError {
            kind: ErrorKind::Closed,
            op: "ConPty.writer",
            os_code: None,
            detail: "already taken".into(),
        })?;
        Ok(Box::new(PipeEnd(h)))
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if self.console.closed {
            return Err(PlatformError {
                kind: ErrorKind::Closed,
                op: "ResizePseudoConsole",
                os_code: None,
                detail: "closed".into(),
            });
        }
        if cols == 0 || rows == 0 {
            return Err(invalid("ResizePseudoConsole", "empty size"));
        }
        let size = COORD { X: cols.min(i16::MAX as u16) as i16, Y: rows.min(i16::MAX as u16) as i16 };
        // SAFETY: the pseudoconsole is open (`closed` is false).
        let hr = unsafe { ResizePseudoConsole(self.console.hpc, size) };
        if hr < 0 {
            return Err(PlatformError {
                kind: ErrorKind::Os,
                op: "ResizePseudoConsole",
                os_code: Some(i64::from(hr)),
                detail: format!("HRESULT {hr:#x}"),
            });
        }
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.code {
            return Ok(Some(code));
        }
        if !self.exited() {
            return Ok(None);
        }
        let seen = *self.exit_seen.get_or_insert_with(Instant::now);
        if seen.elapsed() < EXIT_GRACE {
            return Ok(None);
        }
        let mut raw = 0u32;
        // SAFETY: the process handle is open and the process has exited; `raw` is live.
        if unsafe { GetExitCodeProcess(self.process.0, &mut raw) } == 0 {
            return Err(last_error("GetExitCodeProcess"));
        }
        // Closing the console ends the output pipe once the buffered output is read.
        self.console.close();
        self.code = Some(raw as i32);
        Ok(self.code)
    }

    fn kill(&mut self) -> Result<()> {
        if !self.exited() {
            // SAFETY: the process handle is open and has PROCESS_TERMINATE.
            unsafe { TerminateProcess(self.process.0, 1) };
            // SAFETY: the process handle is open.
            unsafe { WaitForSingleObject(self.process.0, 2000) };
        }
        self.console.close();
        if self.code.is_none() && self.exited() {
            let mut raw = 1u32;
            // SAFETY: the process handle is open and the process has exited; `raw` is live.
            if unsafe { GetExitCodeProcess(self.process.0, &mut raw) } != 0 {
                self.code = Some(raw as i32);
            }
        }
        Ok(())
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// One end of a pseudoconsole pipe as `Read` or `Write`.
struct PipeEnd(OwnedHandle);

impl Read for PipeEnd {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut n = 0u32;
        // SAFETY: the handle is open for reading; `buf` is valid for `len` bytes; `n` is live; no overlapped I/O.
        let ok = unsafe {
            ReadFile(self.0.0, buf.as_mut_ptr().cast(), buf.len().min(u32::MAX as usize) as u32, &mut n, null_mut())
        };
        if ok != 0 {
            return Ok(n as usize);
        }
        match std::io::Error::last_os_error() {
            e if e.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            e => Err(e),
        }
    }
}

impl Write for PipeEnd {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut n = 0u32;
        // SAFETY: the handle is open for writing; `buf` is valid for `len` bytes; `n` is live; no overlapped I/O.
        let ok = unsafe {
            WriteFile(self.0.0, buf.as_ptr().cast(), buf.len().min(u32::MAX as usize) as u32, &mut n, null_mut())
        };
        if ok != 0 { Ok(n as usize) } else { Err(std::io::Error::last_os_error()) }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicProcessIdList, QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};

    use super::*;
    use crate::sys::current_user_sid;

    const USER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    const OTHER: &str = "S-1-5-21-1111111111-2222222222-3333333333-1002";

    fn facts(sid: &str, session_id: u32) -> TokenFacts {
        TokenFacts { user_sid: sid.into(), session_id }
    }

    #[test]
    fn a_user_terminal_refuses_a_system_token_when_told_the_session_user() {
        assert_eq!(check_spawn(false, &facts(SID_SYSTEM, 0), Some(USER)), Err(Refusal::ServiceAccountForUser));
        assert_eq!(
            check_spawn(false, &facts(SID_SYSTEM, 1), None),
            Err(Refusal::ServiceAccountForUser),
            "never SYSTEM, told or not"
        );
        for service in ["S-1-5-19", "S-1-5-20"] {
            assert_eq!(check_spawn(false, &facts(service, 0), Some(USER)), Err(Refusal::ServiceAccountForUser));
        }
    }

    #[test]
    fn a_user_terminal_needs_the_session_user_when_one_is_named() {
        assert_eq!(check_spawn(false, &facts(USER, 1), Some(USER)), Ok(()));
        assert_eq!(check_spawn(false, &facts(USER, 1), None), Ok(()));
        assert_eq!(check_spawn(false, &facts(OTHER, 1), Some(USER)), Err(Refusal::NotSessionUser));
    }

    #[test]
    fn the_administrator_terminal_needs_system_in_session_zero() {
        assert_eq!(check_spawn(true, &facts(SID_SYSTEM, 0), None), Ok(()));
        assert_eq!(check_spawn(true, &facts(USER, 1), None), Err(Refusal::AdminNeedsSystem));
        assert_eq!(
            check_spawn(true, &facts(SID_SYSTEM, 1), Some(USER)),
            Err(Refusal::AdminOnUserSession),
            "never on the user's desktop"
        );
    }

    #[test]
    fn the_spawner_refuses_without_starting_anything() {
        let me = current_user_sid().unwrap();
        assert_ne!(me, SID_SYSTEM, "run the tests as a user");
        // Told that somebody else is logged in: this process's token is not theirs.
        let wrong_user = ConPtySpawner::current_user(Shell::command("cmd.exe /k"), Some(OTHER.into()));
        let e = wrong_user.spawn_conpty(&PtyRequest { cols: 80, rows: 24, admin: false }).err().unwrap();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        // An administrator terminal from a user process is refused as `PermissionDenied`, the C3 rule.
        let e = ConPtySpawner::current_user(Shell::command("cmd.exe /k"), Some(me))
            .spawn(&PtyRequest { cols: 80, rows: 24, admin: true })
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::PermissionDenied(OsGrant::Administrator));
        let e = ConPtySpawner::current_user(Shell::user_default(), None)
            .spawn(&PtyRequest { cols: 0, rows: 24, admin: false })
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
    }

    /// Reads a PTY to the end of its output on a thread; the chunks arrive on the channel.
    fn pump(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        rx
    }

    /// F-TERM-01: the shell the spawner starts runs as the calling user, not as SYSTEM.
    #[test]
    fn the_shell_runs_as_the_calling_user_not_system() {
        let me = current_user_sid().unwrap();
        assert_ne!(me, SID_SYSTEM, "run the tests as a user");
        let spawner = ConPtySpawner::current_user(Shell::command("cmd.exe /k"), Some(me.clone()));
        let mut pty = spawner.spawn_conpty(&PtyRequest { cols: 200, rows: 30, admin: false }).unwrap();

        // The child's own token, read from the running process.
        let child = pty.child_user_sid().unwrap();
        assert_eq!(child, me, "the shell's token user is the calling user");
        assert_ne!(child, SID_SYSTEM);

        // And what the shell itself says it is.
        let output = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        // The system's own whoami (a GNU one earlier in PATH would not know `/user`).
        writer.write_all(b"\"%SystemRoot%\\System32\\whoami.exe\" /user\r\nexit 3\r\n").unwrap();
        let mut text = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut ended = false;
        while Instant::now() < deadline {
            match output.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => text.extend(chunk),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    ended = true;
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // The output only ends once the exit was seen and the console closed: poll it as the relay does.
                    let _ = pty.try_wait();
                }
            }
        }
        let text = String::from_utf8_lossy(&text);
        assert!(ended, "the output ended after the shell exited: {text}");
        assert!(text.contains(&me), "whoami printed the user's SID {me}: {text}");
        assert!(!text.contains(SID_SYSTEM), "never SYSTEM: {text}");
        assert!(
            pty.account().eq_ignore_ascii_case(&std::env::var("USERNAME").unwrap_or_default()),
            "account {}",
            pty.account()
        );
        assert_eq!(pty.try_wait().unwrap(), Some(3), "the exit code reaches the relay");
    }

    /// The process ids in a job object.
    fn job_pids(job: HANDLE) -> Vec<u32> {
        const CAP: usize = 64;
        // The structure ends in a one-element array: a u64 buffer of 2 + CAP words, aligned for it, holds up to CAP ids.
        let mut buf = vec![0u64; 1 + CAP];
        let mut returned = 0u32;
        // SAFETY: `buf` is 8-byte aligned and `size_of_val` bytes long; the class takes a JOBOBJECT_BASIC_PROCESS_ID_LIST.
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicProcessIdList,
                buf.as_mut_ptr().cast(),
                size_of_val(&buf[..]) as u32,
                &mut returned,
            )
        };
        assert_ne!(ok, 0, "QueryInformationJobObject: {:?}", last_error("QueryInformationJobObject"));
        // SAFETY: the call wrote a JOBOBJECT_BASIC_PROCESS_ID_LIST at the start of the buffer.
        let list = unsafe { &*buf.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
        let ids: *const usize = list.ProcessIdList.as_ptr();
        // SAFETY: the kernel wrote `NumberOfProcessIdsInList` ids from `ProcessIdList` on, all inside the buffer.
        (0..list.NumberOfProcessIdsInList as usize).map(|i| unsafe { *ids.add(i) } as u32).collect()
    }

    fn open_pid(pid: u32) -> OwnedHandle {
        // SAFETY: a plain OpenProcess for a pid just read from the job.
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!h.is_null(), "OpenProcess({pid}): {:?}", last_error("OpenProcess"));
        OwnedHandle(h)
    }

    fn in_job(process: &OwnedHandle, job: &OwnedHandle) -> bool {
        let mut result = 0;
        // SAFETY: both handles are open; `result` is live.
        assert_ne!(unsafe { IsProcessInJob(process.0, job.0, &mut result) }, 0, "IsProcessInJob");
        result != 0
    }

    /// B6: the shell is created suspended and put in the kill-on-close job before it runs, so it and what it starts are
    /// members of the job, and closing the terminal ends all of them.
    #[test]
    fn the_shell_and_what_it_starts_are_in_the_job_and_die_with_it() {
        let me = current_user_sid().unwrap();
        let sysroot = std::env::var("SystemRoot").unwrap();
        // cmd starts ping as its child and waits for it: two processes under the pseudoconsole.
        let line = format!(r"cmd.exe /c {sysroot}\System32\ping.exe -n 300 127.0.0.1 >nul");
        let pty = ConPtySpawner::current_user(Shell::command(line), Some(me))
            .spawn_conpty(&PtyRequest { cols: 80, rows: 24, admin: false })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let pids = loop {
            let pids = job_pids(pty.job.0);
            if pids.len() >= 2 || Instant::now() > deadline {
                break pids;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(pids.contains(&pty.pid()), "the shell {} is in the job: {pids:?}", pty.pid());
        assert!(pids.len() >= 2, "the shell's child joined the job on its own: {pids:?}");
        let members: Vec<OwnedHandle> = pids.iter().map(|&p| open_pid(p)).collect();
        for m in &members {
            assert!(in_job(m, &pty.job), "IsProcessInJob");
        }

        drop(pty);
        for (m, pid) in members.iter().zip(&pids) {
            // SAFETY: the handle has SYNCHRONIZE.
            assert_eq!(unsafe { WaitForSingleObject(m.0, 10_000) }, WAIT_OBJECT_0, "process {pid} ended with the job");
        }
    }

    #[test]
    fn resize_and_kill_work_and_kill_ends_the_output() {
        let me = current_user_sid().unwrap();
        let spawner = ConPtySpawner::current_user(Shell::command("cmd.exe /k"), Some(me));
        let mut pty = spawner.spawn(&PtyRequest { cols: 80, rows: 24, admin: false }).unwrap();
        let output = pump(pty.reader().unwrap());
        pty.resize(120, 40).unwrap();
        assert!(pty.resize(0, 40).is_err());
        assert_eq!(pty.try_wait().unwrap(), None, "the shell is running");
        pty.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match output.recv_timeout(Duration::from_millis(100)) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                _ if Instant::now() > deadline => panic!("the output did not end after kill"),
                _ => {}
            }
        }
        assert!(pty.resize(80, 24).is_err(), "closed");
        assert!(pty.try_wait().unwrap().is_some());
    }
}
