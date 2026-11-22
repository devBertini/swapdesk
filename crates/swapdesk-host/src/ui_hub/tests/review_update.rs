//! Review of 22/11/2026, G7: "Atualizar agora" through the hub when a session is accepted while the package
//! downloads. The live-session count of the request is minutes old when the download ends; the cut reads it again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use swapdesk_ipc::UpdatePhase;
use swapdesk_platform::SecretStore;
use swapdesk_update::manager::{MANIFEST_NAME, SIGNATURE_NAME, Setup, UpdateManager};
use swapdesk_update::manifest::{public_key, sign};
use swapdesk_update::{Apply, Channel, FileEntry, Manifest, Version};

use super::*;

const BASE: &str = "https://github.com/devBertini/swapdesk/releases";
const PACKAGE: &[u8] = b"the package bytes";

/// A fresh Ed25519 key as PKCS#8, made for this run. The host has no direct way to make one: the trust store's
/// identity record carries it (format 1: a u16 format, the PKCS#8's length as a varint, the PKCS#8).
fn signing_key() -> Vec<u8> {
    let store = MemorySecretStore::new();
    swapdesk_trust::Identity::load_or_create(&store, 0).expect("an identity");
    let record = store.load(swapdesk_trust::identity::IDENTITY_NAME).unwrap().expect("its record");
    let mut r = swapdesk_proto::codec::Reader::new(&record);
    assert_eq!(r.u16().unwrap(), swapdesk_trust::identity::FORMAT, "the identity record's format");
    let len = r.varint().unwrap() as usize;
    r.take(len).unwrap().to_vec()
}

/// A release server that offers 1.1.1; while its package is sent, a session is accepted on this machine.
struct Server {
    answers: HashMap<String, Vec<u8>>,
    package_url: String,
    live: Arc<AtomicUsize>,
    manifests: AtomicUsize,
}

impl swapdesk_update::http::HttpGet for Server {
    fn get(
        &self,
        url: &str,
        _max: u64,
        sink: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u64),
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), swapdesk_update::http::HttpError> {
        let Some(body) = self.answers.get(url) else {
            return Err(swapdesk_update::http::HttpError::Status(404));
        };
        if url == self.package_url {
            self.live.store(1, Ordering::SeqCst);
        }
        if url.ends_with(MANIFEST_NAME) {
            self.manifests.fetch_add(1, Ordering::SeqCst);
        }
        sink.write_all(body).unwrap();
        progress(body.len() as u64);
        Ok(())
    }
}

#[derive(Default)]
struct Folder(Mutex<HashMap<String, Arc<Mutex<Vec<u8>>>>>);

