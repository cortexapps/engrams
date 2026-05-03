//! GCP Workload Identity strategy: exchange the host-agent's
//! ambient cloud identity for a short-lived OAuth access token,
//! present it to the registry as basic auth.
//!
//! # Mechanics
//!
//! GAR (and the legacy GCR endpoints) accept OAuth tokens via the
//! standard `(username = "oauth2accesstoken", password = <token>)`
//! basic-auth dance. The token comes from the runtime's GCP
//! identity:
//!
//! - On GKE: pod's bound K8s SA → IAM SA via Workload Identity.
//! - On GCE / Cloud Run: the instance's attached SA via the
//!   metadata server.
//! - On a developer laptop (testing): falls back to the
//!   gcloud-CLI's application-default credentials. Not a path
//!   we'd hit in production but useful for local dev iterations.
//!
//! [`gcp_auth`] auto-detects which of these apply and produces a
//! cached `TokenProvider`. We just call `.token(SCOPES)` per pull;
//! the crate handles caching and refresh.
//!
//! # Impersonation (deferred)
//!
//! `RegistryAuthSpec::GcpWorkloadIdentity { impersonate_sa: Some }`
//! requests a chained identity: take Engram's ambient token, call
//! `iamcredentials.googleapis.com :generateAccessToken` against the
//! target SA, present *that* token to the registry. v1 does not
//! implement this — the constructor returns a clear error pointing
//! at the exact API call to wire up. Schema and resolver dispatch
//! are designed-in, so the only work left is the IAM Credentials
//! HTTP call. Tracked as a follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use engram_oci::{BasicCreds, OciError};
use gcp_auth::TokenProvider;

use crate::AuthStrategy;

/// OAuth scopes a GAR pull requires. `cloud-platform` is the
/// universal "all GCP APIs" scope; if a narrower one ever becomes
/// useful (storage-read-only is sometimes recommended for pulls)
/// we revisit.
const GAR_SCOPES: &[&str] = &["https://www.googleapis.com/auth/cloud-platform"];

/// Username GAR/GCR expects when the password is an OAuth token.
/// This is a literal string, not a placeholder — the registry side
/// of the basic-auth handshake matches against it exactly.
const OAUTH_USERNAME: &str = "oauth2accesstoken";

pub struct GcpWorkloadIdentityStrategy {
    provider: Arc<dyn TokenProvider>,
}

impl GcpWorkloadIdentityStrategy {
    /// Build a strategy bound to the runtime's ambient identity.
    /// Construction calls `gcp_auth::provider()` which probes the
    /// environment (metadata server, SA file, gcloud CLI) and
    /// errors if none of those are available — i.e. if the host-
    /// agent isn't running on GCP and has no fallback creds. That
    /// error is what users see at strategy-build time, not per-pull,
    /// so misconfiguration surfaces immediately.
    pub async fn new(impersonate_sa: Option<String>) -> Result<Self, gcp_auth::Error> {
        if impersonate_sa.is_some() {
            // We could plumb this through by:
            //   1. fetch ambient token via `gcp_auth::provider()`
            //   2. POST to
            //      https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/<sa>:generateAccessToken
            //      with `Authorization: Bearer <ambient>` and body
            //      `{"scope": [...], "lifetime": "3600s"}`
            //   3. wrap the result in a custom `TokenProvider` impl
            //      that does this lazily and caches per expiry.
            // ~80 LOC; deferred until a user actually needs it.
            return Err(gcp_auth::Error::Other(
                "engram_oci_auth::gcp::impersonation",
                "service-account impersonation is not yet implemented \
                 (file an issue if you need this — the schema is ready)"
                    .into(),
            ));
        }
        let provider = gcp_auth::provider().await?;
        Ok(Self { provider })
    }

    /// For tests: skip GCP detection, use a caller-supplied
    /// `TokenProvider` impl. Production code uses [`Self::new`].
    pub fn with_provider(provider: Arc<dyn TokenProvider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl AuthStrategy for GcpWorkloadIdentityStrategy {
    async fn fetch_creds(&self) -> Result<BasicCreds, OciError> {
        let token = self
            .provider
            .token(GAR_SCOPES)
            .await
            .map_err(|e| OciError::Distribution(format!("gcp token fetch: {e}")))?;
        Ok(BasicCreds {
            username: OAUTH_USERNAME.into(),
            password: token.as_str().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Mock `TokenProvider`. The trait owns its own caching so we
    /// just hand back a fixed token; the strategy doesn't second-
    /// guess that. Useful for asserting the basic-auth shape
    /// without touching a real metadata server.
    #[derive(Default)]
    struct FixedTokenProvider {
        token: String,
    }

    #[async_trait]
    impl TokenProvider for FixedTokenProvider {
        async fn token(&self, _scopes: &[&str]) -> Result<Arc<gcp_auth::Token>, gcp_auth::Error> {
            // gcp_auth::Token deserializes from the OAuth wire shape:
            // `{access_token, expires_in}`. Build a synthetic token
            // valid for an hour — the strategy doesn't read the
            // expiry, only the access token, so this is fine for
            // unit tests.
            let json = serde_json::json!({
                "access_token": self.token,
                "expires_in": 3600,
            });
            let token: gcp_auth::Token = serde_json::from_value(json)
                .map_err(|e| gcp_auth::Error::Other("test::synthetic_token", Box::new(e)))?;
            Ok(Arc::new(token))
        }

        async fn project_id(&self) -> Result<Arc<str>, gcp_auth::Error> {
            Ok(Arc::from("test-project"))
        }
    }

    #[tokio::test]
    async fn fetch_creds_returns_oauth_username_with_token_password() {
        let provider: Arc<dyn TokenProvider> = Arc::new(FixedTokenProvider {
            token: "ya29.fake".into(),
        });
        let strategy = GcpWorkloadIdentityStrategy::with_provider(provider);
        let creds = strategy.fetch_creds().await.unwrap();
        assert_eq!(creds.username, OAUTH_USERNAME);
        assert_eq!(creds.password, "ya29.fake");
    }

    #[tokio::test]
    async fn impersonation_flag_returns_clear_error_at_build_time() {
        let res =
            GcpWorkloadIdentityStrategy::new(Some("sa@p.iam.gserviceaccount.com".into())).await;
        assert!(res.is_err());
        let msg = format!("{}", res.err().unwrap());
        assert!(
            msg.contains("impersonation"),
            "error must surface the deferred-feature reason; got {msg:?}"
        );
    }
}
