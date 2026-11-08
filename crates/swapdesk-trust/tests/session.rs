//! Flow proofs of PROTOCOL.md 2: pinned keys on the session endpoint, the post-handshake gate, no resumption and
//! no early data (R11), and the ALPN rule of the two endpoints.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use quinn::Endpoint;
use rustls::client::{ClientSessionMemoryCache, ClientSessionStore, Resumption, Tls13ClientSessionValue};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ServerSessionMemoryCache;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use swapdesk_proto::{ALPN, ErrorCode, Os, PAIR_ALPN};
use swapdesk_trust::tls::{
    EndpointKind, SERVER_NAME, client_tls_config, pairing_client_verifier, quic_client_config, quic_server_config,
    server_tls_config, session_client_config, session_client_verifier, session_server_verifier,
};
use swapdesk_trust::{
    DeviceId, Identity, PairedDevice, PinLookup, TrustError, accept_session, connect_session, device_id,
    endpoint_from_socket, gate_session_server, open_session_endpoint,
};

fn pin(on: &Node, who: &Node) {
    on.trust
        .update(|s| {
            s.devices.push(PairedDevice::new_paired(
                *who.identity.spki(),
                who.name.into(),
                Os::Windows,
                "11".into(),
                1,
                None,
            ))
        })
        .unwrap();
}

async fn accept_one(ep: Endpoint, trust: Arc<dyn PinLookup>) -> swapdesk_trust::Result<(quinn::Connection, DeviceId)> {
    let incoming = ep.accept().await.expect("an incoming connection");
    accept_session(incoming, &*trust).await
}

fn session_server(b: &Node) -> (Endpoint, std::net::SocketAddr) {
    let socket = loopback_socket();
    let addr = socket.local_addr().unwrap();
    (open_session_endpoint(socket, &b.identity, b.trust.clone()).unwrap(), addr)
}

/// True when `conn` is unusable: closed, or a stream cannot carry a byte both ways.
async fn conn_is_dead(conn: &quinn::Connection) -> bool {
    let attempt = async {
        let (mut send, mut recv) = conn.open_bi().await.ok()?;
        send.write_all(b"x").await.ok()?;
        send.finish().ok()?;
        recv.read_to_end(16).await.ok()
    };
    !matches!(tokio::time::timeout(Duration::from_secs(2), attempt).await, Ok(Some(_)))
}

#[test]
fn a_pinned_pair_opens_a_session_through_the_gate() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        pin(&a, &b);
        pin(&b, &a);
        let (ep, addr) = session_server(&b);
        let server = tokio::spawn(accept_one(ep, b.trust.clone()));
        let client = client_endpoint();
        let conn = connect_session(&client, &a.identity, addr, *b.identity.spki()).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(b"ping").await.unwrap();
        send.finish().unwrap();

        let (sconn, id) = server.await.unwrap().unwrap();
        assert_eq!(id, device_id(a.identity.spki()));
        let (mut ssend, mut srecv) = sconn.accept_bi().await.unwrap();
        let got = srecv.read_to_end(16).await.unwrap();
        ssend.write_all(&got).await.unwrap();
        ssend.finish().unwrap();
        assert_eq!(recv.read_to_end(16).await.unwrap(), b"ping");
        assert!(sconn.max_datagram_size().is_some(), "the session endpoint has datagrams on");
    });
}

#[test]
fn an_unknown_key_fails_the_session_handshake() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        // B pins nobody, so A's key is unknown.
        let (ep, addr) = session_server(&b);
        let server = tokio::spawn(accept_one(ep, b.trust.clone()));
        let client = client_endpoint();
        let attempt = connect_session(&client, &a.identity, addr, *b.identity.spki()).await;

        let served = server.await.unwrap();
        assert!(
            matches!(served, Err(TrustError::Tls(_))),
            "the server's handshake must fail: {:?}",
            served.as_ref().map(|_| ())
        );
        // TLS 1.3 lets the client finish first; the alert follows. Either way no stream works.
        if let Ok(conn) = attempt {
            assert!(conn_is_dead(&conn).await, "a connection from an unknown key carried data");
        }
    });
}

