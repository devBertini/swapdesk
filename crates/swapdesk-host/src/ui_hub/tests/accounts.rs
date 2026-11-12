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
    /// What the account announces on the network (under its filter).
    presence: Arc<FakePresence>,
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
        let fake = FakePresence::new();
        let presence: Arc<dyn super::super::Presence> =
            Arc::new(super::super::presence::FilteredPresence::new(fake.clone(), own));
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
        let core = match ServiceCore::start(cfg) {
            Ok(c) => Arc::new(c),
            // As the service does: a held port names its holder and the machine moves the account.
            Err(crate::service::ServiceError::Bind { addr, .. }) => {
                return Err(format!("{}{} by the test that holds it", crate::accounts::PORT_HELD, addr.port()));
            }
            Err(e) => return Err(e.to_string()),
        };
        hub.bind_core(&core);
        hub.set_console_session(Some(1));
        reg.lock().unwrap().insert(seat.sid.clone(), Built { identity: Arc::clone(&identity), trust, presence: fake });
        Ok(Account { seat: seat.clone(), device_id: identity.device_id(), hub, core })
    });
    let accounts = Accounts::new(
        base,
        Arc::new(MemorySecretStore::new()),
        factory,
        Box::new(|| 1_700_000_000_000),
        Arc::new(LOG.with(Clone::clone)),
    );
    (accounts, built)
}

thread_local! {
    /// The trace of the machine a test builds on this thread.
    static LOG: LogCapture = LogCapture::new();
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

#[test]
fn a_held_port_moves_the_account_to_the_next_free_slot_and_names_the_holder() {
    let base = free_slots();
    // Another program holds the session port of slot 0.
    let holder = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, base)).unwrap();
    let (accounts, _built) = machine(base);
    let a = accounts.account(A).unwrap();
    assert_eq!(a.seat.slot, 1, "moved to the next free slot");
    assert!(a.core.local_addrs().iter().any(|s| s.port() == base + 2));
    assert_eq!(accounts.seat_for(A).unwrap().slot, 1, "kept in accounts.bin");
    let lines = LOG.with(|l| l.lines());
    assert!(
        lines.iter().any(|l| l.text.contains("slot 0 cannot be used") && l.text.contains("by the test that holds it")),
        "the holder is named: {lines:?}"
    );
    // The next account takes the slot that is free, not the held one.
    let b = accounts.account(B).unwrap();
    assert_eq!(b.seat.slot, 2);
    drop(holder);
}

/// ACCOUNTS.md 5.1: the helpers follow the core of the account at the console; at the sign-in screen, the one that
/// hosts; nobody's helpers serve a user who is not an account of the machine.
#[test]
fn the_helpers_follow_the_console_owners_core() {
    let base = free_slots();
    let (accounts, _built) = machine(base);
    let (a, b) = (accounts.account(A).unwrap(), accounts.account(B).unwrap());
    a.hub.set_account(A);
    b.hub.set_account(B);
    let core_of = |sid: Option<&str>| accounts.host_core(sid).map(|c| Arc::as_ptr(&c));
    assert_eq!(core_of(Some(A)), Some(Arc::as_ptr(&a.core)));
    assert_eq!(core_of(Some(B)), Some(Arc::as_ptr(&b.core)));
    assert_eq!(core_of(Some("S-1-5-21-9-9-9-1500")), None, "a user who has no account here: no helpers");
    assert_eq!(core_of(None), None, "the sign-in screen with nobody hosting: no helpers yet");
}

/// ACCOUNTS.md 5.1 (review of S4): what an account's sessions are told at each console owner. The sign-in screen is
/// `Locked`, so a session that entered with a prompt pauses there; a user who cannot be read is another account.
#[test]
fn each_console_owner_gives_the_sessions_their_desktop_and_an_unreadable_user_is_away() {
    let base = free_slots();
    let (accounts, _built) = machine(base);
    let a = accounts.account(A).unwrap();
    a.hub.set_account(A);
    let at = |user: Option<&str>| {
        a.hub.set_console_user(user.map(str::to_string));
        a.hub.hub.effective_desktop()
    };
    assert_eq!(at(Some(A)), Desktop::Default);
    assert_eq!(at(None), Desktop::Locked, "the sign-in screen");
    assert_eq!(at(Some(B)), Desktop::AccountAway);
    assert_eq!(at(Some(crate::service::UNREADABLE_USER)), Desktop::AccountAway, "fails closed");
    a.hub.set_desktop(Desktop::Secure);
    assert_eq!(at(None), Desktop::Secure, "a secure desktop stays what it is");
}

