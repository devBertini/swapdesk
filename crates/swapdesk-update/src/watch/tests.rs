//! UPDATES.md 14 U6, the OS-free part: the watcher's sequence with a fake Mac.

use super::*;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// A Mac in memory: what the running path and the hidden path hold, what runs, the journal.
struct Fake {
    running_path: &'static str,
    hidden: Option<&'static str>,
    runs: Option<&'static str>,
    quits: bool,
    commits: bool,
    swap_fails: bool,
    journal: Vec<String>,
}

impl Fake {
    /// Right after the app swapped: the running path holds the new bundle, the old one is hidden and still runs.
    fn swapped() -> Self {
        Self {
            running_path: "1.1.1",
            hidden: Some("1.1.0"),
            runs: Some("1.1.0"),
            quits: true,
            commits: true,
            swap_fails: false,
            journal: Vec::new(),
        }
    }
}

impl Watcher for Fake {
    fn wait_quit(&mut self, _limit: Duration) -> bool {
        if self.quits {
            self.runs = None;
        }
        self.quits
    }

    fn open(&mut self) -> Result<(), String> {
        self.runs = Some(self.running_path);
        Ok(())
    }

    fn wait_commit(&mut self, new: &Version, _limit: Duration) -> bool {
        self.commits && self.runs == Some(self.running_path) && self.running_path == new.to_string()
    }

    fn end_running(&mut self) {
        self.runs = None;
    }

    fn swap_back(&mut self) -> Result<(), String> {
        if self.swap_fails {
            return Err("refused".into());
        }
        let hidden = self.hidden.take().ok_or("nothing hidden")?;
        self.hidden = Some(self.running_path);
        self.running_path = hidden;
        Ok(())
    }

    fn remove_hidden(&mut self) {
        self.hidden = None;
    }

    fn journal(&mut self, entry: &Journal) -> Result<(), String> {
        self.journal.push(entry.to_line());
        Ok(())
    }
}

#[test]
fn the_watcher_opens_the_new_bundle_and_removes_the_old_one_after_the_commit() {
    let mut m = Fake::swapped();
    assert_eq!(watch(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Committed);
    assert_eq!((m.running_path, m.runs, m.hidden), ("1.1.1", Some("1.1.1"), None));
}

#[test]
fn the_watcher_reopens_the_old_bundle_when_the_new_one_does_not_commit() {
    let mut m = Fake::swapped();
    m.commits = false;
    assert!(matches!(watch(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::RolledBack(_)));
    assert_eq!((m.running_path, m.runs, m.hidden), ("1.1.0", Some("1.1.0"), Some("1.1.1")), "the old app runs again");
    assert_eq!(m.journal, ["failed 1.1.0 1.1.1"]);

    // An app that never quits: swapped back, so its next start is the old version again.
    let mut m = Fake::swapped();
    m.quits = false;
    assert!(matches!(watch(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::RolledBack(_)));
    assert_eq!(m.running_path, "1.1.0");

    // A swap back that fails is broken, and says so in the journal.
    let mut m = Fake::swapped();
    m.commits = false;
    m.swap_fails = true;
    assert!(matches!(watch(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Broken(_)));
    assert_eq!(m.journal, ["broken 1.1.0 1.1.1"]);
    assert_eq!(m.hidden, Some("1.1.0"), "the old bundle waits at its hidden path");
}
