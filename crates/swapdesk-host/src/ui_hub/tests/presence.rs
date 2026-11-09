//! "On the network" in the device list: a device with a live session is reachable whatever DNS-SD says, and a program
//! with no OS network notices (the Mac app) still tells discovery when the network changed.

use swapdesk_ipc::DeviceStatus;
use swapdesk_platform::{IfKind, NetInterface};

use super::*;

fn status_of(ui: &mut Client, hex: &str) -> DeviceStatus {
    let ReplyBody::Devices(list) = ui.call(UiOp::ListDevices).expect("the list") else { panic!("not a list") };
    list.into_iter().find(|d| d.id == hex).map(|d| d.status).expect("the device is listed")
}

fn on_the_network(s: &DeviceStatus) -> bool {
    matches!(s, DeviceStatus::Online { address, .. } if address == "127.0.0.1")
}

#[test]
fn a_device_with_a_live_session_is_on_the_network_on_both_sides_without_any_dns_sd_record() {
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-mac");
    host.pin(&viewer, ALL);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    // The viewer finds the host to dial it; the host never hears of the viewer from DNS-SD (the field case: the Mac's
    // announcement had died, and it controlled the PC all the same).
    viewer.sees(&host, false);
    let mut host_ui = host.client(info(false));
    let mut viewer_ui = viewer.client(info(false));
    assert_eq!(status_of(&mut host_ui, &viewer.hex()), DeviceStatus::Offline, "no record and no session yet");

    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let incoming = host_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.request_id),
        _ => None,
    });
    host_ui
        .call(UiOp::AnswerIncoming { request_id: incoming, decision: Decision::Accept, grants: ALL.bits() })
        .expect("accepted");
    let ReplyBody::SessionId(session_id) = viewer_ui.reply(req, CONNECT_OP).expect("the session opens") else {
        panic!()
    };
    host_ui.notify("Active on the host", |n| match n {
        Notify::Session { session_id: id, state: SessionState::Active { .. }, .. } if *id == session_id => Some(()),
        _ => None,
    });
    // The host's UI is told at once: the list it gets with the session shows the viewer on the network.
    host_ui.notify("the device list with the viewer on the network", |n| match n {
        Notify::DevicesChanged(list) => {
            list.iter().any(|d| d.id == viewer.hex() && on_the_network(&d.status)).then_some(())
        }
        _ => None,
    });
    assert!(on_the_network(&status_of(&mut host_ui, &viewer.hex())), "the host lists the viewer as reachable");

    // The viewer's side, once the host's record is gone too.
    viewer_ui.notify("Active on the viewer", |n| match n {
        Notify::Session { session_id: id, state: SessionState::Active { .. }, .. } if *id == session_id => Some(()),
        _ => None,
    });
    viewer.presence.set(Vec::new());
    assert!(on_the_network(&status_of(&mut viewer_ui, &host.hex())), "the viewer lists the host as reachable");

    // The session ends: with no record either, both are off the network again.
    viewer_ui.call(UiOp::Disconnect { session_id }).expect("disconnect");
    let ended = |n: &Notify| match n {
        Notify::Session { session_id: id, state: SessionState::Ended { .. }, .. } if *id == session_id => Some(()),
        _ => None,
    };
    viewer_ui.notify("Ended on the viewer", ended);
    host_ui.notify("Ended on the host", ended);
    wait_for("the host to list the viewer offline", || {
        (status_of(&mut host_ui, &viewer.hex()) == DeviceStatus::Offline).then_some(())
    });
    wait_for("the viewer to list the host offline", || {
        (status_of(&mut viewer_ui, &host.hex()) == DeviceStatus::Offline).then_some(())
    });

    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}

fn wifi(addr: Ipv4Addr) -> NetInterface {
    NetInterface {
        index: 11,
        name: "en0".into(),
        kind: IfKind::WiFi,
        up: true,
        mac: Some([0x1C, 0xF6, 0x4C, 0x99, 0x70, 0x77]),
        ipv4: vec![(addr, 24)],
        category: None,
        inbound_blocked: None,
    }
}

/// This machine's primary IPv4 address, as the routing table picks it (no packet is sent), or `None` (the test skips).
fn primary_ipv4() -> Option<Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

