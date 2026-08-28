//! AWS KMS-backed [`MasterKeyProvider`] (ADR 0122).
//!
//! Wrap = `kms:Encrypt` of the 32-byte DEK under a customer-managed
//! symmetric key (far below the 4096-byte plaintext limit); unwrap =
//! `kms:Decrypt`. The KMS ciphertext blob is opaque and self-describes
//! its key, but Decrypt is still called WITH the configured `KeyId` so
//! a ciphertext produced under a different key fails loudly instead of
//! silently unwrapping via whatever key the blob names.
//!
//! `key_id()` is `aws-kms:<key-arn-or-id>` — stored beside each sealed
//! credential so rotation is detectable (the ADR 0086 lazy-rewrap
//! contract). Sealed data is provider-bound: nothing migrates between
//! KEK providers, so switching providers is a new-deployment decision,
//! not a config flip (recorded in ADR 0122 D6).
//!
//! Deliberate asymmetry: `GcpKmsProvider` in `engram-crypto` stays a
//! stub — GCP production uses the `env-var` provider. The AWS arm is
//! real because it ships in the same effort as the rest of the AWS
//! backend.
//!
//! A separate crate, not code inside `engram-crypto`: that crate is a
//! dependency of musl guest binaries and every sealing consumer, and
//! must not pull an AWS SDK.
//!
//! Auth: the SDK default chain via `engram-aws` — IRSA on EKS. No
//! SDK-internal retries (engram-aws posture); a failed wrap/unwrap
//! surfaces to the caller, whose operation (registry-cred seal, cred
//! open) is the right retry unit.

use async_trait::async_trait;
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::Client;

use engram_crypto::{CryptoError, MasterKeyProvider};

/// Per-call deadline. The shared `engram-aws` transport bounds only
/// the CONNECT (5 s) and disables SDK retries; a KMS response that
/// stalls after the handshake would otherwise hang the caller —
/// wrap/unwrap sit on the registry-cred seal/open and session-secret
/// paths, which must fail fast, not wedge. Mirrors the 10 s bound
/// engram-secrets-aws applies to GetSecretValue.
const OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Bound one KMS call with [`OPERATION_TIMEOUT`], mapping a stall to
/// a typed provider error.
async fn with_deadline<T, E, F>(op: &'static str, fut: F) -> Result<T, CryptoError>
where
    F: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match tokio::time::timeout(OPERATION_TIMEOUT, fut).await {
        Ok(r) => r.map_err(|e| CryptoError::Provider(format!("kms {op}: {e}"))),
        Err(_) => Err(CryptoError::Provider(format!(
            "kms {op} exceeded the {}s deadline",
            OPERATION_TIMEOUT.as_secs()
        ))),
    }
}

/// KMS-backed KEK. One client per instance, cheap to clone.
#[derive(Clone, Debug)]
pub struct AwsKmsProvider {
    client: Client,
    /// The configured key — an ARN, key id, or alias (`alias/engram-kek`).
    key: String,
    /// `aws-kms:<key>` — the stable id stored beside sealed creds.
    key_id: String,
}

impl AwsKmsProvider {
    /// Production constructor: SDK default chains (region + IRSA
    /// credentials). `key` is anything KMS accepts as a `KeyId`: a key
    /// ARN (preferred — unambiguous across accounts), a key id, or an
    /// alias like `alias/engram-kek`.
    pub async fn new(key: impl Into<String>) -> Result<Self, CryptoError> {
        let key = key.into();
        if key.is_empty() {
            return Err(CryptoError::Provider("empty AWS KMS key id".into()));
        }
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides::default()).await;
        if cfg.region().is_none() && cfg.endpoint_url().is_none() {
            return Err(CryptoError::Provider(
                "no AWS region resolved; set AWS_REGION or run with IRSA".into(),
            ));
        }
        Ok(Self::from_config(&cfg, key))
    }

    /// Test constructor: explicit endpoint (wiremock) + pinned region.
    pub async fn with_endpoint(endpoint: impl Into<String>, key: impl Into<String>) -> Self {
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides {
            region: Some("us-east-1".into()),
            endpoint_url: Some(endpoint.into()),
        })
        .await;
        Self::from_config(&cfg, key.into())
    }

    fn from_config(cfg: &engram_aws::SdkConfig, key: String) -> Self {
        let key_id = format!("aws-kms:{key}");
        Self {
            client: Client::new(cfg),
            key,
            key_id,
        }
    }
}