/// F-UNAT-01: what the media helper says about the input desktop (a UAC prompt no session event announces) reaches
/// the sessions as the effective desktop, and another account at the console still wins.
#[test]
fn the_helpers_report_of_a_protected_desktop_is_combined_with_the_console_owner() {
    let base = free_slots();
    let (accounts, _built) = machine(base);
    let a = accounts.account(A).unwrap();
    a.hub.set_account(A);
    a.hub.set_console_user(Some(A.to_string()));
    assert_eq!(a.hub.hub.effective_desktop(), Desktop::Default);
    a.hub.set_helper_desktop(Desktop::Secure);
    assert_eq!(a.hub.hub.effective_desktop(), Desktop::Secure, "a UAC prompt");
    a.hub.set_desktop(Desktop::Locked);
    assert_eq!(a.hub.hub.effective_desktop(), Desktop::Locked, "the lock the session knows keeps its name");
    a.hub.set_desktop(Desktop::Default);
    assert_eq!(a.hub.hub.effective_desktop(), Desktop::Secure, "unlocked, the helper's report still holds");
    a.hub.set_console_user(Some(B.to_string()));
    assert_eq!(a.hub.hub.effective_desktop(), Desktop::AccountAway);
    a.hub.set_console_user(Some(A.to_string()));
    a.hub.set_helper_desktop(Desktop::Default);
    assert_eq!(a.hub.hub.effective_desktop(), Desktop::Default, "back on the user's desktop");
}

/// Review of S3: two first calls for one account at once build it once, at its own slot.
#[test]
fn an_account_asked_for_twice_at_once_is_built_once() {
    let base = free_slots();
    let (accounts, _built) = machine(base);
    let accounts = Arc::new(accounts);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let both: Vec<_> = (0..2)
        .map(|_| {
            let (accounts, barrier) = (Arc::clone(&accounts), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                accounts.account(A).unwrap()
            })
        })
        .collect();
    let got: Vec<Arc<Account>> = both.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(Arc::ptr_eq(&got[0], &got[1]), "one account");
    assert_eq!(accounts.seat_for(A).unwrap().slot, 0, "not moved by its own twin");
    assert_eq!(accounts.all().len(), 1);
}

/// D55: an account on a computer a 1.0 install left data on shows the one-time note while it has no pairing; once it
/// pairs, the note is gone. A computer with no 1.0 data never shows it.
#[test]
fn the_note_after_the_update_shows_only_while_the_account_has_no_pairing() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let a = accounts.account(A).unwrap();
    let note = || a.hub.hub.this_device().legacy_note;
    assert!(!note(), "no 1.0 data");
    a.hub.set_had_1_0(true);
    assert!(note());
    let viewer = Identity::generate(0).unwrap();
    pin_viewer(&built.lock().unwrap()[A], &viewer);
    assert!(!note(), "paired");
}

fn settings(name: &str, receive: bool) -> UiOp {
    UiOp::SetSettings(swapdesk_ipc::SettingsUpdate {
        name: name.into(),
        service_enabled: receive,
        lock_on_end: true,
        keep_awake: false,
    })
}

