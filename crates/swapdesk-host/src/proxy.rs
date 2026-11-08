//! The C3 `ClipboardProvider` and `ActionRunner` as proxies over a C2.4 pipe (stage 42), and the server side that
//! answers them with the real objects.
//!
//! The service owns every LAN socket and never touches the user's clipboard or shell, so the work runs where the user
//! is: in the user helper (host role) or in the UI process (viewer role). [`PipeClipboard`] and [`PipeActions`] are
//! blocking clients (C3: they run on a thread that may block, never on the tokio executor); [`answer_clipboard`] and
//! [`answer_action`] are the servers' one-message step. The clipboard one lives in `swapdesk-ui` (the UI backend
//! answers with it too, and the host crate already depends on that one).

use std::sync::Arc;
use std::time::Duration;

use swapdesk_ipc::{ClipData, ClipFail, ClipReplyBody, Message};
use swapdesk_platform::{ActionRunner, ClipContent, ClipRead, ClipboardProvider, ErrorKind, PlatformError, Result};
use swapdesk_proto::{Action, ErrorCode};

pub use swapdesk_ui::service_backend::clipboard::{answer_clipboard, no_clipboard};

use crate::rpc::{Rpc, RpcError};

/// How long a call to a helper or a UI may take. A clipboard read of a large image is the slowest.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// A quick action may wait for the machine (suspend, restart): more room than a clipboard call.
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the proxies find the pipe to ask on, at the moment of each call (a helper may come and go).
pub trait RpcSource: Send + Sync {
    fn rpc(&self) -> Option<Arc<Rpc>>;
}

fn err(kind: ErrorKind, op: &'static str, detail: &str) -> PlatformError {
    PlatformError { kind, op, os_code: None, detail: detail.into() }
}

fn rpc_error(op: &'static str, e: RpcError) -> PlatformError {
    match e {
        RpcError::Closed => err(ErrorKind::Closed, op, "the pipe is closed"),
        RpcError::Timeout => err(ErrorKind::Timeout, op, "no answer in time"),
    }
}

fn fail_error(op: &'static str, f: ClipFail) -> PlatformError {
    match f {
        ClipFail::Unavailable => err(ErrorKind::Unavailable, op, "the clipboard is busy"),
        ClipFail::Invalid => err(ErrorKind::InvalidInput, op, "the content was refused"),
        ClipFail::Other => err(ErrorKind::Os, op, "the clipboard call failed"),
    }
}

/// The clipboard of whoever is on the other end of the pipe.
pub struct PipeClipboard {
    src: Arc<dyn RpcSource>,
}

impl PipeClipboard {
    pub fn new(src: Arc<dyn RpcSource>) -> Self {
        Self { src }
    }

    fn ask(&self, op: &'static str, build: impl FnOnce(u32) -> Message) -> Result<ClipReplyBody> {
        let rpc = self.src.rpc().ok_or_else(|| err(ErrorKind::Unavailable, op, "nobody serves the clipboard now"))?;
        match rpc.call(build, CALL_TIMEOUT).map_err(|e| rpc_error(op, e))? {
            Message::ClipReply { body: ClipReplyBody::Failed(f), .. } => Err(fail_error(op, f)),
            Message::ClipReply { body, .. } => Ok(body),
            _ => Err(err(ErrorKind::Os, op, "an answer of another kind")),
        }
    }
}

impl ClipboardProvider for PipeClipboard {
    fn change_count(&mut self) -> Result<u64> {
        match self.ask("clipboard change count", |req| Message::ClipChangeCount { req })? {
            ClipReplyBody::Count(n) => Ok(n),
            _ => Err(err(ErrorKind::Os, "clipboard change count", "an answer of another kind")),
        }
    }

    fn read(&mut self) -> Result<ClipRead> {
        match self.ask("clipboard read", |req| Message::ClipRead { req })? {
            ClipReplyBody::Content(ClipData::Text(t)) => Ok(ClipRead::Content(ClipContent::Text(t))),
            ClipReplyBody::Content(ClipData::Png(p)) => Ok(ClipRead::Content(ClipContent::Png(p))),
            ClipReplyBody::Concealed => Ok(ClipRead::Concealed),
            ClipReplyBody::Empty => Ok(ClipRead::Empty),
            ClipReplyBody::Unsupported => Ok(ClipRead::Unsupported),
            _ => Err(err(ErrorKind::Os, "clipboard read", "an answer of another kind")),
        }
    }

