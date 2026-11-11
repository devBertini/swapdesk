//! Windows' [`Elevator`] (ELEVATION.md 3.5): starts this same program as an administrator through UAC with the
//! ticket on its command line, waits for it and reports how it ended. The service decides the outcome (op 38); the
//! run only says why a failure failed.
//!
//! The program started is `std::env::current_exe()`, checked with the install policy of C2.1 first (under
//! `%ProgramFiles%`, the file and its folder writable only by SYSTEM, Administrators and TrustedInstaller): a UAC
//! prompt that names a program an ordinary process could have replaced would approve whatever was put there. The
//! check is skipped only in a development build started with `--service-pid`, whose program lives in a build folder.
//!
//! The launcher is a function value, so tests replace the real `ShellExecuteExW` call, which needs a person at a
//! UAC prompt.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use swapdesk_ipc::{ElevAction, ElevationTicket};
use swapdesk_sys_win::elevate::{ElevatedProcess, RunAsError, WaitOutcome};
use swapdesk_sys_win::{InstallPolicy, quote_arg};
use swapdesk_ui::ApiCode;
use swapdesk_ui::elevated_client::{self, ElevatedExit};
use swapdesk_ui::service_backend::{Elevator, ElevatorRun};

use crate::cli::{self, Options};
use crate::connect::{self, ConnectError};

/// A started elevated process, as far as the elevator needs it.
pub trait Started: Send {
    /// Waits up to `timeout` for the process to end.
    fn wait(&self, timeout: Duration) -> WaitOutcome;
}

impl Started for ElevatedProcess {
    fn wait(&self, timeout: Duration) -> WaitOutcome {
        ElevatedProcess::wait(self, timeout)
    }
}

/// Starts `exe` elevated with the command line after the program name already quoted.
pub type Launcher = Box<dyn Fn(&Path, &str) -> Result<Box<dyn Started>, RunAsError> + Send + Sync>;

/// The real launcher: `ShellExecuteExW` with the `runas` verb, no owner window.
pub fn run_as_admin_launcher() -> Launcher {
    Box::new(|exe, params| swapdesk_sys_win::run_as_admin(exe, params, None).map(|p| Box::new(p) as Box<dyn Started>))
}

/// How the program to start is checked before the launcher gets it.
#[derive(Debug, Clone)]
pub enum ExeCheck {
    /// The install policy of C2.1.
    Policy(InstallPolicy),
    /// A development build started with `--service-pid`: its program lives in a build folder.
    Skipped,
    /// The policy could not even be built (the reason): nothing is launched.
    Unavailable(String),
}

pub struct WinElevator {
    exe: PathBuf,
    check: ExeCheck,
    service_pid: Option<u32>,
    instance: Option<u32>,
    launch: Launcher,
}

impl WinElevator {
    /// The elevator of this process: its own executable, the production policy unless this is a development build
    /// started with `--service-pid`, and the real launcher. `--service-pid` and `--instance` go to the instance.
    pub fn new(options: &Options) -> Self {
        let check = if cli::DEV_FLAGS && options.service_pid.is_some() {
            ExeCheck::Skipped
        } else {
            match InstallPolicy::production() {
                Ok(policy) => ExeCheck::Policy(policy),
                Err(e) => ExeCheck::Unavailable(e.to_string()),
            }
        };
        let exe = std::env::current_exe().unwrap_or_default();
        Self::with(exe, check, options, run_as_admin_launcher())
    }

    /// An elevator over any program, check and launcher (tests).
    pub fn with(exe: PathBuf, check: ExeCheck, options: &Options, launch: Launcher) -> Self {
        Self { exe, check, service_pid: options.service_pid, instance: options.instance, launch }
    }

    /// The program the check lets through, or why not.
    fn verified_exe(&self) -> Result<PathBuf, String> {
        match &self.check {
            ExeCheck::Policy(policy) => {
                policy.verify(&self.exe).map(|v| v.path().to_path_buf()).map_err(|e| e.to_string())
            }
            ExeCheck::Skipped => Ok(self.exe.clone()),
            ExeCheck::Unavailable(why) => Err(why.clone()),
        }
    }

    /// The command line after the program name: the ticket, then the development flags this process was given.
    fn params(&self, ticket: &ElevationTicket) -> String {
        let mut args = vec!["--elevated".to_owned(), cli::ticket_hex(ticket)];
        if let Some(pid) = self.service_pid {
            args.extend(["--service-pid".to_owned(), pid.to_string()]);
        }
        if let Some(n) = self.instance {
            args.extend(["--instance".to_owned(), n.to_string()]);
        }
        args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ")
    }
}

