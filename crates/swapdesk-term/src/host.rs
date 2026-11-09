//! The host side of the terminals of one session: PTYs from the C3 `PtySpawner`, their output as events, the viewer's
//! input, resize, close and exit. Blocking threads, bounded channels (C3 "sync, not async"): the network side moves
//! bytes between these channels and the 0x10 streams with `blocking_send` / `blocking_recv`.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use swapdesk_platform::{Pty, PtyRequest, PtySpawner};
use swapdesk_proto::{ErrorCode, PermissionSet, TermOpen, TermOpened};

use crate::wire::code_for;

/// At most this many terminals per session (SPEC F-TERM-01, GUESS).
pub const MAX_TERMINALS: usize = 4;
/// Largest terminal size accepted (columns and rows).
const MAX_DIM: u16 = 1000;
/// One PTY read; also the largest `Output` chunk.
const READ_CHUNK: usize = 8192;
/// Input chunks queued per terminal before the sender must wait.
const INPUT_QUEUE: usize = 64;
/// How soon the exit watcher looks at a shell again, first; the pause doubles up to [`POLL_MAX`] while nothing happens.
/// The PTY trait has no exit notification (`try_wait` only), so a shell that ends without closing its output has to be
/// looked for; what wakes the watcher at once is everything else (the output ending, the terminal being closed).
const POLL_MIN: Duration = Duration::from_millis(25);
const POLL_MAX: Duration = Duration::from_millis(200);
/// How long after the shell exited its last output may still be on its way.
const DRAIN_GRACE: Duration = Duration::from_millis(1500);

/// What the PTY side tells the network side.
#[derive(Clone, PartialEq, Eq)]
pub enum TermEvent {
    /// Bytes for the 0x10 stream of `term_id`.
    Output { term_id: u32, data: Vec<u8> },
    /// The shell ended: the owner sends `TermExited` after the output already queued.
    Exited { term_id: u32, exit_code: Option<i32> },
}

/// The shell's output is as private as typed text: `Debug` shows its size only (CONTRACTS.md C2.3).
impl std::fmt::Debug for TermEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Output { term_id, data } => f
                .debug_struct("Output")
                .field("term_id", term_id)
                .field("data", &swapdesk_proto::RedactedLen(data.len()))
                .finish(),
            Self::Exited { term_id, exit_code } => {
                f.debug_struct("Exited").field("term_id", term_id).field("exit_code", exit_code).finish()
            }
        }
    }
}

/// Facts of the session at the time of a `TermOpen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenContext {
    /// The session's granted permissions (the session core checked them already; this is the second line).
    pub granted: PermissionSet,
    /// A user is logged in at the console (F-TERM-01: without one only an administrator terminal opens).
    pub user_present: bool,
}

/// Where the viewer's bytes for one terminal go; clone it into the stream reader thread and use `send` there.
#[derive(Clone)]
pub struct InputTx(SyncSender<Vec<u8>>);

impl InputTx {
    /// Waits for room: a full queue stops the stream reader, so QUIC flow control reaches the viewer.
    pub fn send(&self, bytes: Vec<u8>) -> Result<(), ErrorCode> {
        self.0.send(bytes).map_err(|_| ErrorCode::NotFound)
    }

    /// Never waits.
    pub fn try_send(&self, bytes: Vec<u8>) -> Result<(), ErrorCode> {
        self.0.try_send(bytes).map_err(|e| match e {
            TrySendError::Full(_) => ErrorCode::Busy,
            TrySendError::Disconnected(_) => ErrorCode::NotFound,
        })
    }
}

/// What the threads of one terminal tell the exit watcher: the output ended, or the terminal is being closed. A flag is
/// set first, then [`Signal::notify`] wakes the watcher, so it never sleeps through either.
#[derive(Clone, Default)]
struct Signal(Arc<(Mutex<()>, Condvar)>);

impl Signal {
    fn notify(&self) {
        let (m, cv) = &*self.0;
        // Taking the lock makes the wake-up come after the flag for a watcher that is between its check and its wait.
        drop(lock(m));
        cv.notify_all();
    }

    /// Sleeps up to `timeout`, or until `stop()` is true (checked under the lock, so a `notify` after the flag is not
    /// missed). Returns whether `stop()` held.
    fn sleep_until(&self, timeout: Duration, stop: impl Fn() -> bool) -> bool {
        let (m, cv) = &*self.0;
        let guard = lock(m);
        let (_guard, _) = cv.wait_timeout_while(guard, timeout, |()| !stop()).unwrap_or_else(|p| p.into_inner());
        stop()
    }
}

