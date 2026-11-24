//! Sockets kept on one Android network (docs/ANDROID.md 4): the NDK's multinetwork calls (`android/multinetwork.h`,
//! API 23). A network is named by the handle Java reads from `Network.getNetworkHandle()`.
//!
//! Why it is needed: Android routes a socket by the network it is bound to, not by the address it is bound to. A
//! Wi-Fi network without internet is not the default network, so a socket that is merely bound to the Wi-Fi address
//! would still be routed through mobile data.

use std::io;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;

/// `net_handle_t` of `android/multinetwork.h`.
pub type NetHandle = u64;

/// `NETWORK_UNSPECIFIED`: no network; clears a binding.
pub const UNSPECIFIED: NetHandle = 0;

#[link(name = "android")]
unsafe extern "C" {
    /// `int android_setsocknetwork(net_handle_t network, int fd)`: 0 on success, -1 with `errno`.
    fn android_setsocknetwork(network: NetHandle, fd: std::ffi::c_int) -> std::ffi::c_int;
    /// `int android_setprocnetwork(net_handle_t network)`: 0 on success, -1 with `errno`.
    fn android_setprocnetwork(network: NetHandle) -> std::ffi::c_int;
}

/// Binds `socket` to `network`: everything it sends leaves through that network, whatever the process or the system
/// default is. Before the socket is connected or sends.
pub fn bind_socket(network: NetHandle, socket: &UdpSocket) -> io::Result<()> {
    // SAFETY: both arguments are passed by value; the descriptor is this socket's and stays open for the call.
    let rc = unsafe { android_setsocknetwork(network, socket.as_raw_fd()) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Binds every socket this process opens from now on, and has not bound itself, to `network` ([`UNSPECIFIED`]
/// clears it). For the sockets of code that cannot be handed a network, such as the discovery library's.
pub fn bind_process(network: NetHandle) -> io::Result<()> {
    // SAFETY: the argument is passed by value and the call touches no memory of ours.
    let rc = unsafe { android_setprocnetwork(network) };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}
