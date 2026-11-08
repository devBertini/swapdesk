//! A quinn connection over two `ImpairedSocket`s survives 1% loss each way (C7).
//!
//! TLS here is a test shortcut and needs no new crate: the server presents placeholder certificate bytes with a
//! real Ed25519 signing key made by ring, and the client's verifier accepts anything. Nothing about trust is under
//! test; production verification (PROTOCOL.md 2) lives in `swapdesk-trust`.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring as ring_provider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, SignatureScheme};
use swapdesk_netsim::{ImpairedSocket, Impairment};

const ALPN: &[u8] = b"swapdesk-netsim-test";
const PAYLOAD: usize = 1024 * 1024;

#[derive(Debug)]
struct AcceptAll;

impl ServerCertVerifier for AcceptAll {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

#[derive(Debug)]
struct FixedKey(Arc<CertifiedKey>);

impl ResolvesServerCert for FixedKey {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0))
    }
}

fn configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    let provider: Arc<CryptoProvider> = Arc::new(ring_provider::default_provider());
    let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let key = provider
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec())))
        .unwrap();
    let certified = CertifiedKey { cert: vec![CertificateDer::from(vec![0u8; 16])], key, ocsp: None };

    let mut server = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(FixedKey(Arc::new(certified))));
    server.alpn_protocols = vec![ALPN.to_vec()];
    let server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server).unwrap()));

    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAll))
        .with_no_client_auth();
    client.alpn_protocols = vec![ALPN.to_vec()];
    let client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client).unwrap()));
    (server, client)
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8).collect()
}

#[test]
fn a_quinn_connection_survives_one_percent_loss() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let (server_cfg, client_cfg) = configs();
        let lo = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let impairment = Impairment {
            loss: 0.01,
            delay: Duration::from_millis(2),
            jitter: Duration::from_millis(1),
            ..Impairment::default()
        };
        let server_sock = ImpairedSocket::bind(lo, impairment.clone(), 1).unwrap();
        let client_sock = ImpairedSocket::bind(lo, impairment, 2).unwrap();
        let (server_link, client_link) = (server_sock.handle(), client_sock.handle());
        let server = server_sock.into_endpoint(Some(server_cfg)).unwrap();
        let server_addr = server.local_addr().unwrap();
        let mut client = client_sock.into_endpoint(None).unwrap();
        client.set_default_client_config(client_cfg);

        let echo = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let data = recv.read_to_end(2 * PAYLOAD).await.unwrap();
            send.write_all(&data).await.unwrap();
            send.finish().unwrap();
            // Keep the connection until the client has everything.
            let _ = tokio::time::timeout(Duration::from_secs(20), conn.closed()).await;
            server
        });

        let work = async {
            let conn = client.connect(server_addr, "localhost").unwrap().await.unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let data = pattern(PAYLOAD);
            send.write_all(&data).await.unwrap();
            send.finish().unwrap();
            let back = recv.read_to_end(2 * PAYLOAD).await.unwrap();
            assert!(back == data, "echo differs: {} bytes back", back.len());
            conn.close(0u32.into(), b"done");
        };
        tokio::time::timeout(Duration::from_secs(30), work).await.expect("transfer timed out under 1% loss");
        let _server = echo.await.unwrap();

        let (s, c) = (server_link.stats(), client_link.stats());
        assert!(s.dropped_loss + c.dropped_loss > 0, "no packet was lost: {s:?} {c:?}");
        let lost = s.dropped_loss + c.dropped_loss;
        let sent = s.sent + c.sent;
        assert!(lost * 100 < sent * 3, "loss {lost} of {sent} is far from 1%");
    });
}