#[test]
fn a_forgotten_device_fails() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        pin(&a, &b);
        pin(&b, &a);
        let (ep, addr) = session_server(&b);
        let client = client_endpoint();

        // While pinned, the session opens.
        let server = tokio::spawn(accept_one(ep.clone(), b.trust.clone()));
        let conn = connect_session(&client, &a.identity, addr, *b.identity.spki()).await.unwrap();
        assert!(server.await.unwrap().is_ok());
        conn.close(0u32.into(), b"");

        // B forgets A (F-PAIR-04): the next handshake fails.
        let a_id = device_id(a.identity.spki());
        b.trust.update(|s| s.forget(&a_id)).unwrap();
        let server = tokio::spawn(accept_one(ep, b.trust.clone()));
        let attempt = connect_session(&client, &a.identity, addr, *b.identity.spki()).await;
        assert!(server.await.unwrap().is_err());
        if let Ok(conn) = attempt {
            assert!(conn_is_dead(&conn).await);
        }
    });
}

#[test]
fn the_gate_looks_the_key_up_again_after_the_handshake() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        pin(&a, &b);
        pin(&b, &a);
        let (ep, addr) = session_server(&b);
        let handshake = tokio::spawn(async move { ep.accept().await.unwrap().accept().unwrap().await.unwrap() });
        let client = client_endpoint();
        let cconn = connect_session(&client, &a.identity, addr, *b.identity.spki()).await.unwrap();
        let sconn = handshake.await.unwrap();

        // The verifier passed. Between the handshake and the gate the key is removed.
        let a_id = device_id(a.identity.spki());
        b.trust.update(|s| s.forget(&a_id)).unwrap();
        assert!(matches!(gate_session_server(&sconn, &*b.trust), Err(TrustError::UnknownDevice)));
        match tokio::time::timeout(Duration::from_secs(2), cconn.closed())
            .await
            .expect("the gate closes the connection")
        {
            quinn::ConnectionError::ApplicationClosed(c) => {
                assert_eq!(u64::from(c.error_code), u64::from(ErrorCode::UnknownDevice.tag()))
            }
            other => panic!("expected an application close with UNKNOWN_DEVICE, got {other:?}"),
        }
    });
}

#[test]
fn a_client_refuses_a_server_that_is_not_the_device_it_meant_to_reach() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        pin(&b, &a);
        let (ep, addr) = session_server(&b);
        let _server = tokio::spawn(accept_one(ep, b.trust.clone()));
        let impostor_expected = Identity::generate(0).unwrap();
        let attempt = connect_session(&client_endpoint(), &a.identity, addr, *impostor_expected.spki()).await;
        assert!(attempt.is_err(), "the client accepted a key it did not expect");
    });
}

// ---------- R11: no resumption, no early data ----------

#[derive(Debug)]
struct Recording {
    inner: ClientSessionMemoryCache,
    inserted: AtomicUsize,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self { inner: ClientSessionMemoryCache::new(32), inserted: AtomicUsize::new(0) })
    }

    fn tickets(&self) -> usize {
        self.inserted.load(Ordering::SeqCst)
    }
}

impl ClientSessionStore for Recording {
    fn set_kx_hint(&self, n: ServerName<'static>, g: rustls::NamedGroup) {
        self.inner.set_kx_hint(n, g);
    }
    fn kx_hint(&self, n: &ServerName<'_>) -> Option<rustls::NamedGroup> {
        self.inner.kx_hint(n)
    }
    fn set_tls12_session(&self, n: ServerName<'static>, v: rustls::client::Tls12ClientSessionValue) {
        self.inner.set_tls12_session(n, v);
    }
    fn tls12_session(&self, n: &ServerName<'_>) -> Option<rustls::client::Tls12ClientSessionValue> {
        self.inner.tls12_session(n)
    }
    fn remove_tls12_session(&self, n: &ServerName<'static>) {
        self.inner.remove_tls12_session(n);
    }
    fn insert_tls13_ticket(&self, n: ServerName<'static>, v: Tls13ClientSessionValue) {
        self.inserted.fetch_add(1, Ordering::SeqCst);
        self.inner.insert_tls13_ticket(n, v);
    }
    fn take_tls13_ticket(&self, n: &ServerName<'static>) -> Option<Tls13ClientSessionValue> {
        self.inner.take_tls13_ticket(n)
    }
}

