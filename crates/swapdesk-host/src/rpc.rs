//! A blocking request/reply client over a C2.4 pipe: the service asks a helper or a UI (clipboard, quick actions) and
//! waits for the answer carrying the same `req`.
//!
//! Used from threads that may block (C3: the traits are blocking and never run on the tokio executor). The pipe's
//! reader thread hands every reply to [`Rpc::resolve`]; when the pipe ends [`Rpc::close`] fails every waiter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use swapdesk_ipc::Message;

/// Why a call returned no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcError {
    /// The pipe is closed (or the request could not be queued).
    Closed,
    /// No answer in time.
    Timeout,
}

type Sender = Box<dyn Fn(Message) -> bool + Send + Sync>;

/// The request side of one pipe.
pub struct Rpc {
    next: AtomicU32,
    pending: Mutex<HashMap<u32, SyncSender<Message>>>,
    send: Sender,
    closed: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The `req` an answer message carries (`None` for any other message).
pub fn reply_req(msg: &Message) -> Option<u32> {
    match msg {
        Message::ClipReply { req, .. } | Message::ActionReply { req, .. } => Some(*req),
        _ => None,
    }
}

impl Rpc {
    /// `send` queues a message on the pipe and says whether it was queued.
    pub fn new(send: impl Fn(Message) -> bool + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            next: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            send: Box::new(send),
            closed: AtomicBool::new(false),
        })
    }

    /// Sends the request `build(req)` and waits up to `limit` for its answer. Blocks: never call it on the runtime.
    pub fn call(&self, build: impl FnOnce(u32) -> Message, limit: Duration) -> Result<Message, RpcError> {
        let req = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = sync_channel(1);
        {
            let mut p = lock(&self.pending);
            if self.closed.load(Ordering::Acquire) {
                return Err(RpcError::Closed);
            }
            p.insert(req, tx);
        }
        if !(self.send)(build(req)) {
            lock(&self.pending).remove(&req);
            return Err(RpcError::Closed);
        }
        match rx.recv_timeout(limit) {
            Ok(m) => Ok(m),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                lock(&self.pending).remove(&req);
                Err(RpcError::Timeout)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(RpcError::Closed),
        }
    }

    /// Gives an answer to its waiter; `false` when nobody waits for it (late, or unknown).
    pub fn resolve(&self, msg: Message) -> bool {
        let Some(req) = reply_req(&msg) else { return false };
        match lock(&self.pending).remove(&req) {
            Some(tx) => tx.send(msg).is_ok(),
            None => false,
        }
    }

    /// The pipe ended: every waiter fails at once and later calls fail.
    pub fn close(&self) {
        let mut p = lock(&self.pending);
        self.closed.store(true, Ordering::Release);
        p.clear();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swapdesk_ipc::ClipReplyBody;

    #[test]
    fn an_answer_reaches_its_waiter_and_a_late_one_is_ignored() {
        let (tx, rx) = std::sync::mpsc::channel();
        let rpc = Rpc::new(move |m| tx.send(m).is_ok());
        let rpc2 = Arc::clone(&rpc);
        let answer = std::thread::spawn(move || {
            let Message::ClipChangeCount { req } = rx.recv().unwrap() else { panic!("not the request") };
            assert!(rpc2.resolve(Message::ClipReply { req, body: ClipReplyBody::Count(7) }));
        });
        let got = rpc.call(|req| Message::ClipChangeCount { req }, Duration::from_secs(5)).unwrap();
        answer.join().unwrap();
        assert!(matches!(got, Message::ClipReply { body: ClipReplyBody::Count(7), .. }));
        assert!(!rpc.resolve(Message::ClipReply { req: 99, body: ClipReplyBody::Empty }));
    }

    #[test]
    fn a_silent_peer_times_out_and_a_closed_pipe_fails_the_waiters() {
        let rpc = Rpc::new(|_| true);
        assert_eq!(
            rpc.call(|req| Message::ClipRead { req }, Duration::from_millis(50)).unwrap_err(),
            RpcError::Timeout
        );
        let closer = Arc::clone(&rpc);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            closer.close();
        });
        assert_eq!(rpc.call(|req| Message::ClipRead { req }, Duration::from_secs(5)).unwrap_err(), RpcError::Closed);
        t.join().unwrap();
        assert_eq!(rpc.call(|req| Message::ClipRead { req }, Duration::from_secs(5)).unwrap_err(), RpcError::Closed);
        let refused = Rpc::new(|_| false);
        assert_eq!(
            refused.call(|req| Message::ClipRead { req }, Duration::from_secs(5)).unwrap_err(),
            RpcError::Closed
        );
    }
}
