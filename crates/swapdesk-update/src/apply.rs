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
    /// `new` started and did not commit; `old` was restored.
    Failed { old: Version, new: Version },
    /// `new` never started: the apply stopped for `why` (a file held open, a step that failed) and the install folder
    /// holds `old` again. Unlike [`Journal::Failed`], `new` stays on offer: it never ran, so nothing says it is bad.
    Refused { old: Version, new: Version, why: String },
    /// `new` did not start and the restore of `old` failed too: the rollback folder is kept for `install.cmd`.
    Broken { old: Version, new: Version },
}

impl Journal {
    pub fn to_line(&self) -> String {
        match self {
            Self::Applying { old, new } => format!("applying {old} {new}"),
            Self::Committed { new } => format!("committed {new}"),
            Self::Failed { old, new } => format!("failed {old} {new}"),
            Self::Refused { old, new, why } => {
                format!("refused {old} {new} {}", why.split_whitespace().collect::<Vec<_>>().join(" "))
            }
            Self::Broken { old, new } => format!("broken {old} {new}"),
        }
    }

    /// The last line that reads; `None` for an empty or unreadable journal.
    pub fn last_of(text: &str) -> Option<Journal> {
        text.lines().rev().find_map(|line| {
            let mut w = line.trim().splitn(4, ' ');
            let (kind, a, b, rest) = (w.next()?, w.next(), w.next(), w.next());
            let v = |s: Option<&str>| s.and_then(|s| Version::parse(s).ok());
            match kind {
                "applying" => Some(Journal::Applying { old: v(a)?, new: v(b)? }),
                "committed" => Some(Journal::Committed { new: v(a)? }),
                "failed" => Some(Journal::Failed { old: v(a)?, new: v(b)? }),
                "refused" => Some(Journal::Refused { old: v(a)?, new: v(b)?, why: rest.unwrap_or_default().into() }),
                "broken" => Some(Journal::Broken { old: v(a)?, new: v(b)? }),
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
    /// Takes hold of every installed file the staged set will replace, so that no other process can keep one open in a
    /// way that stops its replacement, and keeps the hold until [`Machine::start_service`]. An `Err` (a file is held
    /// by another program) comes while nothing in the install folder has changed.
    fn claim_targets(&mut self) -> Result<(), String>;
    /// Whether the install folder holds a complete set of programs (of whichever version): a service started over it
    /// runs, so a failed restore still starts one.
    fn install_complete(&mut self) -> bool;
    /// Copies the installed files into the rollback folder of `old`, unless a complete copy is there already (an apply
    /// resumed after a power cut must keep the copy of the first try, made before any file was replaced).
    fn back_up(&mut self, old: &Version) -> Result<(), String>;
    /// Puts each staged file of the new version into the install folder (replace, write-through), resets it to the
    /// folder's ACL and checks it with the install policy. The staged files stay until [`Machine::clean_staging`], so
    /// an apply cut short can run again from the start.
    fn put_new(&mut self) -> Result<(), String>;
    /// Puts the backed-up files of `old` back the same way (the backup stays).
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
    /// The new version started, did not commit, and the old one was restored and started.
    RolledBack(String),
    /// The new version never started (the apply stopped for the reason given); the install folder holds the old one
    /// again, its service runs, and the new version stays on offer.
    Refused(String),
    /// The restore failed too: the old files wait in the rollback folder (Event Log 1004; `install.cmd` is the way
    /// out).
    Broken(String),
}

/// Replaces `old` with `new` (UPDATES.md 6.2 step 5 to 7): journal, stop, sweep, claim every file to replace, back up,
/// move the new files in, start, wait for the commit; any failure after the stop puts the old version back and starts
/// its service. Whether the new version is then held against is decided by whether it ever started: a failure before
/// that is [`Outcome::Refused`] (the version stays on offer), a start that failed or a commit that never came is
/// [`Outcome::RolledBack`].
pub fn apply(m: &mut dyn Machine, old: &Version, new: &Version) -> Outcome {
    if let Err(e) = m.journal(&Journal::Applying { old: old.clone(), new: new.clone() }) {
        // Nothing was touched: the old service is still the installed one.
        return Outcome::Refused(format!("journal: {e}"));
    }
    // How far the sequence went: whether the install folder may hold new files (from the first move on, a move that
    // failed half-way too) and whether the new service was asked to start.
    let (mut touched, mut started) = (false, false);
    let steps = (|| {
        m.stop_service()?;
        m.sweep()?;
        // Every file to replace is held before the first byte changes: a program that keeps one open stops the
        // apply here, with nothing touched.
        m.claim_targets()?;
        m.back_up(old)?;
        touched = true;
        m.put_new()?;
        started = true;
        m.start_service()
    })();
    match steps {
        Ok(()) if m.wait_commit(new, COMMIT_WAIT) => {
            m.clean_staging();
            Outcome::Committed
        }
        Ok(()) => roll_back(m, old, new, (true, true), "the new service did not commit in time".into()),
        Err(e) => roll_back(m, old, new, (touched, started), e),
    }
}

/// Restores `old` after an apply that was cut short and cannot be finished (its staged files are gone): the start
/// of a service that finds `applying` with no applier alive and nothing staged ([`at_start`]). Whether the new version
/// ran is unknown: it is held against, the safe side.
pub fn roll_back_only(m: &mut dyn Machine, old: &Version, new: &Version) -> Outcome {
    roll_back(m, old, new, (true, true), "the update was cut short and its files are gone".into())
}

/// Back to `old`: its files are put back only when the folder may hold new ones (before that they never left;
/// `touched`). The journal says `failed` (the new version started: `started`) or `refused` (it never did) before the
/// old service starts, so that service does not read `applying` and resume the apply; a restore that fails says
/// `broken`, which keeps the rollback folder. A service is started whenever the restore worked or the install folder
/// holds a complete set anyway: nothing restarts a service that was stopped cleanly, so a failed restore must not leave
/// the machine without one.
fn roll_back(
    m: &mut dyn Machine,
    old: &Version,
    new: &Version,
    (touched, started): (bool, bool),
    why: String,
) -> Outcome {
    let _ = m.stop_service();
    let _ = m.sweep();
    let restored = if touched { m.restore(old) } else { Ok(()) };
    let verdict = match (&restored, started) {
        (Err(_), _) => Journal::Broken { old: old.clone(), new: new.clone() },
        (Ok(()), true) => Journal::Failed { old: old.clone(), new: new.clone() },
        (Ok(()), false) => Journal::Refused { old: old.clone(), new: new.clone(), why: why.clone() },
    };
    let _ = m.journal(&verdict);
    let start = if restored.is_ok() || m.install_complete() {
        m.start_service()
    } else {
        Err("the install folder holds no complete set of programs to start".to_string())
    };
    match (restored, start) {
        (Ok(()), Ok(())) if started => Outcome::RolledBack(why),
        (Ok(()), Ok(())) => Outcome::Refused(why),
        (Err(e), Ok(())) => {
            Outcome::Broken(format!("{why}; the restore failed too: {e}; the service runs over what is there"))
        }
        (Err(e), Err(s)) => Outcome::Broken(format!("{why}; the restore failed too: {e}; {s}")),
        (Ok(()), Err(s)) => Outcome::Broken(format!("{why}; the old service did not start: {s}")),
    }
}

/// What a starting service does with the journal (UPDATES.md 6.2 step 6, 6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtStart {
    Nothing,
    /// This is the new version, started by the applier that waits for it: write `committed`.
    Commit,
    /// An apply was cut short (no applier alive) and its files are staged: run the applier again from the start.
    Finish {
        old: Version,
        new: Version,
    },
    /// An apply was cut short and its staged files are gone: restore `old` from the rollback folder.
    RollBack {
        old: Version,
        new: Version,
    },
    /// The last apply ended: remove what it left (staging, the rollback copy, the applier). `rolled_back`: the version
    /// that did not start and is not offered again by itself.
    Clean {
        rolled_back: Option<Version>,
    },
    /// The restore failed: keep the rollback folder for `install.cmd` and say so.
    Broken {
        old: Version,
        new: Version,
    },
    /// The last apply stopped before the new version ran, and this is the old one: remove what it left and tell the
    /// reason; `new` stays on offer.
    Refused {
        new: Version,
        why: String,
    },
}

/// The decision of [`AtStart`]: `me` is the starting service's version, `applier_alive` whether an applier process
/// runs, `staged` whether every staged program is there. Whatever version starts after a power cut, a cut apply is
/// finished or rolled back by the applier, never committed by a service over a set it did not check.
pub fn at_start(last: Option<&Journal>, me: &Version, applier_alive: bool, staged: bool) -> AtStart {
    match last {
        None => AtStart::Nothing,
        Some(Journal::Applying { new, .. }) if applier_alive => {
            if new == me {
                AtStart::Commit
            } else {
                AtStart::Nothing
            }
        }
        Some(Journal::Applying { old, new }) if staged => AtStart::Finish { old: old.clone(), new: new.clone() },
        Some(Journal::Applying { old, new }) => AtStart::RollBack { old: old.clone(), new: new.clone() },
        Some(Journal::Committed { .. }) => AtStart::Clean { rolled_back: None },
        Some(Journal::Failed { old, new }) => AtStart::Clean { rolled_back: (old == me).then(|| new.clone()) },
        Some(Journal::Broken { old, new }) => AtStart::Broken { old: old.clone(), new: new.clone() },
        Some(Journal::Refused { old, new, why }) if old == me => {
            AtStart::Refused { new: new.clone(), why: why.clone() }
        }
        Some(Journal::Refused { .. }) => AtStart::Clean { rolled_back: None },
    }
}

#[cfg(test)]
mod tests;
