//! The reconnect policy (F-NET-01, PROTOCOL.md 6.4). When a session ends as `Lost` (network drop, sleep, IP change),
//! the viewer shows "Reconectando" with the attempt number and dials again with `Hello { resume }` until the host
//! takes it back or 60 s (the token's life) have passed. The attempts back off 0.2 s, 0.4 s, ... up to 2 s so a
//! network that returns is noticed inside budget B5 (3 s).
//!
//! Nothing here opens sockets: the caller passes a `dial` closure (the service's connect with the stored address),
//! so the policy runs the same over QUIC, over the in-memory link of the tests, and for an unattended host that
//! rebooted (its token is gone: the same dial is accepted through `UNATTENDED`, no prompt).

use std::future::Future;
use std::time::Duration;

use swapdesk_proto::{ErrorCode, GrantMode, Resume};
use swapdesk_session::{EndKind, OpenError, SessionEnd, ViewerSession};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    /// Total time from the loss (the resume token lives 60 s).
    pub window: Duration,
    pub first_delay: Duration,
    pub max_delay: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            first_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(2),
        }
    }
}

impl ReconnectPolicy {
    /// The wait after failed attempt number `attempt` (1-based).
    pub fn delay_after(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        self.first_delay.saturating_mul(1 << shift).min(self.max_delay)
    }
}

/// Only a lost connection is resumed: a `Bye` from either side, a refusal, a violation or a timeout ends for good.
pub fn should_reconnect(end: &SessionEnd) -> bool {
    end.kind == EndKind::Lost
}

/// What the UI shows ("Reconectando N") and what the activity log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectEvent {
    Reconnecting { attempt: u32 },
    Back { attempt: u32, mode: GrantMode },
    GaveUp(GiveUp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// 60 s passed without the host taking the session back.
    WindowElapsed,
    /// The host answered no (declined, nobody present, not permitted, unknown device): retrying cannot change it.
    Refused(ErrorCode),
    /// The host broke the protocol.
    Protocol(ErrorCode),
}

/// Retries `dial` until it opens a session, the answer is final, or the window ends. `resume` is the token of the
/// lost session (`GrantInfo::resume`); a failed attempt does not burn it, so every attempt sends the same one.
pub async fn reconnect<D, Fut>(
    policy: ReconnectPolicy,
    resume: Resume,
    mut dial: D,
    mut notify: impl FnMut(ReconnectEvent),
) -> Result<ViewerSession, GiveUp>
where
    D: FnMut(Resume) -> Fut,
    Fut: Future<Output = Result<ViewerSession, OpenError>>,
{
    let deadline = Instant::now() + policy.window;
    let mut attempt = 0u32;
    loop {
        if Instant::now() >= deadline {
            notify(ReconnectEvent::GaveUp(GiveUp::WindowElapsed));
            return Err(GiveUp::WindowElapsed);
        }
        attempt += 1;
        notify(ReconnectEvent::Reconnecting { attempt });
        let result = match tokio::time::timeout_at(deadline, dial(resume.clone())).await {
            Ok(r) => r,
            Err(_) => Err(OpenError::Timeout),
        };
        let give_up = match result {
            Ok(session) => {
                notify(ReconnectEvent::Back { attempt, mode: session.grant().mode });
                return Ok(session);
            }
            // The host is still unreachable, or has not noticed the loss yet: try again.
            Err(OpenError::Lost | OpenError::Timeout | OpenError::Closed(_) | OpenError::Refused(ErrorCode::Busy)) => {
                None
            }
            Err(OpenError::Refused(code)) => Some(GiveUp::Refused(code)),
            Err(OpenError::Protocol(code)) => Some(GiveUp::Protocol(code)),
        };
        if let Some(g) = give_up {
            notify(ReconnectEvent::GaveUp(g));
            return Err(g);
        }
        tokio::time::sleep(policy.delay_after(attempt)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_up_to_the_cap() {
        let p = ReconnectPolicy::default();
        let ms: Vec<u128> = (1..=6).map(|n| p.delay_after(n).as_millis()).collect();
        assert_eq!(ms, [200, 400, 800, 1600, 2000, 2000]);
        assert_eq!(p.delay_after(u32::MAX), Duration::from_secs(2));
    }

    #[test]
    fn only_a_lost_connection_is_retried() {
        let end = |kind| SessionEnd { kind, code: ErrorCode::NoError };
        assert!(should_reconnect(&end(EndKind::Lost)));
        for k in [
            EndKind::LocalBye,
            EndKind::PeerBye,
            EndKind::Refused,
            EndKind::Violation,
            EndKind::Timeout,
            EndKind::PeerClosed,
        ] {
            assert!(!should_reconnect(&end(k)), "{k:?}");
        }
    }
}
