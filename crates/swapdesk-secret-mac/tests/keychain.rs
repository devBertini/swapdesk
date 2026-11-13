//! The store against the real Security framework, and the folder rules on a temporary folder.
//!
//! * `file_*` tests use a throwaway keychain file made for the test (`create_keychain_file`, not in the search list), so
//!   they run anywhere, including over SSH, where the login keychain cannot be reached (no GUI session: status -25308).
//! * `login_*` tests use the user's login keychain with a unique service name ("swapdesk-test-<pid>-<nanos>-<n>") that
//!   is deleted at the end, so the real "Swapdesk" items are never touched. When macOS cannot serve the login keychain
//!   (`ErrorKind::Unavailable`: locked or no GUI session) the test prints why and ends without asserting; run it from a
//!   Terminal in the owner's GUI session to prove it.
#![cfg(target_os = "macos")]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use swapdesk_platform::{ErrorKind, SecretStore};
use swapdesk_secret_mac::{
    FolderOwner, KeychainLocation, MacSecretStore, SecretLocation, create_keychain_file, delete_keychain_file,
    ensure_folder,
};

static N: AtomicU32 = AtomicU32::new(0);

/// A scratch folder, service name and (for the file flavour) keychain that only this test uses, removed when dropped.
struct Scratch {
    dir: PathBuf,
    keychain: Option<PathBuf>,
    store: MacSecretStore,
}

