//! Tests run as the current user, so the folder owner is the current user (only SYSTEM can make SYSTEM the owner);
//! the production spec is checked as the descriptor text it would apply.

use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use swapdesk_platform::{SecretStore, TraceSink};

use super::*;

const REPARSE: u32 = 0x400;

/// A fresh temp folder, removed at the end (junctions inside are unlinked first, never followed).
struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "swapdesk-secret-win-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        fn clean(dir: &Path) {
            let Ok(entries) = fs::read_dir(dir) else { return };
            for e in entries.flatten() {
                let p = e.path();
                let Ok(md) = fs::symlink_metadata(&p) else { continue };
                if md.file_attributes() & REPARSE != 0 {
                    let _ = fs::remove_dir(&p).or_else(|_| fs::remove_file(&p));
                } else if md.is_dir() {
                    clean(&p);
                }
            }
        }
        clean(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn test_spec() -> FolderSpec {
    let me = current_user_sid().unwrap();
    FolderSpec { owner: me.clone(), full_control: vec![me.clone(), "SY".into(), "BA".into()], trusted_owners: vec![me] }
}

fn is_reparse(p: &Path) -> bool {
    fs::symlink_metadata(p).unwrap().file_attributes() & REPARSE != 0
}

fn junction(link: &Path, target: &Path) {
    let out = Command::new("cmd").arg("/c").arg("mklink").arg("/J").arg(link).arg(target).output().unwrap();
    assert!(out.status.success(), "mklink /J failed: {}", String::from_utf8_lossy(&out.stdout));
    assert!(is_reparse(link));
}

#[derive(Default)]
struct Lines(std::sync::Mutex<Vec<String>>);
impl TraceSink for Lines {
    fn log(&self, _: swapdesk_platform::Level, _: &'static str, what: core::fmt::Arguments<'_>) {
        self.0.lock().unwrap().push(what.to_string());
    }
}

// ---------- DPAPI ----------

#[test]
fn dpapi_round_trips_at_machine_scope_and_binds_the_entropy() {
    let secret = b"a private key that must not sit on disk in the clear".to_vec();
    let blob = protect(&secret, b"swapdesk identity v1").unwrap();
    assert_ne!(blob, secret);
    assert!(!blob.windows(secret.len()).any(|w| w == secret.as_slice()));
    assert_eq!(unprotect(&blob, b"swapdesk identity v1").unwrap(), secret);
    assert!(unprotect(&blob, b"swapdesk trust v1").is_err(), "another entropy must not open the blob");
    let mut altered = blob.clone();
    let mid = altered.len() / 2;
    altered[mid] ^= 0x55;
    assert!(unprotect(&altered, b"swapdesk identity v1").is_err(), "an altered blob must not open");
    assert_eq!(unprotect(&protect(b"", b"e").unwrap(), b"e").unwrap(), b"");
}

// ---------- the folder ----------

#[test]
fn the_production_spec_is_system_owner_with_a_protected_system_and_administrators_dacl() {
    let p = FolderSpec::production();
    assert_eq!(p.sddl(), "O:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    assert_eq!(p.expected_aces().unwrap().concat(), "(A;OICI;FA;;;S-1-5-18)(A;OICI;FA;;;S-1-5-32-544)");
    assert_eq!(normalize_sid("SY").unwrap(), "S-1-5-18");
    assert_eq!(normalize_sid("BA").unwrap(), "S-1-5-32-544");
}

#[test]
fn a_new_folder_has_the_owner_and_the_protected_dacl_of_the_spec() {
    let t = Temp::new();
    let spec = test_spec();
    let rep = ensure_folder(&spec, &t.0, FOLDER_NAME, None).unwrap();
    assert!(rep.created && rep.moved_aside.is_none());
    let read = read_security(&rep.path).unwrap();
    assert_eq!(read.owner, normalize_sid(&spec.owner).unwrap());
    assert!(dacl_matches(&spec, &read).unwrap(), "DACL read back: {}", read.dacl_sddl);
    assert!(read.dacl_sddl.starts_with("D:P"), "inheritance from the parent must be off: {}", read.dacl_sddl);
    // Nobody else: the read-back lists exactly the three principals and no inherited entry.
    assert_eq!(read.dacl_sddl.matches("(A;").count(), 3);
    assert!(!read.dacl_sddl.contains("(D;") && !read.dacl_sddl.contains("WD") && !read.dacl_sddl.contains("BU"));
}

#[test]
fn a_trusted_existing_folder_with_the_right_dacl_is_kept_with_its_contents() {
    let t = Temp::new();
    let spec = test_spec();
    let first = ensure_folder(&spec, &t.0, FOLDER_NAME, None).unwrap();
    fs::write(first.path.join("marker"), b"x").unwrap();
    let again = ensure_folder(&spec, &t.0, FOLDER_NAME, None).unwrap();
    assert!(!again.created && again.moved_aside.is_none());
    assert!(again.path.join("marker").exists(), "a trusted folder must be kept with its contents");
    assert!(dacl_matches(&spec, &read_security(&again.path).unwrap()).unwrap());
}

#[test]
fn a_folder_with_an_untrusted_owner_is_moved_aside_and_replaced() {
    let t = Temp::new();
    let spec = test_spec();
    // Planted by "someone else": made with plain std, then judged by a spec that does not trust its owner.
    let planted = t.0.join(FOLDER_NAME);
    fs::create_dir(&planted).unwrap();
    fs::write(planted.join("trust.bin"), b"forged").unwrap();
    let strict = FolderSpec { trusted_owners: vec!["SY".into()], ..spec.clone() };
    let log = Lines::default();
    let rep = ensure_folder(&strict, &t.0, FOLDER_NAME, Some(&log)).unwrap();
    let aside = rep.moved_aside.expect("the planted folder was moved aside");
    assert!(aside.file_name().unwrap().to_string_lossy().starts_with("Swapdesk.untrusted-"));
    assert_eq!(fs::read(aside.join("trust.bin")).unwrap(), b"forged", "moved, not deleted");
    assert!(rep.created && !is_reparse(&rep.path));
    assert!(!rep.path.join("trust.bin").exists(), "the new folder starts empty");
    assert!(dacl_matches(&strict, &read_security(&rep.path).unwrap()).unwrap());
    assert!(log.0.lock().unwrap().iter().any(|l| l.contains("moved aside")), "an alert is logged");
}

#[test]
fn a_reparse_point_is_never_followed_it_is_moved_aside() {
    let t = Temp::new();
    let spec = test_spec();
    let target = t.0.join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), b"untouched").unwrap();
    let before = read_security(&target).unwrap();
    junction(&t.0.join(FOLDER_NAME), &target);

    let rep = ensure_folder(&spec, &t.0, FOLDER_NAME, None).unwrap();
    let aside = rep.moved_aside.expect("the junction was moved aside");
    assert!(is_reparse(&aside), "the link itself was renamed");
    assert!(!is_reparse(&rep.path) && rep.created, "the new folder is a real folder");
    assert!(dacl_matches(&spec, &read_security(&rep.path).unwrap()).unwrap());
    // The target was not followed: contents and security are exactly as before.
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"untouched");
    assert_eq!(read_security(&target).unwrap(), before);
    assert!(fs::read_dir(&target).unwrap().count() == 1);
}

