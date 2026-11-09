//! The proof of the pre-1.0 security review through the hub: what a UI that is not the console's may not do (a user
//! parked in a background session by fast user switching keeps its `Swapdesk.exe` connected to its own pipe), the
//! widening of unattended access, who sees and answers a pairing, and the blocked addresses.

use super::*;

/// A UI of another session than the console's (session 1 in these tests): `admin` is the elevation of its token.
fn background(machine: &Machine, admin: bool) -> Client {
    machine.client(ConnInfo { session_id: 9, user: user("S-1-5-21-2"), admin })
}

// ---------- 1 and 2: the operations that change who may reach this machine ----------

#[test]
fn pairing_grants_forget_connect_and_wake_belong_to_the_console() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let id = other.hex();
    // Even an elevated token in another session is refused: this is about whose desk it is, not about elevation.
    let mut away = background(&m, true);
    let denied = ApiCode::NotPermitted;

    assert_eq!(
        away.call(UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::VIEW.bits() }).unwrap_err(),
        denied
    );
    assert_eq!(away.call(UiOp::ForgetDevice { device_id: id.clone() }).unwrap_err(), denied);
    assert_eq!(away.call(UiOp::RenameDevice { device_id: id.clone(), name: "novo".into() }).unwrap_err(), denied);
    assert_eq!(m.trust.read(|s| s.devices[0].name.clone()), "other-pc", "nothing renamed");
    assert_eq!(away.call(UiOp::PairOpen { device_id: None }).unwrap_err(), denied);
    assert_eq!(away.call(UiOp::PairStart { pairing_id: 1, device_id: id.clone() }).unwrap_err(), denied);
    assert_eq!(away.call(UiOp::PairConfirm { pairing_id: 1, matches: true }).unwrap_err(), denied);
    assert_eq!(away.call(UiOp::PairCancel { pairing_id: 1 }).unwrap_err(), denied);
    assert_eq!(
        away.call(UiOp::Connect { device_id: id.clone(), intent: Intent::View, unattended: false }).unwrap_err(),
        denied
    );
    assert_eq!(away.call(UiOp::Wake { device_id: id.clone(), intent: Intent::View }).unwrap_err(), denied);
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here), PermissionSet::CONTROL, "nothing changed");
    assert_eq!(m.trust.read(|s| s.devices.len()), 1, "nothing forgotten");
    assert!(!m.hub.hub.mode.is_open(), "no pairing mode was opened");

    // What only reads stays open to it, and the console's UI does all of it.
    assert!(away.call(UiOp::ListDevices).is_ok());
    let mut console = m.client(info(false));
    assert!(console.call(UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::VIEW.bits() }).is_ok());
    drop((away, console));
    m.finish();
    other.finish();
}

#[test]
fn a_connection_that_is_not_elevated_cannot_widen_a_device_that_has_unattended_access() {
    let m = Machine::new("sala");
    let other = Machine::new("other");
    m.pin(&other, PermissionSet::CONTROL);
    let id = other.hex();
    let mut plain = m.client(info(false));
    let mut admin = m.client(info(true));

    // The administrator grants UNATTENDED with FILES.
    let unattended_files = PermissionSet::UNATTENDED.union(PermissionSet::FILES);
    assert!(admin.call(UiOp::SetGrants { device_id: id.clone(), grants: unattended_files.bits() }).is_ok());

    // Nobody who is not elevated adds a bit to it, whichever bit: that is entry without a prompt with more reach.
    for extra in [PermissionSet::CONTROL, PermissionSet::TERMINAL, PermissionSet::CLIPBOARD] {
        let wider = unattended_files.union(extra).bits();
        assert_eq!(
            plain.call(UiOp::SetGrants { device_id: id.clone(), grants: wider }).unwrap_err(),
            ApiCode::NeedsAdmin,
            "adding {extra:?}"
        );
    }
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here), unattended_files, "nothing widened");

    // The administrator may; and taking bits away, UNATTENDED included, stays open to the console's UI.
    let widest = unattended_files.union(PermissionSet::CONTROL).bits();
    assert!(admin.call(UiOp::SetGrants { device_id: id.clone(), grants: widest }).is_ok());
    let narrower = PermissionSet::UNATTENDED.bits();
    assert!(plain.call(UiOp::SetGrants { device_id: id.clone(), grants: narrower }).is_ok());
    assert_eq!(m.trust.read(|s| s.devices[0].grants_here.bits()), narrower);
    assert!(plain.call(UiOp::SetGrants { device_id: id.clone(), grants: PermissionSet::VIEW.bits() }).is_ok());

    // A device with no unattended access still takes any bit from the console's UI.
    assert!(plain.call(UiOp::SetGrants { device_id: id, grants: PermissionSet::CONTROL.bits() }).is_ok());

    drop((plain, admin));
    m.finish();
    other.finish();
}

