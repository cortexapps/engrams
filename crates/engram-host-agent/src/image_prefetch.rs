//! ADR 0015 M5: per-host image-readiness prefetch supervisor.
//!
//! Each host watches the heartbeat-ack's `enabled_images` set and warms each
//! enabled image's **base-snapshot working set** (disk + memory chunks) onto
//! local NVMe. Once those chunks are all reachable via `chunk_store.get_chunk()`
//! (cache-tier hit), the image's `manifest_digest` is added to `ImageReadiness`
//! — which the heartbeat builder reads on every tick to populate
//! `Heartbeat.ready_images`. Coord's scheduler gates session placement on hosts
//! that report the requested image's digest in their ready set; non-ready hosts
//! surface as HTTP 503 `image_not_ready` to the API caller.
//!
//! Why this exists: pre-M5, hosts learned about images via the `templates`
//! table cascade and warm-pool refill — implicit and coupled. The prefetch loop
//! replaces both with one explicit "diff enabled vs ready, fill the delta"
//! supervisor.
//!
//! **What we prefetch (and what we deliberately do NOT):** session create
//! restores from the per-image *base snapshot* (ADR 0020), whose disk manifest
//! is a superset of the image rootfs — it carries the template-boot writes the
//! image's own manifest lacks. So the source OCI image is never read at session
//! time. We therefore warm ONLY the base snapshot's disk + memory manifests
//! (ADR 0021 P2) and never pull the OCI image. Pulling the multi-GB OCI artifact
//! from the registry onto every host was pure redundancy — and an unbounded OCI
//! pull under a registry 429 retry storm wedged prod hosts during the ADR-0025
//! roll. The base-snapshot chunks live in the GCS chunk store (flushed at
//! enable), so this path is GCS-backed, not registry-backed.
//!
//! Storage tier model (for the per-chunk `get_chunk` walk):
//!   - Tier 1: local NVMe `ChunkCache` (canonical "ready" inventory; hit here =
//!     chunk is local + counts toward readiness).
//!   - Tier 2: `BlobStorage` at `chunks/sha256/<hex>` (GCS) — where the base
//!     snapshot's chunks are flushed at enable. This is the hot tier here.
//!   - Tier 3: OCI Range GET (safety net; CDN-fills BlobStorage on a tier-2
//!     miss). In practice the base-snapshot chunks are always in tier 2.
//!
//! `chunk_store.get_chunk(hash)` walks the tiers and tees on miss, so the
//! prefetch driver is just an eager loop over the base snapshot's chunk hashes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use engram_chunk_store::{ChunkCache, ChunkStore, Manifest};
use engram_core::types::SnapshotId;
use engram_protocol::heartbeat::{EnabledImageRef, ManifestDigest};
use parking_lot::RwLock;
use tokio::sync::{watch, Semaphore};

/// ADR 0022 Option A: resolves a base snapshot's id to its on-disk
/// snapshot dir (`<work_dir>/snapshots/<id>`). Supplied by the
/// host-agent as `pooled.snapshot_path_for` so the residency-materialized
/// memfile lands at the *exact* path a base `session.create` restore
/// reads — they agree by construction rather than by duplicated layout
/// logic. `Some` ⇒ density on (materialize the per-template memfile +
/// gate readiness on it); `None` ⇒ off (behaviour-preserving).
pub type SnapshotDirResolver = Arc<dyn Fn(SnapshotId) -> PathBuf + Send + Sync>;

/// Shared, mutable view of "which images is this host ready to
/// serve?" — written by the prefetch supervisor, read by the
/// heartbeat builder.
#[derive(Default, Debug)]
pub struct ImageReadiness {
    inner: RwLock<HashSet<ManifestDigest>>,
}

impl ImageReadiness {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot the current ready set for inclusion on an outbound
    /// heartbeat. Returns a `Vec` rather than a `HashSet` to
    /// match the wire shape.
    pub fn snapshot(&self) -> Vec<ManifestDigest> {
        self.inner.read().iter().cloned().collect()
    }

