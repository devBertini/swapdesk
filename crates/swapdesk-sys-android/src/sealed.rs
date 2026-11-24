//! The phone's `SecretStore` (C5): each blob is a file in the app's private folder, sealed with one data key.
//!
//! The data key is 32 random bytes that never rest on disk in the clear: the Java side keeps them wrapped by a key
//! of the Android Keystore, which never leaves the phone's secure hardware, unwraps them once when the app starts
//! and hands them to the core (docs/ANDROID.md 5). Everything after that is here, in plain Rust: AES-256-GCM with a
//! fresh random nonce for every write and the blob's name as associated data, so a file moved under another name
//! does not open. A write goes to a temporary file that is flushed and renamed over the blob, as on the desktops.
//!
//! Built and tested on every host.

use std::fs::{self, File};
use std::io::{ErrorKind as IoKind, Write};
use std::path::{Path, PathBuf};

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use swapdesk_platform::{ErrorKind, PlatformError, Result, SecretStore};

/// Length of the data key.
pub const KEY_LEN: usize = 32;
/// What every sealed file starts with: two letters and the format's number.
const MAGIC: [u8; 3] = [b'S', b'D', 1];
/// A write in progress; such a name is never a blob's.
const TEMP_SUFFIX: &str = ".tmp";

fn error(kind: ErrorKind, detail: impl Into<String>) -> PlatformError {
    PlatformError { kind, op: "SealedStore", os_code: None, detail: detail.into() }
}

fn io_error(what: &str, e: &std::io::Error) -> PlatformError {
    PlatformError {
        kind: ErrorKind::Os,
        op: "SealedStore",
        os_code: e.raw_os_error().map(i64::from),
        detail: format!("{what}: {e}"),
    }
}

/// The names the desktops' stores take: letters, digits, `.`, `_` and `-`, at most 64, not starting or ending with a
/// dot, and never a temporary file's.
pub fn check_name(name: &str) -> Result<()> {
    let plain = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.ends_with(TEMP_SUFFIX)
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if plain { Ok(()) } else { Err(error(ErrorKind::InvalidInput, format!("{name:?} is not a valid secret name"))) }
}

/// The store of one app: a folder and the data key.
pub struct SealedStore {
    dir: PathBuf,
    key: LessSafeKey,
    random: SystemRandom,
}

impl SealedStore {
    /// Opens the store in `dir` (created when missing) with the data key.
    pub fn open(dir: &Path, key: &[u8; KEY_LEN]) -> Result<Self> {
        fs::create_dir_all(dir).map_err(|e| io_error("the secrets folder", &e))?;
        let key = UnboundKey::new(&AES_256_GCM, key).map_err(|_| error(ErrorKind::InvalidInput, "the data key"))?;
        Ok(Self { dir: dir.to_owned(), key: LessSafeKey::new(key), random: SystemRandom::new() })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl SecretStore for SealedStore {
    fn load(&self, name: &str) -> Result<Option<Vec<u8>>> {
        check_name(name)?;
        let mut sealed = match fs::read(self.path(name)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == IoKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("reading a secret", &e)),
        };
        let head = MAGIC.len() + NONCE_LEN;
        if sealed.len() < head || sealed[..MAGIC.len()] != MAGIC {
            return Err(error(ErrorKind::InvalidInput, format!("{name} is not a sealed file of this format")));
        }
        let nonce = Nonce::try_assume_unique_for_key(&sealed[MAGIC.len()..head])
            .map_err(|_| error(ErrorKind::InvalidInput, "the nonce"))?;
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(name.as_bytes()), &mut sealed[head..])
            .map_err(|_| error(ErrorKind::InvalidInput, format!("{name} does not open with this device's key")))?;
        Ok(Some(plain.to_vec()))
    }