#[test]
fn a_file_that_is_not_a_directory_is_moved_aside_too() {
    let t = Temp::new();
    fs::write(t.0.join(FOLDER_NAME), b"a file squatting on the name").unwrap();
    let rep = ensure_folder(&test_spec(), &t.0, FOLDER_NAME, None).unwrap();
    assert!(rep.moved_aside.is_some() && rep.path.is_dir());
}

#[test]
fn folder_names_with_separators_are_refused() {
    let t = Temp::new();
    for bad in ["", ".", "..", "a\\b", "a/b", "c:x"] {
        assert!(ensure_folder(&test_spec(), &t.0, bad, None).is_err(), "{bad:?}");
    }
}

/// Replaces the DACL of an existing folder (as a user with WRITE_DAC could), through a handle.
fn tamper(path: &Path, sddl: &str) {
    let sd = sys::SecurityDescriptor::from_sddl(sddl).unwrap();
    let h = sys::open_dir_for_security(path, true).unwrap();
    sys::set_dacl(&h, sd.dacl().unwrap()).unwrap();
}

#[test]
fn a_folder_with_a_trusted_owner_but_a_wrong_dacl_is_untrusted_and_moved_aside() {
    // Finding 1: while the DACL was wrong, another user could have written or kept handles; the folder is not
    // "restored", it is set aside with its contents and a fresh one is made.
    let t = Temp::new();
    let spec = test_spec();
    let first = ensure_folder(&spec, &t.0, FOLDER_NAME, None).unwrap();
    fs::write(first.path.join("trust.bin"), b"maybe tampered").unwrap();
    let me = &spec.owner;
    tamper(&first.path, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;WD)"));
    let log = Lines::default();
    let again = ensure_folder(&spec, &t.0, FOLDER_NAME, Some(&log)).unwrap();
    let aside = again.moved_aside.expect("a trusted owner with a wider DACL is not trusted");
    assert_eq!(fs::read(aside.join("trust.bin")).unwrap(), b"maybe tampered", "moved, not deleted");
    assert!(again.created && !again.path.join("trust.bin").exists());
    assert!(dacl_matches(&spec, &read_security(&again.path).unwrap()).unwrap());
    assert!(
        log.0.lock().unwrap().iter().any(|l| l.contains("moved aside") && l.contains("DACL")),
        "{:?}",
        log.0.lock().unwrap()
    );
}

