//! [GCP Secret Manager](https://cloud.google.com/secret-manager)-backed
//! [`SecretStore`].
//!
//! # Resolution
//!
//! Two ways an image's secret schema entry resolves to a backend
//! lookup, in priority order:
//!
//! 1. **Explicit `ref`** — `gcp-sm://projects/<project>/secrets/<name>/versions/<version>`
//!    (or just `<name>` to default to the configured project + `latest`).
//!    This is the production pattern when the image is built specifically
//!    for one deployment.
//!
//! 2. **Default namespacing** — when `schema.ref` is absent, the
//!    backend looks up `projects/<project>/secrets/<repo>--<name>/versions/latest`
//!    where `<repo>` has its `/` swapped for `--` (Secret Manager
//!    keys can't contain `/`). E.g. an image at `cortex/api` needing
//!    `GITHUB_TOKEN` resolves to secret name `cortex--api--GITHUB_TOKEN`.
//!    Predictable for ops; image-portable.
//!
//! # Authentication
//!
//! On GKE / GCE, [`MetadataTokenSource`] fetches a bearer token from
//! the instance metadata server using the pod's Workload Identity
//! mapping. The token is cached in memory and refreshed when within
//! 60 seconds of expiry. Tests inject [`StaticTokenSource`] to avoid
//! the metadata-server dependency.

pub mod ca;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use engram_core::traits::{SecretContext, SecretStore};
use engram_core::types::SecretSchema;
use engram_core::SecretError;
use serde::Deserialize;

/// Production Secret Manager API root. Overridable for tests via
/// [`GcpSecretManager::with_base_url`].
pub(crate) const DEFAULT_SECRETMANAGER_BASE: &str = "https://secretmanager.googleapis.com";

/// Default metadata-server token endpoint. Overridable for tests.
const DEFAULT_METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

// ---------------------------------------------------------------------
// TokenSource
// ---------------------------------------------------------------------

/// Fetches an OAuth bearer token suitable for calling
/// `secretmanager.googleapis.com`. Trait so tests can inject a
/// static value without hitting the metadata server.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self) -> Result<String, SecretError>;
}

/// Returns a fixed token. Test fixture.
#[derive(Clone, Debug)]
pub struct StaticTokenSource(pub String);

#[async_trait]
impl TokenSource for StaticTokenSource {
    async fn token(&self) -> Result<String, SecretError> {
        Ok(self.0.clone())
    }
}

/// Fetches tokens from the GCE/GKE instance metadata server. Caches
/// the token in memory until 60 s before expiry. The metadata server
/// itself caches across the same lifetime, but skipping the round-trip
/// keeps session-create latency tight when several secrets resolve in
/// a row.
pub struct MetadataTokenSource {
    http: reqwest::Client,
    endpoint: String,
    cache: Mutex<Option<CachedToken>>,
}

#[derive(Clone)]
struct CachedToken {
    bearer: String,
    not_after: Instant,
}

#[derive(Deserialize)]
struct MetadataTokenResponse {
    access_token: String,
    /// Seconds until expiry, per RFC 6749. Metadata server returns ~3600.
    expires_in: u64,
}

impl MetadataTokenSource {
    pub fn new() -> Result<Self, SecretError> {
        let http = engram_tls::client_builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| SecretError::Backend(Box::new(e)))?;
        Ok(Self {
            http,
            endpoint: DEFAULT_METADATA_TOKEN_URL.into(),
            cache: Mutex::new(None),
        })
    }

    /// Override the metadata endpoint. For tests pointing at a mock
    /// server.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }
}

#[async_trait]
impl TokenSource for MetadataTokenSource {
    async fn token(&self) -> Result<String, SecretError> {
        if let Some(tok) = self
            .cache
            .lock()
            .expect("token cache mutex poisoned")
            .as_ref()
        {
            if tok.not_after > Instant::now() {
                return Ok(tok.bearer.clone());
            }
        }

        let resp = self
            .http
            .get(&self.endpoint)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| SecretError::Backend(Box::new(e)))?;
        let status = resp.status();
        if status.is_client_error() {
            return Err(SecretError::Unauthorized(format!(
                "metadata-server returned {status} fetching access token"
            )));
        }
        if !status.is_success() {
            return Err(SecretError::Backend(
                format!("metadata-server returned {status} fetching access token").into(),
            ));
        }
        let body: MetadataTokenResponse = resp
            .json()
            .await
            .map_err(|e| SecretError::Protocol(format!("metadata-server token JSON: {e}")))?;

        // Refresh 60 s before actual expiry so we don't trip into an
        // expired token mid-request.
        let lifetime = Duration::from_secs(body.expires_in.saturating_sub(60).max(30));
        let cached = CachedToken {
            bearer: body.access_token.clone(),
            not_after: Instant::now() + lifetime,
        };
        *self.cache.lock().expect("token cache mutex poisoned") = Some(cached);
        Ok(body.access_token)
    }
}

