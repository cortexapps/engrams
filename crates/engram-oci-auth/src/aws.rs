//! AWS ECR strategy (ADR 0122): exchange the host-agent's ambient
//! AWS IAM identity for an ECR authorization token, present it to the
//! registry as basic auth — the ECR twin of [`crate::gcp`].
//!
//! # Mechanics
//!
//! `ecr:GetAuthorizationToken` returns a base64 `AWS:<password>` pair
//! valid for ~12 hours. ECR registries accept it as standard basic
//! auth. The identity comes from the SDK default chain via
//! `engram-aws`: IRSA on EKS (works in the hostNetwork host-agent pod
//! — env/file-based, unlike GKE Workload Identity), or the node
//! instance role via IMDSv2.
//!
//! The strategy caches the decoded credentials and refreshes when
//! within 15 minutes of the server-reported expiry — the GCP token
//! cache's shape, with the TTL owned here because the ECR SDK does no
//! caching of its own.
//!
//! The region is parsed from the registry host
//! (`<acct>.dkr.ecr.<region>.amazonaws.com[.cn]`), so one strategy
//! per host is automatically region-correct with no extra config.
//!
//! # Cross-account assume-role (deferred)
//!
//! `RegistryAuthSpec::AwsEcr { assume_role_arn: Some }` requests a
//! chained identity: `sts:AssumeRole` into the target role, then
//! GetAuthorizationToken under it. v1 does not implement this — the
//! constructor returns a clear error, mirroring the GCP
//! `impersonate_sa` deferral. Schema and resolver dispatch are
//! designed-in; the only work left is the STS call.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;

use engram_oci::{BasicCreds, OciError};

use crate::AuthStrategy;

/// Refresh margin before the server-reported expiry. Tokens live ~12 h;
/// refreshing 15 minutes early keeps a long image pull from straddling
/// the boundary.
const REFRESH_MARGIN: Duration = Duration::from_secs(15 * 60);

/// Parse the region out of an ECR registry host:
/// `<account>.dkr.ecr.<region>.amazonaws.com` (or `.com.cn` for the
/// China partition, where the region already carries the `cn-` prefix).
/// Returns `None` for anything that isn't an ECR host shape.
pub(crate) fn parse_ecr_region(host: &str) -> Option<&str> {
    let host = host
        .strip_suffix(".amazonaws.com.cn")
        .or_else(|| host.strip_suffix(".amazonaws.com"))?;
    let (_, region) = host.split_once(".dkr.ecr.")?;
    if region.is_empty() || region.contains('.') {
        return None;
    }
    Some(region)
}

struct CachedCreds {
    creds: BasicCreds,
    not_after: Instant,
}

pub struct AwsEcrStrategy {
    client: aws_sdk_ecr::Client,
    cache: Mutex<Option<CachedCreds>>,
}

impl AwsEcrStrategy {
    /// Build a strategy for one ECR registry host. The region comes
    /// from the host itself; credentials come from the ambient chain.
    /// A non-ECR host shape errors at build time (misconfiguration
    /// surfaces immediately, not per pull).
    pub async fn new(
        registry_host: &str,
        assume_role_arn: Option<String>,
    ) -> Result<Self, OciError> {
        if assume_role_arn.is_some() {
            // Wiring this is: sts:AssumeRole with the target ARN, an
            // aws-credential-types provider over the response, and a
            // client built on it. Deferred until a user needs it,
            // mirroring the GCP impersonation deferral.
            return Err(OciError::Distribution(
                "aws_ecr assume_role_arn is not yet implemented \
                 (file an issue if you need this — the schema is ready)"
                    .into(),
            ));
        }
        let region = parse_ecr_region(registry_host).ok_or_else(|| {
            OciError::Distribution(format!(
                "`{registry_host}` is not an ECR registry host \
                 (expected <account>.dkr.ecr.<region>.amazonaws.com)"
            ))
        })?;
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides {
            region: Some(region.to_string()),
            endpoint_url: None,
        })
        .await;
        Ok(Self {
            client: aws_sdk_ecr::Client::new(&cfg),
            cache: Mutex::new(None),
        })
    }

    /// For tests: explicit endpoint (wiremock) + pinned region.
    #[cfg(test)]
    async fn with_endpoint(endpoint: String) -> Self {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides {
            region: Some("us-east-1".into()),
            endpoint_url: Some(endpoint),
        })
        .await;
        Self {
            client: aws_sdk_ecr::Client::new(&cfg),
            cache: Mutex::new(None),
        }
    }
}