/// ACCOUNTS.md 7.2: each account's settings are its own.
#[test]
fn settings_of_one_account_do_not_change_anothers() {
    let base = free_slots();
    let (accounts, _built) = machine(base);
    let (a, b) = (accounts.account(A).unwrap(), accounts.account(B).unwrap());
    let mut ui_a = Client::connect(&a.hub, ConnInfo { session_id: 1, user: user(A), admin: false, can_elevate: false });
    let mut ui_b = Client::connect(&b.hub, ConnInfo { session_id: 1, user: user(B), admin: false, can_elevate: false });
    let ReplyBody::Settings(before_b) = ui_b.call(UiOp::GetSettings).unwrap() else { panic!() };
    assert!(ui_a.call(settings("PC · ana", false)).is_ok());
    let ReplyBody::Settings(after_a) = ui_a.call(UiOp::GetSettings).unwrap() else { panic!() };
    let ReplyBody::Settings(after_b) = ui_b.call(UiOp::GetSettings).unwrap() else { panic!() };
    assert_eq!((after_a.name.as_str(), after_a.service_enabled, after_a.lock_on_end), ("PC · ana", false, true));
    assert_eq!(after_b, before_b, "B is untouched");
    drop((ui_a, ui_b));
}

/// ACCOUNTS.md 7.2 (D57): "Receber conexões" off closes the account's endpoint and withdraws its record; on again,
/// both come back. Pairing mode opened by the user still listens.
#[test]
fn receive_off_closes_the_accounts_endpoint_and_withdraws_its_record() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let a = accounts.account(A).unwrap();
    let ka = built.lock().unwrap()[A].clone();
    pin_viewer(&ka, &Identity::generate(0).unwrap());
    a.hub.hub.update_receiving();
    assert!(a.core.receiving() && ka.presence.announced() == Some(true), "paired: it receives");
    let mut ui = Client::connect(&a.hub, ConnInfo { session_id: 1, user: user(A), admin: false, can_elevate: false });
    assert!(ui.call(settings("PC · ana", false)).is_ok());
    assert!(!a.core.receiving(), "the endpoint is closed");
    assert_eq!(ka.presence.announced(), Some(false), "the record is withdrawn");
    assert!(ui.call(settings("PC · ana", true)).is_ok());
    assert!(a.core.receiving() && ka.presence.announced() == Some(true));
    drop(ui);
}

/// ACCOUNTS.md 12.1 (S9, OS-free): a Mac app whose stored pair is held by another account's app takes the next free
/// pair; stored there, it starts there next time.
#[test]
fn a_busy_port_pair_moves_to_the_next_slot_and_stays_there() {
    use crate::accounts::{MAC_SLOTS, first_free_pair};
    let held = [47_820u16, 47_821];
    let free = |p: u16| !held.contains(&p);
    assert_eq!(first_free_pair((47_820, 47_821), MAC_SLOTS, free), Some((47_822, 47_823)));
    // Next start, from the pair it stored: it stays, even once the first pair is free again.
    assert_eq!(first_free_pair((47_822, 47_823), MAC_SLOTS, |_| true), Some((47_822, 47_823)));
    // Only the pairing port held: the pair moves too (both ports go together).
    assert_eq!(first_free_pair((47_820, 47_821), MAC_SLOTS, |p| p != 47_821), Some((47_822, 47_823)));
    assert_eq!(first_free_pair((47_820, 47_821), 3, |_| false), None, "nothing free within reach");
}

/// A viewer machine paired with `who` (both sides pinned), and the address of `port` on loopback.
fn viewer_of(who: &Built) -> Machine {
    let v = Machine::new("viewer-pc");
    pin_viewer(who, &v.identity);
    v.trust
        .update(|s| {
            s.devices.push(PairedDevice::new_paired(
                *who.identity.spki(),
                "host".into(),
                Os::Windows,
                "11".into(),
                1,
                None,
            ));
        })
        .unwrap();
    v
}

fn lo(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

/// GRANTS-SYNC.md 8 S9: a device paired with account A that dials B's port fails the handshake, and B's store is
/// untouched; at A's port it is answered.
#[test]
fn a_sync_reaches_only_the_account_whose_port_it_dials() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let (a, b) = (accounts.account(A).unwrap(), accounts.account(B).unwrap());
    let ka = built.lock().unwrap()[A].clone();
    let kb = built.lock().unwrap()[B].clone();
    let v = viewer_of(&ka);
    a.hub.hub.update_receiving();
    let sync = |port| v.core.runtime().block_on(v.core.sync_grants_async(lo(port), ka.identity.device_id()));
    assert!(sync(a.seat.session_port).is_ok(), "A answers its own device");
    assert!(sync(b.seat.session_port).is_err(), "B presents another key");
    assert!(kb.trust.read(|s| s.devices.is_empty()), "B's store is untouched");
    v.finish();
}

