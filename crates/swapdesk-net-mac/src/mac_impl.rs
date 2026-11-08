//! The macOS side: `getifaddrs` for names, flags, IPv4 addresses and link-layer addresses, SystemConfiguration for each
//! interface's type. Every call is read-only and works for a standard user and for root, without a TCC grant and without
//! touching the network (no Local Network prompt).

use std::collections::HashMap;
use std::ffi::{CStr, c_char};
use std::net::Ipv4Addr;

use objc2_core_foundation::{CFArray, CFRetained};
use objc2_system_configuration::{
    SCNetworkInterface, kSCNetworkInterfaceTypeEthernet, kSCNetworkInterfaceTypeIEEE80211,
};
use swapdesk_platform::{ErrorKind, NetInterface, NetInterfaces, PlatformError, Result};

use crate::classify::{InterfaceFacts, ScKind, classify, is_up};

/// One interface with the raw facts the classification used (printed by the tests and by diagnostics).
#[derive(Debug, Clone)]
pub struct InterfaceReport {
    pub interface: NetInterface,
    /// SystemConfiguration's type for this BSD name; `None` when it does not list the interface.
    pub sc: Option<ScKind>,
    /// SystemConfiguration's localized display name ("Wi-Fi", "Thunderbolt Bridge"), when it lists the interface.
    pub display_name: Option<String>,
    /// `ifa_flags` of `getifaddrs`.
    pub flags: u32,
}

/// `NetInterfaces` over the live macOS network stack.
#[derive(Debug, Default, Clone, Copy)]
pub struct MacNetInterfaces;

impl NetInterfaces for MacNetInterfaces {
    fn list(&self) -> Result<Vec<NetInterface>> {
        Ok(list_interfaces()?.into_iter().map(|r| r.interface).collect())
    }
}

fn os_error(op: &'static str, code: i64, detail: &str) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: Some(code), detail: detail.to_string() }
}

/// Every interface macOS lists, classified, with IPv4 addresses and MAC address.
pub fn list_interfaces() -> Result<Vec<InterfaceReport>> {
    let sc = system_configuration_types();
    let mut raw = read_getifaddrs()?;
    raw.sort_by_key(|r| r.index);
    Ok(raw
        .into_iter()
        .map(|r| {
            let (sc_kind, display_name) = match sc.get(&r.name) {
                Some((k, d)) => (Some(*k), d.clone()),
                None => (None, None),
            };
            let kind = classify(&InterfaceFacts { name: &r.name, sc: sc_kind, flags: r.flags });
            let interface = NetInterface {
                index: r.index,
                name: r.name,
                kind,
                up: is_up(r.flags),
                mac: r.mac,
                ipv4: r.ipv4,
                category: None,
                inbound_blocked: None,
            };
            InterfaceReport { interface, sc: sc_kind, display_name, flags: r.flags }
        })
        .collect())
}

struct Raw {
    name: String,
    index: u32,
    flags: u32,
    mac: Option<[u8; 6]>,
    ipv4: Vec<(Ipv4Addr, u8)>,
}

/// Frees the list `getifaddrs` allocated.
struct IfAddrs(*mut libc::ifaddrs);

impl Drop for IfAddrs {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful `getifaddrs` and is freed exactly once, here.
        unsafe { libc::freeifaddrs(self.0) };
    }
}

