//! Auth strategies for [`engram_oci::OciClient`] pulls.
//!
//! Composes three workspace crates without forcing the others to
//! know about each other:
//!
//! - `engram-oci`: defines the [`engram_oci::RegistryAuthResolver`]
//!   trait the OCI client calls into.
//! - `engram-core`: holds the [`MetadataStore`] trait + the
//!   `RegistryAuthSpec` enum that's persisted in Postgres.
//! - `engram-crypto`: handles envelope decryption of static-cred
//!   ciphertext.
//!
//! # Architecture
//!
//! ```text
//!   PgAuthResolver  (impl RegistryAuthResolver)
//!         │
//!         ▼
//!   AuthStrategy    (per-host, per-variant)
//!     ├── StaticStrategy        — decrypts once, caches BasicCreds
//!     └── GcpWorkloadIdentityStrategy
//!                                — calls metadata server / IAM
//!                                  Credentials API per pull, caches
//!                                  the OAuth token until ~5min
//!                                  before expiry.
//!     (future siblings: AwsInstanceRoleStrategy,
//!                       AwsAssumeRoleStrategy,
//!                       VaultStrategy, ...)
//! ```
//!
//! `PgAuthResolver` looks up the [`RegistryCredential`] row by host,
//! gets-or-builds the matching strategy (caching the strategy across
//! pulls), and asks it for fresh creds. Each strategy is responsible
//! for its own token TTL / refresh logic — the resolver doesn't
//! know whether creds are static or 1-hour-OAuth or anything else.
//!
//! Adding a new variant is three pieces of work:
//!
//! 1. Add a variant to `RegistryAuthSpec` (engram-core).
//! 2. Add an `AuthStrategy` impl in this crate, plus a match arm in
//!    `PgAuthResolver::build_strategy`.
//! 3. Extend the SQL CHECK constraint in a migration.
//!
//! No trait re-design, no schema reshape.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::traits::MetadataStore;
use engram_core::types::registry::RegistryAuthSpec;
use engram_crypto::MasterKeyProvider;
use engram_oci::{BasicCreds, OciError, RegistryAuthResolver};
use parking_lot::Mutex;

mod gcp;
mod static_strategy;

pub use gcp::GcpWorkloadIdentityStrategy;
pub use static_strategy::StaticStrategy;

/// Per-pull auth strategy. Each impl owns its own caching / refresh
/// policy: `StaticStrategy` caches a single decryption forever;
/// `GcpWorkloadIdentityStrategy` caches a token until it's near
/// expiry and refetches in the background.
#[async_trait]
pub trait AuthStrategy: Send + Sync {
    async fn fetch_creds(&self) -> Result<BasicCreds, OciError>;
}

/// `RegistryAuthResolver` impl backed by Postgres + KEK + per-
/// variant strategy dispatch.
///
/// Strategies are built lazily on first lookup per host and cached
/// in memory. A row update doesn't invalidate the cache today —
/// in practice operators add a credential once and don't churn it,
/// so the simplification is fine. If that changes, the right move
/// is a `LISTEN/NOTIFY registry_credentials_changed` channel that
/// the resolver subscribes to and uses to evict its cache.
pub struct PgAuthResolver {
    meta: Arc<dyn MetadataStore>,
    kek: Arc<dyn MasterKeyProvider>,
    strategies: Mutex<std::collections::HashMap<String, Arc<dyn AuthStrategy>>>,
}

impl PgAuthResolver {
    pub fn new(meta: Arc<dyn MetadataStore>, kek: Arc<dyn MasterKeyProvider>) -> Self {
        Self {
            meta,
            kek,
            strategies: Mutex::new(Default::default()),
        }
    }

    async fn build_strategy(
        &self,
        spec: &RegistryAuthSpec,
    ) -> Result<Arc<dyn AuthStrategy>, OciError> {
        match spec {
            RegistryAuthSpec::Static {
                username,
                wrapped_dek,
                nonce,
                ciphertext,
                key_id,
            } => {
                let strategy = StaticStrategy::seal_open(
                    self.kek.as_ref(),
                    username.clone(),
                    wrapped_dek,
                    nonce,
                    ciphertext,
                    key_id,
                )
                .await?;
                Ok(Arc::new(strategy))
            }
            RegistryAuthSpec::GcpWorkloadIdentity { impersonate_sa } => {
                let strategy = GcpWorkloadIdentityStrategy::new(impersonate_sa.clone())
                    .await
                    .map_err(|e| {
                        OciError::Distribution(format!("init gcp workload identity: {e}"))
                    })?;
                Ok(Arc::new(strategy))
            }
            RegistryAuthSpec::Anonymous => {
                // Resolve short-circuits before reaching this site —
                // it's an internal invariant rather than a user-
                // visible error path. Reachable only if a future caller
                // routes Anonymous through `build_strategy`.
                Err(OciError::Distribution(
                    "Anonymous auth is short-circuited in resolve(); should not reach build_strategy"
                        .into(),
                ))
            }
        }
    }
}