// ---------------------------------------------------------------------
// GcpSecretManager
// ---------------------------------------------------------------------

#[derive(Clone)]
pub struct GcpSecretManager {
    pub project: String,
    http: reqwest::Client,
    base_url: String,
    token_source: Arc<dyn TokenSource>,
}

impl std::fmt::Debug for GcpSecretManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpSecretManager")
            .field("project", &self.project)
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl GcpSecretManager {
    /// Construct the production-default backend: metadata-server
    /// auth + the public Secret Manager API.
    pub fn new(project: impl Into<String>) -> Result<Self, SecretError> {
        let http = engram_tls::client_builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| SecretError::Backend(Box::new(e)))?;
        Ok(Self {
            project: project.into(),
            http,
            base_url: DEFAULT_SECRETMANAGER_BASE.into(),
            token_source: Arc::new(MetadataTokenSource::new()?),
        })
    }

    /// Override the Secret Manager API root. Tests point this at a
    /// `wiremock` server.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// Override the token source. Tests inject [`StaticTokenSource`].
    pub fn with_token_source(mut self, ts: Arc<dyn TokenSource>) -> Self {
        self.token_source = ts;
        self
    }

    /// Compute the Secret Manager resource path the backend will
    /// query for `(ctx, name, schema)`. Pure function so deployment
    /// authors can verify their config maps correctly.
    pub fn resolve_path(
        &self,
        ctx: &SecretContext<'_>,
        name: &str,
        schema: &SecretSchema,
    ) -> String {
        if let Some(r) = schema.r#ref.as_deref() {
            return parse_ref(&self.project, r);
        }
        // `cortex/api` + `GITHUB_TOKEN` → cortex--api--GITHUB_TOKEN
        let repo_safe = ctx.repo.replace('/', "--");
        format!(
            "projects/{}/secrets/{}--{}/versions/latest",
            self.project, repo_safe, name,
        )
    }
}

fn parse_ref(default_project: &str, raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("gcp-sm://") {
        // Already a full Secret Manager resource path or a relative
        // "secrets/<name>/versions/<v>". Pass through with project
        // prefix added if needed.
        if rest.starts_with("projects/") {
            return rest.to_string();
        }
        return format!("projects/{default_project}/{rest}");
    }
    // Bare name → default project, latest version.
    format!("projects/{default_project}/secrets/{raw}/versions/latest")
}

/// Wire shape of `AccessSecretVersionResponse`.
/// <https://cloud.google.com/secret-manager/docs/reference/rest/v1/projects.secrets.versions/access>
#[derive(Deserialize)]
struct AccessSecretVersionResponse {
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    /// Base64-encoded UTF-8 secret value (the API always base64-encodes,
    /// even for human-readable strings).
    data: String,
}

/// Low-level `AccessSecretVersion` fetcher: GET the resource path,
/// map status codes to typed errors, return the base64-decoded
/// payload bytes. Shared by the [`SecretStore`] impl (per-image
/// session secrets) and [`ca::GcpSecretManagerCaSource`]
/// (deployment-wide CA material).
///
/// Returns `Ok(None)` on 404 — caller decides whether absent is fatal.
pub(crate) async fn access_payload(
    http: &reqwest::Client,
    base_url: &str,
    token_source: &Arc<dyn TokenSource>,
    path: &str,
) -> Result<Option<Vec<u8>>, SecretError> {
    let url = format!("{base_url}/v1/{path}:access");
    let bearer = token_source.token().await?;

    let resp = http
        .get(&url)
        .bearer_auth(&bearer)
        .send()
        .await
        .map_err(|e| SecretError::Backend(Box::new(e)))?;

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(SecretError::Unauthorized(format!(
            "Secret Manager denied access to `{path}` (status 403); \
             check Workload Identity binding for `roles/secretmanager.secretAccessor`"
        )));
    }
    if status == reqwest::StatusCode::BAD_REQUEST {
        return Err(SecretError::InvalidRef(format!(
            "Secret Manager rejected path `{path}` (status 400)"
        )));
    }
    if !status.is_success() {
        return Err(SecretError::Backend(
            format!("Secret Manager AccessSecretVersion returned {status} for `{path}`").into(),
        ));
    }

    let body: AccessSecretVersionResponse = resp
        .json()
        .await
        .map_err(|e| SecretError::Protocol(format!("AccessSecretVersion JSON: {e}")))?;

    let bytes = BASE64_STANDARD
        .decode(body.payload.data.as_bytes())
        .map_err(|e| {
            SecretError::Protocol(format!("AccessSecretVersion payload not base64: {e}"))
        })?;
    Ok(Some(bytes))
}

#[async_trait]
impl SecretStore for GcpSecretManager {
    async fn get(
        &self,
        ctx: &SecretContext<'_>,
        name: &str,
        schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        let path = self.resolve_path(ctx, name, schema);
        let bytes =
            match access_payload(&self.http, &self.base_url, &self.token_source, &path).await? {
                Some(b) => b,
                None => return Ok(None),
            };
        let value = String::from_utf8(bytes).map_err(|_| {
            SecretError::BadValue(format!(
                "secret `{path}` payload is not valid UTF-8 (binary secrets unsupported)"
            ))
        })?;
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx() -> SecretContext<'static> {
        SecretContext {
            repo: "cortex/api",
            image_tag: "warm-1",
        }
    }