#[async_trait]
impl AuthStrategy for AwsEcrStrategy {
    async fn fetch_creds(&self) -> Result<BasicCreds, OciError> {
        if let Some(cached) = self
            .cache
            .lock()
            .expect("ecr cache mutex poisoned")
            .as_ref()
        {
            if cached.not_after > Instant::now() {
                return Ok(cached.creds.clone());
            }
        }

        let out = self
            .client
            .get_authorization_token()
            .send()
            .await
            .map_err(|e| OciError::Distribution(format!("ecr GetAuthorizationToken: {e}")))?;
        let data = out
            .authorization_data()
            .first()
            .ok_or_else(|| {
                OciError::Distribution("ecr GetAuthorizationToken returned no data".into())
            })?
            .clone();
        let token_b64 = data.authorization_token().ok_or_else(|| {
            OciError::Distribution("ecr GetAuthorizationToken returned no token".into())
        })?;
        let decoded = BASE64_STANDARD
            .decode(token_b64)
            .map_err(|e| OciError::Distribution(format!("ecr token not base64: {e}")))?;
        let decoded = String::from_utf8(decoded)
            .map_err(|_| OciError::Distribution("ecr token not UTF-8".into()))?;
        let (username, password) = decoded
            .split_once(':')
            .ok_or_else(|| OciError::Distribution("ecr token is not `user:password`".into()))?;
        let creds = BasicCreds {
            username: username.to_string(),
            password: password.to_string(),
        };

        // Cache until 15 min before the server-reported expiry.
        // `expires_at` is ~now+12h; if the response omits it (never
        // observed, but the field is optional on the wire), fall back
        // to a conservative 1 h.
        let ttl = data
            .expires_at()
            .and_then(|exp| {
                let now = aws_sdk_ecr::primitives::DateTime::from(std::time::SystemTime::now());
                let secs = exp.secs().saturating_sub(now.secs());
                u64::try_from(secs).ok()
            })
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(3600));
        let not_after = Instant::now() + ttl.saturating_sub(REFRESH_MARGIN);
        *self.cache.lock().expect("ecr cache mutex poisoned") = Some(CachedCreds {
            creds: creds.clone(),
            not_after,
        });
        Ok(creds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::header;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_ecr_region_accepts_standard_and_cn_hosts() {
        assert_eq!(
            parse_ecr_region("123456789012.dkr.ecr.us-west-2.amazonaws.com"),
            Some("us-west-2")
        );
        assert_eq!(
            parse_ecr_region("123456789012.dkr.ecr.cn-north-1.amazonaws.com.cn"),
            Some("cn-north-1")
        );
    }

    #[test]
    fn parse_ecr_region_rejects_non_ecr_hosts() {
        for bad in [
            "ghcr.io",
            "docker.io",
            "us-east1-docker.pkg.dev",
            "dkr.ecr.us-west-2.amazonaws.com.evil.example",
            "123.dkr.ecr..amazonaws.com",
            "s3.us-west-2.amazonaws.com",
        ] {
            assert_eq!(parse_ecr_region(bad), None, "must reject {bad}");
        }
    }

    #[tokio::test]
    async fn non_ecr_host_errors_at_build_time() {
        let err = AwsEcrStrategy::new("ghcr.io", None).await.err().unwrap();
        assert!(format!("{err}").contains("not an ECR registry host"));
    }

    #[tokio::test]
    async fn assume_role_returns_clear_error_at_build_time() {
        let err = AwsEcrStrategy::new(
            "123456789012.dkr.ecr.us-west-2.amazonaws.com",
            Some("arn:aws:iam::123456789012:role/pull".into()),
        )
        .await
        .err()
        .unwrap();
        assert!(format!("{err}").contains("assume_role_arn"));
    }

    fn token_response(user: &str, pass: &str, expires_epoch: i64) -> serde_json::Value {
        serde_json::json!({
            "authorizationData": [{
                "authorizationToken": BASE64_STANDARD.encode(format!("{user}:{pass}")),
                "expiresAt": expires_epoch,
                "proxyEndpoint": "https://123456789012.dkr.ecr.us-east-1.amazonaws.com"
            }]
        })
    }

    #[tokio::test]
    async fn fetch_creds_decodes_the_token_pair() {
        let server = MockServer::start().await;
        let exp = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
            + 12 * 3600;
        Mock::given(header(
            "x-amz-target",
            "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(token_response("AWS", "ecr-pass", exp)),
        )
        .expect(1)
        .mount(&server)
        .await;

        let strategy = AwsEcrStrategy::with_endpoint(server.uri()).await;
        let creds = strategy.fetch_creds().await.unwrap();
        assert_eq!(creds.username, "AWS");
        assert_eq!(creds.password, "ecr-pass");

        // Second fetch rides the cache: the mock's expect(1) fails the
        // test if another request lands.
        let again = strategy.fetch_creds().await.unwrap();
        assert_eq!(again.password, "ecr-pass");
    }

    #[tokio::test]
    async fn expired_cache_refetches() {
        let server = MockServer::start().await;
        // Expiry within the refresh margin ⇒ the cache entry is
        // immediately stale and the second fetch goes to the wire.
        let exp = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap()
            + 60;
        Mock::given(header(
            "x-amz-target",
            "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response("AWS", "p", exp)))
        .expect(2)
        .mount(&server)
        .await;

        let strategy = AwsEcrStrategy::with_endpoint(server.uri()).await;
        strategy.fetch_creds().await.unwrap();
        strategy.fetch_creds().await.unwrap();
    }
}