    pub fn contains(&self, digest: &ManifestDigest) -> bool {
        self.inner.read().contains(digest)
    }

    fn mark_ready(&self, digest: ManifestDigest) {
        self.inner.write().insert(digest);
    }

    fn mark_unready(&self, digest: &ManifestDigest) {
        self.inner.write().remove(digest);
    }
}

/// Concurrency cap on in-flight chunk fetches, applied across all
/// images this host is prefetching. 16 permits ~ 400 MiB in flight
/// at the default 25 MiB chunk size — leaves 10 Gbps NIC headroom
/// for in-flight sessions' lazy faults. Tunable via the env var
/// `ENGRAM_PREFETCH_CONCURRENCY`.
const DEFAULT_PREFETCH_CONCURRENCY: usize = 16;

fn concurrency_from_env() -> usize {
    std::env::var("ENGRAM_PREFETCH_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_PREFETCH_CONCURRENCY)
}

/// Periodic re-check. Catches LRU evictions of chunks from
/// `ChunkCache` — if any chunk for a ready image is no longer
/// resolvable through tier 1, flip the image back to not-ready so
/// the next prefetch round re-fills.
const RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Spawn the supervisor. Returns the [`watch::Sender`] the
/// heartbeat handler should push every received `enabled_images`
/// set into. The supervisor task runs for the lifetime of the
/// returned [`tokio::task::JoinHandle`] — drop it (or `abort()`)
/// to stop. ChunkStore is required; without it nothing can fault
/// chunks. No ImageCache: the prefetch warms the base snapshot's
/// chunks from the GCS chunk store and never reads the source OCI
/// image (see [`prefetch_one`]).
pub fn spawn_supervisor(
    chunk_store: ChunkStore,
    chunk_cache: ChunkCache,
    readiness: Arc<ImageReadiness>,
    // ADR 0022 Option A: when `Some`, also materialize each enabled
    // image's contiguous per-template base memfile at residency (and gate
    // readiness on it). `None` ⇒ density off; the prefetch warms only
    // chunks, exactly as before.
    base_memfile_dir: Option<SnapshotDirResolver>,
) -> (
    watch::Sender<Vec<EnabledImageRef>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = watch::channel(Vec::<EnabledImageRef>::new());
    let permits = concurrency_from_env();
    let semaphore = Arc::new(Semaphore::new(permits));
    tracing::info!(
        permits,
        recheck_secs = RECHECK_INTERVAL.as_secs(),
        base_memfile = base_memfile_dir.is_some(),
        "image prefetch supervisor starting (base-snapshot only; no OCI prefetch)",
    );
    let handle = tokio::spawn(async move {
        // ADR 0022: digest → materialized base memfile path, so a later
        // disable can reclaim the (guest-RAM-sized) file. Lives across
        // ticks. Only populated when `base_memfile_dir` is `Some`.
        let mut memfiles: HashMap<ManifestDigest, PathBuf> = HashMap::new();
        loop {
            let enabled = rx.borrow_and_update().clone();
            reconcile(
                &enabled,
                readiness.clone(),
                chunk_store.clone(),
                chunk_cache.clone(),
                semaphore.clone(),
                base_memfile_dir.as_ref(),
                &mut memfiles,
            )
            .await;

            // Sleep until either a new enabled_images set arrives
            // or the LRU-recheck timer fires. `changed()` returns
            // Err only when every sender drops — that's process
            // shutdown.
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("image prefetch: enabled_images sender dropped; supervisor exiting");
                        break;
                    }
                }
                _ = tokio::time::sleep(RECHECK_INTERVAL) => {}
            }
        }
    });
    (tx, handle)
}

