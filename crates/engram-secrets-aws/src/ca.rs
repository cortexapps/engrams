//! AWS Secrets Manager-backed [`CaSource`] for the egress-proxy CA —
//! the twin of `engram_secrets_gcp::ca` (ADR 0006 / ADR 0122).
//!
//! Targets two named secrets holding the deployment-wide CA cert +
//! key in PEM form. Every host-agent in the deployment loads the same
//! material, so TLS leaves any host's proxy mints validate against
//! the cert baked into guest substrates.
//!
//! Unlike the GKE case — where hostNetwork host-agent pods bypass
//! Workload Identity and force the `env` CA source — IRSA is env/file
//! based and works in hostNetwork pods, so this direct read is the
//! sanctioned EKS path. The IMDS fallback (node instance role) also
//! works when the role carries `secretsmanager:GetSecretValue` on the
//! two CA secrets.

use async_trait::async_trait;

use engram_core::SecretError;
use engram_egress_proxy::{Ca, CaError, CaSource};

use crate::{get_secret_string, parse_ref, SecretRef};

/// Fetches the egress-proxy CA cert + key from two Secrets Manager
/// secrets. Ids accept the same forms as `aws-sm://` ref bodies: a
/// friendly name or a full ARN, with an optional `#<version-stage>`.
pub struct AwsSecretsManagerCaSource {
    client: aws_sdk_secretsmanager::Client,
    cert_ref: SecretRef,
    key_ref: SecretRef,
}

impl AwsSecretsManagerCaSource {
    /// Production constructor: SDK default chains (region + IRSA /
    /// node-role credentials).
    pub async fn new(
        cert_secret: impl AsRef<str>,
        key_secret: impl AsRef<str>,
    ) -> Result<Self, SecretError> {
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides::default()).await;
        if cfg.region().is_none() && cfg.endpoint_url().is_none() {
            return Err(SecretError::Backend(
                "no AWS region resolved; set AWS_REGION or run with IRSA".into(),
            ));
        }
        Ok(Self {
            client: aws_sdk_secretsmanager::Client::new(&cfg),
            cert_ref: parse_ref(cert_secret.as_ref()),
            key_ref: parse_ref(key_secret.as_ref()),
        })
    }

    /// Test constructor: explicit endpoint (wiremock) + pinned region.
    pub async fn with_endpoint(
        endpoint: impl Into<String>,
        cert_secret: impl AsRef<str>,
        key_secret: impl AsRef<str>,
    ) -> Self {
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides {
            region: Some("us-east-1".into()),
            endpoint_url: Some(endpoint.into()),
        })
        .await;
        Self {
            client: aws_sdk_secretsmanager::Client::new(&cfg),
            cert_ref: parse_ref(cert_secret.as_ref()),
            key_ref: parse_ref(key_secret.as_ref()),
        }
    }

    async fn fetch_pem(&self, sref: &SecretRef) -> Result<String, SecretError> {
        get_secret_string(&self.client, sref).await?.ok_or_else(|| {
            SecretError::Backend(
                format!("Secrets Manager secret `{}` not found", sref.secret_id).into(),
            )
        })
    }
}

#[async_trait]
impl CaSource for AwsSecretsManagerCaSource {
    async fn load(&self) -> Result<Ca, CaError> {
        let cert_pem = self
            .fetch_pem(&self.cert_ref)
            .await
            .map_err(|e| CaError::Rcgen(format!("fetch CA cert: {e}")))?;
        let key_pem = self
            .fetch_pem(&self.key_ref)
            .await
            .map_err(|e| CaError::Rcgen(format!("fetch CA key: {e}")))?;
        Ca::from_pem(&cert_pem, &key_pem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{body_string_contains, header};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fixture_ca() -> (String, String) {
        // Generate a real CA via the existing helper so the test
        // exercises Ca::from_pem end-to-end.
        let tmp = tempfile::tempdir().unwrap();
        let ca = Ca::load_or_generate(tmp.path()).unwrap();
        let key_pem = std::fs::read_to_string(tmp.path().join("ca.key")).unwrap();
        (ca.cert_pem, key_pem)
    }

    async fn source(server: &MockServer) -> AwsSecretsManagerCaSource {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        AwsSecretsManagerCaSource::with_endpoint(
            server.uri(),
            "engram/egress-ca-cert",
            "engram/egress-ca-key",
        )
        .await
    }

    fn secret_mock(secret_id: &str, payload: &str) -> Mock {
        Mock::given(header("x-amz-target", "secretsmanager.GetSecretValue"))
            .and(body_string_contains(secret_id))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Name": secret_id,
                "SecretString": payload
            })))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn load_fetches_both_secrets_and_constructs_ca() {
        let (cert_pem, key_pem) = fixture_ca();
        let server = MockServer::start().await;
        secret_mock("engram/egress-ca-cert", &cert_pem)
            .mount(&server)
            .await;
        secret_mock("engram/egress-ca-key", &key_pem)
            .mount(&server)
            .await;

        let ca = source(&server).await.load().await.unwrap();
        assert_eq!(ca.cert_pem, cert_pem);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_cert_secret_surfaces_typed_error() {
        let server = MockServer::start().await;
        Mock::given(header("x-amz-target", "secretsmanager.GetSecretValue"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "ResourceNotFoundException",
                "message": "Secrets Manager can't find the specified secret."
            })))
            .mount(&server)
            .await;
        let err = source(&server).await.load().await.err().unwrap();
        match err {
            CaError::Rcgen(msg) => assert!(msg.contains("not found")),
            CaError::Io(e) => panic!("expected Rcgen, got Io({e})"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn denied_surfaces_irsa_hint() {
        let server = MockServer::start().await;
        Mock::given(header("x-amz-target", "secretsmanager.GetSecretValue"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "AccessDeniedException",
                "Message": "not authorized"
            })))
            .mount(&server)
            .await;
        let err = source(&server).await.load().await.err().unwrap();
        match err {
            CaError::Rcgen(msg) => assert!(msg.contains("IRSA"), "hint missing: {msg}"),
            other => panic!("expected Rcgen(IRSA hint), got {other}"),
        }
    }
}
