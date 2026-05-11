//! `SandboxBackend` wrapper that opportunistically returns warm-pool
//! sandboxes on `create()` and reports its pool state for heartbeats.
//!
//! The host-agent owns one `PooledBackend` wrapping its real backend
//! (Process or Firecracker). Coordinator-side `--mode=all` also wraps
//! the local backend in a `PooledBackend` so warm-pool semantics apply
//! identically to single-binary dev and multi-host production.
//!
//! Pool key is `image_version` (i.e. `spec.image`). With image
//! identity decoupled from workspace identity in phase 2, a single
//! warm slot serves any session referencing the same image — the
//! pool's job is to amortise the create-time cost of a given image,
//! not enforce per-repo isolation.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_chunk_store::ChunkStore;
use engram_core::traits::SandboxBackend;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use engram_protocol::WarmPoolReport;
use tokio::fs;
use tokio::sync::Mutex;

use crate::egress::HostEgress;
use crate::image_cache::{CachedImage, ImageBundle, ImageCache};
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
    /// Phase 5+: if `Some`, `create()` resolves `spec.image_uri`
    /// through this cache before delegating to `inner`. Setting the
    /// cached `rootfs.ext4` path on `spec.rootfs_source` lets the
    /// existing warm-pool logic and inner backend keep working
    /// unchanged. `None` is the legacy single-host path.
    image_cache: Option<ImageCache>,
    /// ADR 0006: per-host egress proxy. When attached, incoming
    /// `notify_session_policy` calls register against the local
    /// proxy registry and `destroy` unregisters. `None` is the
    /// "no egress filtering" path (dev or operator opt-out).
    egress: Option<Arc<HostEgress>>,
    /// `sandbox_id → session_id` index so `destroy(sandbox_id)` can
    /// call `Registry::unregister(session_id)`. Populated when a
    /// policy frame arrives. Skipped for sandboxes never policied
    /// (warm-pool slots that never bound to a session).
    egress_sessions: DashMap<SandboxId, SessionId>,
    /// ADR 0007: chunk-store-backed materialization. When set, the
    /// `bundle.json` on a cached image is the source of truth for
    /// the disk — chunks are fetched from `BlobStorage`, written to
    /// a content-addressed file under `materialize_dir`, and that
    /// path becomes `spec.rootfs_source`. When unset, we fall back
    /// to the OCI-pulled `rootfs.ext4` (transitional path; retired
    /// in Phase 6).
    chunk_store: Option<ChunkStore>,
    /// Per-host directory where chunked manifests are materialized.
    /// `Some` iff `chunk_store` is. Files inside are named by
    /// `manifest_id`-`version` so two sessions hitting the same
    /// manifest share the materialized file (and VZ's per-sandbox
    /// APFS clonefile / FC's NBD-on-the-shared-path work on top).
    materialize_dir: Option<PathBuf>,
    /// Single-flight gate: stops two concurrent `create()` calls
    /// from racing on the same manifest_id+version file. Held only
    /// for the materialize critical section, not the whole call.
    materialize_lock: Mutex<()>,
}

impl PooledBackend {
    pub fn new(inner: Arc<dyn SandboxBackend>, default_target: u32) -> Self {
        Self {
            pool: Pool::new(inner.clone()),
            inner,
            default_target,
            image_cache: None,
            egress: None,
            egress_sessions: DashMap::new(),
            chunk_store: None,
            materialize_dir: None,
            materialize_lock: Mutex::new(()),
        }
    }

    /// Attach a chunk store + a per-host directory where chunked
    /// manifests get materialized. Once set, `create()` resolves the
    /// cached image's `bundle.json` → manifest → file via the chunk
    /// store, bypassing the OCI-pulled `rootfs.ext4`. Required for
    /// chunked storage to actually flow through dev / production
    /// sessions.
    pub fn with_chunk_store(mut self, chunk_store: ChunkStore, materialize_dir: PathBuf) -> Self {
        self.chunk_store = Some(chunk_store);
        self.materialize_dir = Some(materialize_dir);
        self
    }

