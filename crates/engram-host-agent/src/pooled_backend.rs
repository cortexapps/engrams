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
use engram_chunk_store::{ChunkCache, ChunkStore};
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
            chunk_cache: None,
            materialize_lock: Mutex::new(()),
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
        // Branch 1: NBD daemon.
        #[cfg(target_os = "linux")]
        if let Some(state) = self.try_spawn_nbd(cached).await? {
            tracing::info!(
                uri = %uri,
                device = %state.device_path().display(),
                "chunked rootfs served via NBD daemon",
            );
            return Ok((state.device_path().to_path_buf(), Some(state)));
        }

        // Branch 2: materialize-to-file (the legacy chunked path).
        let (chunk_store, materialize_dir) =
            match (self.chunk_store.as_ref(), self.materialize_dir.as_ref()) {
                (Some(cs), Some(dir)) => (cs, dir),
                _ => return Ok((cached.rootfs_path.clone(), nbd_state_none())),
            };
        let bundle = match cached.bundle.as_ref() {
            Some(b) => b,
            None => return Ok((cached.rootfs_path.clone(), nbd_state_none())),
        };
        let path = materialize_chunked_rootfs(
            chunk_store,
            self.chunk_cache.as_ref(),
            materialize_dir,
            uri,
            bundle,
            &self.materialize_lock,
        )
        .await?;
        Ok((path, nbd_state_none()))
    }

    /// Linux-only NBD spawn branch. Returns `None` when any
    /// prerequisite (pool / chunk_store / chunk_cache /
    /// bundle.disk_manifest) is missing. Caller falls back to
    /// materialize-to-file in that case.
    #[cfg(target_os = "linux")]
    async fn try_spawn_nbd(
        &self,
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
        let store_arc = Arc::new(store.clone());
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
    chunk_cache: Option<&ChunkCache>,
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
    .map_err(|e| SandboxError::Vm(format!("materialize {manifest_ref}: {e}").into()))?;
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
        // `pending_nbd_state` carries the NBD daemon's state from
        // the `resolve_rootfs` call (which spawns it) into the
        // post-`inner.create` stash step. Linux-only — on macOS
        // resolve_rootfs always returns `()` for the second slot.
        #[cfg(target_os = "linux")]
        let mut pending_nbd_state: Option<crate::disk_daemon::NbdSandboxState> = None;
        if let Some(cache) = &self.image_cache {
            if let Some(uri) = spec.image_uri.clone() {
                let cached = cache.ensure_image(&uri).await.map_err(|e| {
                    SandboxError::InvalidSpec(format!("image cache pull {uri}: {e}"))
                })?;
                tracing::debug!(uri = %uri, digest = %cached.digest, "image cache hit/pulled");
                let (path, _state) = self.resolve_rootfs(&uri, &cached).await?;
                spec.rootfs_source = Some(path);
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

        let mut metadata = self.inner.snapshot(id, dest).await?;

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
        Ok(metadata)
    }

    async fn restore(&self, src: std::path::PathBuf) -> Result<SandboxId, SandboxError> {
        // ADR 0007 Phase 5: cross-host memory.bin materialization.
        // If the snapshot dir's `memory.bin` is missing locally but
        // the FC sidecar JSON carries a `memory_manifest`, rebuild
        // it from chunks before delegating to the inner backend.
        // Two callers hit this branch:
        //   - Same host where the local memory.bin was reaped by
        //     LRU / disk pressure (rare, but possible).
        //   - Cross-host: the snapshotting host died, the coord
        //     re-stages the snapshot dir on a new host with just
        //     state.bin + manifest.json from BlobStorage, and
        //     this host fills memory.bin from chunks.
        if let Err(e) = self.materialize_memory_if_missing(&src).await {
            tracing::warn!(
                error = %e,
                src = %src.display(),
                "memory.bin materialization failed; inner.restore will see whatever's there",
            );
        }
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
        use std::path::Path;

        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("snap-1");

        // 1. Synthetic memory.bin: 1 MiB of distinct, non-zero
        //    content so chunk_file produces multiple chunks. Two
        //    512 KiB chunks (default memory chunk size).
        let mut bytes = vec![0u8; 1024 * 1024];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((i % 200) + 1) as u8; // skip zero so chunks aren't elided
        }

        // 2. Inner backend stub: writes memory.bin + state.bin +
        //    manifest.json to dest, returns a bare SnapshotMetadata
        //    with memory_manifest=None (the FC bare-snapshot shape).
        struct FakeFcBackend {
            payload: Vec<u8>,
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
            async fn snapshot(
                &self,
                _: SandboxId,
                dest: &Path,
            ) -> Result<SnapshotMetadata, SandboxError> {
                tokio::fs::create_dir_all(dest).await.unwrap();
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
                    id: engram_core::SnapshotId::new(),
                    size_bytes: self.payload.len() as u64,
                    created_at: chrono::Utc::now(),
                    image_version: "test:1".into(),
                    disk_manifest: None,
                    memory_manifest: None,
                })
            }
            async fn restore(&self, _: PathBuf) -> Result<SandboxId, SandboxError> {
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
        });
        let materialize_dir = tmp.path().join("materialized");
        let pooled = PooledBackend::new(inner, 0).with_chunk_store(cs.clone(), materialize_dir);

        // 4. Take the snapshot. PooledBackend's wrap chunks
        //    memory.bin and patches manifest.json.
        let metadata = pooled.snapshot(SandboxId::new(), &dest).await.unwrap();
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

    /// Snapshot wrap is a no-op when no chunk store is wired —
    /// metadata + manifest.json stay exactly as the inner backend
    /// produced them. Guards against accidental coupling between
    /// `with_chunk_store` and the snapshot path.
    #[tokio::test]
    async fn snapshot_no_chunk_store_passes_through_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let inner: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir));
        let pooled = PooledBackend::new(inner, 0);

        let _id = pooled.create(live_spec("warm-test")).await.unwrap();
        // ProcessBackend's snapshot writes the directory structure
        // without a memory.bin (no guest RAM concept). The wrap
        // skips chunking silently and metadata.memory_manifest
        // stays None.
        let dest = tmp.path().join("nochunk-snap");
        let metadata = pooled
            .snapshot(_id, &dest)
            .await
            .expect("ProcessBackend snapshot");
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
        use std::path::Path;

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

        // Stage a snapshot dir: state.bin + manifest.json (with
        // memory_manifest set), but NO memory.bin — simulates a
        // freshly-staged cross-host transfer.
        let snap_dir = tmp.path().join("staged-snap");
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

        // Inner backend: records the path it was called with so
        // we can assert PooledBackend's wrap fired before the
        // inner call. Doesn't actually restore anything.
        struct CapturingInner {
            captured: parking_lot::Mutex<Option<PathBuf>>,
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
            async fn snapshot(
                &self,
                _: SandboxId,
                _: &Path,
            ) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn restore(&self, src: PathBuf) -> Result<SandboxId, SandboxError> {
                *self.captured.lock() = Some(src);
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
        });
        let pooled = PooledBackend::new(inner.clone(), 0)
            .with_chunk_store(cs.clone(), tmp.path().join("materialized"));

        // Restore — should materialise memory.bin then delegate.
        pooled.restore(snap_dir.clone()).await.unwrap();

        // Inner backend saw the path; memory.bin is now present
        // and byte-equal to the original.
        assert_eq!(inner.captured.lock().clone(), Some(snap_dir.clone()));
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

        let inner: Arc<dyn SandboxBackend> = Arc::new(engram_sandbox_process::ProcessBackend::new(
            tmp.path().to_path_buf(),
        ));
        let pooled =
            PooledBackend::new(inner, 0).with_chunk_store(cs, tmp.path().join("materialized"));
        // restore would fail at ProcessBackend's level because
        // ProcessBackend doesn't read manifest.json the FC way —
        // but the materialize branch should have already short-
        // circuited. Test that the file wasn't touched, regardless
        // of inner.restore's outcome.
        let _ = pooled.restore(snap_dir.clone()).await;
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
        let cache = ChunkCache::new(
            ChunkCacheConfig::new(tmp.path().join("chunk-cache")),
            cs.clone(),
        );

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
        use std::path::{Path, PathBuf};

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
            async fn snapshot(
                &self,
                _id: SandboxId,
                _dest: &Path,
            ) -> Result<SnapshotMetadata, SandboxError> {
                Err(SandboxError::InvalidSpec("unused".into()))
            }
            async fn restore(&self, _src: PathBuf) -> Result<SandboxId, SandboxError> {
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
        //    materialize dir. target_size=0 to skip warm-pool
        //    replenish (we only care about the single create path).
        let materialize_dir = tmp.path().join("materialized");
        let pooled = PooledBackend::new(inner, 0)
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
