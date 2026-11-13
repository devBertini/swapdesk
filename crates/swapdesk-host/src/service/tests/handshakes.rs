//! F4 of the 13/11 security review: neighbours that start handshakes and go quiet before their certificate must not
//! shut every other peer out. The stalled clients here complete the QUIC address validation and then block inside
//! their TLS client-certificate step, so the host's handshake waits for a certificate that never comes.

use std::net::UdpSocket;
use std::sync::Condvar;

use quinn::rustls::SignatureScheme;
use quinn::rustls::client::ResolvesClientCert;
use quinn::rustls::sign::CertifiedKey;
use swapdesk_proto::ALPN;
use swapdesk_trust::tls::{EndpointKind, SERVER_NAME, client_tls_config, quic_client_config, session_client_verifier};

use super::*;

/// A client certificate step that waits until the test lets it go (or 30 s), then offers none.
struct Stalled {
    released: Arc<(Mutex<bool>, Condvar)>,
}

impl std::fmt::Debug for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Stalled")
    }
}

impl ResolvesClientCert for Stalled {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        let (flag, cv) = &*self.released;
        let _ = cv.wait_timeout_while(lock(flag), Duration::from_secs(30), |released| !*released);
        None
    }

    fn only_raw_public_keys(&self) -> bool {
        true
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Neighbours that stall: each one a client endpoint on its own socket at `from`, dialling `host`.
struct Stallers {
    runtime: tokio::runtime::Runtime,
    released: Arc<(Mutex<bool>, Condvar)>,
    endpoints: Vec<quinn::Endpoint>,
}

impl Stallers {
    fn new() -> Self {
        // Every stalled client parks the worker that drives its endpoint: one worker each, and some to spare.
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(40).enable_all().build().unwrap();
        Self { runtime, released: Arc::new((Mutex::new(false), Condvar::new())), endpoints: Vec::new() }
    }

    /// One more handshake that stalls, from 127.0.0.`last` towards `host` (macOS has only 127.0.0.1 on loopback).
    fn stall(&mut self, last: u8, host: SocketAddr, expected: swapdesk_trust::Spki) {
        let _enter = self.runtime.enter();
        let socket = UdpSocket::bind((Ipv4Addr::new(127, 0, 0, last), 0)).expect("a loopback source address");
        let endpoint = swapdesk_trust::endpoint_from_socket(socket, None).expect("client endpoint");
        let me = Identity::generate(0).expect("identity");
        let mut tls = client_tls_config(&me, session_client_verifier(expected), &[ALPN]).expect("tls");
        tls.client_auth_cert_resolver = Arc::new(Stalled { released: Arc::clone(&self.released) });
        let config = quic_client_config(tls, EndpointKind::Session).expect("client config");
        let connecting = endpoint.connect_with(config, host, SERVER_NAME).expect("connect");
        self.runtime.spawn(async move {
            let _ = connecting.await;
        });
        self.endpoints.push(endpoint);
        // A burst of 16 first packets on loopback loses some of them; the cap test needs all of them to arrive.
        std::thread::sleep(Duration::from_millis(30));
    }
}

impl Drop for Stallers {
    fn drop(&mut self) {
        let (flag, cv) = &*self.released;
        *lock(flag) = true;
        cv.notify_all();
        for e in &self.endpoints {
            e.close(0u32.into(), b"");
        }
    }
}

/// A host and a paired viewer, the host's machine caps readable.
fn rig() -> (Machine, Machine, super::super::MachineShared) {
    let shared = super::super::MachineShared::new();
    let kept = shared.clone();
    let host = Machine::build("host-pc", true, false, ALL, move |mut cfg| {
        cfg.machine = Some(shared);
        cfg
    });
    let viewer = Machine::new("viewer-pc", true, false);
    Machine::pair(&host, &viewer, ALL);
    (host, viewer, kept)
}

/// Four neighbours with four stalled handshakes each (16: the machine's whole cap, and 4 per address: each one's
/// whole share) shut every other peer out for as long as the handshake stage lasted.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS loopback holds only 127.0.0.1, and this case needs a source address per neighbour (Windows and Linux run it)"
)]
fn four_addresses_with_four_stalled_handshakes_each_do_not_shut_a_fifth_peer_out() {
    let (host, viewer, shared) = rig();
    let mut stallers = Stallers::new();
    for last in 2..=5 {
        for _ in 0..4 {
            stallers.stall(last, host.addr(), *host.identity.spki());
        }
    }
    wait_for("the stalled handshakes to reach the host", || (shared.handshakes_in_flight() >= 8).then_some(()));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        shared.handshakes_in_flight() <= 4 * super::super::handshakes::PER_ADDRESS,
        "an address holds at most its share of handshakes: {} are in flight",
        shared.handshakes_in_flight()
    );
    let session = viewer.core.connect(host.addr(), host.device(), Intent::View);
    assert!(session.is_ok(), "the fifth peer was shut out: {:?}", session.err());
    drop(session);
    drop(stallers);
    host.core.shutdown();
    viewer.core.shutdown();
}

/// Sixteen neighbours, one stalled handshake each, fill the machine's cap: the peer that follows is let in, and the
/// oldest stalled handshake is the one that gives way.
#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "macOS loopback holds only 127.0.0.1, and this case needs a source address per neighbour (Windows and Linux run it)"
)]
fn a_full_cap_of_stalled_handshakes_gives_way_to_the_next_peer() {
    let (host, viewer, shared) = rig();
    let mut stallers = Stallers::new();
    for last in 2..=17 {
        stallers.stall(last, host.addr(), *host.identity.spki());
        std::thread::sleep(Duration::from_millis(30));
    }
    wait_for("the cap to fill", || (shared.handshakes_in_flight() >= 16).then_some(()));
    let session = viewer.core.connect(host.addr(), host.device(), Intent::View);
    assert!(session.is_ok(), "the peer that followed a full cap was ignored: {:?}", session.err());
    drop(session);
    drop(stallers);
    host.core.shutdown();
    viewer.core.shutdown();
}

/// A stalled handshake ends by itself within the short limit of the unauthenticated stage, not the 10 s of a client.
#[test]
fn a_stalled_handshake_gives_up_its_place_within_the_short_limit() {
    let (host, _viewer, shared) = rig();
    let mut stallers = Stallers::new();
    stallers.stall(1, host.addr(), *host.identity.spki());
    wait_for("the stalled handshake to reach the host", || (shared.handshakes_in_flight() == 1).then_some(()));
    let started = Instant::now();
    wait_for("the place to be given back", || (shared.handshakes_in_flight() == 0).then_some(()));
    let held = started.elapsed();
    assert!(held < Duration::from_secs(4), "the place was held for {held:?}");
    drop(stallers);
    host.core.shutdown();
}
