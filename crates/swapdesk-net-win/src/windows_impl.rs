//! The Win32 side: `GetAdaptersAddresses` + `GetIfEntry2` for the adapters, `INetworkListManager` for the category and
//! `INetFwPolicy2` for the firewall state. Every call is read-only and works for a standard user and for SYSTEM.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use swapdesk_platform::{ErrorKind, NetCategory, NetInterface, NetInterfaces, PlatformError, Result};
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, NET_FW_PROFILE_TYPE2, NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_PROFILE2_PUBLIC,
    NetFwPolicy2,
};
use windows::Win32::Networking::NetworkListManager::{
    INetworkListManager, NLM_NETWORK_CATEGORY_PRIVATE, NLM_NETWORK_CATEGORY_PUBLIC, NetworkListManager,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::core::GUID;
use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses, GetIfEntry2,
    IP_ADAPTER_ADDRESSES_LH, MIB_IF_ROW2,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, IpDadStatePreferred, SOCKADDR_IN};

use crate::classify::{AdapterFacts, classify};

/// One adapter with the raw facts the classification used (printed by the tests and by diagnostics).
#[derive(Debug, Clone)]
pub struct AdapterReport {
    pub interface: NetInterface,
    pub description: String,
    pub if_type: u32,
    /// `HardwareInterface` of `GetIfEntry2`.
    pub hardware: bool,
}

/// `NetInterfaces` over the live Windows network stack.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsNetInterfaces;

impl NetInterfaces for WindowsNetInterfaces {
    fn list(&self) -> Result<Vec<NetInterface>> {
        Ok(list_adapters()?.into_iter().map(|a| a.interface).collect())
    }
}

fn os_error(op: &'static str, code: i64, detail: &str) -> PlatformError {
    PlatformError { kind: ErrorKind::Os, op, os_code: Some(code), detail: detail.to_string() }
}

/// Every adapter Windows lists, classified, with IPv4 addresses, category and firewall state.
pub fn list_adapters() -> Result<Vec<AdapterReport>> {
    let mut adapters = read_adapters()?;
    // The category and the firewall are best effort: a failure leaves them unknown (None), never a wrong answer.
    let categories = com_scope(|| Ok((network_categories(), firewall_state())));
    if let Ok((cats, fw)) = categories {
        for a in &mut adapters {
            let Some(guid) = a.guid else { continue };
            if let Some(cat) = cats.get(&guid) {
                a.report.interface.category = Some(*cat);
                a.report.interface.inbound_blocked =
                    fw.as_ref().and_then(|f| f.iter().find(|(c, _)| c == cat).map(|(_, b)| *b));
            }
        }
    }
    Ok(adapters.into_iter().map(|a| a.report).collect())
}

struct Raw {
    report: AdapterReport,
    guid: Option<GUID>,
}

