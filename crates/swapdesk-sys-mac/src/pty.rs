//! The `openpty` `PtySpawner` (C3, D14: own code): the user's login shell on a pseudo-terminal, as this process's own
//! user and never as root (F-TERM-01).
//!
//! * Who: the shell always runs as the uid of the calling process (the Aqua user helper). A request for `admin` is
//!   refused with `PermissionDenied(Administrator)`, as on Windows, and so is a spawn from a process running as root.
//!   The "session user" the service names is not an input here: nothing a caller says can make a shell run as somebody
//!   else, because the spawner never changes uid.
//! * What: the account's login shell from the user database (`getpwuid_r`, falling back to `/bin/zsh`), started as a
//!   login shell (`argv[0]` with a leading `-`), in the home folder, with `TERM=xterm-256color` and a small clean
//!   environment.
//! * How: `openpty`, then `fork`. Between `fork` and `execve` the child makes only async-signal-safe calls (`setsid`,
//!   `ioctl`, `dup2`, `close`, `fcntl`, `chdir`, `signal`, `sigprocmask`, `execve`, `write`, `_exit`); every string
//!   is built before the fork. A close-on-exec pipe carries the child's `errno` back when `execve` fails.
//! * The shell is a session leader, so its process group is its pid: `kill` sends `SIGHUP` to the group, then `SIGKILL`,
//!   and dropping the terminal does the same.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use swapdesk_platform::{ErrorKind, OsGrant, PlatformError, Pty, PtyRequest, PtySpawner, Result};

const FALLBACK_SHELL: &str = "/bin/zsh";
const TERM: &str = "xterm-256color";
const FALLBACK_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
/// How long a `SIGHUP` gets before the group is killed.
const HUP_GRACE: Duration = Duration::from_millis(400);
/// How long `kill` waits for the reaped exit after `SIGKILL`.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// Environment variables the shell inherits from this process, when set.
const INHERITED: [&str; 6] = ["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "ZDOTDIR"];

fn error(kind: ErrorKind, op: &'static str, os_code: Option<i64>, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op, os_code, detail: detail.into() }
}

fn io_error(op: &'static str, e: &std::io::Error) -> PlatformError {
    error(ErrorKind::Os, op, e.raw_os_error().map(i64::from), e.to_string())
}

fn os_error(op: &'static str) -> PlatformError {
    io_error(op, &std::io::Error::last_os_error())
}

// ---------- the guard ----------

/// Why a spawn was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// An administrator terminal: it is not this spawner's to give.
    Admin,
    /// This process runs as root; a user terminal never does.
    Root,
    /// A terminal with no rows or columns.
    EmptySize,
}

impl Refusal {
    fn into_error(self) -> PlatformError {
        match self {
            Refusal::Admin => error(
                ErrorKind::PermissionDenied(OsGrant::Administrator),
                "MacPtySpawner.spawn",
                None,
                "the administrator terminal is not available on this Mac",
            ),
            Refusal::Root => {
                error(ErrorKind::Unsupported, "MacPtySpawner.spawn", None, "a user terminal never runs as root")
            }
            Refusal::EmptySize => error(ErrorKind::InvalidInput, "MacPtySpawner.spawn", None, "empty size"),
        }
    }
}

/// The rule of this module (pure): may a shell be started for this request by a process with this effective uid?
pub fn check_spawn(request: &PtyRequest, euid: u32) -> core::result::Result<(), Refusal> {
    if request.admin {
        return Err(Refusal::Admin);
    }
    if euid == 0 {
        return Err(Refusal::Root);
    }
    if request.cols == 0 || request.rows == 0 {
        return Err(Refusal::EmptySize);
    }
    Ok(())
}

/// The login shell's `argv[0]`: the program's file name with a leading `-`.
pub fn login_argv0(shell: &str) -> String {
    let name = shell.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("zsh");
    format!("-{name}")
}

/// The shell to run: the account's, when it is an absolute path, else `/bin/zsh`.
pub fn choose_shell(account_shell: Option<&str>) -> String {
    match account_shell {
        Some(s) if s.starts_with('/') && s.len() > 1 => s.to_string(),
        _ => FALLBACK_SHELL.to_string(),
    }
}

// ---------- the account ----------

