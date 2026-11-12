//! The reconnect proof across machines (F-XNET-01): the link drops for about 10 s during a session and returns.
//!
//! The orchestrator cuts and restores the network on one of the machines (the Mac's Wi-Fi, from a job scheduled on
//! the Mac so that the command that started it does not need the network); this side watches its own session through
//! its UI backend and its decoder, and they meet through files in the run folder:
//!
//! 1. after the first picture this side writes `reconnect-ready`;
//! 2. the orchestrator cuts the link, waits, restores it and, once the machine has an address again, writes
//!    `reconnect-up`;
//! 3. this side must have seen the session go `Reconnecting` and then `Active` again (the same session, no new
//!    prompt), and must decode a picture within [`BUDGET`] of `reconnect-up` (budget B5, F-XNET-01). The moment of
//!    the return is the file's modification time, so the polling of this side does not add to it.
//!
//! Marker: `RECONNECT_OK` with the measured times.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use swapdesk_ui::{Role, SessionState, UiEvent};

use crate::pump::Pump;

/// How soon after the link returns a picture must be decoded.
pub const BUDGET: Duration = Duration::from_secs(3);

/// What the run measured; each is `None` when it never happened.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Times {
    /// From `reconnect-ready` to the session going `Reconnecting`.
    pub reconnecting_after: Option<Duration>,
    /// From the link's return to the session being `Active` again.
    pub active_after_up: Option<Duration>,
    /// From the link's return to a decoded picture.
    pub picture_after_up: Option<Duration>,
}

/// The verdict on what was measured.
pub fn judge(t: &Times, budget: Duration) -> Result<(), String> {
    if t.reconnecting_after.is_none() {
        return Err("the session never went Reconnecting".into());
    }
    let Some(active) = t.active_after_up else { return Err("the session never went Active again".into()) };
    match t.picture_after_up {
        None => Err(format!("no picture was decoded within {} ms of the link's return", budget.as_millis())),
        Some(p) if p > budget => Err(format!(
            "the first picture after the link's return took {} ms (budget {} ms; Active again after {} ms)",
            p.as_millis(),
            budget.as_millis(),
            active.as_millis()
        )),
        Some(_) => Ok(()),
    }
}

/// `RECONNECT_OK` with the times in milliseconds.
pub fn ok_line(t: &Times) -> String {
    let ms = |d: Option<Duration>| d.map_or("none".to_string(), |d| d.as_millis().to_string());
    format!(
        "RECONNECT_OK reconnecting_ms={} active_after_up_ms={} picture_after_up_ms={}",
        ms(t.reconnecting_after),
        ms(t.active_after_up),
        ms(t.picture_after_up)
    )
}

/// When the file was written, as an `Instant`: its age by the system clock, no more than `at_most`.
fn written_at(path: &Path, at_most: Duration) -> Instant {
    let age = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or(Duration::ZERO)
        .min(at_most);
    Instant::now() - age
}

/// Runs the proof. `last_decoded` says when the last picture was decoded; `tick` runs on every turn of the wait (the
/// caller asks for a keyframe there when its decoder lost its place).
pub fn reconnect_round(
    pump: &mut Pump<'_>,
    session_id: u64,
    dir: &Path,
    timeout: Duration,
    last_decoded: &dyn Fn() -> Option<Instant>,
    tick: &mut dyn FnMut(),
    log: &dyn Fn(&str),
) -> Result<Times, String> {
    let mut cursor = 0;
    pump.fresh(&mut cursor); // what came before is not the drop
    let up_file = dir.join("reconnect-up");
    let _ = std::fs::remove_file(&up_file);
    let ready_at = Instant::now();
    std::fs::write(dir.join("reconnect-ready"), "ready").map_err(|e| e.to_string())?;
    log("VIEWER_RECONNECT_READY");

    let mut t = Times::default();
    let (mut reconnecting_at, mut active_at, mut up_at) = (None::<Instant>, None::<Instant>, None::<Instant>);
    let deadline = ready_at + timeout;
    loop {
        tick();
        for e in pump.fresh(&mut cursor) {
            if let UiEvent::Session { session_id: s, state, .. } = e
                && s == session_id
            {
                match state {
                    SessionState::Reconnecting { .. } if reconnecting_at.is_none() => {
                        reconnecting_at = Some(Instant::now());
                        log("VIEWER_RECONNECTING");
                    }
                    SessionState::Active { role: Role::Viewer, .. }
                        if reconnecting_at.is_some() && active_at.is_none() =>
                    {
                        active_at = Some(Instant::now());
                        log("VIEWER_ACTIVE_AGAIN");
                    }
                    SessionState::Ended { reason } => {
                        return Err(format!("the session ended during the drop ({reason:?}) instead of coming back"));
                    }
                    _ => {}
                }
            }
        }
        if up_at.is_none() && std::fs::read_to_string(&up_file).is_ok_and(|s| !s.trim().is_empty()) {
            up_at = Some(written_at(&up_file, ready_at.elapsed()));
            log("VIEWER_LINK_UP");
        }
        if let Some(up) = up_at {
            let picture = last_decoded().filter(|d| *d >= up);
            if let (Some(p), Some(a), Some(_)) = (picture, active_at, reconnecting_at) {
                t.picture_after_up = Some(p - up);
                t.active_after_up = Some(a.saturating_duration_since(up));
                break;
            }
            if up.elapsed() > BUDGET + Duration::from_millis(500) {
                // Out of budget: report what was seen.
                t.picture_after_up = picture.map(|p| p - up);
                t.active_after_up = active_at.map(|a| a.saturating_duration_since(up));
                break;
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out after {}s waiting for the orchestrator's reconnect-up ({})",
                timeout.as_secs(),
                up_file.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    t.reconnecting_after = reconnecting_at.map(|r| r.saturating_duration_since(ready_at));
    judge(&t, BUDGET)?;
    log(&ok_line(&t));
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Option<Duration> = |n| Some(Duration::from_millis(n));

    #[test]
    fn a_picture_inside_the_budget_after_a_reconnect_passes() {
        let t = Times { reconnecting_after: MS(4200), active_after_up: MS(900), picture_after_up: MS(1400) };
        assert_eq!(judge(&t, BUDGET), Ok(()));
        assert_eq!(ok_line(&t), "RECONNECT_OK reconnecting_ms=4200 active_after_up_ms=900 picture_after_up_ms=1400");
    }

    #[test]
    fn each_missing_step_and_a_late_picture_fail_with_their_own_reason() {
        let ok = Times { reconnecting_after: MS(1), active_after_up: MS(1), picture_after_up: MS(1) };
        assert!(judge(&Times { reconnecting_after: None, ..ok }, BUDGET).unwrap_err().contains("Reconnecting"));
        assert!(judge(&Times { active_after_up: None, ..ok }, BUDGET).unwrap_err().contains("Active again"));
        assert!(judge(&Times { picture_after_up: None, ..ok }, BUDGET).unwrap_err().contains("no picture"));
        let late = Times { picture_after_up: MS(3001), ..ok };
        assert!(judge(&late, BUDGET).unwrap_err().contains("3001 ms"));
        assert_eq!(judge(&Times { picture_after_up: MS(3000), ..ok }, BUDGET), Ok(()));
    }
}
