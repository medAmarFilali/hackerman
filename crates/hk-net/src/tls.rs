//! TLS configuration that trusts peers by certificate fingerprint.
//!
//! Instead of checking certificates against a certificate authority, devices
//! keep a list of the fingerprints they have paired with. Both sides present
//! certificates (mutual TLS), and the TLS handshake proves each side holds the
//! private key for its certificate.
//!
//! The protocol name negotiated during the handshake (ALPN) says what a
//! connection is for. A connection for pairing is by definition with a device
//! that isn't trusted yet, and a TLS server can't choose its certificate check
//! based on the protocol name. So the server accepts any certificate during the
//! handshake, and [`crate::IncomingConnection::establish`] checks the
//! fingerprint right after it, before reading anything from the connection. A
//! pairing connection can only ever carry the pairing exchange.

use std::collections::HashSet;
use std::sync::{Arc, PoisonError, RwLock};

use hk_core::device::Fingerprint;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::{Error, Identity, fingerprint_of};

/// Protocol name of a normal connection between paired devices. Also keeps
/// Hackerman from talking to some other QUIC service on the same port.
pub(crate) const LINK_PROTOCOL: &[u8] = b"hackerman/1";
/// Protocol name of a connection that can only be used to pair.
pub(crate) const PAIRING_PROTOCOL: &[u8] = b"hackerman-pair/1";

/// The devices this one has paired with. Clones share the same list.
#[derive(Debug, Clone, Default)]
pub struct TrustStore(Arc<RwLock<HashSet<Fingerprint>>>);

impl TrustStore {
    pub fn new(devices: impl IntoIterator<Item = Fingerprint>) -> Self {
        Self(Arc::new(RwLock::new(devices.into_iter().collect())))
    }

    pub fn add(&self, device: Fingerprint) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(device);
    }

    pub fn remove(&self, device: Fingerprint) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&device);
    }

    pub fn contains(&self, device: Fingerprint) -> bool {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&device)
    }
}

pub(crate) fn server_config(identity: &Identity) -> Result<rustls::ServerConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PeerCheck::new(Policy::CheckedAfterHandshake, &provider));
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.certificate.clone()], private_key(identity))?;
    config.alpn_protocols = vec![LINK_PROTOCOL.to_vec(), PAIRING_PROTOCOL.to_vec()];
    Ok(config)
}

/// For connecting to a device that must already be paired with this one.
pub(crate) fn link_client_config(
    identity: &Identity,
    trust: TrustStore,
) -> Result<rustls::ClientConfig, Error> {
    client_config(identity, Policy::Trusted(trust), LINK_PROTOCOL)
}

/// For connecting to a device in order to pair with it. Its identity gets
/// verified by the pairing exchange instead.
pub(crate) fn pairing_client_config(identity: &Identity) -> Result<rustls::ClientConfig, Error> {
    client_config(identity, Policy::CheckedAfterHandshake, PAIRING_PROTOCOL)
}

fn client_config(
    identity: &Identity,
    policy: Policy,
    protocol: &[u8],
) -> Result<rustls::ClientConfig, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PeerCheck::new(policy, &provider));
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.certificate.clone()], private_key(identity))?;
    config.alpn_protocols = vec![protocol.to_vec()];
    Ok(config)
}

fn private_key(identity: &Identity) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(identity.private_key.clone_key())
}

#[derive(Debug)]
enum Policy {
    Trusted(TrustStore),
    CheckedAfterHandshake,
}

#[derive(Debug)]
struct PeerCheck {
    policy: Policy,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PeerCheck {
    fn new(policy: Policy, provider: &CryptoProvider) -> Self {
        Self {
            policy,
            algorithms: provider.signature_verification_algorithms,
        }
    }

    fn check(&self, certificate: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        match &self.policy {
            Policy::CheckedAfterHandshake => Ok(()),
            Policy::Trusted(trust) if trust.contains(fingerprint_of(certificate)) => Ok(()),
            Policy::Trusted(_) => Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            )),
        }
    }
}

impl ServerCertVerifier for PeerCheck {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for PeerCheck {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
