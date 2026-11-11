//! ACCOUNTS.md 15 S3: one identity, trust store, hub and core per account, each on its own port slot.

use std::collections::HashMap;

use crate::accounts::{Account, AccountSeat, Accounts, slot_ports};
use crate::service::MachineShared;

use super::*;

/// What a test keeps of each account the factory built.
#[derive(Clone)]
struct Built {
    identity: Arc<Identity>,
    trust: Arc<TrustHandle>,
}

type Registry = Arc<Mutex<HashMap<String, Built>>>;

/// A machine of several accounts in memory: each account listens on loopback at its slot's ports.
fn machine(base: u16) -> (Accounts, Registry) {
    let built: Registry = Arc::default();
    let reg = Arc::clone(&built);
    let factory = Box::new(move |seat: &AccountSeat, shared: &MachineShared, own: Arc<Mutex<Vec<DeviceId>>>| {
        let identity = Arc::new(Identity::generate(0).map_err(|e| e.to_string())?);
        let secrets = Arc::new(MemorySecretStore::new());
        let trust = TrustHandle::open(secrets.clone()).map_err(|e| e.to_string())?;
        let trace = LogCapture::new();
        let interfaces = Arc::new(FakeNetInterfaces::new(Vec::new()));
        let clock = Arc::new(MonotonicClock::new());
        let presence: Arc<dyn super::super::Presence> =
            Arc::new(super::super::presence::FilteredPresence::new(FakePresence::new(), own));
        let lo = |p: u16| SocketAddrV4::new(Ipv4Addr::LOCALHOST, p);
        let hub = UiHub::new(HubParams {
            os: Os::Windows,
            os_version: "11".into(),
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            secrets: secrets.clone(),
            settings: MachineSettings::new(&seat.sid),
            interfaces: interfaces.clone(),
            presence,
            audit: Arc::new(MemoryAudit::new()),
            clock: clock.clone(),
            wall: Arc::new(SystemWallClock),
            trace: Arc::new(trace.clone()),
            hooks: Arc::new(TestHooks::default()),
            dev: Some(DevOptions { pair_listen: vec![lo(seat.pair_port)] }),
        });
        let deps = ServiceDeps {
            identity: Arc::clone(&identity),
            trust: Arc::clone(&trust),
            clock,
            trace: Arc::new(trace),
            ui: hub.host_ui(),
            interfaces,
        };
        let mut cfg = ServiceConfig::new(&seat.sid, Os::Windows, "11", deps);
        cfg.session_port = seat.session_port;
        cfg.machine = Some(shared.clone());
        let cfg = cfg.dev_loopback(lo(seat.session_port));
        let core = Arc::new(ServiceCore::start(cfg).map_err(|e| e.to_string())?);
        hub.bind_core(&core);
        hub.set_console_session(Some(1));
        reg.lock().unwrap().insert(seat.sid.clone(), Built { identity: Arc::clone(&identity), trust });
        Ok(Account { seat: seat.clone(), device_id: identity.device_id(), hub, core })
    });
    let accounts = Accounts::new(
        base,
        Arc::new(MemorySecretStore::new()),
        factory,
        Box::new(|| 1_700_000_000_000),
        Arc::new(LogCapture::new()),
    );
    (accounts, built)
}

/// Four free ports in a row on loopback (two slots).
fn free_slots() -> u16 {
    let start = 43_000 + (std::process::id() % 1_500) as u16 * 4;
    for p in (start..50_000).step_by(4) {
        if (0..4).all(|i| std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, p + i)).is_ok()) {
            return p;
        }
    }
    panic!("no four free ports in a row");
}

const A: &str = "S-1-5-21-1-2-3-1001";
const B: &str = "S-1-5-21-1-2-3-1002";

/// A viewer paired with `who` (its key pinned there).
fn pin_viewer(who: &Built, viewer: &Identity) {
    who.trust
        .update(|s| {
            let mut d = PairedDevice::new_paired(*viewer.spki(), "viewer".into(), Os::MacOs, "15".into(), 1, None);
            d.grants_here = PermissionSet::CONTROL;
            s.devices.push(d);
        })
        .unwrap();
}

/// Dials `port` on loopback expecting the key of `host`, as the viewer `viewer`.
fn dial(port: u16, host: &Identity, viewer: &Identity) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let sock = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = swapdesk_trust::endpoint_from_socket(sock, None).map_err(|e| e.to_string())?;
        let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let conn =
            swapdesk_trust::connect_session(&endpoint, viewer, addr, *host.spki()).await.map_err(|e| e.to_string())?;
        // The host's gate checks the key again right after the handshake: a refused key closes the connection.
        tokio::time::sleep(Duration::from_millis(200)).await;
        match conn.close_reason() {
            Some(e) => Err(e.to_string()),
            None => Ok(()),
        }
    })
}

