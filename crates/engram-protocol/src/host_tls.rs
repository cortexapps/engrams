//! Operator-owned, host-specific mTLS configuration. No plaintext fallback.
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPeer {
    pub endpoint: String,
    pub server_name: String,
    pub server_ca: PathBuf,
    pub server_pins: BTreeSet<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostClientTls {
    pub client_certificate: PathBuf,
    pub client_key: PathBuf,
    pub hosts: BTreeMap<engram_core::HostId, HostPeer>,
}
pub fn certificate_pin(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
pub fn validate_pins(pins: &BTreeSet<String>) -> Result<(), String> {
    if pins.is_empty()
        || pins.len() > 8
        || pins.iter().any(|p| {
            p.len() != 64
                || !p
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err("invalid host certificate pins".into());
    }
    Ok(())
}
#[derive(Debug)]
struct PinnedVerifier {
    chain: Arc<rustls::client::WebPkiServerVerifier>,
    pins: BTreeSet<String>,
}
impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let verified = self
            .chain
            .verify_server_cert(end, intermediates, name, ocsp, now)?;
        if !self.pins.contains(&certificate_pin(end.as_ref())) {
            return Err(rustls::Error::General(
                "host certificate pin mismatch".into(),
            ));
        }
        Ok(verified)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.chain.verify_tls12_signature(message, cert, sig)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.chain.verify_tls13_signature(message, cert, sig)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.chain.supported_verify_schemes()
    }
}
impl HostClientTls {
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os("ENGRAM_HOST_CLIENT_TLS_CONFIG") else {
            return Ok(None);
        };
        let bytes = std::fs::read(path).map_err(|_| "cannot read host TLS configuration")?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "invalid host TLS configuration".into())
    }
    pub fn endpoint(
        &self,
        host: engram_core::HostId,
        address: &str,
        endpoint: tonic::transport::Endpoint,
    ) -> Result<tonic::transport::Endpoint, String> {
        engram_tls::install_provider();
        let peer = self
            .hosts
            .get(&host)
            .ok_or("host TLS identity is not configured")?;
        if peer.endpoint != address
            || !address.starts_with("https://")
            || peer.server_name.is_empty()
        {
            return Err("host TLS endpoint mismatch".into());
        }
        validate_pins(&peer.server_pins)?;
        let ca = std::fs::read(&peer.server_ca).map_err(|_| "cannot read host CA")?;
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut &ca[..]) {
            roots
                .add(cert.map_err(|_| "invalid host CA")?)
                .map_err(|_| "invalid host CA")?;
        }
        let chain = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|_| "invalid host CA")?;
        let certificate = std::fs::read(&self.client_certificate)
            .map_err(|_| "cannot read coordinator certificate")?;
        let key = std::fs::read(&self.client_key).map_err(|_| "cannot read coordinator TLS key")?;
        endpoint
            .tls_config_with_verifier(
                tonic::transport::ClientTlsConfig::new()
                    .domain_name(&peer.server_name)
                    .identity(tonic::transport::Identity::from_pem(certificate, key)),
                Arc::new(PinnedVerifier {
                    chain,
                    pins: peer.server_pins.clone(),
                }),
            )
            .map_err(|_| "invalid host TLS configuration".into())
    }
}
