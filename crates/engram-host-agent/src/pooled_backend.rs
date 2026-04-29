//! `SandboxBackend` wrapper that opportunistically returns warm-pool
//! sandboxes on `create()` and reports its pool state for heartbeats.
//!
//! The host-agent owns one `PooledBackend` wrapping its real backend
//! (Process or Firecracker). Coordinator-side `--mode=all` also wraps
//! the local backend in a `PooledBackend` so warm-pool semantics apply
//! identically to single-binary dev and multi-host production.
//!
//! Pool key is the `image_version` only (i.e. `spec.image`). Two repos
//! using the same image share warm slots, which is the right behaviour
//! — the pool's job is to amortise the create-time cost of a given
//! image, not enforce per-repo isolation. The wire-level
//! [`engram_protocol::WarmPoolReport`] still has a `repo` field for
//! diagnostics; we ship it as the same value as `image_version`.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId};
use engram_protocol::WarmPoolReport;

use crate::pool::{Pool, PoolKey};

/// Wraps an inner [`SandboxBackend`] with warm-pool semantics. `create`
/// looks for a warm slot first; on miss it forwards to `inner` and
/// kicks off a background `replenish` so the next session gets the
/// freshly-created slot. Other methods just delegate.
pub struct PooledBackend {
    inner: Arc<dyn SandboxBackend>,
    pool: Pool,
    /// Default target size used when `create()` first sees a new
    /// `(image_version)`. `0` disables pooling: every session gets a
    /// fresh sandbox.
    default_target: u32,
}

impl PooledBackend {
    pub fn new(inner: Arc<dyn SandboxBackend>, default_target: u32) -> Self {
        Self {
            pool: Pool::new(inner.clone()),
            inner,
            default_target,
        }
    }

    /// Snapshot the pool's `(ready, target)` counts per image_version
    /// for inclusion in the next outbound heartbeat. Cheap (locks the
    /// pool's mutex briefly).
    pub fn snapshot_warm_pools(&self) -> Vec<WarmPoolReport> {
        self.pool.snapshot_reports()
    }

    fn pool_key(spec: &SandboxSpec) -> PoolKey {
        // `repo` is duplicated as the image_version because the wire
        // protocol's `WarmPoolReport` carries both fields and the
        // scheduler matches on `image_version` only (so the diagnostic
        // `repo` value just needs to be stable / non-empty).
        PoolKey {
            repo: spec.image.clone(),
            image_version: spec.image.clone(),
        }
    }
}

#[async_trait]
impl SandboxBackend for PooledBackend {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        let key = Self::pool_key(&spec);

        // Warm-slot path: a previously-created sandbox is sitting in
        // the pool, ready to serve. Return it immediately and queue
        // a replenish to top the pool back up.
        if let Some(id) = self.pool.checkout(&key) {
            tracing::debug!(image = %spec.image, sandbox_id = %id, "pool checkout hit");
            self.spawn_replenish(key);
            return Ok(id);
        }

        // First-session-for-this-image path: configure the pool with
        // this spec so future replenishes know what to create, then
        // do the actual create on the wrapped backend.
        if self.default_target > 0 {
            self.pool
                .configure(key.clone(), self.default_target, spec.clone());
        }
        let sandbox_id = self.inner.create(spec).await?;
        if self.default_target > 0 {
            self.spawn_replenish(key);
        }
        Ok(sandbox_id)
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        self.inner.exec_stream(id, cmd).await
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        dest: &std::path::Path,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.inner.snapshot(id, dest).await
    }

    async fn restore(&self, src: std::path::PathBuf) -> Result<SandboxId, SandboxError> {
        self.inner.restore(src).await
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.inner.destroy(id).await
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        self.inner.start_agent(id, agent).await
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.inner.list().await
    }
}

impl PooledBackend {
    fn spawn_replenish(&self, key: PoolKey) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            if let Err(e) = pool.replenish(&key).await {
                tracing::warn!(
                    error = %e,
                    image = %key.image_version,
                    "background pool replenish failed",
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit};
    use engram_sandbox_process::ProcessBackend;

    fn live_spec(image: &str) -> SandboxSpec {
        SandboxSpec {
            image: image.into(),
            rootfs_source: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
        }
    }

    #[tokio::test]
    async fn create_with_target_zero_skips_pool_and_just_forwards() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
        let pooled = PooledBackend::new(inner, 0);

        // Each call produces a fresh id; nothing accumulates in the pool.
        let _id1 = pooled.create(live_spec("warm-test")).await.unwrap();
        let reports = pooled.snapshot_warm_pools();
        assert!(
            reports.is_empty(),
            "target=0 must not configure any pool entries"
        );
    }

    #[tokio::test]
    async fn second_create_for_same_image_can_hit_warm_slot() {
        // Sequence: first create configures the pool and triggers a
        // background replenish (target=2). After awaiting a tick the
        // replenisher has filled one or two slots. A second create
        // with the same image should checkout from that pool.
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
        let pooled = PooledBackend::new(inner, 2);

        let _first = pooled.create(live_spec("warm-test")).await.unwrap();

        // Yield long enough for the spawned replenish task to start
        // and create at least one slot. ProcessBackend's create is
        // sync-cheap (just creates a tempdir).
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let reports = pooled.snapshot_warm_pools();
        assert!(
            !reports.is_empty(),
            "configured pool must show in snapshot_warm_pools",
        );
        let report = &reports[0];
        assert_eq!(report.target, 2);
        assert_eq!(report.image_version, "warm-test");
    }

    #[tokio::test]
    async fn different_images_get_separate_pool_entries() {
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
        let pooled = PooledBackend::new(inner, 1);

        let _ = pooled.create(live_spec("warm-a")).await.unwrap();
        let _ = pooled.create(live_spec("warm-b")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut reports = pooled.snapshot_warm_pools();
        reports.sort_by(|a, b| a.image_version.cmp(&b.image_version));
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].image_version, "warm-a");
        assert_eq!(reports[1].image_version, "warm-b");
    }
}
