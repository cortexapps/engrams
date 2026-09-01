//! [AWS Secrets Manager](https://docs.aws.amazon.com/secretsmanager/)-backed
//! [`SecretStore`] (ADR 0122) — the AWS twin of `engram-secrets-gcp`.
//!
//! # Resolution
//!
//! Two ways an image's secret schema entry resolves to a backend
//! lookup, in priority order:
//!
//! 1. **Explicit `ref`** — `aws-sm://<name-or-arn>[#<version-stage>]`.
//!    The body is anything `GetSecretValue` accepts as a `SecretId`
//!    (a friendly name or a full ARN); an optional `#stage` fragment
//!    pins a version stage (default `AWSCURRENT`). This is the
//!    production pattern when the image is built for one deployment.
//!
//! 2. **Default namespacing** — when `schema.ref` is absent, the
//!    backend looks up `engram/<repo>/<name>`. Secrets Manager names
//!    allow `/`, so the repo rides verbatim (no `--` mangling like
//!    GCP needs) and IAM prefix policies
//!    (`secretsmanager:GetSecretValue` on `engram/*`) scope cleanly.
//!    E.g. an image at `cortex/api` needing `GITHUB_TOKEN` resolves
//!    to `engram/cortex/api/GITHUB_TOKEN`.
//!
//! # Authentication
//!
//! The SDK default chain via `engram-aws` — IRSA on EKS. Unlike GKE
//! Workload Identity (a metadata-server intercept that hostNetwork
//! pods bypass), IRSA is env/file-based and works in hostNetwork pods
//! too; the IMDS fallback yields the node instance role. Access-denied
//! errors carry that hint. SigV4 signing is why this crate rides the
//! SDK instead of mirroring the GCP crate's hand-rolled REST: the
//! `TokenSource` seam over a bearer header has no cheap AWS analogue.

pub mod ca;

use async_trait::async_trait;
use aws_sdk_secretsmanager::error::SdkError;
use aws_sdk_secretsmanager::Client;

use engram_core::traits::{SecretContext, SecretStore};
use engram_core::types::SecretSchema;
use engram_core::SecretError;

/// Namespace prefix for default (ref-less) resolution.
const DEFAULT_NAMESPACE: &str = "engram";

/// A parsed secret reference: what `GetSecretValue` calls `SecretId`,
/// plus an optional version stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretRef {
    pub secret_id: String,
    pub version_stage: Option<String>,
}

/// Parse an explicit `aws-sm://` ref, or treat a bare string as a
/// literal `SecretId` (mirroring the GCP backend's bare-name rule).
pub(crate) fn parse_ref(raw: &str) -> SecretRef {
    let body = raw.strip_prefix("aws-sm://").unwrap_or(raw);
    match body.rsplit_once('#') {
        // ARNs contain `#`-free colons only, so a `#` split is safe;
        // an empty stage (trailing `#`) means "default".
        Some((id, stage)) if !stage.is_empty() => SecretRef {
            secret_id: id.to_string(),
            version_stage: Some(stage.to_string()),
        },
        Some((id, _)) => SecretRef {
            secret_id: id.to_string(),
            version_stage: None,
        },
        None => SecretRef {
            secret_id: body.to_string(),
            version_stage: None,
        },
    }
}

/// AWS Secrets Manager-backed store. One client per instance, cheap
/// to clone.
#[derive(Clone, Debug)]
pub struct AwsSecretsManager {
    client: Client,
}

impl AwsSecretsManager {
    /// Production constructor: SDK default chains (region + IRSA
    /// credentials). Fails closed when no region resolves — a
    /// mis-configured process must refuse to come up.
    pub async fn new() -> Result<Self, SecretError> {
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides::default()).await;
        if cfg.region().is_none() && cfg.endpoint_url().is_none() {
            return Err(SecretError::Backend(
                "no AWS region resolved; set AWS_REGION or run with IRSA".into(),
            ));
        }
        Ok(Self {
            client: Client::new(&cfg),
        })
    }

    /// Test constructor: explicit endpoint (wiremock) + pinned region.
    pub async fn with_endpoint(endpoint: impl Into<String>) -> Self {
        let cfg = engram_aws::sdk_config(engram_aws::AwsOverrides {
            region: Some("us-east-1".into()),
            endpoint_url: Some(endpoint.into()),
        })
        .await;
        Self {
            client: Client::new(&cfg),
        }
    }

    /// Compute the `SecretId` (+ optional version stage) the backend
    /// will query for `(ctx, name, schema)`. Pure function so
    /// deployment authors can verify their config maps correctly.
    pub fn resolve_ref(
        &self,
        ctx: &SecretContext<'_>,
        name: &str,
        schema: &SecretSchema,
    ) -> SecretRef {
        if let Some(r) = schema.r#ref.as_deref() {
            return parse_ref(r);
        }
        // Deployment-level lookups (org secrets, resolved with an empty
        // repo) live directly under the namespace. Composing the repo
        // segment anyway produced `engram//name` — Secrets Manager
        // tolerates the double slash but nothing sane is stored under
        // it, so the lookup could never succeed (the GCP twin of this
        // bug 400'd instead; both found on the first Phase G walk).
        if ctx.repo.is_empty() {
            return SecretRef {
                secret_id: format!("{DEFAULT_NAMESPACE}/{name}"),
                version_stage: None,
            };
        }
        SecretRef {
            secret_id: format!("{DEFAULT_NAMESPACE}/{}/{name}", ctx.repo),
            version_stage: None,
        }
    }
}

