//! The file-backed store (D31): secrets as 0600 files in a 0700 `secrets` folder, no keychain prompt, values that
//! survive reopening (what a rebuilt or updated app does).
#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use swapdesk_platform::SecretStore;
use swapdesk_secret_mac::{KeychainLocation, MacSecretStore, SecretLocation};

fn location(tag: &str) -> (SecretLocation, PathBuf) {
    let dir = std::env::temp_dir().join(format!("swapdesk-secret-files-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    // A unique service, so the one-time lookup of a legacy login-keychain item finds nothing and never prompts.
    let l = SecretLocation::login(&home)
        .with(format!("swapdesk-files-test-{tag}-{}", std::process::id()), dir.join("folder"));
    assert_eq!(l.keychain, KeychainLocation::Files);
    (l, dir)
}

#[test]
fn secrets_are_private_files_that_survive_reopening() {
    let (loc, dir) = location("roundtrip");
    let store = MacSecretStore::open(&loc).unwrap();
    assert_eq!(store.load("identity.bin").unwrap(), None);
    store.store("identity.bin", b"key bytes").unwrap();
    store.store("identity.bin", b"new key bytes").unwrap();
    let file = loc.folder.join("secrets").join("identity.bin");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(loc.folder.join("secrets")).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(!loc.folder.join("secrets").join("identity.bin.tmp").exists(), "no temporary file is left");
    drop(store);
    let reopened = MacSecretStore::open(&loc).unwrap();
    assert_eq!(reopened.load("identity.bin").unwrap().as_deref(), Some(&b"new key bytes"[..]));
    reopened.delete("identity.bin").unwrap();
    reopened.delete("identity.bin").unwrap();
    assert_eq!(reopened.load("identity.bin").unwrap(), None);
    reopened.store("a.bin", b"1").unwrap();
    reopened.delete_all().unwrap();
    assert_eq!(reopened.load("a.bin").unwrap(), None);
    assert!(reopened.load("../escape").is_err(), "names are checked");
    let _ = std::fs::remove_dir_all(&dir);
}