/// GRANTS-SYNC.md 8 S9: the console rule (ACCOUNTS.md 5.1) is for sessions; a sync to an account whose user is not
/// at the console is answered, without a prompt.
#[test]
fn a_sync_to_an_account_not_at_the_console_is_answered_without_a_prompt() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let a = accounts.account(A).unwrap();
    let ka = built.lock().unwrap()[A].clone();
    a.hub.set_account(A);
    a.hub.set_console_user(Some(B.into()));
    let v = viewer_of(&ka);
    a.hub.hub.update_receiving();
    let got = v.core.runtime().block_on(v.core.sync_grants_async(lo(a.seat.session_port), ka.identity.device_id()));
    assert_eq!(got.expect("answered").grants, PermissionSet::CONTROL, "what A stored for the device");
    v.finish();
}

/// GRANTS-SYNC.md 8 S9: a device out of discovery is dialled at its last address and at the port it was last seen
/// on (`peers.bin`), so an account on a slot other than 0 is reached.
#[test]
fn a_device_out_of_discovery_is_synced_at_its_port_in_peers_bin() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let a = accounts.account(A).unwrap();
    let ka = built.lock().unwrap()[A].clone();
    let peer = Identity::generate(0).unwrap();
    ka.trust
        .update(|s| {
            s.devices.push(PairedDevice::new_paired(
                *peer.spki(),
                "peer".into(),
                Os::MacOs,
                "15".into(),
                1,
                Some([192, 168, 0, 21]),
            ));
        })
        .unwrap();
    let id = peer.device_id();
    assert_eq!(a.hub.hub.address_of(&id).map(|s| s.port()), Some(crate::service::DEFAULT_SESSION_PORT));
    a.hub.hub.note_peer_port(&id, 47_824);
    assert_eq!(a.hub.hub.address_of(&id), Some(SocketAddrV4::new(Ipv4Addr::new(192, 168, 0, 21), 47_824)));
}

/// GRANTS-SYNC.md 8 S9: what account A changes for its device is pushed to that device only; B's device hears
/// nothing.
#[test]
fn a_grant_change_in_one_account_is_pushed_only_to_that_accounts_devices() {
    let base = free_slots();
    let (accounts, built) = machine(base);
    let (a, b) = (accounts.account(A).unwrap(), accounts.account(B).unwrap());
    let ka = built.lock().unwrap()[A].clone();
    let kb = built.lock().unwrap()[B].clone();
    let (va, vb) = (viewer_of(&ka), viewer_of(&kb));
    for (k, v) in [(&ka, &va), (&kb, &vb)] {
        let port = v.core.local_addrs()[0].port();
        k.trust
            .update(|s| {
                let id = v.identity.device_id();
                if let Some(d) = s.devices.iter_mut().find(|d| d.device_id == id) {
                    d.last_addrs = vec![[127, 0, 0, 1]];
                }
            })
            .unwrap();
        v.hub.hub.update_receiving();
        let hub = if std::ptr::eq(k, &ka) { &a.hub } else { &b.hub };
        hub.hub.note_peer_port(&v.identity.device_id(), port);
        // Out of discovery: the minor it said over an authenticated exchange (a sync is never sent to a 1.0 device).
        hub.host_ui().event(swapdesk_session::HostEvent::PeerMinor { device: v.identity.device_id(), minor: 1 });
    }
    let mut ui_a = Client::connect(&a.hub, ConnInfo { session_id: 1, user: user(A), admin: false, can_elevate: false });
    let now = PermissionSet::VIEW;
    let op = UiOp::SetGrants { device_id: device_id_hex(&va.identity.device_id()), grants: now.bits() };
    assert!(ui_a.call(op).is_ok(), "narrowing needs no administrator");
    let there = |v: &Machine| v.trust.read(|s| s.devices[0].grants_there);
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while there(&va) != Some(now) && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(there(&va), Some(now), "A's device learns the change");
    assert_eq!(there(&vb), None, "B's device hears nothing");
    drop(ui_a);
    va.finish();
    vb.finish();
}
