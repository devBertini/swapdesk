//! The Mac's administrator confirmation (`docs/ELEVATION-MAC.md` 2, 3.2 to 3.5): the `Elevator` of the shared backend
//! worker (`swapdesk_ui::service_backend`), which has already taken a ticket for one exact action (op 35).
//!
//! * [`MacElevator::run`] asks macOS through [`AdminConfirm`] on a thread of its own, `swapdesk-admin-check`, because the
//!   system's dialog cannot be withdrawn: it waits for the answer until `deadline`, and only an answer that came in time
//!   is acted on. A late answer goes into a channel nobody reads, so it confirms nothing.
//! * Only after an approval does it build the approver (the asking connection with `admin: true`, never earlier) and
//!   call `UiHub::confirm_elevation` in this process. The hub puts its one-use pass on the app's connection; the worker
//!   then asks the hub how the ticket ended (op 38) and that outcome decides the result.
//! * One dialog at a time: while a timed-out dialog is still on screen, a new run is `Busy`.
//!
//! Built and tested on every host: the macOS dialog itself ([`SystemAdminPrompt`]) is the only macOS-only item, and no
//! test ever shows it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

use swapdesk_host::ui_hub::{ConnInfo, UiHub};
use swapdesk_ipc::{ApiCode as WireCode, ElevationTicket};
use swapdesk_ui::ApiCode;
use swapdesk_ui::service_backend::{Elevator, ElevatorRun};

/// The seam tests replace: asks for an administrator, showing `line`, and blocks until answered.
pub trait AdminConfirm: Send + Sync {
    fn confirm(&self, line: &str) -> Confirmation;
}

/// The app's own copy of the outcome, so this module builds without `swapdesk-sys-mac` off macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    Approved,
    Cancelled,
    Denied,
    NoInteraction,
    Failed,
}

/// macOS's dialog: `swapdesk_sys_mac::admin::confirm_administrator(Some(line))`, its outcome mapped. An outcome that is
/// not an answer (a status the app does not know, the main thread) goes to the trace by its number only: nothing the
/// person typed ever reaches this process.
#[cfg(target_os = "macos")]
pub struct SystemAdminPrompt {
    trace: Arc<dyn swapdesk_platform::TraceSink>,
}

#[cfg(target_os = "macos")]
impl SystemAdminPrompt {
    pub fn new(trace: Arc<dyn swapdesk_platform::TraceSink>) -> Self {
        Self { trace }
    }
}

#[cfg(target_os = "macos")]
impl AdminConfirm for SystemAdminPrompt {
    fn confirm(&self, line: &str) -> Confirmation {
        use swapdesk_sys_mac::admin::{AdminOutcome, confirm_administrator};
        let outcome = confirm_administrator(Some(line));
        match outcome {
            AdminOutcome::Approved => Confirmation::Approved,
            AdminOutcome::Cancelled => Confirmation::Cancelled,
            AdminOutcome::Denied => Confirmation::Denied,
            AdminOutcome::NoInteraction => {
                self.trace.log(
                    swapdesk_platform::Level::Warn,
                    "app",
                    format_args!("the administrator dialog could not be shown"),
                );
                Confirmation::NoInteraction
            }
            AdminOutcome::MainThread | AdminOutcome::Failed(_) => {
                self.trace.log(
                    swapdesk_platform::Level::Warn,
                    "app",
                    format_args!("the administrator check failed: {outcome:?}"),
                );
                Confirmation::Failed
            }
        }
    }
}

/// The Mac's `Elevator`: asks through [`AdminConfirm`], and only on an approval confirms the ticket in process.
pub struct MacElevator {
    hub: UiHub,
    /// The app connection's own `ConnInfo` (admin false), the one `serve` was given.
    asker: ConnInfo,
    confirm: Arc<dyn AdminConfirm>,
    /// A `swapdesk-admin-check` thread is alive: its dialog may still be on screen.
    open: Arc<AtomicBool>,
}

