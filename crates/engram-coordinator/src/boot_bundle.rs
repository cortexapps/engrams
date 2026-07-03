//! Issue #535 (create-as-a-plan) (a): per-enabled-image boot bundle, LISTEN/
//! NOTIFY-invalidated.
//!
//! Create-time facts live at three different cadences but were all
//! re-derived at the fastest one (per create): the manifest TOML, harness
//! descriptor, budgets, and base-snapshot record are pure functions of the
//! **enabled-image row** (changes per bake); the fleet bundle catalog is a
//! function of the **host-image stamp** (changes per host roll). This module
//! caches both, invalidated by `pg_listener` on `enabled_image_changed` /
//! `fleet_catalog_changed` NOTIFYs, with a belt-and-braces TTL so a dropped
//! notification (a `PgListener` reconnect) bounds staleness rather than
//! wedging a cache entry forever.
//!
//! A stale bundle is safe to serve: `base_snapshot_id` lineage rows are
//! pinned (never mutated in place — a re-enable mints a new row via
//! `upsert_enabled_image`'s `ON CONFLICT`), and an unknown-skill/harness name
//! surfaces as the same create-time 400 it always did, which the caller
//! retries.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::MetadataStore;
use engram_core::types::{EnabledImage, ImageManifest, SnapshotRecord};

use crate::error::ApiError;

/// Worst-case staleness on a wedged `PgListener` (a dropped reconnect that
/// missed a NOTIFY). Short enough that an operator re-baking/re-enabling an
/// image never has to think about it; long enough to make the cache actually
/// useful against per-create churn.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Everything [`crate::session_boot::boot_on_reserved_host`] and
/// `prepare_inner` need from an enabled image, resolved ONCE per bake (or
/// per TTL/NOTIFY refresh) instead of per create.
pub(crate) struct BootBundle {
    /// The row snapshot as of the last fill. Always the `_any` (soft-delete-
    /// tolerant) view — callers on the strict live-create path check
    /// `enabled.soft_deleted_at` themselves (mirrors `get_enabled_image` vs
    /// `get_enabled_image_any`'s split without forking the cache).
    pub enabled: EnabledImage,
    /// Parsed ONCE per bake, not per create.
    pub manifest: ImageManifest,
    pub base_snapshot: SnapshotRecord,
    pub memory_mib: u32,
    pub cpu_budget_vcpus: u32,
}

struct FleetCatalogEntry {
    filled_at: Instant,
    catalog: Arc<HashMap<String, String>>,
}

struct BundleEntry {
    filled_at: Instant,
    bundle: Arc<BootBundle>,
}

/// Read-through cache, shared across the coordinator's replica via a
/// `pg_listener`-driven invalidation (see module docs). Cheap to clone
/// (`Arc`-wrapped by the caller, like `harness_hub`/`cow_state_cache` on
/// `AppState`).
#[derive(Default)]
pub struct BootBundleCache {
    bundles: parking_lot::Mutex<HashMap<String, BundleEntry>>,
    fleet_catalog: parking_lot::Mutex<Option<FleetCatalogEntry>>,
}

