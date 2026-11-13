//! UPDATES.md 14 U5, the OS-free part: the applier's sequence and its rollback with a fake machine.

use super::*;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// A machine in memory: the installed version, what runs, the journal, and the step that fails.
#[derive(Default)]
struct Fake {
    installed: String,
    staged_version: String,
    backup: Option<String>,
    running: bool,
    journal: Vec<String>,
    /// The step that fails (its name), and whether the new version commits once started.
    fail_at: Option<&'static str>,
    new_commits: bool,
    steps: Vec<&'static str>,
    /// Whether the install folder holds a complete set whatever its versions (a failed restore starts it).
    complete: bool,
}

impl Fake {
    fn new(old: &str, new: &str) -> Self {
        Self {
            installed: old.into(),
            staged_version: new.into(),
            running: true,
            new_commits: true,
            complete: true,
            ..Self::default()
        }
    }

    fn step(&mut self, name: &'static str) -> Result<(), String> {
        self.steps.push(name);
        if self.fail_at == Some(name) { Err(format!("{name} failed")) } else { Ok(()) }
    }
}

impl Machine for Fake {
    fn journal(&mut self, entry: &Journal) -> Result<(), String> {
        match entry {
            Journal::Failed { .. } => self.steps.push("failed"),
            Journal::Refused { .. } => self.steps.push("refused"),
            Journal::Broken { .. } => self.steps.push("broken"),
            _ => {}
        }
        self.journal.push(entry.to_line());
        Ok(())
    }

    fn stop_service(&mut self) -> Result<(), String> {
        self.step("stop")?;
        self.running = false;
        Ok(())
    }

    fn sweep(&mut self) -> Result<(), String> {
        self.step("sweep")
    }

    fn claim_targets(&mut self) -> Result<(), String> {
        self.step("claim_targets")
    }

    fn install_complete(&mut self) -> bool {
        self.complete
    }

    fn back_up(&mut self, old: &Version) -> Result<(), String> {
        self.step("back_up")?;
        self.backup = Some(old.to_string());
        Ok(())
    }

    fn put_new(&mut self) -> Result<(), String> {
        self.step("put_new")?;
        self.installed = self.staged_version.clone();
        Ok(())
    }

    fn restore(&mut self, old: &Version) -> Result<(), String> {
        self.step("restore")?;
        assert_eq!(self.backup.as_deref(), Some(old.to_string().as_str()), "restores what it backed up");
        self.installed = old.to_string();
        Ok(())
    }

    fn start_service(&mut self) -> Result<(), String> {
        self.step("start")?;
        self.running = true;
        Ok(())
    }

    fn wait_commit(&mut self, new: &Version, _limit: Duration) -> bool {
        let ok = self.installed == new.to_string() && self.new_commits;
        if ok {
            self.journal.push(Journal::Committed { new: new.clone() }.to_line());
        }
        ok
    }

    fn staged(&mut self) -> bool {
        !self.staged_version.is_empty()
    }

    fn clean_staging(&mut self) {
        self.staged_version.clear();
    }
}