    fn store(&self, name: &str, bytes: &[u8]) -> Result<()> {
        check_name(name)?;
        let mut nonce = [0u8; NONCE_LEN];
        self.random.fill(&mut nonce).map_err(|_| error(ErrorKind::Os, "the system gave no random bytes"))?;
        let mut body = bytes.to_vec();
        self.key
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(name.as_bytes()), &mut body)
            .map_err(|_| error(ErrorKind::Os, "sealing"))?;
        let temp = self.path(&format!("{name}{TEMP_SUFFIX}"));
        let written = (|| {
            let mut file = File::create(&temp)?;
            file.write_all(&MAGIC)?;
            file.write_all(&nonce)?;
            file.write_all(&body)?;
            file.sync_all()?;
            fs::rename(&temp, self.path(name))
        })();
        written.map_err(|e| {
            let _ = fs::remove_file(&temp);
            io_error("writing a secret", &e)
        })
    }

    fn delete(&self, name: &str) -> Result<()> {
        check_name(name)?;
        match fs::remove_file(self.path(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == IoKind::NotFound => Ok(()),
            Err(e) => Err(io_error("deleting a secret", &e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of its own under the system's temporary folder, removed when the test ends.
    struct Folder(PathBuf);

    impl Folder {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("swapdesk-sealed-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const KEY: [u8; KEY_LEN] = [7; KEY_LEN];

    #[test]
    fn a_blob_comes_back_and_never_rests_in_the_clear() {
        let folder = Folder::new("round");
        let store = SealedStore::open(&folder.0, &KEY).unwrap();
        assert_eq!(store.load("trust.bin").unwrap(), None, "nothing stored yet");
        let secret = b"the pinned keys of this phone".to_vec();
        store.store("trust.bin", &secret).unwrap();
        assert_eq!(store.load("trust.bin").unwrap(), Some(secret.clone()));
        let on_disk = fs::read(folder.0.join("trust.bin")).unwrap();
        assert!(!on_disk.windows(8).any(|w| secret.windows(8).any(|s| s == w)), "the text is not in the file");
        assert_eq!(on_disk.len(), MAGIC.len() + NONCE_LEN + secret.len() + 16);
        // Opened again with the same key, as after a restart of the app.
        assert_eq!(SealedStore::open(&folder.0, &KEY).unwrap().load("trust.bin").unwrap(), Some(secret));
        store.store("trust.bin", b"").unwrap();
        assert_eq!(store.load("trust.bin").unwrap(), Some(Vec::new()), "an empty blob is a blob");
        store.delete("trust.bin").unwrap();
        store.delete("trust.bin").unwrap();
        assert_eq!(store.load("trust.bin").unwrap(), None);
        assert!(!folder.0.join("trust.bin.tmp").exists(), "no temporary file is left");
    }

    #[test]
    fn every_write_takes_a_new_nonce() {
        let folder = Folder::new("nonce");
        let store = SealedStore::open(&folder.0, &KEY).unwrap();
        store.store("a", b"same bytes").unwrap();
        let first = fs::read(folder.0.join("a")).unwrap();
        store.store("a", b"same bytes").unwrap();
        assert_ne!(first, fs::read(folder.0.join("a")).unwrap());
    }

    #[test]
    fn a_changed_file_another_name_or_another_key_does_not_open() {
        let folder = Folder::new("tamper");
        let store = SealedStore::open(&folder.0, &KEY).unwrap();
        store.store("identity.bin", b"the device key").unwrap();
        let path = folder.0.join("identity.bin");
        let good = fs::read(&path).unwrap();

        // The file under another name: the name is part of what was sealed.
        fs::copy(&path, folder.0.join("trust.bin")).unwrap();
        assert_eq!(store.load("trust.bin").unwrap_err().kind, ErrorKind::InvalidInput);
        // Another key (another phone, or a Keystore that lost its key).
        let other = SealedStore::open(&folder.0, &[8; KEY_LEN]).unwrap();
        assert_eq!(other.load("identity.bin").unwrap_err().kind, ErrorKind::InvalidInput);
        // One flipped bit anywhere, and a file cut short.
        for at in [0, MAGIC.len(), MAGIC.len() + NONCE_LEN, good.len() - 1] {
            let mut bad = good.clone();
            bad[at] ^= 1;
            fs::write(&path, &bad).unwrap();
            assert!(store.load("identity.bin").is_err(), "a flipped bit at {at}");
        }
        fs::write(&path, &good[..10]).unwrap();
        assert!(store.load("identity.bin").is_err(), "a file cut short");
        fs::write(&path, &good).unwrap();
        assert_eq!(store.load("identity.bin").unwrap(), Some(b"the device key".to_vec()));
    }

    #[test]
    fn names_follow_the_desktops_rule() {
        let folder = Folder::new("names");
        let store = SealedStore::open(&folder.0, &KEY).unwrap();
        for ok in ["identity.bin", "trust.bin", "systems.bin", "settings.bin", "a", "A_b-c.d"] {
            assert!(store.store(ok, b"x").is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "trailing.", "a/b", "a\\b", "..", "x.tmp", "sp ace", "ü", &"n".repeat(65)] {
            assert_eq!(store.store(bad, b"x").unwrap_err().kind, ErrorKind::InvalidInput, "{bad:?}");
            assert_eq!(store.load(bad).unwrap_err().kind, ErrorKind::InvalidInput, "{bad:?}");
            assert_eq!(store.delete(bad).unwrap_err().kind, ErrorKind::InvalidInput, "{bad:?}");
        }
    }
}