/// The session verifier plus a count of full client-certificate verifications: a resumed handshake skips it.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn ClientCertVerifier>,
    calls: Arc<AtomicUsize>,
}

impl ClientCertVerifier for Counting {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }
    fn verify_client_cert(
        &self,
        e: &CertificateDer<'_>,
        i: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.verify_client_cert(e, i, now)
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
    fn requires_raw_public_keys(&self) -> bool {
        self.inner.requires_raw_public_keys()
    }
    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }
}

struct Server {
    ep: Endpoint,
    addr: std::net::SocketAddr,
    verifications: Arc<AtomicUsize>,
}

/// A session server on the production settings, or on a deliberately weakened copy that issues tickets and
/// accepts early data (the control that shows the checks below can see a ticket and a resumption).
fn server_with(b: &Node, trust: Arc<dyn PinLookup>, weakened: bool) -> Server {
    let verifications = Arc::new(AtomicUsize::new(0));
    let verifier = Arc::new(Counting { inner: session_server_verifier(trust), calls: verifications.clone() });
    let mut tls = server_tls_config(&b.identity, verifier, EndpointKind::Session).unwrap();
    if weakened {
        tls.send_tls13_tickets = 2;
        tls.session_storage = ServerSessionMemoryCache::new(64);
        tls.max_early_data_size = u32::MAX;
    }
    let socket = loopback_socket();
    let addr = socket.local_addr().unwrap();
    let ep = endpoint_from_socket(socket, Some(quic_server_config(tls, EndpointKind::Session).unwrap())).unwrap();
    Server { ep, addr, verifications }
}

/// A client that keeps tickets and offers resumption and 0-RTT, the opposite of the production client.
fn hostile_client(a: &Node, expected: [u8; 44], store: Arc<Recording>) -> quinn::ClientConfig {
    let mut tls = client_tls_config(&a.identity, session_client_verifier(expected), &[ALPN]).unwrap();
    tls.resumption = Resumption::store(store);
    tls.enable_early_data = true;
    quic_client_config(tls, EndpointKind::Session).unwrap()
}

/// Connects, makes a stream round trip so post-handshake messages are processed, and closes.
async fn visit(
    client: &Endpoint,
    cfg: quinn::ClientConfig,
    server: &Server,
    trust: &Arc<dyn PinLookup>,
) -> swapdesk_trust::Result<quinn::Connection> {
    let ep = server.ep.clone();
    let t = trust.clone();
    let accepted = tokio::spawn(async move { accept_session(ep.accept().await.unwrap(), &*t).await });
    let connected = client.connect_with(cfg, server.addr, SERVER_NAME).unwrap().await;
    let served = accepted.await.unwrap()?;
    let conn = connected.map_err(|e| TrustError::Tls(e.to_string()))?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| TrustError::Tls(e.to_string()))?;
    send.write_all(b"x").await.unwrap();
    send.finish().unwrap();
    let (mut ss, mut sr) = served.0.accept_bi().await.unwrap();
    let _ = sr.read_to_end(8).await.unwrap();
    ss.write_all(b"y").await.unwrap();
    ss.finish().unwrap();
    let _ = recv.read_to_end(8).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(conn)
}