/// One reconciliation pass: compute the ready/enabled delta, drop
/// disabled-but-still-ready digests, kick off prefetch for any
/// enabled-but-not-yet-ready digest.
async fn reconcile(
    enabled: &[EnabledImageRef],
    readiness: Arc<ImageReadiness>,
    chunk_store: ChunkStore,
    chunk_cache: ChunkCache,
    semaphore: Arc<Semaphore>,
    base_memfile_dir: Option<&SnapshotDirResolver>,
    memfiles: &mut HashMap<ManifestDigest, PathBuf>,
) {
    let current = readiness.snapshot();
    let current: HashSet<ManifestDigest> = current.into_iter().collect();
    let enabled_set: HashSet<ManifestDigest> =
        enabled.iter().map(|i| i.manifest_digest.clone()).collect();

    // Drop images that are no longer enabled. We don't evict the
    // chunks — they're still valid content, possibly shared with
    // other images. LRU on the chunk cache handles eviction on
    // disk pressure.
    for digest in current.difference(&enabled_set) {
        readiness.mark_unready(digest);
        // ADR 0022: reclaim the per-template base memfile (a
        // guest-RAM-sized file) when its image is disabled. Best-effort
        // unlink; a live sharer keeps the inode alive via its MAP_PRIVATE
        // mapping even after the dentry is gone, so this is safe to do
        // while sessions are still running.
        if let Some(path) = memfiles.remove(digest) {
            let p = path.clone();
            tokio::spawn(async move {
                match tokio::fs::remove_file(&p).await {
                    Ok(()) => {
                        tracing::info!(path = %p.display(), "reclaimed base memfile on image disable")
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        tracing::warn!(path = %p.display(), error = %e, "base memfile reclaim failed")
                    }
                }
            });
        }
        tracing::info!(
            digest = digest.as_str(),
            "image disabled; removed from ready set",
        );
    }

    // For each enabled image we haven't filled yet, fan out a
    // prefetch task. We don't gather here — completing prefetches
    // mark themselves ready and the heartbeat picks them up
    // independently. The shared semaphore caps total in-flight
    // chunk fetches across all images.
    for image in enabled {
        if current.contains(&image.manifest_digest) {
            continue;
        }
        // ADR 0022: where this image's contiguous per-template base
        // memfile must land — the SAME path a base session.create restore
        // reads (`snapshot_path_for(base_snapshot_id)/memory.bin`). `None`
        // when density is off. Recorded for disable-time reclaim.
        let base_memfile =
            base_memfile_dir.map(|resolver| resolver(image.base_snapshot_id).join("memory.bin"));
        if let Some(ref path) = base_memfile {
            memfiles.insert(image.manifest_digest.clone(), path.clone());
        }
        // Clone the whole ref into the task — it carries everything
        // prefetch_one needs (uri, digest, base-snapshot disk + memory
        // manifests). Two Strings + two Copy refs; cheap per reconcile.
        let image = image.clone();
        let readiness = readiness.clone();
        let chunk_store = chunk_store.clone();
        let chunk_cache = chunk_cache.clone();
        let semaphore = semaphore.clone();
        tokio::spawn(async move {
            match prefetch_one(&image, &chunk_store, &chunk_cache, &semaphore, base_memfile).await {
                Ok(chunks) => {
                    readiness.mark_ready(image.manifest_digest.clone());
                    tracing::info!(
                        image_uri = %image.image_uri,
                        digest = image.manifest_digest.as_str(),
                        chunks,
                        "image base snapshot prefetched; marked ready",
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        image_uri = %image.image_uri,
                        digest = image.manifest_digest.as_str(),
                        error = %e,
                        "image prefetch failed; will retry on next reconcile",
                    );
                }
            }
        });
    }
}

