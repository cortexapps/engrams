//! TLS configuration for the instance connection.
//!
//! Cloud SQL has two server-certificate regimes:
//!
//! - `GOOGLE_MANAGED_INTERNAL_CA` (legacy): a per-instance CA signs the
//!   server certificate directly. The certificate carries
//!   `CN = "project:instance"` and NO subject alternative names, so
//!   standard webpki verification cannot apply. [`LegacyCnVerifier`]
//!   checks the signature against the instance CA, the validity window,
//!   and the CN.
//! - `GOOGLE_MANAGED_CAS_CA` / customer CAS: the server certificate has a
//!   DNS SAN for the instance's DNS name. Standard verification against
//!   the instance CA root applies, with the DNS name as the server name.
//!
//! Both configs present the ephemeral certificate as client auth — for
//! IAM authentication that certificate IS the database credential.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::Error;

/// How to verify the instance's server certificate.
pub enum ServerVerify {
    /// CAS instances: webpki verification against the instance CA; the
    /// connection uses the instance DNS name as the server name.
    StandardDns { dns_name: String },
    /// Legacy instances: per-instance CA + `CN = "project:instance"`.
    LegacyCn { expected_cn: String },
}

pub fn client_config(
    server_ca_pem: &str,
    verify: &ServerVerify,
    client_cert_pem: &str,
    client_key_der: PrivateKeyDer<'static>,
) -> Result<ClientConfig, Error> {
    let ca_der = pem_to_der(server_ca_pem, "server CA certificate")?;
    let client_chain = vec![pem_to_der(client_cert_pem, "ephemeral client certificate")?];
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(format!("TLS protocol setup: {e}")))?;
    let config = match verify {
        ServerVerify::StandardDns { .. } => {
            let mut roots = RootCertStore::empty();
            roots
                .add(ca_der)
                .map_err(|e| Error::Tls(format!("instance CA is not usable as a root: {e}")))?;
            builder
                .with_root_certificates(roots)
                .with_client_auth_cert(client_chain, client_key_der)
        }
        ServerVerify::LegacyCn { expected_cn } => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(LegacyCnVerifier {
                ca_der,
                expected_cn: expected_cn.clone(),
                provider,
            }))
            .with_client_auth_cert(client_chain, client_key_der),
    }
    .map_err(|e| Error::Tls(format!("client certificate rejected: {e}")))?;
    Ok(config)
}

/// One PEM certificate block to DER. Public because callers (and tests)
/// routinely hold the instance CA as PEM.
pub fn pem_to_der(pem_text: &str, what: &str) -> Result<CertificateDer<'static>, Error> {
    let block =
        pem::parse(pem_text).map_err(|e| Error::Tls(format!("{what} is not valid PEM: {e}")))?;
    Ok(CertificateDer::from(block.into_contents()))
}

/// The legacy verifier: signature chain to the per-instance CA (depth 1,
/// the only chain shape this regime produces), validity window at `now`,
/// and the exact CN. The server name from the connection is deliberately
/// ignored — legacy certificates carry no name usable for it.
#[derive(Debug)]
struct LegacyCnVerifier {
    ca_der: CertificateDer<'static>,
    expected_cn: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for LegacyCnVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let bad = |message: String| rustls::Error::General(message);
        let (_, ca) = X509Certificate::from_der(&self.ca_der)
            .map_err(|e| bad(format!("instance CA does not parse: {e}")))?;
        let (_, leaf) = X509Certificate::from_der(end_entity)
            .map_err(|e| bad(format!("server certificate does not parse: {e}")))?;

        let now = x509_parser::time::ASN1Time::from_timestamp(now.as_secs() as i64)
            .map_err(|e| bad(format!("handshake time is out of range: {e}")))?;
        if !leaf.validity().is_valid_at(now) {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Expired,
            ));
        }
        if leaf.issuer() != ca.subject() {
            return Err(bad(
                "server certificate is not issued by the instance CA".into()
            ));
        }
        leaf.verify_signature(Some(ca.public_key()))
            .map_err(|e| bad(format!("server certificate signature is invalid: {e}")))?;

        let cn = leaf
            .subject()
            .iter_common_name()
            .next()
            .and_then(|attr| attr.as_str().ok())
            .unwrap_or_default();
        if cn != self.expected_cn {
            return Err(bad(format!(
                "server certificate CN {cn:?} is not the instance {:?}",
                self.expected_cn,
            )));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
