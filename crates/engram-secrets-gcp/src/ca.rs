//! GCP Secret Manager-backed [`CaSource`] for the egress-proxy CA.
//!
//! Same authentication path as [`crate::GcpSecretManager`] (Workload
//! Identity → metadata-server bearer token), targeting two named
//! secrets that hold the deployment-wide CA cert + key in PEM form.
//! Every host-agent in the deployment loads the same material, so
//! TLS leaves any host's proxy mints validate against the cert
//! baked into guest substrates. ADR 0006.
//!
//! Why this lives in `engram-secrets-gcp` rather than
//! `engram-egress-proxy`: the proxy crate stays cloud-agnostic.
//! Cloud-specific backends (this one, future AWS / Vault) ship in
//! their own crates and are wired together by the host-agent at
//! boot via the [`engram_egress_proxy::CaSource`] trait.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::SecretError;
use engram_egress_proxy::{Ca, CaError, CaSource};

use crate::{access_payload, MetadataTokenSource, TokenSource};

/// Fetches the egress-proxy CA cert + key from two GCP Secret
/// Manager paths. Paths are fully-qualified
/// (`projects/<p>/secrets/<name>/versions/<v>`) so deployments can
/// pin to a specific version or use `latest`.
pub struct GcpSecretManagerCaSource {
    http: reqwest::Client,
    base_url: String,
    token_source: Arc<dyn TokenSource>,
    cert_path: String,
    key_path: String,
}

impl GcpSecretManagerCaSource {
    /// Production constructor: metadata-server token auth +
    /// public Secret Manager API. Both `cert_secret` and
    /// `key_secret` are fully-qualified Secret Manager resource
    /// paths (e.g.
    /// `projects/cortex-prod/secrets/engram-egress-ca-cert/versions/latest`).
    pub fn new(
        cert_secret: impl Into<String>,
        key_secret: impl Into<String>,
    ) -> Result<Self, SecretError> {
        let http = engram_tls::client_builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| SecretError::Backend(Box::new(e)))?;
        Ok(Self {
            http,
            base_url: crate::DEFAULT_SECRETMANAGER_BASE.into(),
            token_source: Arc::new(MetadataTokenSource::new()?),
            cert_path: cert_secret.into(),
            key_path: key_secret.into(),
        })
    }

    /// Override the Secret Manager API root. Tests point this at a
    /// `wiremock` server.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Override the token source. Tests inject `StaticTokenSource`.
    pub fn with_token_source(mut self, ts: Arc<dyn TokenSource>) -> Self {
        self.token_source = ts;
        self
    }

    async fn fetch_pem(&self, path: &str) -> Result<String, SecretError> {
        let bytes = access_payload(&self.http, &self.base_url, &self.token_source, path)
            .await?
            .ok_or_else(|| {
                SecretError::Backend(
                    format!("Secret Manager secret `{path}` not found (404)").into(),
                )
            })?;
        String::from_utf8(bytes).map_err(|_| {
            SecretError::BadValue(format!(
                "Secret Manager secret `{path}` is not valid UTF-8 (expected PEM text)"
            ))
        })
    }
}

#[async_trait]
impl CaSource for GcpSecretManagerCaSource {
    async fn load(&self) -> Result<Ca, CaError> {
        let cert_pem = self
            .fetch_pem(&self.cert_path)
            .await
            .map_err(|e| CaError::Rcgen(format!("fetch CA cert: {e}")))?;
        let key_pem = self
            .fetch_pem(&self.key_path)
            .await
            .map_err(|e| CaError::Rcgen(format!("fetch CA key: {e}")))?;
        Ca::from_pem(&cert_pem, &key_pem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StaticTokenSource;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::Engine as _;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture_ca() -> (String, String) {
        // Generate a real CA via the existing helper so the test
        // exercises Ca::from_pem end-to-end.
        let tmp = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_generate(tmp.path()).unwrap();
        let key_pem = std::fs::read_to_string(tmp.path().join("ca.key")).unwrap();
        (ca.cert_pem, key_pem)
    }

    fn source(server: &MockServer) -> GcpSecretManagerCaSource {
        GcpSecretManagerCaSource::new(
            "projects/test-proj/secrets/engram-egress-ca-cert/versions/latest",
            "projects/test-proj/secrets/engram-egress-ca-key/versions/latest",
        )
        .unwrap()
        .with_base_url(server.uri())
        .with_token_source(Arc::new(StaticTokenSource("test-token".into())))
    }

    fn secret_mock(secret_path: &str, payload: &str) -> Mock {
        Mock::given(method("GET"))
            .and(path(format!("/v1/{secret_path}:access")))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "payload": { "data": BASE64_STANDARD.encode(payload) }
            })))
    }

    #[tokio::test]
    async fn load_fetches_both_secrets_and_constructs_ca() {
        let (cert_pem, key_pem) = fixture_ca();
        let server = MockServer::start().await;
        secret_mock(
            "projects/test-proj/secrets/engram-egress-ca-cert/versions/latest",
            &cert_pem,
        )
        .mount(&server)
        .await;
        secret_mock(
            "projects/test-proj/secrets/engram-egress-ca-key/versions/latest",
            &key_pem,
        )
        .mount(&server)
        .await;

        let ca = source(&server).load().await.unwrap();
        assert_eq!(ca.cert_pem, cert_pem);
    }

    #[tokio::test]
    async fn missing_cert_secret_surfaces_typed_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = source(&server).load().await.err().unwrap();
        match err {
            CaError::Rcgen(msg) => assert!(msg.contains("not found")),
            CaError::Io(e) => panic!("expected Rcgen, got Io({e})"),
        }
    }

    #[tokio::test]
    async fn forbidden_surfaces_workload_identity_hint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let err = source(&server).load().await.err().unwrap();
        match err {
            CaError::Rcgen(msg) => {
                assert!(msg.contains("Workload Identity") || msg.contains("403"));
            }
            other => panic!("expected Rcgen(WI hint), got {other}"),
        }
    }
}