/// Warm an enabled image's base-snapshot working set on local NVMe so the
/// first session restoring it pages in from tier-1, not GCS. We prefetch the
/// base snapshot's disk + memory manifests ONLY — never the source OCI image.
///
/// Session create restores from the per-image base snapshot (ADR 0020), whose
/// disk manifest is a superset of the image rootfs (it carries the
/// template-boot writes the image manifest lacks), so the OCI image is never
/// read at session time. Both manifests are always present (the
/// `enabled_images` columns are NOT NULL, migrations 0042/0043). Returns the
/// total chunk count on success.
async fn prefetch_one(
    image: &EnabledImageRef,
    chunk_store: &ChunkStore,
    chunk_cache: &ChunkCache,
    semaphore: &Arc<Semaphore>,
    // ADR 0022 Option A: when `Some`, after warming the memory chunks,
    // assemble them into the contiguous per-template base memfile at this
    // path so same-template base `session.create` siblings MAP_PRIVATE one
    // resident, page-cache-warm inode (density + faster boot, no UFFD
    // handler). `Some` only for FC images with a memory manifest; gating
    // readiness on this means an image isn't "ready" until the shared
    // memfile exists, so the first session restores against a warm file.
    base_memfile: Option<PathBuf>,
) -> Result<usize, PrefetchError> {
    // (1) ADR 0021 P2 — the base snapshot's rootfs. The session restores from
    // the per-image base snapshot, whose disk manifest carries the runtime
    // files written at template-boot (Bun / node_modules / claude). Warming
    // these on NVMe keeps the resuming guest's rootfs page-in off GCS — the
    // serial 16 MiB-chunk fetches during `resume` were the measured substrate
    // cost.
    let disk_manifest: Manifest = chunk_store
        .get_manifest(image.base_snapshot_disk_manifest)
        .await
        .map_err(|e| PrefetchError::ManifestLoad(format!("base snapshot disk: {e}")))?;
    let mut total =
        prefetch_manifest_chunks(disk_manifest, chunk_store, chunk_cache, semaphore).await?;

    // (2) ADR 0021 P2 (memory residency) — the base snapshot's memory image.
    // The UFFD handler pages these chunks in when the guest resumes; warming
    // them on NVMe retires the cold per-restore memory prefetch that was
    // ~2.84 s on a freshly-rolled host. Folding both into readiness means an
    // image isn't "warm" until BOTH its disk and memory working sets are
    // resident — so a host serves its first session warm, not just its second.
    //
    // `None` for cold-boot backends (VZ, migration 0049): a disk-only base
    // snapshot has no memory image to warm. Disk residency above is the whole
    // working set — readiness folds in only what exists.
    if let Some(memory_ref) = image.base_snapshot_memory_manifest {
        let memory_manifest: Manifest = chunk_store
            .get_manifest(memory_ref)
            .await
            .map_err(|e| PrefetchError::ManifestLoad(format!("base snapshot memory: {e}")))?;
        total +=
            prefetch_manifest_chunks(memory_manifest.clone(), chunk_store, chunk_cache, semaphore)
                .await?;

        // ADR 0022 Option A: materialize the contiguous per-template base
        // memfile (density + faster boot). The chunks are now NVMe-warm
        // from the prefetch above, so this is a local read + sequential
        // write — no GCS round-trip. Idempotent: skip if the file already
        // exists (a prior tick, or a base session.create that raced us and
        // materialized it itself — both write byte-identical content to the
        // same snapshot-id-keyed path, so siblings still share one inode).
        if let Some(dest) = base_memfile {
            if tokio::fs::metadata(&dest).await.is_err() {
                chunk_store
                    .materialize_to_file_cached(&memory_manifest, &dest, chunk_cache)
                    .await
                    .map_err(|e| {
                        PrefetchError::MemfileMaterialize(format!("{}: {e}", dest.display()))
                    })?;
                tracing::info!(
                    image_uri = %image.image_uri,
                    path = %dest.display(),
                    "per-template base memfile materialized at residency",
                );
            }
        }
    }
    // VZ / cold-boot images (base_snapshot_memory_manifest == None) have no
    // memory image — density is FC-only — so no memfile is built and
    // readiness folds in only the disk working set, exactly as before.

    Ok(total)
}