#[derive(Debug, Clone)]
struct Account {
    name: String,
    shell: String,
    home: String,
}

fn lookup_account(uid: libc::uid_t) -> Result<Account> {
    // SAFETY: plain data; zeroed is valid and `getpwuid_r` fills it.
    let mut pwd: libc::passwd = unsafe { core::mem::zeroed() };
    let mut buf = vec![0u8; 4096];
    loop {
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `pwd`, `buf` (with its length) and `result` are live for the call.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut result) };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(error(ErrorKind::Os, "getpwuid_r", Some(i64::from(rc)), "user database lookup failed"));
        }
        if result.is_null() {
            return Err(error(ErrorKind::Unavailable, "getpwuid_r", None, format!("no account for uid {uid}")));
        }
        break;
    }
    let text = |p: *const libc::c_char| -> Option<String> {
        // SAFETY: a non-null field of a `passwd` filled by `getpwuid_r` is a NUL-terminated string in `buf`.
        (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    };
    let name = text(pwd.pw_name)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| error(ErrorKind::Unavailable, "getpwuid_r", None, "the account has no name"))?;
    let shell = choose_shell(text(pwd.pw_shell).as_deref());
    let home = text(pwd.pw_dir).filter(|h| h.starts_with('/')).unwrap_or_else(|| "/".to_string());
    Ok(Account { name, shell, home })
}

// ---------- the spawner ----------

/// Starts login shells of the calling process's own user.
#[derive(Debug, Clone, Default)]
pub struct MacPtySpawner {
    _private: (),
}

impl MacPtySpawner {
    /// The user helper's spawner: shells run as this process's user.
    pub fn current_user() -> MacPtySpawner {
        MacPtySpawner { _private: () }
    }

    /// Spawns and keeps the concrete type (the tests read the child's pid through it).
    pub fn spawn_pty(&self, request: &PtyRequest) -> Result<MacPty> {
        // SAFETY: no preconditions.
        let (uid, euid) = unsafe { (libc::getuid(), libc::geteuid()) };
        check_spawn(request, euid).map_err(Refusal::into_error)?;
        let account = lookup_account(uid)?;
        start(&account, request)
    }
}

impl PtySpawner for MacPtySpawner {
    fn spawn(&self, request: &PtyRequest) -> Result<Box<dyn Pty>> {
        Ok(Box::new(self.spawn_pty(request)?))
    }
}

fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| error(ErrorKind::InvalidInput, "MacPtySpawner.spawn", None, "a string holds a NUL"))
}

fn env_block(account: &Account) -> Vec<String> {
    let mut env = vec![
        format!("HOME={}", account.home),
        format!("USER={}", account.name),
        format!("LOGNAME={}", account.name),
        format!("SHELL={}", account.shell),
        format!("TERM={TERM}"),
    ];
    for key in INHERITED {
        match std::env::var(key) {
            Ok(v) if !v.is_empty() => env.push(format!("{key}={v}")),
            _ if key == "PATH" => env.push(format!("PATH={FALLBACK_PATH}")),
            _ => {}
        }
    }
    env
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

fn set_cloexec(fd: RawFd) -> Result<()> {
    // SAFETY: `fd` is an open descriptor of this process.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(os_error("fcntl(FD_CLOEXEC)"));
    }
    Ok(())
}

