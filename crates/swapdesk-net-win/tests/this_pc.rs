//! Reads the real adapters of the PC that runs the test, classifies them and prints the result (run with
//! `--no-capture` to see it). What can be asserted on any Windows PC is asserted; what depends on the PC's hardware
//! is printed.
#![cfg(windows)]

use swapdesk_net_win::{WindowsNetInterfaces, list_adapters};
use swapdesk_platform::{IfKind, NetInterfaces};

#[test]
fn lists_this_pcs_adapters_and_excludes_the_virtual_ones() {
    let adapters = list_adapters().expect("GetAdaptersAddresses");
    assert!(!adapters.is_empty(), "Windows always lists at least the loopback interface");

    println!(
        "{:<4} {:<34} {:<9} {:<3} {:<4} {:<10} {:<8} {:<40} ipv4",
        "idx", "name", "kind", "up", "hw", "category", "fw-block", "driver"
    );
    for a in &adapters {
        let i = &a.interface;
        println!(
            "{:<4} {:<34} {:<9} {:<3} {:<4} {:<10} {:<8} {:<40} {:?}",
            i.index,
            i.name,
            format!("{:?}", i.kind),
            i.up,
            a.hardware,
            format!("{:?}", i.category),
            format!("{:?}", i.inbound_blocked),
            a.description,
            i.ipv4
        );
    }

    // The loopback interface is classified as such.
    assert!(
        adapters
            .iter()
            .any(|a| a.interface.kind == IfKind::Loopback && a.interface.ipv4.iter().any(|(ip, _)| ip.is_loopback()))
    );

    for a in &adapters {
        let name = a.interface.name.to_lowercase();
        let desc = a.description.to_lowercase();
        // Hyper-V, WSL and Docker adapters (vEthernet) are never Ethernet or Wi-Fi, so the guard never approves them.
        if name.starts_with("vethernet") || desc.contains("hyper-v virtual ethernet") {
            assert!(
                !matches!(a.interface.kind, IfKind::Ethernet | IfKind::WiFi),
                "vEthernet adapter {:?} classified as {:?}",
                a.interface.name,
                a.interface.kind
            );
            assert_eq!(a.interface.kind, IfKind::Virtual);
        }
        // Whatever is called Ethernet or Wi-Fi is hardware and carries no virtual marker.
        if matches!(a.interface.kind, IfKind::Ethernet | IfKind::WiFi) {
            assert!(a.hardware, "{:?} is Ethernet/Wi-Fi but not a hardware interface", a.interface.name);
            assert!(!desc.contains("virtual") && !name.contains("vethernet"), "{:?}", a.interface.name);
        }
        // Addresses are usable unicast IPv4 with a prefix.
        for (_, prefix) in &a.interface.ipv4 {
            assert!(*prefix <= 32);
        }
    }
}

#[test]
fn the_trait_object_gives_the_same_list() {
    let n: &dyn NetInterfaces = &WindowsNetInterfaces;
    let a = n.list().unwrap();
    let b = list_adapters().unwrap();
    assert_eq!(a.len(), b.len());
    // The set of physical addresses is stable between two reads in the same second.
    let phys = |v: &[swapdesk_platform::NetInterface]| {
        let mut x: Vec<_> = v
            .iter()
            .filter(|i| matches!(i.kind, IfKind::Ethernet | IfKind::WiFi))
            .map(|i| (i.index, i.ipv4.clone()))
            .collect();
        x.sort_by_key(|(i, _)| *i);
        x
    };
    assert_eq!(phys(&a), phys(&b.iter().map(|r| r.interface.clone()).collect::<Vec<_>>()));
}

#[test]
fn the_process_cpu_counter_moves_when_the_process_works() {
    let before = swapdesk_net_win::process_cpu_time();
    let start = std::time::Instant::now();
    let mut x = 0u64;
    while start.elapsed() < std::time::Duration::from_millis(200) {
        x = std::hint::black_box(x.wrapping_add(1));
    }
    let used = swapdesk_net_win::process_cpu_time() - before;
    println!("busy 200 ms wall -> {used:?} CPU");
    assert!(used >= std::time::Duration::from_millis(100), "{used:?}");
}