// ---------- 4: a pairing is its opener's ----------

#[test]
fn a_pairing_screen_is_seen_and_answered_by_the_connection_that_opened_it() {
    let a = Machine::new("alfa");
    let b = Machine::new("beta");
    let mut ua = a.client(info(false));
    let mut ub = b.client(info(false));
    // Two more UIs on A: another connection of the console's session, and one of a background session.
    let mut other_console = a.client(info(false));
    let mut away = background(&a, false);

    let ReplyBody::PairingId(id_b) = ub.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    let ReplyBody::PairingId(id_a) = ua.call(UiOp::PairOpen { device_id: Some(b.hex()) }).unwrap() else { panic!() };
    ua.pairing(id_a, "Open", |s| matches!(s, PairingState::Open { .. }));
    let is_pairing = |m: &Ipc| matches!(m, Ipc::UiNotify(Notify::Pairing { .. }));
    assert!(other_console.nothing_within(Duration::from_millis(300), is_pairing), "another connection saw the screen");
    assert!(away.nothing_within(Duration::from_millis(100), is_pairing), "another session saw the screen");

    // The screen is the opener's: no other connection opens a second one, starts, confirms or cancels it.
    assert_eq!(other_console.call(UiOp::PairOpen { device_id: None }).unwrap_err(), ApiCode::Busy);
    a.sees(&b, true);
    assert_eq!(
        other_console.call(UiOp::PairStart { pairing_id: id_a, device_id: b.hex() }).unwrap_err(),
        ApiCode::NotPermitted
    );
    assert!(matches!(ua.call(UiOp::PairStart { pairing_id: id_a, device_id: b.hex() }).unwrap(), ReplyBody::Done));

    // The code is shown to the opener alone; nobody else can give the verdict on it or close the screen.
    ua.pairing(id_a, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. }));
    ub.pairing(id_b, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. }));
    assert!(other_console.nothing_within(Duration::from_millis(300), is_pairing), "another connection saw the code");
    assert!(away.nothing_within(Duration::from_millis(100), is_pairing), "another session saw the code");
    assert_eq!(
        other_console.call(UiOp::PairConfirm { pairing_id: id_a, matches: true }).unwrap_err(),
        ApiCode::NotPermitted
    );
    assert_eq!(other_console.call(UiOp::PairCancel { pairing_id: id_a }).unwrap_err(), ApiCode::NotPermitted);
    assert_eq!(away.call(UiOp::PairConfirm { pairing_id: id_a, matches: true }).unwrap_err(), ApiCode::NotPermitted);
    assert!(a.trust.read(|s| s.devices.is_empty()), "nothing was pinned by the others");

    // The opener still can: it cancels, and the screen is gone.
    assert!(ua.call(UiOp::PairCancel { pairing_id: id_a }).is_ok());
    assert!(!a.hub.hub.mode.is_open());

    drop((ua, ub, other_console, away));
    a.finish();
    b.finish();
}

// ---------- 5: a blocked address is dropped before the handshake ----------

#[test]
fn the_pairing_endpoint_ignores_a_blocked_address_before_any_handshake() {
    let a = Machine::new("alfa");
    let b = Machine::new("beta");
    let mut ua = a.client(info(false));
    let mut ub = b.client(info(false));
    // Both ends are loopback in these tests: B blocks the address A calls from.
    b.trust.update(|s| s.blocked.push([127, 0, 0, 1])).expect("store");

    let ReplyBody::PairingId(id_b) = ub.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    let ReplyBody::PairingId(id_a) = ua.call(UiOp::PairOpen { device_id: Some(b.hex()) }).unwrap() else { panic!() };
    a.sees(&b, true);
    assert!(ua.call(UiOp::PairStart { pairing_id: id_a, device_id: b.hex() }).is_ok());
    let shows_code = |m: &Ipc| matches!(m, Ipc::UiNotify(Notify::Pairing { state: PairingState::ShowCode { .. }, .. }));
    assert!(ub.nothing_within(Duration::from_millis(1500), shows_code), "B talked to a blocked address");
    assert!(ua.nothing_within(Duration::from_millis(100), shows_code));
    assert!(b.trust.read(|s| s.devices.is_empty()));
    let _ = id_b;

    assert!(ua.call(UiOp::PairCancel { pairing_id: id_a }).is_ok());
    drop((ua, ub));
    a.finish();
    b.finish();
}
