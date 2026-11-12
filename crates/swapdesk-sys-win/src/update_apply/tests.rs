//! UPDATES.md 14 U5, the Windows part: real files in temporary folders, a fake SCM.

use std::fs;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use swapdesk_update::apply::{Outcome, apply};

use super::*;
use crate::check_no_other_writers;
use crate::tests::{Temp, create_dir_with_sddl, me};

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn trusted() -> Vec<String> {
    vec![me(), "S-1-5-18".into(), "S-1-5-32-544".into()]
}

/// A strict install folder (the user, SYSTEM: full; Users: read) and a loose staging folder whose files inherit
/// Everyone: modify, as a file made elsewhere might carry.
fn folders(temp: &Temp) -> Layout {
    let me = me();
    let install = temp.0.join("install");
    create_dir_with_sddl(&install, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)"));
    let update = temp.0.join("update");
    fs::create_dir_all(&update).unwrap();
    create_dir_with_sddl(&update.join(STAGING), &format!("D:P(A;OICI;FA;;;{me})(A;OICI;0x1301bf;;;WD)"));
    Layout { install, update }
}

fn fill(dir: &Path, version: &str) {
    for name in INSTALLED {
        fs::write(dir.join(name), format!("{name} {version}")).unwrap();
    }
}

fn read(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).unwrap()
}

/// The SCM of a test: "starting" the service writes `committed <new>` when the installed service is the new one and
/// the test lets it commit.
struct FakeScm {
    journal: PathBuf,
    install: PathBuf,
    commits: Arc<Mutex<bool>>,
}

