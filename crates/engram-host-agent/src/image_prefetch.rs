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

use std::collections::HashSet;
use std::sync::Arc;

use engram_chunk_store::{ChunkCache, ChunkStore, Manifest};
use engram_protocol::heartbeat::{EnabledImageRef, ManifestDigest};
use parking_lot::RwLock;
use tokio::sync::{watch, Semaphore};

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
        "image prefetch supervisor starting (base-snapshot only; no OCI prefetch)",
    );
    let handle = tokio::spawn(async move {
        loop {
            let enabled = rx.borrow_and_update().clone();
            reconcile(
                &enabled,
                readiness.clone(),
                chunk_store.clone(),
                chunk_cache.clone(),
                semaphore.clone(),
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
        // Clone the whole ref into the task — it carries everything
        // prefetch_one needs (uri, digest, base-snapshot disk + memory
        // manifests). Two Strings + two Copy refs; cheap per reconcile.
        let image = image.clone();
        let readiness = readiness.clone();
        let chunk_store = chunk_store.clone();
        let chunk_cache = chunk_cache.clone();
        let semaphore = semaphore.clone();
        tokio::spawn(async move {
            match prefetch_one(&image, &chunk_store, &chunk_cache, &semaphore).await {
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
    let memory_manifest: Manifest = chunk_store
        .get_manifest(image.base_snapshot_memory_manifest)
        .await
        .map_err(|e| PrefetchError::ManifestLoad(format!("base snapshot memory: {e}")))?;
    total +=
        prefetch_manifest_chunks(memory_manifest, chunk_store, chunk_cache, semaphore).await?;

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
}

impl std::fmt::Display for PrefetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestLoad(m) => write!(f, "load chunk manifest: {m}"),
            Self::ChunkFetch(m) => write!(f, "chunk fetch: {m}"),
            Self::SemaphoreClosed => write!(f, "prefetch semaphore closed"),
            Self::JoinError(m) => write!(f, "task join: {m}"),
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
}
