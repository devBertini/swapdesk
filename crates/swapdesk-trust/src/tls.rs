//! TLS 1.3 with pinned keys (PROTOCOL.md 2): RFC 7250 raw public keys, Ed25519 only, mutual authentication, no
//! resumption, no early data. One `ServerConfig` per endpoint (session and pairing) with its own verifier.

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{ResolvesClientCert, Resumption};
use rustls::crypto::{CryptoProvider, ring as ring_provider};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, NoServerSessionStorage, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
use swapdesk_proto::{ALPN, PAIR_ALPN};

use crate::error::{Result, TrustError};
use crate::identity::Identity;
use crate::ids::{Spki, parse_spki};
use crate::store::PinLookup;

/// The server name every Swapdesk client sends; it carries no meaning (the key is what is checked).
pub const SERVER_NAME: &str = "swapdesk";

const SESSION_ALPN_PREFIX: &[u8] = b"swapdesk/";
const PAIR_ALPN_PREFIX: &[u8] = b"swapdesk-pair/";

/// The two endpoints (PROTOCOL.md 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    Session,
    Pairing,
}

impl EndpointKind {
    pub const fn alpn(self) -> &'static [u8] {
        match self {
            Self::Session => ALPN,
            Self::Pairing => PAIR_ALPN,
        }
    }

    /// The ALPN of the other endpoint, which a ClientHello here must not offer.
    const fn other_alpn(self) -> &'static [u8] {
        match self {
            Self::Session => PAIR_ALPN,
            Self::Pairing => ALPN,
        }
    }
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring_provider::default_provider())
}

/// The key as rustls wants it for raw public keys: the "certificate" is the 44-byte SPKI.
pub fn certified_key(identity: &Identity, provider: &CryptoProvider) -> Result<Arc<CertifiedKey>> {
    let key = provider
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.pkcs8().to_vec())))
        .map_err(|_| TrustError::Crypto("signing key"))?;
    Ok(Arc::new(CertifiedKey::new(vec![CertificateDer::from(identity.spki().to_vec())], key)))
}

/// Serves and presents the device key as a raw public key, and refuses a ClientHello whose ALPN list breaks the
/// endpoint's rule: it must offer this endpoint's protocol and must not offer the other endpoint's (the resolver is
/// the one place that sees the ClientHello before the server picks).
#[derive(Debug)]
struct Resolver {
    key: Arc<CertifiedKey>,
    kind: EndpointKind,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let offered: Vec<&[u8]> = hello.alpn()?.collect();
        // Each endpoint refuses the other's whole family, not one version of it: the pairing endpoint any
        // `swapdesk/<major>` (as the session side takes any major), the session endpoint any `swapdesk-pair/<n>`.
        let wants_other = match self.kind {
            EndpointKind::Pairing => offered.iter().any(|p| p.starts_with(SESSION_ALPN_PREFIX)),
            EndpointKind::Session => {
                offered.contains(&self.kind.other_alpn()) || offered.iter().any(|p| p.starts_with(PAIR_ALPN_PREFIX))
            }
        };
        let wants_ours = match self.kind {
            EndpointKind::Pairing => offered.contains(&PAIR_ALPN),
            // Future majors ("swapdesk/2") are negotiated by the ALPN list of the config, not here.
            EndpointKind::Session => offered.iter().any(|p| p.starts_with(SESSION_ALPN_PREFIX)),
        };
        (wants_ours && !wants_other).then(|| Arc::clone(&self.key))
    }

    fn only_raw_public_keys(&self) -> bool {
        true
    }
}

impl ResolvesClientCert for Resolver {
    fn resolve(&self, _root_hint_subjects: &[&[u8]], sigschemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        sigschemes.contains(&SignatureScheme::ED25519).then(|| Arc::clone(&self.key))
    }

    fn only_raw_public_keys(&self) -> bool {
        true
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// What a verifier accepts as the peer's key.
#[derive(Clone)]
enum Policy {
    /// Session server: the client's key must be in the trust store, looked up now.
    Pinned(Arc<dyn PinLookup>),
    /// Session client: only the key of the device it meant to reach.
    Expected(Spki),
    /// Pairing endpoint, both sides: any well-formed Ed25519 key (possession is proved by `CertificateVerify`).
    AnyEd25519,
}

impl core::fmt::Debug for Policy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Pinned(_) => "Pinned",
            Self::Expected(_) => "Expected",
            Self::AnyEd25519 => "AnyEd25519",
        })
    }
}

#[derive(Debug)]
struct Verifier {
    provider: Arc<CryptoProvider>,
    policy: Policy,
}

fn bad_key() -> TlsError {
    TlsError::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
}