    fn write(&mut self, content: &ClipContent) -> Result<()> {
        let data = match content {
            ClipContent::Text(t) => ClipData::Text(t.clone()),
            ClipContent::Png(p) => ClipData::Png(p.clone()),
        };
        match self.ask("clipboard write", |req| Message::ClipWrite { req, content: data })? {
            ClipReplyBody::Written => Ok(()),
            _ => Err(err(ErrorKind::Os, "clipboard write", "an answer of another kind")),
        }
    }
}

/// The quick actions of the machine's user, run by the user helper.
pub struct PipeActions {
    src: Arc<dyn RpcSource>,
}

impl PipeActions {
    pub fn new(src: Arc<dyn RpcSource>) -> Self {
        Self { src }
    }

    /// Whether a helper is there to take the action.
    pub fn available(&self) -> bool {
        self.src.rpc().is_some_and(|r| !r.is_closed())
    }

    fn ask(&self, op: &'static str, build: impl FnOnce(u32) -> Message) -> Result<()> {
        let rpc = self.src.rpc().ok_or_else(|| err(ErrorKind::Unavailable, op, "no user helper is attached"))?;
        match rpc.call(build, ACTION_TIMEOUT).map_err(|e| rpc_error(op, e))? {
            Message::ActionReply { code: ErrorCode::NoError, .. } => Ok(()),
            Message::ActionReply { code, .. } => Err(error_of_code(op, code)),
            _ => Err(err(ErrorKind::Os, op, "an answer of another kind")),
        }
    }
}

/// The `PlatformError` a helper's `ActionReply` code stands for (the inverse of `swapdesk_term::code_for` as far as
/// the wire keeps it).
fn error_of_code(op: &'static str, code: ErrorCode) -> PlatformError {
    let kind = match code {
        ErrorCode::NotPermitted => ErrorKind::Unsupported,
        ErrorCode::Unavailable => ErrorKind::Unavailable,
        ErrorCode::Timeout => ErrorKind::Timeout,
        ErrorCode::ProtocolViolation => ErrorKind::InvalidInput,
        ErrorCode::NotFound => ErrorKind::Closed,
        _ => ErrorKind::Os,
    };
    err(kind, op, "the user helper refused or failed the action")
}

impl ActionRunner for PipeActions {
    fn run(&mut self, action: Action) -> Result<()> {
        self.ask("quick action", |req| Message::RunAction { req, action })
    }

    fn keep_awake(&mut self, on: bool) -> Result<()> {
        self.ask("keep awake", |req| Message::KeepAwake { req, on })
    }
}

fn code_of(e: &PlatformError) -> ErrorCode {
    match e.kind {
        ErrorKind::PermissionDenied(_) | ErrorKind::Unsupported => ErrorCode::NotPermitted,
        ErrorKind::Unavailable | ErrorKind::AccessLost => ErrorCode::Unavailable,
        ErrorKind::Timeout => ErrorCode::Timeout,
        ErrorKind::InvalidInput => ErrorCode::ProtocolViolation,
        ErrorKind::Closed => ErrorCode::NotFound,
        _ => ErrorCode::IoFailed,
    }
}

/// The server step of the action pair: `Some(reply)` for 0x64 and 0x65, `None` for any other message.
pub fn answer_action<R: ActionRunner + ?Sized>(runner: &mut R, msg: &Message) -> Option<Message> {
    let (req, result) = match msg {
        Message::RunAction { req, action } => (*req, runner.run(*action)),
        Message::KeepAwake { req, on } => (*req, runner.keep_awake(*on)),
        _ => return None,
    };
    Some(Message::ActionReply { req, code: result.map_or_else(|e| code_of(&e), |()| ErrorCode::NoError) })
}

/// The answer of a process that cannot run actions.
pub fn no_actions(msg: &Message) -> Option<Message> {
    let req = match msg {
        Message::RunAction { req, .. } | Message::KeepAwake { req, .. } => *req,
        _ => return None,
    };
    Some(Message::ActionReply { req, code: ErrorCode::Unavailable })
}
