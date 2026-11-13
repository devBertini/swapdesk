//! Two `mdns-sd` daemons on this PC, bound to this PC's real approved interface (physical Ethernet or Wi-Fi).
//!
//! Both instances live on one machine, so they share its real address and the guard would call each other "own address".
//! The instance that plays the remote device therefore announces another address of the same subnet (its view of the
//! interface list rewrites the address); the guard rules themselves are the production ones. If the PC has no approved
//! interface (no Ethernet or Wi-Fi up with an IPv4 address) the tests print a SKIP line and pass.
#![cfg(windows)]

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use swapdesk_lan::discovery::{Config, Discovery, DiscoveryEvent, Peer};
use swapdesk_lan::guard::approved_addrs;
use swapdesk_lan::txt::Txt;
use swapdesk_net_win::WindowsNetInterfaces;
use swapdesk_platform::{IfKind, NetInterface, NetInterfaces, Result};
use swapdesk_proto::Os;

/// The real list, optionally altered for the instance that plays "another machine".
struct Nets {
    real: WindowsNetInterfaces,
    /// Replace this address by that one on every interface (a test changes it to play a DHCP change).
    rewrite: Mutex<Option<(Ipv4Addr, Ipv4Addr)>>,
    /// Pretend every physical interface is virtual (nothing may be announced).
    all_virtual: bool,
}

impl NetInterfaces for Nets {
    fn list(&self) -> Result<Vec<NetInterface>> {
        let mut list = self.real.list()?;
        for i in &mut list {
            if self.all_virtual && matches!(i.kind, IfKind::Ethernet | IfKind::WiFi) {
                i.kind = IfKind::Virtual;
            }
            if let Some((from, to)) = *self.rewrite.lock().unwrap() {
                for (a, _) in &mut i.ipv4 {
                    if *a == from {
                        *a = to;
                    }
                }
            }
        }
        Ok(list)
    }
}

fn real_nets() -> Arc<dyn NetInterfaces> {
    Arc::new(Nets { real: WindowsNetInterfaces, rewrite: Mutex::new(None), all_virtual: false })
}

/// An unused-looking neighbour address in the subnet of the first approved address, or None (skip) when there is none.
fn setup() -> Option<(Ipv4Addr, Ipv4Addr)> {
    let list = WindowsNetInterfaces.list().ok()?;
    let approved = approved_addrs(&list);
    let Some(first) = approved.first() else {
        println!("SKIP: no approved interface (physical Ethernet or Wi-Fi, up, with an IPv4 address) on this machine");
        return None;
    };
    println!(
        "approved interfaces: {:?}",
        approved.iter().map(|a| format!("{} {}/{}", a.if_name, a.addr, a.prefix)).collect::<Vec<_>>()
    );
    let [a, b, c, d] = first.addr.octets();
    let fake = Ipv4Addr::new(a, b, c, if d == 249 { 248 } else { 249 });
    Some((first.addr, fake))
}

fn txt(name: &str, fp: u8, os: Os) -> Txt {
    Txt { name: name.into(), os, fingerprint: [fp; 16], pairing: false }
}

fn wait_for(
    d: &Discovery,
    within: Duration,
    mut pred: impl FnMut(&DiscoveryEvent) -> bool,
) -> Option<(DiscoveryEvent, Duration)> {
    let start = Instant::now();
    while start.elapsed() < within {
        if let Some(ev) = d.next_event(Duration::from_millis(100))
            && pred(&ev)
        {
            return Some((ev, start.elapsed()));
        }
    }
    None
}

fn found(ev: &DiscoveryEvent, fp: u8) -> Option<&Peer> {
    match ev {
        DiscoveryEvent::Found(p) if p.txt.fingerprint == [fp; 16] => Some(p),
        _ => None,
    }
}

/// Each test's private mDNS port, below Windows' dynamic range (49152 and up): Hyper-V and WinNAT reserve blocks of
/// that range at each boot, and an mDNS port inside such a block binds nothing, so every peer goes unseen (and a test of
/// absence would pass without testing). The port must bind now, or the test stops on that.
fn mdns_port(n: u16) -> u16 {
    let port = 45353 + n;
    if let Err(e) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
        panic!("the test's mDNS port {port} cannot be bound on this machine: {e}");
    }
    port
}