/// Pull every chunk of one chunked manifest through the tiered
/// resolver into the local NVMe cache, bounded by `semaphore`.
/// Returns the chunk count. Shared by the base-snapshot disk + memory
/// prefetch paths.
async fn prefetch_manifest_chunks(
    manifest: Manifest,
    chunk_store: &ChunkStore,
    chunk_cache: &ChunkCache,
    semaphore: &Arc<Semaphore>,
) -> Result<usize, PrefetchError> {
    let total = manifest.chunks.len();
    let mut handles = Vec::with_capacity(total);
    for chunk in manifest.chunks.into_iter() {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| PrefetchError::SemaphoreClosed)?;
        let chunk_store = chunk_store.clone();
        let chunk_cache = chunk_cache.clone();
        let hash = chunk.hash;
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            // Singleflight'd through ChunkCache::prefetch — concurrent
            // calls for the same chunk dedup, and a successful fetch
            // writes the bytes to the NVMe cache so the lazy-fault
            // path on session boot hits tier 1 instead of paying a
            // BlobStorage round-trip per page.
            chunk_cache
                .prefetch(hash, || async move { chunk_store.get_chunk(hash).await })
                .await
        }));
    }

    for h in handles {
        h.await
            .map_err(|e| PrefetchError::JoinError(format!("{e}")))?
            .map_err(|e| PrefetchError::ChunkFetch(format!("{e}")))?;
    }

    Ok(total)
}

#[derive(Debug)]
enum PrefetchError {
    ManifestLoad(String),
    ChunkFetch(String),
    SemaphoreClosed,
    JoinError(String),
    MemfileMaterialize(String),
}

impl std::fmt::Display for PrefetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestLoad(m) => write!(f, "load chunk manifest: {m}"),
            Self::ChunkFetch(m) => write!(f, "chunk fetch: {m}"),
            Self::SemaphoreClosed => write!(f, "prefetch semaphore closed"),
            Self::JoinError(m) => write!(f, "task join: {m}"),
            Self::MemfileMaterialize(m) => write!(f, "materialize base memfile: {m}"),
        }
    }
}

impl std::error::Error for PrefetchError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_snapshot_round_trips() {
        let r = ImageReadiness::new();
        let d = ManifestDigest::new("sha256:abc");
        assert!(!r.contains(&d));
        r.mark_ready(d.clone());
        assert!(r.contains(&d));
        let snap = r.snapshot();
        assert_eq!(snap, vec![d.clone()]);
        r.mark_unready(&d);
        assert!(!r.contains(&d));
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn concurrency_env_override_clamps_to_default_on_garbage() {
        // SAFETY: tests are single-threaded; setting an env we
        // immediately consume.
        unsafe { std::env::set_var("ENGRAM_PREFETCH_CONCURRENCY", "not-a-number") };
        assert_eq!(concurrency_from_env(), DEFAULT_PREFETCH_CONCURRENCY);
        unsafe { std::env::set_var("ENGRAM_PREFETCH_CONCURRENCY", "0") };
        assert_eq!(concurrency_from_env(), DEFAULT_PREFETCH_CONCURRENCY);
        unsafe { std::env::set_var("ENGRAM_PREFETCH_CONCURRENCY", "32") };
        assert_eq!(concurrency_from_env(), 32);
        unsafe { std::env::remove_var("ENGRAM_PREFETCH_CONCURRENCY") };
    }

    // ---- ADR 0022 Option A: residency memfile materialization ----

    use engram_chunk_store::{ChunkCacheConfig, ManifestKind};
    use engram_core::traits::BlobStorage;
    use engram_core::types::manifest::ManifestRef;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc as StdArc;

