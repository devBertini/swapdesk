//! `Command::Elevate` in the backend: ask the service for a ticket, let the platform's [`Elevator`] run, ask the
//! service how it ended and tell the interface (ELEVATION.md 3.4 and 3.6).
//!
//! The work blocks for up to [`RUN_FOR`], so it never runs on the backend thread: each `Elevate` gets a worker thread.
//! The worker's two requests (op 35, op 38) are written by the backend thread (`Item::Request`) and answered by the
//! reader thread into the worker's own channel (`Inner::waiters`), the way `pending` pairs the other requests with
//! their tickets. Neither thread waits for the worker.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use swapdesk_ipc::{ElevAction, ElevationOutcome, ElevationTicket, ReplyBody, UiOp, status_code};

use super::{Inner, Item, convert, lock};
use crate::api::{ApiCode, ApiError, ElevatedAction, Reply, Ticket, UiEvent};

/// How long the worker lets the elevated side run, counted from the ticket. The ticket lives 120 s.
pub const RUN_FOR: Duration = Duration::from_secs(110);

/// How long the worker waits for the service to answer one of its requests.
const SERVICE_WAIT: Duration = Duration::from_secs(30);

/// The platform's way to get an administrator: Windows starts an elevated instance of the program through UAC, the
/// Mac asks the system for the administrator's password.
pub trait Elevator: Send + Sync {
    /// Asks the system for an administrator and lets the elevated side confirm `ticket` with the service.
    /// Blocks until that side ended or `deadline` passed. It never decides the outcome; the service does (op 38).
    /// `line` is what the step shows: the Mac's system dialog shows it as its extra text; Windows' elevated window
    /// shows the action as the service holds it instead.
    fn run(&self, ticket: &ElevationTicket, line: &str, deadline: Instant) -> ElevatorRun;
}

/// How the elevated side ended, as far as the platform could tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevatorRun {
    /// The elevated process ended; its exit code when the system gave it.
    Ended { exit_code: Option<u32> },
    /// The user refused the system's prompt.
    Declined,
    /// `deadline` passed first.
    TimedOut,
    /// The elevator could not start the elevated side.
    Failed(ApiCode),
}

/// What a worker waits for: the op's number (the body is decoded with it) and where the answer goes.
pub(super) type Waiter = (u16, mpsc::Sender<Result<ReplyBody, ApiCode>>);

/// The reader thread's side: hands the answer of `req` to the worker that waits for it. False when no worker does.
pub(super) fn answer_waiter(inner: &Inner, req: u32, status: u16, body: &[u8]) -> bool {
    let Some((op, tx)) = lock(&inner.waiters).remove(&req) else { return false };
    let result = match status_code(status) {
        Ok(Some(code)) => Err(convert::code_from_wire(code)),
        Ok(None) => ReplyBody::decode(op, body).map_err(|_| ApiCode::Internal),
        Err(_) => Err(ApiCode::Internal),
    };
    // A worker that stopped waiting dropped its end: nothing to tell.
    let _ = tx.send(result);
    true
}

/// The wire action of a C6 one (ELEVATION.md 3.1: tags 0, 2 and 3).
fn wire_action(action: &ElevatedAction) -> Option<ElevAction> {
    Some(match action {
        ElevatedAction::SetGrants { device_id, grants } => {
            ElevAction::SetGrants { device_id: device_id.clone(), grants: grants.0 }
        }
        ElevatedAction::OpenAdminTerminal { session_id } => ElevAction::OpenAdminTerminal { session_id: *session_id },
        ElevatedAction::ElevateSession { session_id } => ElevAction::ElevateSession { session_id: *session_id },
    })
}

/// Starts the worker of an `Elevate` command; its ticket is answered when the worker is done.
pub(super) fn spawn(inner: &Arc<Inner>, ticket: Ticket, action: ElevatedAction, line: String, run: u32) {
    let Some(out) = lock(&inner.out).clone() else {
        inner.reply(ticket, Err(ApiError::new(ApiCode::ServiceDown)));
        return;
    };
    let worker = Arc::clone(inner);
    let spawned = thread::Builder::new().name("swapdesk-ui-elevate".into()).spawn(move || {
        let result = elevate(&worker, &out, &action, &line).map_err(ApiError::new);
        // One batch: the step learns how it ended before the `Elevate` ticket is answered, and the answer is a plain
        // `Done` so the error strip never shows a second message.
        worker.push(vec![
            UiEvent::ElevationResult { action, run, result },
            UiEvent::Reply { ticket, result: Ok(Reply::Done) },
        ]);
    });
    if spawned.is_err() {
        inner.reply(ticket, Err(ApiError::new(ApiCode::Internal)));
    }
}