    /// Attach an image cache. Calling this after `new()` lets
    /// `create()` resolve `spec.image_uri` to a cached on-disk path
    /// before forwarding to the inner backend.
    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// Attach a host-egress proxy. Once set, incoming
    /// `notify_session_policy` calls register against this proxy's
    /// registry and `destroy` unregisters. The CA PEM is available
    /// to substrate-builders via `host_egress.ca_cert_pem`.
    pub fn with_egress(mut self, egress: Arc<HostEgress>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Snapshot the pool's `(ready, target)` counts per image_version
    /// for inclusion in the next outbound heartbeat. Cheap (locks the
    /// pool's mutex briefly).
    pub fn snapshot_warm_pools(&self) -> Vec<WarmPoolReport> {
        self.pool.snapshot_reports()
    }

    /// Resolve the disk for a cached image. When a chunk store is
    /// wired AND the cached image carries a Bundle, materialize the
    /// disk from the chunk manifest into a content-addressed file
    /// in `materialize_dir`. Otherwise hand back the OCI-pulled
    /// `rootfs.ext4` unchanged. Idempotent: a second call against
    /// the same manifest hits the materialized file directly.
    async fn resolve_rootfs(
        &self,
        uri: &str,
        cached: &CachedImage,
    ) -> Result<PathBuf, SandboxError> {
        let (chunk_store, materialize_dir) =
            match (self.chunk_store.as_ref(), self.materialize_dir.as_ref()) {
                (Some(cs), Some(dir)) => (cs, dir),
                _ => return Ok(cached.rootfs_path.clone()),
            };
        let bundle = match cached.bundle.as_ref() {
            Some(b) => b,
            None => return Ok(cached.rootfs_path.clone()),
        };
        materialize_chunked_rootfs(
            chunk_store,
            materialize_dir,
            uri,
            bundle,
            &self.materialize_lock,
        )
        .await
    }

    fn pool_key(spec: &SandboxSpec) -> PoolKey {
        // `rootfs_source` is set by the image cache (OCI path) or
        // the caller (local:// dev) before this is called — see
        // `create()`. Two specs with the same `image` tag but
        // different rootfs files now hash to distinct keys, so the
        // known-issues-#1 collision (two `local://` repos sharing
        // `image: "warm-1"` but shipping different rootfs) can't
        // happen.
        PoolKey {
            image_version: spec.image.clone(),
            rootfs_source: spec.rootfs_source.clone(),
        }
    }
}

/// Materialize a chunked manifest to a content-addressed file under
/// `materialize_dir`. The output path is named by
/// `manifest_id-v{version}.ext4` so two sessions referencing the same
/// manifest deterministically land at the same file (the inner backend
/// can then APFS-clonefile or NBD-attach on top). Idempotent: returns
/// fast when the file already exists with the expected size.
async fn materialize_chunked_rootfs(
    chunk_store: &ChunkStore,
    materialize_dir: &std::path::Path,
    uri: &str,
    bundle: &ImageBundle,
    materialize_lock: &Mutex<()>,
) -> Result<PathBuf, SandboxError> {
    fs::create_dir_all(materialize_dir)
        .await
        .map_err(|e| SandboxError::Vm(format!("materialize dir: {e}").into()))?;
    let manifest_ref = bundle.disk_manifest;
    let dest = materialize_dir.join(format!(
        "{}-v{}.ext4",
        manifest_ref.manifest_id, manifest_ref.version,
    ));

    // Fast-path: file exists at the expected size. We trust local-
    // disk integrity here — the chunk store's content addressing
    // already validates bytes on the way in.
    let manifest = chunk_store
        .get_manifest(manifest_ref)
        .await
        .map_err(|e| SandboxError::Vm(format!("get manifest {manifest_ref}: {e}").into()))?;
    if let Ok(meta) = fs::metadata(&dest).await {
        if meta.len() == manifest.total_bytes {
            tracing::debug!(
                uri = %uri,
                manifest = %manifest_ref,
                path = %dest.display(),
                "chunked rootfs already materialized",
            );
            return Ok(dest);
        }
    }

    // Slow-path: hold the lock so two concurrent creates against the
    // same manifest don't both walk the chunks. The second waiter
    // sees the file once the first releases and short-circuits via
    // the fast-path check we redo below.
    let _guard = materialize_lock.lock().await;
    if let Ok(meta) = fs::metadata(&dest).await {
        if meta.len() == manifest.total_bytes {
            return Ok(dest);
        }
    }

    tracing::info!(
        uri = %uri,
        manifest = %manifest_ref,
        chunks = manifest.chunks.len(),
        total_bytes = manifest.total_bytes,
        path = %dest.display(),
        "materializing chunked rootfs",
    );
    chunk_store
        .materialize_to_file(&manifest, &dest)
        .await
        .map_err(|e| SandboxError::Vm(format!("materialize {manifest_ref}: {e}").into()))?;
    Ok(dest)
}

/// Pick a stable name for the harness substrate's mount-root
/// subdirectory. Tries: (1) the env-injected
/// `ENGRAM_SESSION_HARNESS_NAME` hint set by `resolve_harness` —
/// the canonical name sessions select by; (2) the last path segment
/// of the registry URI (drops `:tag`) — robust fallback for paths
/// the coordinator hasn't annotated. The returned string lands in
/// `/run/engram/harnesses/<name>/harness` inside the VM, so it must
/// match what bootstrap exec's against — the env hint guarantees
/// alignment, the URI fallback is best-effort.
fn harness_name_for_substrate(spec: &SandboxSpec, uri: &str) -> String {
    if let Some(name) = spec.env.get("ENGRAM_SESSION_HARNESS_NAME") {
        return name.clone();
    }
    // last segment of `host/path/repo:tag` minus the `:tag`.
    let last = uri.rsplit('/').next().unwrap_or(uri);
    let no_tag = last.split(':').next().unwrap_or(last);
    no_tag.to_string()
}

#[async_trait]
impl SandboxBackend for PooledBackend {
    // Capability methods proxy to the wrapped backend — the pool is
    // a thin caching layer; whatever Process / VZ / FC reports
    // about itself is what callers see.
    fn harness_dial(&self) -> engram_core::traits::HarnessDial {
        self.inner.harness_dial()
    }

