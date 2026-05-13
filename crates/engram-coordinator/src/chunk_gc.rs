//! Chunk-store GC scheduler.
//!
//! ADR 0007 Phase 1 closeout. Pairs the existing
//! `POST /api/admin/gc-chunks` admin endpoint (explicit, on-demand)
//! with a cron driver that fires the same primitive on a cadence so
//! storage cost doesn't grow unbounded between operator-driven
//! sweeps. Both paths share the same live-set query
//! (`MetadataStore::list_live_disk_manifest_ids` +
//! `list_live_memory_manifest_ids`) and the same `gc::run` —
//! anything fixed here flows to the admin endpoint and vice versa.
//!
//! Two env knobs:
//! - `ENGRAM_CHUNK_GC_INTERVAL_SECS` — how often the sweep fires.
//!   Default 1h. `0` disables the loop entirely (useful in tests +
//!   environments that prefer the admin endpoint only).
//! - `ENGRAM_CHUNK_GC_RETAIN_SECS` — minimum age an unreferenced
//!   chunk must reach before it's eligible for deletion. Default
//!   24h, mirroring the admin endpoint's default.
//!
//! Mirrors the `idle_evictor::spawn` shape (interval-driven tokio
//! task, JoinHandle dropped for process-lifetime, errors logged but
//! never panic the loop). The pipeline function is intentionally
//! `pub` so the admin endpoint and a future host-agent fanout can
//! both call it.

use std::time::Duration;

use tokio::task::JoinHandle;

use crate::state::SharedState;

/// Default cadence for the GC sweep. 1h is generous enough that a
/// running cron doesn't churn over an idle cluster, tight enough
/// that operators don't have to schedule one out-of-band.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(3600);

/// Default retention window matching the admin endpoint default. 24h
/// is long enough that an in-flight snapshot writer committing a new
/// manifest version isn't racing the sweep.
pub const DEFAULT_RETAIN_FOR: Duration = Duration::from_secs(24 * 3600);

/// Parse `ENGRAM_CHUNK_GC_INTERVAL_SECS`. `None` → use
/// [`DEFAULT_INTERVAL`]. `Some(0)` → disable the loop.
pub fn interval_from_env() -> Option<Duration> {
    parse_secs_env("ENGRAM_CHUNK_GC_INTERVAL_SECS").or(Some(DEFAULT_INTERVAL))
}

/// Parse `ENGRAM_CHUNK_GC_RETAIN_SECS`. Falls back to
/// [`DEFAULT_RETAIN_FOR`] on absent/unparseable.
pub fn retain_for_from_env() -> Duration {
    parse_secs_env("ENGRAM_CHUNK_GC_RETAIN_SECS").unwrap_or(DEFAULT_RETAIN_FOR)
}

fn parse_secs_env(var: &str) -> Option<Duration> {
    let raw = std::env::var(var).ok()?;
    let secs: u64 = raw.parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Spawn the chunk-GC driver as a background tokio task. Returns
/// `None` if `interval` is zero — operators opt out of the cron by
/// setting `ENGRAM_CHUNK_GC_INTERVAL_SECS=0` and rely on the
/// admin-endpoint trigger alone.
pub fn spawn(
    state: SharedState,
    interval: Duration,
    retain_for: Duration,
) -> Option<JoinHandle<()>> {
    if interval.is_zero() {
        tracing::info!(
            "chunk-store GC scheduler disabled (ENGRAM_CHUNK_GC_INTERVAL_SECS=0); \
             admin endpoint POST /api/admin/gc-chunks still available",
        );
        return None;
    }
    Some(tokio::spawn(async move {
        // Skip the immediate tick so the first sweep doesn't race a
        // freshly-started coord whose `repopulate_routing` is still
        // resurrecting active sessions.
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&state, retain_for).await {
                tracing::warn!(error = %e, "chunk-store GC tick failed; will retry next tick");
            }
        }
    }))
}

