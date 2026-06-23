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
use std::sync::Arc;

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

/// ADR 0057: compose N [`SecretStore`] backends into one, first-hit-wins on
/// `get`. The coordinator layers the org-secret backend in front of the
/// deployment backend (GCP SM / env): an admin-entered org secret resolves
/// from the org store, and anything it doesn't hold falls through to the
/// deployment backend (image/manifest env secrets, ops-provisioned refs).
///
/// A backend that *errors* (vs. returns `Ok(None)`) propagates — a broken
/// backend is a real fault, not a silent fall-through. `resolve` rides the
/// trait default (a per-name loop over this `get`), so a batched-`resolve`
/// backend is queried per-name when layered; correctness is unchanged.
pub struct LayeredSecretStore {
    backends: Vec<Arc<dyn SecretStore>>,
}

impl LayeredSecretStore {
    /// Backends are consulted in order; the first `Ok(Some)` wins.
    pub fn new(backends: Vec<Arc<dyn SecretStore>>) -> Self {
        Self { backends }
    }
}

#[async_trait]
impl SecretStore for LayeredSecretStore {
    async fn get(
        &self,
        ctx: &SecretContext<'_>,
        name: &str,
        schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        for backend in &self.backends {
            if let Some(value) = backend.get(ctx, name, schema).await? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
}

/// A fixed in-memory [`SecretStore`] over a known `name → value` map. Used to
/// layer deployment-provided fallback values behind the org-secret store — e.g.
/// during the ADR 0057 migration the coordinator seeds the boot-env GitHub App
/// key (`<kind>.<field>` names) here, layered *behind the org store but ahead of
/// the deployment backend*: an admin-entered org secret still takes precedence,
/// while a deployment backend that errors on the synthetic name can't block the
/// fallback (it's consulted first). Minting keeps working until the boot env is
/// dropped. Ignores `SecretContext` (global, name-keyed values).
pub struct StaticSecretStore {
    map: HashMap<String, String>,
}

impl StaticSecretStore {
    pub fn new(map: HashMap<String, String>) -> Self {
        Self { map }
    }
}

#[async_trait]
impl SecretStore for StaticSecretStore {
    async fn get(
        &self,
        _ctx: &SecretContext<'_>,
        name: &str,
        _schema: &SecretSchema,
    ) -> Result<Option<String>, SecretError> {
        Ok(self.map.get(name).cloned())
    }
}

#[cfg(test)]
mod layered_tests {
    use super::*;

    struct Fixed {
        map: HashMap<String, String>,
    }
    #[async_trait]
    impl SecretStore for Fixed {
        async fn get(
            &self,
            _ctx: &SecretContext<'_>,
            name: &str,
            _schema: &SecretSchema,
        ) -> Result<Option<String>, SecretError> {
            Ok(self.map.get(name).cloned())
        }
    }

    struct AlwaysErr;
    #[async_trait]
    impl SecretStore for AlwaysErr {
        async fn get(
            &self,
            _ctx: &SecretContext<'_>,
            _name: &str,
            _schema: &SecretSchema,
        ) -> Result<Option<String>, SecretError> {
            Err(SecretError::Backend("boom".into()))
        }
    }

    fn ctx() -> SecretContext<'static> {
        SecretContext {
            repo: "cortex/api",
            image_tag: "warm-1",
        }
    }
    fn fixed(pairs: &[(&str, &str)]) -> Arc<dyn SecretStore> {
        Arc::new(Fixed {
            map: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
    }

    #[tokio::test]
    async fn first_backend_with_a_hit_wins() {
        let layered = LayeredSecretStore::new(vec![
            fixed(&[("A", "from-first")]),
            fixed(&[("A", "from-second"), ("B", "from-second")]),
        ]);
        // `A` resolves from the first backend (the org store, layered ahead).
        assert_eq!(
            layered
                .get(&ctx(), "A", &SecretSchema::default())
                .await
                .unwrap(),
            Some("from-first".into())
        );
        // `B` is absent from the first → falls through to the second.
        assert_eq!(
            layered
                .get(&ctx(), "B", &SecretSchema::default())
                .await
                .unwrap(),
            Some("from-second".into())
        );
        // Absent everywhere → None (caller decides if required).
        assert_eq!(
            layered
                .get(&ctx(), "MISSING", &SecretSchema::default())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_backend_error_propagates_rather_than_falling_through() {
        // The first (org) backend faulting must NOT silently resolve from the
        // deployment fallback — fail loud.
        let layered =
            LayeredSecretStore::new(vec![Arc::new(AlwaysErr), fixed(&[("A", "fallback")])]);
        assert!(layered
            .get(&ctx(), "A", &SecretSchema::default())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_earlier_hit_shields_a_later_erroring_backend() {
        // ADR 0057 C2 safety: the boot-env mint-key fallback (a `StaticSecretStore`) is
        // layered AHEAD of the deployment backend, so a deployment backend that *errors*
        // on the synthetic `<kind>.<field>` name (e.g. GCP SM 400 on an invalid secret id)
        // cannot break minting — an earlier hit wins before the erroring backend is ever
        // consulted. This is the property that keeps git minting working on the C2 roll.
        let layered = LayeredSecretStore::new(vec![
            fixed(&[("github_app.app_id", "123")]),
            Arc::new(AlwaysErr),
        ]);
        assert_eq!(
            layered
                .get(&ctx(), "github_app.app_id", &SecretSchema::default())
                .await
                .unwrap(),
            Some("123".into())
        );
    }
}
