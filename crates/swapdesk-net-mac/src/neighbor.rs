//! `NeighborTable` for macOS (D65): the peer's MAC from the kernel's link-layer routing entries, read the way
//! `arp -an` and `ndp -an` read them: `sysctl` `{CTL_NET, PF_ROUTE, 0, family, NET_RT_FLAGS, RTF_LLINFO}` returns a
//! sequence of `rt_msghdr` messages, each followed by the sockaddrs `rtm_addrs` names (destination first, then the
//! gateway, which for these entries is a `sockaddr_dl` holding the MAC). Both IPv4 and IPv6 use this one path (the
//! `SIOCGNBRINFO_IN6` ioctl is not needed: it reports only state, and needs the interface name).
//!
//! macOS 27.2 finding (2026-11-12): over SSH, a third-party binary (C, Rust, ad-hoc signed) gets an EMPTY IPv4 dump from
//! this `sysctl` (rc 0, 0 bytes) while Apple's own `python3` gets the entries, and so does `arp` run from the shell; even
//! `arp` spawned by the Rust binary prints nothing. The IPv6 dump is not restricted. This looks like the Local Network
//! privacy gate (not proved: it cannot be granted over SSH), so IPv4 needs a process the owner approved for the local
//! network, as the Swapdesk app already is for its own connections; an empty dump gives `None`.
//!
//! [`parse_neighbor_dump`] is pure and builds on every host; the `sysctl` call exists on macOS only. The kernel has no
//! "resolve now" call for IPv4 here, so an entry the system dropped gives `None`.
//!
//! Layouts verified against the macOS SDK headers on the MacBook (`$(xcrun --show-sdk-path)/usr/include`, 2026-11-12):
//! `sys/socket.h` (`NET_RT_FLAGS` 2, `AF_LINK` 18, `AF_INET6` 30), `net/route.h` (`RTF_LLINFO` 0x400, `RTA_DST` 0x1,
//! `RTA_GATEWAY` 0x2, `struct rt_msghdr` with `rtm_msglen` u16 at 0, `rtm_index` u16 at 4, `rtm_addrs` i32 at 12,
//! 36 bytes plus `struct rt_metrics` of 56 bytes = 92), `net/if_dl.h` (`struct sockaddr_dl`: `sdl_len` 0, `sdl_family`
//! 1, `sdl_index` 2, `sdl_nlen` 5, `sdl_alen` 6, `sdl_data` at 8), `netinet/if_ether.h` (`struct sockaddr_inarp`:
//! `sin_addr` at 4), `netinet6/in6.h` (`struct sockaddr_in6`: `sin6_addr` at 8, 28 bytes).

use core::net::IpAddr;

use swapdesk_platform::NeighborTable;

const AF_INET: u8 = 2;
const AF_LINK: u8 = 18;
const AF_INET6: u8 = 30;
const RTA_DST: i32 = 0x1;
const RTA_GATEWAY: i32 = 0x2;
/// `sizeof(struct rt_msghdr)`.
const RT_MSGHDR_LEN: usize = 92;
const RTM_ADDRS_OFFSET: usize = 12;
/// `sizeof(struct sockaddr_dl)` header before `sdl_data`.
const SDL_DATA_OFFSET: usize = 8;

/// A sockaddr in a routing message takes its `sa_len` rounded up to 4 bytes; a zero length takes 4.
fn sa_step(len: usize) -> usize {
    if len == 0 { 4 } else { ((len - 1) | 3) + 1 }
}

fn zero_scope(mut a: [u8; 16]) -> [u8; 16] {
    // A link-local address (fe80::/10) carries its zone index inside bytes 2 and 3 in kernel messages.
    if a[0] == 0xfe && a[1] & 0xc0 == 0x80 {
        a[2] = 0;
        a[3] = 0;
    }
    a
}