/// One pass of the sweep. Public so the admin handler can share the
/// exact pipeline. Returns the typed stats unchanged from
/// `engram_chunk_store::gc::run` plus the live-set size for
/// telemetry — operators want both axes to spot a stuck DB query
/// vs. a stuck blob backend.
///
/// ADR 0009 Phase 2: a future addition here would re-verify
/// `snapshots.recoverable=true` rows by HEAD-checking their
/// manifests against the chunk store and flipping any that fail
/// back to false. In the current GC semantics that's a no-op:
/// `live_manifest_ids` IS the set of manifests referenced by any
/// `snapshots` row, so by construction every reaped manifest has
/// no referencing row to flip. The defensive sweep only matters
/// for catastrophic blob loss (S3 lifecycle, region failure,
/// operator error) — out of scope for Phase 2; tracked under
/// observability as a future periodic verifier.
pub async fn run_once(state: &SharedState, retain_for: Duration) -> Result<RunStats, ChunkGcError> {
    // Union of disk + memory live sets — both axes back onto the
    // same chunk store. Filtering on only one would prematurely
    // sweep the other side's chunks.
    let mut live: Vec<uuid::Uuid> = state
        .services
        .meta
        .list_live_disk_manifest_ids()
        .await
        .map_err(|e| ChunkGcError::Meta(e.to_string()))?;
    let mem = state
        .services
        .meta
        .list_live_memory_manifest_ids()
        .await
        .map_err(|e| ChunkGcError::Meta(e.to_string()))?;
    live.extend(mem);
    // Dedup so the gc::run iterator doesn't double-fetch any
    // manifest_id that's referenced by both a disk and memory
    // snapshot record (rare but cheap to handle).
    live.sort();
    live.dedup();
    let live_count = live.len();

    tracing::info!(
        live_manifest_count = live_count,
        retain_secs = retain_for.as_secs(),
        "chunk-store GC sweep starting",
    );

    let stats = engram_chunk_store::gc::run(&state.services.chunk_store, retain_for, live)
        .await
        .map_err(|e| ChunkGcError::ChunkStore(e.to_string()))?;

    tracing::info!(
        chunks_deleted = stats.chunks_deleted,
        bytes_freed = stats.bytes_freed,
        chunks_retained_age = stats.chunks_retained_age,
        elapsed_ms = stats.elapsed.as_millis() as u64,
        "chunk-store GC sweep complete",
    );

    Ok(RunStats {
        stats,
        live_manifest_count: live_count,
    })
}

#[derive(Debug)]
pub struct RunStats {
    pub stats: engram_chunk_store::gc::GcStats,
    pub live_manifest_count: usize,
}

#[derive(Debug)]
pub enum ChunkGcError {
    Meta(String),
    ChunkStore(String),
}

impl std::fmt::Display for ChunkGcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Meta(e) => write!(f, "metadata: {e}"),
            Self::ChunkStore(e) => write!(f, "chunk store: {e}"),
        }
    }
}

