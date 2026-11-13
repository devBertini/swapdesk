//! The helpers' supervisor (C2.1): starts `swapdesk-helper.exe --role system` and `--role user` in the console
//! session on a fresh pipe each, attaches them to the core, and restarts them with the backoff 1, 2, 4 ... 30 s
//! (reset after 60 s of uptime).
//!
//! Called from the service's one supervisor thread, which also follows the session watcher and the stop request, so
//! it never waits: a start (the helper has up to 10 s to open its pipe and 5 s to say `Hello`) runs on a thread of its
//! own and reports through a channel and the owner's wake-up. The supervisor thread learns when it must look again from
//! [`Supervisor::next_wake`] (the earliest restart that is due) and from the wake-ups, never on a timer of its own.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
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
/// How long `stop_all` waits for a start that is still running before it leaves that start to end itself.
const STOP_WAITS_FOR_START: Duration = Duration::from_secs(3);

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
    /// The core it attached to (its address): when another account's core may host now, it moves (ACCOUNTS.md 5.1).
    core: usize,
    /// A second owner of the pipe handle, to cancel its blocked read at shutdown.
    cancel: PipeConn,
}

/// What a start thread tells the supervisor.
type Started = Result<Running, String>;

/// A start in flight on its own thread.
struct Starting {
    result: Receiver<Started>,
    /// The `Supervisor::epoch` it began under: a console change since then makes its helper the wrong one.
    epoch: u64,
}

struct Slot {
    role: HelperRole,
    ipc: IpcRole,
    running: Option<Running>,
    starting: Option<Starting>,
    backoff: RestartBackoff,
    /// When the next start may happen.
    due: Option<Instant>,
}

/// What the supervisor starts helpers with: how, the pipe name suffix of an instance, and the helper's exe.
pub struct HowToStart {
    pub launch: Launch,
    pub suffix: String,
    pub exe: PathBuf,
}

/// What a start thread needs, without holding the core alive: the service can stop while a start is waiting for its
/// helper to connect.
struct StartEnv {
    core: CoreOf,
    launch: Launch,
    suffix: String,
    exe: PathBuf,
}

/// The core the helpers attach to now: the account that may host (ACCOUNTS.md 15 S3; S4 binds it to the console
/// with the fence). `None`: no account may host, and no helper is attached.
/// How often the supervisor looks again for an account that may host, while there is none.
const NO_ACCOUNT_RECHECK: Duration = Duration::from_secs(1);

pub type CoreOf = Arc<dyn Fn() -> Option<Arc<ServiceCore>> + Send + Sync>;

pub struct Supervisor {
    core: CoreOf,
    trace: Arc<dyn TraceSink>,
    env: Arc<StartEnv>,
    /// Told when a start thread has its result, so the owner's loop looks at the supervisor.
    wake: Arc<dyn Fn() + Send + Sync>,
    slots: [Slot; 2],
    session: Option<u32>,
    user_present: bool,
    /// Bumped when the console session changes and when the service stops.
    epoch: u64,
}

fn slot(role: HelperRole, ipc: IpcRole) -> Slot {
    Slot { role, ipc, running: None, starting: None, backoff: RestartBackoff::new(), due: Some(Instant::now()) }
}

/// The earliest instant a slot's start is due: only for a slot with nothing running or starting. `None` when nothing is
/// waiting for a time (every helper runs or is being started, there is no console session, or the slot waits for a
/// console change instead, as the user helper does with nobody logged in).
fn earliest_due(session: Option<u32>, slots: &[(bool, Option<Instant>)]) -> Option<Instant> {
    session?;
    slots.iter().filter(|(busy, _)| !busy).filter_map(|(_, due)| *due).min()
}