impl Verifier {
    fn check_key(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
    ) -> core::result::Result<(), TlsError> {
        if !intermediates.is_empty() {
            return Err(bad_key());
        }
        let spki = parse_spki(end_entity.as_ref()).ok_or_else(bad_key)?;
        match &self.policy {
            Policy::Pinned(trust) => trust.pinned(&spki).map(|_| ()).ok_or_else(bad_key),
            Policy::Expected(expected) => (expected == &spki).then_some(()).ok_or_else(bad_key),
            Policy::AnyEd25519 => Ok(()),
        }
    }

    fn check_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> core::result::Result<HandshakeSignatureValid, TlsError> {
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(bad_key());
        }
        rustls::crypto::verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> core::result::Result<ServerCertVerified, TlsError> {
        self.check_key(end_entity, intermediates)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> core::result::Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> core::result::Result<HandshakeSignatureValid, TlsError> {
        self.check_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for Verifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> core::result::Result<ClientCertVerified, TlsError> {
        self.check_key(end_entity, intermediates)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> core::result::Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> core::result::Result<HandshakeSignatureValid, TlsError> {
        self.check_signature(message, cert, dss)
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

/// Verifier of the session endpoint: only keys in the trust store, looked up at every handshake.
pub fn session_server_verifier(trust: Arc<dyn PinLookup>) -> Arc<dyn ClientCertVerifier> {
    Arc::new(Verifier { provider: provider(), policy: Policy::Pinned(trust) })
}

/// Verifier of the pairing endpoint: any well-formed Ed25519 key.
pub fn pairing_server_verifier() -> Arc<dyn ClientCertVerifier> {
    Arc::new(Verifier { provider: provider(), policy: Policy::AnyEd25519 })
}

/// The rustls server configuration of an endpoint, hardened as PROTOCOL.md 2.4 says: TLS 1.3 only, no tickets, a
/// session storage that stores nothing, no ticketer, no early data. Exposed so a test can check each setting.
pub fn server_tls_config(
    identity: &Identity,
    verifier: Arc<dyn ClientCertVerifier>,
    kind: EndpointKind,
) -> Result<rustls::ServerConfig> {
    let provider = provider();
    let key = certified_key(identity, &provider)?;
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TrustError::Tls(e.to_string()))?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(Resolver { key, kind }));
    config.alpn_protocols = vec![kind.alpn().to_vec()];
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.max_early_data_size = 0;
    Ok(config)
}

/// The rustls client configuration: the same hardening on the client side (no resumption, no early data).
pub fn client_tls_config(
    identity: &Identity,
    verifier: Arc<dyn ServerCertVerifier>,
    alpn: &[&[u8]],
) -> Result<rustls::ClientConfig> {
    let provider = provider();
    let key = certified_key(identity, &provider)?;
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TrustError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(Resolver { key, kind: EndpointKind::Session }));
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    config.resumption = Resumption::disabled();
    config.enable_early_data = false;
    Ok(config)
}

/// Verifier a session client uses: only the key of the device it meant to reach.
pub fn session_client_verifier(expected: Spki) -> Arc<dyn ServerCertVerifier> {
    Arc::new(Verifier { provider: provider(), policy: Policy::Expected(expected) })
}

/// Verifier a pairing client uses: any well-formed Ed25519 key.
pub fn pairing_client_verifier() -> Arc<dyn ServerCertVerifier> {
    Arc::new(Verifier { provider: provider(), policy: Policy::AnyEd25519 })
}

fn varint(v: u32) -> quinn::VarInt {
    quinn::VarInt::from_u32(v)
}

fn idle(d: Duration) -> quinn::IdleTimeout {
    quinn::IdleTimeout::try_from(d).unwrap_or_else(|_| quinn::IdleTimeout::from(varint(10_000)))
}

/// Session limits (PROTOCOL.md 1, GUESS values): idle 6 s, keep-alive 1 s, datagrams on, 512 uni and 16 bidi
/// streams, 16 MiB connection and 8 MiB stream receive windows.
pub fn session_transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(idle(Duration::from_secs(6))));
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.datagram_receive_buffer_size(Some(64 * 1024));
    t.max_concurrent_uni_streams(varint(512));
    t.max_concurrent_bidi_streams(varint(16));
    t.receive_window(varint(16 * 1024 * 1024));
    t.stream_receive_window(varint(8 * 1024 * 1024));
    t
}

/// Pairing limits (PROTOCOL.md 1): one bidirectional stream, no unidirectional streams, no datagrams, 64 KiB
/// receive windows, idle 10 s with keep-alive 2 s (the 120 s comparison keeps the connection open).
pub fn pairing_transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(idle(Duration::from_secs(10))));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.datagram_receive_buffer_size(None);
    t.max_concurrent_uni_streams(varint(0));
    t.max_concurrent_bidi_streams(varint(1));
    t.receive_window(varint(64 * 1024));
    t.stream_receive_window(varint(64 * 1024));
    t
}

