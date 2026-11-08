//! `ActionRunner` (C3) on Windows: the quick actions of F-ACT-01 and keep-awake.
//!
//! Two scopes (C3 note): the **user helper** runs every action with the user's rights, and the **service** (SYSTEM,
//! session 0) runs lock, sleep and restart as system calls and refuses the Task Manager, because no GUI process is ever
//! started as SYSTEM. What the OS actually does sits behind [`SystemCalls`], so the logic (which scope may do what) is
//! tested with a recorder and the destructive calls (lock, sleep, restart) are never made by a test.

use std::ffi::OsStr;
use std::ptr::{null, null_mut};

use swapdesk_platform::{ActionRunner, ErrorKind, PlatformError};
use swapdesk_proto::Action;
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Power::SetSuspendState;
use windows_sys::Win32::System::RemoteDesktop::WTSDisconnectSession;
use windows_sys::Win32::System::Shutdown::{
    InitiateSystemShutdownExW, LockWorkStation, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
};
use windows_sys::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};

use crate::launch::active_console_session;
use crate::launch::session_user_sid;
use crate::power::KeepAwake;
use crate::sys::{OwnedHandle, Result, last_error, open_process_token, wide};

/// Who runs the actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The user helper: the logged-in user's token, on the user's desktop.
    User,
    /// The service: SYSTEM in session 0; no process is started.
    Service,
}

/// The effects on the machine; the real ones are [`Win32Calls`].
pub trait SystemCalls {
    /// `LockWorkStation`: only a process on the user's interactive desktop can lock it.
    fn lock_workstation(&mut self) -> Result<()>;
    /// Locks the console session from the service (disconnects it to the logon screen).
    fn lock_console_session(&mut self) -> Result<()>;
    /// Whether a user is logged in at the console.
    fn console_user_present(&mut self) -> bool;
    fn suspend(&mut self) -> Result<()>;
    fn reboot(&mut self) -> Result<()>;
    /// Starts `taskmgr.exe` with this process's own token (the user's).
    fn launch_task_manager(&mut self) -> Result<()>;
}

pub struct WinActions<S: SystemCalls = Win32Calls> {
    calls: S,
    scope: Scope,
    awake: Option<KeepAwake>,
}

impl WinActions<Win32Calls> {
    /// For the user helper.
    pub fn user() -> Self {
        Self::with_calls(Win32Calls, Scope::User)
    }

    /// For the service.
    pub fn service() -> Self {
        Self::with_calls(Win32Calls, Scope::Service)
    }
}

impl<S: SystemCalls> WinActions<S> {
    pub fn with_calls(calls: S, scope: Scope) -> Self {
        Self { calls, scope, awake: None }
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// Whether the keep-awake request is on.
    pub fn is_awake_requested(&self) -> bool {
        self.awake.as_ref().is_some_and(KeepAwake::is_on)
    }

    pub fn calls(&self) -> &S {
        &self.calls
    }
}

impl<S: SystemCalls> ActionRunner for WinActions<S> {
    fn run(&mut self, action: Action) -> Result<()> {
        match (self.scope, action) {
            (Scope::User, Action::Lock) => self.calls.lock_workstation(),
            (Scope::Service, Action::Lock) => {
                // With nobody logged in the machine is at the logon screen already.
                if self.calls.console_user_present() { self.calls.lock_console_session() } else { Ok(()) }
            }
            (_, Action::Sleep) => self.calls.suspend(),
            (_, Action::Restart) => self.calls.reboot(),
            (Scope::User, Action::TaskMonitor) => self.calls.launch_task_manager(),
            (Scope::Service, Action::TaskMonitor) => Err(PlatformError {
                kind: ErrorKind::Unsupported,
                op: "ActionRunner.run",
                os_code: None,
                detail: "the Task Manager starts only as the logged-in user, never from the service".into(),
            }),
        }
    }

