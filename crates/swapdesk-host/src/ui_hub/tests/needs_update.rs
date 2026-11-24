//! docs/ANDROID.md 5: a phone needs protocol minor 3 of a computer, since an older build knows no `Os::Android`.
//! The phone's service says so about such a computer ("needs an update") and dials nothing toward it; a computer's
//! service needs nothing of anybody and never says it.

use super::*;
use swapdesk_ipc::DeviceStatus;
use swapdesk_proto::ErrorCode;

fn listed(ui: &mut Client) -> Vec<swapdesk_ipc::DeviceView> {
    let ReplyBody::Devices(list) = ui.call(UiOp::ListDevices).unwrap() else { panic!("not a device list") };
    list
}

#[test]
fn a_phone_flags_a_computer_below_minor_3_and_dials_nothing_toward_it() {
    let phone = Machine::phone("celular");
    let pc = Machine::new("sala");
    let mut up = phone.client(info(false));
    let mut uc = pc.client(info(false));
    // The computer's own pairing is open, as its person opens it for the phone: a dial would start a ceremony.
    let ReplyBody::PairingId(_) = uc.call(UiOp::PairOpen { device_id: None }).unwrap() else { panic!() };
    let heard = pc.audit.entries().len();

    // Not paired, and its record says minor 2 (a 1.1.0 build).
    phone.sees_minor(&pc, true, 2);
    let list = listed(&mut up);
    assert_eq!(list.len(), 1);
    assert!(list[0].needs_update && !list[0].paired, "{:?}", list[0]);
    assert_eq!(list[0].status, DeviceStatus::OnNetworkUnpaired { pairing_open: true }, "still where it is");
    let ReplyBody::PairingId(id) = up.call(UiOp::PairOpen { device_id: Some(pc.hex()) }).unwrap() else { panic!() };
    assert_eq!(
        up.call(UiOp::PairStart { pairing_id: id, device_id: pc.hex() }).unwrap_err(),
        ApiCode::NeedsUpdate,
        "the pairing is refused before anything is dialled"
    );
    assert!(up.call(UiOp::PairCancel { pairing_id: id }).is_ok());

    // Paired (before the computer went back to an older build): the same, for a connect and for the grants sync.
    phone.pin(&pc, ALL);
    pc.pin(&phone, ALL);
    phone.sees_minor(&pc, false, 2);
    let list = listed(&mut up);
    assert!(list[0].needs_update && list[0].paired, "{:?}", list[0]);
    let connect = up.call(UiOp::Connect { device_id: pc.hex(), intent: Intent::Control, unattended: false });
    assert_eq!(connect.unwrap_err(), ApiCode::NeedsUpdate);
    assert!(phone.hub.hub.sync_target(&pc.id()).is_none(), "no grants sync toward it either");

    // Nothing reached the computer: no attempt in its audit, its pairing still open and untouched.
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(pc.audit.entries().len(), heard, "{:?}", pc.audit.entries());
    assert!(pc.hub.hub.mode.is_open());
    assert_eq!(lock(&phone.hub.hub.sync).dialled(), 0);

    // The computer was updated: its record says minor 3, and the phone dials it as any other.
    phone.sees_minor(&pc, false, swapdesk_proto::PROTO_MINOR);
    assert!(!listed(&mut up)[0].needs_update);
    assert!(phone.hub.hub.sync_target(&pc.id()).is_some());

    drop((up, uc));
    phone.finish();
    pc.finish();
}

#[test]
fn only_evidence_flags_a_computer_and_a_computers_own_service_flags_nobody() {
    let phone = Machine::phone("celular");
    let pc = Machine::new("sala");
    let mut up = phone.client(info(false));
    phone.pin(&pc, ALL);

    // Away and never heard: nothing is known of its build, and "minor 0 until proven otherwise" (what is asked of
    // it) is not a statement about it. Without this every paired computer that is off would need an update.
    let list = listed(&mut up);
    assert_eq!(list[0].status, DeviceStatus::Offline);
    assert!(!list[0].needs_update, "{:?}", list[0]);
    assert_eq!(phone.hub.hub.known_minor(&pc.id(), None), 0);
    assert!(!phone.hub.hub.needs_update(&pc.id()));

    // On the network with a record of today's build: fine. With an old record: flagged. Gone again: nothing is
    // known once more (a record is a hint, not what the device said).
    phone.sees_minor(&pc, false, swapdesk_proto::PROTO_MINOR);
    assert!(!listed(&mut up)[0].needs_update);
    phone.sees_minor(&pc, false, 0);
    assert!(listed(&mut up)[0].needs_update);
    phone.presence.set(Vec::new());
    assert!(!listed(&mut up)[0].needs_update);

    // A computer needs nothing newer of anybody: a 1.0 record is a device it talks to.
    let desk = Machine::new("escritorio");
    let mut ud = desk.client(info(false));
    desk.pin(&pc, ALL);
    desk.sees(&pc, false);
    assert!(!listed(&mut ud)[0].needs_update);
    assert!(!desk.hub.hub.needs_update(&pc.id()));
    desk.trust.update(|s| s.devices.clear()).expect("store");
    assert!(!listed(&mut ud)[0].needs_update, "nor one it is not paired with");

    drop((up, ud));
    phone.finish();
    pc.finish();
    desk.finish();
}

/// A computer reached by a typed address has no record to read: the phone tries, and a build that does not know a
/// phone closes at the first message (`swapdesk-trust`: `TrustError::PeerTooOld`). The phone's screen says "needs
/// an update"; from a computer the same close is no matter of versions and stays the internal error it was.
#[test]
fn a_close_at_the_first_pairing_message_reads_as_needs_an_update_only_on_a_phone() {
    use super::super::pairing::pair_error_code;
    use swapdesk_trust::TrustError;
    assert_eq!(pair_error_code(&TrustError::PeerTooOld, Os::Android), ApiCode::NeedsUpdate);
    assert_eq!(pair_error_code(&TrustError::PeerTooOld, Os::Windows), ApiCode::Internal);
    assert_eq!(pair_error_code(&TrustError::PeerTooOld, Os::MacOs), ApiCode::Internal);
    // Every other failure reads on a phone as it does on a computer.
    for e in [
        TrustError::Tls("pairing stream closed".into()),
        TrustError::Refused(ErrorCode::UnsupportedVersion),
        TrustError::Protocol(ErrorCode::ProtocolViolation),
        TrustError::SasRejected,
    ] {
        assert_eq!(pair_error_code(&e, Os::Android), pair_error_code(&e, Os::Windows), "{e}");
        assert_ne!(pair_error_code(&e, Os::Android), ApiCode::NeedsUpdate, "{e}");
    }
}
