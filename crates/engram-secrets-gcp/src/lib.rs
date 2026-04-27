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
//! # Status
//!
//! **Stub.** Every lookup returns a structured error pointing at the
//! relevant Secret Manager API call. Phase 2-ish work to fill in
//! against the `google-cloud-secretmanager` crate (or directly via
//! REST + a service-account JWT). Not on the critical path for the
//! orchestration layer; lands when Cortex stands up the first
//! production deployment.

use async_trait::async_trait;
use engram_core::traits::{SecretContext, SecretStore};
use engram_core::types::SecretSchema;
use engram_core::SecretError;

#[derive(Clone, Debug)]
pub struct GcpSecretManager {
    pub project: String,
}

impl GcpSecretManager {
    pub fn new(project: impl Into<String>) -> Self {
        Self {
            project: project.into(),
        }
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

#[async_trait]
impl SecretStore for GcpSecretManager {
    async fn get(
        &self,
        ctx: &SecretContext<'_>,
        name: &str,
        schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        // TODO(prod): call SecretManagerService.AccessSecretVersion
        // on `self.resolve_path(ctx, name, schema)`. Use either:
        //   - google-cloud-secretmanager crate, or
        //   - direct REST (https://secretmanager.googleapis.com) with
        //     a service-account JWT.
        // Map NOT_FOUND -> Ok(None); PERMISSION_DENIED -> Unauthorized.
        let path = self.resolve_path(ctx, name, schema);
        Err(SecretError::Backend(
            format!(
                "GcpSecretManager.get not yet implemented (target: AccessSecretVersion {path})"
            )
            .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn resolve_path_namespaces_when_no_ref() {
        let s = GcpSecretManager::new("cortex-prod");
        let path = s.resolve_path(&ctx(), "GITHUB_TOKEN", &schema(None));
        assert_eq!(
            path,
            "projects/cortex-prod/secrets/cortex--api--GITHUB_TOKEN/versions/latest",
        );
    }

    #[test]
    fn resolve_path_honors_full_ref() {
        let s = GcpSecretManager::new("cortex-prod");
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
        let s = GcpSecretManager::new("cortex-prod");
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
        let s = GcpSecretManager::new("cortex-prod");
        let p = s.resolve_path(&ctx(), "GITHUB_TOKEN", &schema(Some("team-github")));
        assert_eq!(
            p,
            "projects/cortex-prod/secrets/team-github/versions/latest",
        );
    }

    #[tokio::test]
    async fn get_returns_typed_stub_referencing_target_api() {
        let s = GcpSecretManager::new("cortex-prod");
        match s.get(&ctx(), "GITHUB_TOKEN", &schema(None)).await {
            Err(SecretError::Backend(e)) => {
                let msg = e.to_string();
                assert!(msg.contains("AccessSecretVersion"));
                assert!(msg.contains("cortex--api--GITHUB_TOKEN"));
            }
            other => panic!("expected Backend stub error, got {other:?}"),
        }
    }
}