/// Per-call deadline. The shared `engram-aws` transport bounds only
/// the CONNECT (5 s) and disables SDK retries — correct for the
/// GB-scale blob path, but a secret read that stalls after the
/// handshake (an LB draining mid-response, a partial partition)
/// would otherwise hang forever, wedging session-create and
/// host-agent boot instead of failing them. 10 s matches the GCP
/// twin's reqwest operation timeout
/// (`engram-secrets-gcp`'s client builder).
const OPERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Low-level `GetSecretValue`: map service errors to typed
/// [`SecretError`]s, return the string payload. Shared by the
/// [`SecretStore`] impl (per-image session secrets) and
/// [`ca::AwsSecretsManagerCaSource`] (deployment-wide CA material).
///
/// Returns `Ok(None)` when the secret (or the requested stage) does
/// not exist — caller decides whether absent is fatal.
pub(crate) async fn get_secret_string(
    client: &Client,
    sref: &SecretRef,
) -> Result<Option<String>, SecretError> {
    get_secret_string_with_deadline(client, sref, OPERATION_TIMEOUT).await
}

/// Deadline-parameterized inner so the stall path unit-tests in
/// milliseconds instead of waiting out the production 10 s.
async fn get_secret_string_with_deadline(
    client: &Client,
    sref: &SecretRef,
    deadline: std::time::Duration,
) -> Result<Option<String>, SecretError> {
    let mut req = client.get_secret_value().secret_id(&sref.secret_id);
    if let Some(stage) = &sref.version_stage {
        req = req.version_stage(stage);
    }
    let sent = tokio::time::timeout(deadline, req.send())
        .await
        .map_err(|_| {
            SecretError::Backend(
                format!(
                    "Secrets Manager GetSecretValue for `{}` exceeded the {:?} deadline",
                    sref.secret_id, deadline
                )
                .into(),
            )
        })?;
    let out = match sent {
        Ok(out) => out,
        Err(e) => {
            let id = &sref.secret_id;
            return match &e {
                SdkError::ServiceError(ctx) if ctx.err().is_resource_not_found_exception() => {
                    Ok(None)
                }
                SdkError::ServiceError(ctx) if ctx.err().is_invalid_parameter_exception() => {
                    Err(SecretError::InvalidRef(format!(
                        "Secrets Manager rejected SecretId `{id}`: {e}"
                    )))
                }
                SdkError::ServiceError(ctx)
                    if ctx.err().meta().code() == Some("AccessDeniedException") =>
                {
                    Err(SecretError::Unauthorized(format!(
                        "Secrets Manager denied access to `{id}`; check the IRSA role \
                         (or node instance role) for `secretsmanager:GetSecretValue`"
                    )))
                }
                _ => Err(SecretError::Backend(Box::new(e))),
            };
        }
    };
    match (out.secret_string, out.secret_binary) {
        (Some(s), _) => Ok(Some(s)),
        (None, Some(_)) => Err(SecretError::BadValue(format!(
            "secret `{}` holds a binary payload (binary secrets unsupported)",
            sref.secret_id
        ))),
        (None, None) => Err(SecretError::Protocol(format!(
            "GetSecretValue for `{}` returned neither SecretString nor SecretBinary",
            sref.secret_id
        ))),
    }
}