fn tag() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("swapdesk-test-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

impl Scratch {
    fn new(file_keychain: bool) -> Self {
        let tag = tag();
        let dir = std::env::temp_dir().join(&tag);
        std::fs::create_dir_all(&dir).expect("scratch folder");
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let mut location = SecretLocation::login(&home).with(tag.clone(), dir.join("folder"));
        let keychain = file_keychain.then(|| dir.join(format!("{tag}.keychain-db")));
        if let Some(path) = &keychain {
            create_keychain_file(path, "swapdesk-test-password").expect("a throwaway keychain is created");
            location.keychain = KeychainLocation::File(path.clone());
        }
        let store = MacSecretStore::open(&location).expect("the store opens");
        Self { dir, keychain, store }
    }

    /// The login keychain flavour, or `None` (with the reason printed) when macOS cannot serve it now.
    fn login() -> Option<Self> {
        let s = Self::new(false);
        // A load of a missing item never needs the keychain's key; a write does, so the probe writes and removes.
        match s.store.store("probe", b"x").and_then(|()| s.store.delete("probe")) {
            Ok(()) => Some(s),
            Err(e) if e.kind == ErrorKind::Unavailable => {
                println!(
                    "SKIPPED: the login keychain is not reachable from this session ({e}); run this test from the owner's GUI session"
                );
                None
            }
            Err(e) => panic!("login keychain probe: {e}"),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = self.store.delete_all();
        if let Some(k) = &self.keychain {
            let _ = delete_keychain_file(k);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn round_trip(store: &MacSecretStore) {
    let bytes: Vec<u8> = (0..=255u8).cycle().take(20_000).collect();
    assert_eq!(store.load("identity.bin").expect("load of a missing item"), None);
    store.store("identity.bin", &bytes).expect("store");
    store.store("trust.bin", b"trust").expect("store");
    assert_eq!(store.load("identity.bin").expect("load"), Some(bytes));
    assert_eq!(store.load("trust.bin").expect("load"), Some(b"trust".to_vec()));

    store.store("identity.bin", &[1, 2, 3]).expect("overwrite");
    assert_eq!(store.load("identity.bin").expect("load"), Some(vec![1, 2, 3]));
    assert_eq!(store.load("trust.bin").expect("load"), Some(b"trust".to_vec()), "the other item is untouched");

    store.delete("identity.bin").expect("delete");
    store.delete("identity.bin").expect("delete again");
    assert_eq!(store.load("identity.bin").expect("load"), None);

    // An empty value is still a value.
    store.store("settings.bin", &[]).expect("store empty");
    assert_eq!(store.load("settings.bin").expect("load empty"), Some(Vec::new()));

    store.delete_all().expect("delete_all");
    assert_eq!(store.load("trust.bin").expect("load"), None);
    assert_eq!(store.load("settings.bin").expect("load"), None);
}

/// macOS locks a new keychain file after five minutes without use (and when the Mac sleeps); a call on a locked
/// keychain waits for a prompt, and a throwaway one has nobody to answer it (a scratch run of the app that lasted
/// over five minutes hung at quit). Every call of the store unlocks its throwaway keychain again, with no prompt.
#[test]
fn file_keychain_that_locked_itself_serves_the_next_calls() {
    let s = Scratch::new(true);
    let path = s.keychain.as_ref().expect("a file keychain");
    s.store.store("kept", b"value").expect("store");

    swapdesk_secret_mac::lock_keychain_file(path).expect("the keychain locks");
    assert_eq!(s.store.load("kept").expect("load after the lock").as_deref(), Some(&b"value"[..]));
    swapdesk_secret_mac::lock_keychain_file(path).expect("the keychain locks");
    s.store.store("second", b"2").expect("store after the lock");
    swapdesk_secret_mac::lock_keychain_file(path).expect("the keychain locks");
    s.store.delete("kept").expect("delete after the lock");
    assert_eq!(s.store.load("kept").expect("load").as_deref(), None);
    swapdesk_secret_mac::lock_keychain_file(path).expect("the keychain locks");
    s.store.delete_all().expect("delete everything after the lock");
}

#[test]
fn file_keychain_store_load_overwrite_delete_round_trip() {
    round_trip(&Scratch::new(true).store);
}

#[test]
fn login_keychain_store_load_overwrite_delete_round_trip() {
    if let Some(s) = Scratch::login() {
        round_trip(&s.store);
        println!("login keychain round trip: ok");
    }
}

#[test]
fn stores_with_different_services_do_not_see_each_other() {
    let a = Scratch::new(true);
    let b = Scratch::new(true);
    assert_ne!(a.store.service(), b.store.service());
    a.store.store("identity.bin", b"a").expect("store");
    assert_eq!(b.store.load("identity.bin").expect("load"), None);
    b.store.store("identity.bin", b"b").expect("store");
    assert_eq!(a.store.load("identity.bin").expect("load"), Some(b"a".to_vec()));
    b.store.delete("identity.bin").expect("delete");
    assert_eq!(a.store.load("identity.bin").expect("load"), Some(b"a".to_vec()));
}

#[test]
fn a_file_keychain_is_not_the_login_keychain() {
    // The item of a file keychain is invisible to a store on the login keychain with the same service, and the other
    // way round: the location decides where the bytes live.
    let a = Scratch::new(true);
    a.store.store("identity.bin", b"in the file").expect("store");
    let Some(login) = Scratch::login() else { return };
    let same_service = SecretLocation::login(&PathBuf::from(std::env::var_os("HOME").expect("HOME")))
        .with(a.store.service().to_string(), a.dir.join("folder2"));
    let other = MacSecretStore::open(&same_service).expect("opens");
    assert_eq!(other.load("identity.bin").expect("load"), None);
    drop(login);
}

#[test]
fn a_bad_name_is_refused_before_the_keychain_is_asked() {
    let s = Scratch::new(true);
    for bad in ["", "../x", "a/b", ".x", "x.tmp"] {
        assert_eq!(s.store.load(bad).unwrap_err().kind, ErrorKind::InvalidInput, "{bad:?}");
        assert_eq!(s.store.store(bad, b"x").unwrap_err().kind, ErrorKind::InvalidInput, "{bad:?}");
        assert_eq!(s.store.delete(bad).unwrap_err().kind, ErrorKind::InvalidInput, "{bad:?}");
    }
}

#[test]
fn the_folder_is_created_0700_and_owned_by_the_user() {
    let s = Scratch::new(true);
    let meta = std::fs::symlink_metadata(s.store.dir()).expect("folder");
    assert!(meta.is_dir());
    assert_eq!(meta.mode() & 0o777, 0o700);
    assert_eq!(meta.uid(), current_uid());
}

#[test]
fn a_loose_folder_is_tightened_and_a_symlink_a_file_or_a_foreign_owner_are_refused() {
    let s = Scratch::new(true);
    let dir = s.dir.join("rules");
    std::fs::create_dir_all(&dir).expect("scratch");

    let loose = dir.join("loose");
    std::fs::create_dir(&loose).expect("mkdir");
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    ensure_folder(&loose, FolderOwner::CurrentUser).expect("owned by us: tightened");
    assert_eq!(std::fs::metadata(&loose).expect("meta").mode() & 0o777, 0o700);

    let target = dir.join("target");
    std::fs::create_dir(&target).expect("mkdir");
    let link = dir.join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    assert_eq!(
        ensure_folder(&link, FolderOwner::CurrentUser).unwrap_err().kind,
        ErrorKind::InvalidInput,
        "a symlink is never followed"
    );

    let file = dir.join("file");
    std::fs::write(&file, b"x").expect("write");
    assert_eq!(
        ensure_folder(&file, FolderOwner::CurrentUser).unwrap_err().kind,
        ErrorKind::InvalidInput,
        "a file is not a folder"
    );

    // A folder this user owns is not root's: the stage 24 owner check refuses it.
    assert_eq!(ensure_folder(&loose, FolderOwner::Root).unwrap_err().kind, ErrorKind::InvalidInput);

    // Nested missing folders are all created 0700.
    let nested = dir.join("a").join("b");
    ensure_folder(&nested, FolderOwner::CurrentUser).expect("nested");
    assert_eq!(std::fs::metadata(dir.join("a")).expect("meta").mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(&nested).expect("meta").mode() & 0o777, 0o700);
}

/// The effective uid, read from a folder this process just made (the test crate needs no `unsafe`).
fn current_uid() -> u32 {
    let probe = std::env::temp_dir().join(format!("swapdesk-uid-probe-{}", std::process::id()));
    std::fs::create_dir_all(&probe).expect("probe");
    let uid = std::fs::metadata(&probe).expect("meta").uid();
    let _ = std::fs::remove_dir(&probe);
    uid
}
