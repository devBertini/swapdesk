//! What the other side stored for a paired device (GRANTS-SYNC.md 3.4): kept as `grants_there`, written at most once a
//! second per device and only when it changed, pushed to the interfaces with `DevicesChanged`; in a session both
//! machines learn it from each other.

use super::extras::active_as;
use super::*;

const TRUST: &str = "trust.bin";

fn there_of(list: &[swapdesk_ipc::DeviceView], id: &str) -> Option<Option<u16>> {
    list.iter().find(|d| d.id == id).map(|d| d.grants_there)
}

fn listed_there(ui: &mut Client, id: &str) -> Option<u16> {
    let ReplyBody::Devices(list) = ui.call(UiOp::ListDevices).unwrap() else { panic!("a list") };
    there_of(&list, id).expect("the device is listed")
}

#[test]
fn a_peers_stored_grants_reach_the_device_view_and_devices_changed() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let mut ui = m.client(info(false));
    let id = other.hex();
    // The interface is registered (it answers) before the value arrives, so the push cannot go before it.
    assert_eq!(listed_there(&mut ui, &id), None);

    m.hub.hub.apply_peer_grants(other.id(), PermissionSet::VIEW.union(PermissionSet::FILES));
    let want = PermissionSet::VIEW.union(PermissionSet::FILES).bits();
    ui.notify("DevicesChanged with what the other side allows", |n| match n {
        Notify::DevicesChanged(l) if there_of(l, &id) == Some(Some(want)) => Some(()),
        _ => None,
    });
    assert_eq!(listed_there(&mut ui, &id), Some(want));
    assert_eq!(
        m.trust.read(|s| s.devices[0].grants_there),
        Some(PermissionSet::VIEW.union(PermissionSet::FILES)),
        "kept in the trust store"
    );
    assert_eq!(
        m.trust.read(|s| s.devices[0].grants_here),
        PermissionSet::CONTROL,
        "what this side allows is untouched"
    );

    // A value for a device that is not paired is dropped.
    let stranger = Machine::new("stranger");
    m.hub.hub.apply_peer_grants(stranger.id(), PermissionSet::VIEW);
    assert_eq!(m.trust.read(|s| s.devices.len()), 1);

    drop(ui);
    stranger.finish();
    m.finish();
    other.finish();
}

#[test]
fn an_unchanged_peer_value_writes_nothing() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let base = m.secrets.writes(TRUST);
    let hub = &m.hub.hub;

    hub.apply_peer_grants(other.id(), PermissionSet::VIEW);
    assert_eq!(m.secrets.writes(TRUST), base + 1, "a new value is written at once");
    hub.apply_peer_grants(other.id(), PermissionSet::VIEW);
    hub.apply_peer_grants(other.id(), PermissionSet::VIEW);
    assert_eq!(m.secrets.writes(TRUST), base + 1, "the same value again writes nothing");

    // Inside the second after a write, values wait; the latest is written once at its end.
    hub.apply_peer_grants(other.id(), PermissionSet::CONTROL);
    hub.apply_peer_grants(other.id(), PermissionSet::FILES);
    hub.apply_peer_grants(other.id(), PermissionSet::CONTROL.union(PermissionSet::AUDIO));
    assert_eq!(m.secrets.writes(TRUST), base + 1, "nothing written inside the gap");
    wait_for("the value that waited", || (m.secrets.writes(TRUST) == base + 2).then_some(()));
    assert_eq!(m.trust.read(|s| s.devices[0].grants_there), Some(PermissionSet::CONTROL.union(PermissionSet::AUDIO)));
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(m.secrets.writes(TRUST), base + 2, "one write for the burst");

    // A peer that repeats a value forever costs no write.
    for _ in 0..50 {
        hub.apply_peer_grants(other.id(), PermissionSet::CONTROL.union(PermissionSet::AUDIO));
    }
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(m.secrets.writes(TRUST), base + 2);

    m.finish();
    other.finish();
}

#[test]
fn the_device_view_of_a_never_reported_device_is_none() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let mut ui = m.client(info(false));
    assert_eq!(listed_there(&mut ui, &other.hex()), None, "never reported: the column shows a dash");
    drop(ui);
    m.finish();
    other.finish();
}

