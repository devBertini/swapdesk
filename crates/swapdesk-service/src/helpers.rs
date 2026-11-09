//! The helpers' supervisor (C2.1): starts `swapdesk-helper.exe --role system` and `--role user` in the console
//! session on a fresh pipe each, attaches them to the core, and restarts them with the backoff 1, 2, 4 ... 30 s
//! (reset after 60 s of uptime). Called from the service's one supervisor thread; it never blocks for long (a
//! start waits at most 10 s for the helper to connect and 5 s for its `Hello`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use swapdesk_host::service::{HelperKind, ServiceCore};
use swapdesk_ipc::IpcRole;
use swapdesk_platform::{Level, TraceSink};
use swapdesk_sys_win::{
    ChildProcess, HelperRole, InstallPolicy, PipeAcl, PipeConn, PipeListener, PipeSpec, RestartBackoff,
    helper_pipe_name, launch_helper, launch_helper_dev, random_hex_128,
};

use crate::ui_pipe::session_user_sid;

/// How long a started helper has to open its pipe.
const CONNECT_WITHIN: Duration = Duration::from_secs(10);
/// How long a connected helper has to send `Hello`.
const HELLO_WITHIN: Duration = Duration::from_secs(5);

/// How helpers are started.
pub enum Launch {
    /// `--console`: `CreateProcessW` as the current user, from the exe's own folder.
    Dev,
    /// The service: the tokens of C2.1, only from the install folder.
    Production(InstallPolicy),
}

struct Running {
    child: ChildProcess,
    started: Instant,
    /// A second owner of the pipe handle, to cancel its blocked read at shutdown.
    cancel: PipeConn,
}

struct Slot {
    role: HelperRole,
    ipc: IpcRole,
    running: Option<Running>,
    backoff: RestartBackoff,
    /// When the next start may happen.
    due: Option<Instant>,
}

pub struct Supervisor {
    core: Arc<ServiceCore>,
    trace: Arc<dyn TraceSink>,
    launch: Launch,
    suffix: String,
    exe: PathBuf,
    slots: [Slot; 2],
    session: Option<u32>,
    user_present: bool,
}

fn slot(role: HelperRole, ipc: IpcRole) -> Slot {
    Slot { role, ipc, running: None, backoff: RestartBackoff::new(), due: Some(Instant::now()) }
}

impl Supervisor {
    pub fn new(
        core: Arc<ServiceCore>,
        trace: Arc<dyn TraceSink>,
        launch: Launch,
        suffix: String,
        exe: PathBuf,
        session: Option<u32>,
        user_present: bool,
    ) -> Self {
        Self {
            core,
            trace,
            launch,
            suffix,
            exe,
            slots: [slot(HelperRole::System, IpcRole::HelperSystem), slot(HelperRole::User, IpcRole::HelperUser)],
            session,
            user_present,
        }
    }

    fn log(&self, level: Level, what: core::fmt::Arguments<'_>) {
        self.trace.log(level, "supervisor", what);
    }

    /// The console session or its user changed: both helpers go and come back in the new one at once.
    pub fn console_changed(&mut self, session: Option<u32>, user_present: bool) {
        self.log(
            Level::Info,
            format_args!("console session {session:?}, user present {user_present}: restarting the helpers"),
        );
        self.session = session;
        self.user_present = user_present;
        for s in &mut self.slots {
            if let Some(r) = s.running.take() {
                end(r);
            }
            s.backoff.reset();
            s.due = Some(Instant::now());
        }
    }

    /// A helper's pipe closed (the core said so): the slot whose helper this was waits out its backoff.
    pub fn helper_gone(&mut self, pid: u32) {
        let now = Instant::now();
        for s in &mut self.slots {
            if s.running.as_ref().is_some_and(|r| r.child.pid() == pid) {
                let r = s.running.take().expect("checked");
                let delay = s.backoff.after_exit(now.duration_since(r.started));
                end(r);
                s.due = Some(now + delay);
                self.trace.log(
                    Level::Warn,
                    "supervisor",
                    format_args!("{} helper (pid {pid}) is gone; restart in {} s", s.role.as_str(), delay.as_secs()),
                );
            }
        }
    }