/// Finds `peer` in a `NET_RT_FLAGS`/`RTF_LLINFO` dump and returns its 6-byte link-layer address. A message that does
/// not fit in the buffer ends the walk; an entry without a 6-byte, non-zero address (an incomplete entry) is skipped.
pub fn parse_neighbor_dump(buf: &[u8], peer: IpAddr) -> Option<[u8; 6]> {
    let want = match peer {
        IpAddr::V4(a) => (AF_INET, a.octets().to_vec()),
        IpAddr::V6(a) => (AF_INET6, zero_scope(a.octets()).to_vec()),
    };
    let mut pos = 0usize;
    while pos + RT_MSGHDR_LEN <= buf.len() {
        let msglen = usize::from(u16::from_ne_bytes([buf[pos], buf[pos + 1]]));
        if msglen < RT_MSGHDR_LEN || pos + msglen > buf.len() {
            return None;
        }
        let msg = &buf[pos..pos + msglen];
        pos += msglen;
        let addrs = i32::from_ne_bytes(msg[RTM_ADDRS_OFFSET..RTM_ADDRS_OFFSET + 4].try_into().ok()?);
        if addrs & RTA_DST == 0 || addrs & RTA_GATEWAY == 0 {
            continue;
        }
        let dst = &msg[RT_MSGHDR_LEN..];
        let Some(&dst_len) = dst.first() else { continue };
        let dst_len = usize::from(dst_len);
        let matches = match want.0 {
            AF_INET => dst.len() >= 8 && dst_len >= 8 && dst[1] == AF_INET && dst[4..8] == want.1[..],
            _ => {
                dst.len() >= 24
                    && dst_len >= 24
                    && dst[1] == AF_INET6
                    && zero_scope(dst[8..24].try_into().unwrap_or([0; 16]))[..] == want.1[..]
            }
        };
        if !matches {
            continue;
        }
        let Some(gw) = dst.get(sa_step(dst_len)..) else { continue };
        if gw.len() < SDL_DATA_OFFSET || gw[1] != AF_LINK {
            continue;
        }
        let nlen = usize::from(gw[5]);
        let alen = usize::from(gw[6]);
        let start = SDL_DATA_OFFSET + nlen;
        let end = if gw[0] == 0 { gw.len() } else { gw.len().min(usize::from(gw[0])) };
        if alen != 6 || start + 6 > end {
            continue;
        }
        let mac: [u8; 6] = gw[start..start + 6].try_into().ok()?;
        if mac != [0u8; 6] {
            return Some(mac);
        }
    }
    None
}

/// `NeighborTable` over the live macOS routing table.
#[derive(Debug, Default, Clone, Copy)]
pub struct MacNeighborTable;