/// A host and a viewer of minor 1 in a session: each learns what the other stored for it.
fn session_of_minor_1(host_stores: PermissionSet, viewer_stores: PermissionSet) -> (Machine, Machine, Client, Client) {
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-pc");
    host.pin(&viewer, host_stores);
    viewer.pin(&host, viewer_stores);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    viewer.sees_minor(&host, false, 1);
    host.hub.set_console_session(Some(1));
    let mut host_ui = host.client(info(false));
    let mut viewer_ui = viewer.client(info(false));
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let incoming = host_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.clone()),
        _ => None,
    });
    host_ui
        .call(UiOp::AnswerIncoming { request_id: incoming.request_id, decision: Decision::Accept, grants: ALL.bits() })
        .expect("accepted");
    viewer_ui.connected(req);
    active_as(&mut host_ui, Role::Host, &[]);
    (host, viewer, host_ui, viewer_ui)
}

fn learns(ui: &mut Client, id: &str, want: PermissionSet, what: &str) {
    ui.notify(what, |n| match n {
        Notify::DevicesChanged(l) if there_of(l, id) == Some(Some(want.bits())) => Some(()),
        _ => None,
    });
}

#[test]
fn two_cores_in_a_session_learn_each_others_stored_grants() {
    let host_stores = ALL;
    let viewer_stores = PermissionSet::VIEW.union(PermissionSet::FILES);
    let (host, viewer, mut host_ui, mut viewer_ui) = session_of_minor_1(host_stores, viewer_stores);
    learns(&mut viewer_ui, &host.hex(), host_stores, "what the host allows, on the viewer");
    learns(&mut host_ui, &viewer.hex(), viewer_stores, "what the viewer allows, on the host");
    assert_eq!(viewer.trust.read(|s| s.devices[0].grants_there), Some(host_stores));
    assert_eq!(host.trust.read(|s| s.devices[0].grants_there), Some(viewer_stores));
    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}

#[test]
fn a_grant_change_on_the_host_during_a_session_shows_on_the_viewer_within_a_second() {
    let (host, viewer, mut host_ui, mut viewer_ui) = session_of_minor_1(ALL, PermissionSet::VIEW);
    learns(&mut viewer_ui, &host.hex(), ALL, "the first value");
    // The first value was written just now: the change below may wait for the end of that second, never longer.
    std::thread::sleep(Duration::from_millis(1100));

    let narrowed = PermissionSet::VIEW.union(PermissionSet::AUDIO);
    let t = Instant::now();
    host_ui.call(UiOp::SetGrants { device_id: viewer.hex(), grants: narrowed.bits() }).expect("narrowed");
    learns(&mut viewer_ui, &host.hex(), narrowed, "the new value on the viewer");
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());

    // And the other way: the viewer's user changes what the host may do there.
    let t = Instant::now();
    viewer_ui.call(UiOp::SetGrants { device_id: host.hex(), grants: PermissionSet::EMPTY.bits() }).expect("changed");
    learns(&mut host_ui, &viewer.hex(), PermissionSet::EMPTY, "the new value on the host");
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());

    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}

// ---------- GRANTS-SYNC.md 8 S6: the sync outside a session ----------

/// Two paired machines with no session. `a_allows` is what A lets B do, `b_allows` what B lets A do.
fn paired(a_allows: PermissionSet, b_allows: PermissionSet) -> (Machine, Machine) {
    let a = Machine::new("pc-a");
    let b = Machine::new("pc-b");
    a.pin(&b, a_allows);
    b.pin(&a, b_allows);
    (a, b)
}

fn there(m: &Machine) -> Option<PermissionSet> {
    m.trust.read(|s| s.devices[0].grants_there)
}

fn wait_there(m: &Machine, want: PermissionSet, limit: Duration, what: &str) -> Duration {
    let t = Instant::now();
    while there(m) != Some(want) {
        assert!(t.elapsed() < limit, "{what}: still {:?} after {:?}", there(m), t.elapsed());
        std::thread::sleep(Duration::from_millis(20));
    }
    t.elapsed()
}

fn dialled(m: &Machine) -> u64 {
    lock(&m.hub.hub.sync).dialled()
}