#[async_trait]
impl MasterKeyProvider for AwsKmsProvider {
    async fn wrap(&self, dek: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let out = with_deadline(
            "Encrypt",
            self.client
                .encrypt()
                .key_id(&self.key)
                .plaintext(Blob::new(dek))
                .send(),
        )
        .await?;
        let blob = out
            .ciphertext_blob
            .ok_or_else(|| CryptoError::Provider("kms Encrypt returned no ciphertext".into()))?;
        Ok(blob.into_inner())
    }

    async fn unwrap(&self, wrapped: &[u8]) -> Result<Vec<u8>, CryptoError> {
        // KeyId is passed explicitly (see the module doc): a blob
        // produced under a different key errs instead of resolving via
        // the key the blob itself names.
        let out = with_deadline(
            "Decrypt",
            self.client
                .decrypt()
                .key_id(&self.key)
                .ciphertext_blob(Blob::new(wrapped))
                .send(),
        )
        .await?;
        let blob = out
            .plaintext
            .ok_or_else(|| CryptoError::Provider("kms Decrypt returned no plaintext".into()))?;
        Ok(blob.into_inner())
    }

    fn key_id(&self) -> &str {
        &self.key_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::Engine as _;
    use wiremock::matchers::{body_string_contains, header};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const KEY_ARN: &str =
        "arn:aws:kms:us-east-1:123456789012:key/00000000-0000-0000-0000-000000000000";

    async fn provider(server: &MockServer) -> AwsKmsProvider {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        AwsKmsProvider::with_endpoint(server.uri(), KEY_ARN).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrap_returns_the_ciphertext_blob_bytes() {
        let server = MockServer::start().await;
        let ciphertext = b"opaque-kms-ciphertext";
        Mock::given(header("x-amz-target", "TrentService.Encrypt"))
            .and(body_string_contains(KEY_ARN))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "CiphertextBlob": BASE64_STANDARD.encode(ciphertext),
                "KeyId": KEY_ARN
            })))
            .expect(1)
            .mount(&server)
            .await;

        let wrapped = provider(&server)
            .await
            .wrap(&[7u8; 32])
            .await
            .expect("wrap");
        assert_eq!(wrapped, ciphertext);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unwrap_sends_the_configured_key_id_and_returns_plaintext() {
        let server = MockServer::start().await;
        let dek = [9u8; 32];
        Mock::given(header("x-amz-target", "TrentService.Decrypt"))
            // The cross-check pin: Decrypt must carry OUR KeyId, not
            // rely on the blob's self-description.
            .and(body_string_contains(KEY_ARN))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Plaintext": BASE64_STANDARD.encode(dek),
                "KeyId": KEY_ARN
            })))
            .expect(1)
            .mount(&server)
            .await;

        let out = provider(&server)
            .await
            .unwrap(b"opaque-kms-ciphertext")
            .await
            .expect("unwrap");
        assert_eq!(out, dek);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreign_key_ciphertext_surfaces_a_typed_error() {
        let server = MockServer::start().await;
        Mock::given(header("x-amz-target", "TrentService.Decrypt"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "IncorrectKeyException",
                "message": "The key ID in the request does not identify a CMK that can perform this operation."
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = provider(&server)
            .await
            .unwrap(b"blob-from-another-key")
            .await
            .expect_err("foreign-key blob must error");
        assert!(matches!(err, CryptoError::Provider(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn key_id_is_stable_and_prefixed() {
        let server = MockServer::start().await;
        let p = provider(&server).await;
        assert_eq!(p.key_id(), format!("aws-kms:{KEY_ARN}"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_key_fails_construction() {
        let err = AwsKmsProvider::new("").await.expect_err("empty key");
        assert!(matches!(err, CryptoError::Provider(_)));
    }
}