impl MacElevator {
    /// `asker` is the app connection's own `ConnInfo` (admin false), the one `serve` was given.
    pub fn new(hub: UiHub, asker: ConnInfo, confirm: Arc<dyn AdminConfirm>) -> Self {
        Self { hub, asker, confirm, open: Arc::new(AtomicBool::new(false)) }
    }

    /// `run` with the line the system's dialog shows. `Elevator::run` carries none until the shared side hands the
    /// step's line down (N1, `ELEVATION-MAC.md` 3.6), so for now its line is empty and the dialog has no extra text.
    fn run_with_line(&self, ticket: &ElevationTicket, line: &str, deadline: Instant) -> ElevatorRun {
        if self.open.swap(true, Ordering::SeqCst) {
            return ElevatorRun::Failed(ApiCode::Busy);
        }
        let (tx, rx) = mpsc::channel();
        let (confirm, open, line) = (Arc::clone(&self.confirm), Arc::clone(&self.open), line.to_owned());
        let spawned = std::thread::Builder::new().name("swapdesk-admin-check".into()).spawn(move || {
            // The flag falls when the dialog ends, even by a panic of the confirm.
            struct Closed(Arc<AtomicBool>);
            impl Drop for Closed {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _closed = Closed(open);
            let answer = confirm.confirm(&line);
            // A run that stopped waiting dropped its end: the late answer goes nowhere.
            let _ = tx.send(answer);
        });
        if spawned.is_err() {
            self.open.store(false, Ordering::SeqCst);
            return ElevatorRun::Failed(ApiCode::Internal);
        }
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(answer) => {
                let confirmed = (answer == Confirmation::Approved).then(|| {
                    // Built now, never earlier: no administrator connection exists before an administrator answered.
                    let approver = approver_of(&self.asker);
                    self.hub.confirm_elevation(ticket, &approver, true)
                });
                run_of(answer, confirmed)
            }
            Err(RecvTimeoutError::Timeout) => ElevatorRun::TimedOut,
            // The confirm panicked before it answered.
            Err(RecvTimeoutError::Disconnected) => ElevatorRun::Failed(ApiCode::Internal),
        }
    }
}

impl Elevator for MacElevator {
    fn run(&self, ticket: &ElevationTicket, deadline: Instant) -> ElevatorRun {
        self.run_with_line(ticket, "", deadline)
    }
}

/// The approver an approval stands for: `asker` with `admin: true`, same session and user. It lives for one call and is
/// never stored.
pub fn approver_of(asker: &ConnInfo) -> ConnInfo {
    ConnInfo { session_id: asker.session_id, user: asker.user.clone(), admin: true, can_elevate: true }
}

