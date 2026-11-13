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
    /// What the service control was asked, in order (`start 1.1.0` carries the installed service's version).
    log: Log,
    /// Runs inside every start, as a local program would at that moment.
    on_start: Option<OnStart>,
}

type Log = Arc<Mutex<Vec<String>>>;

/// What a local program does at the moment a service starts (given the installed service's version).
type OnStart = Box<dyn FnMut(&str) + Send>;

impl ServiceControl for FakeScm {
    fn stop(&mut self) -> Result<(), String> {
        self.log.lock().unwrap().push("stop".into());
        Ok(())
    }

    fn start(&mut self) -> Result<(), String> {
        let text = read(&self.install, "swapdesk-service.exe");
        let version = text.rsplit(' ').next().unwrap().to_string();
        self.log.lock().unwrap().push(format!("start {version}"));
        if let Some(on_start) = &mut self.on_start {
            on_start(&version);
        }
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
    machine_with(layout, root, commits, None).0
}

fn machine_with(layout: &Layout, root: &Path, commits: bool, on_start: Option<OnStart>) -> (WinMachine, Log) {
    let log = Log::default();
    let m = WinMachine {
        control: Box::new(FakeScm {
            journal: layout.journal(),
            install: layout.install.clone(),
            commits: Arc::new(Mutex::new(commits)),
            log: Arc::clone(&log),
            on_start,
        }),
        layout: layout.clone(),
        policy: InstallPolicy { root: root.to_path_buf(), trusted_sids: trusted() },
        grace: Duration::ZERO,
        poll: Duration::from_millis(20),
        retry: Duration::from_millis(300),
        held: None,
    };
    (m, log)
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

/// A program started through the 8.3 short name of its folder (the GitHub runner's temp folder is
/// `C:\Users\RUNNER~1\...`) has its image written with that short name: it is still found inside the install folder.
#[test]
fn a_program_started_through_a_short_folder_name_is_found_inside_the_install_folder() {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let temp = Temp::new();
    let install = temp.0.join("an install folder with a long name");
    fs::create_dir_all(&install).unwrap();
    let wide: Vec<u16> = install.as_os_str().encode_wide().chain([0]).collect();
    let mut buf = [0u16; 1024];
    // SAFETY: `wide` ends in a NUL; `buf` holds `buf.len()` units.
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    assert!(len > 0 && len < buf.len(), "the short name of the folder");
    let short = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..len]));
    if short == install {
        // This volume makes no 8.3 names: the case cannot be built here.
        return;
    }
    let mut child = sleeper(&short.join("swapdesk-helper.exe"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while programs_inside(&install) != vec![child.id()] {
        assert!(Instant::now() < deadline, "the helper started by its short name never showed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Opens `path` as a hostile local program would: for read, sharing only read.
fn hold_for_read(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new().read(true).share_mode(1).open(path)
}

fn journal_of(layout: &Layout) -> Option<Journal> {
    read_journal(&layout.journal())
}

/// The review's defect: one read handle (share READ) on an installed program broke every update. Now the apply stops
/// before it changes anything, names the holder, starts the service again and leaves the version on offer; once the
/// holder lets go the same staged files apply.
#[test]
fn a_target_held_open_with_share_read_fails_the_apply_before_any_file_changes() {
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    let hold = hold_for_read(&layout.install.join("Swapdesk.exe")).unwrap();

    let (mut m, log) = machine_with(&layout, &temp.0, true, None);
    let out = apply(&mut m, &v("1.1.0"), &v("1.1.1"));
    let Outcome::Refused(why) = &out else { panic!("{out:?}") };
    assert!(why.contains("Swapdesk.exe") && why.contains("nothing was changed"), "{why}");
    for name in INSTALLED {
        assert_eq!(read(&layout.install, name), format!("{name} 1.1.0"), "{name} untouched");
    }
    assert!(!layout.rollback(&v("1.1.0")).exists(), "not even a backup was made");
    assert!(layout.staging().join("Swapdesk.exe").exists(), "the staged files wait for a second try");
    // The service was stopped for the swap and is started again: a clean stop is not restarted by the SCM.
    assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("start 1.1.0"), "{:?}", log.lock().unwrap());
    // Not recorded as a version that did not start: the next start of the old service finds `refused`.
    let last = journal_of(&layout).unwrap();
    assert!(matches!(last, Journal::Refused { .. }), "{last:?}");
    let at = swapdesk_update::apply::at_start(Some(&last), &v("1.1.0"), false, true);
    assert!(matches!(at, swapdesk_update::apply::AtStart::Refused { .. }), "{at:?}");

    // The holder lets go: the same staged files apply.
    drop(hold);
    let (mut m, _) = machine_with(&layout, &temp.0, true, None);
    assert_eq!(apply(&mut m, &v("1.1.0"), &v("1.1.1")), Outcome::Committed);
    assert_eq!(read(&layout.install, "Swapdesk.exe"), "Swapdesk.exe 1.1.1");
}

/// The other half of the incident: the restore fails (here: its rollback folder lost a file), and the service was left
/// stopped. The install folder holds a complete set, so the service is started anyway and the journal says `broken`.
#[test]
fn a_failed_restore_still_starts_the_service_when_the_set_is_complete() {
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    let rollback = layout.rollback(&v("1.1.0"));
    let on_start: OnStart = Box::new(move |version| {
        if version == "1.1.1" {
            // The new service starts and the old files' copy is damaged before anyone can restore them.
            let _ = fs::remove_file(rollback.join("swapdesk-helper.exe"));
        }
    });
    let (mut m, log) = machine_with(&layout, &temp.0, false, Some(on_start));
    let out = apply_with_wait(&mut m);
    assert!(matches!(out, Outcome::Broken(_)), "{out:?}");
    assert_eq!(read_journal(&layout.journal()), Some(Journal::Broken { old: v("1.1.0"), new: v("1.1.1") }));
    assert!(complete(&layout.install));
    let starts: Vec<String> = log.lock().unwrap().iter().filter(|l| l.starts_with("start")).cloned().collect();
    assert_eq!(starts.len(), 2, "the service is started again after the failed restore: {starts:?}");
}

/// The race: between the check and the move nothing can grab a target. Once the files are claimed a program that
/// opens one for read sharing only read is refused by the system, through the move, and until the hold is let go.
#[test]
fn no_program_can_grab_a_target_between_the_claim_and_the_move() {
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    let (mut m, _) = machine_with(&layout, &temp.0, true, None);
    let target = layout.install.join("swapdesk-service.exe");
    hold_for_read(&target).expect("before the claim a reader gets in");

    m.claim_targets().unwrap();
    let refused = hold_for_read(&target).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(32), "after the claim the open is refused: {refused}");
    m.back_up(&v("1.1.0")).unwrap();
    m.put_new().unwrap();
    for name in INSTALLED {
        assert_eq!(read(&layout.install, name), format!("{name} 1.1.1"));
    }
    // The new file is held through the rename: there was no moment in which it could be grabbed.
    let refused = hold_for_read(&target).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(32), "the new file is held too: {refused}");
    m.clean_staging();
    hold_for_read(&target).expect("after the commit the hold is let go");
}

/// A held file the claim cannot take is named by the Restart Manager, and the claim waits `retry` for a scanner.
#[test]
fn a_claim_that_fails_names_the_holder_and_waits_only_the_retry() {
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    let hold = hold_for_read(&layout.install.join("swapdesk-helper.exe")).unwrap();
    let started = Instant::now();
    let Err(why) = lock::Held::claim(&layout.install, &layout.staging(), Duration::from_millis(300)) else {
        panic!("the claim must fail")
    };
    assert!(started.elapsed() >= Duration::from_millis(300) && started.elapsed() < Duration::from_secs(5));
    assert!(why.contains("swapdesk-helper.exe"), "{why}");
    // This very process holds it: the Restart Manager names it.
    assert!(why.contains(&format!("pid {}", std::process::id())), "{why}");
    drop(hold);
    lock::Held::claim(&layout.install, &layout.staging(), Duration::ZERO).expect("claims once the holder is gone");
}

/// The hold must not stop the service from starting from the files it holds: a program is started from a claimed file.
#[test]
fn a_program_starts_from_a_claimed_file() {
    let temp = Temp::new();
    let layout = folders(&temp);
    fill(&layout.install, "1.1.0");
    fill(&layout.staging(), "1.1.1");
    let exe = layout.install.join("swapdesk-helper.exe");
    let mut m = machine(&layout, &temp.0, true);
    fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    fs::copy(&exe, layout.staging().join("swapdesk-helper.exe")).unwrap();
    m.claim_targets().unwrap();
    let mut child = Command::new(&exe)
        .args(["--exact", "update_apply::tests::sweep_target_sleeps", "--ignored", "--nocapture"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("a program starts from a file whose hold is ours");
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.try_wait().unwrap().is_none(), "it runs");
    let _ = child.kill();
    let _ = child.wait();
}