impl Supervisor {
    /// `console` is the console session and whether a user is logged in at it. `wake` is what a start thread calls when it
    /// has its result.
    pub fn new(
        core: CoreOf,
        trace: Arc<dyn TraceSink>,
        how: HowToStart,
        console: (Option<u32>, bool),
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let HowToStart { launch, suffix, exe } = how;
        let (session, user_present) = console;
        let env = Arc::new(StartEnv { core: Arc::clone(&core), launch, suffix, exe });
        Self {
            core,
            trace,
            env,
            wake,
            slots: [slot(HelperRole::System, IpcRole::HelperSystem), slot(HelperRole::User, IpcRole::HelperUser)],
            session,
            user_present,
            epoch: 0,
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
        // A start in flight belongs to the old session: its helper is ended when it reports, and the new start follows.
        self.epoch += 1;
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

    /// When the supervisor must look again for a reason of its own: the earliest restart that is due. The owner sleeps
    /// until then or until something wakes it.
    pub fn next_wake(&self) -> Option<Instant> {
        let slots: Vec<(bool, Option<Instant>)> =
            self.slots.iter().map(|s| (s.running.is_some() || s.starting.is_some(), s.due)).collect();
        earliest_due(self.session, &slots)
    }

    /// Takes the results of finished starts, notices a helper that exited without its pipe being noticed, then starts
    /// every helper that is due on a thread of its own. Never waits.
    pub fn tick(&mut self) {
        let now = Instant::now();
        self.collect_starts(now);
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
        let Some(session) = self.session else { return };
        // A helper attached to a core that may no longer host goes now and starts again on the right one (no backoff:
        // it did not fail).
        let current = (self.core)().map(|c| core_id(&c));
        let attached: Vec<Option<usize>> = self.slots.iter().map(|s| s.running.as_ref().map(|r| r.core)).collect();
        for i in misplaced(&attached, current) {
            if let Some(r) = self.slots[i].running.take() {
                self.trace.log(
                    Level::Info,
                    "supervisor",
                    format_args!("{} helper moves to the account that may host now", self.slots[i].role.as_str()),
                );
                end(r);
                self.slots[i].due = Some(now);
            }
        }
        // No account may host yet (nobody has opened Swapdesk): no helper is started, and the slots look again in a
        // second, without counting it as a failure.
        if (self.core)().is_none() {
            for s in &mut self.slots {
                if s.running.is_none() && s.starting.is_none() && s.due.is_some_and(|d| d <= now) {
                    s.due = Some(now + NO_ACCOUNT_RECHECK);
                }
            }
            return;
        }
        for i in 0..self.slots.len() {
            let (due, role) = (self.slots[i].due, self.slots[i].role);
            if self.slots[i].running.is_some() || self.slots[i].starting.is_some() || due.is_none_or(|d| d > now) {
                continue;
            }
            // The user helper has no user to run as at the login screen: it waits for the console change that brings one.
            if role == HelperRole::User && !self.user_present {
                self.slots[i].due = None;
                continue;
            }
            self.begin_start(i, session);
        }
    }

    fn collect_starts(&mut self, now: Instant) {
        for i in 0..self.slots.len() {
            let Some(starting) = &self.slots[i].starting else { continue };
            let result = match starting.result.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => Err("the start thread ended without a result".to_string()),
            };
            let epoch = starting.epoch;
            let role = self.slots[i].role;
            self.slots[i].starting = None;
            if epoch != self.epoch {
                // Begun under another console session: that helper is the wrong one. A new start follows at once.
                if let Ok(r) = result {
                    end(r);
                }
                self.slots[i].due = Some(now);
                continue;
            }
            match result {
                Ok(r) => {
                    self.log(Level::Info, format_args!("{} helper started (pid {})", role.as_str(), r.child.pid()));
                    self.slots[i].running = Some(Running { started: Instant::now(), ..r });
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

    /// Starts the helper of slot `i` on its own thread: it can take 15 s, and the supervisor thread also follows the
    /// session watcher and the stop request.
    fn begin_start(&mut self, i: usize, session: u32) {
        let (role, ipc) = (self.slots[i].role, self.slots[i].ipc);
        let (tx, rx) = mpsc::channel();
        let (env, wake) = (Arc::clone(&self.env), Arc::clone(&self.wake));
        let spawned = std::thread::Builder::new().name(format!("helper-start-{}", role.as_str())).spawn(move || {
            let result = start_helper(&env, role, ipc, session);
            // The supervisor is gone (the service stopped): nobody takes the helper, so it ends here.
            if let Err(mpsc::SendError(Ok(r))) = tx.send(result) {
                end(r);
            }
            wake();
        });
        match spawned {
            Ok(_) => {
                self.log(Level::Debug, format_args!("starting the {} helper in session {session}", role.as_str()));
                self.slots[i].starting = Some(Starting { result: rx, epoch: self.epoch });
            }
            Err(e) => {
                let delay = self.slots[i].backoff.next_delay();
                self.log(
                    Level::Warn,
                    format_args!(
                        "cannot start a thread for the {} helper: {e}; retry in {} s",
                        role.as_str(),
                        delay.as_secs()
                    ),
                );
                self.slots[i].due = Some(Instant::now() + delay);
            }
        }
    }

    /// The service stops: the core already told the helpers to shut down; give them a moment, then end what is left.
    pub fn stop_all(&mut self) {
        self.epoch += 1;
        for s in &mut self.slots {
            if let Some(starting) = s.starting.take() {
                // A helper that is still connecting is ended here when it reports soon, or by its own thread (the core
                // is gone by then and the thread kills it).
                if let Ok(Ok(r)) = starting.result.recv_timeout(STOP_WAITS_FOR_START) {
                    end(r);
                }
            }
            if let Some(r) = s.running.take() {
                if let Some(link) = HelperKind::of_role(s.ipc).and_then(|k| (self.core)().and_then(|c| c.helper(k))) {
                    link.send_now(swapdesk_ipc::Message::Shutdown);
                }
                let _ = r.child.wait(Duration::from_secs(2));
                end(r);
            }
        }
    }
}

/// Starts one helper and attaches it to the core: the body of a start thread. It may take up to 15 s.
fn start_helper(env: &StartEnv, role: HelperRole, ipc: IpcRole, session: u32) -> Started {
    let hex = random_hex_128().map_err(|e| e.to_string())?;
    let name = format!("{}{}", helper_pipe_name(role, session, &hex), env.suffix);
    let acl = match (&env.launch, role) {
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
    let child = match &env.launch {
        Launch::Dev => launch_helper_dev(&env.exe, &args),
        Launch::Production(policy) => {
            let verified = policy.verify(&env.exe).map_err(|e| format!("install check: {e:?}"))?;
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
    // The handshake reads on the reader the core keeps afterwards, so a timeout cannot be a read timeout: a watchdog
    // cancels the blocked read if `Hello` does not come. It sleeps on a channel until the handshake is over (the sender
    // dropped) or `HELLO_WITHIN` ran out; it never looks at a flag on a timer.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    {
        let watch = cancel.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("helper-hello-watchdog".into())
            .spawn(move || {
                if done_rx.recv_timeout(HELLO_WITHIN) == Err(RecvTimeoutError::Timeout) {
                    watch.cancel_io();
                }
            })
            .map_err(|e| e.to_string())?;
    }
    // The core records the PID verified above, never the one the helper writes in its `Hello`. A core that is gone (the
    // service is stopping) takes no helper.
    let Some(core) = (env.core)() else {
        let _ = child.kill();
        return Err("no account may host now, or the service is stopping".into());
    };
    let attached = core.attach_helper_pid(ipc, verified_pid, reader, writer);
    drop(done_tx);
    match attached {
        Ok(_) => Ok(Running { child, started: Instant::now(), cancel, core: core_id(&core) }),
        Err(e) => {
            let _ = child.kill();
            Err(e.to_string())
        }
    }
}

fn core_id(core: &Arc<ServiceCore>) -> usize {
    Arc::as_ptr(core) as usize
}

/// The slots whose running helper is attached to another core than the one that may host now (review of S3/S4, M4):
/// at the sign-in screen the hosting account changes from A to B, and B's core has no helper until they move. With no
/// core that may host, every running helper is misplaced.
fn misplaced(attached: &[Option<usize>], current: Option<usize>) -> Vec<usize> {
    attached.iter().enumerate().filter(|(_, a)| a.is_some_and(|a| Some(a) != current)).map(|(i, _)| i).collect()
}

/// Ends a helper that is still running and unblocks its pipe.
fn end(r: Running) {
    if r.child.try_wait().ok().flatten().is_none() {
        let _ = r.child.kill();
    }
    r.cancel.cancel_io();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_supervisor_sleeps_until_the_earliest_restart_that_is_due() {
        let t0 = Instant::now();
        let at = |s: u64| Some(t0 + Duration::from_secs(s));
        // A restart is waiting out its backoff: that instant is the only thing to wake for.
        assert_eq!(earliest_due(Some(1), &[(true, None), (false, at(4))]), at(4));
        assert_eq!(earliest_due(Some(1), &[(false, at(9)), (false, at(4))]), at(4));
        // A slot that runs or is starting needs no wake of its own (its end is an event), whatever its stale `due`.
        assert_eq!(earliest_due(Some(1), &[(true, at(1)), (false, at(7))]), at(7));
        // Nothing waits for a time: every helper runs, or the slot waits for a console change (`due` is `None`).
        assert_eq!(earliest_due(Some(1), &[(true, None), (true, None)]), None);
        assert_eq!(earliest_due(Some(1), &[(true, None), (false, None)]), None);
        // No console session: nothing can start, so nothing to wake for.
        assert_eq!(earliest_due(None, &[(false, at(1)), (false, at(2))]), None);
    }

    /// Review of S3/S4, M4: a running helper whose core is no longer the one that may host is moved; one on the right
    /// core stays; with no core that may host, every running helper goes.
    #[test]
    fn a_helper_moves_when_the_hosting_account_changes() {
        assert_eq!(misplaced(&[Some(1), Some(1)], Some(1)), Vec::<usize>::new(), "the right core: they stay");
        assert_eq!(misplaced(&[Some(1), Some(2)], Some(2)), vec![0], "A's helper moves to B");
        assert_eq!(misplaced(&[Some(1), None], Some(2)), vec![0], "a slot with nothing running is not counted");
        assert_eq!(misplaced(&[Some(1), Some(1)], None), vec![0, 1], "nobody may host: both go");
    }
}
