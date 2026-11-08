//! Reads the real interfaces of the Mac that runs the test, classifies them and prints the result (run with
//! `--no-capture` to see it). What can be asserted on any Mac is asserted; what depends on the Mac's hardware is printed.
#![cfg(target_os = "macos")]

use swapdesk_net_mac::{MacNetInterfaces, list_interfaces};
use swapdesk_platform::{IfKind, NetInterfaces};

#[test]
fn lists_this_macs_interfaces_and_approves_only_physical_ones() {
    let reports = list_interfaces().expect("getifaddrs");
    assert!(!reports.is_empty(), "macOS always lists at least the loopback interface");

    println!(
        "{:<4} {:<10} {:<9} {:<3} {:<6} {:<10} {:<22} {:<19} ipv4",
        "idx", "name", "kind", "up", "flags", "sc", "display", "mac"
    );
    for r in &reports {
        let i = &r.interface;
        let mac = i.mac.map(|m| m.map(|b| format!("{b:02x}")).join(":")).unwrap_or_default();
        println!(
            "{:<4} {:<10} {:<9} {:<3} {:<#6x} {:<10} {:<22} {:<19} {:?}",
            i.index,
            i.name,
            format!("{:?}", i.kind),
            i.up,
            r.flags,
            format!("{:?}", r.sc),
            r.display_name.clone().unwrap_or_default(),
            mac,
            i.ipv4
        );
    }

    // Loopback is classified as such.
    assert!(
        reports
            .iter()
            .any(|r| r.interface.kind == IfKind::Loopback && r.interface.ipv4.iter().any(|(ip, _)| ip.is_loopback()))
    );

    for r in &reports {
        let name = r.interface.name.as_str();
        let approved = matches!(r.interface.kind, IfKind::Ethernet | IfKind::WiFi);
        // Software interfaces are never approved, whatever type SystemConfiguration gives them.
        for soft in
            ["utun", "bridge", "vmnet", "vmenet", "awdl", "llw", "anpi", "ap", "lo", "gif", "stf", "ipsec", "ppp"]
        {
            if name.trim_end_matches(|c: char| c.is_ascii_digit()) == soft {
                assert!(!approved, "{name} must not be approved");
            }
        }
        // An approved interface is an `en` interface that SystemConfiguration calls Ethernet or Wi-Fi, with a MAC address.
        if approved {
            assert!(name.starts_with("en"), "{name}");
            assert!(r.sc.is_some(), "{name} is approved without a SystemConfiguration type");
            assert!(r.interface.mac.is_some(), "{name} is approved without a MAC address");
        }
    }

    // The trait and the report agree.
    let listed = MacNetInterfaces.list().expect("trait list");
    assert_eq!(listed.len(), reports.len());
    assert_eq!(
        listed.iter().filter(|i| matches!(i.kind, IfKind::Ethernet | IfKind::WiFi)).count(),
        reports.iter().filter(|r| matches!(r.interface.kind, IfKind::Ethernet | IfKind::WiFi)).count()
    );
}