#[test]
fn tls_resumption_and_0rtt_are_refused() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        pin(&b, &a);
        let trust: Arc<dyn PinLookup> = b.trust.clone();
        let b_spki = *b.identity.spki();
        let client = client_endpoint();

        // Control: a weakened server does issue tickets, and a client holding one resumes (no second
        // verification) and may send early data. So the checks below can tell the difference.
        let weak = server_with(&b, trust.clone(), true);
        // One client configuration for the whole test: rustls resumes only between connections that share the same
        // verifier and credentials objects, so a configuration rebuilt per connection would never resume.
        let store = Recording::new();
        let hostile = hostile_client(&a, b_spki, store.clone());
        let c1 = visit(&client, hostile.clone(), &weak, &trust).await.unwrap();
        assert!(store.tickets() > 0, "control: the weakened server issued no ticket");
        assert_eq!(weak.verifications.load(Ordering::SeqCst), 1);
        c1.close(0u32.into(), b"");
        let c2 = visit(&client, hostile.clone(), &weak, &trust).await.unwrap();
        assert_eq!(
            weak.verifications.load(Ordering::SeqCst),
            1,
            "control: the second handshake was resumed, so the verifier did not run"
        );
        c2.close(0u32.into(), b"");

        // Production: no ticket is ever issued to a client that would keep it.
        let strong = server_with(&b, trust.clone(), false);
        let fresh = Recording::new();
        let c3 = visit(&client, hostile_client(&a, b_spki, fresh.clone()), &strong, &trust).await.unwrap();
        assert_eq!(fresh.tickets(), 0, "the production server issued a ticket");
        assert_eq!(strong.verifications.load(Ordering::SeqCst), 1);
        c3.close(0u32.into(), b"");

        // A client that offers the ticket the weakened server gave it, and 0-RTT, still gets a full handshake.
        assert!(store.tickets() >= 2);
        let ep = strong.ep.clone();
        let t = trust.clone();
        let accepted = tokio::spawn(async move { accept_session(ep.accept().await.unwrap(), &*t).await });
        let connecting = client.connect_with(hostile.clone(), strong.addr, SERVER_NAME).unwrap();
        match connecting.into_0rtt() {
            Ok((conn, accepted_0rtt)) => {
                assert!(!accepted_0rtt.await, "the production server accepted 0-RTT");
                conn.close(0u32.into(), b"");
            }
            Err(connecting) => {
                let conn = connecting.await.unwrap();
                conn.close(0u32.into(), b"");
            }
        }
        assert!(accepted.await.unwrap().is_ok(), "the gate ran and the key is pinned");
        assert_eq!(
            strong.verifications.load(Ordering::SeqCst),
            2,
            "the ticket was not honoured: a full handshake verified the key again"
        );

        // Even if a server resumed, the gate looks the key up again: remove it between two connections.
        let ticketed = Recording::new();
        let hostile = hostile_client(&a, b_spki, ticketed.clone());
        let c = visit(&client, hostile.clone(), &weak, &trust).await.unwrap();
        c.close(0u32.into(), b"");
        let a_id = device_id(a.identity.spki());
        b.trust.update(|s| s.forget(&a_id)).unwrap();
        let before = weak.verifications.load(Ordering::SeqCst);
        let attempt = visit(&client, hostile, &weak, &trust).await;
        assert!(attempt.is_err(), "a forgotten key was let in on a resumed handshake");
        assert_eq!(
            weak.verifications.load(Ordering::SeqCst),
            before,
            "the handshake was resumed, so only the gate could refuse it"
        );
    });
}

// ---------- ALPN ----------