#[async_trait]
impl SecretStore for AwsSecretsManager {
    async fn get(
        &self,
        ctx: &SecretContext<'_>,
        name: &str,
        schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        let sref = self.resolve_ref(ctx, name, schema);
        get_secret_string(&self.client, &sref).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use wiremock::matchers::{body_string_contains, header};
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

    // ---- resolve_ref (pure) ----

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_ref_namespaces_when_no_ref() {
        let s = AwsSecretsManager::with_endpoint("http://127.0.0.1:1").await;
        let sref = s.resolve_ref(&ctx(), "GITHUB_TOKEN", &schema(None));
        assert_eq!(sref.secret_id, "engram/cortex/api/GITHUB_TOKEN");
        assert_eq!(sref.version_stage, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn repo_less_context_composes_directly_under_the_namespace() {
        // Org secrets resolve with `repo: ""` (deployment-wide). The
        // path must be `engram/<name>` — the old compose produced
        // `engram//<name>`, which nothing sane is stored under.
        let s = AwsSecretsManager::with_endpoint("http://127.0.0.1:1").await;
        let ctx = SecretContext {
            repo: "",
            image_tag: "",
        };
        let sref = s.resolve_ref(&ctx, "openrouter.api_key", &schema(None));
        assert_eq!(sref.secret_id, "engram/openrouter.api_key");
        assert_eq!(sref.version_stage, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_ref_parses_scheme_arn_and_stage() {
        let s = AwsSecretsManager::with_endpoint("http://127.0.0.1:1").await;
        let arn = "arn:aws:secretsmanager:us-west-2:123456789012:secret:prod/api-AbCdEf";
        let sref = s.resolve_ref(
            &ctx(),
            "TOKEN",
            &schema(Some(&format!("aws-sm://{arn}#AWSPREVIOUS"))),
        );
        assert_eq!(sref.secret_id, arn);
        assert_eq!(sref.version_stage.as_deref(), Some("AWSPREVIOUS"));

        // Bare body without the scheme is a literal SecretId (the
        // GCP bare-name rule's twin).
        let bare = s.resolve_ref(&ctx(), "TOKEN", &schema(Some("my-secret")));
        assert_eq!(bare.secret_id, "my-secret");
        assert_eq!(bare.version_stage, None);
    }

    // ---- wire tests (wiremock, JSON protocol) ----

    async fn manager(server: &MockServer) -> AwsSecretsManager {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        AwsSecretsManager::with_endpoint(server.uri()).await
    }

    fn sm_target() -> wiremock::matchers::HeaderExactMatcher {
        header("x-amz-target", "secretsmanager.GetSecretValue")
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_returns_the_secret_string() {
        let server = MockServer::start().await;
        Mock::given(sm_target())
            .and(body_string_contains("engram/cortex/api/GITHUB_TOKEN"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Name": "engram/cortex/api/GITHUB_TOKEN",
                "SecretString": "hunter2"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let got = manager(&server)
            .await
            .get(&ctx(), "GITHUB_TOKEN", &schema(None))
            .await
            .expect("get");
        assert_eq!(got.as_deref(), Some("hunter2"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_secret_maps_to_none() {
        let server = MockServer::start().await;
        Mock::given(sm_target())
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "ResourceNotFoundException",
                "message": "Secrets Manager can't find the specified secret."
            })))
            .expect(1)
            .mount(&server)
            .await;

        let got = manager(&server)
            .await
            .get(&ctx(), "MISSING", &schema(None))
            .await
            .expect("absent secret is Ok(None)");
        assert_eq!(got, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn access_denied_maps_to_unauthorized_with_irsa_hint() {
        let server = MockServer::start().await;
        Mock::given(sm_target())
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "AccessDeniedException",
                "Message": "User is not authorized to perform: secretsmanager:GetSecretValue"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = manager(&server)
            .await
            .get(&ctx(), "DENIED", &schema(None))
            .await
            .expect_err("denied must error");
        match err {
            SecretError::Unauthorized(msg) => {
                assert!(msg.contains("IRSA"), "hint missing: {msg}");
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_ref_maps_to_typed_error() {
        let server = MockServer::start().await;
        Mock::given(sm_target())
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "__type": "InvalidParameterException",
                "message": "Invalid name."
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = manager(&server)
            .await
            .get(&ctx(), "BAD", &schema(Some("aws-sm://???")))
            .await
            .expect_err("invalid ref must error");
        assert!(matches!(err, SecretError::InvalidRef(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn binary_secret_is_a_typed_bad_value() {
        let server = MockServer::start().await;
        Mock::given(sm_target())
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "Name": "engram/cortex/api/BLOB",
                "SecretBinary": "aGVsbG8="
            })))
            .expect(1)
            .mount(&server)
            .await;

        let err = manager(&server)
            .await
            .get(&ctx(), "BLOB", &schema(None))
            .await
            .expect_err("binary must error");
        assert!(matches!(err, SecretError::BadValue(_)));
    }

    /// A response that stalls past the deadline surfaces as a typed
    /// Backend error instead of hanging the caller (session-create /
    /// host-agent boot). Tested via the deadline-parameterized inner
    /// so it runs in milliseconds.
    #[tokio::test(flavor = "current_thread")]
    async fn stalled_response_hits_the_deadline() {
        let server = MockServer::start().await;
        Mock::given(sm_target())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "SecretString": "late" }))
                    .set_delay(std::time::Duration::from_millis(500)),
            )
            .mount(&server)
            .await;

        let m = manager(&server).await;
        let sref = SecretRef {
            secret_id: "engram/stalled".into(),
            version_stage: None,
        };
        let err =
            get_secret_string_with_deadline(&m.client, &sref, std::time::Duration::from_millis(50))
                .await
                .expect_err("a stalled response must fail, not hang");
        assert!(
            format!("{err}").contains("deadline"),
            "error must name the deadline: {err}"
        );
    }

    /// A pinned version stage rides the request body.
    #[tokio::test(flavor = "current_thread")]
    async fn version_stage_is_sent_on_the_wire() {
        let server = MockServer::start().await;
        Mock::given(sm_target())
            .and(body_string_contains("\"VersionStage\":\"AWSPREVIOUS\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "SecretString": "old-value"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let got = manager(&server)
            .await
            .get(
                &ctx(),
                "TOKEN",
                &schema(Some("aws-sm://my-secret#AWSPREVIOUS")),
            )
            .await
            .expect("get");
        assert_eq!(got.as_deref(), Some("old-value"));
    }
}
