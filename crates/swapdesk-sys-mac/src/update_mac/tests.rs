//! UPDATES.md 14 U6, the Mac part: real signed code, real folders.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU32, Ordering};

use swapdesk_update::package::Entry;

use super::*;

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "swapdesk-update-mac-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_bundle_signed_by_another_identity_is_refused() {
    // Apple's own tools: each has its own identifier, so one does not meet the other's designated requirement.
    let (ls, cat) = (Path::new("/bin/ls"), Path::new("/bin/cat"));
    same_signer(ls, ls).expect("the same code meets its own requirement");
    assert_eq!(same_signer(cat, ls), Err(SignError::OtherIdentity));

    // A copy is still the same signed code; an altered copy is refused.
    let temp = Temp::new();
    let copy = temp.0.join("ls");
    fs::copy(ls, &copy).unwrap();
    same_signer(&copy, ls).expect("an unaltered copy passes");
    let mut bytes = fs::read(&copy).unwrap();
    let mid = bytes.len() / 3;
    bytes[mid] ^= 0xff;
    fs::write(&copy, &bytes).unwrap();
    match same_signer(&copy, ls) {
        Err(SignError::Invalid(_) | SignError::Unreadable(_) | SignError::OtherIdentity) => {}
        other => panic!("an altered copy passed: {other:?}"),
    }
}

#[test]
fn a_translocated_or_unwritable_location_is_refused_with_its_reason() {
    let translocated = Path::new("/private/var/folders/ab/T/AppTranslocation/0A1B/d/Swapdesk.app");
    assert_eq!(check_location(translocated), Err(LocationRefusal::Translocated));

    let temp = Temp::new();
    let app = temp.0.join("Swapdesk.app");
    fs::create_dir_all(&app).unwrap();
    assert_eq!(check_location(&app), Ok(()), "a folder this user writes");
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o555)).unwrap();
    assert_eq!(check_location(&app), Err(LocationRefusal::NotWritable));
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(bundle_of(Path::new("/Applications/Swapdesk.app/Contents/MacOS/Swapdesk")), Some(app_path()));
    assert_eq!(bundle_of(Path::new("/usr/local/bin/swapdesk")), None);
    assert_eq!(
        hidden_path(&app_path(), &Version::parse("1.1.1").unwrap()),
        PathBuf::from("/Applications/.Swapdesk-1.1.1.app")
    );
}

fn app_path() -> PathBuf {
    PathBuf::from("/Applications/Swapdesk.app")
}

#[test]
fn swap_and_swap_back_leave_the_original_bundle() {
    let temp = Temp::new();
    let (running, hidden) = (temp.0.join("Swapdesk.app"), temp.0.join(".Swapdesk-1.1.1.app"));
    for (dir, v) in [(&running, "old"), (&hidden, "new")] {
        fs::create_dir_all(dir.join("Contents")).unwrap();
        fs::write(dir.join("Contents").join("version"), v).unwrap();
    }
    let read = |dir: &Path| fs::read_to_string(dir.join("Contents").join("version")).unwrap();
    swap(&hidden, &running).unwrap();
    assert_eq!((read(&running).as_str(), read(&hidden).as_str()), ("new", "old"));
    swap(&hidden, &running).unwrap();
    assert_eq!((read(&running).as_str(), read(&hidden).as_str()), ("old", "new"));
    assert!(swap(&temp.0.join("missing"), &running).is_err());
}

#[test]
fn a_staged_bundle_keeps_its_modes_and_nothing_outside_the_app_is_staged() {
    let temp = Temp::new();
    let dest = temp.0.join(".Swapdesk-1.1.1.app");
    let entry = |path: &str, mode| Entry { path: path.into(), mode, bytes: path.as_bytes().to_vec() };
    let mut pkg = Package {
        entries: vec![
            entry("Swapdesk.app/Contents/MacOS/Swapdesk", 0o755),
            entry("Swapdesk.app/Contents/Info.plist", 0o644),
        ],
    };
    stage_bundle(&pkg, &dest).unwrap();
    let mode = |p: &str| fs::metadata(dest.join(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode("Contents/MacOS/Swapdesk"), mode("Contents/Info.plist")), (0o755, 0o644));
    assert_eq!(executable_of(&dest), Some(dest.join("Contents/MacOS/Swapdesk")));
    pkg.entries.push(entry("Other.app/Contents/MacOS/x", 0o755));
    assert!(stage_bundle(&pkg, &dest).is_err());
    assert!(!dest.exists(), "nothing is left staged");
}

#[test]
fn the_journal_round_trips_through_its_file() {
    let temp = Temp::new();
    let path = temp.0.join("journal");
    assert_eq!(read_journal(&path), None);
    let (old, new) = (Version::parse("1.1.0").unwrap(), Version::parse("1.1.1").unwrap());
    append_journal(&path, &Journal::Applying { old, new: new.clone() }).unwrap();
    append_journal(&path, &Journal::Committed { new: new.clone() }).unwrap();
    assert_eq!(read_journal(&path), Some(Journal::Committed { new }));
    // This process is found by its own executable.
    let me = std::env::current_exe().unwrap().canonicalize().unwrap();
    assert!(pids_of(&me).contains(&(std::process::id() as i32)));
}
