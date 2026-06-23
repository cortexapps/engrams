//! ADR 0056/0057: the coordinator's registry of provider [`Integration`]s.
//!
//! Subsumes the single `state.forge: Option<Arc<dyn GitForge>>`. Keyed by
//! `Integration::provider()`, so the forge seam looks up `"github"` and a future
//! provider registers alongside it without new platform plumbing.
//!
//! ADR 0057 C2: mint engines are no longer built eagerly from coordinator boot
//! env. The broker carries a **mint-kind registry** (data-driven descriptors,
//! one per built-in mint crate) and builds engines **lazily** from the composed
//! [`SecretStore`] (org-secret store → deployment → boot-env fallback) on first
//! use, caching them under a `version` that an org-secret rotation bumps
//! ([`IntegrationBroker::invalidate`]). The eager path stays for tests + any
//! explicitly-registered integration.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use engram_core::traits::{
    Integration, MintKindDescriptor, ResolvedFields, SecretContext, SecretStore,
};
use engram_core::types::SecretSchema;
use parking_lot::Mutex;

/// Every built-in mint kind the coordinator knows how to build. Surfaced over
/// gRPC in C3 (`ListMintKinds`) so the Plane-A admin form is data-driven; the
/// mint *logic* stays bespoke Rust (each crate's engine).
pub fn mint_kind_registry() -> Vec<MintKindDescriptor> {
    vec![engram_git_github::github_app_descriptor()]
}

/// A lazily-built mint engine tagged with the secret `version` it was built at
/// (a mismatch with the broker's current version triggers a rebuild).
type CachedEngine = (u64, Arc<dyn Integration>);

/// Provider id → integration, with lazy mint-engine resolution. Cheap to clone
/// (the lazy cache + version are shared via `Arc`, so a clone in `pg_listener`
/// can `invalidate` the same cache the request path reads).
#[derive(Clone)]
pub struct IntegrationBroker {
    /// Eagerly-registered integrations (tests inject fakes; an explicit
    /// registration wins over the lazy mint path).
    eager: HashMap<String, Arc<dyn Integration>>,
    /// provider → mint-kind descriptor (from the registry); drives lazy builds.
    by_provider: HashMap<String, MintKindDescriptor>,
    /// Lazily-built engines, keyed by provider, tagged with the secret `version`
    /// they were built at. Shared across clones.
    lazy: Arc<Mutex<HashMap<String, CachedEngine>>>,
    /// Bumped on `org_secret_changed` to invalidate the lazy cache.
    version: Arc<AtomicU64>,
}

impl Default for IntegrationBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl IntegrationBroker {
    /// An empty broker carrying the built-in mint-kind registry (production).
    pub fn new() -> Self {
        Self::with_registry(mint_kind_registry())
    }