fn read_getifaddrs() -> Result<Vec<Raw>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a valid out-pointer; on success the list it receives is owned by `IfAddrs`.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        let e = std::io::Error::last_os_error();
        return Err(os_error(
            "getifaddrs",
            i64::from(e.raw_os_error().unwrap_or(0)),
            "could not read the interface list",
        ));
    }
    let list = IfAddrs(head);

    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Raw> = Vec::new();
    let mut cur = list.0;
    while !cur.is_null() {
        // SAFETY: `cur` walks the linked list that `list` owns and keeps alive until the end of this function.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_name.is_null() {
            continue;
        }
        // SAFETY: `ifa_name` is a NUL-terminated C string inside the list.
        let name = unsafe { CStr::from_ptr(ifa.ifa_name.cast::<c_char>()) }.to_string_lossy().into_owned();
        let slot = *by_name.entry(name.clone()).or_insert_with(|| {
            // SAFETY: `ifa_name` is the NUL-terminated name read above and outlives the call.
            let index = unsafe { libc::if_nametoindex(ifa.ifa_name) };
            out.push(Raw { name, index, flags: ifa.ifa_flags, mac: None, ipv4: Vec::new() });
            out.len() - 1
        });
        let entry = &mut out[slot];
        if ifa.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: a non-null `ifa_addr` points to a sockaddr whose family field is readable.
        let family = i32::from(unsafe { (*ifa.ifa_addr).sa_family });
        if family == libc::AF_INET {
            // SAFETY: an AF_INET sockaddr is a `sockaddr_in`.
            let addr =
                Ipv4Addr::from(u32::from_be(unsafe { (*ifa.ifa_addr.cast::<libc::sockaddr_in>()).sin_addr.s_addr }));
            let prefix = if ifa.ifa_netmask.is_null() {
                32
            } else {
                // SAFETY: the netmask of an AF_INET address is a `sockaddr_in` too; only `sin_addr` is read.
                let mask = u32::from_be(unsafe { (*ifa.ifa_netmask.cast::<libc::sockaddr_in>()).sin_addr.s_addr });
                // A contiguous mask has this many leading ones.
                u8::try_from(mask.leading_ones()).unwrap_or(32)
            };
            entry.ipv4.push((addr, prefix));
        } else if family == libc::AF_LINK {
            entry.mac = link_address(ifa.ifa_addr.cast::<libc::sockaddr_dl>());
        }
    }
    Ok(out)
}

/// The 6-byte link-layer address of a `sockaddr_dl`, when it has one (Ethernet and Wi-Fi do; loopback and tunnels do not).
fn link_address(sdl: *const libc::sockaddr_dl) -> Option<[u8; 6]> {
    // SAFETY: `sdl` points to the AF_LINK sockaddr that `getifaddrs` returned; its fixed fields are readable.
    let (nlen, alen) = unsafe { (usize::from((*sdl).sdl_nlen), usize::from((*sdl).sdl_alen)) };
    if alen != 6 {
        return None;
    }
    // The name and the address follow `sdl_data` and run past its declared 12 bytes inside the same allocation
    // (`sdl_len` covers them), so the offset is taken from the start of the structure.
    let offset = std::mem::offset_of!(libc::sockaddr_dl, sdl_data) + nlen;
    let mut mac = [0u8; 6];
    // SAFETY: `sdl_len` is at least `offset + 6` for a `sockaddr_dl` with `nlen` name bytes and `alen == 6` address
    // bytes, and the whole sockaddr lives in the `getifaddrs` allocation.
    unsafe {
        let base = sdl.cast::<u8>();
        for (i, b) in mac.iter_mut().enumerate() {
            *b = *base.add(offset + i);
        }
    }
    Some(mac)
}

/// BSD name to SystemConfiguration's type and display name. Empty when SystemConfiguration cannot be read, which makes
/// every `en` interface unlisted and therefore refused (fail closed).
fn system_configuration_types() -> HashMap<String, (ScKind, Option<String>)> {
    let mut map = HashMap::new();
    let all: CFRetained<CFArray> = SCNetworkInterface::all();
    // SAFETY: `SCNetworkInterfaceCopyAll` documents an array of `SCNetworkInterfaceRef`.
    let all = unsafe { CFRetained::cast_unchecked::<CFArray<SCNetworkInterface>>(all) };
    // SAFETY: reading the two exported constants of SystemConfiguration, which live for the whole process.
    let (ethernet, wifi) =
        unsafe { (kSCNetworkInterfaceTypeEthernet.to_string(), kSCNetworkInterfaceTypeIEEE80211.to_string()) };
    for item in all.iter() {
        let Some(bsd) = item.bsd_name() else { continue };
        let kind = match item.interface_type().map(|t| t.to_string()) {
            Some(t) if t == ethernet => ScKind::Ethernet,
            Some(t) if t == wifi => ScKind::Wifi,
            _ => ScKind::Other,
        };
        map.insert(bsd.to_string(), (kind, item.localized_display_name().map(|n| n.to_string())));
    }
    map
}
