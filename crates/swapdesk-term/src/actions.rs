//! Quick-action relay (F-ACT-01): a `QuickAction` from the viewer, run on the host through the C3 `ActionRunner`.

use swapdesk_platform::ActionRunner;
use swapdesk_proto::{Action, ErrorCode, PermissionSet, QuickAction, QuickActionResult};

use crate::wire::code_for;

/// What the relay needs to know about the session and the host at the time of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionContext {
    /// The session's granted permissions.
    pub granted: PermissionSet,
    /// A user is logged in at the console.
    pub user_present: bool,
}

/// The answer and what the audit log (F-SESS-04) records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionDone {
    pub action: Action,
    /// What goes back as `QuickActionResult`.
    pub result: QuickActionResult,
    /// The runner was called (false when the relay refused it first).
    pub ran: bool,
}

pub struct ActionRelay<R: ActionRunner> {
    runner: R,
}

impl<R: ActionRunner> ActionRelay<R> {
    pub fn new(runner: R) -> Self {
        Self { runner }
    }

    /// Requires `CONTROL`; Task Manager / Activity Monitor also need a logged-in user (they start only as that user, so with
    /// nobody there the host answers `NOBODY_PRESENT` without calling the runner). Lock, sleep and restart go to the
    /// runner either way (with nobody logged in the service's runner makes system calls).
    pub fn handle(&mut self, req: &QuickAction, ctx: &ActionContext) -> ActionDone {
        let done = |code: ErrorCode, ran: bool| ActionDone {
            action: req.kind,
            result: QuickActionResult { action_id: req.action_id, code },
            ran,
        };
        if !ctx.granted.contains(PermissionSet::CONTROL) {
            return done(ErrorCode::NotPermitted, false);
        }
        if req.kind == Action::TaskMonitor && !ctx.user_present {
            return done(ErrorCode::NobodyPresent, false);
        }
        match self.runner.run(req.kind) {
            Ok(()) => done(ErrorCode::NoError, true),
            Err(e) => done(code_for(&e), true),
        }
    }

    /// Keep the machine awake while a session lives (C3).
    pub fn keep_awake(&mut self, on: bool) -> Result<(), ErrorCode> {
        self.runner.keep_awake(on).map_err(|e| code_for(&e))
    }

    pub fn runner(&self) -> &R {
        &self.runner
    }
}
