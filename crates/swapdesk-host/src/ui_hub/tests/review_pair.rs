//! Review of 22/11/2026, group A (find, pair, LAN guard, trust): pairing through two hubs.

use super::*;

/// Shows `other` on `me`'s network with its pairing port, open or not yet.
fn sees_at(me: &Machine, other: &Machine, pair_port: u16) {
    let SocketAddr::V4(addr) = other.session_addr() else { unreachable!() };
    me.presence.set(vec![PresencePeer {
        device_id: other.id(),
        name: "other-pc".into(),
        os: Os::Windows,
        pairing_open: true,
        addrs: vec![*addr.ip()],
        session_port: addr.port(),
        pair_port,
        proto_minor: swapdesk_proto::PROTO_MINOR,
    }]);
}

fn pairing_id(r: Result<ReplyBody, ApiCode>) -> u32 {
    match r {
        Ok(ReplyBody::PairingId(id)) => id,
        other => panic!("not a pairing id: {other:?}"),
    }
}

/// Both screens show the same code, both users compare it for `comparing` and confirm, and each screen ends paired
/// once, with the other machine.
fn both_pair(a: &Machine, ua: &mut Client, id_a: u32, b: &Machine, ub: &mut Client, id_b: u32, comparing: Duration) {
    let code = |ui: &mut Client, id| match ui
        .pairing(id, "ShowCode", |s| matches!(s, PairingState::ShowCode { .. } | PairingState::Failed { .. }))
    {
        PairingState::ShowCode { code, .. } => code.0,
        other => panic!("the screen ended instead of showing the code: {other:?}"),
    };
    let (code_a, code_b) = (code(ua, id_a), code(ub, id_b));
    assert_eq!(code_a, code_b, "one ceremony, one code on both screens");
    std::thread::sleep(comparing);
    assert!(ua.call(UiOp::PairConfirm { pairing_id: id_a, matches: true }).is_ok());
    assert!(ub.call(UiOp::PairConfirm { pairing_id: id_b, matches: true }).is_ok());
    let end = |ui: &mut Client, id| {
        ui.pairing(id, "the end", |s| matches!(s, PairingState::Done { .. } | PairingState::Failed { .. }))
    };
    assert_eq!(end(ua, id_a), PairingState::Done { device_id: b.hex() });
    assert_eq!(end(ub, id_b), PairingState::Done { device_id: a.hex() });
    let failed = |m: &Ipc| matches!(m, Ipc::UiNotify(Notify::Pairing { state: PairingState::Failed { .. }, .. }));
    assert!(ua.nothing_within(Duration::from_millis(500), failed), "a second end on A's screen");
    assert!(ub.nothing_within(Duration::from_millis(500), failed), "a second end on B's screen");
    for (me, other) in [(a, b), (b, a)] {
        assert!(me.trust.read(|s| s.find_by_id(&other.id()).is_some()), "the other machine is pinned");
        assert!(me.trust.read(|s| s.lockout.failures.is_empty() && s.lockout.provisional.is_none()));
    }
}

/// Review of 22/11/2026, A3: both users pressed "Parear" on each other's card at once. Each machine's initiator took its
/// own pairing slot, each responder answered the other's `PairHello` with `BUSY`, and both screens failed. Now the
/// machine with the higher device ID gives way and its screen follows the other's ceremony.
#[test]
fn both_users_pressing_parear_on_each_other_at_once_pair_once() {
    let a = Machine::new("alfa");
    let b = Machine::new("beta");
    let mut ua = a.client(info(false));
    let mut ub = b.client(info(false));
    let id_a = pairing_id(ua.call(UiOp::PairOpen { device_id: Some(b.hex()) }));
    let id_b = pairing_id(ub.call(UiOp::PairOpen { device_id: Some(a.hex()) }));
    a.sees(&b, true);
    b.sees(&a, true);
    // The two presses at once: neither waits for its own reply before the other's goes.
    let (start_a, start_b) = (
        UiOp::PairStart { pairing_id: id_a, device_id: b.hex() },
        UiOp::PairStart { pairing_id: id_b, device_id: a.hex() },
    );
    let op = start_a.code();
    let (ra, rb) = (ua.request(start_a), ub.request(start_b));
    assert!(matches!(ua.reply(ra, op), Ok(ReplyBody::Done)));
    assert!(matches!(ub.reply(rb, op), Ok(ReplyBody::Done)));
    both_pair(&a, &mut ua, id_a, &b, &mut ub, id_b, Duration::ZERO);
    drop((ua, ub));
    a.finish();
    b.finish();
}

/// Review of 22/11/2026, A3: one user pressed "Parear" on the other's card first, and the other did the same on this
/// one's card while the first machine's redial had not reached the second yet. The second press reached the first
/// machine's pairing endpoint, so its responder held the slot and ran the ceremony; the first machine's next redial then
/// met its own `BUSY` and ended the screen under the running ceremony while the users compared the code. Now it waits
/// for that ceremony, which drives the screen.
#[test]
fn a_side_still_redialling_follows_the_ceremony_the_other_press_started() {
    let a = Machine::new("alfa");
    let b = Machine::new("beta");
    let mut ua = a.client(info(false));
    let mut ub = b.client(info(false));
    let id_a = pairing_id(ua.call(UiOp::PairOpen { device_id: Some(b.hex()) }));
    // A's dials go where nothing answers (B's pairing is not reached yet), so they are made again every few seconds.
    let silent = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("a silent port");
    sees_at(&a, &b, silent.local_addr().expect("its port").port());
    assert!(ua.call(UiOp::PairStart { pairing_id: id_a, device_id: b.hex() }).is_ok());
    let id_b = pairing_id(ub.call(UiOp::PairOpen { device_id: Some(a.hex()) }));
    b.sees(&a, true);
    assert!(ub.call(UiOp::PairStart { pairing_id: id_b, device_id: a.hex() }).is_ok());
    // The users take a few seconds to compare the code: longer than one of A's dials and its pause.
    both_pair(&a, &mut ua, id_a, &b, &mut ub, id_b, Duration::from_millis(4500));
    drop((ua, ub, silent));
    a.finish();
    b.finish();
}