fn start(account: &Account, request: &PtyRequest) -> Result<MacPty> {
    // Everything the child needs is built before the fork: it may not allocate.
    let shell = cstring(&account.shell)?;
    let argv0 = cstring(&login_argv0(&account.shell))?;
    let home = cstring(&account.home)?;
    let env: Vec<CString> = env_block(account).iter().map(|e| cstring(e)).collect::<Result<_>>()?;
    let argv = [argv0.as_ptr(), std::ptr::null()];
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());

    let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
    let mut ws = winsize(request.cols, request.rows);
    // SAFETY: both out-parameters and the window size are live; no name buffer and no termios (defaults).
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) } == -1 {
        return Err(os_error("openpty"));
    }
    // SAFETY: openpty returned two open descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    set_cloexec(master.as_raw_fd())?;
    set_cloexec(slave.as_raw_fd())?;

    // The exec-status pipe: the child writes its errno to it when `execve` fails; it closes on a good exec.
    let mut fds = [-1 as RawFd; 2];
    // SAFETY: `fds` holds two ints.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(os_error("pipe"));
    }
    // SAFETY: pipe returned two open descriptors that nothing else owns.
    let (status_read, status_write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(status_read.as_raw_fd())?;
    set_cloexec(status_write.as_raw_fd())?;

    let (slave_fd, status_fd) = (slave.as_raw_fd(), status_write.as_raw_fd());
    // SAFETY: the child branch makes only async-signal-safe calls on data prepared above, then execs or `_exit`s.
    let pid = unsafe { libc::fork() };
    if pid == -1 {
        return Err(os_error("fork"));
    }
    if pid == 0 {
        // SAFETY: in the child after fork; the pointers are NUL-terminated data that live until the exec.
        unsafe { child_exec(slave_fd, status_fd, home.as_ptr(), shell.as_ptr(), argv.as_ptr(), envp.as_ptr()) }
    }

    drop(slave);
    drop(status_write);
    // Wait for the exec: an empty read means the pipe closed on a good exec, four bytes are the child's errno.
    let mut code = [0u8; 4];
    let mut status = File::from(status_read);
    let n = loop {
        match status.read(&mut code) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break 0,
            Ok(n) => break n,
        }
    };
    if n == 4 {
        // SAFETY: the child ended by itself (`_exit`); reap it so no zombie stays.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        let errno = i32::from_ne_bytes(code);
        return Err(error(
            ErrorKind::Os,
            "MacPtySpawner.spawn",
            Some(i64::from(errno)),
            format!("starting {}: {}", account.shell, std::io::Error::from_raw_os_error(errno)),
        ));
    }
    let input = File::from(master.try_clone().map_err(|e| io_error("dup(master)", &e))?);
    let output = File::from(master.try_clone().map_err(|e| io_error("dup(master)", &e))?);
    Ok(MacPty { master, pid, input: Some(input), output: Some(output), account: account.name.clone(), code: None })
}

/// Reports `errno` on the status pipe and ends the child.
///
/// # Safety
/// Child after `fork` only; `status` is the open status pipe.
unsafe fn child_fail(status: RawFd) -> ! {
    // SAFETY: `__error`, `write` and `_exit` are async-signal-safe; the buffer is a local array.
    unsafe {
        let errno = (*libc::__error()).to_ne_bytes();
        libc::write(status, errno.as_ptr().cast(), errno.len());
        libc::_exit(127)
    }
}

/// The child between `fork` and `execve`: async-signal-safe calls only, no allocation, no Rust runtime.
///
/// # Safety
/// Only after `fork` in the child; the pointers are NUL-terminated strings and NULL-terminated arrays that outlive it.
unsafe fn child_exec(
    slave: RawFd,
    status: RawFd,
    home: *const libc::c_char,
    shell: *const libc::c_char,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
) -> ! {
    // SAFETY: every call below is async-signal-safe and takes valid arguments (see the function's contract).
    unsafe {
        // A new session with the pty as its controlling terminal; the shell becomes leader of its own process group.
        if libc::setsid() == -1 {
            child_fail(status);
        }
        if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
            child_fail(status);
        }
        for target in 0..3 {
            if libc::dup2(slave, target) == -1 {
                child_fail(status);
            }
            // A descriptor 0..2 must survive the exec (dup2 onto itself would keep FD_CLOEXEC).
            libc::fcntl(target, libc::F_SETFD, 0);
        }
        // Rust ignores SIGPIPE and may block signals; the shell starts with the defaults.
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        let mut set: libc::sigset_t = core::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        if libc::chdir(home) == -1 {
            libc::chdir(c"/".as_ptr());
        }
        libc::execve(shell, argv, envp);
        child_fail(status)
    }
}

// ---------- the running terminal ----------

pub struct MacPty {
    master: OwnedFd,
    pid: libc::pid_t,
    input: Option<File>,
    output: Option<File>,
    account: String,
    code: Option<i32>,
}