struct FileOf(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for FileOf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl swapdesk_update::manager::UpdateFolder for Folder {
    fn create(&self, name: &str) -> std::io::Result<Box<dyn std::io::Write + Send>> {
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

    fn free_bytes(&self) -> Option<u64> {
        None
    }
}

/// Review of 22/11/2026, G7: the hand-over to the applier was decided by the count read when the install was asked.
/// It is decided by the count read when the package is ready, unless the person agreed to end the sessions.
#[test]
fn the_package_goes_to_the_applier_only_with_no_session_live_now_or_their_end_agreed() {
    use super::super::update::may_hand_over;
    use swapdesk_update::manager::InstallAsk;
    let asked = |live_sessions, end_sessions| InstallAsk { from_console: true, live_sessions, end_sessions };
    assert!(may_hand_over(asked(0, false), 0), "nothing live then or now");
    assert!(!may_hand_over(asked(0, false), 1), "a session accepted during the download");
    assert!(may_hand_over(asked(1, true), 2), "\"Encerrar e atualizar\" was pressed");
    assert!(may_hand_over(asked(1, true), 0));
}

fn phase(ui: &mut Client) -> UpdatePhase {
    let ReplyBody::Update(view) = ui.call(UiOp::UpdateGet).unwrap() else { panic!("not the update view") };
    view.phase
}

fn offered(p: &UpdatePhase) -> bool {
    matches!(p, UpdatePhase::Offered { version, manual: false } if version == "1.1.1")
}

/// Review of 22/11/2026, G7: the live sessions were counted once, when "Atualizar agora" was pressed. A session
/// accepted during the download was then cut by the applier without the "Encerrar e atualizar" question. The count
/// is read again when the package is ready: with a live session nobody agreed to end, the package is not handed
/// over, nothing is told to restart, and the version is offered again, so the next "Atualizar agora" asks.
#[test]
fn a_session_accepted_during_the_download_is_not_cut_without_the_question() {
    let pkcs8 = signing_key();
    let entry = FileEntry {
        target: "windows-x64".into(),
        name: "Swapdesk-windows-x64-1.1.1.sdpkg".into(),
        size: PACKAGE.len() as u64,
        sha256: swapdesk_update::sha256(PACKAGE),
    };
    let manifest = Manifest {
        version: Version::parse("1.1.1").unwrap(),
        channel: Channel::Stable,
        apply: Apply::Auto,
        files: vec![entry.clone()],
    };
    let text = manifest.to_text();
    let package_url = format!("{BASE}/download/v1.1.1/{}", entry.name);
    let live = Arc::new(AtomicUsize::new(0));
    let server = Arc::new(Server {
        answers: HashMap::from([
            (format!("{BASE}/latest/download/{MANIFEST_NAME}"), text.clone().into_bytes()),
            (format!("{BASE}/latest/download/{SIGNATURE_NAME}"), sign(text.as_bytes(), &pkcs8).unwrap().into_bytes()),
            (package_url.clone(), PACKAGE.to_vec()),
        ]),
        package_url,
        live: Arc::clone(&live),
        manifests: AtomicUsize::new(0),
    });
    let setup = Setup {
        running: Version::parse("1.1.0").unwrap(),
        target: "windows-x64".into(),
        base: BASE.into(),
        keys: vec![public_key(&pkcs8).unwrap()],
        dev: false,
        first_delay_ms: 60_000,
    };
    let manager = UpdateManager::new(setup, server.clone(), Arc::new(super::NoSaved), Arc::new(Folder::default()), 0);
    let handed = Arc::new(AtomicUsize::new(0));
    let hooks = super::super::update::UpdaterHooks {
        now_ms: Box::new(|| 0),
        jitter_ms: Box::new(|| 0),
        live_sessions: {
            let live = Arc::clone(&live);
            Box::new(move || live.load(Ordering::SeqCst))
        },
        ready: {
            let handed = Arc::clone(&handed);
            Box::new(move |_, _| {
                handed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        },
    };
    let m = Machine::new("sala");
    m.hub.set_updater(super::super::update::Updater::new(manager, hooks));
    let mut ui = m.client(info(false));

    assert!(matches!(ui.call(UiOp::UpdateCheckNow), Ok(ReplyBody::Done)));
    let until = Instant::now() + Duration::from_secs(10);
    while !offered(&phase(&mut ui)) {
        assert!(Instant::now() < until, "1.1.1 was never offered");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Nothing is live when "Atualizar agora" is pressed: no question, the download starts.
    assert!(matches!(
        ui.call(UiOp::UpdateInstall { version: "1.1.1".into(), end_sessions: false }),
        Ok(ReplyBody::Done)
    ));
    // The download ends with one session live. Wait for the hand-over, or for the version to be offered again by a
    // second check.
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if handed.load(Ordering::SeqCst) > 0 {
            break;
        }
        if server.manifests.load(Ordering::SeqCst) >= 2 && offered(&phase(&mut ui)) {
            break;
        }
        assert!(Instant::now() < until, "the install neither ended nor was offered again: {:?}", phase(&mut ui));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(handed.load(Ordering::SeqCst), 0, "the package was handed to the applier with a session nobody ended");
    assert!(
        ui.nothing_within(Duration::from_millis(200), |n| matches!(n, Ipc::UiNotify(Notify::UpdateRestarting))),
        "the interfaces were told to restart"
    );
    // Offered again: "Atualizar agora" now asks "Encerrar e atualizar" (Busy), and the confirmed one goes on.
    assert_eq!(
        ui.call(UiOp::UpdateInstall { version: "1.1.1".into(), end_sessions: false }).unwrap_err(),
        ApiCode::Busy
    );
    assert!(matches!(
        ui.call(UiOp::UpdateInstall { version: "1.1.1".into(), end_sessions: true }),
        Ok(ReplyBody::Done)
    ));
    let until = Instant::now() + Duration::from_secs(10);
    while handed.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < until, "the confirmed install was never handed to the applier");
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(ui);
    m.finish();
}