/// A neighbour table that knows one device and counts what it was asked: `resolve_mac` is the one that may send a
/// packet (ARP, a probe datagram) toward the address.
#[derive(Default)]
struct CountingTable {
    probed: Mutex<Vec<std::net::IpAddr>>,
}

impl swapdesk_platform::NeighborTable for CountingTable {
    fn cached_mac(&self, peer: std::net::IpAddr) -> Option<[u8; 6]> {
        (peer == std::net::IpAddr::from([192, 168, 0, 57])).then_some([0x02, 0x5a, 0xd0, 0, 0, 0x39])
    }

    fn resolve_mac(&self, peer: std::net::IpAddr) -> Option<[u8; 6]> {
        lock(&self.probed).push(peer);
        self.cached_mac(peer)
    }
}

/// Review of 22/11/2026, A9: "Bloquear este dispositivo" asked the network for the MAC of any IPv4 address it was
/// given, so an address off every approved link sent a packet out through the router. The network is asked only for
/// an address the LAN guard finds on-link; any other is looked up in the table as it is, with no traffic.
#[test]
fn blocking_a_device_asks_the_network_only_for_an_address_on_an_approved_link() {
    let m = Machine::with_interfaces("sala", FakeNetInterfaces::new(vec![wifi(Ipv4Addr::new(192, 168, 0, 10))]));
    let table = Arc::new(CountingTable::default());
    m.hub.set_neighbors(Arc::clone(&table) as Arc<dyn swapdesk_platform::NeighborTable>);
    let mut ui = m.client(info(false));
    let block = |ipv4: &str| UiOp::BlockDevice { ipv4: ipv4.into(), name: String::new() };
    for off_link in ["8.8.8.8", "10.1.2.3", "192.168.1.57", "127.0.0.1", "192.168.0.10", "192.168.0.255"] {
        assert_eq!(ui.call(block(off_link)).unwrap_err(), ApiCode::NotFound, "{off_link}");
    }
    assert!(lock(&table.probed).is_empty(), "the network was asked about {:?}", lock(&table.probed));
    // On the Wi-Fi's subnet: the network may be asked, and the device is blocked.
    assert!(matches!(ui.call(block("192.168.0.57")), Ok(ReplyBody::Blocked(l)) if l.len() == 1));
    assert_eq!(*lock(&table.probed), [std::net::IpAddr::from([192, 168, 0, 57])]);
    drop(ui);
    m.finish();
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

fn wifi(addr: Ipv4Addr) -> swapdesk_platform::NetInterface {
    swapdesk_platform::NetInterface {
        index: 11,
        name: "en0".into(),
        kind: swapdesk_platform::IfKind::WiFi,
        up: true,
        mac: Some([0x1C, 0xF6, 0x4C, 0x99, 0x70, 0x77]),
        ipv4: vec![(addr, 24)],
        category: None,
        inbound_blocked: None,
    }
}

/// Review of 22/11/2026, A7: the pairing endpoints were bound once, when "Parear" opened; a network change during the
/// two minutes only gave their filter the new interface list. An address the machine got meanwhile (DHCP, a cable)
/// had no pairing endpoint, so the other machine's "Parear" toward it found nothing until the screen was opened again.
#[test]
fn an_address_the_machine_gets_while_pairing_is_open_gets_its_pairing_endpoint() {
    let Some(real) = primary_ipv4() else {
        println!("SKIP: no IPv4 address with a route on this machine");
        return;
    };
    let m = Machine::new("alfa");
    // A port of its own on the real address (never the installed service's 47821).
    let port = std::net::UdpSocket::bind((real, 0)).expect("a free port").local_addr().expect("its port").port();
    lock(&m.hub.hub.settings).pair_port = port;
    let mut ui = m.client(info(false));
    let id = pairing_id(ui.call(UiOp::PairOpen { device_id: None }));
    let before = m.hub.pair_local_addrs();
    assert!(before.iter().all(|a| a.ip().is_loopback()), "only the development address so far: {before:?}");

    // The Wi-Fi gets an address while the screen is open.
    m.nets.set(vec![wifi(real)]);
    m.hub.network_changed();
    let on_real = SocketAddr::V4(SocketAddrV4::new(real, port));
    let after = m.hub.pair_local_addrs();
    assert!(after.contains(&on_real), "a pairing endpoint on the new address: {after:?}");
    assert!(before.iter().all(|a| after.contains(a)), "the address that stayed keeps its endpoint");
    assert!(std::net::UdpSocket::bind(on_real).is_err(), "something listens on the new address");

    // The address goes again: its endpoint closes, the one that stayed is untouched.
    m.nets.set(Vec::new());
    m.hub.network_changed();
    assert_eq!(m.hub.pair_local_addrs(), before);
    wait_for("the port on the gone address is free", || std::net::UdpSocket::bind(on_real).ok().map(|_| ()));
    assert!(ui.call(UiOp::PairCancel { pairing_id: id }).is_ok());
    drop(ui);
    m.finish();
}