fn config(mdns_port: u16, service_port: u16) -> Config {
    Config { mdns_port, service_port, ..Config::default() }
}

#[test]
fn two_instances_discover_each_other_on_an_approved_interface_within_5_seconds() {
    let Some((real, fake)) = setup() else { return };
    let a_nets = real_nets();
    let b_nets: Arc<dyn NetInterfaces> =
        Arc::new(Nets { real: WindowsNetInterfaces, rewrite: Mutex::new(Some((real, fake))), all_virtual: false });

    let port = mdns_port(0);
    let a = Discovery::start(a_nets, txt("Alpha", 0xA1, Os::Windows), config(port, 47820)).unwrap();
    let b = Discovery::start(b_nets, txt("Beta", 0xB2, Os::MacOs), config(port, 47822)).unwrap();
    assert!(!a.applied().is_empty() && !b.applied().is_empty());

    let t0 = Instant::now();
    let (ev_a, took_a) =
        wait_for(&a, Duration::from_secs(5), |e| found(e, 0xB2).is_some()).expect("A did not find B within 5 s");
    let peer_b = found(&ev_a, 0xB2).unwrap();
    assert_eq!(peer_b.txt.name, "Beta");
    assert_eq!(peer_b.txt.os, Os::MacOs);
    assert!(!peer_b.txt.pairing);
    assert_eq!(peer_b.port, 47822);
    assert!(peer_b.links.iter().any(|l| l.remote == fake && l.local == real), "{:?}", peer_b.links);

    // B's own copy of A: A announces the real address, B's rewritten list has the fake one, so A is "another machine".
    let (ev_b, took_b) =
        wait_for(&b, Duration::from_secs(5), |e| found(e, 0xA1).is_some()).expect("B did not find A within 5 s");
    let peer_a = found(&ev_b, 0xA1).unwrap();
    assert_eq!((peer_a.txt.name.as_str(), peer_a.txt.os, peer_a.port), ("Alpha", Os::Windows, 47820));
    println!(
        "discovery: A found B in {took_a:?}, B found A in {took_b:?} (since start of the wait {:?})",
        t0.elapsed()
    );

    // Pairing mode changes the record (PROTOCOL.md 3: pair=1).
    a.set_pairing(true).unwrap();
    let (ev, _) = wait_for(&b, Duration::from_secs(5), |e| found(e, 0xA1).is_some_and(|p| p.txt.pairing))
        .expect("B never saw pair=1");
    assert!(found(&ev, 0xA1).unwrap().txt.pairing);

    // A goodbye makes the peer disappear.
    let t = Instant::now();
    a.shutdown();
    let lost = wait_for(
        &b,
        Duration::from_secs(10),
        |e| matches!(e, DiscoveryEvent::Lost { fingerprint } if *fingerprint == [0xA1; 16]),
    );
    println!("goodbye seen after {:?}", t.elapsed());
    assert!(lost.is_some(), "B did not see A leave");
    b.shutdown();
}

#[test]
fn an_instance_never_reports_itself_nor_a_peer_at_its_own_address() {
    let Some((_real, _fake)) = setup() else { return };
    // Both instances share this machine's real address: each answer is "own address" for the guard.
    let port = mdns_port(1);
    let a = Discovery::start(real_nets(), txt("Alpha", 0xA1, Os::Windows), config(port, 47820)).unwrap();
    let b = Discovery::start(real_nets(), txt("Beta", 0xB2, Os::Windows), config(port, 47822)).unwrap();
    assert!(
        wait_for(&a, Duration::from_secs(4), |_| true).is_none(),
        "A reported a peer that sits at this machine's own address"
    );
    assert!(
        wait_for(&b, Duration::from_secs(1), |_| true).is_none(),
        "B reported a peer that sits at this machine's own address"
    );
    a.shutdown();
    b.shutdown();
}