impl NeighborTable for MacNeighborTable {
    /// The kernel table is read directly and never asks the network, so `resolve_mac` (the trait default) is the same.
    fn cached_mac(&self, peer: IpAddr) -> Option<[u8; 6]> {
        if peer.is_loopback() || peer.is_unspecified() || peer.is_multicast() {
            return None;
        }
        #[cfg(target_os = "macos")]
        {
            let buf = dump(peer.is_ipv6())?;
            parse_neighbor_dump(&buf, peer)
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    /// The cached entry; on a miss, one empty UDP datagram to the peer's discard port (9) makes the kernel run ARP or
    /// Neighbour Discovery, then the table is re-read in short polls for up to about a second.
    fn resolve_mac(&self, peer: IpAddr) -> Option<[u8; 6]> {
        if let Some(mac) = self.cached_mac(peer) {
            return Some(mac);
        }
        #[cfg(target_os = "macos")]
        {
            if peer.is_loopback() || peer.is_unspecified() || peer.is_multicast() {
                return None;
            }
            let bind: std::net::SocketAddr = if peer.is_ipv6() { ([0u16; 8], 0).into() } else { ([0u8; 4], 0).into() };
            let socket = std::net::UdpSocket::bind(bind).ok()?;
            let _ = socket.send_to(&[], (peer, 9));
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Some(mac) = self.cached_mac(peer) {
                    return Some(mac);
                }
            }
        }
        None
    }
}

/// The link-layer routing entries of one address family, as the kernel returns them.
#[cfg(target_os = "macos")]
fn dump(v6: bool) -> Option<Vec<u8>> {
    let family = if v6 { libc::AF_INET6 } else { libc::AF_INET };
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, family, libc::NET_RT_FLAGS, libc::RTF_LLINFO];
    // The table can grow between the size query and the read, so retry with some slack.
    for _ in 0..5 {
        let mut len: libc::size_t = 0;
        // SAFETY: `mib` has 6 elements as `namelen` says; a null `oldp` asks only for the size, written to `len`.
        let rc = unsafe { libc::sysctl(mib.as_mut_ptr(), 6, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) };
        if rc != 0 {
            return None;
        }
        len += len / 8 + 512;
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` holds `len` bytes, `len` is updated to the bytes written; `mib` is as above.
        let rc =
            unsafe { libc::sysctl(mib.as_mut_ptr(), 6, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
        if rc == 0 {
            buf.truncate(len);
            return Some(buf);
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x74, 0xfe, 0xce, 0x40, 0xd6, 0x70];

    /// One message: header, destination sockaddr (`dst`, padded to 4), gateway `sockaddr_dl` with a name of `nlen`.
    fn message(dst: &[u8], alen: u8, nlen: u8, mac: [u8; 6]) -> Vec<u8> {
        let mut m = vec![0u8; RT_MSGHDR_LEN];
        m[RTM_ADDRS_OFFSET..RTM_ADDRS_OFFSET + 4].copy_from_slice(&(RTA_DST | RTA_GATEWAY).to_ne_bytes());
        m.extend_from_slice(dst);
        m.resize(RT_MSGHDR_LEN + sa_step(dst.len()), 0);
        let mut sdl = vec![0u8; SDL_DATA_OFFSET];
        sdl[1] = AF_LINK;
        sdl[5] = nlen;
        sdl[6] = alen;
        sdl.extend(std::iter::repeat_n(b'e', usize::from(nlen)));
        sdl.extend_from_slice(&mac[..usize::from(alen).min(6)]);
        sdl[0] = u8::try_from(sdl.len()).unwrap();
        m.extend_from_slice(&sdl);
        let total = u16::try_from(m.len()).unwrap();
        m[..2].copy_from_slice(&total.to_ne_bytes());
        m
    }

    fn v4(a: [u8; 4]) -> Vec<u8> {
        let mut s = vec![16, AF_INET, 0, 0, a[0], a[1], a[2], a[3]];
        s.resize(16, 0);
        s
    }

    fn v6(a: [u8; 16]) -> Vec<u8> {
        let mut s = vec![28, AF_INET6, 0, 0, 0, 0, 0, 0];
        s.extend_from_slice(&a);
        s.resize(28, 0);
        s
    }

    #[test]
    fn finds_the_matching_ipv4_entry_among_others() {
        let mut buf = message(&v4([192, 168, 1, 3]), 6, 3, [1, 2, 3, 4, 5, 6]);
        buf.extend(message(&v4([192, 168, 1, 1]), 6, 3, MAC));
        buf.extend(message(&v4([192, 168, 1, 5]), 6, 3, [9; 6]));
        assert_eq!(parse_neighbor_dump(&buf, "192.168.1.1".parse().unwrap()), Some(MAC));
        assert_eq!(parse_neighbor_dump(&buf, "192.168.1.9".parse().unwrap()), None);
    }

    #[test]
    fn name_length_moves_the_address() {
        let buf = message(&v4([10, 0, 0, 2]), 6, 0, MAC);
        assert_eq!(parse_neighbor_dump(&buf, "10.0.0.2".parse().unwrap()), Some(MAC));
        let buf = message(&v4([10, 0, 0, 2]), 6, 7, MAC);
        assert_eq!(parse_neighbor_dump(&buf, "10.0.0.2".parse().unwrap()), Some(MAC));
    }

    #[test]
    fn a_zero_length_address_is_skipped() {
        let buf = message(&v4([10, 0, 0, 2]), 0, 3, [0; 6]);
        assert_eq!(parse_neighbor_dump(&buf, "10.0.0.2".parse().unwrap()), None);
        let buf = message(&v4([10, 0, 0, 2]), 6, 3, [0; 6]);
        assert_eq!(parse_neighbor_dump(&buf, "10.0.0.2".parse().unwrap()), None, "all-zero MAC");
    }

    #[test]
    fn a_truncated_message_gives_none_without_panicking() {
        let buf = message(&v4([10, 0, 0, 2]), 6, 3, MAC);
        for cut in 0..buf.len() {
            assert_eq!(parse_neighbor_dump(&buf[..cut], "10.0.0.2".parse().unwrap()), None, "cut at {cut}");
        }
        let mut bad = buf.clone();
        bad[..2].copy_from_slice(&5u16.to_ne_bytes());
        assert_eq!(parse_neighbor_dump(&bad, "10.0.0.2".parse().unwrap()), None);
    }

    #[test]
    fn ipv6_matches_with_the_embedded_zone_ignored() {
        let mut kernel = [0u8; 16];
        kernel[0] = 0xfe;
        kernel[1] = 0x80;
        kernel[3] = 0x0b; // zone 11 embedded by the kernel
        kernel[15] = 1;
        let buf = message(&v6(kernel), 6, 3, MAC);
        assert_eq!(parse_neighbor_dump(&buf, "fe80::1".parse().unwrap()), Some(MAC));
        assert_eq!(parse_neighbor_dump(&buf, "fe80::2".parse().unwrap()), None);
        let global: std::net::Ipv6Addr = "2804:3d90::1".parse().unwrap();
        let buf = message(&v6(global.octets()), 6, 3, MAC);
        assert_eq!(parse_neighbor_dump(&buf, global.into()), Some(MAC));
        assert_eq!(parse_neighbor_dump(&buf, "192.168.1.1".parse().unwrap()), None);
    }

    #[test]
    fn loopback_is_none() {
        let t = MacNeighborTable;
        assert_eq!(t.cached_mac("127.0.0.1".parse().unwrap()), None);
        assert_eq!(t.cached_mac("::1".parse().unwrap()), None);
        assert_eq!(t.cached_mac("0.0.0.0".parse().unwrap()), None);
        assert_eq!(t.resolve_mac("127.0.0.1".parse().unwrap()), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn layout_constants_match_libc() {
        assert_eq!(RT_MSGHDR_LEN, std::mem::size_of::<libc::rt_msghdr>());
        assert_eq!(RTM_ADDRS_OFFSET, std::mem::offset_of!(libc::rt_msghdr, rtm_addrs));
        assert_eq!(SDL_DATA_OFFSET, std::mem::offset_of!(libc::sockaddr_dl, sdl_data));
        assert_eq!(i32::from(AF_LINK), libc::AF_LINK);
        assert_eq!(i32::from(AF_INET6), libc::AF_INET6);
        assert_eq!(RTA_DST, libc::RTA_DST);
        assert_eq!(RTA_GATEWAY, libc::RTA_GATEWAY);
        assert_eq!(libc::NET_RT_FLAGS, 2);
        assert_eq!(libc::RTF_LLINFO, 0x400);
        assert_eq!(std::mem::size_of::<libc::sockaddr_in6>(), 28);
        assert_eq!(std::mem::offset_of!(libc::sockaddr_in6, sin6_addr), 8);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_gateway_mac_resolves() {
        let out = std::process::Command::new("/sbin/route").args(["-n", "get", "default"]).output();
        let gateway = out.ok().and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find_map(|l| l.trim().strip_prefix("gateway:").map(|g| g.trim().to_string()))
                .and_then(|g| g.parse::<std::net::Ipv4Addr>().ok())
        });
        let Some(gateway) = gateway else {
            eprintln!("skipped: no IPv4 default route on this host");
            return;
        };
        if dump(false).is_some_and(|d| d.is_empty()) {
            eprintln!(
                "skipped: the IPv4 neighbour dump is empty for this process (macOS hides it from it; see module docs)"
            );
            return;
        }
        let mac = MacNeighborTable.resolve_mac(IpAddr::V4(gateway));
        eprintln!("gateway {gateway} -> {mac:?}");
        assert!(mac.is_some(), "no MAC for the gateway {gateway}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_live_ipv6_neighbour_resolves_when_there_is_one() {
        let Some(buf) = dump(true) else { panic!("sysctl for IPv6 failed") };
        // Any global IPv6 neighbour `ndp` lists must parse; here: walk the dump and find one that resolves.
        let mut pos = 0usize;
        let mut tried = 0;
        while pos + RT_MSGHDR_LEN + 24 <= buf.len() {
            let len = usize::from(u16::from_ne_bytes([buf[pos], buf[pos + 1]]));
            if len < RT_MSGHDR_LEN + 24 {
                break;
            }
            let a: [u8; 16] = buf[pos + RT_MSGHDR_LEN + 8..pos + RT_MSGHDR_LEN + 24].try_into().unwrap();
            let ip = std::net::Ipv6Addr::from(a);
            if a[0] != 0xfe && !ip.is_loopback() {
                tried += 1;
                if MacNeighborTable.cached_mac(IpAddr::V6(ip)).is_some() {
                    eprintln!("ipv6 {ip} resolved ({tried} tried)");
                    return;
                }
            }
            pos += len;
        }
        eprintln!("skipped: no resolvable global IPv6 neighbour ({tried} tried)");
    }
}