    /// A broker over a specific mint-kind registry (tests).
    pub fn with_registry(descriptors: Vec<MintKindDescriptor>) -> Self {
        let by_provider = descriptors
            .into_iter()
            .map(|d| (d.provider.to_string(), d))
            .collect();
        Self {
            eager: HashMap::new(),
            by_provider,
            lazy: Arc::new(Mutex::new(HashMap::new())),
            version: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Build a broker holding a single eager integration (no mint-kind registry).
    /// The common test shape — the eager engine is the only provider.
    pub fn with(integration: Arc<dyn Integration>) -> Self {
        let mut b = Self::with_registry(Vec::new());
        b.register(integration);
        b
    }

    /// Register (or replace) an eager integration for its `provider()`. Eager
    /// integrations win over the lazy mint path.
    pub fn register(&mut self, integration: Arc<dyn Integration>) {
        self.eager
            .insert(integration.provider().to_string(), integration);
    }

    /// Invalidate the lazy engine cache (call on org-secret rotation). The next
    /// [`resolve`](Self::resolve) rebuilds engines from the updated secret store.
    pub fn invalidate(&self) {
        self.version.fetch_add(1, Ordering::Release);
    }

    /// Resolve the integration for `provider`: an eagerly-registered one wins;
    /// otherwise lazily build the mint engine from the composed `SecretStore`
    /// (org store → deployment → boot-env fallback) and cache it until the next
    /// [`invalidate`](Self::invalidate). Returns `None` when the provider has no
    /// engine — no eager registration, no mint kind, or a required field that the
    /// secret store can't resolve (e.g. the App key isn't configured yet).
    pub async fn resolve(
        &self,
        provider: &str,
        secrets: &Arc<dyn SecretStore>,
    ) -> Option<Arc<dyn Integration>> {
        if let Some(e) = self.eager.get(provider) {
            return Some(e.clone());
        }
        let desc = self.by_provider.get(provider)?;
        let version = self.version.load(Ordering::Acquire);
        if let Some((v, eng)) = self.lazy.lock().get(provider) {
            if *v == version {
                return Some(eng.clone());
            }
        }
        // Resolve each field from the composed secret store, keyed
        // `<kind>.<field>` so the Plane-A write + this read agree. A missing
        // required field → no engine (the App key just isn't configured yet).
        let ctx = SecretContext {
            repo: "",
            image_tag: "",
        };
        let mut fields: ResolvedFields = HashMap::new();
        for f in &desc.fields {
            let name = format!("{}.{}", desc.kind, f.name);
            let schema = SecretSchema {
                required: f.required,
                ..Default::default()
            };
            match secrets.get(&ctx, &name, &schema).await {
                Ok(Some(v)) => {
                    fields.insert(f.name.to_string(), v);
                }
                Ok(None) => {
                    if f.required {
                        tracing::debug!(
                            provider,
                            field = f.name,
                            "mint engine not built: required field not configured"
                        );
                        return None;
                    }
                }
                Err(e) => {
                    tracing::warn!(provider, field = f.name, error = %e, "mint field resolve failed");
                    return None;
                }
            }
        }
        let engine = match (desc.build)(&fields) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(provider, error = %e, "mint engine build failed");
                return None;
            }
        };
        self.lazy
            .lock()
            .insert(provider.to_string(), (version, engine.clone()));
        Some(engine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use engram_core::error::{IntegrationError, SecretError};
    use engram_core::traits::{CredentialHint, MintFieldKind, MintFieldSchema, ScopedCredential};
    use engram_core::types::Capability;

    /// A fake mint engine that echoes the token it was built from (so a test can
    /// observe *which* secret value built it).
    struct FakeEngine {
        token: String,
    }
    #[async_trait]
    impl Integration for FakeEngine {
        fn provider(&self) -> &str {
            "fake"
        }
        async fn mint_credential(
            &self,
            _caps: &[Capability],
            _hint: &CredentialHint,
        ) -> Result<ScopedCredential, IntegrationError> {
            Ok(ScopedCredential::Bearer {
                token: self.token.clone(),
                expires_at: chrono::Utc::now(),
            })
        }
    }

    fn fake_descriptor() -> MintKindDescriptor {
        MintKindDescriptor {
            kind: "fake_kind",
            provider: "fake",
            display_name: "Fake",
            fields: vec![MintFieldSchema {
                name: "token",
                label: "Token",
                field_kind: MintFieldKind::SealedSecret,
                required: true,
            }],
            build: |fields| {
                let token = fields
                    .get("token")
                    .ok_or_else(|| IntegrationError::InvalidSpec("missing token".into()))?;
                Ok(Arc::new(FakeEngine {
                    token: token.clone(),
                }) as Arc<dyn Integration>)
            },
        }
    }

    /// A mutable fake `SecretStore` so a test can rotate a value mid-run.
    struct MutSecrets {
        map: Mutex<HashMap<String, String>>,
    }
    impl MutSecrets {
        fn raw(pairs: &[(&str, &str)]) -> Self {
            Self {
                map: Mutex::new(
                    pairs
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
            }
        }
        fn set(&self, name: &str, value: &str) {
            self.map.lock().insert(name.to_string(), value.to_string());
        }
    }
    #[async_trait]
    impl SecretStore for MutSecrets {
        async fn get(
            &self,
            _ctx: &SecretContext<'_>,
            name: &str,
            _schema: &SecretSchema,
        ) -> Result<Option<String>, SecretError> {
            Ok(self.map.lock().get(name).cloned())
        }
    }

    async fn minted(eng: &Arc<dyn Integration>) -> String {
        match eng
            .mint_credential(&[], &CredentialHint::default())
            .await
            .unwrap()
        {
            ScopedCredential::Bearer { token, .. } => token,
            other => panic!("expected bearer, got {other:?}"),
        }
    }

    fn broker() -> IntegrationBroker {
        IntegrationBroker::with_registry(vec![fake_descriptor()])
    }

    #[tokio::test]
    async fn resolves_lazily_from_the_secret_store() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MutSecrets::raw(&[("fake_kind.token", "v1")]));
        let eng = broker().resolve("fake", &secrets).await.expect("engine");
        assert_eq!(minted(&eng).await, "v1");
    }

    #[tokio::test]
    async fn missing_required_field_yields_no_engine() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MutSecrets::raw(&[]));
        assert!(broker().resolve("fake", &secrets).await.is_none());
    }

    #[tokio::test]
    async fn unknown_provider_yields_no_engine() {
        let secrets: Arc<dyn SecretStore> = Arc::new(MutSecrets::raw(&[("fake_kind.token", "v1")]));
        assert!(broker().resolve("nope", &secrets).await.is_none());
    }

    #[tokio::test]
    async fn eager_registration_wins_over_the_lazy_path() {
        let secrets: Arc<dyn SecretStore> =
            Arc::new(MutSecrets::raw(&[("fake_kind.token", "from-secrets")]));
        let mut b = broker();
        b.register(Arc::new(FakeEngine {
            token: "from-eager".into(),
        }));
        let eng = b.resolve("fake", &secrets).await.expect("engine");
        assert_eq!(minted(&eng).await, "from-eager");
    }

    #[tokio::test]
    async fn caches_until_invalidated() {
        let concrete = Arc::new(MutSecrets::raw(&[("fake_kind.token", "v1")]));
        let secrets: Arc<dyn SecretStore> = concrete.clone();
        let b = broker();
        assert_eq!(
            minted(&b.resolve("fake", &secrets).await.unwrap()).await,
            "v1"
        );
        // Rotate the value WITHOUT invalidating → the cached engine still mints v1.
        concrete.set("fake_kind.token", "v2");
        assert_eq!(
            minted(&b.resolve("fake", &secrets).await.unwrap()).await,
            "v1"
        );
        // Invalidate (as pg_listener does on org_secret_changed) → rebuild → v2.
        b.invalidate();
        assert_eq!(
            minted(&b.resolve("fake", &secrets).await.unwrap()).await,
            "v2"
        );
    }
}
