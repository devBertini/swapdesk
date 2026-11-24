//! The phone's service as the app assembles it (`PhoneService`), against a computer's hub in the same process over
//! loopback: the phone pairs as the initiator through its own backend, as the interface will, and never listens.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use swapdesk_android::phone_service::{PhoneParams, PhoneService, StartError};
use swapdesk_crossdrive::Pump;
use swapdesk_fakes::{FakeNetInterfaces, LogCapture, MemorySecretStore, MonotonicClock};
use swapdesk_host::pipe::duplex;
use swapdesk_host::service::{ServiceConfig, ServiceCore, ServiceDeps};
use swapdesk_host::ui_hub::{ConnInfo, DevOptions, HubParams, MachineSettings, NoHooks, NoPresence, UiHub};
use swapdesk_ipc::{FrameReader, FrameWriter, Handshake};
use swapdesk_platform::SecretStore;
use swapdesk_proto::{Os, Wire};
use swapdesk_trust::{Identity, MemoryAudit, PeerSystems, SYSTEMS_NAME, SystemWallClock, TrustHandle, UserRef};
use swapdesk_ui::service_backend::ServiceBackend;
use swapdesk_ui::{Command, PairingState, Reply, UiEvent, UiWaker};

struct NoWake;

impl UiWaker for NoWake {
    fn wake(&self) {}
}

/// A session port with the pairing port next to it (a typed address implies it), below the system's dynamic range.
fn free_port_pair() -> u16 {
    let start = 43_000 + (std::process::id() % 2_000) as u16 * 2;
    for p in (start..48_000).step_by(2) {
        let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, p));
        let b = UdpSocket::bind((Ipv4Addr::LOCALHOST, p + 1));
        if a.is_ok() && b.is_ok() {
            return p;
        }
    }
    panic!("no two free ports in a row");
}

/// A computer: a core and a hub that listen on loopback, with one interface connected.
struct Computer {
    core: Arc<ServiceCore>,
    hub: UiHub,
    trust: Arc<TrustHandle>,
    secrets: Arc<MemorySecretStore>,
    ui: Option<ServiceBackend>,
    serve: Option<JoinHandle<()>>,
}