#[test]
fn a_client_hello_offering_both_alpns_is_refused_on_either_endpoint() {
    run(async {
        let a = Node::new("a", TestUi::answering(true));
        let b = Node::new("b", TestUi::answering(true));
        pin(&b, &a);
        let both: &[&[u8]] = &[ALPN, PAIR_ALPN];
        let client = client_endpoint();

        // Session endpoint. A client that offers only `swapdesk/1` works; one that adds the pairing ALPN does not.
        let (ep, addr) = session_server(&b);
        let tls = client_tls_config(&a.identity, pairing_client_verifier(), &[ALPN]).unwrap();
        let ok = tokio::spawn(accept_one(ep.clone(), b.trust.clone()));
        let conn = client
            .connect_with(quic_client_config(tls, EndpointKind::Session).unwrap(), addr, SERVER_NAME)
            .unwrap()
            .await
            .unwrap();
        assert!(ok.await.unwrap().is_ok());
        conn.close(0u32.into(), b"");

        let tls = client_tls_config(&a.identity, pairing_client_verifier(), both).unwrap();
        let refused = tokio::spawn(accept_one(ep.clone(), b.trust.clone()));
        let attempt = client
            .connect_with(quic_client_config(tls, EndpointKind::Session).unwrap(), addr, SERVER_NAME)
            .unwrap()
            .await;
        assert!(
            refused.await.unwrap().is_err(),
            "the session endpoint accepted a ClientHello that also offered the pairing ALPN"
        );
        assert!(attempt.is_err());

        // Any later pairing version is refused too, not only `swapdesk-pair/1`.
        let later: &[&[u8]] = &[ALPN, b"swapdesk-pair/2"];
        let tls = client_tls_config(&a.identity, pairing_client_verifier(), later).unwrap();
        let refused = tokio::spawn(accept_one(ep, b.trust.clone()));
        let attempt = client
            .connect_with(quic_client_config(tls, EndpointKind::Session).unwrap(), addr, SERVER_NAME)
            .unwrap()
            .await;
        assert!(
            refused.await.unwrap().is_err(),
            "the session endpoint accepted a ClientHello that also offered swapdesk-pair/2"
        );
        assert!(attempt.is_err());

        // Pairing endpoint: the same rule the other way round.
        let socket = loopback_socket();
        let addr = socket.local_addr().unwrap();
        let pep = swapdesk_trust::open_pairing_endpoint(socket, &b.identity).unwrap();
        let handshake = |ep: Endpoint| {
            tokio::spawn(async move {
                match ep.accept().await.unwrap().accept() {
                    Ok(connecting) => connecting.await,
                    Err(e) => Err(e),
                }
            })
        };
        let tls = client_tls_config(&a.identity, pairing_client_verifier(), &[PAIR_ALPN]).unwrap();
        let ok = handshake(pep.clone());
        let conn = client
            .connect_with(quic_client_config(tls, EndpointKind::Pairing).unwrap(), addr, SERVER_NAME)
            .unwrap()
            .await
            .unwrap();
        assert!(ok.await.unwrap().is_ok());
        conn.close(0u32.into(), b"");

        let tls = client_tls_config(&a.identity, pairing_client_verifier(), both).unwrap();
        let refused = handshake(pep.clone());
        let attempt = client
            .connect_with(quic_client_config(tls, EndpointKind::Pairing).unwrap(), addr, SERVER_NAME)
            .unwrap()
            .await;
        assert!(
            refused.await.unwrap().is_err(),
            "the pairing endpoint accepted a ClientHello that also offered the session ALPN"
        );
        assert!(attempt.is_err());

        // Symmetric to the session side, which takes any `swapdesk/` major: so does the rule here. A future major
        // offered next to the pairing ALPN is refused as well, not only `swapdesk/1`.
        let future: &[&[u8]] = &[PAIR_ALPN, b"swapdesk/2"];
        let tls = client_tls_config(&a.identity, pairing_client_verifier(), future).unwrap();
        let refused = handshake(pep);
        let attempt = client
            .connect_with(quic_client_config(tls, EndpointKind::Pairing).unwrap(), addr, SERVER_NAME)
            .unwrap()
            .await;
        assert!(
            refused.await.unwrap().is_err(),
            "the pairing endpoint accepted a ClientHello that also offered swapdesk/2"
        );
        assert!(attempt.is_err());
        let _ = session_client_config(&a.identity, *b.identity.spki()).unwrap();
    });
}

// ---------- pairing endpoint limits ----------

#[test]
fn the_pairing_endpoint_allows_one_bidi_stream_no_uni_and_no_datagrams() {
    run(async {
        let b = Node::new("b", TestUi::holding());
        b.open_pairing_mode();
        let server = start_responder(&b);
        let a = Identity::generate(0).unwrap();
        let client = client_endpoint();
        let cfg = swapdesk_trust::tls::pairing_client_config(&a).unwrap();
        let conn = client.connect_with(cfg, server.addr, SERVER_NAME).unwrap().await.unwrap();
        let _first = conn.open_bi().await.unwrap();
        let second = tokio::time::timeout(Duration::from_millis(400), conn.open_bi()).await;
        assert!(second.is_err(), "a second bidirectional stream opened");
        let uni = tokio::time::timeout(Duration::from_millis(400), conn.open_uni()).await;
        assert!(uni.is_err(), "a unidirectional stream opened");
        assert_eq!(conn.max_datagram_size(), None, "datagrams are on at the pairing endpoint");
    });
}