    fn schema(r#ref: Option<&str>) -> SecretSchema {
        SecretSchema {
            r#ref: r#ref.map(str::to_string),
            ..SecretSchema::default()
        }
    }

    fn manager(server: &MockServer) -> GcpSecretManager {
        GcpSecretManager::new("cortex-prod")
            .unwrap()
            .with_base_url(server.uri())
            .with_token_source(Arc::new(StaticTokenSource("test-token".into())))
    }

    #[test]
    fn resolve_path_namespaces_when_no_ref() {
        let s = GcpSecretManager::new("cortex-prod").unwrap();
        let path = s.resolve_path(&ctx(), "GITHUB_TOKEN", &schema(None));
        assert_eq!(
            path,
            "projects/cortex-prod/secrets/cortex--api--GITHUB_TOKEN/versions/latest",
        );
    }

    #[test]
    fn resolve_path_honors_full_ref() {
        let s = GcpSecretManager::new("cortex-prod").unwrap();
        let p = s.resolve_path(
            &ctx(),
            "GITHUB_TOKEN",
            &schema(Some(
                "gcp-sm://projects/shared-secrets/secrets/cortex-github/versions/3",
            )),
        );
        assert_eq!(
            p,
            "projects/shared-secrets/secrets/cortex-github/versions/3"
        );
    }

    #[test]
    fn resolve_path_honors_relative_ref() {
        let s = GcpSecretManager::new("cortex-prod").unwrap();
        let p = s.resolve_path(
            &ctx(),
            "GITHUB_TOKEN",
            &schema(Some("gcp-sm://secrets/team-github/versions/latest")),
        );
        assert_eq!(
            p,
            "projects/cortex-prod/secrets/team-github/versions/latest",
        );
    }

    #[test]
    fn resolve_path_treats_bare_string_as_secret_name_in_default_project() {
        let s = GcpSecretManager::new("cortex-prod").unwrap();
        let p = s.resolve_path(&ctx(), "GITHUB_TOKEN", &schema(Some("team-github")));
        assert_eq!(
            p,
            "projects/cortex-prod/secrets/team-github/versions/latest",
        );
    }

    #[tokio::test]
    async fn get_returns_decoded_payload_on_200() {
        let server = MockServer::start().await;
        let expected_path =
            "/v1/projects/cortex-prod/secrets/cortex--api--GITHUB_TOKEN/versions/latest:access";
        let payload = BASE64_STANDARD.encode(b"ghp_real_value");
        Mock::given(method("GET"))
            .and(path(expected_path))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "payload": { "data": payload } })),
            )
            .mount(&server)
            .await;
        let value = manager(&server)
            .get(&ctx(), "GITHUB_TOKEN", &schema(None))
            .await
            .unwrap();
        assert_eq!(value.as_deref(), Some("ghp_real_value"));
    }

    #[tokio::test]
    async fn get_maps_404_to_none() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let value = manager(&server)
            .get(&ctx(), "MISSING", &schema(None))
            .await
            .unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn get_maps_403_to_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        match manager(&server)
            .get(&ctx(), "GITHUB_TOKEN", &schema(None))
            .await
        {
            Err(SecretError::Unauthorized(_)) => {}
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_maps_400_to_invalid_ref() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        match manager(&server)
            .get(&ctx(), "GITHUB_TOKEN", &schema(None))
            .await
        {
            Err(SecretError::InvalidRef(_)) => {}
            other => panic!("expected InvalidRef, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_rejects_non_utf8_payload_as_bad_value() {
        let server = MockServer::start().await;
        // Invalid UTF-8 byte sequence
        let payload = BASE64_STANDARD.encode([0xff, 0xfe, 0xfd]);
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "payload": { "data": payload } })),
            )
            .mount(&server)
            .await;
        match manager(&server).get(&ctx(), "BINARY", &schema(None)).await {
            Err(SecretError::BadValue(_)) => {}
            other => panic!("expected BadValue, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn metadata_token_source_parses_and_caches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/computeMetadata/v1/instance/service-accounts/default/token",
            ))
            .and(header("metadata-flavor", "Google"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ya29.example",
                "expires_in": 3600,
                "token_type": "Bearer"
            })))
            .expect(1) // verify caching: should only fetch once
            .mount(&server)
            .await;
        let ts = MetadataTokenSource::new().unwrap().with_endpoint(format!(
            "{}/computeMetadata/v1/instance/service-accounts/default/token",
            server.uri()
        ));
        assert_eq!(ts.token().await.unwrap(), "ya29.example");
        // Second call must hit the cache, not the server.
        assert_eq!(ts.token().await.unwrap(), "ya29.example");
    }
}