#[test]
fn a_grant_change_reaches_the_other_core_without_a_session_within_seconds() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    a.sees_minor(&b, false, 1);
    b.sees_minor(&a, false, 1);
    // Seeing each other syncs both ways.
    wait_there(&a, PermissionSet::VIEW, Duration::from_secs(6), "A learns what B allows");
    wait_there(&b, PermissionSet::CONTROL, Duration::from_secs(6), "B learns what A allows");
    let mut ui = a.client(info(false));
    let wider = PermissionSet::CONTROL.union(PermissionSet::FILES);
    assert!(ui.call(UiOp::SetGrants { device_id: b.hex(), grants: wider.bits() }).is_ok());
    let took = wait_there(&b, wider, Duration::from_secs(3), "B hears the change");
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert!(b.core.host().sessions().is_empty() && a.core.host().sessions().is_empty(), "no session was opened");
    drop(ui);
    a.finish();
    b.finish();
}

#[test]
fn a_core_that_comes_online_learns_the_grants_changed_while_it_was_away() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    // B is away: A changes what B may do; nothing can be sent.
    let mut ui = a.client(info(false));
    let changed = PermissionSet::VIEW.union(PermissionSet::AUDIO);
    assert!(ui.call(UiOp::SetGrants { device_id: b.hex(), grants: changed.bits() }).is_ok());
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(there(&b), None, "nothing reached B while it was away");
    // B comes back: each sees the other.
    a.sees_minor(&b, false, 1);
    b.sees_minor(&a, false, 1);
    wait_there(&b, changed, Duration::from_secs(6), "B learns the change made while it was away");
    drop(ui);
    a.finish();
    b.finish();
}

#[test]
fn two_cores_that_see_each_other_at_once_make_one_sync() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    a.sees_minor(&b, false, 1);
    b.sees_minor(&a, false, 1);
    wait_there(&a, PermissionSet::VIEW, Duration::from_secs(6), "A");
    wait_there(&b, PermissionSet::CONTROL, Duration::from_secs(6), "B");
    // Past the 3 s of the machine with the higher ID.
    std::thread::sleep(super::super::sync_dial::TIE_DELAY + Duration::from_millis(800));
    assert_eq!(dialled(&a) + dialled(&b), 1, "one exchange serves both ways");
    a.finish();
    b.finish();
}

#[test]
fn a_peer_that_answers_a_sync_with_a_violation_is_not_dialled_again_until_it_reappears() {
    // B is a 1.0 host behind a record that says minor 1.
    let a = Machine::new("pc-a");
    let b = Machine::with_minor("old-b", 0);
    a.pin(&b, PermissionSet::CONTROL);
    b.pin(&a, PermissionSet::VIEW);
    a.sees_minor(&b, false, 1);
    let t = Instant::now();
    while !lock(&a.hub.hub.sync).refused(&b.id()) {
        assert!(t.elapsed() < Duration::from_secs(8), "A dials B once and is refused");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!((dialled(&a), lock(&a.hub.hub.sync).answered()), (1, 0));
    assert_eq!(a.hub.hub.known_minor(&b.id(), Some(1)), 0, "it counts as minor 0 now");
    // A change is not pushed to it, nor is the same record seen again.
    let mut ui = a.client(info(false));
    assert!(ui.call(UiOp::SetGrants { device_id: b.hex(), grants: PermissionSet::VIEW.bits() }).is_ok());
    a.sees_minor(&b, false, 1);
    std::thread::sleep(super::super::sync_dial::PUSH_DELAY + Duration::from_millis(800));
    assert_eq!(dialled(&a), 1, "not dialled again");
    // It leaves the network and comes back: one more try.
    a.presence.set(Vec::new());
    a.sees_minor(&b, false, 1);
    let t = Instant::now();
    while dialled(&a) < 2 {
        assert!(t.elapsed() < Duration::from_secs(6), "dialled again after it reappeared");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(ui);
    a.finish();
    b.finish();
}

#[test]
fn a_peer_that_forgot_this_core_is_not_dialled_again_automatically() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    // B forgot A: the key of A is no longer pinned there.
    b.trust.update(|s| s.devices.clear()).unwrap();
    a.sees_minor(&b, false, 1);
    let t = Instant::now();
    while lock(&a.hub.hub.sync).per_unknown_key(&b.id()) != Some(true) {
        assert!(t.elapsed() < Duration::from_secs(8), "the key of A is refused once: {:?}", a.log.lines());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(dialled(&a), 1);
    let mut ui = a.client(info(false));
    assert!(ui.call(UiOp::SetGrants { device_id: b.hex(), grants: PermissionSet::VIEW.bits() }).is_ok());
    std::thread::sleep(super::super::sync_dial::PUSH_DELAY + Duration::from_millis(800));
    assert_eq!(dialled(&a), 1, "no automatic sync to a device that refused this key");
    drop(ui);
    a.finish();
    b.finish();
}

#[test]
fn an_answer_older_than_a_push_does_not_overwrite_it() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    let hub = &a.hub.hub;
    let started = tokio::time::Instant::now();
    std::thread::sleep(Duration::from_millis(30));
    // B pushed FILES after the sync of A started; that sync then answers with the older VIEW.
    hub.apply_peer_grants(b.id(), PermissionSet::FILES);
    hub.sync_finished(b.id(), started, Ok(swapdesk_proto::GrantsSync { proto_minor: 1, grants: PermissionSet::VIEW }));
    std::thread::sleep(Duration::from_millis(1300));
    assert_eq!(there(&a), Some(PermissionSet::FILES), "the push wins over the older answer");
    // An answer to a sync that started after the push is applied.
    let later = tokio::time::Instant::now();
    hub.sync_finished(b.id(), later, Ok(swapdesk_proto::GrantsSync { proto_minor: 1, grants: PermissionSet::CONTROL }));
    wait_there(&a, PermissionSet::CONTROL, Duration::from_secs(3), "the newer answer");
    a.finish();
    b.finish();
}

#[test]
fn known_minor_prefers_the_authenticated_value_over_the_hint() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    let hub = &a.hub.hub;
    assert_eq!(hub.known_minor(&b.id(), Some(0)), 0, "the hint alone");
    lock(&hub.minors).said(b.id(), 1, Some(0));
    assert_eq!(hub.known_minor(&b.id(), Some(0)), 1, "what it said over an authenticated connection wins");
    a.finish();
    b.finish();
}

