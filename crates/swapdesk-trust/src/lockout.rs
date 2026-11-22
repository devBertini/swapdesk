//! Pairing lockout (PROTOCOL.md 3, F-PAIR-02): 3 failed attempts within 15 minutes lock pairing for 15 minutes;
//! each further lockout within 24 hours doubles it, up to 24 hours (GUESS numbers). Pure logic on [`Lockout`]; the
//! caller persists the record before it sends the responder's nonce.

use crate::records::{Lockout, MAX_FAILURES};

pub const FAILURE_WINDOW_MS: u64 = 15 * 60 * 1000;
pub const FAILURES_TO_LOCK: usize = 3;
pub const BASE_LOCK_MS: u64 = 15 * 60 * 1000;
pub const MAX_LOCK_MS: u64 = 24 * 60 * 60 * 1000;
/// A lockout streak is forgotten this long after the last lockout ended.
pub const STREAK_MEMORY_MS: u64 = 24 * 60 * 60 * 1000;

impl Lockout {
    /// Milliseconds of lockout left, if locked at `now`. Never more than the running lockout's own length: the end is
    /// a wall-clock time, and a clock set back would otherwise add the whole jump to it (review of 22/11/2026, A10).
    pub fn remaining_ms(&self, now: u64) -> Option<u64> {
        (self.locked_until_ms > now).then(|| (self.locked_until_ms - now).min(self.lock_length_ms()))
    }

    /// How long the lockout now running (or the last one) lasts: the length `push_failure` gave it.
    fn lock_length_ms(&self) -> u64 {
        let shift = u32::from(self.lockouts_24h.saturating_sub(1)).min(16);
        BASE_LOCK_MS.saturating_mul(1u64 << shift).min(MAX_LOCK_MS)
    }

    /// Whether the record holds times past `now` that only a clock set back explains: a lockout ending later than
    /// its length from now, a failure or a provisional attempt in the future.
    pub fn needs_rebase(&self, now: u64) -> bool {
        self.locked_until_ms > now.saturating_add(self.lock_length_ms())
            || self.failures.iter().any(|&t| t > now)
            || self.provisional.is_some_and(|t| t > now)
    }

    /// Brings the times a clock set back left in the future to `now`: a running lockout then ends one length from
    /// now (never sooner than it would have: what was left is at most that), and a failure counts for the window from
    /// now instead of from a time that may be a year away. Returns whether anything changed, for the caller to write.
    pub fn rebase(&mut self, now: u64) -> bool {
        if !self.needs_rebase(now) {
            return false;
        }
        self.locked_until_ms = self.locked_until_ms.min(now.saturating_add(self.lock_length_ms()));
        for t in &mut self.failures {
            *t = (*t).min(now);
        }
        if let Some(t) = self.provisional.as_mut() {
            *t = (*t).min(now);
        }
        true
    }

    fn prune(&mut self, now: u64) {
        self.failures.retain(|&t| t.saturating_add(FAILURE_WINDOW_MS) > now);
        if self.lockouts_24h != 0 && now >= self.locked_until_ms.saturating_add(STREAK_MEMORY_MS) {
            self.lockouts_24h = 0;
        }
    }

    /// Counts a failed attempt that ended at `at` and locks when the threshold is reached.
    fn push_failure(&mut self, at: u64, now: u64) {
        self.failures.push(at);
        self.prune(now);
        if self.failures.len() > MAX_FAILURES {
            let extra = self.failures.len() - MAX_FAILURES;
            self.failures.drain(..extra);
        }
        if self.failures.len() >= FAILURES_TO_LOCK {
            let shift = u32::from(self.lockouts_24h).min(16);
            let duration = BASE_LOCK_MS.saturating_mul(1u64 << shift).min(MAX_LOCK_MS);
            self.locked_until_ms = at.saturating_add(duration);
            self.lockouts_24h = self.lockouts_24h.saturating_add(1);
            self.failures.clear();
        }
    }

    /// Turns a provisional attempt left by a crash or a dropped task into a counted failure.
    /// Returns true when it caused a lockout that is still running.
    pub fn settle_provisional(&mut self, now: u64) -> bool {
        match self.provisional.take() {
            Some(at) => {
                self.push_failure(at, now);
                self.remaining_ms(now).is_some()
            }
            None => false,
        }
    }

    /// Starts an attempt: the stale provisional failure (if any) is counted, a running lockout refuses it with
    /// the time left, otherwise a provisional failure is set. The caller persists before sending `PairNonce`.
    pub fn begin(&mut self, now: u64) -> Result<(), u64> {
        self.rebase(now);
        self.settle_provisional(now);
        self.prune(now);
        if let Some(left) = self.remaining_ms(now) {
            return Err(left);
        }
        self.provisional = Some(now);
        Ok(())
    }

    /// The attempt ended in a pinning: the provisional failure is cleared.
    pub fn succeed(&mut self) {
        self.provisional = None;
    }

