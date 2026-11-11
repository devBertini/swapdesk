//! The applier's sequence (UPDATES.md 6.2 steps 5 to 7, 6.4), OS-free: the journal and the steps over traits the
//! Windows layer implements (`swapdesk-sys-win` `update_apply.rs`) and tests fake. The rule it keeps: the install
//! folder holds a complete set of files at every moment (each file is replaced, never removed first), and whatever
//! stops the sequence (a service that does not commit, a crash, a power cut) ends in the new version committed or the
//! old one restored.

use std::time::Duration;

use crate::Version;

/// How long the new service has to write `committed` (UPDATES.md 6.2 step 6, GUESS).
pub const COMMIT_WAIT: Duration = Duration::from_secs(60);

/// The journal (`updates\journal`): one line per step, the last one says where the update is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Journal {
    /// The applier started replacing `old` with `new`.
    Applying { old: Version, new: Version },
    /// The new service is up.
    Committed { new: Version },
    /// `new` did not start; `old` was restored.
    Failed { old: Version, new: Version },
}

impl Journal {
    pub fn to_line(&self) -> String {
        match self {
            Self::Applying { old, new } => format!("applying {old} {new}"),
            Self::Committed { new } => format!("committed {new}"),
            Self::Failed { old, new } => format!("failed {old} {new}"),
        }
    }

    /// The last line that reads; `None` for an empty or unreadable journal.
    pub fn last_of(text: &str) -> Option<Journal> {
        text.lines().rev().find_map(|line| {
            let mut w = line.split_whitespace();
            let (kind, a, b) = (w.next()?, w.next(), w.next());
            let v = |s: Option<&str>| s.and_then(|s| Version::parse(s).ok());
            match kind {
                "applying" => Some(Journal::Applying { old: v(a)?, new: v(b)? }),
                "committed" => Some(Journal::Committed { new: v(a)? }),
                "failed" => Some(Journal::Failed { old: v(a)?, new: v(b)? }),
                _ => None,
            }
        })
    }
}

/// What the applier does to the machine (the Windows layer; fakes in tests).
pub trait Machine {
    /// Appends a line to the journal (written through to disk before it returns).
    fn journal(&mut self, entry: &Journal) -> Result<(), String>;
    /// Stops the service (the SCM, 30 s, then the process ends).
    fn stop_service(&mut self) -> Result<(), String>;
    /// Ends every Swapdesk process whose image is inside the install folder.
    fn sweep(&mut self) -> Result<(), String>;
    /// Copies the installed programs into the rollback folder of `old`.
    fn back_up(&mut self, old: &Version) -> Result<(), String>;
    /// Moves each staged file of the new version into the install folder (replace, write-through), resets it to
    /// the folder's ACL and checks it with the install policy.
    fn put_new(&mut self) -> Result<(), String>;
    /// Puts the backed-up files of `old` back the same way.
    fn restore(&mut self, old: &Version) -> Result<(), String>;
    fn start_service(&mut self) -> Result<(), String>;
    /// Waits up to `limit` for the journal's `committed <new>`.
    fn wait_commit(&mut self, new: &Version, limit: Duration) -> bool;
    /// Whether the staged files of the update are still there.
    fn staged(&mut self) -> bool;
    /// Deletes the staging copy (after a commit).
    fn clean_staging(&mut self);
}

/// How an apply ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The new version runs and committed.
    Committed,
    /// The new version did not start; the old one was restored and started.
    RolledBack(String),
    /// The restore failed too: the old files wait in the rollback folder (Event Log 1004; `install.cmd` is the way
    /// out).
    Broken(String),
}

/// Replaces `old` with `new` (UPDATES.md 6.2 step 5 to 7): journal, stop, sweep, back up, move the new files in,
/// start, wait for the commit; any failure after the stop rolls back.
pub fn apply(m: &mut dyn Machine, old: &Version, new: &Version) -> Outcome {
    if let Err(e) = m.journal(&Journal::Applying { old: old.clone(), new: new.clone() }) {
        // Nothing was touched: the old service is still the installed one.
        return Outcome::RolledBack(format!("journal: {e}"));
    }
    // Whether the install folder may hold new files: from the first move on (a move that failed half-way too).
    let mut touched = false;
    let steps = (|| {
        m.stop_service()?;
        m.sweep()?;
        m.back_up(old)?;
        touched = true;
        m.put_new()?;
        m.start_service()
    })();
    match steps {
        Ok(()) if m.wait_commit(new, COMMIT_WAIT) => {
            m.clean_staging();
            Outcome::Committed
        }
        Ok(()) => roll_back(m, old, new, true, "the new service did not commit in time".into()),
        Err(e) => roll_back(m, old, new, touched, e),
    }
}

/// Back to `old`: its files are put back only when the folder may hold new ones (before that they never left).
fn roll_back(m: &mut dyn Machine, old: &Version, new: &Version, touched: bool, why: String) -> Outcome {
    let restored = (|| {
        let _ = m.stop_service();
        let _ = m.sweep();
        if touched {
            m.restore(old)?;
        }
        m.start_service()
    })();
    let _ = m.journal(&Journal::Failed { old: old.clone(), new: new.clone() });
    match restored {
        Ok(()) => Outcome::RolledBack(why),
        Err(e) => Outcome::Broken(format!("{why}; the restore failed too: {e}")),
    }
}

/// What a service that starts does with the journal (UPDATES.md 6.4, a power cut mid-way): `None`, nothing to do;
/// `Some(true)`, finish the apply from staging; `Some(false)`, roll back (staging is gone).
pub fn resume_from(journal: Option<&Journal>, staged: bool) -> Option<bool> {
    match journal {
        Some(Journal::Applying { .. }) => Some(staged),
        _ => None,
    }
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