#[test]
fn a_folder_whose_security_cannot_be_read_is_untrusted_and_moved_aside() {
    // Finding 5: the owner always holds READ_CONTROL unless an OWNER RIGHTS entry says otherwise, so the planted
    // folder carries one, and denies READ_CONTROL to the current user as well. It still allows DELETE, SYNCHRONIZE
    // and attributes (0x110081 with LIST), which a rename needs, so the move aside can succeed.
    let t = Temp::new();
    let spec = test_spec();
    let me = &spec.owner;
    let planted = t.0.join(FOLDER_NAME);
    let sd = sys::SecurityDescriptor::from_sddl(&format!("O:{me}D:P(D;;RC;;;{me})(A;;0x1;;;OW)(A;;0x110081;;;{me})"))
        .unwrap();
    assert!(sys::create_dir_with(&planted, &sd).unwrap());
    assert!(
        sys::open_dir_for_security(&planted, false).is_err(),
        "the planted folder must refuse READ_CONTROL for this test to mean anything"
    );
    let log = Lines::default();
    let rep = ensure_folder(&spec, &t.0, FOLDER_NAME, Some(&log)).unwrap();
    assert!(rep.moved_aside.is_some() && rep.created);
    assert!(dacl_matches(&spec, &read_security(&rep.path).unwrap()).unwrap());
    assert!(log.0.lock().unwrap().iter().any(|l| l.contains("moved aside") && l.contains("could not be judged")));
    let _ = fs::remove_dir(rep.moved_aside.unwrap());
}

/// Holds a directory open without FILE_SHARE_DELETE, so renaming it fails with a sharing violation.
fn hold_without_share_delete(path: &Path) -> sys::Handle {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let w = sys::wide(path.as_os_str()).unwrap();
    // SAFETY: `w` is a null-terminated wide string that outlives the call.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            0x8000_0000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(h, INVALID_HANDLE_VALUE);
    sys::Handle(h)
}

fn plant_untrusted(t: &Temp) -> (FolderSpec, PathBuf) {
    let planted = t.0.join(FOLDER_NAME);
    fs::create_dir(&planted).unwrap();
    (FolderSpec { trusted_owners: vec!["SY".into()], ..test_spec() }, planted)
}

#[test]
fn a_rename_that_keeps_failing_ends_in_a_clear_alert_and_an_error() {
    let t = Temp::new();
    let (strict, planted) = plant_untrusted(&t);
    let _hold = hold_without_share_delete(&planted);
    let log = Lines::default();
    let err = ensure_folder(&strict, &t.0, FOLDER_NAME, Some(&log)).unwrap_err();
    assert!(err.detail.contains("could not be moved aside"), "{err:?}");
    assert!(
        log.0.lock().unwrap().iter().any(|l| l.starts_with("alert:") && l.contains("could not be moved aside")),
        "{:?}",
        log.0.lock().unwrap()
    );
    assert!(planted.exists(), "nothing was deleted");
}