    /// Build a real local-backed ChunkStore + ChunkCache and seed a
    /// disk + memory manifest, returning everything `prefetch_one` needs.
    async fn seed() -> (
        ChunkStore,
        ChunkCache,
        tempfile::TempDir,
        ManifestRef,
        ManifestRef,
        Vec<u8>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let blob: StdArc<dyn BlobStorage> =
            StdArc::new(LocalBlobStorage::new(dir.path().join("blob")));
        let store = ChunkStore::new(blob);
        let cache = ChunkCache::new(ChunkCacheConfig {
            root: dir.path().join("cache"),
            budget_bytes: 256 * 1024 * 1024,
        });
        // Distinct disk + memory payloads so a mix-up would be caught.
        let disk_bytes = (0..37u8).cycle().take(2 * 1024 * 1024).collect::<Vec<_>>();
        let mem_bytes = (3..200u8).cycle().take(3 * 1024 * 1024).collect::<Vec<_>>();
        let disk_src = dir.path().join("disk.bin");
        let mem_src = dir.path().join("mem.bin");
        tokio::fs::write(&disk_src, &disk_bytes).await.unwrap();
        tokio::fs::write(&mem_src, &mem_bytes).await.unwrap();
        let disk_m = store
            .chunk_file(&disk_src, ManifestKind::Disk, Some(512 * 1024))
            .await
            .unwrap();
        let mem_m = store
            .chunk_file(&mem_src, ManifestKind::Memory, Some(512 * 1024))
            .await
            .unwrap();
        let disk_ref = ManifestRef {
            manifest_id: uuid::Uuid::new_v4(),
            version: 1,
        };
        let mem_ref = ManifestRef {
            manifest_id: uuid::Uuid::new_v4(),
            version: 1,
        };
        store.put_manifest(disk_ref, &disk_m).await.unwrap();
        store.put_manifest(mem_ref, &mem_m).await.unwrap();
        (store, cache, dir, disk_ref, mem_ref, mem_bytes)
    }

    fn image_ref(
        base_snapshot_id: SnapshotId,
        disk_ref: ManifestRef,
        mem_ref: Option<ManifestRef>,
    ) -> EnabledImageRef {
        EnabledImageRef {
            image_uri: "localhost:5001/demo:warm".into(),
            manifest_digest: ManifestDigest::new("sha256:deadbeef"),
            base_snapshot_id,
            base_snapshot_disk_manifest: disk_ref,
            base_snapshot_memory_manifest: mem_ref,
        }
    }

    #[tokio::test]
    async fn prefetch_materializes_base_memfile_and_is_idempotent() {
        let (store, cache, dir, disk_ref, mem_ref, mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let base_id = SnapshotId::new();
        let dest = dir
            .path()
            .join("snapshots")
            .join(base_id.to_string())
            .join("memory.bin");

        let img = image_ref(base_id, disk_ref, Some(mem_ref));
        prefetch_one(&img, &store, &cache, &sem, Some(dest.clone()))
            .await
            .unwrap();

        // The contiguous memfile materialized byte-faithfully at the path
        // a base session.create restore reads — this is the shared inode.
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), mem_bytes);

        // Idempotent: a second residency pass (or a racing first-create
        // that already wrote it) is a no-op, not an error or a rewrite.
        let before = tokio::fs::metadata(&dest)
            .await
            .unwrap()
            .modified()
            .unwrap();
        prefetch_one(&img, &store, &cache, &sem, Some(dest.clone()))
            .await
            .unwrap();
        let after = tokio::fs::metadata(&dest)
            .await
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            before, after,
            "second prefetch must not rewrite the memfile"
        );
    }

    #[tokio::test]
    async fn prefetch_skips_memfile_for_disk_only_image() {
        // VZ / cold-boot: base_snapshot_memory_manifest == None. Even with a
        // dest path supplied, no memfile is built — density is FC-only.
        let (store, cache, dir, disk_ref, _mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let base_id = SnapshotId::new();
        let dest = dir
            .path()
            .join("snapshots")
            .join(base_id.to_string())
            .join("memory.bin");

        let img = image_ref(base_id, disk_ref, None);
        prefetch_one(&img, &store, &cache, &sem, Some(dest.clone()))
            .await
            .unwrap();
        assert!(
            tokio::fs::metadata(&dest).await.is_err(),
            "disk-only image must not materialize a memfile",
        );
    }
}
