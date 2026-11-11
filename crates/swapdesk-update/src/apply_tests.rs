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
}

impl Fake {
    fn new(old: &str, new: &str) -> Self {
        Self { installed: old.into(), staged_version: new.into(), running: true, new_commits: true, ..Self::default() }
    }

    fn step(&mut self, name: &'static str) -> Result<(), String> {
        self.steps.push(name);
        if self.fail_at == Some(name) { Err(format!("{name} failed")) } else { Ok(()) }
    }
}

impl Machine for Fake {
    fn journal(&mut self, entry: &Journal) -> Result<(), String> {
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
    assert_eq!(m.steps, ["stop", "sweep", "back_up", "put_new", "start"]);
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
    // A step that fails after the stop rolls back the same way.
    for step in ["sweep", "put_new", "start"] {
        let mut m = Fake::new("1.1.0", "1.1.1");
        m.fail_at = Some(step);
        let out = apply(&mut m, &v("1.1.0"), &v("1.1.1"));
        if step == "start" {
            // The restore starts the service again, and that fails too.
            assert!(matches!(out, Outcome::Broken(_)), "{step}: {out:?}");
        } else {
            assert!(matches!(out, Outcome::RolledBack(_)), "{step}: {out:?}");
            assert_eq!(m.installed, "1.1.0", "{step}");
        }
    }
}

#[test]
fn an_interrupted_apply_is_finished_or_rolled_back_from_the_journal() {
    let applying = Journal::last_of("applying 1.1.0 1.1.1\n");
    assert_eq!(resume_from(applying.as_ref(), true), Some(true), "staging is there: finish");
    assert_eq!(resume_from(applying.as_ref(), false), Some(false), "staging gone: roll back");
    let committed = Journal::last_of("applying 1.1.0 1.1.1\ncommitted 1.1.1\n");
    assert_eq!(resume_from(committed.as_ref(), true), None, "committed: nothing to do");
    assert_eq!(Journal::last_of("garbage\n"), None);
    assert_eq!(resume_from(None, true), None);
    // The lines round-trip.
    for j in [
        Journal::Applying { old: v("1.1.0"), new: v("1.1.1") },
        Journal::Committed { new: v("1.1.1") },
        Journal::Failed { old: v("1.1.0"), new: v("1.1.1") },
    ] {
        assert_eq!(Journal::last_of(&j.to_line()), Some(j));
    }
}