    async fn create(&self, mut spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        // Phase 5+: resolve OCI image_uri / harness_pack_uri to local
        // cached paths before warm-pool key derivation. Both paths
        // overwrite the legacy filesystem fields when set, so the
        // inner backend keeps working with rootfs_source / harness_
        // substrate unchanged. On cache miss we pull synchronously;
        // subsequent sessions for the same digest hit the cache and
        // pay only the FS cost.
        if let Some(cache) = &self.image_cache {
            if let Some(uri) = spec.image_uri.clone() {
                let cached = cache.ensure_image(&uri).await.map_err(|e| {
                    SandboxError::InvalidSpec(format!("image cache pull {uri}: {e}"))
                })?;
                tracing::debug!(uri = %uri, digest = %cached.digest, "image cache hit/pulled");
                spec.rootfs_source = Some(self.resolve_rootfs(&uri, &cached).await?);
            }
            if let Some(uri) = spec.harness_pack_uri.clone() {
                // Backends that attach the substrate as a virtio-blk
                // device (FC, VZ block-device mode) need a real ext4
                // file at `harness_substrate`. ProcessBackend is
                // fine with a directory but accepts an ext4 path
                // too. We resolve the harness name from the spec's
                // existing `engram_session_harness_name` env hint
                // when present, falling back to the URI's last path
                // segment. This keeps the in-VM `/run/engram/
                // harnesses/<name>/harness` invariant intact.
                let name = harness_name_for_substrate(&spec, &uri);
                // ADR 0006: when a local egress proxy is attached,
                // stamp its CA cert into the substrate so the guest
                // trust store accepts MITM leaves.
                let host_ca_pem = self.egress.as_ref().map(|e| e.ca_cert_pem.as_str());
                let cached = cache
                    .ensure_harness_ext4(&uri, &name, host_ca_pem)
                    .await
                    .map_err(|e| {
                        SandboxError::InvalidSpec(format!("harness cache pull {uri}: {e}"))
                    })?;
                tracing::debug!(
                    uri = %uri,
                    name = %name,
                    digest = %cached.digest,
                    ext4 = %cached.ext4_path.display(),
                    "harness substrate built"
                );
                spec.harness_substrate = Some(cached.ext4_path);
            }
        }

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
        // Unregister from the local egress proxy first, so any
        // outstanding traffic from a still-alive guest stops being
        // rewritten. The destroy below tears down the VM; in the
        // brief window between the two, a closed-fail-by-default
        // registry would reject — which is the safe behavior.
        if let Some(egress) = self.egress.as_ref() {
            if let Some((_, session_id)) = self.egress_sessions.remove(&id) {
                egress.registry.unregister(session_id);
            }
        }
        self.inner.destroy(id).await
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        self.inner.start_agent(id, agent).await
    }

    fn set_harness_sink(&self, sink: engram_core::traits::HarnessSink) {
        self.inner.set_harness_sink(sink);
    }

