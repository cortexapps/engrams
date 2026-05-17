//! `SandboxBackend` wrapper that adds host-side resource resolution
//! (image cache, chunk store, materialize-to-file, optional NBD,
//! optional egress proxy) on top of the inner backend (VZ or FC).
//!
//! Originally this also held a warm pool of pre-created sandboxes;
//! that was deleted as part of the ADR 0008 follow-up — see commit
//! history for context. Briefly: warm pools' value (~30 ms saved on
//! cold start) didn't earn its complexity in the multi-tenant
//! world we'd grown into (every session is a different
//! SandboxSpec, so the per-key warm-slot hit rate trended to
//! zero), and the FC production-target replacement
//! (canonical-memory restore from ADR 0007 Phase 5) is strictly
//! better. VZ dev now pays full cold-boot (~1 s) per session;
//! that's the explicit tradeoff.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_chunk_store::{ChunkCache, ChunkStore};
use engram_core::traits::SandboxBackend;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId, SessionId};
use tokio::fs;
use tokio::sync::Mutex;

use crate::egress::HostEgress;
use crate::image_cache::{CachedImage, ImageBundle, ImageCache};

/// Cross-platform optional NBD state slot. Linux carries the real
/// state; non-Linux is `()` so the `resolve_rootfs` return signature
/// is consistent and call sites cfg-gate the storage-stash branch.
#[cfg(target_os = "linux")]
type NbdStateSlot = Option<crate::disk_daemon::NbdSandboxState>;
#[cfg(not(target_os = "linux"))]
type NbdStateSlot = ();

#[cfg(target_os = "linux")]
fn nbd_state_none() -> NbdStateSlot {
    None
}
#[cfg(not(target_os = "linux"))]
fn nbd_state_none() -> NbdStateSlot {}

/// Return the cached image's legacy `rootfs.ext4` path or a
/// typed error when neither it nor a bundle is present.
///
/// ADR 0007 Phase 6: chunked-storage OCI pushes ship only
/// `bundle.json`; the disk is resolved through the chunk store.
/// Callers hit this helper on the fallback paths (no chunk_store
/// wired, or no bundle on the image) — at that point we *need* a
/// concrete file, and the absence of both rootfs.ext4 AND bundle
/// is a config error worth surfacing loudly rather than papering
/// over.
fn legacy_rootfs_path(cached: &CachedImage) -> Result<PathBuf, SandboxError> {
    cached.rootfs_path.clone().ok_or_else(|| {
        SandboxError::InvalidSpec(format!(
            "image {} has neither rootfs.ext4 nor a chunk-store path resolvable here \
             (host missing chunk_store/materialize_dir wiring?)",
            cached.digest,
        ))
    })
}

/// Wraps an inner [`SandboxBackend`] (FC or VZ) with host-side
/// resource resolution: image cache, chunk store, materialize-to-
/// file, optional NBD daemon, optional egress proxy.
///
/// Pre-warm-pool-deletion this also held a `Pool` of pre-created
/// sandboxes and a `pool_key`-based checkout dance. Both are gone;
/// `create` just resolves resources and forwards to `inner`.
/// Cold-create cost on each backend:
///
/// - **FC + canonical-memory-baked image**: ~100-500 ms via UFFD-
///   from-chunks restore. The "warm" of post-warm-pool-world.
/// - **FC, legacy bake**: ~500-700 ms full cold boot.
/// - **VZ**: ~1 s full cold boot. No memory-snapshot primitive
///   available on macOS arm64 Linux (Apple-side bug).
pub struct PooledBackend {
    inner: Arc<dyn SandboxBackend>,
    /// Phase 5+: if `Some`, `create()` resolves `spec.image_uri`
    /// through this cache before delegating to `inner`. The cache
    /// owns OCI pull + on-disk layout; `create` reads
    /// `cached.bundle` to decide between rootfs paths (legacy
    /// ext4, ADR 0007 chunked manifest, ADR 0008 chunked-OCI).
    image_cache: Option<ImageCache>,
    /// ADR 0006: per-host egress proxy. When attached, incoming
    /// `notify_session_policy` calls register against the local
    /// proxy registry and `destroy` unregisters. `None` is the
    /// "no egress filtering" path (dev or operator opt-out).
    egress: Option<Arc<HostEgress>>,
    /// `sandbox_id → session_id` index so `destroy(sandbox_id)` can
    /// call `Registry::unregister(session_id)`. Populated when a
    /// policy frame arrives.
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
    /// Optional NVMe-backed LRU cache fronting the chunk store.
    /// When wired, chunk reads during materialization go through
    /// the cache — chunks shared across manifests (canonical-base
    /// images, fork lineage) stay local across rematerializes
    /// rather than re-fetching from `BlobStorage` every time.
    /// When unset, `materialize_chunked_rootfs` goes straight to
    /// the store.
    chunk_cache: Option<ChunkCache>,
    /// Single-flight gate: stops two concurrent `create()` calls
    /// from racing on the same manifest_id+version file. Held only
    /// for the materialize critical section, not the whole call.
    materialize_lock: Mutex<()>,
    /// ADR 0008 Phase 5: OCI client used as the *origin tier* of the
    /// tiered chunk-fault path. When `Some`, `resolve_rootfs` checks
    /// `cached.is_disk_chunked_oci()` and builds a
    /// `TieredChunkResolver` that consults BlobStorage first and
    /// falls through to OCI Range GET on miss, opportunistically
    /// write-filling BlobStorage with the fetched chunk (CDN-fill).
    /// `None` keeps the legacy BlobStorage-only path.
    oci_client: Option<engram_oci::OciClient>,
    /// ADR 0007 Phase 4: pool of `/dev/nbdN` device paths the
    /// host-agent allocates from when serving chunked disks via
    /// the NBD daemon. `None` = NBD disabled (the materialize-to-
    /// file fallback runs instead — that's the macOS path and the
    /// transitional Linux path before operators wire `nbds_max=N`).
    /// Cross-platform because the allocator itself is target-
    /// agnostic; the daemon that consumes a slot is Linux-only.
    nbd_pool: Option<Arc<crate::disk_daemon::NbdSlotAllocator>>,
    /// Live NBD daemon state, keyed by sandbox id. Each entry
    /// owns the daemon's kernel-binding handle, the data-plane
    /// backend (used for snapshot flush), and the slot lease
    /// (Drop returns `/dev/nbdN` to the pool). Linux-only — on
    /// macOS the materialize-to-file path is the only option for
    /// chunked disks.
    #[cfg(target_os = "linux")]
    nbd_sandboxes: Arc<DashMap<SandboxId, crate::disk_daemon::NbdSandboxState>>,
}

impl PooledBackend {
    pub fn new(inner: Arc<dyn SandboxBackend>) -> Self {
        Self {
            inner,
            image_cache: None,
            egress: None,
            egress_sessions: DashMap::new(),
            chunk_store: None,
            materialize_dir: None,
            chunk_cache: None,
            materialize_lock: Mutex::new(()),
            oci_client: None,
            nbd_pool: None,
            #[cfg(target_os = "linux")]
            nbd_sandboxes: Arc::new(DashMap::new()),
        }
    }

