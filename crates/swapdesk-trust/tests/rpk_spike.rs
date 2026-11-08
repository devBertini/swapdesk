//! Spike R1: does quinn 0.11 + rustls 0.23.46 (ring) negotiate RFC 7250 raw public keys, mutual, TLS 1.3?

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use ring::signature::{Ed25519KeyPair, KeyPair};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring as ring_provider};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

const ALPN: &[u8] = b"spike/1";
const ED25519_SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

fn make_key(provider: &CryptoProvider) -> (Arc<CertifiedKey>, Vec<u8>) {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut spki = ED25519_SPKI_PREFIX.to_vec();
    spki.extend_from_slice(pair.public_key().as_ref());
    let key = provider
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec())))
        .unwrap();
    println!(
        "signing key public_key() equals spki: {}",
        key.public_key().map(|p| p.as_ref() == spki.as_slice()).unwrap_or(false)
    );
    (Arc::new(CertifiedKey::new(vec![CertificateDer::from(spki.clone())], key)), spki)
}

#[derive(Debug)]
struct Resolver(Arc<CertifiedKey>);
impl ResolvesServerCert for Resolver {
    fn resolve(&self, ch: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        println!(
            "server resolver: alpn = {:?}",
            ch.alpn().map(|a| a.map(|x| String::from_utf8_lossy(x).to_string()).collect::<Vec<_>>())
        );
        Some(self.0.clone())
    }
    fn only_raw_public_keys(&self) -> bool {
        true
    }
}

impl rustls::client::ResolvesClientCert for Resolver {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
    fn only_raw_public_keys(&self) -> bool {
        true
    }
    fn has_certs(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct Accept(Arc<CryptoProvider>);

impl Accept {
    fn verify(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature_with_raw_key(
            m,
            &SubjectPublicKeyInfoDer::from(c.as_ref()),
            d,
            &self.0.signature_verification_algorithms,
        )
    }
}

impl ServerCertVerifier for Accept {
    fn verify_server_cert(
        &self,
        e: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        println!("client verifier saw server end entity of {} bytes", e.len());
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("tls12".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for Accept {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        e: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        println!("server verifier saw client end entity of {} bytes", e.len());
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("tls12".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        true
    }
}

#[test]
fn raw_public_keys_through_quinn() {
    let provider: Arc<CryptoProvider> = Arc::new(ring_provider::default_provider());
    let (skey, sspki) = make_key(&provider);
    let (ckey, cspki) = make_key(&provider);
    let verifier = Arc::new(Accept(provider.clone()));

    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(verifier.clone())
        .with_cert_resolver(Arc::new(Resolver(skey)));
    server.alpn_protocols = vec![ALPN.to_vec()];
    let server = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server).unwrap()));

    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(Resolver(ckey)));
    client.alpn_protocols = vec![ALPN.to_vec()];
    let client = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client).unwrap()));

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let lo = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let ep = quinn::Endpoint::server(server, lo).unwrap();
        let addr = ep.local_addr().unwrap();
        let mut cep = quinn::Endpoint::client(lo).unwrap();
        cep.set_default_client_config(client);
        let srv = tokio::spawn(async move {
            let conn = ep.accept().await.unwrap().await.unwrap();
            let id = conn.peer_identity().unwrap().downcast::<Vec<CertificateDer<'static>>>().unwrap();
            assert_eq!(id[0].as_ref(), cspki.as_slice());
            let (mut s, mut r) = conn.accept_bi().await.unwrap();
            let d = r.read_to_end(100).await.unwrap();
            s.write_all(&d).await.unwrap();
            s.finish().unwrap();
            let mut ek = [0u8; 32];
            conn.export_keying_material(&mut ek, b"EXPERIMENTAL spike", b"").unwrap();
            let _ = conn.closed().await;
            ek
        });
        let conn = cep.connect(addr, "swapdesk").unwrap().await.unwrap();
        let id = conn.peer_identity().unwrap().downcast::<Vec<CertificateDer<'static>>>().unwrap();
        assert_eq!(id[0].as_ref(), sspki.as_slice());
        println!("peer cert/rpk length {}", id[0].len());
        let (mut s, mut r) = conn.open_bi().await.unwrap();
        s.write_all(b"hello").await.unwrap();
        s.finish().unwrap();
        assert_eq!(r.read_to_end(100).await.unwrap(), b"hello");
        let mut ek = [0u8; 32];
        conn.export_keying_material(&mut ek, b"EXPERIMENTAL spike", b"").unwrap();
        conn.close(0u32.into(), b"");
        let sek = srv.await.unwrap();
        assert_eq!(ek, sek, "exporter outputs match");
    });
}