#[test]
fn each_account_listens_on_its_own_slot_and_presents_its_own_key() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let (a, b) = (accounts.account(A).unwrap(), accounts.account(B).unwrap());
    assert_eq!((a.seat.slot, b.seat.slot), (0, 1));
    assert_eq!((a.seat.session_port, a.seat.pair_port), slot_ports(base, 0));
    assert_eq!((b.seat.session_port, b.seat.pair_port), (base + 2, base + 3));
    assert_ne!(a.device_id, b.device_id, "an identity per account");
    let (ka, kb) = {
        let reg = built.lock().unwrap();
        (reg[A].clone(), reg[B].clone())
    };
    assert!(a.core.local_addrs().iter().any(|s| s.port() == base));
    assert!(b.core.local_addrs().iter().any(|s| s.port() == base + 2));
    // A viewer paired with each account reaches it on its own port, with its own key.
    let viewer = Identity::generate(0).unwrap();
    pin_viewer(&ka, &viewer);
    pin_viewer(&kb, &viewer);
    assert!(dial(base, &ka.identity, &viewer).is_ok(), "slot 0 presents the key of A");
    assert!(dial(base + 2, &kb.identity, &viewer).is_ok(), "slot 1 presents the key of B");
    assert!(dial(base, &kb.identity, &viewer).is_err(), "slot 0 is not B");
    // The seat is kept: asking again gives the same account; the index holds both.
    assert_eq!(accounts.account(A).unwrap().seat, a.seat);
    assert_eq!(accounts.all().len(), 2);
    assert_eq!(accounts.own_ids().lock().unwrap().len(), 2);
}

#[test]
fn a_key_pinned_by_one_account_fails_the_handshake_on_the_other() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let (_a, _b) = (accounts.account(A).unwrap(), accounts.account(B).unwrap());
    let (ka, kb) = {
        let reg = built.lock().unwrap();
        (reg[A].clone(), reg[B].clone())
    };
    let viewer = Identity::generate(0).unwrap();
    pin_viewer(&ka, &viewer);
    let r = dial(base, &ka.identity, &viewer);
    assert!(r.is_ok(), "A pinned it: {r:?}");
    assert!(dial(base + 2, &kb.identity, &viewer).is_err(), "B never pinned it: refused");
}

#[test]
fn a_ui_connection_reaches_only_its_own_accounts_data() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let (a, b) = (accounts.account(A).unwrap(), accounts.account(B).unwrap());
    let kb = built.lock().unwrap()[B].clone();
    let viewer = Identity::generate(0).unwrap();
    pin_viewer(&kb, &viewer);
    let b_device = device_id_hex(&viewer.device_id());
    let user_a = ConnInfo { session_id: 1, user: user(A), admin: false, can_elevate: false };
    let user_b = ConnInfo { session_id: 1, user: user(B), admin: false, can_elevate: false };
    let mut ui_a = Client::connect(&a.hub, user_a);
    let mut ui_b = Client::connect(&b.hub, user_b);
    let ReplyBody::Devices(list_a) = ui_a.call(UiOp::ListDevices).unwrap() else { panic!() };
    let ReplyBody::Devices(list_b) = ui_b.call(UiOp::ListDevices).unwrap() else { panic!() };
    assert!(list_a.iter().all(|d| d.id != b_device), "A does not see the pairing of B");
    assert!(list_b.iter().any(|d| d.id == b_device && d.paired));
    assert_eq!(
        ui_a.call(UiOp::SetGrants { device_id: b_device.clone(), grants: PermissionSet::VIEW.bits() }).unwrap_err(),
        ApiCode::NotFound,
        "A cannot change what B allows"
    );
    assert_eq!(ui_a.call(UiOp::ForgetDevice { device_id: b_device.clone() }).unwrap_err(), ApiCode::NotFound);
    assert!(kb.trust.read(|s| s.find_by_id(&viewer.device_id()).is_some()), "the pairing of B is intact");
    let ReplyBody::Activity(act_a) = ui_a.call(UiOp::ActivityList { before_ms: None, limit: 50 }).unwrap() else {
        panic!()
    };
    assert!(act_a.is_empty(), "A has an activity log of its own");
    drop((ui_a, ui_b));
}