    /// Attach a `/dev/nbdN` slot pool. When set + `chunk_store` +
    /// `chunk_cache` are also wired AND the cached image bundle
    /// carries a `disk_manifest`, `create()` spawns an NBD daemon
    /// instead of materializing the manifest to a single file. FC's
    /// `path_on_host` becomes `/dev/nbdN`. Linux-only at runtime;
    /// on macOS the builder accepts a pool but the spawn path is
    /// gated so dev workflows fall back to materialize-to-file.
    pub fn with_nbd_pool(mut self, pool: Arc<crate::disk_daemon::NbdSlotAllocator>) -> Self {
        self.nbd_pool = Some(pool);
        self
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

    /// Attach an NVMe-backed `ChunkCache`. Optional; chains on top
    /// of `with_chunk_store`. Production hosts wire one to
    /// amortise repeated chunk reads across manifests; dev /
    /// single-host setups can skip it without breaking the
    /// materialization path.
    pub fn with_chunk_cache(mut self, cache: ChunkCache) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Attach an image cache. Calling this after `new()` lets
    /// `create()` resolve `spec.image_uri` to a cached on-disk path
    /// before forwarding to the inner backend.
    pub fn with_image_cache(mut self, cache: ImageCache) -> Self {
        self.image_cache = Some(cache);
        self
    }

    /// ADR 0008 Phase 5: attach an `OciClient` to enable the
    /// chunks-in-OCI fault path. When set together with a chunk
    /// store, `resolve_rootfs` upgrades chunked-OCI images
    /// (`CachedImage::is_disk_chunked_oci()`) from BlobStorage-only
    /// to a tiered `BlobStorage → OCI` fault path with
    /// write-through fill to BlobStorage. Without this, chunked-OCI
    /// images degrade to legacy behavior — chunks must live in the
    /// host's BlobStorage namespace or session-create fails on
    /// first chunk fault.
    pub fn with_oci_client(mut self, client: engram_oci::OciClient) -> Self {
        self.oci_client = Some(client);
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

    /// Resolve the disk for a cached image. Three branches in
    /// priority order:
    ///
    /// 1. **NBD path** (Linux + `nbd_pool` + `chunk_store` +
    ///    `chunk_cache` + `bundle.disk_manifest` all present):
    ///    spawn an NBD daemon serving the chunk manifest, return
    ///    `(/dev/nbdN, Some(NbdSandboxState))`. The state is held
    ///    by `create()` and stashed in `nbd_sandboxes` keyed by
    ///    the eventual SandboxId.
    /// 2. **Materialize-to-file** (chunk_store + materialize_dir +
    ///    bundle): walk the chunk manifest into a single ext4
    ///    file at `<materialize_dir>/<manifest_id>-v<n>.ext4`,
    ///    return that path. The legacy chunked path; still the
    ///    only option on macOS or when NBD isn't wired.
    /// 3. **OCI fallback**: hand back the OCI-pulled `rootfs.ext4`
    ///    unchanged. The pre-chunked-storage path; retiring as
    ///    every host gets a chunk_store.
    async fn resolve_rootfs(
        &self,
        uri: &str,
        cached: &CachedImage,
    ) -> Result<(PathBuf, NbdStateSlot), SandboxError> {
        // ADR 0008 Phase 5 final piece: chunked-OCI images carry
        // the `Manifest` object only in the bake's BlobStorage
        // namespace. The runtime host's namespace may be empty.
        // Synthesize the manifest from the bootstrap sidecar (which
        // arrived with the OCI pull) and persist it before either
        // disk-resolution branch reads it. Idempotent — skips when
        // the manifest is already present.
        if let Some(cs) = self.chunk_store.as_ref() {
            ensure_chunked_manifest_in_blob_storage(cs, cached)
                .await
                .map_err(|e| {
                    tracing::error!(uri = %uri, error = %e, "chunked-OCI manifest synthesis failed");
                    e
                })?;
        }

        // Branch 1: NBD daemon.
        #[cfg(target_os = "linux")]
        if let Some(state) = self.try_spawn_nbd(uri, cached).await? {
            tracing::info!(
                uri = %uri,
                device = %state.device_path().display(),
                "rootfs branch: NBD daemon",
            );
            return Ok((state.device_path().to_path_buf(), Some(state)));
        }

        // Branch 2: materialize-to-file (the chunked path).
        let (chunk_store, materialize_dir) = match (
            self.chunk_store.as_ref(),
            self.materialize_dir.as_ref(),
        ) {
            (Some(cs), Some(dir)) => (cs, dir),
            _ => {
                let path = legacy_rootfs_path(cached)?;
                tracing::debug!(uri = %uri, path = %path.display(), "rootfs branch: legacy ext4 (no chunk store wired)");
                return Ok((path, nbd_state_none()));
            }
        };
        let bundle = match cached.bundle.as_ref() {
            Some(b) => b,
            None => {
                let path = legacy_rootfs_path(cached)?;
                tracing::debug!(uri = %uri, path = %path.display(), "rootfs branch: legacy ext4 (no bundle)");
                return Ok((path, nbd_state_none()));
            }
        };

        // ADR 0008 Phase 5: when the image is chunked-OCI shaped AND
        // an `OciClient` is wired, build a per-call `ChunkStore`
        // with a `TieredChunkResolver` so chunks missing from
        // BlobStorage fall through to OCI Range GET. Successful OCI
        // fetches tee back into BlobStorage (CDN-fill) so subsequent
        // sessions on the same host hit the cache tier.
        //
        // For non-chunked-OCI images (legacy ADR 0007 path,
        // BlobStorage-only) we use the default chunk_store as-is.
        let effective_store = self
            .upgrade_chunk_store_for_chunked_oci(chunk_store, uri, cached)
            .await
            .map_err(|e| {
                tracing::error!(uri = %uri, error = %e, "chunk-store upgrade for chunked-OCI failed");
                e
            })?;
        tracing::debug!(
            uri = %uri,
            manifest = %bundle.disk_manifest,
            chunked_oci = cached.is_disk_chunked_oci(),
            "rootfs branch: materialize-to-file",
        );

        // ADR 0008: chunk reads inside `materialize_to_file_cached`
        // route through `effective_store` (the per-session tiered
        // store) via the cache's closure-based fetcher. The cache
        // itself is backend-agnostic — no rebind needed.
        let path = materialize_chunked_rootfs(
            &effective_store,
            self.chunk_cache.as_ref(),
            materialize_dir,
            uri,
            bundle,
            &self.materialize_lock,
        )
        .await?;
        Ok((path, nbd_state_none()))
    }

    /// ADR 0008 Phase 5: upgrade the base `ChunkStore` to a tiered
    /// one when the cached image carries Nydus-shaped chunked OCI
    /// layers AND an `OciClient` is configured. Returns the
    /// original store unchanged otherwise — non-chunked-OCI images
    /// don't need the OCI tier.
    ///
    /// Errors here surface from bootstrap parsing (corrupt sidecar)
    /// or the chunk-store's blob_storage handle; both indicate a
    /// real failure mode worth aborting on.
    async fn upgrade_chunk_store_for_chunked_oci(
        &self,
        base: &ChunkStore,
        uri: &str,
        cached: &CachedImage,
    ) -> Result<ChunkStore, SandboxError> {
        let oci = match self.oci_client.as_ref() {
            Some(c) => c,
            None => return Ok(base.clone()),
        };
        if !cached.is_disk_chunked_oci() {
            return Ok(base.clone());
        }
        let index = cached
            .build_oci_chunk_index()
            .await
            .map_err(|e| SandboxError::Vm(format!("chunked-OCI index: {e}").into()))?;
        let index = match index {
            Some(i) => i,
            None => return Ok(base.clone()),
        };
        let blob = base.blob_storage();
        let cache_tier: Arc<dyn engram_chunk_store::ChunkResolver> =
            Arc::new(engram_chunk_store::BlobStorageResolver::new(blob.clone()));
        let origin_tier: Arc<dyn engram_chunk_store::ChunkResolver> = Arc::new(
            engram_oci::OciChunkResolver::new(oci.clone(), uri.to_string(), index),
        );
        let tiered: Arc<dyn engram_chunk_store::ChunkResolver> = Arc::new(
            engram_chunk_store::TieredChunkResolver::new(vec![cache_tier, origin_tier], Some(blob)),
        );
        tracing::debug!(
            uri = %uri,
            "chunked-OCI image: installing TieredChunkResolver for materialize",
        );
        Ok(base.clone().with_resolver(tiered))
    }

    /// Linux-only NBD spawn branch. Returns `None` when any
    /// prerequisite (pool / chunk_store / chunk_cache /
    /// bundle.disk_manifest) is missing. Caller falls back to
    /// materialize-to-file in that case.
    ///
    /// For chunked-OCI v2 images the store is wrapped with the
    /// same `TieredChunkResolver` the materialize-to-file branch
    /// uses, so NBD reads of chunks absent from BlobStorage fall
    /// through to OCI Range GET and tee back into BlobStorage.
    /// Without this the NBD daemon 404s forever on first-touch
    /// chunks.
    #[cfg(target_os = "linux")]
    async fn try_spawn_nbd(
        &self,
        uri: &str,
        cached: &CachedImage,
    ) -> Result<Option<crate::disk_daemon::NbdSandboxState>, SandboxError> {
        let (pool, store, cache) = match (
            self.nbd_pool.as_ref(),
            self.chunk_store.as_ref(),
            self.chunk_cache.as_ref(),
        ) {
            (Some(p), Some(s), Some(c)) => (p, s, c),
            _ => return Ok(None),
        };
        let bundle = match cached.bundle.as_ref() {
            Some(b) => b,
            None => return Ok(None),
        };
        let effective_store = self
            .upgrade_chunk_store_for_chunked_oci(store, uri, cached)
            .await
            .map_err(|e| {
                tracing::error!(uri = %uri, error = %e, "chunk-store upgrade for chunked-OCI (NBD) failed");
                e
            })?;
        let store_arc = Arc::new(effective_store);
        let state = crate::disk_daemon::attach_manifest(
            bundle.disk_manifest,
            cache.clone(),
            store_arc,
            pool,
        )
        .await
        .map_err(|e| SandboxError::Vm(format!("nbd attach_manifest: {e}").into()))?;
        Ok(Some(state))
    }

    /// Materialise `<src>/memory.bin` from the FC sidecar JSON's
    /// `memory_manifest` (if both the manifest exists in the
    /// chunk store and the local file is absent). Used by
    /// `restore()` to bootstrap cross-host transfers — the
    /// snapshot directory may carry only `state.bin` +
    /// `manifest.json` when staged by the coord; this fills in
    /// the memory file from the chunked durability tier.
    ///
    /// Returns early-Ok on every common skip condition (no chunk
    /// store wired, sidecar absent, memory_manifest unset, file
    /// already present). Failures bubble; callers can log + fall
    /// through to inner.restore which will surface a clearer
    /// "memory.bin missing" error.
    /// ADR 0014 M1.13: parallel prefetch the memory manifest's
    /// chunks into the local NVMe chunk cache. Idempotent (no-op
    /// when the cache is already warm) and best-effort (errors
    /// degrade to the serial fault path inside materialize_to_file_cached).
    ///
    /// Concurrency is bounded at 8 — well under the NIC's
    /// saturation point on the prod n2-standard-8 hosts (single-
    /// stream GCS hits ~80 MB/s; 8× parallel = ~640 MB/s, half the
    /// 10 Gbps line rate) and well below GCS's per-object rate
    /// limits.
    async fn prefetch_memory_chunks(
        &self,
        metadata: &SnapshotMetadata,
    ) -> Result<usize, SandboxError> {
        let Some(mref) = metadata.memory_manifest else {
            return Ok(0);
        };
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return Ok(0);
        };
        let Some(cache) = self.chunk_cache.as_ref() else {
            return Ok(0);
        };
        // ADR 0014 M1.14: if the snapshot has a published working-
        // set trace, narrow the prefetch to those chunks only —
        // typically a small subset of the full manifest (~5-15
        // chunks vs ~30+). Shrinks cold-cache refill from ~2 s to
        // ~500 ms in the best case. Fallback to full-manifest
        // prefetch (M1.13 behavior) when the trace isn't present,
        // unreachable, or empty.
        let hashes_to_prefetch: Vec<_> = match metadata.working_set_blob_key.as_deref() {
            Some(ws_key) => match self.fetch_working_set_chunks(chunk_store, ws_key).await {
                Ok(chunks) if !chunks.is_empty() => {
                    tracing::debug!(
                        ws_key,
                        chunk_count = chunks.len(),
                        "warm-pool refill: prefetch narrowed to working set",
                    );
                    chunks
                }
                Ok(_) => {
                    tracing::debug!(
                        ws_key,
                        "warm-pool refill: working set empty, falling back to full manifest",
                    );
                    let manifest = chunk_store.get_manifest(mref).await.map_err(|e| {
                        SandboxError::Snapshot(format!("prefetch get_manifest {mref}: {e}"))
                    })?;
                    manifest.chunks.iter().map(|c| c.hash).collect()
                }
                Err(e) => {
                    tracing::warn!(
                        ws_key,
                        error = %e,
                        "warm-pool refill: working set fetch failed, falling back to full manifest",
                    );
                    let manifest = chunk_store.get_manifest(mref).await.map_err(|e| {
                        SandboxError::Snapshot(format!("prefetch get_manifest {mref}: {e}"))
                    })?;
                    manifest.chunks.iter().map(|c| c.hash).collect()
                }
            },
            None => {
                let manifest = chunk_store.get_manifest(mref).await.map_err(|e| {
                    SandboxError::Snapshot(format!("prefetch get_manifest {mref}: {e}"))
                })?;
                manifest.chunks.iter().map(|c| c.hash).collect()
            }
        };
        let chunk_count = hashes_to_prefetch.len();
        let store_for_fetch = chunk_store.clone();
        cache
            .prefetch_chunks_parallel(hashes_to_prefetch, 8, move |hash| {
                let s = store_for_fetch.clone();
                async move { s.get_chunk(hash).await }
            })
            .await
            .map_err(|e| SandboxError::Snapshot(format!("prefetch_chunks_parallel: {e}")))?;
        tracing::debug!(
            manifest = %mref,
            chunk_count,
            "warm-pool refill: memory chunks prefetched into NVMe",
        );
        Ok(chunk_count)
    }

    /// ADR 0014 M1.14: load a working_set.json from BlobStorage and
    /// return its chunk hashes. Used by `prefetch_memory_chunks`
    /// to narrow the parallel prefetch to the chunks the kernel
    /// actually touches on warm-restore. Returns `Ok(vec![])` when
    /// the blob is missing (older bakes that pre-date M1.14) so
    /// the caller falls back to full-manifest prefetch.
    async fn fetch_working_set_chunks(
        &self,
        chunk_store: &ChunkStore,
        ws_key: &str,
    ) -> Result<Vec<engram_chunk_store::manifest::ChunkHash>, SandboxError> {
        let blob = chunk_store.blob_storage();
        let bytes = match blob.get(ws_key).await {
            Ok(b) => b,
            Err(engram_core::BlobError::NotFound) => return Ok(Vec::new()),
            Err(e) => {
                return Err(SandboxError::Snapshot(format!(
                    "fetch working-set blob {ws_key}: {e}"
                )))
            }
        };
        let trace: engram_chunk_store::working_set::WorkingSetTrace =
            serde_json::from_slice(&bytes).map_err(|e| {
                SandboxError::Snapshot(format!("parse working-set trace {ws_key}: {e}"))
            })?;
        Ok(trace.chunks)
    }

    async fn materialize_memory_if_missing(
        &self,
        src: &std::path::Path,
    ) -> Result<(), SandboxError> {
        let mem_path = src.join("memory.bin");
        if fs::metadata(&mem_path).await.is_ok() {
            return Ok(());
        }
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return Ok(());
        };
        let manifest_json = src.join("manifest.json");
        let Ok(bytes) = fs::read(&manifest_json).await else {
            return Ok(());
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Ok(());
        };
        let mref_value = value.get("memory_manifest").filter(|v| !v.is_null());
        let Some(mref_value) = mref_value else {
            return Ok(());
        };
        let mref: engram_core::types::manifest::ManifestRef =
            serde_json::from_value(mref_value.clone()).map_err(|e| {
                SandboxError::Snapshot(format!(
                    "parse memory_manifest from {}: {e}",
                    manifest_json.display(),
                ))
            })?;
        // Ensure the destination dir exists (cross-host stages may
        // hand us a path that's missing intermediate components).
        if let Some(parent) = mem_path.parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                SandboxError::Snapshot(format!("create snapshot dir {}: {e}", parent.display(),))
            })?;
        }
        let manifest = chunk_store
            .get_manifest(mref)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("get_manifest {mref}: {e}")))?;
        match self.chunk_cache.as_ref() {
            Some(cache) => chunk_store
                .materialize_to_file_cached(&manifest, &mem_path, cache)
                .await
                .map_err(|e| {
                    SandboxError::Snapshot(format!("materialize memory.bin (cached): {e}"))
                })?,
            None => chunk_store
                .materialize_to_file(&manifest, &mem_path)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("materialize memory.bin: {e}")))?,
        }
        tracing::info!(
            manifest = %mref,
            path = %mem_path.display(),
            "materialised memory.bin from chunked memory manifest",
        );
        Ok(())
    }
}