#[test]
fn apply_replaces_every_file_and_starts_the_service() {
    let mut m = Fake::new("1.1.0", "1.1.1");
    assert_eq!(apply(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Committed);
    assert_eq!((m.installed.as_str(), m.running), ("1.1.1", true));
    assert_eq!(m.steps, ["stop", "sweep", "claim_targets", "back_up", "put_new", "start"]);
    assert_eq!(m.journal, ["applying 1.1.0 1.1.1", "committed 1.1.1"]);
    assert!(m.staged_version.is_empty(), "the staging copy goes after the commit");
}

#[test]
fn a_service_that_does_not_commit_in_time_is_rolled_back() {
    let mut m = Fake::new("1.1.0", "1.1.1");
    m.new_commits = false;
    assert!(matches!(apply(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::RolledBack(_)));
    assert_eq!((m.installed.as_str(), m.running), ("1.1.0", true), "the old version runs again");
    assert_eq!(m.journal.last().map(String::as_str), Some("failed 1.1.0 1.1.1"));
    // `failed` is written before the old service starts, so that service does not resume the apply.
    assert!(m.steps.ends_with(&["restore", "failed", "start"]), "{:?}", m.steps);
    // A new service that cannot start is a bad version: the restore starts the old one, and a start that fails again
    // is broken.
    let mut m = Fake::new("1.1.0", "1.1.1");
    m.fail_at = Some("start");
    let out = apply(&mut m, &v("1.1.0"), &v("1.1.1"));
    assert!(matches!(out, Outcome::Broken(_)), "{out:?}");
}

/// A step that fails before the new service started leaves the old version in place and on offer: the journal says
/// `refused` (the next start does not record the version as rolled back), the old service runs, and a step that never
/// ran is not undone.
#[test]
fn a_failure_before_the_new_service_started_is_refused_and_the_old_service_runs() {
    for step in ["stop", "sweep", "claim_targets", "back_up", "put_new"] {
        let mut m = Fake::new("1.1.0", "1.1.1");
        m.fail_at = Some(step);
        let out = apply(&mut m, &v("1.1.0"), &v("1.1.1"));
        assert!(matches!(out, Outcome::Refused(ref why) if why.contains(step)), "{step}: {out:?}");
        assert_eq!((m.installed.as_str(), m.running), ("1.1.0", true), "{step}: the old service runs");
        let last = Journal::last_of(m.journal.last().unwrap()).unwrap();
        assert!(matches!(last, Journal::Refused { ref why, .. } if why.contains(step)), "{step}: {last:?}");
        assert!(!m.steps.contains(&"failed"), "{step}: never recorded as a version that did not start");
        let touched = ["put_new"].contains(&step);
        assert_eq!(m.steps.contains(&"restore"), touched, "{step}: files are put back only once they may have changed");
        assert!(m.steps.ends_with(&["start"]), "{step}: {:?}", m.steps);
        // The next start of the old service: the version is not rolled back, the reason is told.
        let at = at_start(Some(&last), &v("1.1.0"), false, false);
        assert!(matches!(at, AtStart::Refused { ref new, .. } if *new == v("1.1.1")), "{step}: {at:?}");
    }
    // Another version starting over a refused journal only cleans.
    let refused = Journal::Refused { old: v("1.1.0"), new: v("1.1.1"), why: "a held".into() };
    assert_eq!(at_start(Some(&refused), &v("1.1.1"), false, false), AtStart::Clean { rolled_back: None });
    // Round trip with a reason of several words, and a newline in it stays on one line.
    let j = Journal::Refused {
        old: v("1.1.0"),
        new: v("1.1.1"),
        why: "swapdesk.exe is held
by Foo (pid 4)"
            .into(),
    };
    assert_eq!(
        Journal::last_of(&j.to_line()),
        Some(Journal::Refused { old: v("1.1.0"), new: v("1.1.1"), why: "swapdesk.exe is held by Foo (pid 4)".into() })
    );
}

/// The incident of 13/11: a restore that fails (a file held) returned before the service was started, and the SCM does
/// not restart a clean stop. A complete set of programs in the install folder starts a service whatever happened.
#[test]
fn a_failed_restore_still_starts_the_service_over_a_complete_set() {
    let mut m = Fake::new("1.1.0", "1.1.1");
    m.new_commits = false;
    m.fail_at = Some("restore");
    let out = apply(&mut m, &v("1.1.0"), &v("1.1.1"));
    assert!(matches!(out, Outcome::Broken(_)), "{out:?}");
    assert!(m.running, "the service runs again");
    assert_eq!(m.journal.last().map(String::as_str), Some("broken 1.1.0 1.1.1"));
    assert!(m.steps.ends_with(&["restore", "broken", "start"]), "{:?}", m.steps);

    // An incomplete set cannot run: the service stays stopped and the way out is install.cmd.
    let mut m = Fake::new("1.1.0", "1.1.1");
    m.new_commits = false;
    m.fail_at = Some("restore");
    m.complete = false;
    assert!(matches!(apply(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Broken(_)));
    assert!(!m.running);
}

#[test]
fn an_interrupted_apply_is_finished_or_rolled_back_from_the_journal() {
    let (old, new) = (v("1.1.0"), v("1.1.1"));
    let applying = Journal::last_of("applying 1.1.0 1.1.1\n");
    let a = applying.as_ref();
    // The applier waits: the new service commits, the old one (restarted by the SCM) does nothing.
    assert_eq!(at_start(a, &new, true, true), AtStart::Commit);
    assert_eq!(at_start(a, &old, true, true), AtStart::Nothing);
    // No applier (a power cut): whichever version starts, a mixed set included, the applier finishes from staging...
    for me in [&old, &new] {
        assert_eq!(at_start(a, me, false, true), AtStart::Finish { old: old.clone(), new: new.clone() }, "{me}");
        // ... or restores the old version when staging is gone.
        assert_eq!(at_start(a, me, false, false), AtStart::RollBack { old: old.clone(), new: new.clone() }, "{me}");
    }
    let committed = Journal::last_of("applying 1.1.0 1.1.1\ncommitted 1.1.1\n");
    assert_eq!(at_start(committed.as_ref(), &new, false, false), AtStart::Clean { rolled_back: None });
    let failed = Journal::last_of("applying 1.1.0 1.1.1\nfailed 1.1.0 1.1.1\n");
    assert_eq!(at_start(failed.as_ref(), &old, false, false), AtStart::Clean { rolled_back: Some(new.clone()) });
    let broken = Journal::last_of("applying 1.1.0 1.1.1\nfailed 1.1.0 1.1.1\nbroken 1.1.0 1.1.1\n");
    assert_eq!(
        at_start(broken.as_ref(), &new, false, false),
        AtStart::Broken { old: old.clone(), new: new.clone() },
        "a broken restore keeps its rollback folder"
    );
    assert_eq!(Journal::last_of("garbage\n"), None);
    assert_eq!(at_start(None, &old, false, true), AtStart::Nothing);
    // The lines round-trip.
    for j in [
        Journal::Applying { old: old.clone(), new: new.clone() },
        Journal::Committed { new: new.clone() },
        Journal::Failed { old: old.clone(), new: new.clone() },
        Journal::Broken { old: old.clone(), new: new.clone() },
    ] {
        assert_eq!(Journal::last_of(&j.to_line()), Some(j));
    }
}

#[test]
fn a_failed_restore_journals_broken_and_a_cut_apply_can_be_rolled_back_alone() {
    let mut m = Fake::new("1.1.0", "1.1.1");
    m.new_commits = false;
    m.fail_at = Some("restore");
    assert!(matches!(apply(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Broken(_)));
    assert_eq!(m.journal.last().map(String::as_str), Some("broken 1.1.0 1.1.1"));

    let mut m = Fake::new("1.1.0", "1.1.1");
    m.backup = Some("1.1.0".into());
    m.installed = "1.1.1".into();
    assert!(matches!(roll_back_only(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::RolledBack(_)));
    assert_eq!((m.installed.as_str(), m.running), ("1.1.0", true));
    assert_eq!(m.journal.last().map(String::as_str), Some("failed 1.1.0 1.1.1"));
}
