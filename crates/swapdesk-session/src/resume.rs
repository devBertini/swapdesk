//! Resume tokens (PROTOCOL.md 6.4, F-NET-01) and swap tokens (6.3 step 4). Pure tables clocked by the caller.
//!
//! A resume token lets the same viewer device resume the same session without a prompt, but only after the
//! connection was lost, within the window after the loss, and once. It dies with a `Bye`, a "Desconectar", a grant
//! change, Forget and lock-on-end. A resumed session gets the original grants intersected with the grants still
//! stored, and a fresh token.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use swapdesk_proto::PermissionSet;
use swapdesk_trust::DeviceId;

use crate::util::{random_token, tokens_equal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeError {
    /// No such session (never existed, ended, or the host rebooted).
    Unknown,
    /// The token belongs to another device. The token is not consumed.
    WrongDevice,
    /// The token does not match (a used token, or a guess). Nothing is consumed.
    BadToken,
    /// The session's connection is not known to be lost yet.
    NotLost,
    /// More than the window passed since the loss.
    Expired,
}

struct Entry {
    device: DeviceId,
    granted: PermissionSet,
    /// The session entered without a prompt: a resume of it is still such a session.
    unattended: bool,
    token: Option<[u8; 16]>,
    lost_at: Option<Instant>,
}

pub struct ResumeTable {
    window: Duration,
    entries: HashMap<u64, Entry>,
}

impl ResumeTable {
    pub fn new(window: Duration) -> Self {
        Self { window, entries: HashMap::new() }
    }

    /// A session started or was resumed with a live connection. Returns its fresh token. `unattended` says it
    /// entered without a prompt.
    pub fn insert(&mut self, session_id: u64, device: DeviceId, granted: PermissionSet, unattended: bool) -> [u8; 16] {
        let token = random_token();
        self.entries.insert(session_id, Entry { device, granted, unattended, token: Some(token), lost_at: None });
        token
    }

    /// Whether the session `session_id` entered without a prompt (false for one the table does not hold).
    pub fn entered_unattended(&self, session_id: u64) -> bool {
        self.entries.get(&session_id).is_some_and(|e| e.unattended)
    }

    /// The connection was lost (an idle timeout or a transport error): the token becomes usable.
    pub fn mark_lost(&mut self, session_id: u64, now: Instant) {
        if let Some(e) = self.entries.get_mut(&session_id) {
            e.lost_at = Some(now);
        }
    }

    /// The token dies (`Bye`, "Desconectar", a grant change, lock-on-end); the entry goes with it.
    pub fn invalidate(&mut self, session_id: u64) {
        self.entries.remove(&session_id);
    }

    /// Forget: every token of the device dies.
    pub fn invalidate_device(&mut self, device: &DeviceId) {
        self.entries.retain(|_, e| &e.device != device);
    }

    /// The host rebooted or shut down: every token dies.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drops the entries whose window has passed.
    pub fn purge(&mut self, now: Instant) {
        let window = self.window;
        self.entries.retain(|_, e| e.lost_at.is_none_or(|t| now.saturating_duration_since(t) <= window));
    }

    pub fn contains(&self, session_id: u64) -> bool {
        self.entries.contains_key(&session_id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `token` would be accepted from `device` now, without using it. `Err(NotLost)` means "ask again
    /// when the old connection has been seen lost".
    pub fn check(&self, now: Instant, device: &DeviceId, session_id: u64, token: &[u8; 16]) -> Result<(), ResumeError> {
        let e = self.entries.get(&session_id).ok_or(ResumeError::Unknown)?;
        if &e.device != device {
            return Err(ResumeError::WrongDevice);
        }
        match &e.token {
            Some(t) if tokens_equal(t, token) => {}
            _ => return Err(ResumeError::BadToken),
        }
        match e.lost_at {
            None => Err(ResumeError::NotLost),
            Some(t) if now.saturating_duration_since(t) > self.window => Err(ResumeError::Expired),
            Some(_) => Ok(()),
        }
    }

    /// Uses the token: single use. Returns the grants of the resumed session (the original ones intersected with
    /// `stored_now`) and the session's new token; the old token is dead from here on.
    pub fn resume(
        &mut self,
        now: Instant,
        device: &DeviceId,
        session_id: u64,
        token: &[u8; 16],
        stored_now: PermissionSet,
    ) -> Result<(PermissionSet, [u8; 16]), ResumeError> {
        match self.check(now, device, session_id, token) {
            Ok(()) => {}
            Err(ResumeError::Expired) => {
                self.entries.remove(&session_id);
                return Err(ResumeError::Expired);
            }
            Err(e) => return Err(e),
        }
        let Some(e) = self.entries.get_mut(&session_id) else { return Err(ResumeError::Unknown) };
        let granted = crate::policy::resumed_grant(e.granted, stored_now);
        let fresh = random_token();
        e.granted = granted;
        e.token = Some(fresh);
        e.lost_at = None;
        Ok((granted, fresh))
    }
}

/// Swap tokens (6.3 step 4): armed by the future host for the future viewer's device ID, valid 10 s, single use.
pub struct SwapTokens {
    ttl: Duration,
    armed: Vec<([u8; 16], DeviceId, Instant)>,
}

impl SwapTokens {
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, armed: Vec::new() }
    }

    pub fn arm(&mut self, now: Instant, device: DeviceId, token: [u8; 16]) {
        self.armed.retain(|(_, _, at)| now.saturating_duration_since(*at) <= self.ttl);
        self.armed.push((token, device, now));
    }

    /// Consumes the token if it is armed for `device` and fresh. A token for another device stays armed.
    pub fn take(&mut self, now: Instant, device: &DeviceId, token: &[u8; 16]) -> bool {
        self.armed.retain(|(_, _, at)| now.saturating_duration_since(*at) <= self.ttl);
        match self.armed.iter().position(|(t, d, _)| d == device && tokens_equal(t, token)) {
            Some(i) => {
                self.armed.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// Forget or shutdown: every armed token of the device dies.
    pub fn disarm_device(&mut self, device: &DeviceId) {
        self.armed.retain(|(_, d, _)| d != device);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: DeviceId = [1; 16];
    const B: DeviceId = [2; 16];
    const WINDOW: Duration = Duration::from_secs(60);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn table() -> (ResumeTable, Instant, [u8; 16]) {
        let mut t = ResumeTable::new(WINDOW);
        let tok = t.insert(7, A, PermissionSet::CONTROL | PermissionSet::FILES, false);
        (t, Instant::now(), tok)
    }

    #[test]
    fn a_token_works_only_after_the_connection_was_lost() {
        let (mut t, now, tok) = table();
        assert_eq!(
            t.resume(now, &A, 7, &tok, PermissionSet::CONTROL | PermissionSet::FILES),
            Err(ResumeError::NotLost)
        );
        t.mark_lost(7, now);
        assert!(t.resume(now + secs(1), &A, 7, &tok, PermissionSet::CONTROL | PermissionSet::FILES).is_ok());
    }

    #[test]
    fn a_token_is_single_use_and_the_new_one_works_after_another_loss() {
        let (mut t, now, tok) = table();
        t.mark_lost(7, now);
        let stored = PermissionSet::CONTROL | PermissionSet::FILES;
        let (_, fresh) = t.resume(now, &A, 7, &tok, stored).unwrap();
        assert_ne!(fresh, tok);
        t.mark_lost(7, now + secs(5));
        assert_eq!(t.resume(now + secs(6), &A, 7, &tok, stored), Err(ResumeError::BadToken), "the used token is dead");
        assert!(
            t.resume(now + secs(6), &A, 7, &fresh, stored).is_ok(),
            "the fresh one works once more after another loss"
        );
    }

    #[test]
    fn a_token_is_bound_to_its_device_and_a_wrong_try_does_not_burn_it() {
        let (mut t, now, tok) = table();
        t.mark_lost(7, now);
        let stored = PermissionSet::CONTROL | PermissionSet::FILES;
        assert_eq!(t.resume(now, &B, 7, &tok, stored), Err(ResumeError::WrongDevice));
        let mut bad = tok;
        bad[0] ^= 1;
        assert_eq!(t.resume(now, &A, 7, &bad, stored), Err(ResumeError::BadToken));
        assert_eq!(t.resume(now, &A, 99, &tok, stored), Err(ResumeError::Unknown));
        assert!(t.resume(now, &A, 7, &tok, stored).is_ok(), "the owner can still resume");
    }

    #[test]
    fn a_token_lasts_60_seconds_from_the_loss() {
        let (mut t, now, tok) = table();
        t.mark_lost(7, now);
        let stored = PermissionSet::CONTROL | PermissionSet::FILES;
        assert!(t.check(now + secs(60), &A, 7, &tok).is_ok());
        assert_eq!(t.resume(now + secs(61), &A, 7, &tok, stored), Err(ResumeError::Expired));
        assert_eq!(
            t.resume(now + secs(62), &A, 7, &tok, stored),
            Err(ResumeError::Unknown),
            "an expired entry is gone"
        );
    }

    #[test]
    fn each_invalidation_kills_the_token() {
        let stored = PermissionSet::CONTROL | PermissionSet::FILES;
        // Bye, "Desconectar", a grant change and lock-on-end are `invalidate`; Forget is `invalidate_device`;
        // a reboot is `clear`.
        for case in 0..3 {
            let (mut t, now, tok) = table();
            t.mark_lost(7, now);
            match case {
                0 => t.invalidate(7),
                1 => t.invalidate_device(&A),
                _ => t.clear(),
            }
            assert_eq!(t.resume(now, &A, 7, &tok, stored), Err(ResumeError::Unknown), "case {case}");
        }
        // Forgetting another device leaves it alone.
        let (mut t, now, tok) = table();
        t.mark_lost(7, now);
        t.invalidate_device(&B);
        assert!(t.resume(now, &A, 7, &tok, stored).is_ok());
    }

    #[test]
    fn resumed_grants_are_the_original_intersected_with_what_is_still_stored() {
        let (mut t, now, tok) = table();
        t.mark_lost(7, now);
        // The device still holds CONTROL but FILES was removed in Permissões.
        let (granted, _) = t.resume(now, &A, 7, &tok, PermissionSet::CONTROL | PermissionSet::TERMINAL).unwrap();
        assert_eq!(
            granted,
            PermissionSet::CONTROL,
            "FILES is gone, TERMINAL was never in the session, so it is not added"
        );
    }

    #[test]
    fn purge_drops_expired_entries_only() {
        let mut t = ResumeTable::new(WINDOW);
        t.insert(1, A, PermissionSet::VIEW, false);
        t.insert(2, A, PermissionSet::VIEW, true);
        let now = Instant::now();
        t.mark_lost(1, now);
        t.purge(now + secs(30));
        assert_eq!(t.len(), 2);
        t.purge(now + secs(61));
        assert_eq!(t.len(), 1, "the live session stays");
    }

    #[test]
    fn swap_tokens_are_device_bound_single_use_and_last_10_seconds() {
        let mut s = SwapTokens::new(secs(10));
        let now = Instant::now();
        let tok = random_token();
        s.arm(now, A, tok);
        assert!(!s.take(now, &B, &tok), "another device cannot use it");
        let mut other = tok;
        other[3] ^= 1;
        assert!(!s.take(now, &A, &other));
        assert!(s.take(now + secs(10), &A, &tok));
        assert!(!s.take(now + secs(10), &A, &tok), "single use");
        s.arm(now, A, tok);
        assert!(!s.take(now + secs(11), &A, &tok), "expired");
        s.arm(now, A, tok);
        s.disarm_device(&A);
        assert!(!s.take(now, &A, &tok));
    }
}