fn wide_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: GetAdaptersAddresses fills these fields with NUL-terminated UTF-16 strings that live in the buffer.
    unsafe {
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

fn parse_guid(name: &str) -> Option<GUID> {
    // AdapterName is "{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}".
    let s = name.trim_matches(|c| c == '{' || c == '}');
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let d1 = u32::from_str_radix(parts[0], 16).ok()?;
    let d2 = u16::from_str_radix(parts[1], 16).ok()?;
    let d3 = u16::from_str_radix(parts[2], 16).ok()?;
    let tail = format!("{}{}", parts[3], parts[4]);
    if tail.len() != 16 {
        return None;
    }
    let mut d4 = [0u8; 8];
    for (i, b) in d4.iter_mut().enumerate() {
        *b = u8::from_str_radix(&tail[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(GUID { data1: d1, data2: d2, data3: d3, data4: d4 })
}

fn hardware_flag(if_index: u32) -> bool {
    // SAFETY: MIB_IF_ROW2 is plain data; zeroed is a valid value and GetIfEntry2 only reads the index we set.
    unsafe {
        let mut row: MIB_IF_ROW2 = std::mem::zeroed();
        row.InterfaceIndex = if_index;
        if GetIfEntry2(&mut row) != NO_ERROR {
            return false;
        }
        // Bit 0 of the flags byte is HardwareInterface (netioapi.h: HardwareInterface:1 is the first bit-field).
        row.InterfaceAndOperStatusFlags._bitfield & 0b1 != 0
    }
}

fn read_adapters() -> Result<Vec<Raw>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    // 8-byte aligned buffer; the adapter list can change between the size query and the read, so retry.
    let mut size: u32 = 16 * 1024;
    let mut buf: Vec<u64> = Vec::new();
    let mut last = 0u32;
    for _ in 0..5 {
        buf.resize((size as usize).div_ceil(8), 0);
        // SAFETY: `buf` holds at least `size` bytes and is 8-byte aligned; `size` is updated by the call.
        last = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut size,
            )
        };
        if last != ERROR_BUFFER_OVERFLOW {
            break;
        }
    }
    if last != ERROR_SUCCESS {
        return Err(os_error("GetAdaptersAddresses", i64::from(last), "could not read the adapter list"));
    }

    let mut out = Vec::new();
    let mut cur = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    while !cur.is_null() {
        // SAFETY: `cur` walks the linked list inside `buf`, which outlives this loop; every pointer was written by Windows.
        let a = unsafe { &*cur };
        let friendly_name = wide_to_string(a.FriendlyName);
        let description = wide_to_string(a.Description);
        // SAFETY: AdapterName is a NUL-terminated ANSI string inside the buffer.
        let adapter_name = if a.AdapterName.is_null() {
            String::new()
        } else {
            unsafe { std::ffi::CStr::from_ptr(a.AdapterName.cast()) }.to_string_lossy().into_owned()
        };
        // SAFETY: the union holds {Length, IfIndex} for the structure version GetAdaptersAddresses returns.
        let index = unsafe { a.Anonymous1.Anonymous.IfIndex };
        let hardware = index != 0 && hardware_flag(index);
        let kind = classify(&AdapterFacts {
            if_type: a.IfType,
            friendly_name: &friendly_name,
            description: &description,
            hardware,
        });

        let mac = (a.PhysicalAddressLength == 6).then(|| {
            let mut m = [0u8; 6];
            m.copy_from_slice(&a.PhysicalAddress[..6]);
            m
        });

        let mut ipv4: Vec<(Ipv4Addr, u8)> = Vec::new();
        let mut u = a.FirstUnicastAddress;
        while !u.is_null() {
            // SAFETY: the unicast list is part of the same buffer.
            let ua = unsafe { &*u };
            let sa = ua.Address.lpSockaddr;
            // SAFETY: lpSockaddr points to a sockaddr whose family field is readable; an AF_INET one is a SOCKADDR_IN.
            if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET && ua.DadState == IpDadStatePreferred {
                let sin = unsafe { &*sa.cast::<SOCKADDR_IN>() };
                // SAFETY: reading the S_addr member of the IN_ADDR union.
                let raw = unsafe { sin.sin_addr.S_un.S_addr };
                ipv4.push((Ipv4Addr::from(u32::from_be(raw)), ua.OnLinkPrefixLength));
            }
            u = ua.Next;
        }

        let interface = NetInterface {
            index,
            name: friendly_name,
            kind,
            up: a.OperStatus == IfOperStatusUp,
            mac,
            ipv4,
            category: None,
            inbound_blocked: None,
        };
        out.push(Raw {
            report: AdapterReport { interface, description, if_type: a.IfType, hardware },
            guid: parse_guid(&adapter_name),
        });
        cur = a.Next;
    }
    Ok(out)
}

/// Runs `f` with COM initialised on this thread (multithreaded apartment). If the thread already runs COM in another
/// mode, `f` runs in that apartment.
fn com_scope<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    // SAFETY: balanced with CoUninitialize below when this call initialised (or re-referenced) the apartment.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let owns = hr.is_ok();
    let result = f();
    if owns {
        // SAFETY: pairs the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    result
}

fn network_categories() -> HashMap<GUID, NetCategory> {
    let mut map = HashMap::new();
    // SAFETY: COM calls on interfaces created here; every returned interface is released on drop.
    let mut walk = || -> windows::core::Result<()> {
        unsafe {
            let nlm: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL)?;
            let conns = nlm.GetNetworkConnections()?;
            loop {
                let mut item = [None];
                let mut fetched = 0u32;
                conns.Next(&mut item, Some(&mut fetched))?;
                if fetched == 0 {
                    break;
                }
                let Some(conn) = item[0].take() else { break };
                let Ok(adapter) = conn.GetAdapterId() else { continue };
                let Ok(network) = conn.GetNetwork() else { continue };
                let Ok(cat) = network.GetCategory() else { continue };
                let cat = if cat == NLM_NETWORK_CATEGORY_PUBLIC {
                    NetCategory::Public
                } else if cat == NLM_NETWORK_CATEGORY_PRIVATE {
                    NetCategory::Private
                } else {
                    NetCategory::Domain
                };
                map.insert(adapter, cat);
            }
        }
        Ok(())
    };
    let _ = walk();
    map
}

/// Per profile: true when the firewall blocks every inbound connection (firewall on and "block all incoming" set).
fn firewall_state() -> Option<Vec<(NetCategory, bool)>> {
    // SAFETY: COM calls on an interface created here.
    unsafe {
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_ALL).ok()?;
        let mut out = Vec::new();
        let profiles: [(NetCategory, NET_FW_PROFILE_TYPE2); 3] = [
            (NetCategory::Domain, NET_FW_PROFILE2_DOMAIN),
            (NetCategory::Private, NET_FW_PROFILE2_PRIVATE),
            (NetCategory::Public, NET_FW_PROFILE2_PUBLIC),
        ];
        for (cat, profile) in profiles {
            let enabled = policy.get_FirewallEnabled(profile).ok()?.as_bool();
            let block_all = policy.get_BlockAllInboundTraffic(profile).ok()?.as_bool();
            out.push((cat, enabled && block_all));
        }
        Some(out)
    }
}

/// CPU time (kernel + user) this process has used so far: the counter of the idle-cost measurement (SPEC B6).
pub fn process_cpu_time() -> std::time::Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut create, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle of the current process is always valid and the four FILETIMEs are writable locals.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut create, &mut exit, &mut kernel, &mut user) };
    if ok == 0 {
        return std::time::Duration::ZERO;
    }
    let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    // FILETIME ticks are 100 ns.
    std::time::Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}