    fn keep_awake(&mut self, on: bool) -> Result<()> {
        if self.awake.is_none() {
            if !on {
                return Ok(());
            }
            self.awake = Some(KeepAwake::new()?);
        }
        match &self.awake {
            Some(k) => k.set(on),
            None => Ok(()),
        }
    }
}

/// The real calls.
#[derive(Debug, Clone, Copy, Default)]
pub struct Win32Calls;

/// `SeShutdownPrivilege` is held but disabled in most tokens: turn it on for this process before shutdown calls.
fn enable_shutdown_privilege() -> Result<()> {
    let token = open_process_token(TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY)?;
    let name = wide(OsStr::new("SeShutdownPrivilege"))?;
    // SAFETY: plain data; zeroed is valid and the one entry is filled below.
    let mut privileges: TOKEN_PRIVILEGES = unsafe { core::mem::zeroed() };
    // SAFETY: `name` is a null-terminated wide string; the LUID is a live out-parameter inside `privileges`.
    if unsafe { LookupPrivilegeValueW(null(), name.as_ptr(), &mut privileges.Privileges[0].Luid) } == 0 {
        return Err(last_error("LookupPrivilegeValueW"));
    }
    privileges.PrivilegeCount = 1;
    privileges.Privileges[0] =
        LUID_AND_ATTRIBUTES { Luid: privileges.Privileges[0].Luid, Attributes: SE_PRIVILEGE_ENABLED };
    // SAFETY: the token has TOKEN_ADJUST_PRIVILEGES; `privileges` holds one entry; the previous state is not requested.
    if unsafe { AdjustTokenPrivileges(token.0, 0, &privileges, 0, null_mut(), null_mut()) } == 0 {
        return Err(last_error("AdjustTokenPrivileges"));
    }
    // AdjustTokenPrivileges succeeds with ERROR_NOT_ALL_ASSIGNED when the token does not hold the privilege at all.
    // SAFETY: GetLastError has no preconditions.
    let not_all = unsafe { windows_sys::Win32::Foundation::GetLastError() }
        == windows_sys::Win32::Foundation::ERROR_NOT_ALL_ASSIGNED;
    if not_all {
        return Err(PlatformError {
            kind: ErrorKind::PermissionDenied(swapdesk_platform::OsGrant::Administrator),
            op: "AdjustTokenPrivileges",
            os_code: None,
            detail: "this account may not shut the machine down".into(),
        });
    }
    Ok(())
}

impl SystemCalls for Win32Calls {
    fn lock_workstation(&mut self) -> Result<()> {
        // SAFETY: no preconditions; it fails when the caller is not on the interactive desktop.
        if unsafe { LockWorkStation() } == 0 { Err(last_error("LockWorkStation")) } else { Ok(()) }
    }

    fn lock_console_session(&mut self) -> Result<()> {
        let Some(session) = active_console_session() else { return Ok(()) };
        // SAFETY: a null server handle is the local server; the session id came from the OS; do not wait.
        if unsafe { WTSDisconnectSession(null_mut(), session, 0) } == 0 {
            Err(last_error("WTSDisconnectSession"))
        } else {
            Ok(())
        }
    }

    fn console_user_present(&mut self) -> bool {
        active_console_session().is_some_and(|s| session_user_sid(s).is_ok())
    }

    fn suspend(&mut self) -> Result<()> {
        enable_shutdown_privilege()?;
        // SAFETY: no preconditions; sleep (not hibernate), no force, wake events enabled.
        if !unsafe { SetSuspendState(false, false, false) } { Err(last_error("SetSuspendState")) } else { Ok(()) }
    }

    fn reboot(&mut self) -> Result<()> {
        enable_shutdown_privilege()?;
        // SAFETY: a null machine name is this machine, a null message shows none; no delay; applications are asked to close,
        // not forced (no data loss); restart.
        let ok = unsafe {
            InitiateSystemShutdownExW(null(), null_mut(), 0, 0, 1, SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED)
        };
        if ok == 0 { Err(last_error("InitiateSystemShutdownExW")) } else { Ok(()) }
    }