    async fn notify_session_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError> {
        let Some(egress) = self.egress.as_ref() else {
            // No proxy attached — egress is unfiltered. The
            // coordinator may still send policy frames (the
            // coordinator-side codepath doesn't know whether a host
            // happens to have a proxy); silently no-op.
            tracing::debug!(
                session_id = %policy.session_id,
                "notify_session_policy: no local egress proxy, ignoring",
            );
            return Ok(());
        };
        let sandbox_id = policy.sandbox_id;
        let session_id = policy.session_id;
        crate::egress::register_policy(&egress.registry, policy)
            .map_err(|e| SandboxError::InvalidSpec(format!("translate egress policy: {e}")))?;
        self.egress_sessions.insert(sandbox_id, session_id);
        tracing::debug!(
            %sandbox_id,
            %session_id,
            "egress policy registered with local proxy",
        );
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.inner.list().await
    }

    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        self.inner.guest_ip(id).await
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
            image_uri: None,
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: None,
            network: Default::default(),
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

    #[tokio::test]
    async fn same_image_tag_but_different_rootfs_does_not_collide() {
        // Known-issues #1 regression at the PooledBackend layer: two
        // specs with identical `image` strings but different
        // `rootfs_source` paths must not share a warm slot. Before
        // the fix the second `create` would hand back a sandbox
        // configured with the first spec's rootfs and the harness
        // would silently load wrong-image binaries.
        let dir = tempfile::tempdir().unwrap();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
        let pooled = PooledBackend::new(inner, 1);

        // ProcessBackend materialises `rootfs_source` (it copies a
        // directory tree into the sandbox cwd), so both paths have
        // to be real directories. Their contents don't matter for
        // the pool-key test — only the path strings do.
        let demo_rootfs = dir.path().join("demo");
        let oauth_rootfs = dir.path().join("oauth");
        std::fs::create_dir_all(&demo_rootfs).unwrap();
        std::fs::create_dir_all(&oauth_rootfs).unwrap();

        let mut demo = live_spec("warm-1");
        demo.rootfs_source = Some(demo_rootfs);
        let mut oauth = live_spec("warm-1");
        oauth.rootfs_source = Some(oauth_rootfs);

        let _ = pooled.create(demo).await.unwrap();
        let _ = pooled.create(oauth).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Two pool entries, both reported under the same
        // `image_version` (heartbeat shape is unchanged) but
        // internally keyed on distinct rootfs paths.
        let reports = pooled.snapshot_warm_pools();
        assert_eq!(
            reports.len(),
            2,
            "specs with distinct rootfs must produce two pool entries even when image strings match",
        );
        assert!(reports.iter().all(|r| r.image_version == "warm-1"));
    }

    #[tokio::test]
    async fn materialize_chunked_rootfs_round_trips_and_dedupes() {
        // ADR 0007 acceptance: a chunked manifest is sufficient to
        // reproduce the disk bit-for-bit, AND concurrent resolves
        // share the materialized file rather than each producing
        // their own.
        use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
        let tmp = tempfile::tempdir().unwrap();
        // Local-filesystem BlobStorage = production code path for
        // dev. Same trait the host-agent uses against GCS in prod.
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // A fake "ext4" — small but enough to span more than one
        // 16 MiB chunk so the chunking path is exercised end-to-end.
        let src = tmp.path().join("rootfs.ext4");
        let mut bytes = vec![0u8; 24 * 1024 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        tokio::fs::write(&src, &bytes).await.unwrap();
        let manifest = cs.chunk_file(&src, ManifestKind::Disk, None).await.unwrap();
        let mref = ManifestRef::new();
        cs.put_manifest(mref, &manifest).await.unwrap();

        let bundle = ImageBundle {
            schema_version: 1,
            disk_manifest: mref,
        };
        let materialize_dir = tmp.path().join("materialized");
        let lock = Mutex::new(());

        let path1 = materialize_chunked_rootfs(&cs, &materialize_dir, "img:1", &bundle, &lock)
            .await
            .unwrap();
        let restored = tokio::fs::read(&path1).await.unwrap();
        assert_eq!(restored, bytes, "byte-for-byte mismatch");

        // Second call short-circuits via the size check — same path,
        // no rewrite.
        let mtime_before = std::fs::metadata(&path1).unwrap().modified().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let path2 = materialize_chunked_rootfs(&cs, &materialize_dir, "img:1", &bundle, &lock)
            .await
            .unwrap();
        assert_eq!(path1, path2);
        let mtime_after = std::fs::metadata(&path2).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "second call must not rewrite the file",
        );
    }
}