    /// The attempt ended without pinning: the provisional failure becomes a counted one.
    pub fn fail(&mut self, now: u64) {
        self.rebase(now);
        self.provisional = None;
        self.push_failure(now, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60 * 1000;

    fn failed_attempt(l: &mut Lockout, at: u64) {
        l.begin(at).unwrap();
        l.fail(at + 1000);
    }

    #[test]
    fn three_failures_in_fifteen_minutes_lock_for_fifteen() {
        let mut l = Lockout::default();
        failed_attempt(&mut l, 0);
        failed_attempt(&mut l, 2 * MIN);
        assert!(l.remaining_ms(3 * MIN).is_none());
        failed_attempt(&mut l, 4 * MIN);
        assert_eq!(l.remaining_ms(5 * MIN), Some(4 * MIN + 1000 + BASE_LOCK_MS - 5 * MIN));
        assert!(l.begin(6 * MIN).is_err());
        assert!(l.provisional.is_none());
        assert!(l.begin(4 * MIN + 1000 + BASE_LOCK_MS + 1).is_ok());
    }

    #[test]
    fn failures_older_than_the_window_do_not_count() {
        let mut l = Lockout::default();
        failed_attempt(&mut l, 0);
        failed_attempt(&mut l, 10 * MIN);
        failed_attempt(&mut l, 20 * MIN);
        assert!(l.remaining_ms(21 * MIN).is_none(), "the first failure fell out of the window");
    }

    #[test]
    fn a_success_clears_the_provisional_failure() {
        let mut l = Lockout::default();
        l.begin(0).unwrap();
        assert_eq!(l.provisional, Some(0));
        l.succeed();
        assert!(l.provisional.is_none());
        assert!(l.failures.is_empty());
    }

    #[test]
    fn a_leftover_provisional_counts_as_a_failure() {
        let mut l = Lockout::default();
        l.begin(0).unwrap();
        // The process died here. After the restart the next attempt settles it.
        l.begin(MIN).unwrap();
        assert_eq!(l.failures, vec![0]);
        assert_eq!(l.provisional, Some(MIN));
    }

    #[test]
    fn a_second_lockout_within_a_day_doubles_and_the_streak_decays() {
        let mut l = Lockout::default();
        for i in 0..3 {
            failed_attempt(&mut l, i * MIN);
        }
        let first_end = l.locked_until_ms;
        let t = first_end + MIN;
        for i in 0..3 {
            failed_attempt(&mut l, t + i * MIN);
        }
        let second = l.locked_until_ms - (t + 2 * MIN + 1000);
        assert_eq!(second, 2 * BASE_LOCK_MS);
        // A day after the last lockout the streak is forgotten.
        let later = l.locked_until_ms + STREAK_MEMORY_MS + MIN;
        l.begin(later).unwrap();
        assert_eq!(l.lockouts_24h, 0);
    }

    #[test]
    fn the_lockout_never_exceeds_a_day() {
        let mut l = Lockout::default();
        let mut t = 0;
        for _ in 0..12 {
            for _ in 0..3 {
                failed_attempt(&mut l, t);
                t += 1000;
            }
            assert!(l.locked_until_ms - t <= MAX_LOCK_MS);
            t = l.locked_until_ms + 1;
        }
    }

    const DAY: u64 = 24 * 60 * MIN;

    /// Review of 22/11/2026, A10: the lockout ends at a wall-clock time, so a clock set back (a dead CMOS battery, a
    /// manual change, a wrong NTP answer) added the whole jump to it: a year back locked pairing for a year and 15
    /// minutes. No lockout may outlast its own length, whatever the clock did.
    #[test]
    fn a_clock_set_back_never_stretches_the_lockout_past_its_length() {
        let mut l = Lockout::default();
        let t = 400 * DAY;
        for i in 0..3 {
            failed_attempt(&mut l, t + i * MIN);
        }
        let back = t - 365 * DAY;
        let left = l.remaining_ms(back);
        assert!(left.is_some_and(|ms| ms <= BASE_LOCK_MS), "a year back: {left:?} ms left");
        let refused = l.begin(back);
        assert!(refused.is_err_and(|ms| ms <= BASE_LOCK_MS), "{refused:?}");
    }

    /// Review of 22/11/2026, A10: bounded is not enough, the time left must also run out. Rebased at the jump, the
    /// lockout counts down from there and ends one length later; the failures stamped before the jump count for the
    /// window from the jump, not for a year.
    #[test]
    fn a_rebased_lockout_counts_down_from_the_jump_and_old_failures_leave_the_window() {
        let mut l = Lockout::default();
        let t = 400 * DAY;
        for i in 0..3 {
            failed_attempt(&mut l, t + i * MIN);
        }
        let back = t - 365 * DAY;
        assert!(l.needs_rebase(back));
        assert!(l.rebase(back));
        assert!(!l.needs_rebase(back) && !l.rebase(back), "a second rebase changes nothing");
        assert_eq!(l.remaining_ms(back + BASE_LOCK_MS / 2), Some(BASE_LOCK_MS / 2));
        assert!(l.begin(back + BASE_LOCK_MS).is_ok(), "over one lockout length after the jump");
        l.succeed();

        // Two failures, then the clock goes back a year: they count from the jump, so a third one after the window
        // locks nothing.
        let mut l = Lockout::default();
        failed_attempt(&mut l, t);
        failed_attempt(&mut l, t + MIN);
        assert!(l.rebase(back));
        failed_attempt(&mut l, back + FAILURE_WINDOW_MS + MIN);
        assert!(l.remaining_ms(back + FAILURE_WINDOW_MS + 2 * MIN).is_none(), "{l:?}");
        // A clock that only moves forward is never rebased.
        assert!(!l.rebase(back + FAILURE_WINDOW_MS + 3 * MIN));
    }

    #[test]
    fn the_record_survives_the_codec() {
        use swapdesk_proto::Wire;
        let mut l = Lockout::default();
        failed_attempt(&mut l, 5);
        l.begin(10 * MIN).unwrap();
        assert_eq!(Lockout::decode(&l.encode().unwrap()).unwrap(), l);
    }
}