    /// A helper that exited without its pipe being noticed (or one that never connected), then every start that is due.
    pub fn tick(&mut self) {
        let now = Instant::now();
        for s in &mut self.slots {
            if let Some(r) = &s.running
                && let Ok(Some(code)) = r.child.try_wait()
            {
                let r = s.running.take().expect("checked");
                let delay = s.backoff.after_exit(now.duration_since(r.started));
                end(r);
                s.due = Some(now + delay);
                self.trace.log(
                    Level::Warn,
                    "supervisor",
                    format_args!("{} helper exited with {code}; restart in {} s", s.role.as_str(), delay.as_secs()),
                );
            }
        }
        let Some(session) = self.session.filter(|_| !self.slots.iter().all(|s| s.running.is_some())) else { return };
        for i in 0..self.slots.len() {
            let (due, role) = (self.slots[i].due, self.slots[i].role);
            if self.slots[i].running.is_some() || due.is_none_or(|d| d > now) {
                continue;
            }
            // The user helper has no user to run as at the login screen.
            if role == HelperRole::User && !self.user_present {
                self.slots[i].due = Some(now + Duration::from_millis(500));
                continue;
            }
            match self.start(i, session) {
                Ok(r) => {
                    self.log(
                        Level::Info,
                        format_args!("{} helper started (pid {}) in session {session}", role.as_str(), r.child.pid()),
                    );
                    self.slots[i].running = Some(r);
                    self.slots[i].due = None;
                }
                Err(e) => {
                    let delay = self.slots[i].backoff.next_delay();
                    self.log(
                        Level::Warn,
                        format_args!("cannot start the {} helper: {e}; retry in {} s", role.as_str(), delay.as_secs()),
                    );
                    self.slots[i].due = Some(now + delay);
                }
            }
        }
    }

    fn start(&self, i: usize, session: u32) -> Result<Running, String> {
        let (role, ipc) = (self.slots[i].role, self.slots[i].ipc);
        let hex = random_hex_128().map_err(|e| e.to_string())?;
        let name = format!("{}{}", helper_pipe_name(role, session, &hex), self.suffix);
        let acl = match (&self.launch, role) {
            (Launch::Dev, _) => PipeAcl::dev_only(),
            (Launch::Production(_), HelperRole::System) => PipeAcl::system_only(),
            (Launch::Production(_), HelperRole::User) => PipeAcl::system_and_user(session_user_sid(session)?),
        };
        // One instance, created first: a squatted name fails here and nothing is launched.
        let mut listener = PipeListener::bind(PipeSpec::helper(name.clone(), acl)).map_err(|e| e.to_string())?;
        let args = vec![
            "--role".to_string(),
            role.as_str().to_string(),
            "--pipe".to_string(),
            name,
            "--server-pid".to_string(),
            std::process::id().to_string(),
        ];
        let child = match &self.launch {
            Launch::Dev => launch_helper_dev(&self.exe, &args),
            Launch::Production(policy) => {
                let verified = policy.verify(&self.exe).map_err(|e| format!("install check: {e:?}"))?;
                launch_helper(role, &verified, &args, session)
            }
        }
        .map_err(|e| e.to_string())?;
        let connected = (|| {
            let conn = listener.accept(Some(CONNECT_WITHIN)).map_err(|e| e.to_string())?;
            // C2.1: the client PID must be the PID the service launched.
            let client = conn.client_pid().map_err(|e| e.to_string())?;
            if client != child.pid() {
                return Err(format!("the pipe client is process {client}, not the launched {}", child.pid()));
            }
            Ok((conn, client))
        })();
        let (conn, verified_pid) = match connected {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                return Err(e);
            }
        };
        let cancel = conn.try_clone().map_err(|e| e.to_string())?;
        let (reader, writer) = conn.split().map_err(|e| e.to_string())?;
        // The handshake reads on the reader the core keeps afterwards, so a timeout cannot be a read timeout: a
        // watchdog cancels the blocked read if `Hello` does not come.
        let done = Arc::new(AtomicBool::new(false));
        {
            let (done, watch) = (Arc::clone(&done), cancel.try_clone().map_err(|e| e.to_string())?);
            std::thread::Builder::new()
                .name("helper-hello-watchdog".into())
                .spawn(move || {
                    let end = Instant::now() + HELLO_WITHIN;
                    while Instant::now() < end {
                        if done.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    if !done.load(Ordering::Acquire) {
                        watch.cancel_io();
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        // The core records the PID verified above, never the one the helper writes in its `Hello`.
        let attached = self.core.attach_helper_pid(ipc, verified_pid, reader, writer);
        done.store(true, Ordering::Release);
        match attached {
            Ok(_) => Ok(Running { child, started: Instant::now(), cancel }),
            Err(e) => {
                let _ = child.kill();
                Err(e.to_string())
            }
        }
    }

    /// The service stops: the core already told the helpers to shut down; give them a moment, then end what is left.
    pub fn stop_all(&mut self) {
        for s in &mut self.slots {
            if let Some(r) = s.running.take() {
                if let Some(link) = HelperKind::of_role(s.ipc).and_then(|k| self.core.helper(k)) {
                    link.send_now(swapdesk_ipc::Message::Shutdown);
                }
                let _ = r.child.wait(Duration::from_secs(2));
                end(r);
            }
        }
    }
}

/// Ends a helper that is still running and unblocks its pipe.
fn end(r: Running) {
    if r.child.try_wait().ok().flatten().is_none() {
        let _ = r.child.kill();
    }
    r.cancel.cancel_io();
}
