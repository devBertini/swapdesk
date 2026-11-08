//! The helper's side of the terminal proxy (C2.4 "Terminal", stage 43): the service sends `PtyOpen`, `PtyInput`,
//! `PtyResize` and `PtyClose`; this runs the shell through the C3 `PtySpawner` of the user helper and answers
//! `PtyOpenReply`, `PtyOutput` and `PtyExited`.
//!
//! The shell is started by the spawner of this process, so it runs as this helper's user and nobody else: the spawner
//! re-runs the token guard (`check_spawn`, in `swapdesk-sys-win`) with the helper's own token and the console user the
//! service named. Nothing the service sends can widen that: a `PtyOpen` to a system helper is refused, an
//! administrator terminal does not exist on this path, and a refusal of the spawner goes back as its code.
//!
//! The terminals themselves are `swapdesk_term::TermHost`s (the limits of four terminals, sizes and ids, the exit
//! watcher). Two threads serve them: one applies the service's commands in order (a shell takes a moment to start, so
//! the pipe reader never waits for it), one relays the output. The relay waits for room in the helper's outbox, which
//! waits for room in the pipe, which the service empties only as fast as the viewer takes the output: that is the
//! back-pressure, and it reaches the shell's own output pipe.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_ipc::{IpcRole, MAX_TERM_CHUNK, Message, Text};
use swapdesk_platform::{Pty, PtyRequest, PtySpawner, Result as PResult};
use swapdesk_proto::{ErrorCode, PermissionSet, TermOpen};
use swapdesk_term::{OpenContext, TermEvent, TermHost};

use crate::outbox::Outbox;

/// Builds the spawner for a terminal, given the SID of the user the service says is logged in at the console (`None`
/// when it did not say). The Windows helper returns `ConPtySpawner::current_user(Shell::user_default(), session_user)`.
pub type PtyFactory = Arc<dyn Fn(Option<String>) -> Arc<dyn PtySpawner> + Send + Sync>;

/// Commands queued toward the thread that applies them.
const COMMAND_QUEUE: usize = 64;
/// Output chunks a terminal queues before its shell is made to wait.
const OUTPUT_CHUNKS: usize = 8;
/// How often the relay looks at the stop flag while no output arrives.
const RELAY_POLL: Duration = Duration::from_millis(50);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

enum Command {
    Open { id: u32, cols: u16, rows: u16, session_user: String },
    Input { id: u32, bytes: Vec<u8> },
    Resize { id: u32, cols: u16, rows: u16 },
    Close { id: u32 },
}

/// The spawner `TermHost` holds: it builds the real one for the session user of the open in progress.
struct PerOpen {
    factory: PtyFactory,
    user: Mutex<Option<String>>,
}

impl PtySpawner for PerOpen {
    fn spawn(&self, request: &PtyRequest) -> PResult<Box<dyn Pty>> {
        let user = lock(&self.user).clone();
        (self.factory)(user).spawn(request)
    }
}

/// The terminals of one helper. Dropping it (or [`PtyServer::stop`]) closes every shell.
pub(crate) struct PtyServer {
    commands: Option<SyncSender<Command>>,
    outbox: Outbox,
    stop: Arc<AtomicBool>,
    host: Arc<Mutex<TermHost>>,
    threads: Vec<JoinHandle<()>>,
    /// False when this process may not run a user's shell at all (the system helper): every open is refused.
    allowed: bool,
}

impl PtyServer {
    /// `factory`: `None` when the process has no terminal support (a refusal `Unavailable` answers every open).
    pub fn new(factory: Option<PtyFactory>, role: IpcRole, outbox: &Outbox) -> std::io::Result<Self> {
        let allowed = role == IpcRole::HelperUser;
        let factory = factory.unwrap_or_else(|| Arc::new(|_| Arc::new(Unsupported)));
        let spawner = Arc::new(PerOpen { factory, user: Mutex::new(None) });
        let (host, events) = TermHost::new(Arc::clone(&spawner) as Arc<dyn PtySpawner>, OUTPUT_CHUNKS);
        let host = Arc::new(Mutex::new(host));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = sync_channel(COMMAND_QUEUE);
        let mut threads = Vec::new();
        {
            let (host, outbox, spawner) = (Arc::clone(&host), outbox.clone(), Arc::clone(&spawner));
            threads.push(
                std::thread::Builder::new()
                    .name("helper-pty-cmd".into())
                    .spawn(move || apply(&rx, &host, &spawner, &outbox))?,
            );
        }
        {
            let (host, outbox, stop) = (Arc::clone(&host), outbox.clone(), Arc::clone(&stop));
            threads.push(
                std::thread::Builder::new()
                    .name("helper-pty-out".into())
                    .spawn(move || relay(&events, &host, &outbox, &stop))?,
            );
        }
        Ok(Self { commands: Some(tx), outbox: outbox.clone(), stop, host, threads, allowed })
    }

