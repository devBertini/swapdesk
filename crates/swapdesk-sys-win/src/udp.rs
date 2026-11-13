//! The UDP ports of the session endpoints (`docs/ACCOUNTS.md` 5.5).
//!
//! * [`bind_exclusive`]: a UDP socket bound with `SO_EXCLUSIVEADDRUSE`. Bound so on the wildcard address, no other
//!   account can bind that port on any address afterwards, so a local user cannot squat an account's slot.
//! * [`udp_port_owner`]: the process that holds a UDP port (the owning process of the UDP table entry), so the service
//!   can name the program when a slot's port is taken.

use std::io;
use std::net::{SocketAddrV4, UdpSocket};
use std::os::windows::io::FromRawSocket;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedUdpTable, MIB_UDPTABLE_OWNER_PID, UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, IN_ADDR, IN_ADDR_0, INVALID_SOCKET, IPPROTO_UDP, SO_EXCLUSIVEADDRUSE, SOCK_DGRAM, SOCKADDR, SOCKADDR_IN,
    SOL_SOCKET, WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSAGetLastError, WSASocketW, bind, closesocket,
    setsockopt,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

fn last_wsa() -> io::Error {
    // SAFETY: reads the calling thread's last Winsock error; no argument.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

/// A UDP socket on `addr` with `SO_EXCLUSIVEADDRUSE` set before the bind.
pub fn bind_exclusive(addr: SocketAddrV4) -> io::Result<UdpSocket> {
    // The standard library starts Winsock on its first socket; one throwaway socket makes sure it has.
    drop(UdpSocket::bind(("127.0.0.1", 0))?);
    // SAFETY: plain arguments; the returned socket is owned below (closed on every failure, else moved into the
    // `UdpSocket`, which closes it on drop).
    let s = unsafe {
        WSASocketW(
            i32::from(AF_INET),
            SOCK_DGRAM,
            IPPROTO_UDP,
            null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if s == INVALID_SOCKET {
        return Err(last_wsa());
    }
    let fail = |e: io::Error| {
        // SAFETY: `s` is the socket made above and not used after this.
        unsafe { closesocket(s) };
        Err(e)
    };
    let one: i32 = 1;
    // SAFETY: `one` lives across the call, and its size is passed.
    let set = unsafe {
        setsockopt(s, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, std::ptr::from_ref(&one).cast(), size_of::<i32>() as i32)
    };
    if set != 0 {
        return fail(last_wsa());
    }
    let sa = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: addr.port().to_be(),
        sin_addr: IN_ADDR { S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(addr.ip().octets()) } },
        sin_zero: [0; 8],
    };
    // SAFETY: `sa` is a valid IPv4 socket address that lives across the call; its size is passed.
    let bound = unsafe { bind(s, std::ptr::from_ref(&sa).cast::<SOCKADDR>(), size_of::<SOCKADDR_IN>() as i32) };
    if bound != 0 {
        return fail(last_wsa());
    }
    // SAFETY: `s` is a valid, bound UDP socket that nothing else owns.
    Ok(unsafe { UdpSocket::from_raw_socket(s as u64) })
}

/// The process that holds the IPv4 UDP port `port`: its ID and the file name of its image (`None` for the name when
/// the process cannot be opened, such as one of another account without the rights).
pub fn udp_port_owner(port: u16) -> Option<(u32, Option<String>)> {
    let mut size: u32 = 0;
    // SAFETY: a null table with a size of 0 asks for the size needed.
    let first = unsafe { GetExtendedUdpTable(null_mut(), &mut size, 0, u32::from(AF_INET), UDP_TABLE_OWNER_PID, 0) };
    if first != ERROR_INSUFFICIENT_BUFFER && first != NO_ERROR {
        return None;
    }
    // A u32 buffer keeps the table's alignment.
    let mut buf = vec![0u32; (size as usize).div_ceil(4) + 16];
    size = (buf.len() * 4) as u32;
    // SAFETY: `buf` holds `size` bytes, aligned for the table.
    let got = unsafe {
        GetExtendedUdpTable(buf.as_mut_ptr().cast(), &mut size, 0, u32::from(AF_INET), UDP_TABLE_OWNER_PID, 0)
    };
    if got != NO_ERROR {
        return None;
    }
    // SAFETY: on success the buffer starts with a `MIB_UDPTABLE_OWNER_PID` whose rows follow it.
    let table = unsafe { &*buf.as_ptr().cast::<MIB_UDPTABLE_OWNER_PID>() };
    // SAFETY: the table holds `dwNumEntries` rows inside the buffer the call filled.
    let rows = unsafe { std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize) };
    let row = rows.iter().find(|r| u16::from_be(r.dwLocalPort as u16) == port)?;
    Some((row.dwOwningPid, image_name(row.dwOwningPid)))
}

fn image_name(pid: u32) -> Option<String> {
    // SAFETY: plain arguments; the handle is closed below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units; `h` is the process handle opened above.
    let ok = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) };
    // SAFETY: `h` is a valid handle, not used after this.
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    path.rsplit('\\').next().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn an_exclusive_wildcard_bind_keeps_the_port_from_everyone_else_and_its_owner_is_named() {
        let held = bind_exclusive(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        // No other bind of that port succeeds, on the wildcard or on a specific address.
        assert!(UdpSocket::bind(("0.0.0.0", port)).is_err());
        assert!(UdpSocket::bind(("127.0.0.1", port)).is_err());
        assert!(bind_exclusive(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).is_err());
        // The holder is this test process.
        let (pid, name) = udp_port_owner(port).expect("an owner");
        assert_eq!(pid, std::process::id());
        let name = name.expect("its image").to_ascii_lowercase();
        assert!(name.ends_with(".exe"), "{name}");
        drop(held);
        assert!(UdpSocket::bind(("127.0.0.1", port)).is_ok(), "free again once dropped");
    }
}