impl std::error::Error for ChunkGcError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::session::HarnessSpec;
    use engram_core::types::{Session, SessionStatus};
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_state() -> (SharedState, TempDir) {
        let local = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(local.path().join("sandboxes")));
        let host_registry = Arc::new(HostRegistry::new());
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(local.path().join("blob")),
        );
        let services = Services {
            meta: Arc::new(MiniMeta::new(ephemeral_session())),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: blob.clone(),
            chunk_store: engram_chunk_store::ChunkStore::new(blob),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path: local.path().to_path_buf(),
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        (state, local)
    }

    fn ephemeral_session() -> Session {
        Session {
            id: engram_core::SessionId::new(),
            user_id: None,
            status: SessionStatus::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:gc".into(),
            harness: HarnessSpec::None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        }
    }

    // Per-test env-var guard. Tests poke process-global env vars, so
    // they must serialize and the guard must recover from poisoning
    // (a panicking test mustn't poison the others).
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn interval_from_env_defaults_when_unset() {
        let _g = env_guard();
        std::env::remove_var("ENGRAM_CHUNK_GC_INTERVAL_SECS");
        assert_eq!(interval_from_env(), Some(DEFAULT_INTERVAL));
    }

    #[test]
    fn interval_from_env_parses_zero_as_disabled_sentinel() {
        let _g = env_guard();
        std::env::set_var("ENGRAM_CHUNK_GC_INTERVAL_SECS", "0");
        let parsed = interval_from_env();
        std::env::remove_var("ENGRAM_CHUNK_GC_INTERVAL_SECS");
        assert_eq!(parsed, Some(Duration::ZERO));
    }

    #[test]
    fn interval_from_env_round_trips_seconds() {
        let _g = env_guard();
        std::env::set_var("ENGRAM_CHUNK_GC_INTERVAL_SECS", "120");
        let parsed = interval_from_env();
        std::env::remove_var("ENGRAM_CHUNK_GC_INTERVAL_SECS");
        assert_eq!(parsed, Some(Duration::from_secs(120)));
    }

    #[test]
    fn retain_for_from_env_defaults_when_unset() {
        let _g = env_guard();
        std::env::remove_var("ENGRAM_CHUNK_GC_RETAIN_SECS");
        assert_eq!(retain_for_from_env(), DEFAULT_RETAIN_FOR);
    }

    #[test]
    fn retain_for_from_env_parses_value() {
        let _g = env_guard();
        std::env::set_var("ENGRAM_CHUNK_GC_RETAIN_SECS", "300");
        let parsed = retain_for_from_env();
        std::env::remove_var("ENGRAM_CHUNK_GC_RETAIN_SECS");
        assert_eq!(parsed, Duration::from_secs(300));
    }

    #[tokio::test]
    async fn spawn_zero_interval_yields_none() {
        let (state, _tmp) = build_state();
        let h = spawn(state, Duration::ZERO, DEFAULT_RETAIN_FOR);
        assert!(
            h.is_none(),
            "zero interval is the explicit-disable sentinel",
        );
    }

    #[tokio::test]
    async fn run_once_returns_zero_when_no_chunks_exist() {
        let (state, _tmp) = build_state();
        let result = run_once(&state, Duration::from_secs(0)).await.unwrap();
        assert_eq!(result.stats.chunks_deleted, 0);
        assert_eq!(result.stats.bytes_freed, 0);
        assert_eq!(result.live_manifest_count, 0);
    }

    #[tokio::test]
    async fn run_once_sweeps_dangling_chunks_when_no_live_manifests() {
        // Plant two chunks via the store. MiniMeta's
        // `list_live_*_manifest_ids` defaults yield empty vectors —
        // i.e. the sweep sees zero live manifests. With a zero-
        // retention window every chunk in the store is sweep-
        // eligible. This proves the gc pipeline is wired end-to-end
        // (state → meta → chunk-store → blob). The *live-set
        // filtering* (preserving chunks reachable from a live
        // manifest) is covered by `engram_chunk_store::gc`'s own
        // unit tests against a real chunk count.
        let (state, _tmp) = build_state();
        let store = &state.services.chunk_store;
        let h1 = store.put_chunk(b"dangling-chunk-bytes-A").await.unwrap();
        let h2 = store.put_chunk(b"dangling-chunk-bytes-B").await.unwrap();
        assert!(store.chunk_exists(h1).await.unwrap());
        assert!(store.chunk_exists(h2).await.unwrap());
        let result = run_once(&state, Duration::from_secs(0)).await.unwrap();
        assert_eq!(result.live_manifest_count, 0);
        assert_eq!(
            result.stats.chunks_deleted, 2,
            "with no live manifests both chunks are sweep-eligible",
        );
        assert!(!store.chunk_exists(h1).await.unwrap());
        assert!(!store.chunk_exists(h2).await.unwrap());
    }
}