#[test]
fn a_peer_of_minor_0_is_never_dialled_for_a_sync() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    a.sees_minor(&b, false, 0);
    let mut ui = a.client(info(false));
    assert!(ui.call(UiOp::SetGrants { device_id: b.hex(), grants: PermissionSet::VIEW.bits() }).is_ok());
    std::thread::sleep(super::super::sync_dial::TIE_DELAY + Duration::from_millis(500));
    assert_eq!(dialled(&a), 0, "a record without the minor key is a 1.0 device");
    drop(ui);
    a.finish();
    b.finish();
}

/// GRANTS-SYNC.md 8 S7: op 39 syncs the device when its last sync is older than 30 s, and not again within them;
/// a device that is not paired is `NotFound`.
#[test]
fn refresh_grants_is_limited_to_once_per_30_s_per_device() {
    let (a, b) = paired(PermissionSet::CONTROL, PermissionSet::VIEW);
    // A sees B as minor 1; B does not see A, so only the refresh dials.
    a.sees_minor(&b, false, 1);
    let t = Instant::now();
    while dialled(&a) < 1 {
        assert!(t.elapsed() < Duration::from_secs(6), "the appearance syncs once");
        std::thread::sleep(Duration::from_millis(20));
    }
    wait_there(&a, PermissionSet::VIEW, Duration::from_secs(6), "the first answer");
    let mut ui = a.client(info(false));
    // Right after a sync: no new one.
    assert!(ui.call(UiOp::RefreshGrants { device_id: b.hex() }).is_ok());
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(dialled(&a), 1, "the last sync is fresh");
    // Pretend the last sync is old: the refresh dials once, and a second refresh at once does not.
    {
        let mut st = lock(&a.hub.hub.sync);
        st.age_for_test(&b.id(), super::super::sync_dial::REFRESH_GAP + Duration::from_secs(1));
    }
    assert!(ui.call(UiOp::RefreshGrants { device_id: b.hex() }).is_ok());
    assert!(ui.call(UiOp::RefreshGrants { device_id: b.hex() }).is_ok());
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(dialled(&a), 2, "one refresh within the 30 s");
    assert_eq!(ui.call(UiOp::RefreshGrants { device_id: device_id_hex(&[7; 16]) }).unwrap_err(), ApiCode::NotFound);
    drop(ui);
    a.finish();
    b.finish();
}