fn transport_for(kind: EndpointKind) -> Arc<quinn::TransportConfig> {
    Arc::new(match kind {
        EndpointKind::Session => session_transport(),
        EndpointKind::Pairing => pairing_transport(),
    })
}

/// A quinn server configuration from a rustls one, with the endpoint's transport limits.
pub fn quic_server_config(tls: rustls::ServerConfig, kind: EndpointKind) -> Result<quinn::ServerConfig> {
    let quic = QuicServerConfig::try_from(tls).map_err(|e| TrustError::Tls(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(transport_for(kind));
    Ok(config)
}

pub fn quic_client_config(tls: rustls::ClientConfig, kind: EndpointKind) -> Result<quinn::ClientConfig> {
    let quic = QuicClientConfig::try_from(tls).map_err(|e| TrustError::Tls(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(transport_for(kind));
    Ok(config)
}

/// The session endpoint's server configuration: pinned keys only, looked up in the trust store at each handshake.
pub fn session_server_config(identity: &Identity, trust: Arc<dyn PinLookup>) -> Result<quinn::ServerConfig> {
    quic_server_config(
        server_tls_config(identity, session_server_verifier(trust), EndpointKind::Session)?,
        EndpointKind::Session,
    )
}

/// The pairing endpoint's server configuration: any well-formed Ed25519 key, one bidirectional stream.
pub fn pairing_server_config(identity: &Identity) -> Result<quinn::ServerConfig> {
    quic_server_config(
        server_tls_config(identity, pairing_server_verifier(), EndpointKind::Pairing)?,
        EndpointKind::Pairing,
    )
}

/// A session client's configuration: it accepts only `expected`, the key of the device it meant to reach.
pub fn session_client_config(identity: &Identity, expected: Spki) -> Result<quinn::ClientConfig> {
    quic_client_config(client_tls_config(identity, session_client_verifier(expected), &[ALPN])?, EndpointKind::Session)
}

/// A pairing client's configuration.
pub fn pairing_client_config(identity: &Identity) -> Result<quinn::ClientConfig> {
    quic_client_config(client_tls_config(identity, pairing_client_verifier(), &[PAIR_ALPN])?, EndpointKind::Pairing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::spki_from_public_key;

    struct Nothing;
    impl PinLookup for Nothing {
        fn pinned(&self, _: &[u8]) -> Option<crate::ids::DeviceId> {
            None
        }
    }

    struct Everything;
    impl PinLookup for Everything {
        fn pinned(&self, _: &[u8]) -> Option<crate::ids::DeviceId> {
            Some([0; 16])
        }
    }

    fn check(v: &Verifier, key: &[u8]) -> bool {
        v.check_key(&CertificateDer::from(key.to_vec()), &[]).is_ok()
    }

    #[test]
    fn the_hardening_settings_are_what_section_2_4_says() {
        let id = Identity::generate(0).unwrap();
        let s = server_tls_config(&id, pairing_server_verifier(), EndpointKind::Pairing).unwrap();
        assert_eq!(s.send_tls13_tickets, 0);
        assert_eq!(s.max_early_data_size, 0);
        assert_eq!(s.alpn_protocols, vec![PAIR_ALPN.to_vec()]);
        assert!(!s.ticketer.enabled());
        // The storage refuses to put and to get anything.
        assert!(!s.session_storage.put(vec![1], vec![2]));
        assert!(s.session_storage.get(&[1]).is_none());
        assert!(s.session_storage.take(&[1]).is_none());
        let c = client_tls_config(&id, pairing_client_verifier(), &[PAIR_ALPN]).unwrap();
        assert!(!c.enable_early_data);
        let s = server_tls_config(&id, session_server_verifier(Arc::new(Nothing)), EndpointKind::Session).unwrap();
        assert_eq!(s.alpn_protocols, vec![ALPN.to_vec()]);
    }

    #[test]
    fn the_policies_pick_keys() {
        let known = spki_from_public_key(&[1; 32]);
        let other = spki_from_public_key(&[2; 32]);
        let mk = |policy| Verifier { provider: provider(), policy };
        assert!(check(&mk(Policy::Pinned(Arc::new(Everything))), &known));
        assert!(!check(&mk(Policy::Pinned(Arc::new(Nothing))), &known));
        assert!(check(&mk(Policy::Expected(known)), &known));
        assert!(!check(&mk(Policy::Expected(known)), &other));
        assert!(check(&mk(Policy::AnyEd25519), &other));
        // Not a well-formed Ed25519 SPKI: refused whatever the policy.
        assert!(!check(&mk(Policy::AnyEd25519), &known[..43]));
        assert!(!check(&mk(Policy::Pinned(Arc::new(Everything))), &[0u8; 44]));
        // A chain is not a raw key.
        let v = mk(Policy::AnyEd25519);
        assert!(v.check_key(&CertificateDer::from(known.to_vec()), &[CertificateDer::from(known.to_vec())]).is_err());
    }
}
