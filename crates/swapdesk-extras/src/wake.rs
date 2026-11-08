//! The Wake-on-LAN sender (F-WAKE-01). The packet and the route come from `swapdesk-lan` (`magic_packet`,
//! `wake_route`: the broadcast address of the approved interface that holds the device's last address, never taken
//! from the peer). This module sends it through a [`WakeSocket`] to UDP port 9, repeats it every few seconds, and
//! gives up after 60 s ("Acordando", then connect when the device appears).

use std::future::Future;
use std::io;
use std::net::{SocketAddrV4, UdpSocket};
use std::time::Duration;

use swapdesk_lan::socket::bind_quic_socket;
use swapdesk_lan::wol::magic_packet;
pub use swapdesk_lan::wol::{WOL_PORT, WakeRoute, wake_route};
use tokio::time::Instant;

/// Where datagrams go. The real one is [`UdpWake`]; tests use a recording fake.
pub trait WakeSocket {
    fn send_to(&mut self, packet: &[u8], dest: SocketAddrV4) -> io::Result<()>;
}

/// A UDP socket bound to the route's local address with TTL 1 (read back, from `bind_quic_socket`) and broadcast on.
pub struct UdpWake(UdpSocket);

impl UdpWake {
    pub fn bind(route: &WakeRoute) -> io::Result<Self> {
        let socket = bind_quic_socket(SocketAddrV4::new(route.local, 0))?;
        socket.set_broadcast(true)?;
        Ok(Self(socket))
    }
}