impl Computer {
    fn new(name: &str, port: u16) -> Self {
        let identity = Arc::new(Identity::generate(0).expect("identity"));
        let secrets = Arc::new(MemorySecretStore::new());
        let trust = TrustHandle::open(secrets.clone()).expect("trust");
        let trace = Arc::new(LogCapture::new());
        let interfaces = Arc::new(FakeNetInterfaces::new(Vec::new()));
        let clock = Arc::new(MonotonicClock::new());
        let hub = UiHub::new(HubParams {
            os: Os::Windows,
            os_version: "11".into(),
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            secrets: secrets.clone(),
            settings: MachineSettings::new(name),
            interfaces: interfaces.clone(),
            presence: Arc::new(NoPresence),
            audit: Arc::new(MemoryAudit::new()),
            clock: clock.clone(),
            wall: Arc::new(SystemWallClock),
            trace: trace.clone(),
            hooks: Arc::new(NoHooks),
            dev: Some(DevOptions { pair_listen: vec![SocketAddrV4::new(Ipv4Addr::LOCALHOST, port + 1)] }),
        });
        let deps = ServiceDeps { identity, trust: Arc::clone(&trust), clock, trace, ui: hub.host_ui(), interfaces };
        let mut cfg = ServiceConfig::new(name, Os::Windows, "11", deps);
        cfg.session_port = port;
        let cfg = cfg.dev_loopback(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let core = Arc::new(ServiceCore::start(cfg).expect("core starts"));
        hub.bind_core(&core);
        hub.set_console_session(Some(1));

        let (client_end, server_end) = duplex();
        let serving = hub.clone();
        let serve = std::thread::spawn(move || {
            let (mut reader, mut writer) = (FrameReader::new(server_end.reader), FrameWriter::new(server_end.writer));
            Handshake::server(&mut reader, &mut writer, 1).expect("server handshake");
            let info = ConnInfo {
                session_id: 1,
                user: UserRef { id: "S-1-5-21-1".into(), name: "tester".into() },
                admin: false,
                can_elevate: false,
            };
            let _ = serving.serve(reader, writer, info);
        });
        let ui =
            ServiceBackend::start(client_end.reader, client_end.writer, Arc::new(NoWake), 4321).expect("handshake");
        Self { core, hub, trust, secrets, ui: Some(ui), serve: Some(serve) }
    }

    fn shutdown(mut self) {
        drop(self.ui.take());
        self.hub.shutdown();
        if let Some(t) = self.serve.take() {
            let _ = t.join();
        }
        if let Ok(core) = Arc::try_unwrap(self.core) {
            core.shutdown();
        }
    }
}

/// A folder of its own for the phone's activity log, removed at the end.
struct Folder(PathBuf);

impl Folder {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("swapdesk-phone-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn phone(folder: &Folder, secrets: Arc<MemorySecretStore>) -> PhoneService {
    PhoneService::start(
        PhoneParams {
            default_name: "celular".into(),
            os_version: "16".into(),
            secrets,
            dir: folder.0.clone(),
            interfaces: Arc::new(FakeNetInterfaces::new(Vec::new())),
            clock: Arc::new(MonotonicClock::new()),
            trace: Arc::new(LogCapture::new()),
            discovery: false,
            dev_loopback: true,
        },
        Arc::new(NoWake),
        None,
    )
    .expect("the phone's service starts")
}

const T: Duration = Duration::from_secs(30);

/// docs/ANDROID.md 4 and 5, F-AND-03: the phone's service, driven through its backend as the interface drives it,
/// adds a computer by its address and pairs with it as the initiator. Both sides show one code; after both confirm,
/// each pins the other, the computer knows the phone as an Android device (kept in `systems.bin`), and the phone
/// listened on nothing at any moment. Started again, the phone still holds its key and its pairing.
#[test]
fn the_phone_pairs_with_a_computer_by_address_and_never_listens() {
    let port = free_port_pair();
    let computer = Computer::new("sala", port);
    let folder = Folder::new("pair");
    let phone_secrets = Arc::new(MemorySecretStore::new());
    let mut phone_service = phone(&folder, phone_secrets.clone());
    assert!(phone_service.listening_on().is_empty(), "before: {:?}", phone_service.listening_on());

    let phone_ui = phone_service.backend();
    let (mut cp, mut pp) = (Pump::new(computer.ui.as_ref().unwrap()), Pump::new(&*phone_ui));
    let Reply::PairingId(on_computer) = cp.ask(Command::PairOpen { device_id: None }, T).expect("computer open") else {
        panic!("a pairing id")
    };
    let Reply::Device(typed) =
        pp.ask(Command::AddByAddress { ipv4: format!("127.0.0.1:{port}") }, T).expect("add by address")
    else {
        panic!("a device")
    };
    // docs/ANDROID.md 5 (a lost phone): until Android said the phone has a secure screen lock, the service enters
    // no pairing, whoever asks.
    let refused = pp.ask(Command::PairOpen { device_id: None }, T).expect_err("no screen lock is known yet");
    assert!(refused.contains("NeedsScreenLock"), "{refused}");
    assert!(!phone_service.device_secure());
    phone_service.set_device_secure(true);
    let Reply::PairingId(on_phone) = pp.ask(Command::PairOpen { device_id: None }, T).expect("phone open") else {
        panic!("a pairing id")
    };
    assert!(phone_service.listening_on().is_empty(), "with its pairing mode open");
    pp.ask(Command::PairStart { pairing_id: on_phone, device_id: typed.id }, T).expect("start");
    let code = |p: &mut Pump<'_>, id: u32| {
        p.wait("the code", T, |e| match e {
            UiEvent::Pairing { pairing_id, state: PairingState::ShowCode { code, peer_name } } if *pairing_id == id => {
                Some((code.clone(), peer_name.clone()))
            }
            _ => None,
        })
        .expect("the code is shown")
    };
    let (code_c, peer_c) = code(&mut cp, on_computer);
    let (code_p, peer_p) = code(&mut pp, on_phone);
    assert_eq!(code_c, code_p, "one code on both screens");
    assert_eq!((peer_c.as_str(), peer_p.as_str()), ("celular", "sala"));
    assert!(phone_service.listening_on().is_empty(), "during the ceremony");
    cp.ask(Command::PairConfirm { pairing_id: on_computer, matches: true }, T).expect("computer confirm");
    pp.ask(Command::PairConfirm { pairing_id: on_phone, matches: true }, T).expect("phone confirm");
    let done = |p: &mut Pump<'_>, id: u32| {
        p.wait("the pairing to finish", T, |e| match e {
            UiEvent::Pairing { pairing_id, state: PairingState::Done { device_id } } if *pairing_id == id => {
                Some(device_id.clone())
            }
            _ => None,
        })
        .expect("the pairing is done")
    };
    let (_phone_on_computer, _computer_on_phone) = (done(&mut cp, on_computer), done(&mut pp, on_phone));