    fn launch_task_manager(&mut self) -> Result<()> {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let exe = std::path::PathBuf::from(root).join(r"System32\taskmgr.exe");
        let app = wide(exe.as_os_str())?;
        let mut cmd = wide(&{
            let mut s = std::ffi::OsString::from("\"");
            s.push(exe.as_os_str());
            s.push("\"");
            s
        })?;
        // SAFETY: plain data; zeroed is valid, `cb` is set below.
        let mut si: STARTUPINFOW = unsafe { core::mem::zeroed() };
        si.cb = size_of::<STARTUPINFOW>() as u32;
        // SAFETY: plain data.
        let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: `app` and `cmd` (writable) are null-terminated wide strings; no environment or directory override; `si`, `pi` are live.
        let ok = unsafe {
            CreateProcessW(app.as_ptr(), cmd.as_mut_ptr(), null(), null(), 0, 0, null(), null(), &si, &mut pi)
        };
        if ok == 0 {
            return Err(last_error("CreateProcessW(taskmgr)"));
        }
        drop(OwnedHandle(pi.hThread));
        drop(OwnedHandle(pi.hProcess));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what would have happened; nothing touches the machine.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<&'static str>,
        user_present: bool,
        fail: Option<&'static str>,
    }

    impl Recorder {
        fn go(&mut self, name: &'static str) -> Result<()> {
            self.calls.push(name);
            if self.fail == Some(name) {
                Err(PlatformError { kind: ErrorKind::Os, op: name, os_code: Some(5), detail: "denied".into() })
            } else {
                Ok(())
            }
        }
    }

    impl SystemCalls for Recorder {
        fn lock_workstation(&mut self) -> Result<()> {
            self.go("lock_workstation")
        }
        fn lock_console_session(&mut self) -> Result<()> {
            self.go("lock_console_session")
        }
        fn console_user_present(&mut self) -> bool {
            self.user_present
        }
        fn suspend(&mut self) -> Result<()> {
            self.go("suspend")
        }
        fn reboot(&mut self) -> Result<()> {
            self.go("reboot")
        }
        fn launch_task_manager(&mut self) -> Result<()> {
            self.go("launch_task_manager")
        }
    }

    fn actions(scope: Scope, user_present: bool) -> WinActions<Recorder> {
        WinActions::with_calls(Recorder { user_present, ..Recorder::default() }, scope)
    }

    #[test]
    fn the_user_helper_runs_every_action_with_the_users_rights() {
        let mut a = actions(Scope::User, true);
        for action in [Action::Lock, Action::Sleep, Action::Restart, Action::TaskMonitor] {
            a.run(action).unwrap();
        }
        assert_eq!(a.calls().calls, ["lock_workstation", "suspend", "reboot", "launch_task_manager"]);
    }

    #[test]
    fn the_service_makes_system_calls_and_never_starts_the_task_manager() {
        let mut a = actions(Scope::Service, true);
        a.run(Action::Lock).unwrap();
        a.run(Action::Sleep).unwrap();
        a.run(Action::Restart).unwrap();
        let e = a.run(Action::TaskMonitor).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unsupported);
        assert_eq!(a.calls().calls, ["lock_console_session", "suspend", "reboot"], "no process was started");
    }

    #[test]
    fn the_service_has_nothing_to_lock_at_the_logon_screen() {
        let mut a = actions(Scope::Service, false);
        a.run(Action::Lock).unwrap();
        assert!(a.calls().calls.is_empty());
    }

    #[test]
    fn an_os_failure_is_returned_not_swallowed() {
        let mut a = actions(Scope::User, true);
        a.calls.fail = Some("reboot");
        let e = a.run(Action::Restart).unwrap_err();
        assert_eq!((e.kind, e.os_code), (ErrorKind::Os, Some(5)));
    }

    #[test]
    fn keep_awake_turns_on_and_off_and_off_first_is_a_no_op() {
        let mut a = actions(Scope::User, true);
        a.keep_awake(false).unwrap();
        assert!(a.awake.is_none(), "turning off what was never on starts no thread");
        a.keep_awake(true).unwrap();
        assert!(a.is_awake_requested());
        a.keep_awake(false).unwrap();
        assert!(!a.is_awake_requested());
    }
}
