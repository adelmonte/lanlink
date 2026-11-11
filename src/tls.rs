//! TLS setup. KDE Connect uses self-signed certs on both sides and pins the
//! peer's cert at pairing time, so the handshake accepts any cert and the
//! daemon compares it against the stored one afterwards.

use crate::config::Identity;
use anyhow::{Context, Result, anyhow};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct AcceptAny(WebPkiSupportedAlgorithms);

impl AcceptAny {
    fn new() -> Arc<Self> {
        Arc::new(AcceptAny(provider().signature_verification_algorithms))
    }
}

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

impl ClientCertVerifier for AcceptAny {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn client_auth_mandatory(&self) -> bool {
        true
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_schemes()
    }
}

pub fn server_config(id: &Identity) -> Result<Arc<ServerConfig>> {
    let cfg = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_client_cert_verifier(AcceptAny::new())
        .with_single_cert(vec![id.cert.clone()], id.key.clone_key())?;
    Ok(Arc::new(cfg))
}

pub fn client_config(id: &Identity) -> Result<Arc<ClientConfig>> {
    let cfg = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .dangerous()
        .with_custom_certificate_verifier(AcceptAny::new())
        .with_client_auth_cert(vec![id.cert.clone()], id.key.clone_key())?;
    Ok(Arc::new(cfg))
}

/// The SNI name is ignored by the verifier; it just has to be syntactically valid.
pub fn dummy_name() -> ServerName<'static> {
    ServerName::try_from("lanlink").expect("valid name")
}

pub fn peer_cert(s: &TlsStream<TcpStream>) -> Result<Vec<u8>> {
    let certs = match s {
        TlsStream::Client(c) => c.get_ref().1.peer_certificates(),
        TlsStream::Server(c) => c.get_ref().1.peer_certificates(),
    };
    certs
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec())
        .ok_or_else(|| anyhow!("peer sent no certificate"))
}

pub fn common_name(der: &[u8]) -> Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).context("parsing peer cert")?;
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|a| a.as_str().ok())
        .ok_or_else(|| anyhow!("cert has no CN"))?;
    Ok(cn.to_string())
}

fn spki(der: &[u8]) -> Result<Vec<u8>> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).context("parsing cert")?;
    Ok(cert.tbs_certificate.subject_pki.raw.to_vec())
}

/// The 8-character code both screens show while pairing (protocol v8):
/// SHA-256 over both public keys (larger first) plus the pairing timestamp.
pub fn verification_key(ours: &[u8], theirs: &[u8], timestamp: i64) -> Result<String> {
    let mut a = spki(ours)?;
    let mut b = spki(theirs)?;
    if a < b {
        std::mem::swap(&mut a, &mut b);
    }
    let mut h = Sha256::new();
    h.update(&a);
    h.update(&b);
    h.update(timestamp.to_string().as_bytes());
    Ok(hex::encode(h.finalize())[..8].to_uppercase())
}

pub fn fingerprint(der: &[u8]) -> String {
    let d = Sha256::digest(der);
    hex::encode(&d[..8]).to_uppercase()
}