#[test]
fn after_an_address_change_the_session_endpoint_listens_on_the_new_address_and_keeps_its_live_session() {
    // The field case (09/11): DHCP moved the Mac from 192.168.1.2 to .3 and its session endpoint stayed bound to .2,
    // so nobody could open a session to it until the app restarted.
    let Some(real) = primary_ipv4() else {
        println!("SKIP: no IPv4 address with a route on this machine");
        return;
    };
    let mut host = Machine::new("host-pc");
    let viewer = Machine::new("viewer-mac");
    host.pin(&viewer, ALL);
    viewer.pin(&host, PermissionSet::EMPTY);
    host.attach_helper(IpcRole::HelperSystem);
    host.attach_helper(IpcRole::HelperUser);
    viewer.sees(&host, false);
    let mut host_ui = host.client(info(false));
    let mut viewer_ui = viewer.client(info(false));
    let before = host.core.local_addrs();
    assert!(before.iter().all(|a| a.ip().is_loopback()), "only the development address so far: {before:?}");

    // A session over the development (loopback) endpoint.
    let req = viewer_ui.request(UiOp::Connect { device_id: host.hex(), intent: Intent::Control, unattended: false });
    let incoming = host_ui.notify("IncomingRequest", |n| match n {
        Notify::IncomingRequest(r) => Some(r.request_id),
        _ => None,
    });
    host_ui
        .call(UiOp::AnswerIncoming { request_id: incoming, decision: Decision::Accept, grants: ALL.bits() })
        .expect("accepted");
    let ReplyBody::SessionId(session_id) = viewer_ui.reply(req, CONNECT_OP).expect("the session opens") else {
        panic!()
    };
    wait_for("the host session", || (host.core.host().sessions().len() == 1).then_some(()));

    // The machine's Wi-Fi gets an address: the next network notice makes the endpoint listen there too.
    host.nets.set(vec![wifi(real)]);
    host.hub.network_changed();
    let after = host.core.local_addrs();
    assert!(after.iter().any(|a| a.ip() == std::net::IpAddr::V4(real)), "listens on the new address: {after:?}");
    assert!(before.iter().all(|a| after.contains(a)), "the address that stayed is untouched");
    assert!(
        host.log.lines().iter().any(|l| l.text.contains("session endpoint on") && l.text.contains(&real.to_string())),
        "the new endpoint is in the trace"
    );
    // A notice with nothing new changes nothing.
    assert!(!host.core.network_changed());

    // The address goes again: it stops being listened on; the session on the address that stayed lives on.
    host.nets.set(Vec::new());
    host.hub.network_changed();
    let gone = host.core.local_addrs();
    assert!(!gone.iter().any(|a| a.ip() == std::net::IpAddr::V4(real)), "{gone:?}");
    assert_eq!(gone, before);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(host.core.host().sessions().len(), 1, "the live session was not dropped");
    assert!(viewer_ui.nothing_within(Duration::from_millis(200), |m| matches!(
        m,
        Ipc::UiNotify(Notify::Session { session_id: id, state: SessionState::Ended { .. }, .. }) if *id == session_id
    )));

    viewer_ui.call(UiOp::Disconnect { session_id }).expect("disconnect");
    drop((host_ui, viewer_ui));
    viewer.finish();
    host.finish();
}

#[test]
fn a_paired_device_typed_by_address_or_seen_by_dns_sd_is_one_entry_on_the_network_whatever_its_name() {
    // The field case (09/11): the PC had paired with the MacBook (the pairing stored 192.168.1.3), the owner typed that
    // address too and renamed the Mac; the PC listed "MacBook, Fora da rede" and an unpaired "192.168.1.3" below it.
    let m = Machine::with_interfaces("pc", FakeNetInterfaces::windows_typical());
    let mac = Identity::generate(0).expect("identity");
    let mac_hex = device_id_hex(&mac.device_id());
    m.trust
        .update(|s| {
            s.devices.push(PairedDevice::new_paired(
                *mac.spki(),
                "MacBook".into(),
                Os::MacOs,
                "27.2".into(),
                1,
                Some([192, 168, 1, 3]),
            ))
        })
        .expect("store");
    let mut ui = m.client(info(false));
    let list = |ui: &mut Client| {
        let ReplyBody::Devices(list) = ui.call(UiOp::ListDevices).expect("the list") else { panic!() };
        list
    };
    let online_at = |addr: &str| DeviceStatus::Online { address: addr.into(), rtt_ms: 0 };

    // Typed: the address the pairing stored is that device's, so the reply and the list are its paired entry.
    let typed = device_of(ui.call(UiOp::AddByAddress { ipv4: "192.168.1.3".into() }));
    assert_eq!((typed.id.as_str(), typed.paired, typed.name.as_str()), (mac_hex.as_str(), true, "MacBook"));
    assert_eq!(typed.status, online_at("192.168.1.3"));
    let now = list(&mut ui);
    assert_eq!(now.len(), 1, "listed once: {now:?}");
    assert_eq!((now[0].id.as_str(), now[0].status.clone()), (mac_hex.as_str(), online_at("192.168.1.3")));

    // DNS-SD announces it under its new name: matched by its fingerprint, still one entry, named as paired.
    let announce = |addr: Ipv4Addr| {
        m.presence.set(vec![PresencePeer {
            device_id: mac.device_id(),
            name: "Mac da Alex".into(),
            os: Os::MacOs,
            pairing_open: false,
            addrs: vec![addr],
            session_port: 47820,
            pair_port: 47821,
        }])
    };
    announce(Ipv4Addr::new(192, 168, 1, 3));
    let now = list(&mut ui);
    assert_eq!(now.len(), 1, "listed once: {now:?}");
    assert_eq!((now[0].name.as_str(), now[0].status.clone()), ("MacBook", online_at("192.168.1.3")));

    // It moved: DNS-SD places it at .9, so the typed .3 is somebody else's now and is listed on its own.
    announce(Ipv4Addr::new(192, 168, 1, 9));
    let now = list(&mut ui);
    assert_eq!(now.len(), 2, "{now:?}");
    assert_eq!((now[0].id.as_str(), now[0].status.clone()), (mac_hex.as_str(), online_at("192.168.1.9")));
    assert_eq!((now[1].name.as_str(), now[1].paired), ("192.168.1.3", false));

    // An address no device is known at is an entry of its own.
    m.presence.set(Vec::new());
    let stranger = device_of(ui.call(UiOp::AddByAddress { ipv4: "192.168.1.50".into() }));
    assert_eq!((stranger.paired, stranger.name.as_str()), (false, "192.168.1.50"));
    let now = list(&mut ui);
    assert_eq!(now.iter().filter(|d| d.id == mac_hex).count(), 1);
    assert_eq!(now.len(), 2, "the MacBook (its stored address typed) and 192.168.1.50: {now:?}");

    drop(ui);
    m.finish();
}