impl ServiceControl for FakeScm {
    fn stop(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn start(&mut self) -> Result<(), String> {
        let text = read(&self.install, "swapdesk-service.exe");
        let version = text.rsplit(' ').next().unwrap().to_string();
        if *self.commits.lock().unwrap() && version == "1.1.1" {
            append_journal(&self.journal, &Journal::Committed { new: v(&version) }).unwrap();
        }
        Ok(())
    }

    fn running(&mut self) -> bool {
        true
    }
}

fn machine(layout: &Layout, root: &Path, commits: bool) -> WinMachine {
    WinMachine {
        control: Box::new(FakeScm {
            journal: layout.journal(),
            install: layout.install.clone(),
            commits: Arc::new(Mutex::new(commits)),
        }),
        layout: layout.clone(),
        policy: InstallPolicy { root: root.to_path_buf(), trusted_sids: trusted() },
        grace: Duration::ZERO,
        poll: Duration::from_millis(20),
    }
}

#[test]
fn moved_files_inherit_the_folder_acl_and_pass_the_install_policy() {
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    // Before the move, a staged program grants Everyone write access: the policy would refuse it.
    let staged = layout.staging().join("swapdesk-helper.exe");
    assert!(check_no_other_writers(&staged, &trusted()).is_err(), "the staged file is loose");

    let mut m = machine(&layout, &temp.0, true);
    m.put_new().unwrap();
    for name in INSTALLED {
        assert_eq!(read(&layout.install, name), format!("{name} 1.1.1"));
        check_no_other_writers(&layout.install.join(name), &trusted())
            .unwrap_or_else(|e| panic!("{name} keeps an entry of its own: {e}"));
        assert!(layout.staging().join(name).exists(), "{name} stays staged for a second try");
    }

    // A program that cannot pass the policy (outside its root) is refused.
    fill(&layout.staging(), "1.1.2");
    let mut strict = machine(&layout, &temp.0.join("elsewhere"), true);
    fs::create_dir_all(temp.0.join("elsewhere")).unwrap();
    assert!(strict.put_new().is_err());
}

#[test]
fn apply_in_temporary_folders_replaces_the_files_or_restores_the_old_ones() {
    // Commits: every file is the new one, staging is gone, the journal ends in committed.
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    let mut m = machine(&layout, &temp.0, true);
    assert!(m.staged());
    assert_eq!(apply(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Committed);
    assert_eq!(read(&layout.install, "Swapdesk.exe"), "Swapdesk.exe 1.1.1");
    assert!(!layout.staging().exists());
    assert_eq!(read_journal(&layout.journal()), Some(Journal::Committed { new: v("1.1.1") }));
    assert_eq!(read(&layout.rollback(&v("1.1.0")), "swapdesk-service.exe"), "swapdesk-service.exe 1.1.0");

    // Does not commit: the old files are back and the journal says failed.
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    let mut m = machine(&layout, &temp.0, false);
    assert!(!m.wait_commit(&v("1.1.1"), Duration::from_millis(200)), "nothing committed");
    let out = apply_with_wait(&mut m);
    assert!(matches!(out, Outcome::RolledBack(_)), "{out:?}");
    for name in INSTALLED {
        assert_eq!(read(&layout.install, name), format!("{name} 1.1.0"), "{name} restored");
    }
    assert_eq!(read_journal(&layout.journal()), Some(Journal::Failed { old: v("1.1.0"), new: v("1.1.1") }));
}

/// `apply` with the real `COMMIT_WAIT` would take a minute: the fake machine's `running` turns false after the start,
/// which ends the wait early (UPDATES.md 6.4: a service that stops again will not commit).
fn apply_with_wait(m: &mut WinMachine) -> Outcome {
    struct Stops(Box<dyn ServiceControl>, bool);
    impl ServiceControl for Stops {
        fn stop(&mut self) -> Result<(), String> {
            self.0.stop()
        }
        fn start(&mut self) -> Result<(), String> {
            self.1 = true;
            self.0.start()
        }
        fn running(&mut self) -> bool {
            !self.1
        }
    }
    let inner = std::mem::replace(&mut m.control, Box::new(Stops(Box::new(NoScm), false)));
    m.control = Box::new(Stops(inner, false));
    apply(m, &v("1.1.0"), &v("1.1.1"))
}

struct NoScm;

impl ServiceControl for NoScm {
    fn stop(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn start(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn running(&mut self) -> bool {
        false
    }
}

#[test]
fn an_apply_cut_short_after_one_file_runs_again_and_keeps_the_first_backup() {
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    // The first try: the backup is made, one program is replaced, then the power goes.
    let mut m = machine(&layout, &temp.0, true);
    m.back_up(&v("1.1.0")).unwrap();
    fs::copy(layout.staging().join("Swapdesk.exe"), layout.install.join("Swapdesk.exe")).unwrap();
    append_journal(&layout.journal(), &Journal::Applying { old: v("1.1.0"), new: v("1.1.1") }).unwrap();
    assert!(m.staged(), "the staged files are whole after the first try");

    // A second try that does not commit restores the files of before the first try, not the mixed set.
    let mut m = machine(&layout, &temp.0, false);
    assert!(matches!(apply_with_wait(&mut m), Outcome::RolledBack(_)));
    for name in INSTALLED {
        assert_eq!(read(&layout.install, name), format!("{name} 1.1.0"), "{name}");
    }
    // And one that commits finishes the set.
    let mut m = machine(&layout, &temp.0, true);
    assert_eq!(apply(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Committed);
    for name in INSTALLED {
        assert_eq!(read(&layout.install, name), format!("{name} 1.1.1"), "{name}");
    }
}

#[test]
fn a_package_with_a_name_an_install_never_holds_is_not_staged() {
    use swapdesk_update::package::Entry;
    let temp = Temp::new();
    let staging = temp.0.join(STAGING);
    let entry = |path: &str| Entry { path: path.into(), mode: 0o755, bytes: path.as_bytes().to_vec() };
    let mut pkg = Package { entries: PROGRAMS.iter().map(|p| entry(p)).collect() };
    stage(&pkg, &staging).unwrap();
    assert_eq!(fs::read(staging.join("Swapdesk.exe")).unwrap(), b"Swapdesk.exe");
    pkg.entries.push(entry("evil.dll"));
    assert!(stage(&pkg, &staging).is_err());
    assert!(!staging.exists(), "nothing is left staged");
    pkg.entries.retain(|e| e.path != "evil.dll" && e.path != "swapdesk-helper.exe");
    assert!(stage(&pkg, &staging).is_err(), "a program is missing");
}

/// The helper process of the sweep test: sleeps until it is ended. Not a test of its own.
#[test]
#[ignore = "helper process of the_sweep_ends_only_processes_inside_the_install_folder"]
fn sweep_target_sleeps() {
    std::thread::sleep(Duration::from_secs(60));
}

/// Starts a copy of this test binary at `exe`, running the sleeping helper.
fn sleeper(exe: &Path) -> Child {
    fs::copy(std::env::current_exe().unwrap(), exe).unwrap();
    Command::new(exe)
        .args(["--exact", "update_apply::tests::sweep_target_sleeps", "--ignored", "--nocapture"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn the_sweep_ends_only_processes_inside_the_install_folder() {
    let temp = Temp::new();
    let install = temp.0.join("install");
    // A sibling whose name starts with the install folder's: not inside it.
    let sibling = temp.0.join("install2");
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    let mut inside = sleeper(&install.join("swapdesk-helper.exe"));
    let mut other_name = sleeper(&install.join("not-swapdesk.exe"));
    let mut outside = sleeper(&sibling.join("swapdesk-helper.exe"));
    // Wait until the three run (their images are read through their PIDs).
    let deadline = Instant::now() + Duration::from_secs(10);
    while programs_inside(&install) != vec![inside.id()] {
        assert!(Instant::now() < deadline, "the helper inside never showed: {:?}", programs_inside(&install));
        std::thread::sleep(Duration::from_millis(50));
    }

    sweep(&install, Duration::ZERO).unwrap();
    let ended = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        inside.try_wait().unwrap().is_some()
    });
    assert!(ended, "the program inside the install folder was ended");
    assert!(other_name.try_wait().unwrap().is_none(), "another name in the folder runs on");
    assert!(outside.try_wait().unwrap().is_none(), "the same name in another folder runs on");
    assert!(is_program_inside(r"c:\x\install\Swapdesk.exe", r"c:\x\install"));
    assert!(!is_program_inside(r"c:\x\install2\swapdesk.exe", r"c:\x\install"));
    assert!(!is_program_inside(r"c:\x\install\sub\swapdesk.exe", r"c:\x\install"));
    for mut c in [other_name, outside] {
        let _ = c.kill();
        let _ = c.wait();
    }
}
