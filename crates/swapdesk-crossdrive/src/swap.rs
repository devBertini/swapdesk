//! The swap proof across machines (F-XSWAP-01), the viewer's half; the host's is in [`crate::host`].
//!
//! During a session the viewer asks for a swap exactly as the toolbar does (`RequestSwap`); the host's scripted user
//! accepts the question the way a person would (`AnswerSwap`); the viewer's machine becomes the host and the
//! host's the viewer, over a new connection with the pinned keys (D1), and nobody pairs or answers a prompt.
//!
//! The viewer's side of it, with its own clock:
//!
//! * the future host (this side) must have granted `CONTROL` to the other device in its own trust store (PROTOCOL.md
//!   6.3 step 1), so the proof sets that first;
//! * `SWAP_VIEWER_REQUESTED`; then the swap card closes (`UiEvent::Swap { Done }`: the swap is under way) and the new
//!   session this machine hosts goes active. The time from the first to the second is the viewer's reading of the
//!   reverse session opening (`SWAP_VIEWER_OK done_to_hosting_ms=`, budget [`BUDGET`]); the time from the request is
//!   logged too, but it holds the host's answer (the script's, almost at once);
//! * no `Pairing` event and no `IncomingRequest` may show up meanwhile.
//!
//! The host's reading (accept to its new viewer session active) is `HOST_SWAP_VIEWING` in the host's log; the
//! orchestrator prints `SWAP_OK` only when both are inside the budget. Both measure the session going active, not the
//! first presented frame (budget B4's end), which neither scripted side renders.

use std::time::{Duration, Instant};

use swapdesk_ui::{Command, Permission, Permissions, Role, SessionState, SwapEvent, UiEvent};

use crate::pump::Pump;

/// How long the reverse session may take to open once the swap is under way (B4).
pub const BUDGET: Duration = Duration::from_secs(1);
const REPLY: Duration = Duration::from_secs(10);
/// How long the host's answer and the new session may take altogether.
const TOTAL: Duration = Duration::from_secs(30);

/// What the viewer measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Times {
    pub done_to_hosting: Duration,
    pub from_request: Duration,
}

/// The verdict on the viewer's reading.
pub fn judge(t: &Times, budget: Duration) -> Result<(), String> {
    if t.done_to_hosting > budget {
        return Err(format!(
            "the reverse session took {} ms to open after the swap was under way (budget {} ms)",
            t.done_to_hosting.as_millis(),
            budget.as_millis()
        ));
    }
    Ok(())
}

/// Runs the viewer's half on viewer session `session_id` toward paired device `device_id`. Returns the id of the new
/// session this machine now hosts; the caller ends it.
pub fn swap_round(pump: &mut Pump<'_>, session_id: u64, device_id: &str, log: &dyn Fn(&str)) -> Result<u64, String> {
    // The future host has granted CONTROL to the other in its own store.
    pump.ask(
        Command::SetGrants {
            device_id: device_id.to_string(),
            grants: Permissions::NONE.with(Permission::Control).with(Permission::Audio),
        },
        REPLY,
    )?;
    let mut cursor = 0;
    pump.fresh(&mut cursor);
    let requested = Instant::now();
    pump.ask(Command::RequestSwap { session_id }, REPLY)?;
    log("SWAP_VIEWER_REQUESTED");

    let (mut done_at, mut hosting) = (None::<Instant>, None::<(u64, Instant)>);
    let mut old_ended = None;
    let mut prompts: Vec<String> = Vec::new();
    let deadline = requested + TOTAL;
    loop {
        for e in pump.fresh(&mut cursor) {
            match e {
                UiEvent::Swap { session_id: s, event } if s == session_id => match event {
                    SwapEvent::Declined { why } => return Err(format!("the swap was not done: {why:?}")),
                    SwapEvent::Done if done_at.is_none() => {
                        done_at = Some(Instant::now());
                        log("SWAP_VIEWER_DONE the swap card closed: the swap is under way");
                    }
                    _ => {}
                },
                UiEvent::Session {
                    session_id: s,
                    device_id: d,
                    state: SessionState::Active { role: Role::Host, .. },
                } if s != session_id && d == device_id && hosting.is_none() => {
                    hosting = Some((s, Instant::now()));
                    log(&format!("SWAP_VIEWER_HOSTING new_session={s}"));
                }
                UiEvent::Session { session_id: s, state: SessionState::Ended { reason }, .. }
                    if s == session_id && old_ended.is_none() =>
                {
                    old_ended = Some(reason);
                    log(&format!("SWAP_VIEWER_OLD_SESSION_ENDED reason={reason:?}"));
                }
                UiEvent::Pairing { pairing_id, .. } => prompts.push(format!("a pairing ({pairing_id})")),
                UiEvent::IncomingRequest(r) => prompts.push(format!("a request ({})", r.request_id)),
                _ => {}
            }
        }
        if hosting.is_some() && old_ended.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out after {}s: the new session {}, the old one {}",
                TOTAL.as_secs(),
                if hosting.is_some() { "opened" } else { "never opened" },
                if old_ended.is_some() { "ended" } else { "never ended" }
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let (new_session, hosting_at) = hosting.expect("checked");
    if !prompts.is_empty() {
        return Err(format!("the swap needed {}", prompts.join(", ")));
    }
    let times = Times {
        done_to_hosting: done_at.map_or(Duration::ZERO, |d| hosting_at.saturating_duration_since(d)),
        from_request: hosting_at.saturating_duration_since(requested),
    };
    judge(&times, BUDGET)?;
    log(&format!(
        "SWAP_VIEWER_OK new_session={new_session} done_to_hosting_ms={} from_request_ms={} no_pairing=true",
        times.done_to_hosting.as_millis(),
        times.from_request.as_millis()
    ));
    Ok(new_session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reverse_session_inside_the_budget_passes_and_a_late_one_fails() {
        let ok = Times { done_to_hosting: Duration::from_millis(1000), from_request: Duration::from_millis(1400) };
        assert_eq!(judge(&ok, BUDGET), Ok(()));
        let late = Times { done_to_hosting: Duration::from_millis(1001), ..ok };
        assert!(judge(&late, BUDGET).unwrap_err().contains("1001 ms"));
    }
}