struct Live {
    admin: bool,
    pty: Arc<Mutex<Box<dyn Pty>>>,
    input: InputTx,
    closing: Arc<AtomicBool>,
    signal: Signal,
}

pub struct TermHost {
    spawner: Arc<dyn PtySpawner>,
    events: SyncSender<TermEvent>,
    terms: HashMap<u32, Live>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl TermHost {
    /// A host of terminals; its events arrive on the returned receiver, a channel of `event_capacity` chunks (at least 1).
    /// A full channel stops the PTY readers, which is the back-pressure from the network.
    pub fn new(spawner: Arc<dyn PtySpawner>, event_capacity: usize) -> (TermHost, Receiver<TermEvent>) {
        let (tx, rx) = sync_channel(event_capacity.max(1));
        (TermHost { spawner, events: tx, terms: HashMap::new() }, rx)
    }

    pub fn open_count(&self) -> usize {
        self.terms.len()
    }

    /// Opens the shell of a `TermOpen` and answers `TermOpened`, or the code to refuse with.
    pub fn open(&mut self, req: &TermOpen, ctx: &OpenContext) -> Result<TermOpened, ErrorCode> {
        let needed = if req.admin { PermissionSet::ADMIN_TERMINAL } else { PermissionSet::TERMINAL };
        if !ctx.granted.contains(needed) {
            return Err(ErrorCode::NotPermitted);
        }
        if !req.admin && !ctx.user_present {
            return Err(ErrorCode::NobodyPresent);
        }
        if req.cols == 0
            || req.rows == 0
            || req.cols > MAX_DIM
            || req.rows > MAX_DIM
            || self.terms.contains_key(&req.term_id)
        {
            return Err(ErrorCode::ProtocolViolation);
        }
        if self.terms.len() >= MAX_TERMINALS {
            return Err(ErrorCode::Busy);
        }
        let mut pty = self
            .spawner
            .spawn(&PtyRequest { cols: req.cols, rows: req.rows, admin: req.admin })
            .map_err(|e| code_for(&e))?;
        let streams = pty.reader().and_then(|r| pty.writer().map(|w| (r, w))).map_err(|e| {
            let _ = pty.kill();
            code_for(&e)
        })?;
        let (mut reader, mut writer) = streams;
        let account: String = pty.account().chars().take(64).collect();
        let term_id = req.term_id;
        let pty = Arc::new(Mutex::new(pty));
        let closing = Arc::new(AtomicBool::new(false));
        let reader_done = Arc::new(AtomicBool::new(false));
        let signal = Signal::default();
        let (in_tx, in_rx) = sync_channel::<Vec<u8>>(INPUT_QUEUE);

        // PTY -> events.
        {
            let (events, done, closing) = (self.events.clone(), Arc::clone(&reader_done), Arc::clone(&closing));
            let (pty, signal) = (Arc::clone(&pty), signal.clone());
            std::thread::Builder::new()
                .name(format!("swapdesk-term-{term_id}-out"))
                .spawn(move || {
                    let mut buf = vec![0u8; READ_CHUNK];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if events.send(TermEvent::Output { term_id, data: buf[..n].to_vec() }).is_err() {
                                    // Nobody listens any more: end the shell.
                                    closing.store(true, Ordering::SeqCst);
                                    let _ = lock(&pty).kill();
                                    break;
                                }
                            }
                        }
                    }
                    done.store(true, Ordering::SeqCst);
                    signal.notify();
                })
                .map_err(|_| ErrorCode::Internal)?;
        }
        // Viewer input -> PTY.
        std::thread::Builder::new()
            .name(format!("swapdesk-term-{term_id}-in"))
            .spawn(move || {
                while let Ok(bytes) = in_rx.recv() {
                    if std::io::Write::write_all(&mut writer, &bytes)
                        .and_then(|()| std::io::Write::flush(&mut writer))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|_| ErrorCode::Internal)?;
        // Exit watcher.
        {
            let (events, pty, closing, done, signal) =
                (self.events.clone(), Arc::clone(&pty), Arc::clone(&closing), Arc::clone(&reader_done), signal.clone());
            std::thread::Builder::new()
                .name(format!("swapdesk-term-{term_id}-wait"))
                .spawn(move || {
                    let mut pause = POLL_MIN;
                    // The output ending wakes the watcher once; after that only a closing does.
                    let mut output_ended = false;
                    let exit_code = loop {
                        if closing.load(Ordering::SeqCst) {
                            return;
                        }
                        match lock(&pty).try_wait() {
                            Ok(Some(code)) => break Some(code),
                            Ok(None) => {}
                            Err(_) => break None,
                        }
                        // Woken at once by the output ending or the terminal closing; otherwise it looks again after
                        // `pause`, which grows while the shell just runs.
                        let woken = signal.sleep_until(pause, || {
                            closing.load(Ordering::SeqCst) || (!output_ended && done.load(Ordering::SeqCst))
                        });
                        output_ended = done.load(Ordering::SeqCst);
                        pause = if woken { POLL_MIN } else { (pause * 2).min(POLL_MAX) };
                    };
                    // The shell ended: its last output may still be on its way, so wait for the reader to finish (it
                    // tells us), for the terminal to be closed, or for the grace to run out.
                    signal.sleep_until(DRAIN_GRACE, || done.load(Ordering::SeqCst) || closing.load(Ordering::SeqCst));
                    if !closing.load(Ordering::SeqCst) {
                        let _ = events.send(TermEvent::Exited { term_id, exit_code });
                    }
                })
                .map_err(|_| ErrorCode::Internal)?;
        }
        self.terms.insert(term_id, Live { admin: req.admin, pty, input: InputTx(in_tx), closing, signal });
        Ok(TermOpened { term_id, account })
    }

    /// The sender for a terminal's input, for the stream reader thread.
    pub fn input_sender(&self, term_id: u32) -> Option<InputTx> {
        self.terms.get(&term_id).map(|t| t.input.clone())
    }

    /// Queues input without waiting.
    pub fn input(&self, term_id: u32, bytes: Vec<u8>) -> Result<(), ErrorCode> {
        self.terms.get(&term_id).ok_or(ErrorCode::NotFound)?.input.try_send(bytes)
    }

    pub fn resize(&self, term_id: u32, cols: u16, rows: u16) -> Result<(), ErrorCode> {
        if cols == 0 || rows == 0 || cols > MAX_DIM || rows > MAX_DIM {
            return Err(ErrorCode::ProtocolViolation);
        }
        let t = self.terms.get(&term_id).ok_or(ErrorCode::NotFound)?;
        let r = lock(&t.pty).resize(cols, rows);
        r.map_err(|e| code_for(&e))
    }

    /// `TermClose`: ends the shell; no `Exited` event follows (the viewer asked for it).
    pub fn close(&mut self, term_id: u32) -> Result<(), ErrorCode> {
        let t = self.terms.remove(&term_id).ok_or(ErrorCode::NotFound)?;
        end(&t);
        Ok(())
    }

    /// Ends every terminal (session over).
    pub fn close_all(&mut self) {
        for (_, t) in self.terms.drain() {
            end(&t);
        }
    }

    /// F-PERM-02: ends what the new grants no longer cover, so it stops within the second. Returns the ids closed.
    pub fn enforce(&mut self, granted: PermissionSet) -> Vec<u32> {
        let gone: Vec<u32> = self
            .terms
            .iter()
            .filter(|(_, t)| {
                !granted.contains(if t.admin { PermissionSet::ADMIN_TERMINAL } else { PermissionSet::TERMINAL })
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &gone {
            if let Some(t) = self.terms.remove(id) {
                end(&t);
            }
        }
        gone
    }

    /// Forgets terminals whose shell already ended (call after relaying `Exited`).
    pub fn forget(&mut self, term_id: u32) {
        if let Some(t) = self.terms.remove(&term_id) {
            end(&t);
        }
    }
}

fn end(t: &Live) {
    t.closing.store(true, Ordering::SeqCst);
    t.signal.notify();
    let _ = lock(&t.pty).kill();
}

impl Drop for TermHost {
    fn drop(&mut self) {
        self.close_all();
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_of_terminal_output_shows_its_size_and_never_the_shell_text() {
        let shown = format!("{:?}", TermEvent::Output { term_id: 2, data: b"hunter2-from-the-shell".to_vec() });
        assert!(!shown.contains("hunter2") && !shown.contains("shell"), "{shown}");
        let bytes = format!("{:?}", TermEvent::Output { term_id: 2, data: vec![233; 5] });
        assert!(!bytes.split(|c: char| !c.is_ascii_digit()).any(|t| t == "233"), "{bytes}");
        assert!(shown.contains("<redacted 22 bytes>"), "{shown}");
        assert!(format!("{:?}", TermEvent::Exited { term_id: 2, exit_code: Some(1) }).contains("exit_code: Some(1)"));
    }
}
