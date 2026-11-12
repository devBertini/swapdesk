//! UPDATES.md 14 U4: the manager with fakes for HTTP, the saved state and the folder.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ring::rand::SystemRandom;
use ring::signature::Ed25519KeyPair;

use super::*;
use crate::manifest::{Apply, Channel, Manifest, public_key, sign};

const BASE: &str = "https://github.com/devBertini/swapdesk/releases";
const START: u64 = 1_000_000;

/// The answers of a fake server by URL, and the URLs asked.
#[derive(Default)]
struct FakeHttp {
    answers: Mutex<HashMap<String, Result<Vec<u8>, HttpError>>>,
    asked: Mutex<Vec<String>>,
}

impl FakeHttp {
    fn answer(&self, url: &str, r: Result<Vec<u8>, HttpError>) {
        self.answers.lock().unwrap().insert(url.to_string(), r);
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl HttpGet for FakeHttp {
    fn get(
        &self,
        url: &str,
        max_bytes: u64,
        sink: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<(), HttpError> {
        self.asked.lock().unwrap().push(url.to_string());
        if cancel.load(Ordering::Acquire) {
            return Err(HttpError::Cancelled);
        }
        let answer = self.answers.lock().unwrap().get(url).cloned();
        let body = answer.unwrap_or(Err(HttpError::Status(404)))?;
        let take = (body.len() as u64).min(max_bytes) as usize;
        sink.write_all(&body[..take]).unwrap();
        progress(take as u64);
        if take < body.len() { Err(HttpError::TooLarge) } else { Ok(()) }
    }
}

#[derive(Default)]
struct FakeStore(Mutex<Option<Saved>>);

impl SavedStore for FakeStore {
    fn load(&self) -> Saved {
        self.0.lock().unwrap().clone().unwrap_or_default()
    }

    fn save(&self, saved: &Saved) {
        *self.0.lock().unwrap() = Some(saved.clone());
    }
}

/// The update folder in memory: what was written, by name, and what is still there.
/// One file's bytes, shared between the folder and the writer that fills it.
type Bytes = Arc<Mutex<Vec<u8>>>;

#[derive(Default, Clone)]
struct FakeFolder(Arc<Mutex<HashMap<String, Bytes>>>);

struct FileOf(Bytes);

impl Write for FileOf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl UpdateFolder for FakeFolder {
    fn create(&self, name: &str) -> std::io::Result<Box<dyn Write + Send>> {
        let file = Arc::new(Mutex::new(Vec::new()));
        self.0.lock().unwrap().insert(name.to_string(), Arc::clone(&file));
        Ok(Box::new(FileOf(file)))
    }

    fn remove(&self, name: &str) {
        self.0.lock().unwrap().remove(name);
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from("update").join(name)
    }
}

impl FakeFolder {
    fn names(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

struct Rig {
    m: UpdateManager,
    http: Arc<FakeHttp>,
    store: Arc<FakeStore>,
    folder: FakeFolder,
    pkcs8: Vec<u8>,
}

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn rig_with(dev: bool, saved: Option<Saved>) -> Rig {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap().as_ref().to_vec();
    let key = public_key(&pkcs8).unwrap();
    let http = Arc::new(FakeHttp::default());
    let store = Arc::new(FakeStore(Mutex::new(saved)));
    let folder = FakeFolder::default();
    let setup = Setup {
        running: v("1.1.0"),
        target: "windows-x64".into(),
        base: BASE.into(),
        keys: vec![key],
        dev,
        first_delay_ms: 5 * 60_000,
    };
    let m = UpdateManager::new(setup, http.clone(), store.clone(), Arc::new(folder.clone()), START);
    Rig { m, http, store, folder, pkcs8 }
}

fn rig() -> Rig {
    rig_with(false, None)
}

const PACKAGE: &[u8] = b"the package bytes";

impl Rig {
    /// Publishes a signed manifest offering `version` (with the package `PACKAGE`).
    fn publish(&self, version: &str, apply: Apply) {
        let entry = FileEntry {
            target: "windows-x64".into(),
            name: format!("Swapdesk-windows-x64-{version}.sdpkg"),
            size: PACKAGE.len() as u64,
            sha256: ring::digest::digest(&ring::digest::SHA256, PACKAGE).as_ref().try_into().unwrap(),
        };
        let m = Manifest { version: v(version), channel: Channel::Stable, apply, files: vec![entry.clone()] };
        let text = m.to_text();
        let sig = sign(text.as_bytes(), &self.pkcs8).unwrap();
        self.http.answer(&format!("{BASE}/latest/download/{MANIFEST_NAME}"), Ok(text.into_bytes()));
        self.http.answer(&format!("{BASE}/latest/download/{SIGNATURE_NAME}"), Ok(sig.into_bytes()));
        self.http.answer(&format!("{BASE}/download/v{version}/{}", entry.name), Ok(PACKAGE.to_vec()));
    }
}

const CONSOLE: InstallAsk = InstallAsk { from_console: true, live_sessions: 0, end_sessions: false };

#[test]
fn the_first_check_waits_its_delay_then_runs_every_period() {
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    assert!(!r.m.tick(START + 4 * 60_000, 0), "before the delay: nothing");
    assert!(r.http.asked().is_empty());
    assert!(r.m.tick(START + 5 * 60_000, 0), "the delay passed: one check");
    assert_eq!(r.m.view().phase, Phase::Offered { version: v("1.1.1"), manual: false });
    let first = START + 5 * 60_000;
    assert!(!r.m.tick(first + PERIOD_MS - 1, 0), "not before the period");
    assert!(r.m.tick(first + PERIOD_MS, 0), "a period later");
    // The jitter moves the next one within its hour.
    assert_eq!(r.m.next_check_ms(), Some(first + 2 * PERIOD_MS));
    assert!(r.m.tick(first + 2 * PERIOD_MS, -(PERIOD_JITTER_MS as i64)));
    assert_eq!(r.m.next_check_ms(), Some(first + 3 * PERIOD_MS - PERIOD_JITTER_MS));
}

#[test]
fn the_switch_off_sends_nothing() {
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    r.m.set_check(false);
    assert_eq!(r.m.next_check_ms(), None);
    for t in [START + 10 * 60_000, START + 3 * PERIOD_MS] {
        assert!(!r.m.tick(t, 0));
    }
    assert!(r.http.asked().is_empty(), "no request at all");
    assert!(!r.store.load().check_on, "kept across restarts");
}

#[test]
fn development_instances_never_check() {
    let r = rig_with(true, None);
    r.publish("1.1.1", Apply::Auto);
    assert_eq!(r.m.next_check_ms(), None);
    assert!(!r.m.tick(START + PERIOD_MS, 0));
    assert_eq!(r.m.check(START, 0), Ok(()), "even 'Procurar agora' sends nothing");
    assert!(r.http.asked().is_empty());
    assert_eq!(r.m.install(&v("1.1.1"), CONSOLE, &AtomicBool::new(false)), Err(InstallFailure::NotOffered));
}

#[test]
fn a_failed_or_rate_limited_check_is_silent_and_waits_for_the_next_period() {
    let r = rig();
    let manifest_url = format!("{BASE}/latest/download/{MANIFEST_NAME}");
    r.http.answer(&manifest_url, Err(HttpError::Status(429)));
    assert_eq!(r.m.check(START, 0), Err(CheckFailure::RateLimited));
    assert_eq!(r.m.view().phase, Phase::Idle, "silent: nothing shows on Início");
    assert_eq!(r.m.view().last_failure, Some(CheckFailure::RateLimited), "'Procurar agora' can say it");
    assert_eq!(r.m.next_check_ms(), Some(START + PERIOD_MS), "the next try is a period later");
    r.http.answer(&manifest_url, Err(HttpError::Network("no route".into())));
    assert_eq!(r.m.check(START, 0), Err(CheckFailure::Network));
    // An offer on screen stays when a later check fails.
    r.publish("1.1.1", Apply::Auto);
    assert_eq!(r.m.check(START, 0), Ok(()));
    r.http.answer(&manifest_url, Err(HttpError::Network("no route".into())));
    assert_eq!(r.m.check(START + PERIOD_MS, 0), Err(CheckFailure::Network));
    assert_eq!(r.m.view().phase, Phase::Offered { version: v("1.1.1"), manual: false });
    // A manifest whose signature does not verify is refused.
    r.http.answer(&manifest_url, Ok(b"swapdesk-update 1\n".to_vec()));
    assert_eq!(r.m.check(START + 2 * PERIOD_MS, 0), Err(CheckFailure::Signature));
}

#[test]
fn install_is_refused_from_a_ui_outside_the_console_session() {
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    r.m.check(START, 0).unwrap();
    let ask = InstallAsk { from_console: false, ..CONSOLE };
    assert_eq!(r.m.install(&v("1.1.1"), ask, &AtomicBool::new(false)), Err(InstallFailure::NotConsole));
    assert!(r.folder.names().is_empty(), "nothing downloaded");
}

#[test]
fn install_with_a_live_session_needs_the_confirmation() {
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    r.m.check(START, 0).unwrap();
    let live = InstallAsk { live_sessions: 1, ..CONSOLE };
    assert_eq!(r.m.install(&v("1.1.1"), live, &AtomicBool::new(false)), Err(InstallFailure::NeedsConfirmation));
    let confirmed = InstallAsk { end_sessions: true, ..live };
    assert!(r.m.install(&v("1.1.1"), confirmed, &AtomicBool::new(false)).is_ok(), "Encerrar e atualizar");
}

#[test]
fn the_download_lands_only_in_the_update_folder_and_is_deleted_on_any_failure() {
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    r.m.check(START, 0).unwrap();
    let path = r.m.install(&v("1.1.1"), CONSOLE, &AtomicBool::new(false)).unwrap();
    assert_eq!(path, std::path::PathBuf::from("update").join("Swapdesk-windows-x64-1.1.1.sdpkg"));
    assert_eq!(r.m.view().phase, Phase::Ready { version: v("1.1.1") });

    // A package whose bytes are not the manifest's: refused, and the partial file is gone.
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    r.m.check(START, 0).unwrap();
    let name = "Swapdesk-windows-x64-1.1.1.sdpkg";
    r.http.answer(&format!("{BASE}/download/v1.1.1/{name}"), Ok(b"other bytes of a size".to_vec()));
    assert!(matches!(
        r.m.install(&v("1.1.1"), CONSOLE, &AtomicBool::new(false)),
        Err(InstallFailure::Mismatch | InstallFailure::Download)
    ));
    assert!(r.folder.names().is_empty(), "deleted on failure");
    // A cancel: the same.
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    r.m.check(START, 0).unwrap();
    assert_eq!(r.m.install(&v("1.1.1"), CONSOLE, &AtomicBool::new(true)), Err(InstallFailure::Cancelled));
    assert!(r.folder.names().is_empty());
    // Only the offered version, and a manual release never downloads.
    let r = rig();
    r.publish("1.2.0", Apply::Manual);
    r.m.check(START, 0).unwrap();
    assert_eq!(r.m.view().phase, Phase::Offered { version: v("1.2.0"), manual: true });
    assert_eq!(r.m.install(&v("1.2.0"), CONSOLE, &AtomicBool::new(false)), Err(InstallFailure::NotOffered));
    assert_eq!(r.m.install(&v("9.9.9"), CONSOLE, &AtomicBool::new(false)), Err(InstallFailure::NotOffered));
}

#[test]
fn a_rolled_back_version_is_not_offered_again_by_itself() {
    let r = rig();
    r.publish("1.1.1", Apply::Auto);
    r.m.check(START, 0).unwrap();
    r.m.rolled_back(&v("1.1.1"));
    assert_eq!(r.m.view().phase, Phase::Idle);
    r.m.check(START + PERIOD_MS, 0).unwrap();
    assert_eq!(r.m.view().phase, Phase::Idle, "not offered again");
    assert_eq!(r.store.load().rolled_back, vec![v("1.1.1")], "kept across restarts");
    // A newer one is.
    r.publish("1.1.2", Apply::Auto);
    r.m.check(START + 2 * PERIOD_MS, 0).unwrap();
    assert_eq!(r.m.view().phase, Phase::Offered { version: v("1.1.2"), manual: false });
}

#[test]
fn the_saved_state_round_trips_and_a_bad_file_is_the_defaults() {
    let s = Saved {
        check_on: false,
        last_check_ms: Some(1_700_000_000_000),
        dismissed: Some(v("1.1.1")),
        rolled_back: vec![v("1.1.2"), v("1.1.3")],
    };
    assert_eq!(Saved::from_text(&s.to_text()), s);
    assert_eq!(Saved::from_text("garbage"), Saved::default());
    assert!(Saved::from_text("").check_on, "the switch is on by default");
}