#[test]
fn a_rename_blocked_for_a_moment_is_retried() {
    let t = Temp::new();
    let (strict, planted) = plant_untrusted(&t);
    struct Moved(#[allow(dead_code)] sys::Handle);
    // SAFETY: a kernel handle may be closed from any thread.
    unsafe impl Send for Moved {}
    let hold = Moved(hold_without_share_delete(&planted));
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(hold);
    });
    let rep = ensure_folder(&strict, &t.0, FOLDER_NAME, None).unwrap();
    releaser.join().unwrap();
    assert!(rep.moved_aside.is_some() && rep.created);
}

// ---------- the store ----------

fn open(t: &Temp) -> WinSecretStore {
    WinSecretStore::open_in(&test_spec(), &t.0, None).unwrap()
}

#[test]
fn store_load_overwrite_delete() {
    let t = Temp::new();
    let s = open(&t);
    assert_eq!(s.load("identity.bin").unwrap(), None);
    s.store("identity.bin", b"first").unwrap();
    s.store("trust.bin", b"other").unwrap();
    assert_eq!(s.load("identity.bin").unwrap(), Some(b"first".to_vec()));
    s.store("identity.bin", b"second").unwrap();
    assert_eq!(s.load("identity.bin").unwrap(), Some(b"second".to_vec()));
    assert_eq!(s.load("trust.bin").unwrap(), Some(b"other".to_vec()));
    s.delete("identity.bin").unwrap();
    s.delete("identity.bin").unwrap();
    assert_eq!(s.load("identity.bin").unwrap(), None);
    // The write rule leaves no temporary file behind.
    let names: Vec<_> =
        fs::read_dir(s.dir()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["trust.bin".to_string()]);
}

#[test]
fn the_file_on_disk_is_a_dpapi_blob_bound_to_its_name() {
    let t = Temp::new();
    let s = open(&t);
    s.store("identity.bin", b"private-key-bytes-0123456789").unwrap();
    let raw = fs::read(s.dir().join("identity.bin")).unwrap();
    assert!(!raw.windows(10).any(|w| w == b"private-ke"));
    assert_eq!(unprotect(&raw, b"swapdesk identity v1").unwrap(), b"private-key-bytes-0123456789");
    // Copying the blob under another name does not open it: the entropy differs.
    fs::copy(s.dir().join("identity.bin"), s.dir().join("trust.bin")).unwrap();
    assert!(s.load("trust.bin").is_err());
}

#[test]
fn a_store_survives_being_reopened() {
    let t = Temp::new();
    open(&t).store("trust.bin", b"devices").unwrap();
    assert_eq!(open(&t).load("trust.bin").unwrap(), Some(b"devices".to_vec()));
}

#[test]
fn bad_names_are_refused() {
    let t = Temp::new();
    let s = open(&t);
    for bad in [
        "",
        ".",
        "..",
        "..\\x",
        "a/b",
        "a\\b",
        "C:x",
        "x:stream",
        ".hidden",
        "trust.bin.",
        "x.tmp",
        "CON",
        "nul.bin",
        "com1",
        "a b",
        "é",
    ] {
        assert!(s.store(bad, b"x").is_err(), "store {bad:?}");
        assert!(s.load(bad).is_err(), "load {bad:?}");
        assert!(s.delete(bad).is_err(), "delete {bad:?}");
    }
    assert!(s.store(&"a".repeat(65), b"x").is_err());
}

#[test]
fn a_reparse_point_in_the_folder_is_not_followed_for_reading_or_writing() {
    let t = Temp::new();
    let s = open(&t);
    let target = t.0.join("target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), b"untouched").unwrap();
    junction(&s.dir().join("trust.bin"), &target);

    assert!(s.load("trust.bin").is_err(), "reading through a junction must fail");
    assert!(s.store("trust.bin", b"x").is_err(), "writing over a junction must fail");
    assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"untouched");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 1, "nothing was written into the target");
    assert!(!s.dir().join("trust.bin.tmp").exists(), "no temporary file is left");
    // Deleting removes the link itself, never the target.
    let _ = fs::remove_dir(s.dir().join("trust.bin"));
    assert!(target.join("sentinel").exists());
}