#[test]
fn nothing_is_announced_or_browsed_when_no_interface_is_approved() {
    let Some((real, fake)) = setup() else { return };
    let port = mdns_port(2);
    let a = Discovery::start(real_nets(), txt("Alpha", 0xA1, Os::Windows), config(port, 47820)).unwrap();
    // B sees the same PC but every physical interface looks virtual: the guard approves nothing.
    let b_nets: Arc<dyn NetInterfaces> =
        Arc::new(Nets { real: WindowsNetInterfaces, rewrite: Mutex::new(Some((real, fake))), all_virtual: true });
    let b = Discovery::start(b_nets, txt("Beta", 0xB2, Os::Windows), config(port, 47822)).unwrap();
    assert!(b.applied().is_empty());
    assert!(
        wait_for(&a, Duration::from_secs(4), |e| found(e, 0xB2).is_some()).is_none(),
        "an unapproved instance was announced"
    );
    assert!(wait_for(&b, Duration::from_secs(1), |_| true).is_none(), "an unapproved instance browsed");
    a.shutdown();
    b.shutdown();
}

#[test]
fn discovery_also_works_on_the_standard_mdns_port_beside_the_windows_responder() {
    let Some((real, fake)) = setup() else { return };
    let b_nets: Arc<dyn NetInterfaces> =
        Arc::new(Nets { real: WindowsNetInterfaces, rewrite: Mutex::new(Some((real, fake))), all_virtual: false });
    let a = Discovery::start(
        real_nets(),
        txt("Alpha", 0xA1, Os::Windows),
        Config { service_port: 47820, ..Config::default() },
    )
    .unwrap();
    let b = Discovery::start(b_nets, txt("Beta", 0xB2, Os::MacOs), Config { service_port: 47822, ..Config::default() })
        .unwrap();
    let (_, took) =
        wait_for(&a, Duration::from_secs(5), |e| found(e, 0xB2).is_some()).expect("A did not find B on port 5353");
    println!("port 5353: A found B in {took:?}");
    a.shutdown();
    b.shutdown();
}

#[test]
fn an_address_change_starts_a_fresh_daemon_and_both_sides_follow_the_new_address() {
    // The field case (09/11): DHCP moved a running machine to another address. With mdns-sd's own re-check off, the old
    // daemon kept the lost address and never joined the mDNS group through the new one; a fresh daemon has no such
    // memory. (On one PC the OS address does not change, so this proves the replacement and that peers follow it; the
    // deafness itself shows only on a real address change.)
    let Some((real, fake)) = setup() else { return };
    let [a, b, c, d] = fake.octets();
    let moved = Ipv4Addr::new(a, b, c, if d == 247 { 246 } else { 247 });
    let b_nets =
        Arc::new(Nets { real: WindowsNetInterfaces, rewrite: Mutex::new(Some((real, fake))), all_virtual: false });
    let port = mdns_port(3);
    let a = Discovery::start(real_nets(), txt("Alpha", 0xA1, Os::Windows), config(port, 47820)).unwrap();
    let b = Discovery::start(b_nets.clone(), txt("Beta", 0xB2, Os::MacOs), config(port, 47822)).unwrap();
    wait_for(&a, Duration::from_secs(5), |e| found(e, 0xB2).is_some_and(|p| p.links.iter().any(|l| l.remote == fake)))
        .expect("A did not find B at its first address");
    wait_for(&b, Duration::from_secs(5), |e| found(e, 0xA1).is_some()).expect("B did not find A");
    assert_eq!(b.daemons_started(), 1);
    assert!(!b.refresh().unwrap(), "nothing changed, nothing is rebuilt");
    assert_eq!(b.daemons_started(), 1);

    // B's address changes: a refresh replaces its daemon, A sees B at the new address, and B finds A again.
    *b_nets.rewrite.lock().unwrap() = Some((real, moved));
    assert!(b.refresh().unwrap());
    assert_eq!(b.daemons_started(), 2, "an address change starts a fresh daemon");
    assert!(b.applied().iter().any(|x| x.addr == moved));
    let (_, took) = wait_for(&a, Duration::from_secs(5), |e| {
        found(e, 0xB2).is_some_and(|p| p.links.iter().any(|l| l.remote == moved))
    })
    .expect("A did not follow B to its new address");
    let (_, again) =
        wait_for(&b, Duration::from_secs(5), |e| found(e, 0xA1).is_some()).expect("B's new daemon did not find A");
    println!("after the change: A saw B's new address in {took:?}, B found A again in {again:?}");
    a.shutdown();
    b.shutdown();
}
