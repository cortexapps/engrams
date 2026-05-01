//! Pluggable secret store. Hot-swappable across deployments without
//! changing image manifests.
//!
//! # Why a trait
//!
//! Image manifests declare *which* secrets a sandbox needs and what
//! hosts each may be substituted on (`SecretSchema`) — they NEVER
//! carry values. Values come from the SecretStore at session-create
//! time. The same image manifest runs against:
//!
//! - **`InMemorySecretStore`** in tests (fixed map of test secrets),
//! - **`EnvSecretStore`** in dev (reads `$NAME` from the host env),
//! - **`GcpSecretManager`** in production (resolves
//!   `gcp-sm://...` refs or namespaces `<repo>/<name>`),
//! - **Vault / AWS Secrets Manager / K8s Secrets** as additional
//!   `engram-secrets-*` crates that ship later.
//!
//! Switching backends is a coordinator config flag.
//!
//! # Resolution
//!
//! 1. Coordinator loads the image manifest at session create.
//! 2. For each entry in `manifest.secrets`, calls
//!    [`SecretStore::resolve`] passing the schema map.
//! 3. The store returns a `SecretBundle` mapping name → value.
//!    Required-but-missing secrets cause an error; optional
//!    secrets that aren't present are simply absent from the bundle.
//! 4. Coordinator hands the bundle + manifest's `secret_mode` to the
//!    sandbox backend, which materialises env vars accordingly
//!    (`Literal` = real values, `Broker` = placeholders + proxy).

use std::collections::HashMap;

use async_trait::async_trait;

use crate::error::SecretError;
use crate::types::SecretSchema;

/// Resolution context — the per-image data a SecretStore can use to
/// disambiguate secret names. Backends that use namespacing (e.g.
/// `<repo>/<secret_name>`) need this; backends that just look up by
/// `name` directly (e.g. `EnvSecretStore`) ignore it.
#[derive(Clone, Debug)]
pub struct SecretContext<'a> {
    pub repo: &'a str,
    pub image_tag: &'a str,
}

/// Resolved name → value bundle. The `schema` for each secret is
/// passed through so downstream code (the broker proxy) can consult
/// the per-secret allow_hosts policy.
#[derive(Clone, Debug, Default)]
pub struct SecretBundle {
    pub secrets: HashMap<String, ResolvedSecret>,
}

#[derive(Clone, Debug)]
pub struct ResolvedSecret {
    pub value: String,
    pub schema: SecretSchema,
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Look up a single secret. Returns `Ok(None)` for "not present"
    /// (caller decides if that's fatal based on `schema.required`).
    /// `schema.ref` may be set, in which case the backend should
    /// resolve via the ref; otherwise namespacing is the backend's
    /// choice (see `SecretContext`).
    async fn get(
        &self,
        ctx: &SecretContext<'_>,
        name: &str,
        schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError>;

    /// Resolve a manifest's full secret schema map. Default impl
    /// loops over `get`; backends that can batch (single RPC for all
    /// secrets) override. Errors if a `required` secret is missing.
    ///
    /// `overrides` is a per-request escape hatch: any name present in
    /// the override map short-circuits the backend lookup and uses the
    /// supplied value verbatim. Used by the dashboard's "create
    /// session" form, where a user pastes a credential into the
    /// browser instead of relying on host-process env. Only safe under
    /// `SecretMode::Literal` — the broker mode's per-session proxy
    /// doesn't know about request-scoped secrets.
    async fn resolve(
        &self,
        ctx: &SecretContext<'_>,
        schema: &HashMap<String, SecretSchema>,
        overrides: Option<&HashMap<String, String>>,
    ) -> Result<SecretBundle, SecretError> {
        let mut bundle = SecretBundle::default();
        for (name, sch) in schema {
            let override_value = overrides.and_then(|m| m.get(name));
            let resolved = match override_value {
                Some(v) => Some(v.clone()),
                None => self.get(ctx, name, sch).await?,
            };
            match resolved {
                Some(value) => {
                    bundle.secrets.insert(
                        name.clone(),
                        ResolvedSecret {
                            value,
                            schema: sch.clone(),
                        },
                    );
                }
                None if sch.required => {
                    return Err(SecretError::Backend(
                        format!("required secret `{name}` is not available in this deployment")
                            .into(),
                    ));
                }
                None => {} // optional + absent → just skip
            }
        }
        Ok(bundle)
    }
}