/// ADR 0014 cross-host restore helper: download `state.bin` and the
/// FC sidecar JSON from `BlobStorage` into the snapshot staging
/// directory iff (a) the metadata carries portable blob keys and
/// (b) the local files aren't already there. Idempotent — same-
/// host restore is a pair of `fs::metadata` short-circuits.
async fn materialize_state_if_missing(
    blob: &dyn engram_core::traits::storage::BlobStorage,
    src: &std::path::Path,
    state_blob_key: Option<&str>,
    sidecar_blob_key: Option<&str>,
) -> Result<(), SandboxError> {
    // The dest paths mirror the inner FC backend's snapshot layout
    // (`<snapshot_dir>/{state.bin,manifest.json}`). Cross-host
    // restore may hand us a path whose parent doesn't exist yet.
    fs::create_dir_all(src).await.map_err(|e| {
        SandboxError::Snapshot(format!("create snapshot dir {}: {e}", src.display()))
    })?;
    let state_path = src.join("state.bin");
    if let Some(key) = state_blob_key {
        if fs::metadata(&state_path).await.is_err() {
            engram_chunk_store::snapshot_blob::download_file(blob, key, &state_path)
                .await
                .map_err(|e| {
                    SandboxError::Snapshot(format!("download state.bin from {key}: {e}"))
                })?;
            tracing::info!(
                path = %state_path.display(),
                key = key,
                "materialised state.bin from BlobStorage",
            );
        }
    }
    let sidecar_path = src.join("manifest.json");
    if let Some(key) = sidecar_blob_key {
        if fs::metadata(&sidecar_path).await.is_err() {
            engram_chunk_store::snapshot_blob::download_file(blob, key, &sidecar_path)
                .await
                .map_err(|e| {
                    SandboxError::Snapshot(format!("download sidecar.json from {key}: {e}"))
                })?;
            tracing::info!(
                path = %sidecar_path.display(),
                key = key,
                "materialised sidecar.json from BlobStorage",
            );
        }
    }
    Ok(())
}