/// The whole flow of one elevation, as the result the step shows.
fn elevate(inner: &Inner, out: &mpsc::Sender<Item>, action: &ElevatedAction, line: &str) -> Result<(), ApiCode> {
    let elevator = lock(&inner.elevator).clone().ok_or(ApiCode::Internal)?;
    let wire = wire_action(action).ok_or(ApiCode::Internal)?;

    let (ticket, ttl_s) = match request(inner, out, UiOp::ElevationBegin { action: wire })? {
        ReplyBody::ElevationTicket { ticket, ttl_s } => (ticket, ttl_s),
        _ => return Err(ApiCode::Internal),
    };
    // `ttl_s` 0: an administrator approved this device's permissions on this connection a moment ago (D62): the
    // ticket is already confirmed and no elevated instance is started.
    let run = if ttl_s == 0 {
        ElevatorRun::Ended { exit_code: Some(0) }
    } else {
        elevator.run(&ticket, line, Instant::now() + RUN_FOR)
    };

    // The ticket is always ended, whatever the run said: it closes an open one and tells the outcome.
    let outcome = match request(inner, out, UiOp::ElevationEnd { ticket }) {
        Ok(ReplyBody::ElevationOutcome(o)) => o,
        Ok(_) => return Err(ApiCode::Internal),
        Err(ApiCode::ServiceDown) => return Err(ApiCode::ServiceDown),
        Err(_) => return Err(ApiCode::Internal),
    };
    outcome_result(outcome, run)
}

/// ELEVATION.md 3.6: the service's outcome decides; the run only says why a failure failed.
fn outcome_result(outcome: ElevationOutcome, run: ElevatorRun) -> Result<(), ApiCode> {
    match outcome {
        ElevationOutcome::Confirmed => return Ok(()),
        ElevationOutcome::Declined => return Err(ApiCode::Refused),
        ElevationOutcome::Open | ElevationOutcome::Unknown => {}
    }
    Err(match run {
        ElevatorRun::Declined => ApiCode::Refused,
        ElevatorRun::TimedOut => ApiCode::Timeout,
        ElevatorRun::Failed(code) => code,
        ElevatorRun::Ended { exit_code: Some(1) } => ApiCode::Refused,
        ElevatorRun::Ended { exit_code: Some(3) } => ApiCode::ServiceDown,
        ElevatorRun::Ended { exit_code: Some(4 | 6) } => ApiCode::NotPermitted,
        ElevatorRun::Ended { exit_code: Some(5) } => ApiCode::Timeout,
        // The ticket ran out while the elevated side was still on its way.
        ElevatorRun::Ended { .. } if outcome == ElevationOutcome::Unknown => ApiCode::Timeout,
        // An exit code of 0 that the service does not back, 2 or 7, or none readable.
        ElevatorRun::Ended { .. } => ApiCode::Internal,
    })
}

/// Sends `op` through the backend thread and waits for the reader thread to hand over its answer.
pub(super) fn request(inner: &Inner, out: &mpsc::Sender<Item>, op: UiOp) -> Result<ReplyBody, ApiCode> {
    let req = inner.next_req.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    {
        // Same lock as `fail_all`: no waiter is registered after the pipe was declared down.
        let _pending = lock(&inner.pending);
        if inner.is_down() {
            return Err(ApiCode::ServiceDown);
        }
        lock(&inner.waiters).insert(req, (op.code(), tx));
    }
    if out.send(Item::Request(req, op)).is_err() {
        lock(&inner.waiters).remove(&req);
        return Err(ApiCode::ServiceDown);
    }
    match rx.recv_timeout(SERVICE_WAIT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            lock(&inner.waiters).remove(&req);
            Err(ApiCode::Timeout)
        }
        // `fail_all` dropped the sender.
        Err(RecvTimeoutError::Disconnected) => Err(ApiCode::ServiceDown),
    }
}
