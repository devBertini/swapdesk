//! `bonjour`: `DNSServiceRegister` and `DNSServiceBrowse` for `_swapdesk._udp` through libSystem's `dns_sd.h` (P8.4).
//! The declarations below are written from `dns_sd.h` (stable C API); no crate wraps it here.

use crate::d;
use crate::log::emit;
use std::ffi::{c_char, c_void, CStr, CString};
use std::net::UdpSocket;
use std::time::{Duration, Instant};

type DNSServiceRef = *mut c_void;
type DNSServiceErrorType = i32;

type RegisterReply = unsafe extern "C" fn(
    DNSServiceRef,
    u32,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type BrowseReply = unsafe extern "C" fn(
    DNSServiceRef,
    u32,
    u32,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);

extern "C" {
    fn DNSServiceRegister(
        sd_ref: *mut DNSServiceRef,
        flags: u32,
        interface_index: u32,
        name: *const c_char,
        reg_type: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        callback: RegisterReply,
        context: *mut c_void,
    ) -> DNSServiceErrorType;
    fn DNSServiceBrowse(
        sd_ref: *mut DNSServiceRef,
        flags: u32,
        interface_index: u32,
        reg_type: *const c_char,
        domain: *const c_char,
        callback: BrowseReply,
        context: *mut c_void,
    ) -> DNSServiceErrorType;
    fn DNSServiceRefSockFD(sd_ref: DNSServiceRef) -> i32;
    fn DNSServiceProcessResult(sd_ref: DNSServiceRef) -> DNSServiceErrorType;
    fn DNSServiceRefDeallocate(sd_ref: DNSServiceRef);
}

fn text(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        // SAFETY: dns_sd passes NUL-terminated strings valid for the callback.
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

unsafe extern "C" fn on_register(
    _r: DNSServiceRef,
    flags: u32,
    err: DNSServiceErrorType,
    name: *const c_char,
    reg_type: *const c_char,
    domain: *const c_char,
    _ctx: *mut c_void,
) {
    emit(
        "bonjour.registered",
        err == 0,
        &d!("error_code" => i64::from(err), "name" => text(name), "type" => text(reg_type), "domain" => text(domain), "flags" => u64::from(flags)),
    );
}

unsafe extern "C" fn on_browse(
    _r: DNSServiceRef,
    flags: u32,
    interface: u32,
    err: DNSServiceErrorType,
    name: *const c_char,
    reg_type: *const c_char,
    domain: *const c_char,
    _ctx: *mut c_void,
) {
    // kDNSServiceFlagsAdd = 0x2
    emit(
        "bonjour.browse",
        err == 0,
        &d!("error_code" => i64::from(err), "added" => (flags & 0x2) != 0, "name" => text(name), "type" => text(reg_type), "domain" => text(domain), "interface" => u64::from(interface)),
    );
}

pub fn run(seconds: u64) {
    // A real UDP port, so the advertised service is honest.
    let sock = UdpSocket::bind("0.0.0.0:0");
    let port = sock.as_ref().ok().and_then(|s| s.local_addr().ok()).map_or(0, |a| a.port());
    let name = CString::new("swapdesk-probe").unwrap_or_default();
    let reg_type = CString::new("_swapdesk._udp").unwrap_or_default();

    let mut register_ref: DNSServiceRef = std::ptr::null_mut();
    let mut browse_ref: DNSServiceRef = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the calls; the port is passed in network byte order as dns_sd requires.
    let register_err = unsafe {
        DNSServiceRegister(
            &mut register_ref,
            0,
            0,
            name.as_ptr(),
            reg_type.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            port.to_be(),
            0,
            std::ptr::null(),
            on_register,
            std::ptr::null_mut(),
        )
    };
    emit("bonjour.register_call", register_err == 0, &d!("error_code" => i64::from(register_err), "port" => u64::from(port)));
    // SAFETY: as above.
    let browse_err =
        unsafe { DNSServiceBrowse(&mut browse_ref, 0, 0, reg_type.as_ptr(), std::ptr::null(), on_browse, std::ptr::null_mut()) };
    emit("bonjour.browse_call", browse_err == 0, &d!("error_code" => i64::from(browse_err)));

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        let mut fds = Vec::new();
        for r in [register_ref, browse_ref] {
            if !r.is_null() {
                // SAFETY: r is a live service ref.
                fds.push((r, unsafe { DNSServiceRefSockFD(r) }));
            }
        }
        if fds.is_empty() {
            break;
        }
        let mut polls: Vec<libc::pollfd> =
            fds.iter().map(|(_, fd)| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 }).collect();
        // SAFETY: `polls` is a valid array of pollfd of the given length.
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, 500) };
        if ready > 0 {
            for (i, p) in polls.iter().enumerate() {
                if p.revents & libc::POLLIN != 0 {
                    // SAFETY: the ref is live; callbacks run on this thread.
                    let err = unsafe { DNSServiceProcessResult(fds[i].0) };
                    if err != 0 {
                        emit("bonjour.process", false, &d!("error_code" => i64::from(err)));
                    }
                }
            }
        }
    }
    for r in [register_ref, browse_ref] {
        if !r.is_null() {
            // SAFETY: each ref is deallocated exactly once.
            unsafe { DNSServiceRefDeallocate(r) };
        }
    }
    emit("bonjour.done", true, &d!("seconds" => seconds));
}