#[async_trait]
impl RegistryAuthResolver for PgAuthResolver {
    async fn resolve(&self, host: &str) -> Result<Option<BasicCreds>, OciError> {
        // Look up the row. Missing row = anonymous (works for public
        // registries and `localhost:5001`).
        let row = self
            .meta
            .registry_credential_for_host(host)
            .await
            .map_err(|e| OciError::Distribution(format!("postgres lookup: {e}")))?;
        let Some(row) = row else {
            return Ok(None);
        };

        // `Anonymous` rows exist primarily so the dashboard can list
        // the host; they carry no auth material. Short-circuit here so
        // the OCI client falls through to anonymous pull just like a
        // missing row would.
        if matches!(row.auth, RegistryAuthSpec::Anonymous) {
            return Ok(None);
        }

        // Get-or-build strategy. We hold the lock only for the
        // map mutation — strategy construction can call into
        // crypto / GCP and we don't want either blocking the lock.
        let cached = { self.strategies.lock().get(host).cloned() };
        let strategy = match cached {
            Some(s) => s,
            None => {
                let built = self.build_strategy(&row.auth).await?;
                let mut map = self.strategies.lock();
                map.entry(host.to_string()).or_insert(built).clone()
            }
        };

        let creds = strategy.fetch_creds().await?;
        Ok(Some(creds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::registry::{HarnessPack, RegistryCredential};
    use engram_core::types::{
        HostRecord, PersistedEvent, Session, SessionSpec, SessionStatus, SnapshotRecord,
    };
    use engram_core::{HostId, MetaError, SandboxId, SessionId};

    /// MetadataStore stub that holds at most one registry credential.
    /// All other methods unreachable / empty — we never call them in
    /// these tests. Lives here (not as a workspace-wide test util)
    /// to keep the abstraction surface clean: this crate's tests
    /// are about resolver dispatch, not the broader trait surface.
    struct OneCredMeta {
        cred: parking_lot::Mutex<Option<RegistryCredential>>,
    }

    impl OneCredMeta {
        fn new(cred: Option<RegistryCredential>) -> Self {
            Self {
                cred: parking_lot::Mutex::new(cred),
            }
        }
    }

    #[async_trait]
    impl MetadataStore for OneCredMeta {
        async fn create_session(&self, _: SessionSpec) -> Result<SessionId, MetaError> {
            unreachable!()
        }
        async fn get_session(&self, _: SessionId) -> Result<Session, MetaError> {
            unreachable!()
        }
        async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
            Ok(vec![])
        }
        async fn set_session_status(
            &self,
            _: SessionId,
            _: SessionStatus,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn assign_session_host(
            &self,
            _: SessionId,
            _: Option<HostId>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn assign_session_sandbox(
            &self,
            _: SessionId,
            _: Option<SandboxId>,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn upsert_host(&self, _: HostRecord) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
            Ok(vec![])
        }
        async fn set_host_status(
            &self,
            _: HostId,
            _: engram_core::types::HostStatus,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn touch_host_heartbeat(
            &self,
            _: HostId,
            _: engram_core::types::HostStatus,
        ) -> Result<(), MetaError> {
            Ok(())
        }

        async fn list_stale_hosts(&self, _: u64) -> Result<Vec<HostRecord>, MetaError> {
            Ok(vec![])
        }
        async fn mark_host_dead_and_reassign_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<SessionId>, MetaError> {
            Ok(vec![])
        }
        async fn record_snapshot(&self, _: SnapshotRecord) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_snapshots_for_session(
            &self,
            _: SessionId,
        ) -> Result<Vec<SnapshotRecord>, MetaError> {
            Ok(vec![])
        }
        async fn latest_snapshot_for_session(
            &self,
            _: SessionId,
        ) -> Result<Option<SnapshotRecord>, MetaError> {
            Ok(None)
        }
        async fn append_session_event(
            &self,
            _: SessionId,
            _: &str,
            _: serde_json::Value,
        ) -> Result<i64, MetaError> {
            Ok(0)
        }
        async fn list_session_events_since(
            &self,
            _: SessionId,
            _: i64,
            _: i64,
        ) -> Result<Vec<PersistedEvent>, MetaError> {
            Ok(vec![])
        }
        async fn upsert_registry_credential(
            &self,
            cred: RegistryCredential,
        ) -> Result<(), MetaError> {
            *self.cred.lock() = Some(cred);
            Ok(())
        }
        async fn list_registry_credentials(&self) -> Result<Vec<RegistryCredential>, MetaError> {
            Ok(self.cred.lock().clone().into_iter().collect())
        }
        async fn registry_credential_for_host(
            &self,
            host: &str,
        ) -> Result<Option<RegistryCredential>, MetaError> {
            Ok(self
                .cred
                .lock()
                .as_ref()
                .filter(|c| c.registry_host == host)
                .cloned())
        }
        async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
            *self.cred.lock() = None;
            Ok(())
        }
        async fn upsert_harness_pack(&self, _: HarnessPack) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_harness_packs(&self) -> Result<Vec<HarnessPack>, MetaError> {
            Ok(vec![])
        }
        async fn get_harness_pack(&self, _: &str) -> Result<Option<HarnessPack>, MetaError> {
            Ok(None)
        }
        async fn delete_harness_pack(&self, _: &str) -> Result<(), MetaError> {
            Ok(())
        }
        async fn upsert_enabled_image(
            &self,
            _: engram_core::types::EnabledImage,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn list_enabled_images(
            &self,
        ) -> Result<Vec<engram_core::types::EnabledImage>, MetaError> {
            Ok(Vec::new())
        }
        async fn get_enabled_image(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
            Ok(())
        }
        async fn upsert_session_secrets(
            &self,
            _: engram_core::types::SessionSecrets,
        ) -> Result<(), MetaError> {
            Ok(())
        }
        async fn get_session_secrets(
            &self,
            _: SessionId,
        ) -> Result<Option<engram_core::types::SessionSecrets>, MetaError> {
            Ok(None)
        }
        async fn delete_session_secrets(&self, _: SessionId) -> Result<(), MetaError> {
            Ok(())
        }
    }

    fn test_kek() -> Arc<dyn MasterKeyProvider> {
        Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
            [0xab; 32], "test:v1",
        ))
    }

    #[tokio::test]
    async fn resolve_returns_none_for_unknown_host() {
        let meta: Arc<dyn MetadataStore> = Arc::new(OneCredMeta::new(None));
        let resolver = PgAuthResolver::new(meta, test_kek());
        assert!(resolver.resolve("gcr.io").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_decrypts_static_credentials() {
        // Seal a known plaintext under the test KEK, persist it as a
        // RegistryCredential, then verify the resolver round-trips
        // back to the same username/password pair.
        let kek = test_kek();
        let cipher = engram_crypto::CredCipher::new(kek.as_ref());
        let sealed = cipher.seal(b"hunter2").await.unwrap();
        let cred = RegistryCredential {
            id: uuid::Uuid::new_v4(),
            registry_host: "gcr.io".into(),
            auth: RegistryAuthSpec::Static {
                username: "_json_key".into(),
                wrapped_dek: sealed.wrapped_dek,
                nonce: sealed.nonce.to_vec(),
                ciphertext: sealed.ciphertext,
                key_id: sealed.key_id,
            },
            created_at: chrono::Utc::now(),
            updated_at: None,
        };
        let meta: Arc<dyn MetadataStore> = Arc::new(OneCredMeta::new(Some(cred)));
        let resolver = PgAuthResolver::new(meta, kek);
        let creds = resolver.resolve("gcr.io").await.unwrap().unwrap();
        assert_eq!(creds.username, "_json_key");
        assert_eq!(creds.password, "hunter2");
    }

    #[tokio::test]
    async fn strategy_cache_serves_subsequent_resolves_from_memory() {
        // Same as above, but call resolve() twice. The second call
        // should hit the in-memory strategy cache — which we observe
        // by checking that the cache map is non-empty after the
        // first call (we don't have a "decrypt count" hook to assert
        // against directly, but presence in the map is the
        // load-bearing signal).
        let kek = test_kek();
        let cipher = engram_crypto::CredCipher::new(kek.as_ref());
        let sealed = cipher.seal(b"x").await.unwrap();
        let cred = RegistryCredential {
            id: uuid::Uuid::new_v4(),
            registry_host: "gcr.io".into(),
            auth: RegistryAuthSpec::Static {
                username: "u".into(),
                wrapped_dek: sealed.wrapped_dek,
                nonce: sealed.nonce.to_vec(),
                ciphertext: sealed.ciphertext,
                key_id: sealed.key_id,
            },
            created_at: chrono::Utc::now(),
            updated_at: None,
        };
        let meta: Arc<dyn MetadataStore> = Arc::new(OneCredMeta::new(Some(cred)));
        let resolver = PgAuthResolver::new(meta, kek);
        resolver.resolve("gcr.io").await.unwrap();
        assert!(resolver.strategies.lock().contains_key("gcr.io"));
        // Second call still works — would also exercise the cache hit path.
        let creds = resolver.resolve("gcr.io").await.unwrap().unwrap();
        assert_eq!(creds.password, "x");
    }
}