impl BootBundleCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fill (or serve fresh) the bundle for `image_uri`. Always resolves the
    /// `_any` row — the caller applies strictness (see [`BootBundle::enabled`]
    /// docs).
    pub(crate) async fn bundle_for(
        &self,
        meta: &dyn MetadataStore,
        image_uri: &str,
    ) -> Result<Arc<BootBundle>, ApiError> {
        if let Some(hit) = self.fresh_bundle(image_uri) {
            return Ok(hit);
        }
        let enabled = meta
            .get_enabled_image_any(image_uri)
            .await
            .map_err(|e| ApiError::Internal(format!("enabled_images lookup: {e}")))?
            .ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "image `{image_uri}` is not enabled. Operators enable images via \
                     POST /api/enabled-images before sessions can reference them.",
                ))
            })?;
        let manifest: ImageManifest = toml::from_str(&enabled.manifest_toml).map_err(|e| {
            ApiError::Internal(format!(
                "stored manifest for {image_uri} failed to parse: {e}"
            ))
        })?;
        let base_snapshot_id = enabled.base_snapshot_id.ok_or_else(|| {
            ApiError::Internal(format!(
                "enabled image `{image_uri}` has no base snapshot — re-enable it \
                 (POST /api/enabled-images) to capture one"
            ))
        })?;
        let base_snapshot = meta
            .get_snapshot(base_snapshot_id)
            .await
            .map_err(|e| ApiError::Internal(format!("get_snapshot {base_snapshot_id}: {e}")))?
            .ok_or_else(|| {
                ApiError::Internal(format!(
                    "enabled image `{image_uri}` references base snapshot \
                     {base_snapshot_id} but its row is gone"
                ))
            })?;
        let memory_mib = crate::api::sessions::resolved_memory_mib(&manifest);
        let cpu_budget_vcpus = crate::api::sessions::resolved_vcpus(&manifest);
        let bundle = Arc::new(BootBundle {
            enabled,
            manifest,
            base_snapshot,
            memory_mib,
            cpu_budget_vcpus,
        });
        self.bundles.lock().insert(
            image_uri.to_string(),
            BundleEntry {
                filled_at: Instant::now(),
                bundle: bundle.clone(),
            },
        );
        Ok(bundle)
    }

    fn fresh_bundle(&self, image_uri: &str) -> Option<Arc<BootBundle>> {
        let g = self.bundles.lock();
        let e = g.get(image_uri)?;
        (e.filled_at.elapsed() < CACHE_TTL).then(|| e.bundle.clone())
    }

    /// `pg_listener` invalidation on `enabled_image_changed <image_uri>`.
    pub(crate) fn invalidate_image(&self, image_uri: &str) {
        self.bundles.lock().remove(image_uri);
    }

    /// The fleet's baked bundle catalog (name -> staged sha256), cached
    /// against the `hosts.current_bundles` NOTIFY trigger. Subsumes
    /// `fleet_bundle_catalog`'s per-call `list_active_hosts` scan.
    pub(crate) async fn fleet_catalog(
        &self,
        meta: &dyn MetadataStore,
    ) -> Result<Arc<HashMap<String, String>>, ApiError> {
        if let Some(hit) = self.fresh_fleet_catalog() {
            return Ok(hit);
        }
        let hosts = meta
            .list_active_hosts()
            .await
            .map_err(|e| ApiError::Internal(format!("list_active_hosts for skill resolve: {e}")))?;
        let catalog: HashMap<String, String> = hosts
            .iter()
            .find(|h| !h.current_bundles.is_empty())
            .map(|h| {
                h.current_bundles
                    .iter()
                    .map(|b| (b.drive_id.clone(), b.sha256.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let catalog = Arc::new(catalog);
        *self.fleet_catalog.lock() = Some(FleetCatalogEntry {
            filled_at: Instant::now(),
            catalog: catalog.clone(),
        });
        Ok(catalog)
    }

    fn fresh_fleet_catalog(&self) -> Option<Arc<HashMap<String, String>>> {
        let g = self.fleet_catalog.lock();
        let e = g.as_ref()?;
        (e.filled_at.elapsed() < CACHE_TTL).then(|| e.catalog.clone())
    }

    /// `pg_listener` invalidation on `fleet_catalog_changed <host_id>`. Any
    /// host's stamp change invalidates the whole cached catalog — cheap
    /// (host rolls are rare) and correct without tracking which host's
    /// `current_bundles` the cached view actually came from (`fleet_bundle_
    /// catalog`'s "first non-empty report wins" rule means the winning host
    /// can change even when a *different* host rolls, since a formerly-empty
    /// host might now report first).
    pub(crate) fn invalidate_fleet_catalog(&self) {
        *self.fleet_catalog.lock() = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use async_trait::async_trait;
    use engram_core::types::{HostRecord, HostStatus};
    use engram_core::{HostId, MetaError};

    use super::*;

    /// A `MetadataStore` that counts `get_enabled_image_any` / `get_snapshot`
    /// / `list_active_hosts` calls, so the cache-fill tests can assert a warm
    /// cache does zero I/O.
    #[derive(Default)]
    struct CountingMeta {
        enabled_image_calls: AtomicU32,
        snapshot_calls: AtomicU32,
        list_hosts_calls: AtomicU32,
    }

    fn test_manifest_toml() -> String {
        "name = \"demo\"\n[resources]\nsuggested_memory_mib = 2048\nsuggested_vcpus = 2\n"
            .to_string()
    }

    fn test_enabled_image(snapshot_id: engram_core::types::SnapshotId) -> EnabledImage {
        EnabledImage {
            id: uuid::Uuid::new_v4(),
            image_uri: "localhost:5001/demo:test".into(),
            manifest_toml: test_manifest_toml(),
            manifest_digest: "sha256:deadbeef".into(),
            disk_manifest: None,
            base_snapshot_id: Some(snapshot_id),
            base_snapshot_disk_manifest: None,
            base_snapshot_memory_manifest: None,
            last_refreshed_at: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            updated_at: None,
            soft_deleted_at: None,
            capture_env: Vec::new(),
        }
    }

    fn test_snapshot(id: engram_core::types::SnapshotId) -> SnapshotRecord {
        SnapshotRecord {
            id,
            session_id: None,
            host_id: None,
            image_version: "test".into(),
            size_bytes: 1024,
            created_at: chrono::Utc::now(),
            last_accessed_at: chrono::Utc::now(),
            disk_manifest: None,
            memory_manifest: None,
            recoverable: true,
            aux_bundles: Vec::new(),
            events_cursor: None,
        }
    }

    #[async_trait]
    impl MetadataStore for CountingMeta {
        async fn create_session(
            &self,
            _: engram_core::types::SessionSpec,
        ) -> Result<engram_core::SessionId, MetaError> {
            unimplemented!()
        }
        async fn get_session(
            &self,
            _: engram_core::SessionId,
        ) -> Result<engram_core::types::Session, MetaError> {
            unimplemented!()
        }
        async fn list_active_sessions(
            &self,
        ) -> Result<Vec<engram_core::types::Session>, MetaError> {
            unimplemented!()
        }
        async fn transition_session_created(
            &self,
            _: engram_core::SessionId,
            _: engram_core::SandboxId,
        ) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn reserve_and_persist_create(
            &self,
            _: engram_core::traits::SessionCreateWriteSet,
            _: &[HostId],
            _: usize,
        ) -> Result<engram_core::traits::CreateDisposition, MetaError> {
            unimplemented!()
        }
        async fn transition_session(
            &self,
            _: engram_core::SessionId,
            _: engram_core::types::SessionState,
        ) -> Result<engram_core::types::SessionState, MetaError> {
            unimplemented!()
        }
        async fn assign_session_host(
            &self,
            _: engram_core::SessionId,
            _: Option<HostId>,
        ) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn assign_session_sandbox(
            &self,
            _: engram_core::SessionId,
            _: Option<engram_core::SandboxId>,
        ) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn upsert_host(&self, _: HostRecord) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn set_host_status(&self, _: HostId, _: HostStatus) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn touch_host_heartbeat(
            &self,
            _: HostId,
            _: engram_core::types::host::HostHeartbeat,
        ) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn set_host_cordoned(&self, _: HostId, _: bool) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn list_stale_hosts(&self, _: u64) -> Result<Vec<HostRecord>, MetaError> {
            unimplemented!()
        }
        async fn mark_host_dead_and_orphan_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<(engram_core::SessionId, engram_core::types::SessionState)>, MetaError>
        {
            unimplemented!()
        }
        async fn record_snapshot(&self, _: SnapshotRecord) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn list_snapshots_for_session(
            &self,
            _: engram_core::SessionId,
        ) -> Result<Vec<SnapshotRecord>, MetaError> {
            unimplemented!()
        }
        async fn latest_snapshot_for_session(
            &self,
            _: engram_core::SessionId,
        ) -> Result<Option<SnapshotRecord>, MetaError> {
            unimplemented!()
        }
        async fn append_session_event(
            &self,
            _: engram_core::SessionId,
            _: &str,
            _: serde_json::Value,
        ) -> Result<i64, MetaError> {
            unimplemented!()
        }
        async fn list_session_events_since(
            &self,
            _: engram_core::SessionId,
            _: i64,
            _: i64,
        ) -> Result<Vec<engram_core::types::PersistedEvent>, MetaError> {
            unimplemented!()
        }
        async fn insert_artifact(
            &self,
            _: uuid::Uuid,
            _: engram_core::SessionId,
            _: &str,
            _: &str,
            _: i64,
            _: Option<&str>,
        ) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn get_artifact(
            &self,
            _: engram_core::SessionId,
            _: uuid::Uuid,
        ) -> Result<Option<engram_core::types::ArtifactRow>, MetaError> {
            unimplemented!()
        }
        async fn artifact_usage(&self, _: engram_core::SessionId) -> Result<(i64, i64), MetaError> {
            unimplemented!()
        }
        async fn upsert_registry_credential(
            &self,
            _: engram_core::types::RegistryCredential,
        ) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn list_registry_credentials(
            &self,
        ) -> Result<Vec<engram_core::types::RegistryCredential>, MetaError> {
            unimplemented!()
        }
        async fn registry_credential_for_host(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::RegistryCredential>, MetaError> {
            unimplemented!()
        }
        async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn upsert_enabled_image(&self, _: EnabledImage) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn list_enabled_images(&self) -> Result<Vec<EnabledImage>, MetaError> {
            unimplemented!()
        }
        async fn get_enabled_image(&self, _: &str) -> Result<Option<EnabledImage>, MetaError> {
            unimplemented!()
        }
        async fn soft_delete_enabled_image(
            &self,
            _: &str,
        ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
            unimplemented!()
        }
        async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn get_session_secrets(
            &self,
            _: engram_core::SessionId,
        ) -> Result<Option<engram_core::types::SessionSecrets>, MetaError> {
            unimplemented!()
        }
        async fn delete_session_secrets(&self, _: engram_core::SessionId) -> Result<(), MetaError> {
            unimplemented!()
        }
        async fn get_enabled_image_any(
            &self,
            image_uri: &str,
        ) -> Result<Option<EnabledImage>, MetaError> {
            self.enabled_image_calls.fetch_add(1, Ordering::SeqCst);
            let snapshot_id = engram_core::types::SnapshotId::new();
            let mut img = test_enabled_image(snapshot_id);
            img.image_uri = image_uri.to_string();
            Ok(Some(img))
        }
        async fn get_snapshot(
            &self,
            id: engram_core::types::SnapshotId,
        ) -> Result<Option<SnapshotRecord>, MetaError> {
            self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(test_snapshot(id)))
        }
        async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
            self.list_hosts_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![HostRecord {
                id: HostId::new(),
                hostname: "h1".into(),
                cloud_metadata: Default::default(),
                capacity: engram_core::types::HostCapacity {
                    total_gb: 0,
                    used_gb: 0,
                    total_mib: 0,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: None,
                ready_images: Vec::new(),
                local_snapshots: Vec::new(),
                current_bundles: vec![engram_core::types::sandbox::AuxBundleRef {
                    drive_id: "claude".into(),
                    sha256: "sha256:cafe".into(),
                }],
                cordoned: false,
                total_vcpus: 4,
                wire_version: 1,
            }])
        }
    }

    #[tokio::test]
    async fn bundle_for_caches_across_calls() {
        let meta = CountingMeta::default();
        let cache = BootBundleCache::new();
        let a = cache.bundle_for(&meta, "img:1").await.unwrap();
        let b = cache.bundle_for(&meta, "img:1").await.unwrap();
        assert_eq!(a.enabled.image_uri, b.enabled.image_uri);
        assert_eq!(meta.enabled_image_calls.load(Ordering::SeqCst), 1);
        assert_eq!(meta.snapshot_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidate_image_forces_refill() {
        let meta = CountingMeta::default();
        let cache = BootBundleCache::new();
        cache.bundle_for(&meta, "img:1").await.unwrap();
        cache.invalidate_image("img:1");
        cache.bundle_for(&meta, "img:1").await.unwrap();
        assert_eq!(meta.enabled_image_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fleet_catalog_caches_across_calls() {
        let meta = CountingMeta::default();
        let cache = BootBundleCache::new();
        let a = cache.fleet_catalog(&meta).await.unwrap();
        let b = cache.fleet_catalog(&meta).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(meta.list_hosts_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidate_fleet_catalog_forces_refill() {
        let meta = CountingMeta::default();
        let cache = BootBundleCache::new();
        cache.fleet_catalog(&meta).await.unwrap();
        cache.invalidate_fleet_catalog();
        cache.fleet_catalog(&meta).await.unwrap();
        assert_eq!(meta.list_hosts_calls.load(Ordering::SeqCst), 2);
    }
}