    let phone_id = phone_service.identity().device_id();
    assert_eq!(
        computer.trust.read(|s| s.find_by_id(&phone_id).map(|d| (d.os, d.name.clone()))),
        Some((Os::Android, "celular".into()))
    );
    let side: PeerSystems = Wire::decode(&computer.secrets.load(SYSTEMS_NAME).unwrap().expect("systems.bin")).unwrap();
    assert_eq!(side.entries, vec![(phone_id, Os::Android.tag())]);
    assert_eq!(
        phone_service.trust().read(|s| s.devices.iter().map(|d| (d.os, d.name.clone())).collect::<Vec<_>>()),
        vec![(Os::Windows, "sala".into())]
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(phone_service.listening_on().is_empty(), "paired: {:?}", phone_service.listening_on());

    // The app is closed and opened again: the same key, the same pairing, and still no port.
    drop(pp);
    drop(phone_ui);
    phone_service.shutdown();
    drop(phone_service);
    let again = phone(&folder, phone_secrets);
    assert_eq!(again.identity().device_id(), phone_id, "the device key is kept");
    assert_eq!(again.trust().read(|s| s.devices.len()), 1);
    assert!(again.listening_on().is_empty(), "started again with a pairing");
    drop(again);
    drop(cp);
    computer.shutdown();
}

/// A secret store that answers every read with an error of the system (a key store that did not answer).
struct Unanswering;

impl SecretStore for Unanswering {
    fn load(&self, _: &str) -> swapdesk_platform::Result<Option<Vec<u8>>> {
        Err(swapdesk_platform::PlatformError {
            kind: swapdesk_platform::ErrorKind::Os,
            op: "load",
            os_code: None,
            detail: "the store did not answer".into(),
        })
    }
    fn store(&self, _: &str, _: &[u8]) -> swapdesk_platform::Result<()> {
        Ok(())
    }
    fn delete(&self, _: &str) -> swapdesk_platform::Result<()> {
        Ok(())
    }
}

/// What the app stored and cannot read again stops the start as unreadable (the screen then says what the way out
/// costs, and the app erases nothing); a store that did not answer is a start that failed this time, and nothing
/// says the next one will.
#[test]
fn a_stored_record_that_does_not_read_stops_the_start_as_unreadable() {
    let params = |folder: &Folder, secrets: Arc<dyn SecretStore>| PhoneParams {
        default_name: "celular".into(),
        os_version: "16".into(),
        secrets,
        dir: folder.0.clone(),
        interfaces: Arc::new(FakeNetInterfaces::new(Vec::new())),
        clock: Arc::new(MonotonicClock::new()),
        trace: Arc::new(LogCapture::new()),
        discovery: false,
        dev_loopback: true,
    };
    let folder = Folder::new("unreadable");
    let secrets = Arc::new(MemorySecretStore::new());
    secrets.store("identity.bin", &[7, 7, 7]).expect("a record that is no device key");
    let stopped = PhoneService::start(params(&folder, secrets.clone()), Arc::new(NoWake), None).err();
    assert!(matches!(stopped, Some(StartError::Unreadable(_))), "{stopped:?}");
    assert_eq!(secrets.load("identity.bin").unwrap().as_deref(), Some(&[7u8, 7, 7][..]), "nothing was replaced");

    let folder = Folder::new("unanswering");
    let stopped = PhoneService::start(params(&folder, Arc::new(Unanswering)), Arc::new(NoWake), None).err();
    assert!(matches!(stopped, Some(StartError::Failed(_))), "{stopped:?}");
}