/// The ticket's life (120 s, ELEVATION.md 3.1) minus how long the asking interface waits for the run
/// (`swapdesk_ui::service_backend::elevation::RUN_FOR`, 110 s).
pub const WINDOW_MARGIN_S: u16 = 10;

/// The person's answer, from the instance's own window (ELEVATION.md 3.5): it shows the action as the service holds
/// it, in this process's display language, with "Cancelar" and "Confirmar", and closes as declined when the
/// ticket's time runs out. Only a real click or key on "Confirmar" approves; a window that cannot be shown
/// declines.
pub fn confirm_in_window(action: &ElevAction, peer_name: &str, remaining_s: u16) -> bool {
    let lang = swapdesk_shell_win::system_language();
    let line = elevated_client::action_line(lang, action, peer_name);
    // The service's seconds are the ticket's (120 s); the asking interface gives up 10 s earlier (`RUN_FOR`, 110 s),
    // so the window ends with it and never offers "Confirmar" for a step already shown as timed out.
    let remaining_s = remaining_s.saturating_sub(WINDOW_MARGIN_S);
    let state = swapdesk_ui::UiState::for_elevated_confirm(lang, swapdesk_shell_win::system_dark(), line, remaining_s);
    match swapdesk_shell_win::run_confirm(state) {
        Ok(answer) => answer == Some(true),
        Err(e) => {
            trace(&format!("the confirmation window could not be shown: {e}"));
            false
        }
    }
}

/// How a failure to open the pipe ends the instance: the system refusing this process is its own code, anything else
/// (no service, no session, a server that is not the service, a handshake that fails) is "cannot be reached".
fn exit_of_connect(e: &ConnectError) -> ElevatedExit {
    match e {
        ConnectError::Refused(_) => ElevatedExit::Refused,
        ConnectError::NoService(_) | ConnectError::Session(_) | ConnectError::Pipe(_) | ConnectError::Handshake(_) => {
            ElevatedExit::Unreachable
        }
    }
}

/// The elevated instance (`--elevated <ticket>`): opens the UI pipe of this session with the same check of the
/// server as the normal interface, runs the instance's side of the conversation as role `Elevator` and returns the
/// process exit code (ELEVATION.md 3.5). It starts nothing else: no viewer, no preferences, no commands.
pub fn run_instance(
    options: &Options,
    ticket: &ElevationTicket,
    confirm: impl FnOnce(&ElevAction, &str, u16) -> bool,
) -> u8 {
    let conn = match connect::open_pipe(options) {
        Ok(c) => c,
        Err(e) => {
            trace(&format!("instance: {e}"));
            return exit_of_connect(&e).code();
        }
    };
    let (reader, writer) = match conn.split() {
        Ok(rw) => rw,
        Err(e) => {
            trace(&format!("instance: {e}"));
            return ElevatedExit::Unreachable.code();
        }
    };
    elevated_client::run(reader, writer, ticket, confirm).code()
}

/// One line to the debugger output. Never the ticket.
fn trace(line: &str) {
    swapdesk_shell_win::trace_line(&format!("swapdesk: elevate: {line}"));
}

