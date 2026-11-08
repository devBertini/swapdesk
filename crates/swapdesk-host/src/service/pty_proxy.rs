//! The service's end of the terminal proxy (C2.4 "Terminal", stage 43): a `PtySpawner` whose shells run in the user
//! helper. The service never starts the user's shell itself (D13, `docs/PLAN-m4-wiring.md`): `spawn` sends `PtyOpen`
//! to the user helper and the [`Pty`] it returns is the other end of that helper's `PtyInput`, `PtyOutput`,
//! `PtyResize`, `PtyClose` and `PtyExited`.
//!
//! The proxy is blocking, as every C3 object is: it is used from `swapdesk-term`'s own threads, never on the
//! executor. Output arrives from the pump task of the helper link into a bounded channel; when the viewer is slow the
//! channel fills, the pump waits, the helper link's reader stops reading, the pipe fills and the helper's relay waits
//! (back-pressure by not reading, all the way to the shell's output pipe).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use swapdesk_ipc::{Bytes, MAX_TERM_CHUNK, Message as Ipc, Text};
use swapdesk_platform::{ErrorKind, OsGrant, PlatformError, Pty, PtyRequest, PtySpawner, Result as PResult};
use swapdesk_proto::ErrorCode;
use tokio::sync::mpsc;

use super::helper_link::HelperLink;
use super::routing::HelperKind;
use super::shared::Shared;

/// Output chunks a proxied terminal queues for its reader before the pump of the helper link waits.
const OUTPUT_CHUNKS: usize = 16;
/// How long a shell may take to start in the helper (a profile can be slow; past this the open fails).
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct RouteState {
    /// The helper's answer to `PtyOpen`: the refusal code (`NoError` is success) and the account.
    reply: Option<(ErrorCode, String)>,
    /// The shell ended: its code, when the helper gave one.
    exit: Option<Option<i32>>,
}

/// One proxied terminal, as the pump of the helper link and the `Pty` both see it.
struct Route {
    link_id: u64,
    state: Mutex<RouteState>,
    changed: Condvar,
    /// The sender of the output channel; taken when the terminal ends so the reader sees the end of the stream.
    out: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
}

/// The proxied terminals of the service, by the id the service chose for each (unique across helpers and restarts).
#[derive(Default)]
pub(crate) struct PtyRoutes {
    routes: Mutex<HashMap<u32, Arc<Route>>>,
    next: AtomicU32,
}

impl PtyRoutes {
    fn fresh(&self, link_id: u64) -> (u32, Arc<Route>, mpsc::Receiver<Vec<u8>>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed).wrapping_add(1).max(1);
        let (tx, rx) = mpsc::channel(OUTPUT_CHUNKS);
        let route =
            Arc::new(Route { link_id, state: Mutex::default(), changed: Condvar::new(), out: Mutex::new(Some(tx)) });
        lock(&self.routes).insert(id, Arc::clone(&route));
        (id, route, rx)
    }

    fn get(&self, link_id: u64, id: u32) -> Option<Arc<Route>> {
        // A helper only speaks for the terminals it was asked to open.
        lock(&self.routes).get(&id).filter(|r| r.link_id == link_id).cloned()
    }

    fn forget(&self, id: u32) {
        lock(&self.routes).remove(&id);
    }

    /// `PtyOpenReply` from helper `link_id`.
    pub fn reply(&self, link_id: u64, id: u32, code: ErrorCode, account: String) {
        if let Some(r) = self.get(link_id, id) {
            lock(&r.state).reply.get_or_insert((code, account));
            r.changed.notify_all();
        }
    }

    /// `PtyOutput` from helper `link_id`: waits for room in the terminal's channel (the back-pressure).
    pub async fn output(&self, link_id: u64, id: u32, bytes: Vec<u8>) {
        let Some(r) = self.get(link_id, id) else { return };
        let tx = lock(&r.out).clone();
        if let Some(tx) = tx {
            // The reader is gone when the terminal was closed: the chunk has nowhere to go.
            let _ = tx.send(bytes).await;
        }
    }

    /// `PtyExited` from helper `link_id`: the output already relayed stays readable, then the reader ends.
    pub fn exited(&self, link_id: u64, id: u32, code: Option<i32>) {
        if let Some(r) = self.get(link_id, id) {
            lock(&r.state).exit = Some(code);
            *lock(&r.out) = None;
            r.changed.notify_all();
        }
    }

    /// The pipe of helper `link_id` ended: every terminal it ran is over, and every open waiting for it fails.
    pub fn fail_link(&self, link_id: u64) {
        let gone: Vec<Arc<Route>> = lock(&self.routes).values().filter(|r| r.link_id == link_id).cloned().collect();
        for r in gone {
            {
                let mut s = lock(&r.state);
                s.reply.get_or_insert((ErrorCode::Unavailable, String::new()));
                s.exit.get_or_insert(None);
            }
            *lock(&r.out) = None;
            r.changed.notify_all();
        }
    }
}

/// The `PtySpawner` of user terminals: shells in the user helper.
pub(crate) struct PtyProxy {
    shared: Arc<Shared>,
    /// The SID of the user logged in at the console, asked when a terminal opens.
    console_user: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl PtyProxy {
    pub fn new(shared: Arc<Shared>, console_user: Arc<dyn Fn() -> Option<String> + Send + Sync>) -> Self {
        Self { shared, console_user }
    }
}

fn error(kind: ErrorKind, op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.into() }
}