impl WakeSocket for UdpWake {
    fn send_to(&mut self, packet: &[u8], dest: SocketAddrV4) -> io::Result<()> {
        self.0.send_to(packet, dest).map(|_| ())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WakeConfig {
    /// Give up after this long (60 s).
    pub give_up: Duration,
    /// Send the packets again this often while waiting.
    pub resend: Duration,
    /// Ask whether the device appeared this often.
    pub poll: Duration,
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self { give_up: Duration::from_secs(60), resend: Duration::from_secs(5), poll: Duration::from_secs(1) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    Appeared,
    GaveUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeError {
    /// No MAC is stored for the device.
    NoMacs,
    /// Every send failed on the first round.
    Send(io::ErrorKind),
}

/// Sends one magic packet per MAC (duplicates once) to the route's broadcast address. Returns how many went out.
pub fn send_round<S: WakeSocket>(socket: &mut S, route: &WakeRoute, macs: &[[u8; 6]]) -> Result<usize, io::ErrorKind> {
    let dest = SocketAddrV4::new(route.broadcast, WOL_PORT);
    let mut sent = 0;
    let mut last_err = None;
    for (i, mac) in macs.iter().enumerate() {
        if macs[..i].contains(mac) {
            continue;
        }
        match socket.send_to(&magic_packet(*mac), dest) {
            Ok(()) => sent += 1,
            Err(e) => last_err = Some(e.kind()),
        }
    }
    match (sent, last_err) {
        (0, Some(kind)) => Err(kind),
        _ => Ok(sent),
    }
}

/// Wakes a device and waits for it: sends at once, repeats every `resend`, asks `appeared` every `poll`, and ends
/// with `GaveUp` after `give_up`. `on_waiting` is called once when the packets are out (the UI shows "Acordando").
pub async fn wake_and_wait<S, F, Fut>(
    socket: &mut S,
    route: &WakeRoute,
    macs: &[[u8; 6]],
    cfg: WakeConfig,
    mut appeared: F,
    on_waiting: impl FnOnce(),
) -> Result<WakeOutcome, WakeError>
where
    S: WakeSocket,
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    if macs.is_empty() {
        return Err(WakeError::NoMacs);
    }
    let start = Instant::now();
    send_round(socket, route, macs).map_err(WakeError::Send)?;
    on_waiting();
    let mut next_send = start + cfg.resend;
    loop {
        if appeared().await {
            return Ok(WakeOutcome::Appeared);
        }
        let now = Instant::now();
        if now.duration_since(start) >= cfg.give_up {
            return Ok(WakeOutcome::GaveUp);
        }
        tokio::time::sleep(cfg.poll).await;
        if Instant::now() >= next_send {
            // A failed repeat is not fatal: the first round went out.
            let _ = send_round(socket, route, macs);
            next_send = Instant::now() + cfg.resend;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::net::Ipv4Addr;
    use swapdesk_fakes::FakeNetInterfaces;
    use swapdesk_platform::NetInterfaces;

    #[derive(Default)]
    struct FakeSocket {
        sent: Vec<(Vec<u8>, SocketAddrV4)>,
        fail: bool,
    }

    impl WakeSocket for FakeSocket {
        fn send_to(&mut self, packet: &[u8], dest: SocketAddrV4) -> io::Result<()> {
            if self.fail {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            self.sent.push((packet.to_vec(), dest));
            Ok(())
        }
    }

    fn route() -> WakeRoute {
        let list = FakeNetInterfaces::windows_typical().list().unwrap();
        wake_route(&list, Ipv4Addr::new(192, 168, 1, 50)).unwrap()
    }

    const MAC_A: [u8; 6] = [1, 2, 3, 4, 5, 6];
    const MAC_B: [u8; 6] = [7, 8, 9, 10, 11, 12];

    fn run_paused<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().start_paused(true).build().unwrap().block_on(f)
    }

    #[test]
    fn it_sends_to_the_route_broadcast_on_port_9_and_stops_when_the_device_appears() {
        run_paused(async {
            let mut sock = FakeSocket::default();
            let polls = Cell::new(0);
            let waiting = Cell::new(false);
            let out = wake_and_wait(
                &mut sock,
                &route(),
                &[MAC_A, MAC_B, MAC_A],
                WakeConfig::default(),
                || {
                    polls.set(polls.get() + 1);
                    let up = polls.get() >= 8;
                    async move { up }
                },
                || waiting.set(true),
            )
            .await;
            assert_eq!(out, Ok(WakeOutcome::Appeared));
            assert!(waiting.get(), "Acordando was shown");
            let dest = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 255), 9);
            assert!(sock.sent.iter().all(|(_, d)| *d == dest));
            // Round one: two distinct MACs; a repeat after 5 s: two more.
            assert_eq!(sock.sent.len(), 4);
            assert_eq!(sock.sent[0].0, swapdesk_lan::wol::magic_packet(MAC_A).to_vec());
            assert_eq!(sock.sent[1].0, swapdesk_lan::wol::magic_packet(MAC_B).to_vec());
        });
    }

    #[test]
    fn it_gives_up_after_60_seconds() {
        run_paused(async {
            let mut sock = FakeSocket::default();
            let t0 = Instant::now();
            let out =
                wake_and_wait(&mut sock, &route(), &[MAC_A], WakeConfig::default(), || async { false }, || {}).await;
            assert_eq!(out, Ok(WakeOutcome::GaveUp));
            let took = t0.elapsed();
            assert!(took >= Duration::from_secs(60) && took < Duration::from_secs(62), "took {took:?}");
            assert!(sock.sent.len() >= 12, "repeated every 5 s: {}", sock.sent.len());
        });
    }

    #[test]
    fn no_macs_and_a_dead_socket_are_errors() {
        run_paused(async {
            let mut sock = FakeSocket::default();
            assert_eq!(
                wake_and_wait(&mut sock, &route(), &[], WakeConfig::default(), || async { false }, || {}).await,
                Err(WakeError::NoMacs)
            );
            sock.fail = true;
            let r =
                wake_and_wait(&mut sock, &route(), &[MAC_A], WakeConfig::default(), || async { false }, || {}).await;
            assert_eq!(r, Err(WakeError::Send(io::ErrorKind::PermissionDenied)));
        });
    }
}