/// What `run` returns for a confirmation and, after an approval, the hub's answer to `confirm_elevation`
/// (`ELEVATION-MAC.md` 3.4 and 3.5).
pub fn run_of(c: Confirmation, confirmed: Option<Result<(), WireCode>>) -> ElevatorRun {
    match c {
        Confirmation::Approved => match confirmed {
            Some(Ok(())) => ElevatorRun::Ended { exit_code: Some(0) },
            // The ticket expired, or the asking connection is gone.
            Some(Err(WireCode::NotFound)) => ElevatorRun::Failed(ApiCode::Timeout),
            // A refusal of the approver the app built itself is a bug (the hub traced it) or no answer at all.
            Some(Err(_)) | None => ElevatorRun::Failed(ApiCode::Internal),
        },
        Confirmation::Cancelled => ElevatorRun::Declined,
        Confirmation::Denied => ElevatorRun::Failed(ApiCode::NotPermitted),
        Confirmation::NoInteraction | Confirmation::Failed => ElevatorRun::Failed(ApiCode::Internal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::service::test_support::{Rig, Scripted, grants_with_unattended, initial_grants};
    use swapdesk_trust::UserRef;
    use swapdesk_ui::{Command, UiBackend, UiEvent};

    fn asker() -> ConnInfo {
        ConnInfo {
            session_id: 1,
            user: UserRef { id: "local-user".into(), name: "tester".into() },
            admin: false,
            can_elevate: true,
        }
    }

    #[test]
    fn every_confirmation_maps_to_its_run() {
        assert_eq!(run_of(Confirmation::Approved, Some(Ok(()))), ElevatorRun::Ended { exit_code: Some(0) });
        assert_eq!(
            run_of(Confirmation::Approved, Some(Err(WireCode::NotFound))),
            ElevatorRun::Failed(ApiCode::Timeout)
        );
        for other in [WireCode::NotPermitted, WireCode::NeedsAdmin, WireCode::Busy] {
            assert_eq!(run_of(Confirmation::Approved, Some(Err(other))), ElevatorRun::Failed(ApiCode::Internal));
        }
        assert_eq!(
            run_of(Confirmation::Approved, None),
            ElevatorRun::Failed(ApiCode::Internal),
            "no answer is no approval"
        );
        assert_eq!(run_of(Confirmation::Cancelled, None), ElevatorRun::Declined);
        assert_eq!(run_of(Confirmation::Denied, None), ElevatorRun::Failed(ApiCode::NotPermitted));
        assert_eq!(run_of(Confirmation::NoInteraction, None), ElevatorRun::Failed(ApiCode::Internal));
        assert_eq!(run_of(Confirmation::Failed, None), ElevatorRun::Failed(ApiCode::Internal));
        // Only an approval looks at the hub's answer.
        assert_eq!(run_of(Confirmation::Cancelled, Some(Ok(()))), ElevatorRun::Declined);
    }

    #[test]
    fn the_approver_is_the_asker_as_administrator_and_nothing_else() {
        let a = asker();
        let p = approver_of(&a);
        assert!(p.admin && p.can_elevate);
        assert_eq!((p.session_id, &p.user.id, &p.user.name), (a.session_id, &a.user.id, &a.user.name));
        assert!(!a.admin, "the asker itself is untouched");
    }

    /// Lets the worker's ticket through to a `MacElevator` with a short deadline of the test's choice.
    struct Within(Arc<MacElevator>, Duration);

    impl Elevator for Within {
        fn run(&self, ticket: &ElevationTicket, _deadline: Instant) -> ElevatorRun {
            self.0.run(ticket, Instant::now() + self.1)
        }
    }

    fn elevator(rig: &Rig, confirm: &Arc<Scripted>) -> Arc<MacElevator> {
        let confirm: Arc<dyn AdminConfirm> = confirm.clone();
        Arc::new(MacElevator::new(rig.svc.hub().clone(), crate::service::app_connection(), confirm))
    }

    #[test]
    fn an_approval_confirms_the_ticket_and_the_grant_passes_once() {
        let confirm = Scripted::new(Confirmation::Approved);
        let rig = Rig::start("approve", None);
        assert_eq!(rig.widen(), Err(ApiCode::NeedsAdmin), "before the approval");
        rig.backend.set_elevator(elevator(&rig, &confirm));
        assert_eq!(rig.elevate(), Ok(()));
        assert_eq!(confirm.calls(), 1);
        assert_eq!(rig.widen(), Ok(()), "the resend passes with the pass");
        assert_eq!(rig.grants_here(), grants_with_unattended().0);
        // Narrowing is open to anybody; widening again needs a new approval.
        assert_eq!(rig.narrow(), Ok(()));
        assert_eq!(rig.widen(), Err(ApiCode::NeedsAdmin), "the pass was used up");
    }

    #[test]
    fn a_cancel_confirms_nothing_and_ends_refused() {
        let confirm = Scripted::new(Confirmation::Cancelled);
        let rig = Rig::start("cancel", None);
        rig.backend.set_elevator(elevator(&rig, &confirm));
        assert_eq!(rig.elevate(), Err(ApiCode::Refused));
        assert_eq!(rig.widen(), Err(ApiCode::NeedsAdmin));
        assert_eq!(rig.grants_here(), initial_grants().0);
    }

    #[test]
    fn a_denial_confirms_nothing_and_ends_not_permitted() {
        let confirm = Scripted::new(Confirmation::Denied);
        let rig = Rig::start("deny", None);
        rig.backend.set_elevator(elevator(&rig, &confirm));
        assert_eq!(rig.elevate(), Err(ApiCode::NotPermitted));
        assert_eq!(rig.widen(), Err(ApiCode::NeedsAdmin));
        assert_eq!(rig.grants_here(), initial_grants().0);
    }

    #[test]
    fn an_answer_after_the_deadline_confirms_nothing() {
        let confirm = Scripted::slow(Confirmation::Approved, Duration::from_millis(900));
        let rig = Rig::start("late", None);
        let mac = elevator(&rig, &confirm);
        rig.backend.set_elevator(Arc::new(Within(Arc::clone(&mac), Duration::from_millis(200))));
        assert_eq!(rig.elevate(), Err(ApiCode::Timeout));
        // The dialog is still open: the late approval comes after the run ended. Wait for it, then a little longer for
        // anything it might wrongly confirm.
        let end = Instant::now() + Duration::from_secs(5);
        while mac.open.load(Ordering::SeqCst) {
            assert!(Instant::now() < end, "the dialog flag never fell");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(confirm.calls(), 1);
        assert_eq!(rig.widen(), Err(ApiCode::NeedsAdmin), "no pass on the connection");
        assert_eq!(rig.grants_here(), initial_grants().0);
    }

    #[test]
    fn a_second_run_while_a_dialog_is_open_is_busy() {
        let confirm = Scripted::slow(Confirmation::Approved, Duration::from_millis(1200));
        let rig = Rig::start("busy", None);
        let mac = elevator(&rig, &confirm);
        let ticket = ElevationTicket([7; 16]);
        assert_eq!(mac.run(&ticket, Instant::now() + Duration::from_millis(150)), ElevatorRun::TimedOut);
        let second = mac.run(&ticket, Instant::now() + Duration::from_millis(150));
        assert_eq!(second, ElevatorRun::Failed(ApiCode::Busy), "the first dialog is still open");
        assert_eq!(confirm.calls(), 1, "no second dialog is stacked");
        // When the first dialog ends the elevator is free again.
        let end = Instant::now() + Duration::from_secs(5);
        while mac.open.load(Ordering::SeqCst) {
            assert!(Instant::now() < end, "the dialog flag never fell");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_hub_sees_no_approver_before_the_approval() {
        let confirm = Scripted::new(Confirmation::Approved);
        let rig = Rig::start("before", None);
        // While the dialog is up, the resend of the widening is still `NeedsAdmin`, and so is a confirmation by the asker
        // itself: nothing in the hub has an administrator yet.
        let seen: Arc<std::sync::Mutex<Vec<Result<(), ApiCode>>>> = Arc::default();
        let (hub, asker, sink) = (rig.svc.hub().clone(), crate::service::app_connection(), Arc::clone(&seen));
        let (backend, inbox, device) = (Arc::clone(&rig.backend), Arc::clone(&rig.inbox), rig.device.clone());
        confirm.while_asking(move || {
            let t = backend.send(Command::SetGrants { device_id: device, grants: grants_with_unattended() });
            let event = inbox.wait_for(|e| matches!(e, UiEvent::Reply { ticket, .. } if *ticket == t));
            let UiEvent::Reply { result, .. } = event else { unreachable!() };
            sink.lock().unwrap().push(result.map(|_| ()).map_err(|e| e.code));
            // The asker as it is (not an administrator) cannot confirm a ticket either.
            assert_eq!(hub.confirm_elevation(&ElevationTicket([9; 16]), &asker, true), Err(WireCode::NeedsAdmin));
        });
        rig.backend.set_elevator(elevator(&rig, &confirm));
        assert_eq!(rig.elevate(), Ok(()));
        assert_eq!(*seen.lock().unwrap(), vec![Err(ApiCode::NeedsAdmin)], "asked while asking");
        assert_eq!(rig.widen(), Ok(()), "and passes after the approval");
    }
}