/// The platform error a helper's refusal stands for; `swapdesk-term` maps it back to the same code.
fn refusal(code: ErrorCode) -> PlatformError {
    let kind = match code {
        ErrorCode::NotPermitted => ErrorKind::PermissionDenied(OsGrant::Administrator),
        ErrorCode::Unavailable | ErrorCode::NobodyPresent => ErrorKind::Unavailable,
        ErrorCode::Timeout => ErrorKind::Timeout,
        ErrorCode::ProtocolViolation => ErrorKind::InvalidInput,
        _ => ErrorKind::Os,
    };
    error(kind, "PtyProxy.spawn", "the user helper refused the terminal")
}

impl PtySpawner for PtyProxy {
    fn spawn(&self, request: &PtyRequest) -> PResult<Box<dyn Pty>> {
        if request.admin {
            // The administrator terminal runs in the service itself, never in a helper on a user's desktop.
            return Err(error(
                ErrorKind::PermissionDenied(OsGrant::Administrator),
                "PtyProxy.spawn",
                "an administrator terminal does not run in a helper",
            ));
        }
        let session_user = (self.console_user)()
            .ok_or_else(|| error(ErrorKind::Unavailable, "PtyProxy.spawn", "nobody is logged in"))?;
        let link = self
            .shared
            .helper(HelperKind::User)
            .ok_or_else(|| error(ErrorKind::Unavailable, "PtyProxy.spawn", "no user helper is attached"))?;
        let (id, route, rx) = self.shared.ptys.fresh(link.id);
        let forget = |shared: &Shared| shared.ptys.forget(id);
        let open = Ipc::PtyOpen { id, cols: request.cols, rows: request.rows, session_user: Text(session_user) };
        if !link.send_blocking(open) {
            forget(&self.shared);
            return Err(error(ErrorKind::Unavailable, "PtyProxy.spawn", "the user helper's pipe is closed"));
        }
        let reply = {
            let guard = lock(&route.state);
            let (guard, _) = route
                .changed
                .wait_timeout_while(guard, OPEN_TIMEOUT, |s| s.reply.is_none())
                .unwrap_or_else(PoisonError::into_inner);
            guard.reply.clone()
        };
        match reply {
            None => {
                // Too late: if the shell does start after all, it is told to end.
                link.send_now(Ipc::PtyClose { id });
                forget(&self.shared);
                Err(error(ErrorKind::Timeout, "PtyProxy.spawn", "the shell did not start in time"))
            }
            Some((code, _)) if code != ErrorCode::NoError => {
                forget(&self.shared);
                Err(refusal(code))
            }
            Some((_, account)) => Ok(Box::new(ProxyPty {
                id,
                account,
                link,
                route,
                shared: Arc::clone(&self.shared),
                output: Some(rx),
                writer_taken: false,
                ended: false,
            })),
        }
    }
}

/// A shell in the user helper.
struct ProxyPty {
    id: u32,
    account: String,
    link: Arc<HelperLink>,
    route: Arc<Route>,
    shared: Arc<Shared>,
    output: Option<mpsc::Receiver<Vec<u8>>>,
    writer_taken: bool,
    /// `PtyClose` was sent (or the shell ended by itself).
    ended: bool,
}

impl Pty for ProxyPty {
    fn account(&self) -> &str {
        &self.account
    }

    fn reader(&mut self) -> PResult<Box<dyn Read + Send>> {
        let rx = self
            .output
            .take()
            .ok_or_else(|| error(ErrorKind::InvalidInput, "ProxyPty.reader", "the reader was taken already"))?;
        Ok(Box::new(ProxyReader { rx, chunk: Vec::new(), pos: 0 }))
    }

    fn writer(&mut self) -> PResult<Box<dyn Write + Send>> {
        if std::mem::replace(&mut self.writer_taken, true) {
            return Err(error(ErrorKind::InvalidInput, "ProxyPty.writer", "the writer was taken already"));
        }
        Ok(Box::new(ProxyWriter { link: Arc::clone(&self.link), id: self.id }))
    }

    fn resize(&mut self, cols: u16, rows: u16) -> PResult<()> {
        if self.ended || !self.link.send_blocking(Ipc::PtyResize { id: self.id, cols, rows }) {
            return Err(error(ErrorKind::Closed, "ProxyPty.resize", "the terminal is closed"));
        }
        Ok(())
    }

    fn try_wait(&mut self) -> PResult<Option<i32>> {
        let exit = lock(&self.route.state).exit;
        Ok(exit.map(|code| {
            self.ended = true;
            // A shell the helper could not report a code for (its pipe broke) counts as failed.
            code.unwrap_or(-1)
        }))
    }

    fn kill(&mut self) -> PResult<()> {
        self.end();
        Ok(())
    }
}

impl ProxyPty {
    fn end(&mut self) {
        if !std::mem::replace(&mut self.ended, true) {
            self.link.send_blocking(Ipc::PtyClose { id: self.id });
        }
        *lock(&self.route.out) = None;
        self.shared.ptys.forget(self.id);
    }
}

impl Drop for ProxyPty {
    fn drop(&mut self) {
        self.end();
    }
}

struct ProxyReader {
    rx: mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for ProxyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pos >= self.chunk.len() {
            // A plain thread of `swapdesk-term` calls this, never a runtime task.
            match self.rx.blocking_recv() {
                Some(chunk) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len() - self.pos);
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct ProxyWriter {
    link: Arc<HelperLink>,
    id: u32,
}

impl Write for ProxyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for chunk in buf.chunks(MAX_TERM_CHUNK) {
            if !self.link.send_blocking(Ipc::PtyInput { id: self.id, bytes: Bytes(chunk.to_vec()) }) {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