    /// Handles one terminal message of the service. `false` for a message that is not one.
    pub fn handle(&self, msg: Message) -> bool {
        let command = match msg {
            Message::PtyOpen { id, cols, rows, session_user } => {
                if !self.allowed {
                    // A system helper never runs a user's shell: the token guard would refuse it too.
                    self.outbox.send_control(Message::PtyOpenReply {
                        id,
                        code: ErrorCode::NotPermitted,
                        account: Text(String::new()),
                    });
                    return true;
                }
                Command::Open { id, cols, rows, session_user: session_user.0 }
            }
            Message::PtyInput { id, bytes } => Command::Input { id, bytes: bytes.0 },
            Message::PtyResize { id, cols, rows } => Command::Resize { id, cols, rows },
            Message::PtyClose { id } => Command::Close { id },
            _ => return false,
        };
        if let Some(tx) = &self.commands {
            // A closed queue means the server is stopping.
            let _ = tx.send(command);
        }
        true
    }

    /// Ends every shell and joins the threads.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The command thread ends when its queue closes; it never holds the host while it waits.
        self.commands = None;
        lock(&self.host).close_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for PtyServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The spawner of a helper with no terminal support.
struct Unsupported;

impl PtySpawner for Unsupported {
    fn spawn(&self, _request: &PtyRequest) -> PResult<Box<dyn Pty>> {
        Err(swapdesk_platform::PlatformError {
            kind: swapdesk_platform::ErrorKind::Unavailable,
            op: "PtyServer.spawn",
            os_code: None,
            detail: "this helper has no terminal support".into(),
        })
    }
}

fn apply(rx: &Receiver<Command>, host: &Mutex<TermHost>, spawner: &PerOpen, outbox: &Outbox) {
    while let Ok(command) = rx.recv() {
        match command {
            Command::Open { id, cols, rows, session_user } => {
                *lock(&spawner.user) = (!session_user.is_empty()).then_some(session_user);
                // F-TERM-01: `TERMINAL` and a present user are facts of the service's decision already; the helper's own
                // guard is the token check inside the spawner.
                let ctx = OpenContext { granted: PermissionSet::TERMINAL, user_present: true };
                let result = lock(host).open(&TermOpen { term_id: id, cols, rows, admin: false }, &ctx);
                let reply = match result {
                    Ok(opened) => Message::PtyOpenReply { id, code: ErrorCode::NoError, account: Text(opened.account) },
                    Err(code) => Message::PtyOpenReply { id, code, account: Text(String::new()) },
                };
                outbox.send_control(reply);
            }
            Command::Input { id, bytes } => {
                // Never waits: a shell that does not read loses keystrokes past its queue instead of freezing the helper.
                let _ = lock(host).input(id, bytes);
            }
            Command::Resize { id, cols, rows } => {
                let _ = lock(host).resize(id, cols, rows);
            }
            Command::Close { id } => {
                let _ = lock(host).close(id);
            }
        }
    }
}

fn relay(events: &Receiver<TermEvent>, host: &Mutex<TermHost>, outbox: &Outbox, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let event = match events.recv_timeout(RELAY_POLL) {
            Ok(e) => e,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match event {
            TermEvent::Output { term_id, data } => {
                for chunk in data.chunks(MAX_TERM_CHUNK) {
                    if !outbox
                        .send_control(Message::PtyOutput { id: term_id, bytes: swapdesk_ipc::Bytes(chunk.to_vec()) })
                    {
                        return;
                    }
                }
            }
            TermEvent::Exited { term_id, exit_code } => {
                outbox.send_control(Message::PtyExited { id: term_id, code: exit_code });
                lock(host).forget(term_id);
            }
        }
    }
}