impl Elevator for WinElevator {
    fn run(&self, ticket: &ElevationTicket, _line: &str, deadline: Instant) -> ElevatorRun {
        let exe = match self.verified_exe() {
            Ok(exe) => exe,
            Err(why) => {
                trace(&format!("the program is not in a protected folder, nothing is started: {why}"));
                return ElevatorRun::Failed(ApiCode::Internal);
            }
        };
        let process = match (self.launch)(&exe, &self.params(ticket)) {
            Ok(p) => p,
            Err(RunAsError::Declined) => return ElevatorRun::Declined,
            Err(RunAsError::Os(e)) => {
                trace(&format!("the launch failed: {e}"));
                return ElevatorRun::Failed(ApiCode::Internal);
            }
        };
        match process.wait(deadline.saturating_duration_since(Instant::now())) {
            WaitOutcome::Ended { exit_code } => ElevatorRun::Ended { exit_code },
            WaitOutcome::TimedOut => ElevatorRun::TimedOut,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use swapdesk_platform::{ErrorKind, PlatformError};

    use super::*;

    const TICKET: ElevationTicket = ElevationTicket([
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
    ]);
    const HEX: &str = "00112233445566778899aabbccddeeff";

    /// A process that ended, or runs until the wait times out; it records the timeout it was asked to wait.
    struct Fake {
        ends: WaitOutcome,
        asked: Arc<Mutex<Vec<Duration>>>,
    }

    impl Started for Fake {
        fn wait(&self, timeout: Duration) -> WaitOutcome {
            self.asked.lock().unwrap().push(timeout);
            self.ends
        }
    }

    /// What the fake launcher was called with.
    type Calls = Arc<Mutex<Vec<(PathBuf, String)>>>;

    fn launcher(result: Result<WaitOutcome, RunAsError>) -> (Launcher, Calls, Arc<Mutex<Vec<Duration>>>) {
        let calls: Calls = Arc::default();
        let asked: Arc<Mutex<Vec<Duration>>> = Arc::default();
        let (c, a) = (calls.clone(), asked.clone());
        let launch: Launcher = Box::new(move |exe, params| {
            c.lock().unwrap().push((exe.to_path_buf(), params.to_owned()));
            match &result {
                Ok(ends) => Ok(Box::new(Fake { ends: *ends, asked: a.clone() }) as Box<dyn Started>),
                Err(e) => Err(e.clone()),
            }
        });
        (launch, calls, asked)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// A program the real install policy lets through: `cmd.exe` in System32, owned by TrustedInstaller and writable
    /// only by SYSTEM, Administrators and TrustedInstaller. Returns the policy and the program's canonical path.
    fn protected_exe() -> (InstallPolicy, PathBuf) {
        let root = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        let policy = InstallPolicy {
            root: root.clone(),
            trusted_sids: vec![
                "S-1-5-18".into(),
                "S-1-5-32-544".into(),
                swapdesk_sys_win::TRUSTED_INSTALLER_SID.into(),
            ],
        };
        let exe = std::fs::canonicalize(root.join("cmd.exe")).unwrap();
        (policy, exe)
    }

    #[test]
    fn the_command_line_is_the_verified_exe_with_the_ticket_and_the_dev_flags() {
        let (policy, exe) = protected_exe();
        let options = Options { service_pid: Some(4242), instance: Some(3), ..Options::default() };
        let (launch, calls, _) = launcher(Ok(WaitOutcome::Ended { exit_code: Some(0) }));
        // The program is the one the policy returned (its canonical path), not the text it was given.
        let given = exe.parent().unwrap().join(".").join("cmd.exe");
        let elevator = WinElevator::with(given, ExeCheck::Policy(policy.clone()), &options, launch);
        assert_eq!(elevator.run(&TICKET, "", soon()), ElevatorRun::Ended { exit_code: Some(0) });
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, exe);
            assert_eq!(calls[0].1, format!("--elevated {HEX} --service-pid 4242 --instance 3"));
        }

        // Without the development flags only the ticket goes; it is always 32 lowercase digits.
        let (launch, calls, _) = launcher(Ok(WaitOutcome::Ended { exit_code: Some(0) }));
        WinElevator::with(exe, ExeCheck::Policy(policy), &Options::default(), launch).run(&TICKET, "", soon());
        assert_eq!(calls.lock().unwrap()[0].1, format!("--elevated {HEX}"));
        // A development build started with `--service-pid` skips the check and gets its own path as it is.
        let (launch, calls, _) = launcher(Ok(WaitOutcome::Ended { exit_code: Some(0) }));
        let dev = Options { service_pid: Some(7), ..Options::default() };
        let built = PathBuf::from(r"D:\build\Swapdesk.exe");
        WinElevator::with(built.clone(), ExeCheck::Skipped, &dev, launch).run(&TICKET, "", soon());
        assert_eq!(calls.lock().unwrap()[0].0, built);
        // What goes on the command line is what the instance's own parser takes back.
        let parsed = cli::parse(&["--elevated".to_owned(), HEX.to_owned()]).unwrap();
        assert_eq!(parsed.elevated, Some(TICKET));
    }

    #[test]
    fn exit_codes_and_uac_cancel_map_to_api_codes() {
        let exe = PathBuf::from(r"C:\Program Files\Swapdesk\Swapdesk.exe");
        let run = |result: Result<WaitOutcome, RunAsError>| {
            let (launch, _, _) = launcher(result);
            WinElevator::with(exe.clone(), ExeCheck::Skipped, &Options::default(), launch).run(&TICKET, "", soon())
        };
        // UAC "No" or a closed credentials prompt.
        assert_eq!(run(Err(RunAsError::Declined)), ElevatorRun::Declined);
        // Any other launch error.
        let os = PlatformError { kind: ErrorKind::Os, op: "ShellExecuteExW", os_code: Some(2), detail: "x".into() };
        assert_eq!(run(Err(RunAsError::Os(os))), ElevatorRun::Failed(ApiCode::Internal));
        // Every exit code of the instance, and none readable, reaches the worker as it is: the worker maps them.
        for code in 0..=8 {
            assert_eq!(
                run(Ok(WaitOutcome::Ended { exit_code: Some(code) })),
                ElevatorRun::Ended { exit_code: Some(code) }
            );
        }
        assert_eq!(run(Ok(WaitOutcome::Ended { exit_code: None })), ElevatorRun::Ended { exit_code: None });
        assert_eq!(run(Ok(WaitOutcome::TimedOut)), ElevatorRun::TimedOut);
    }

    #[test]
    fn the_instance_ends_with_the_codes_of_the_note_when_the_pipe_cannot_be_used() {
        assert_eq!(exit_of_connect(&ConnectError::Refused("x".into())).code(), 4);
        for e in [
            ConnectError::NoService("x".into()),
            ConnectError::Session("x".into()),
            ConnectError::Pipe("x".into()),
            ConnectError::Handshake("x".into()),
        ] {
            assert_eq!(exit_of_connect(&e).code(), 3);
        }
        // A pipe that does not exist (no such development instance): "cannot be reached", at once, nothing asked.
        let options = Options { service_pid: Some(std::process::id()), instance: Some(987_655), ..Options::default() };
        let asked = std::cell::Cell::new(false);
        let code = run_instance(&options, &TICKET, |_, _, _| {
            asked.set(true);
            true
        });
        assert_eq!(code, 3);
        assert!(!asked.get());
    }

    #[test]
    fn the_wait_is_what_is_left_of_the_deadline() {
        let exe = PathBuf::from(r"C:\Program Files\Swapdesk\Swapdesk.exe");
        let (launch, _, asked) = launcher(Ok(WaitOutcome::TimedOut));
        let elevator = WinElevator::with(exe, ExeCheck::Skipped, &Options::default(), launch);
        elevator.run(&TICKET, "", Instant::now() + Duration::from_secs(100));
        // A deadline already behind asks for no wait at all.
        elevator.run(&TICKET, "", Instant::now() - Duration::from_secs(1));
        let asked = asked.lock().unwrap();
        assert!(asked[0] > Duration::from_secs(90) && asked[0] <= Duration::from_secs(100));
        assert_eq!(asked[1], Duration::ZERO);
    }

    #[test]
    fn an_unprotected_exe_is_never_launched() {
        let (launch, calls, _) = launcher(Ok(WaitOutcome::Ended { exit_code: Some(0) }));
        // Under a folder anyone may write to: a temp folder is outside %ProgramFiles%, and one the policy takes as its
        // root is refused for its writers. The program does not even have to exist for the refusal.
        let dir = std::env::temp_dir().join(format!("swapdesk-elevate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("Swapdesk.exe");
        std::fs::write(&exe, b"not a program").unwrap();
        let policy = InstallPolicy { root: dir.clone(), trusted_sids: vec!["S-1-5-18".into(), "S-1-5-32-544".into()] };
        let elevator = WinElevator::with(exe.clone(), ExeCheck::Policy(policy), &Options::default(), launch);
        // Inside the root, but written by an account the policy does not trust (or owned by one).
        let why = elevator.verified_exe().unwrap_err();
        assert!(why.contains("grants write access") || why.contains("is owned by"), "{why}");
        assert_eq!(elevator.run(&TICKET, "", soon()), ElevatorRun::Failed(ApiCode::Internal));
        // The production policy refuses the same file as outside %ProgramFiles%.
        let (launch2, calls2, _) = launcher(Ok(WaitOutcome::Ended { exit_code: Some(0) }));
        let production = ExeCheck::Policy(InstallPolicy::production().unwrap());
        let elevator = WinElevator::with(exe, production, &Options::default(), launch2);
        assert!(elevator.verified_exe().unwrap_err().contains("outside the install root"));
        assert_eq!(elevator.run(&TICKET, "", soon()), ElevatorRun::Failed(ApiCode::Internal));
        // A file that does not exist, and a policy that could not be built, refuse too.
        let (launch3, calls3, _) = launcher(Ok(WaitOutcome::Ended { exit_code: Some(0) }));
        let missing = ExeCheck::Policy(InstallPolicy::production().unwrap());
        let elevator = WinElevator::with(dir.join("missing.exe"), missing, &Options::default(), launch3);
        assert!(elevator.verified_exe().is_err());
        assert_eq!(elevator.run(&TICKET, "", soon()), ElevatorRun::Failed(ApiCode::Internal));
        let (launch4, calls4, _) = launcher(Ok(WaitOutcome::Ended { exit_code: Some(0) }));
        let elevator = WinElevator::with(
            dir.join("Swapdesk.exe"),
            ExeCheck::Unavailable("no".into()),
            &Options::default(),
            launch4,
        );
        assert_eq!(elevator.run(&TICKET, "", soon()), ElevatorRun::Failed(ApiCode::Internal));
        let _ = std::fs::remove_dir_all(&dir);
        for c in [calls, calls2, calls3, calls4] {
            assert!(c.lock().unwrap().is_empty());
        }
    }
}