/// ADR 0008 Phase 5 final piece: ensure the disk `Manifest`
/// referenced by `cached.bundle.disk_manifest` is reachable in
/// the host's `BlobStorage`. For chunked-OCI images, the bake-
/// side wrote the Manifest to its own BlobStorage namespace; the
/// runtime host's namespace may be empty. We synthesize from the
/// bootstrap sidecar (which arrived with the OCI pull) and
/// persist via `put_manifest`. Idempotent — skips when the
/// manifest is already in BlobStorage, or when the artifact is
/// not chunked-OCI shaped (legacy bundle.json / no bundle).
///
/// Called from `resolve_rootfs` before either disk-resolution
/// branch (NBD or materialize-to-file) reads the manifest.
pub async fn ensure_chunked_manifest_in_blob_storage(
    chunk_store: &ChunkStore,
    cached: &CachedImage,
) -> Result<(), SandboxError> {
    // Three short-circuits — non-chunked-OCI images don't need
    // this path at all.
    let Some(bundle) = cached.bundle.as_ref() else {
        return Ok(());
    };
    let Some(bs_path) = cached.disk_bootstrap_path.as_ref() else {
        return Ok(());
    };
    let manifest_ref = bundle.disk_manifest;

    // Already there? Most common case after the first session on a
    // given image: cheap stat-shape lookup, no-op.
    match chunk_store.get_manifest(manifest_ref).await {
        Ok(_) => return Ok(()),
        Err(engram_chunk_store::ChunkStoreError::Blob(engram_core::error::BlobError::NotFound)) => {
        }
        Err(e) => {
            tracing::error!(
                manifest_ref = %manifest_ref,
                error = %e,
                "chunked-OCI: probing for existing manifest hit unexpected error",
            );
            return Err(SandboxError::Vm(
                format!("probe manifest {manifest_ref}: {e}").into(),
            ));
        }
    }

    // Synthesize from the bootstrap on disk and persist. The
    // bootstrap was written by `pull_image` (`bootstrap.disk.json`
    // sidecar); it carries every byte of Manifest info we need.
    let bytes = fs::read(bs_path).await.map_err(|e| {
        tracing::error!(path = %bs_path.display(), error = %e, "chunked-OCI: read bootstrap failed");
        SandboxError::Vm(format!("read bootstrap {}: {e}", bs_path.display()).into())
    })?;
    let bootstrap: engram_chunk_store::Bootstrap = serde_json::from_slice(&bytes).map_err(|e| {
        tracing::error!(path = %bs_path.display(), error = %e, "chunked-OCI: parse bootstrap failed");
        SandboxError::Vm(format!("parse bootstrap {}: {e}", bs_path.display()).into())
    })?;
    let synthesized = bootstrap.to_manifest();
    tracing::info!(
        manifest_ref = %manifest_ref,
        chunks = synthesized.chunks.len(),
        total_bytes = synthesized.total_bytes,
        kind = ?synthesized.kind,
        "chunked-OCI: synthesizing manifest from bootstrap + persisting to BlobStorage",
    );
    match chunk_store.put_manifest(manifest_ref, &synthesized).await {
        Ok(_) => Ok(()),
        // VersionConflict means another concurrent caller raced us
        // to put the same manifest. Synthesis is deterministic
        // (same bootstrap → same chunk list at same offsets), so
        // their version is identical to ours. Treat as success.
        Err(engram_chunk_store::ChunkStoreError::VersionConflict { .. }) => {
            tracing::debug!(
                manifest_ref = %manifest_ref,
                "chunked-OCI: concurrent put_manifest beat us; using their copy",
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                manifest_ref = %manifest_ref,
                error = %e,
                "chunked-OCI: put_manifest of synthesized manifest failed",
            );
            Err(SandboxError::Vm(
                format!("persist synthesized manifest {manifest_ref}: {e}").into(),
            ))
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
    chunk_cache: Option<&ChunkCache>,
    materialize_dir: &std::path::Path,
    uri: &str,
    bundle: &ImageBundle,
    materialize_lock: &Mutex<()>,
) -> Result<PathBuf, SandboxError> {
    fs::create_dir_all(materialize_dir).await.map_err(|e| {
        tracing::error!(
            dir = %materialize_dir.display(),
            error = %e,
            "materialize_chunked_rootfs: create dir failed",
        );
        SandboxError::Vm(format!("materialize dir: {e}").into())
    })?;
    let manifest_ref = bundle.disk_manifest;
    let dest = materialize_dir.join(format!(
        "{}-v{}.ext4",
        manifest_ref.manifest_id, manifest_ref.version,
    ));

    // Fast-path: file exists at the expected size. We trust local-
    // disk integrity here — the chunk store's content addressing
    // already validates bytes on the way in.
    let manifest = chunk_store.get_manifest(manifest_ref).await.map_err(|e| {
        tracing::error!(
            uri = %uri,
            manifest_ref = %manifest_ref,
            error = %e,
            "materialize_chunked_rootfs: get_manifest failed",
        );
        SandboxError::Vm(format!("get manifest {manifest_ref}: {e}").into())
    })?;
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
        cached = chunk_cache.is_some(),
        "materializing chunked rootfs",
    );
    // Route chunk reads through the local NVMe cache when one is
    // attached. Without it we re-fetch from BlobStorage every
    // materialize even when chunks haven't changed across
    // manifests.
    match chunk_cache {
        Some(cache) => {
            chunk_store
                .materialize_to_file_cached(&manifest, &dest, cache)
                .await
        }
        None => chunk_store.materialize_to_file(&manifest, &dest).await,
    }
    .map_err(|e| {
        tracing::error!(
            uri = %uri,
            manifest_ref = %manifest_ref,
            dest = %dest.display(),
            chunks = manifest.chunks.len(),
            total_bytes = manifest.total_bytes,
            error = %e,
            "materialize_chunked_rootfs: materialize_to_file failed",
        );
        SandboxError::Vm(format!("materialize {manifest_ref}: {e}").into())
    })?;
    Ok(dest)
}

/// Chunk an FC `memory.bin` snapshot file into the chunk store
/// and commit a fresh memory manifest. Returns the new
/// `ManifestRef` so callers can attach it to the FC sidecar JSON
/// and `SnapshotMetadata`.
///
/// Uses [`engram_chunk_store::ChunkStore::chunk_file`] with the
/// default 512 KiB memory chunk size. Sparse / all-zero blocks
/// are omitted from the manifest (the chunk_file primitive already
/// short-circuits them); on restore the resolver treats absent
/// chunk entries as "use canonical mmap at that offset" so zero
/// pages cost zero chunks + zero bytes of object storage.
async fn chunk_memory_to_store(
    chunk_store: &ChunkStore,
    memory_bin: &std::path::Path,
) -> Result<engram_core::types::manifest::ManifestRef, SandboxError> {
    let manifest = chunk_store
        .chunk_file(memory_bin, engram_chunk_store::ManifestKind::Memory, None)
        .await
        .map_err(|e| {
            SandboxError::Snapshot(format!("chunk memory.bin {}: {e}", memory_bin.display(),))
        })?;
    let manifest_ref = engram_core::types::manifest::ManifestRef::new();
    chunk_store
        .put_manifest(manifest_ref, &manifest)
        .await
        .map_err(|e| {
            SandboxError::Snapshot(format!(
                "put_manifest {manifest_ref} for {}: {e}",
                memory_bin.display(),
            ))
        })?;
    Ok(manifest_ref)
}

/// Set / overwrite the `memory_manifest` field on the FC sidecar
/// JSON at `manifest_json`. Operates on the JSON value rather than
/// the FC backend's private `FcSnapshotManifest` type — the on-disk
/// shape is the wire contract between the two crates, and FC
/// deserializes via `#[serde(default)]` so JSON-level patches stay
/// compatible without a circular dependency.
async fn patch_fc_manifest_memory_ref(
    manifest_json: &std::path::Path,
    memory_manifest: engram_core::types::manifest::ManifestRef,
) -> Result<(), SandboxError> {
    let bytes = fs::read(manifest_json).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "read fc manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
        SandboxError::Snapshot(format!(
            "parse fc manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    let obj = value.as_object_mut().ok_or_else(|| {
        SandboxError::Snapshot(format!(
            "fc manifest.json {} root is not an object",
            manifest_json.display(),
        ))
    })?;
    obj.insert(
        "memory_manifest".into(),
        serde_json::to_value(memory_manifest)
            .map_err(|e| SandboxError::Snapshot(format!("serialize memory_manifest: {e}")))?,
    );
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|e| SandboxError::Snapshot(format!("serialize patched manifest.json: {e}")))?;
    fs::write(manifest_json, bytes).await.map_err(|e| {
        SandboxError::Snapshot(format!(
            "write patched manifest.json {}: {e}",
            manifest_json.display(),
        ))
    })?;
    Ok(())
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
    harness_name_from_uri(uri)
}