/// The exit code a wait status stands for: the exit status, or 128 + the signal as a shell reports it.
fn code_of(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

impl MacPty {
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// The uid the shell process runs as, read from the kernel (not from what was asked for).
    pub fn child_uid(&self) -> Result<u32> {
        // SAFETY: plain data; zeroed is valid and `proc_pidinfo` fills it.
        let mut info: libc::proc_bsdinfo = unsafe { core::mem::zeroed() };
        // SAFETY: `info` is live with the size passed.
        let n = unsafe {
            libc::proc_pidinfo(
                self.pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                size_of::<libc::proc_bsdinfo>() as libc::c_int,
            )
        };
        if n <= 0 {
            return Err(os_error("proc_pidinfo"));
        }
        Ok(info.pbi_uid)
    }

    fn reap(&mut self, flags: libc::c_int) -> Result<Option<i32>> {
        if let Some(code) = self.code {
            return Ok(Some(code));
        }
        let mut status = 0;
        loop {
            // SAFETY: `status` is live; `pid` is this terminal's own child.
            let r = unsafe { libc::waitpid(self.pid, &mut status, flags) };
            if r == self.pid {
                self.code = Some(code_of(status));
                return Ok(self.code);
            }
            if r == 0 {
                return Ok(None);
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => {}
                // Already reaped elsewhere: the code is gone.
                Some(libc::ECHILD) => {
                    self.code = Some(1);
                    return Ok(self.code);
                }
                _ => return Err(io_error("waitpid", &e)),
            }
        }
    }

    fn signal_group(&self, signal: libc::c_int) {
        // SAFETY: the shell is its own process group's leader (setsid), so the group id is its pid; ESRCH is fine.
        unsafe { libc::killpg(self.pid, signal) };
    }

    fn wait_until(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if matches!(self.reap(libc::WNOHANG), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn taken(op: &'static str) -> PlatformError {
    error(ErrorKind::Closed, op, None, "already taken")
}

impl Pty for MacPty {
    fn account(&self) -> &str {
        &self.account
    }

    fn reader(&mut self) -> Result<Box<dyn Read + Send>> {
        let file = self.output.take().ok_or_else(|| taken("MacPty.reader"))?;
        Ok(Box::new(MasterReader(file)))
    }

    fn writer(&mut self) -> Result<Box<dyn Write + Send>> {
        let file = self.input.take().ok_or_else(|| taken("MacPty.writer"))?;
        Ok(Box::new(file))
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        if self.code.is_some() {
            return Err(error(ErrorKind::Closed, "TIOCSWINSZ", None, "the shell has exited"));
        }
        if cols == 0 || rows == 0 {
            return Err(error(ErrorKind::InvalidInput, "TIOCSWINSZ", None, "empty size"));
        }
        let ws = winsize(cols, rows);
        // SAFETY: the master is open; `ws` is live; the kernel signals SIGWINCH to the foreground group.
        if unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } == -1 {
            return Err(os_error("TIOCSWINSZ"));
        }
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        self.reap(libc::WNOHANG)
    }

    fn kill(&mut self) -> Result<()> {
        // The group is signalled even when the shell itself is gone: what it started may still hold the terminal.
        self.signal_group(libc::SIGHUP);
        if self.wait_until(HUP_GRACE) {
            self.signal_group(libc::SIGKILL);
        } else {
            self.signal_group(libc::SIGKILL);
            self.wait_until(KILL_WAIT);
        }
        Ok(())
    }
}

impl Drop for MacPty {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// The master side as `Read`: when the last slave descriptor closes, a read fails with `EIO`, which is the end.
struct MasterReader(File);

impl Read for MasterReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.0.read(buf) {
                Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::sync::{Mutex, Once};

    use super::*;

    /// A folder of its own for the test shells' zsh start-up files, so the login shells these tests start never
    /// touch the user's `~/.zsh_history`. An interactive zsh reads that file at start and writes it at exit under a
    /// lock file (`.zsh_history.LOCK`) it retries for about ten seconds: a stale lock stalls every login shell on the
    /// Mac, and a test run would write its commands into the owner's history. The spawner passes `ZDOTDIR` on, and
    /// zsh then keeps its history and profile there instead of in `$HOME`.
    static SHELL_DIR_USERS: Mutex<usize> = Mutex::new(0);

    struct ShellDir(PathBuf);

    impl ShellDir {
        fn open() -> ShellDir {
            let dir = std::env::temp_dir().join(format!("swapdesk-sys-mac-zdotdir-{}", std::process::id()));
            let mut users = SHELL_DIR_USERS.lock().unwrap();
            std::fs::create_dir_all(&dir).expect("the shells' start-up folder");
            static SET: Once = Once::new();
            SET.call_once(|| {
                // SAFETY: done once, under the lock every test that starts a shell takes first, with the same value.
                unsafe { std::env::set_var("ZDOTDIR", &dir) };
            });
            *users += 1;
            ShellDir(dir)
        }
    }

    impl Drop for ShellDir {
        fn drop(&mut self) {
            // The last test of this process out removes the folder; a test that starts later makes it again.
            let mut users = SHELL_DIR_USERS.lock().unwrap();
            *users -= 1;
            if *users == 0 {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    /// A login shell of this user on its own start-up folder; keep the folder guard alive as long as the shell
    /// (bind it first: it is dropped last).
    fn isolated_pty(request: &PtyRequest) -> (ShellDir, MacPty) {
        let dir = ShellDir::open();
        (dir, MacPtySpawner::current_user().spawn_pty(request).unwrap())
    }

    fn request(admin: bool) -> PtyRequest {
        PtyRequest { cols: 80, rows: 24, admin }
    }

    #[test]
    fn the_guard_refuses_admin_root_and_an_empty_size() {
        assert_eq!(check_spawn(&request(false), 501), Ok(()));
        assert_eq!(check_spawn(&request(true), 501), Err(Refusal::Admin));
        assert_eq!(check_spawn(&request(true), 0), Err(Refusal::Admin));
        assert_eq!(check_spawn(&request(false), 0), Err(Refusal::Root));
        assert_eq!(check_spawn(&PtyRequest { cols: 0, rows: 24, admin: false }, 501), Err(Refusal::EmptySize));
        assert_eq!(check_spawn(&PtyRequest { cols: 80, rows: 0, admin: false }, 501), Err(Refusal::EmptySize));
    }

    #[test]
    fn the_login_shell_is_named_with_a_leading_dash_and_falls_back_to_zsh() {
        assert_eq!(login_argv0("/bin/zsh"), "-zsh");
        assert_eq!(login_argv0("/opt/homebrew/bin/fish"), "-fish");
        assert_eq!(choose_shell(Some("/bin/bash")), "/bin/bash");
        assert_eq!(choose_shell(Some("")), "/bin/zsh");
        assert_eq!(choose_shell(Some("zsh")), "/bin/zsh");
        assert_eq!(choose_shell(None), "/bin/zsh");
    }

    #[test]
    fn the_spawner_refuses_admin_and_an_empty_size_without_starting_anything() {
        let spawner = MacPtySpawner::current_user();
        let e = spawner.spawn(&request(true)).err().unwrap();
        assert_eq!(e.kind, ErrorKind::PermissionDenied(OsGrant::Administrator));
        let e = spawner.spawn(&PtyRequest { cols: 0, rows: 24, admin: false }).err().unwrap();
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

    /// Collects output until `needle` appears, the stream ends or the bound passes.
    fn read_until(rx: &mpsc::Receiver<Vec<u8>>, text: &mut Vec<u8>, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if String::from_utf8_lossy(text).contains(needle) {
                return true;
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => text.extend(chunk),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        String::from_utf8_lossy(text).contains(needle)
    }

    /// The name the user database gives the current uid (not `$USER`, which a test runner may not set).
    fn user_name() -> String {
        // SAFETY: no preconditions.
        lookup_account(unsafe { libc::getuid() }).unwrap().name
    }

    fn wait_exit(pty: &mut MacPty) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(code) = pty.try_wait().unwrap() {
                return Some(code);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_end(rx: &mpsc::Receiver<Vec<u8>>, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                _ if Instant::now() > deadline => panic!("the output did not end {what}"),
                _ => {}
            }
        }
    }

    #[test]
    fn a_shell_runs_a_command_as_the_calling_user() {
        let (_dir, mut pty) = isolated_pty(&request(false));
        // SAFETY: no preconditions.
        assert_eq!(pty.child_uid().unwrap(), unsafe { libc::getuid() }, "the shell's own uid is the caller's");
        assert_eq!(pty.account(), user_name(), "the account is the current user's name");
        let rx = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        writer.write_all(b"echo swapdesk-$((6*7))\n").unwrap();
        let mut text = Vec::new();
        assert!(read_until(&rx, &mut text, "swapdesk-42"), "the shell answered: {}", String::from_utf8_lossy(&text));
        writer.write_all(b"echo term=$TERM home=$HOME cwd=$(pwd -P)\n").unwrap();
        assert!(read_until(&rx, &mut text, "term=xterm-256color"), "{}", String::from_utf8_lossy(&text));
        assert!(pty.reader().is_err() && pty.writer().is_err(), "each end is taken once");
    }

    #[test]
    fn a_login_shell_has_the_dash_argv0() {
        let (_dir, mut pty) = isolated_pty(&request(false));
        let rx = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        // zsh and bash keep the leading dash of a login shell in `$0`.
        writer.write_all(b"echo argv0=[$0]\n").unwrap();
        let mut text = Vec::new();
        assert!(read_until(&rx, &mut text, "argv0=[-"), "{}", String::from_utf8_lossy(&text));
    }

    #[test]
    fn the_exit_code_reaches_try_wait_and_the_output_ends() {
        let (_dir, mut pty) = isolated_pty(&request(false));
        let rx = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        writer.write_all(b"exit 3\n").unwrap();
        assert_eq!(wait_exit(&mut pty), Some(3));
        wait_end(&rx, "after the shell exited");
        assert_eq!(pty.try_wait().unwrap(), Some(3), "the code is kept");
        assert!(pty.resize(80, 24).is_err(), "closed after the exit");
    }

    #[test]
    fn resize_changes_the_window_size_the_shell_sees() {
        let (_dir, mut pty) = isolated_pty(&request(false));
        let rx = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        pty.resize(120, 40).unwrap();
        assert!(pty.resize(0, 40).is_err());
        writer.write_all(b"stty size\n").unwrap();
        let mut text = Vec::new();
        assert!(read_until(&rx, &mut text, "40 120"), "{}", String::from_utf8_lossy(&text));
    }

    #[test]
    fn the_shell_reports_its_user_and_the_size_it_was_opened_with() {
        let (_dir, mut pty) = isolated_pty(&PtyRequest { cols: 97, rows: 31, admin: false });
        let rx = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        // The echoed command line holds `$(whoami)`, never the expanded text, so a match is the shell's own output.
        writer
            .write_all(
                b"echo who=$(whoami) size=$(stty size)
",
            )
            .unwrap();
        let mut text = Vec::new();
        let want = format!("who={} size=31 97", user_name());
        assert!(read_until(&rx, &mut text, &want), "{}", String::from_utf8_lossy(&text));
    }

    #[test]
    fn a_resize_reaches_the_shell_as_sigwinch() {
        let (_dir, mut pty) = isolated_pty(&request(false));
        let rx = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        writer
            .write_all(
                b"trap 'echo winch=$(stty size)' WINCH; echo armed=$((40+2))
",
            )
            .unwrap();
        let mut text = Vec::new();
        assert!(read_until(&rx, &mut text, "armed=42"), "{}", String::from_utf8_lossy(&text));
        pty.resize(101, 33).unwrap();
        assert!(read_until(&rx, &mut text, "winch=33 101"), "{}", String::from_utf8_lossy(&text));
    }

    #[test]
    fn kill_ends_the_shell_and_what_it_started() {
        let (_dir, mut pty) = isolated_pty(&request(false));
        let rx = pump(pty.reader().unwrap());
        let mut writer = pty.writer().unwrap();
        writer.write_all(b"sleep 300 & echo child=$!\n").unwrap();
        let mut text = Vec::new();
        assert!(read_until(&rx, &mut text, "child="));
        assert_eq!(pty.try_wait().unwrap(), None, "the shell is running");
        pty.kill().unwrap();
        assert!(pty.try_wait().unwrap().is_some(), "reaped after kill");
        wait_end(&rx, "after kill (the background job died with the group)");
    }

    #[test]
    fn dropping_the_terminal_kills_the_shell() {
        let (_dir, pty) = isolated_pty(&request(false));
        let pid = pty.pid() as libc::pid_t;
        drop(pty);
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the shell {pid} is gone after drop");
    }
}