#[test]
fn a_stale_temporary_file_does_not_block_a_write() {
    let t = Temp::new();
    let s = open(&t);
    fs::write(s.dir().join("trust.bin.tmp"), b"left by a crash").unwrap();
    s.store("trust.bin", b"now").unwrap();
    assert_eq!(s.load("trust.bin").unwrap(), Some(b"now".to_vec()));
    assert!(!s.dir().join("trust.bin.tmp").exists());
}

#[test]
fn files_inherit_the_protected_dacl_of_the_folder() {
    let t = Temp::new();
    let s = open(&t);
    s.store("trust.bin", b"x").unwrap();
    let read = read_security(&s.dir().join("trust.bin")).unwrap();
    // Inherited from the folder: the same three principals, flagged as inherited, no one else.
    assert_eq!(read.dacl_sddl.matches("(A;").count(), 3, "{}", read.dacl_sddl);
    for sid in test_spec().full_control {
        let sid = normalize_sid(&sid).unwrap();
        let alias = match sid.as_str() {
            "S-1-5-18" => "SY".to_string(),
            "S-1-5-32-544" => "BA".to_string(),
            other => other.to_string(),
        };
        assert!(read.dacl_sddl.contains(&alias), "{alias} missing from {}", read.dacl_sddl);
    }
}

// ---------- stage 34 ----------

#[test]
fn load_refuses_a_file_whose_owner_is_not_trusted() {
    let t = Temp::new();
    let mut s = open(&t);
    s.store("trust.bin", b"devices").unwrap();
    assert_eq!(s.load("trust.bin").unwrap(), Some(b"devices".to_vec()), "the current user is the trusted owner here");
    // The same file, read by a store that trusts only SYSTEM: its owner (this user) is not trusted.
    s.trusted_owners = vec![normalize_sid("SY").unwrap()];
    let err = s.load("trust.bin").unwrap_err();
    assert!(err.detail.contains("not a trusted owner"), "{err:?}");
    // A missing file is still just absent, and the check does not touch writing.
    assert_eq!(s.load("identity.bin").unwrap(), None);
    s.store("identity.bin", b"x").unwrap();
}

#[test]
fn a_name_swapped_after_the_judgment_is_judged_again() {
    // Finding 6: the DACL is fixed through the handle the folder was judged on, and the name is checked against
    // that handle afterwards; a swap in between sends the folder back to be judged again.
    let t = Temp::new();
    let spec = test_spec();
    ensure_folder(&spec, &t.0, FOLDER_NAME, None).unwrap();
    let target = t.0.join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("sentinel"), b"untouched").unwrap();
    let before = read_security(&target).unwrap();
    let mut swapped = false;
    let rep = folder::ensure_folder_with(&spec, &t.0, FOLDER_NAME, None, &mut || {
        if !swapped {
            swapped = true;
            fs::rename(t.0.join(FOLDER_NAME), t.0.join("original")).unwrap();
            junction(&t.0.join(FOLDER_NAME), &target);
        }
    })
    .unwrap();
    assert!(swapped);
    assert!(
        !is_reparse(&rep.path) && rep.created,
        "the junction was judged, moved aside and replaced by a real folder"
    );
    assert!(is_reparse(&rep.moved_aside.expect("the junction was moved aside")));
    assert_eq!(read_security(&target).unwrap(), before, "the target of the link was never touched");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
}

#[test]
fn wipe_zeroes_every_byte() {
    let mut v = vec![0xAAu8; 37];
    sys::wipe(&mut v);
    assert!(v.iter().all(|&b| b == 0));
    sys::wipe(&mut []);
}

#[test]
fn the_dpapi_output_buffer_is_wiped_after_it_is_copied() {
    let mut buffer = b"plaintext key material from DPAPI".to_vec();
    let blob = windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        cbData: buffer.len() as u32,
        pbData: buffer.as_mut_ptr(),
    };
    let copy = dpapi::copy_and_wipe(&blob);
    assert_eq!(copy, b"plaintext key material from DPAPI");
    assert!(buffer.iter().all(|&b| b == 0), "the system's buffer must be zeroed before it is freed");
}