/// ADR 0014 M1.12: spec-less variant for the warm-lease path. The
/// warm-pool slot doesn't carry an `ENGRAM_SESSION_HARNESS_NAME` env
/// hint (it's per-session, not per-template), so the URI's last
/// path segment is the only signal. Same fallback the cold path
/// uses when the env hint is missing.
pub fn harness_name_from_uri(uri: &str) -> String {
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
        // `pending_nbd_state` carries the NBD daemon's state from
        // the `resolve_rootfs` call (which spawns it) into the
        // post-`inner.create` stash step. Linux-only — on macOS
        // resolve_rootfs always returns `()` for the second slot.
        #[cfg(target_os = "linux")]
        let mut pending_nbd_state: Option<crate::disk_daemon::NbdSandboxState> = None;
        // ADR 0014 follow-up: per-phase boot timing. Captured as
        // sum-of-durations because the image + harness branches are
        // independently optional (image-only specs skip the harness
        // pull and vice-versa). The histogram labels follow the
        // contract in `crate::metrics::SANDBOX_BOOT_SECONDS`.
        let phase_total = std::time::Instant::now();
        let mut image_resolve = std::time::Duration::ZERO;
        let mut materialize = std::time::Duration::ZERO;
        let result: Result<SandboxId, SandboxError> = async {
            if let Some(cache) = &self.image_cache {
                if let Some(uri) = spec.image_uri.clone() {
                    let t = std::time::Instant::now();
                    let cached = cache.ensure_image(&uri).await.map_err(|e| {
                        SandboxError::InvalidSpec(format!("image cache pull {uri}: {e}"))
                    })?;
                    image_resolve += t.elapsed();
                    tracing::debug!(uri = %uri, digest = %cached.digest, "image cache hit/pulled");
                    let t = std::time::Instant::now();
                    let (path, _state) = self.resolve_rootfs(&uri, &cached).await?;
                    materialize += t.elapsed();
                    spec.rootfs_source = Some(path);
                    // ADR 0007 Phase 5: lift the bundle's canonical
                    // memory manifest onto the spec so FC backend's
                    // snapshot can stamp it on `FcSnapshotManifest.
                    // canonical_memory_manifest`. UFFD handler reads
                    // that on restore and serves shared-canonical
                    // reads from the per-image page cache.
                    if let Some(bundle) = cached.bundle.as_ref() {
                        spec.canonical_memory_manifest = bundle.canonical_memory_manifest;
                    }
                    #[cfg(target_os = "linux")]
                    {
                        pending_nbd_state = _state;
                    }
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
                    let t = std::time::Instant::now();
                    let cached = cache
                        .ensure_harness_ext4(&uri, &name, host_ca_pem)
                        .await
                        .map_err(|e| {
                            SandboxError::InvalidSpec(format!("harness cache pull {uri}: {e}"))
                        })?;
                    image_resolve += t.elapsed();
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

            // `fc_boot` is emitted by the inner FC backend itself
            // (see `engram_sandbox_firecracker::create`), with the
            // same metric name + label set. We don't double-record
            // here because that backend has finer-grained insight
            // into the sub-steps of the FC `PUT` calls if we want
            // to drill in later.
            let sandbox_id = self.inner.create(spec).await?;
            // Stash any spawned NBD daemon under the freshly-assigned
            // sandbox id. `destroy()` removes the entry (Drop tears
            // down the daemon + returns the slot); `snapshot()` reads
            // it back to call `backend.flush()`.
            #[cfg(target_os = "linux")]
            if let Some(state) = pending_nbd_state {
                self.nbd_sandboxes.insert(sandbox_id, state);
            }
            Ok(sandbox_id)
        }
        .await;

        let outcome = match &result {
            Ok(_) => "success",
            Err(SandboxError::InvalidSpec(_)) => "invalid_spec",
            Err(_) => "fc_error",
        };
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "image_resolve",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(image_resolve.as_secs_f64());
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "materialize",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(materialize.as_secs_f64());
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "create_total",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(phase_total.elapsed().as_secs_f64());
        result
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        self.inner.exec_stream(id, cmd).await
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        // ADR 0007 Phase 4: if this sandbox is NBD-backed, flush
        // its dirty disk chunks BEFORE we ask the inner FC backend
        // to take its memory snapshot. The flush produces a new
        // disk manifest version that we attach to
        // `SnapshotMetadata.disk_manifest`; the snapshot row's
        // chunked-disk pointer matches the bytes the kernel saw
        // at quiesce time. Flushing AFTER FC's pause + memory
        // capture would race the post-pause writes the guest
        // might queue.
        #[cfg(target_os = "linux")]
        let nbd_disk_manifest = if let Some(entry) = self.nbd_sandboxes.get(&id) {
            let outcome = entry
                .backend
                .flush()
                .await
                .map_err(|e| SandboxError::Snapshot(format!("nbd disk flush: {e}")))?;
            tracing::info!(
                sandbox_id = %id,
                manifest = %outcome.manifest_ref,
                chunks_flushed = outcome.chunks_flushed,
                bytes_uploaded = outcome.bytes_uploaded,
                "chunked NBD disk flushed",
            );
            Some(outcome.manifest_ref)
        } else {
            None
        };

        // ADR 0007 Phase 6: backend owns its staging dir; we look
        // it up via snapshot_path_for after the inner call so we
        // can read/patch the on-disk artifacts the inner backend
        // wrote (memory.bin, manifest.json).
        let mut metadata = self.inner.snapshot(id).await?;
        let dest = self.inner.snapshot_path_for(metadata.id);

        // Plumb the new disk manifest onto SnapshotMetadata so
        // the coord-side snapshot recorder persists it on the
        // `snapshots` row's `disk_manifest_*` columns.
        #[cfg(target_os = "linux")]
        if let Some(mref) = nbd_disk_manifest {
            metadata.disk_manifest = Some(mref);
        }
        // ADR 0007 / Phase 5: when a chunk store is wired AND the
        // underlying backend left a memory.bin in `dest` (FC does;
        // VZ + Process don't), chunk it into the store + patch the
        // FC snapshot manifest so the UFFD handler can resolve
        // session-divergent pages on restore. Skipped silently when
        // either condition isn't met — VZ + Process paths still
        // produce valid snapshots without a memory_manifest, and
        // FC without a chunk_store falls back to RestoreMode::File.
        let Some(chunk_store) = self.chunk_store.as_ref() else {
            return Ok(metadata);
        };
        let mem_path = dest.join("memory.bin");
        if fs::metadata(&mem_path).await.is_err() {
            return Ok(metadata);
        }
        let manifest_ref = chunk_memory_to_store(chunk_store, &mem_path).await?;
        // Patch the FC sidecar JSON (`manifest.json`) so its
        // `memory_manifest` field carries the ref the UFFD handler
        // needs at restore time. The FC backend deserializes via
        // serde with `#[serde(default)]`, so a JSON patch over the
        // wire-shape stays compatible without us depending on its
        // private struct.
        let manifest_json = dest.join("manifest.json");
        patch_fc_manifest_memory_ref(&manifest_json, manifest_ref).await?;
        metadata.memory_manifest = Some(manifest_ref);
        tracing::info!(
            session_sandbox = %id,
            manifest = %manifest_ref,
            "chunked FC memory.bin → chunk store",
        );

        // ADR 0014: upload state.bin + sidecar to BlobStorage so a
        // sibling host can restore from this snapshot. memory.bin
        // is already chunk-stored above; state.bin and sidecar are
        // small opaque blobs (state.bin is FC VMM+device state,
        // sidecar is `manifest.json` carrying spec + memory_manifest
        // + source sandbox_id). Skipped silently when state.bin is
        // missing (defensive: should always exist after FC snapshot,
        // but the chunked-memory path already gates on memory.bin
        // existence for the same reason).
        let blob = chunk_store.blob_storage();
        let state_path = dest.join("state.bin");
        let sidecar_path = dest.join("manifest.json");
        if fs::metadata(&state_path).await.is_ok() && fs::metadata(&sidecar_path).await.is_ok() {
            let state_key = engram_chunk_store::snapshot_blob::state_blob_key(metadata.id);
            let sidecar_key = engram_chunk_store::snapshot_blob::sidecar_blob_key(metadata.id);
            engram_chunk_store::snapshot_blob::upload_file(blob.as_ref(), &state_key, &state_path)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("upload state.bin: {e}")))?;
            engram_chunk_store::snapshot_blob::upload_file(
                blob.as_ref(),
                &sidecar_key,
                &sidecar_path,
            )
            .await
            .map_err(|e| SandboxError::Snapshot(format!("upload sidecar.json: {e}")))?;
            metadata.state_blob_key = Some(state_key);
            metadata.sidecar_blob_key = Some(sidecar_key);
            metadata.source_sandbox_id = Some(id);
            tracing::info!(
                source_sandbox = %id,
                snapshot_id = %metadata.id,
                "portable snapshot artifacts uploaded to BlobStorage",
            );
        }
        Ok(metadata)
    }

    fn snapshot_path_for(&self, snapshot_id: engram_core::types::SnapshotId) -> std::path::PathBuf {
        self.inner.snapshot_path_for(snapshot_id)
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        // ADR 0014 M1.13: eager parallel prefetch of memory chunks
        // into local NVMe BEFORE we hand off to materialize +
        // inner.restore. Without this, materialize_to_file_cached's
        // serial chunk iteration pays N×GCS-RTT (~300 ms each) on
        // a cold-cache host — a 512 MiB template's 32 chunks come
        // out as ~10 s of serial fetch. Parallel prefetch with
        // bounded concurrency reduces that to roughly ~2 s.
        // Subsequent restores against the same template hit NVMe
        // and the prefetch is a no-op.
        let prefetch_start = std::time::Instant::now();
        let prefetched_chunks = self.prefetch_memory_chunks(&metadata).await;
        if let Err(e) = prefetched_chunks.as_ref() {
            tracing::warn!(
                error = %e,
                snapshot_id = %metadata.id,
                "warm-pool memory chunk prefetch failed; falling back to serial fault path",
            );
        }
        let prefetch_outcome = if prefetched_chunks.is_ok() {
            "success"
        } else {
            "failed"
        };
        metrics::histogram!(
            crate::metrics::WARM_POOL_REFILL_SECONDS,
            "phase" => "prefetch",
            "outcome" => prefetch_outcome,
        )
        .record(prefetch_start.elapsed().as_secs_f64());

        // ADR 0007 Phase 5+6: cross-host memory.bin materialization.
        // The backend owns its staging dir layout (Phase 6); we ask
        // it where this snapshot would live, then ensure the
        // memory.bin file is present before delegating to inner —
        // either because we're on the same host where it was
        // written, or because we need to rebuild it from chunks
        // (cross-host migration). With M1.13's prefetch above, the
        // serial materialize loop now hits the NVMe-warm cache for
        // every chunk.
        let src = self.inner.snapshot_path_for(metadata.id);
        if let Err(e) = self.materialize_memory_if_missing(&src).await {
            tracing::warn!(
                error = %e,
                src = %src.display(),
                "memory.bin materialization failed; inner.restore will see whatever's there",
            );
        }

        // ADR 0014: cross-host state.bin + sidecar materialization.
        // Symmetric to memory.bin — if the metadata carries portable
        // blob keys and the local files aren't present, download
        // them from BlobStorage. Same-host restore (idle resume on
        // the host that produced the snapshot) hits the early-return
        // because the files are already at `src`.
        if let Some(chunk_store) = self.chunk_store.as_ref() {
            let blob = chunk_store.blob_storage();
            if let Err(e) = materialize_state_if_missing(
                blob.as_ref(),
                &src,
                metadata.state_blob_key.as_deref(),
                metadata.sidecar_blob_key.as_deref(),
            )
            .await
            {
                tracing::warn!(
                    error = %e,
                    src = %src.display(),
                    "state.bin/sidecar materialization failed; inner.restore will see whatever's there",
                );
            }
        }

        let restore_start = std::time::Instant::now();
        let inner_result = self.inner.restore(metadata).await;
        metrics::histogram!(
            crate::metrics::WARM_POOL_REFILL_SECONDS,
            "phase" => "restore",
            "outcome" => if inner_result.is_ok() { "success" } else { "failed" },
        )
        .record(restore_start.elapsed().as_secs_f64());
        inner_result
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
        let result = self.inner.destroy(id).await;
        // Tear down the NBD daemon AFTER the inner backend has
        // closed the VM (so the kernel doesn't surface "device
        // busy" on disconnect). Drop on NbdSandboxState handles
        // disconnect → join → slot release. Done last so even
        // if inner.destroy errors, the daemon cleanup still runs.
        #[cfg(target_os = "linux")]
        {
            let _ = self.nbd_sandboxes.remove(&id);
        }
        result
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
            canonical_memory_manifest: None,
        }
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
            canonical_memory_manifest: None,
            bootstrap_disk_available: false,
            bootstrap_memory_available: false,
        };
        let materialize_dir = tmp.path().join("materialized");
        let lock = Mutex::new(());

        let path1 =
            materialize_chunked_rootfs(&cs, None, &materialize_dir, "img:1", &bundle, &lock)
                .await
                .unwrap();
        let restored = tokio::fs::read(&path1).await.unwrap();
        assert_eq!(restored, bytes, "byte-for-byte mismatch");

        // Second call short-circuits via the size check — same path,
        // no rewrite.
        let mtime_before = std::fs::metadata(&path1).unwrap().modified().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let path2 =
            materialize_chunked_rootfs(&cs, None, &materialize_dir, "img:1", &bundle, &lock)
                .await
                .unwrap();
        assert_eq!(path1, path2);
        let mtime_after = std::fs::metadata(&path2).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "second call must not rewrite the file",
        );
    }

    /// ADR 0007 / Phase 5: snapshot wrap chunks an FC-style
    /// `memory.bin` into the chunk store and patches the FC sidecar
    /// `manifest.json` with the resulting `memory_manifest` ref.
    /// Validates that:
    ///  - `SnapshotMetadata.memory_manifest` carries the ref
    ///  - `manifest.json` gained a `memory_manifest` field
    ///  - the manifest in the chunk store is byte-equal to memory.bin
    ///    when materialised back
    ///
    /// Uses a fake inner backend (writes the snapshot artifacts to
    /// `dest` like FC does on a real snapshot, but skips the actual
    /// VM machinery) so the test runs cross-platform.
    #[tokio::test]
    async fn snapshot_chunks_fc_memory_bin_and_patches_manifest() {
        use engram_chunk_store::{ChunkStore, ManifestKind};
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();
        let _dest = tmp.path().join("snap-1");

        // 1. Synthetic memory.bin: 1 MiB of distinct, non-zero
        //    content so chunk_file produces multiple chunks. Two
        //    512 KiB chunks (default memory chunk size).
        let mut bytes = vec![0u8; 1024 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i % 200) + 1) as u8; // skip zero so chunks aren't elided
        }

        // 2. Inner backend stub: writes memory.bin + state.bin +
        //    manifest.json to its own per-snapshot dir (ADR 0007
        //    Phase 6 contract), returns a bare SnapshotMetadata
        //    with memory_manifest=None (the FC bare-snapshot shape).
        struct FakeFcBackend {
            payload: Vec<u8>,
            staging_root: PathBuf,
        }
        impl FakeFcBackend {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for FakeFcBackend {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"state-bin-placeholder")
                    .await
                    .unwrap();
                // Mirror FC's manifest.json shape minimally.
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1",
                        "rootfs_source": null,
                        "image_uri": null,
                        "harness_pack_uri": null,
                        "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64},
                        "disk": {"max_gib": 1},
                        "ttl": null,
                        "env": {},
                        "workdir": null,
                        "harness_substrate": null,
                        "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        // 3. Wire the PooledBackend with a chunk store.
        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob);
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeFcBackend {
            payload: bytes.clone(),
            staging_root: tmp.path().join("fc-snaps"),
        });
        let materialize_dir = tmp.path().join("materialized");
        let pooled = PooledBackend::new(inner).with_chunk_store(cs.clone(), materialize_dir);

        // 4. Take the snapshot. PooledBackend's wrap chunks
        //    memory.bin and patches manifest.json.
        let metadata = pooled.snapshot(SandboxId::new()).await.unwrap();
        let dest = pooled.snapshot_path_for(metadata.id);
        let manifest_ref = metadata
            .memory_manifest
            .expect("PooledBackend must populate memory_manifest");

        // 5. manifest.json now carries the same ref under
        //    `memory_manifest`.
        let mj: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(dest.join("manifest.json")).await.unwrap())
                .unwrap();
        let patched_ref: engram_core::types::manifest::ManifestRef =
            serde_json::from_value(mj["memory_manifest"].clone())
                .expect("memory_manifest field must round-trip");
        assert_eq!(patched_ref, manifest_ref);

        // 6. The chunk store materializes the manifest back to the
        //    exact bytes.
        let manifest = cs.get_manifest(manifest_ref).await.unwrap();
        assert!(matches!(manifest.kind, ManifestKind::Memory));
        assert_eq!(manifest.total_bytes, bytes.len() as u64);
        let recovered = tmp.path().join("recovered.bin");
        cs.materialize_to_file(&manifest, &recovered).await.unwrap();
        let recovered_bytes = tokio::fs::read(&recovered).await.unwrap();
        assert_eq!(recovered_bytes, bytes, "memory bytes round-trip");
    }

    /// ADR 0014: PooledBackend's snapshot wrap uploads state.bin +
    /// sidecar.json to BlobStorage and stamps the metadata with the
    /// portable blob keys. Mirrors the chunked-memory test pattern
    /// above but verifies the new state/sidecar upload step rather
    /// than the existing memory-chunk step.
    #[tokio::test]
    async fn snapshot_uploads_state_and_sidecar_to_blob_storage() {
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();

        // FakeFcBackend writes the three artifacts (memory.bin,
        // state.bin, manifest.json) into its staging dir, mirroring
        // what real FC `create_snapshot` produces.
        #[derive(Clone)]
        struct FakeFcBackend {
            payload: Vec<u8>,
            staging_root: PathBuf,
        }
        impl FakeFcBackend {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for FakeFcBackend {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"fake-state-bin-bytes")
                    .await
                    .unwrap();
                let manifest = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "test:1", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&manifest).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob.clone());
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let inner: Arc<dyn SandboxBackend> = Arc::new(FakeFcBackend {
            payload,
            staging_root: tmp.path().join("fc-snaps"),
        });
        let pooled = PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("mat"));

        let source_sandbox = SandboxId::new();
        let metadata = pooled.snapshot(source_sandbox).await.unwrap();

        // Portable fields populated.
        assert_eq!(
            metadata.source_sandbox_id,
            Some(source_sandbox),
            "PooledBackend must stamp source_sandbox_id",
        );
        let state_key = metadata
            .state_blob_key
            .as_ref()
            .expect("state_blob_key must be set after upload");
        let sidecar_key = metadata
            .sidecar_blob_key
            .as_ref()
            .expect("sidecar_blob_key must be set after upload");
        assert!(state_key.contains(&metadata.id.to_string()));
        assert!(sidecar_key.contains(&metadata.id.to_string()));

        // Blobs are actually in BlobStorage.
        let downloaded_state: bytes::Bytes = blob.get(state_key).await.unwrap();
        assert_eq!(&downloaded_state[..], b"fake-state-bin-bytes");
        let downloaded_sidecar: bytes::Bytes = blob.get(sidecar_key).await.unwrap();
        let sidecar: serde_json::Value = serde_json::from_slice(&downloaded_sidecar).unwrap();
        assert_eq!(sidecar["format"], "fc");
        assert_eq!(sidecar["spec"]["image"], "test:1");
    }

    /// ADR 0014: PooledBackend's restore wrap pulls state.bin +
    /// sidecar from BlobStorage when the local files are missing
    /// (cross-host restore case). Validated by deleting the local
    /// staging files between snapshot and restore — restore must
    /// rebuild them before delegating to inner.
    #[tokio::test]
    async fn restore_materializes_missing_state_and_sidecar_from_blob_storage() {
        use engram_chunk_store::ChunkStore;
        use engram_storage_local::LocalBlobStorage;

        let tmp = tempfile::tempdir().unwrap();

        // Inner backend that lets us drive snapshot then asserts on
        // restore that state.bin + manifest.json are present on disk
        // by the time it's invoked.
        struct AssertingInner {
            staging_root: PathBuf,
            payload: Vec<u8>,
            saw_state: parking_lot::Mutex<Option<Vec<u8>>>,
            saw_sidecar: parking_lot::Mutex<Option<Vec<u8>>>,
        }
        impl AssertingInner {
            fn dir_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
        }
        #[async_trait]
        impl SandboxBackend for AssertingInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                let snapshot_id = engram_core::SnapshotId::new();
                let dest = self.dir_for(snapshot_id);
                tokio::fs::create_dir_all(&dest).await.unwrap();
                tokio::fs::write(dest.join("memory.bin"), &self.payload)
                    .await
                    .unwrap();
                tokio::fs::write(dest.join("state.bin"), b"upload-me-state")
                    .await
                    .unwrap();
                let sidecar = serde_json::json!({
                    "sandbox_id": uuid::Uuid::new_v4(),
                    "created_at": chrono::Utc::now(),
                    "spec": {
                        "image": "t", "rootfs_source": null, "image_uri": null,
                        "harness_pack_uri": null, "cpu": {"vcpus": 1},
                        "memory": {"max_mib": 64}, "disk": {"max_gib": 1},
                        "ttl": null, "env": {}, "workdir": null,
                        "harness_substrate": null, "network": {}
                    },
                    "format": "fc"
                });
                tokio::fs::write(
                    dest.join("manifest.json"),
                    serde_json::to_vec_pretty(&sidecar).unwrap(),
                )
                .await
                .unwrap();
                Ok(SnapshotMetadata {
                    id: snapshot_id,
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "t".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                    source_sandbox_id: None,
                    state_blob_key: None,
                    sidecar_blob_key: None,
                    rootfs_blob_key: None,
                    working_set_blob_key: None,
                })
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.dir_for(id)
            }
            async fn restore(&self, meta: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                // Capture what was on disk at the moment inner.restore ran.
                let dest = self.dir_for(meta.id);
                let state = tokio::fs::read(dest.join("state.bin"))
                    .await
                    .expect("state.bin must be materialised before inner.restore");
                let sidecar = tokio::fs::read(dest.join("manifest.json"))
                    .await
                    .expect("manifest.json must be materialised before inner.restore");
                *self.saw_state.lock() = Some(state);
                *self.saw_sidecar.lock() = Some(sidecar);
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let blob: Arc<dyn engram_core::traits::BlobStorage> =
            Arc::new(LocalBlobStorage::new(tmp.path().join("blob")));
        let cs = ChunkStore::new(blob.clone());
        let payload: Vec<u8> = (0..(64 * 1024)).map(|i| (i % 251) as u8).collect();
        let inner = Arc::new(AssertingInner {
            staging_root: tmp.path().join("fc-snaps"),
            payload: payload.clone(),
            saw_state: parking_lot::Mutex::new(None),
            saw_sidecar: parking_lot::Mutex::new(None),
        });
        let inner_dyn: Arc<dyn SandboxBackend> = inner.clone();
        let pooled = PooledBackend::new(inner_dyn).with_chunk_store(cs, tmp.path().join("mat"));

        // Snapshot — uploads state + sidecar to BlobStorage.
        let metadata = pooled.snapshot(SandboxId::new()).await.unwrap();
        let staging = inner.dir_for(metadata.id);

        // Simulate cross-host: delete the local staging files so
        // restore has nothing to read from disk. Memory.bin is also
        // deleted; it gets materialised from the chunked manifest.
        tokio::fs::remove_file(staging.join("state.bin"))
            .await
            .unwrap();
        tokio::fs::remove_file(staging.join("manifest.json"))
            .await
            .unwrap();
        tokio::fs::remove_file(staging.join("memory.bin"))
            .await
            .unwrap();

        // Restore — wrap must re-download state.bin + sidecar
        // before delegating to inner.
        pooled.restore(metadata).await.unwrap();

        let seen_state = inner.saw_state.lock().clone().expect("inner.restore ran");
        assert_eq!(&seen_state[..], b"upload-me-state");
        let seen_sidecar = inner.saw_sidecar.lock().clone().expect("inner.restore ran");
        let parsed: serde_json::Value = serde_json::from_slice(&seen_sidecar).unwrap();
        assert_eq!(parsed["format"], "fc");
    }

    /// Snapshot wrap is a no-op when no chunk store is wired —
    /// metadata + manifest.json stay exactly as the inner backend
    /// produced them. Guards against accidental coupling between
    /// `with_chunk_store` and the snapshot path.
    #[tokio::test]
    async fn snapshot_no_chunk_store_passes_through_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir));
        let pooled = PooledBackend::new(inner);

        let _id = pooled.create(live_spec("warm-test")).await.unwrap();
        // ProcessBackend's snapshot writes the directory structure
        // without a memory.bin (no guest RAM concept). The wrap
        // skips chunking silently and metadata.memory_manifest
        // stays None.
        let metadata = pooled.snapshot(_id).await.expect("ProcessBackend snapshot");
        assert!(
            metadata.memory_manifest.is_none(),
            "no chunk_store wired must leave memory_manifest unset",
        );
    }

    /// ADR 0007 Phase 5: when `restore()` is called on a snapshot
    /// dir whose `memory.bin` is missing locally but whose sidecar
    /// JSON carries a `memory_manifest`, PooledBackend materialises
    /// the file from chunks before delegating to the inner backend.
    /// Validates by:
    ///   1. Build a chunk store + put a small memory manifest
    ///   2. Stage a snapshot dir with state.bin + manifest.json
    ///      (memory_manifest set, but NO memory.bin)
    ///   3. Call PooledBackend::restore
    ///   4. Assert memory.bin now exists at byte-equal content
    #[tokio::test]
    async fn restore_materializes_missing_memory_bin_from_chunks() {
        use engram_chunk_store::{ChunkStore, ManifestKind};
        use engram_core::types::manifest::ManifestRef;

        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // Plant a synthetic memory.bin (1 MiB of non-zero pattern)
        // and chunk it into the store. `chunk_file` skips all-zero
        // blocks; we want the test to exercise non-empty chunks.
        let src_path = tmp.path().join("source-memory.bin");
        let bytes: Vec<u8> = (0..(1024 * 1024)).map(|i| ((i % 251) + 1) as u8).collect();
        tokio::fs::write(&src_path, &bytes).await.unwrap();
        let manifest = cs
            .chunk_file(&src_path, ManifestKind::Memory, None)
            .await
            .unwrap();
        let manifest_ref = ManifestRef::new();
        cs.put_manifest(manifest_ref, &manifest).await.unwrap();

        // Inner backend: records the metadata it was called with
        // so we can assert PooledBackend's wrap fired the
        // materialize step BEFORE inner.restore. Owns a staging
        // root that snapshot_path_for derives from — the wrap
        // looks the path up from there to stage memory.bin.
        struct CapturingInner {
            captured: parking_lot::Mutex<Option<SnapshotMetadata>>,
            staging_root: PathBuf,
        }
        #[async_trait]
        impl SandboxBackend for CapturingInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                self.staging_root.join(id.to_string())
            }
            async fn restore(&self, m: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                *self.captured.lock() = Some(m);
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(&self, _: SandboxId, _: AgentSpec) -> Result<(), SandboxError> {
                Ok(())
            }
        }

        let inner = Arc::new(CapturingInner {
            captured: parking_lot::Mutex::new(None),
            staging_root: tmp.path().join("inner-snaps"),
        });

        // Allocate the snapshot_id up front so we can stage its
        // dir at the path snapshot_path_for will return.
        let snapshot_id = engram_core::SnapshotId::new();
        let snap_dir = inner.snapshot_path_for(snapshot_id);
        tokio::fs::create_dir_all(&snap_dir).await.unwrap();
        tokio::fs::write(snap_dir.join("state.bin"), b"state-placeholder")
            .await
            .unwrap();
        let manifest_json = serde_json::json!({
            "sandbox_id": uuid::Uuid::new_v4(),
            "created_at": chrono::Utc::now(),
            "spec": {
                "image": "test:1",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 64},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            },
            "format": "fc",
            "memory_manifest": manifest_ref,
        });
        tokio::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest_json).unwrap(),
        )
        .await
        .unwrap();
        assert!(
            !snap_dir.join("memory.bin").exists(),
            "precondition: memory.bin must be absent before restore"
        );

        let pooled = PooledBackend::new(inner.clone())
            .with_chunk_store(cs.clone(), tmp.path().join("materialized"));

        // Restore — should materialise memory.bin then delegate.
        let metadata = SnapshotMetadata {
            id: snapshot_id,
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: Some(manifest_ref),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
        };
        pooled.restore(metadata.clone()).await.unwrap();

        // Inner backend saw the metadata; memory.bin is now
        // present and byte-equal to the original.
        assert_eq!(
            inner.captured.lock().as_ref().map(|m| m.id),
            Some(snapshot_id)
        );
        let recovered = tokio::fs::read(snap_dir.join("memory.bin"))
            .await
            .expect("memory.bin must exist post-restore");
        assert_eq!(recovered, bytes, "materialized bytes round-trip");
    }

    /// Cross-host trace replay: when a snapshot carries a
    /// `trace_host_hint` on its sidecar JSON, the FC restore path
    /// passes that as `--prefault-trace <host>` to the UFFD
    /// handler so the recorded trace replays on a different host.
    /// Locked at the wire-shape level here — we assert the field
    /// round-trips through serde so a refactor can't accidentally
    /// drop it. (End-to-end replay needs a live VM; that's the
    /// follow-up.)
    #[tokio::test]
    async fn snapshot_skips_materialize_when_memory_bin_already_present() {
        // Negative case: memory.bin already exists locally, so
        // the materialize branch must be a no-op. Guards against a
        // refactor that accidentally truncates the file.
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = engram_chunk_store::ChunkStore::new(blob);

        let snap_dir = tmp.path().join("snap");
        tokio::fs::create_dir_all(&snap_dir).await.unwrap();
        // Pre-existing memory.bin with sentinel bytes.
        let original = b"original-memory-bytes";
        tokio::fs::write(snap_dir.join("memory.bin"), original)
            .await
            .unwrap();
        // Manifest.json claims a different memory_manifest. The
        // materialize branch MUST skip because the local file is
        // present.
        let manifest_json = serde_json::json!({
            "sandbox_id": uuid::Uuid::new_v4(),
            "created_at": chrono::Utc::now(),
            "spec": {
                "image": "test:1",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 64},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            },
            "format": "fc",
            "memory_manifest": {"manifest_id": uuid::Uuid::new_v4(), "version": 1},
        });
        tokio::fs::write(
            snap_dir.join("manifest.json"),
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await
        .unwrap();

        // Phase 6: the test backend now owns where snap_dir lives.
        // Use a CapturingInner that points snapshot_path_for at the
        // dir we staged, so PooledBackend's wrap finds the
        // already-existing memory.bin and the materialize branch
        // short-circuits.
        struct StagedInner {
            snap_dir: PathBuf,
        }
        #[async_trait]
        impl SandboxBackend for StagedInner {
            async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
                self.snap_dir.clone()
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                // Inner restore is irrelevant for this test — we
                // only assert the materialize branch was a no-op.
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
        }
        let inner: Arc<dyn SandboxBackend> = Arc::new(StagedInner {
            snap_dir: snap_dir.clone(),
        });
        let pooled =
            PooledBackend::new(inner).with_chunk_store(cs, tmp.path().join("materialized"));
        let metadata = SnapshotMetadata {
            id: engram_core::SnapshotId::new(),
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
        };
        let _ = pooled.restore(metadata).await;
        let after = tokio::fs::read(snap_dir.join("memory.bin")).await.unwrap();
        assert_eq!(after, original, "memory.bin must not be overwritten");
    }

    /// Cache wiring: when a `ChunkCache` is provided,
    /// `materialize_chunked_rootfs` routes chunk reads through it.
    /// We verify by materializing twice — once to populate, once
    /// against a "broken" store (drop the chunks underneath). The
    /// second materialize succeeds iff reads served from the cache.
    #[tokio::test]
    async fn materialize_chunked_rootfs_uses_chunk_cache_when_present() {
        use engram_chunk_store::{
            cache::ChunkCacheConfig, ChunkCache, ChunkStore, ManifestKind, ManifestRef,
        };

        let tmp = tempfile::tempdir().unwrap();
        let blob_root = tmp.path().join("blob");
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(blob_root.clone()),
        );
        let cs = ChunkStore::new(blob);
        let cache = ChunkCache::new(ChunkCacheConfig::new(tmp.path().join("chunk-cache")));

        // Plant a small source + manifest.
        let src = tmp.path().join("source.ext4");
        let bytes: Vec<u8> = (0..(1024 * 1024)).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&src, &bytes).await.unwrap();
        let manifest = cs.chunk_file(&src, ManifestKind::Disk, None).await.unwrap();
        let mref = ManifestRef::new();
        cs.put_manifest(mref, &manifest).await.unwrap();

        let bundle = ImageBundle {
            schema_version: 1,
            disk_manifest: mref,
            canonical_memory_manifest: None,
            bootstrap_disk_available: false,
            bootstrap_memory_available: false,
        };
        let materialize_dir = tmp.path().join("materialized");
        let lock = Mutex::new(());

        // First materialize warms the cache.
        let path1 = materialize_chunked_rootfs(
            &cs,
            Some(&cache),
            &materialize_dir,
            "img:cached",
            &bundle,
            &lock,
        )
        .await
        .unwrap();
        let restored = tokio::fs::read(&path1).await.unwrap();
        assert_eq!(restored, bytes, "first materialize must reproduce bytes");

        // Delete the materialized file and the underlying blob store —
        // the cache should still have everything we need.
        tokio::fs::remove_file(&path1).await.unwrap();
        let chunks_dir = blob_root.join("chunks");
        tokio::fs::remove_dir_all(&chunks_dir).await.unwrap();

        // Second materialize would fail if it hit the store; succeeds
        // when reads are served from the cache.
        let path2 = materialize_chunked_rootfs(
            &cs,
            Some(&cache),
            &materialize_dir,
            "img:cached",
            &bundle,
            &lock,
        )
        .await
        .expect("cache should serve chunks after store is gone");
        let restored2 = tokio::fs::read(&path2).await.unwrap();
        assert_eq!(restored2, bytes, "cached materialize must reproduce bytes");
    }

    /// Tier 1 regression guard for the ADR 0007 chunked-lifecycle
    /// rollout. Exercises the full `PooledBackend::create()` flow with
    /// a real `ImageCache` + `ChunkStore` wired, asserting that an
    /// `image_uri`-driven spec lands a chunked-materialized path on
    /// the inner backend's spec — not the OCI-pulled `rootfs.ext4`.
    ///
    /// Covered ground:
    /// - `ImageCache.ensure_image()` returns a `CachedImage.bundle`
    ///   when the on-disk artifact carries a `bundle.json`.
    /// - `PooledBackend.resolve_rootfs()` dispatches to the chunk
    ///   store rather than the cached file.
    /// - The materialized file lands at the deterministic content-
    ///   addressed path, and its bytes match the chunked source.
    /// - The inner backend's `create()` receives `spec.rootfs_source`
    ///   pointing at that materialized path.
    #[tokio::test]
    async fn create_with_image_uri_resolves_chunked_path_on_inner() {
        use crate::image_cache::ImageCache;
        use engram_chunk_store::{ChunkStore, ManifestKind, ManifestRef};
        use engram_core::traits::SandboxBackend;
        use engram_core::types::sandbox::{
            AgentSpec, CpuLimit, DiskLimit, ExecRequest, ExecStream, MemoryLimit,
        };
        use engram_core::types::snapshot::SnapshotMetadata;
        use engram_core::SandboxId;
        use parking_lot::Mutex as PlMutex;
        use std::path::PathBuf;

        // 1. Shared BlobStorage + ChunkStore, mirroring `--mode=all`.
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let cs = ChunkStore::new(blob);

        // 2. Plant a fake rootfs in the chunk store + produce a
        //    manifest. Smaller than the materialize round-trip test
        //    because we just need the wiring to flow, not multi-
        //    chunk content addressing — that's covered above.
        let source = tmp.path().join("source.ext4");
        let bytes: Vec<u8> = (0..(2 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
        tokio::fs::write(&source, &bytes).await.unwrap();
        let manifest = cs
            .chunk_file(&source, ManifestKind::Disk, None)
            .await
            .unwrap();
        let mref = ManifestRef::new();
        cs.put_manifest(mref, &manifest).await.unwrap();

        // 3. Stand up an ImageCache rooted under tmp, then plant a
        //    fake "OCI-pulled" artifact + a uri-map entry pointing at
        //    it. ensure_image will short-circuit via the cache-hit
        //    path without attempting a real OCI fetch.
        let cache_root = tmp.path().join("oci-cache");
        let oci = engram_oci::OciClient::new(Arc::new(engram_oci::AnonymousResolver));
        let cache = ImageCache::open(cache_root.clone(), oci).await.unwrap();
        let digest = "sha256:deadbeefcafefeedfacefeed";
        let image_dir = cache.image_dir_for_test(digest);
        tokio::fs::create_dir_all(&image_dir).await.unwrap();
        tokio::fs::write(
            image_dir.join("manifest.toml"),
            b"name = \"chunked-test\"\n",
        )
        .await
        .unwrap();
        // Drop a bytes-equal `rootfs.ext4` alongside so the cache-hit
        // existence check passes; the chunked path supersedes it.
        tokio::fs::write(image_dir.join("rootfs.ext4"), &bytes)
            .await
            .unwrap();
        let bundle_bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "disk_manifest": mref,
        }))
        .unwrap();
        tokio::fs::write(image_dir.join("bundle.json"), bundle_bytes)
            .await
            .unwrap();
        cache.prime_image_for_test("test:1", digest);

        // 4. Capturing inner backend — records the spec it receives.
        struct Capturing {
            captured: PlMutex<Option<SandboxSpec>>,
        }
        #[async_trait]
        impl SandboxBackend for Capturing {
            async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
                *self.captured.lock() = Some(spec);
                Ok(SandboxId::new())
            }
            async fn exec_stream(
                &self,
                _id: SandboxId,
                _cmd: ExecRequest,
            ) -> Result<ExecStream, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            fn snapshot_path_for(&self, id: engram_core::SnapshotId) -> PathBuf {
                std::env::temp_dir()
                    .join("engram-test-snaps")
                    .join(id.to_string())
            }
            async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
                Ok(Vec::new())
            }
            async fn start_agent(
                &self,
                _id: SandboxId,
                _agent: AgentSpec,
            ) -> Result<(), SandboxError> {
                Ok(())
            }
        }
        let captured = Arc::new(Capturing {
            captured: PlMutex::new(None),
        });
        let inner: Arc<dyn SandboxBackend> = captured.clone();

        // 5. PooledBackend wires both image cache + chunk store +
        //    materialize dir. We only care about the single create
        //    path here.
        let materialize_dir = tmp.path().join("materialized");
        let pooled = PooledBackend::new(inner)
            .with_image_cache(cache)
            .with_chunk_store(cs.clone(), materialize_dir.clone());

        // 6. Drive a session-create with image_uri set.
        let mut spec = SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            image_uri: Some("test:1".into()),
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: None,
            network: Default::default(),
            canonical_memory_manifest: None,
        };
        spec.image_uri = Some("test:1".into());
        let _id = pooled.create(spec).await.unwrap();

        // 7. The inner backend must have received the chunked
        //    materialized path, NOT the OCI-pulled rootfs.ext4.
        let received = captured.captured.lock().clone().expect("inner not called");
        let received_rootfs = received.rootfs_source.expect("rootfs_source not set");
        let expected =
            materialize_dir.join(format!("{}-v{}.ext4", mref.manifest_id, mref.version,));
        assert_eq!(
            received_rootfs, expected,
            "PooledBackend must route chunked manifests through materialize_dir, not the OCI rootfs.ext4",
        );

        // 8. The materialized file exists and is byte-equal.
        let materialized = tokio::fs::read(&expected).await.unwrap();
        assert_eq!(materialized, bytes, "materialized file bytes mismatch");
    }
}
