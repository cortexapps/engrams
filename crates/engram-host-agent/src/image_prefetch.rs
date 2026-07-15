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
//! Storage tier model (for the per-chunk `get_chunk` walk on the supervisor's
//! `ChunkStore`, which uses the default `BlobStorageResolver`):
//!   - Tier 1: local NVMe `ChunkCache` (canonical "ready" inventory; a hit
//!     here = chunk is local + counts toward readiness).
//!   - Tier 2: `BlobStorage` at `chunks/sha256/<hex>` (GCS) — where the base
//!     snapshot's chunks are flushed at enable. This is the hot tier here.
//!
//! (The OCI-fallback `TieredChunkResolver` is installed only on the
//! host-agent's per-restore chunked-OCI store, not on this supervisor's
//! store.) `chunk_store.get_chunk(hash)` resolves NVMe → BlobStorage, so the
//! prefetch driver is just an eager loop over the base snapshot's chunk hashes.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use engram_chunk_store::manifest::ChunkHash;
use engram_chunk_store::{ChunkCache, ChunkStore, Manifest};
use engram_core::types::SnapshotId;
use engram_protocol::heartbeat::{EnabledImageRef, ManifestDigest};
use parking_lot::{Mutex, RwLock};
use tokio::sync::{watch, Semaphore};

/// ADR 0039 (cache locality): per-enabled-image record of the canonical
/// base-manifest (disk + memory) chunk hashes this host has pinned in the
/// `ChunkCache`, so the LRU can never evict the shared base out from under
/// live File-backend siblings. Keyed by manifest digest; the `Vec` is the
/// pin batch we hand back to `ChunkCache::unpin_all` when the image is
/// disabled. Shared because pinning happens inside spawned per-image
/// prefetch tasks while unpinning happens in the supervisor's reconcile
/// loop. Pins are refcounted in the cache, so two enabled images sharing a
/// base chunk both pin it and one disable leaves it pinned for the other.
type PinnedManifests = Arc<Mutex<HashMap<ManifestDigest, Vec<ChunkHash>>>>;

/// ADR 0045 addendum (2026-07-10): per-enabled-image record of the
/// substrate base shm file THIS host's readiness claim rests on. An image lands here when its prefetch found the base file present
/// (pre-warmed by us, or surviving a pod roll on the node tmpfs) — and the
/// reconcile recheck then treats the file's disappearance exactly like an
/// evicted base chunk: flip to not-ready and re-prefetch (which re-warms
/// the file, since it's now absent). Images whose pre-warm was skipped
/// (tmpfs headroom) or failed are deliberately NOT tracked: for them the
/// handler's lazy path is the contract, and gating readiness on a file
/// we chose not to write would wedge the image unready forever.
type TrackedBaseShm = Arc<Mutex<HashMap<ManifestDigest, PathBuf>>>;

/// ADR 0092: digests whose base-shm pre-warm was headroom-skipped and is
/// owed a retry on the next recheck tick (the skip used to be sticky —
/// nothing re-attempted the write when tmpfs freed).
type HeadroomSkipped = Arc<Mutex<HashSet<ManifestDigest>>>;

/// ADR 0022 Option A: resolves a base snapshot's id to its on-disk
/// snapshot dir (`<work_dir>/snapshots/<id>`). Supplied by the
/// host-agent as `pooled.snapshot_path_for` so the residency-materialized
/// memfile lands at the *exact* path a base `session.create` restore
/// reads — they agree by construction rather than by duplicated layout
/// logic. `Some` ⇒ density on (materialize the per-template memfile +
/// gate readiness on it); `None` ⇒ off (behaviour-preserving).
pub type SnapshotDirResolver = Arc<dyn Fn(SnapshotId) -> PathBuf + Send + Sync>;

/// ADR 0022: a per-template base memfile we're tracking for residency —
/// its on-disk path (for reclaim on image-disable) plus, once
/// materialized, an [`MemfilePin`] keeping its pages resident in the page
/// cache so File-backend siblings always hit a warm shared base.
struct MemfileState {
    path: PathBuf,
    /// `Some` once the memfile is materialized AND successfully pinned.
    /// Stays `None` if pinning is off (non-Linux) or best-effort-fails.
    pin: Option<MemfilePin>,
}

/// ADR 0022 cold-restore mitigation: a `mmap(MAP_SHARED, PROT_READ)` +
/// `mlock` of a per-template base memfile, pinning its page-cache pages
/// resident. File-backend siblings `MAP_PRIVATE` the same inode, so this
/// guarantees their clean base pages can't be LRU-evicted under memory
/// pressure — which is what turns a warm ~0.6 s restore into a cold
/// multi-GiB-read ~4.7 s one (measured, ADR 0022 prod canary). `Drop`
/// `munmap`s, which also releases the `mlock`.
#[cfg(target_os = "linux")]
struct MemfilePin {
    addr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is read-only and owned solely by this guard; the raw
// pointer is only ever handed back to `munmap` on drop. No aliasing.
#[cfg(target_os = "linux")]
unsafe impl Send for MemfilePin {}

#[cfg(target_os = "linux")]
impl Drop for MemfilePin {
    fn drop(&mut self) {
        // SAFETY: addr/len came from the successful `mmap` in
        // `pin_memfile`; `munmap` releases both the mapping and its mlock.
        unsafe {
            libc::munmap(self.addr, self.len);
        }
    }
}

/// `mmap`+`mlock` `path` resident. Best-effort: any failure (mmap, or
/// `mlock` hitting `RLIMIT_MEMLOCK`/`ENOMEM`) logs a warning and returns
/// `None`, leaving the memfile evictable — i.e. the pre-mitigation
/// behaviour, never a hard failure.
#[cfg(target_os = "linux")]
fn pin_memfile(path: &std::path::Path) -> Option<MemfilePin> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }
    // SAFETY: a standard read-only file mapping; result checked vs MAP_FAILED.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        tracing::warn!(path = %path.display(), error = %std::io::Error::last_os_error(),
            "ADR 0022: mmap for base-memfile pin failed; memfile stays evictable");
        return None;
    }
    // SAFETY: addr/len from the successful mmap above.
    if unsafe { libc::mlock(addr, len) } != 0 {
        tracing::warn!(path = %path.display(), error = %std::io::Error::last_os_error(),
            "ADR 0022: mlock for base-memfile pin failed (RLIMIT_MEMLOCK?); memfile stays evictable");
        // SAFETY: same mapping; release it since we won't return a guard.
        unsafe {
            libc::munmap(addr, len);
        }
        return None;
    }
    tracing::info!(path = %path.display(), bytes = len,
        "ADR 0022: base memfile pinned resident (mlock) — File restores stay warm");
    Some(MemfilePin { addr, len })
}

#[cfg(not(target_os = "linux"))]
struct MemfilePin;

#[cfg(not(target_os = "linux"))]
fn pin_memfile(_path: &std::path::Path) -> Option<MemfilePin> {
    None
}

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

    /// `pub` for the peer-fill integration tests (which stand up a
    /// serving host without a live supervisor); in production the
    /// prefetch supervisor is the sole writer.
    pub fn mark_ready(&self, digest: ManifestDigest) {
        self.inner.write().insert(digest);
    }

    fn mark_unready(&self, digest: &ManifestDigest) {
        self.inner.write().remove(digest);
    }
}

/// ADR 0036 amendment (issue #538): the deduped union of `enabled_images`
/// and `prestage_images` from a heartbeat ack — what the supervisor
/// actually watches (fed into `enabled_images_tx`). The supervisor itself
/// needs zero changes: from its point of view a prestaging image is just
/// another enabled image to warm, pin, and report ready; the coordinator's
/// enable scanner is the one reading `ready_images` back out during the
/// wait. Pure so it's unit-testable without a live watch channel — dedup by
/// `manifest_digest`, `enabled` entries winning ties (their manifests are
/// authoritative once an image is live; identical digests carry identical
/// manifests anyway, so which copy wins is never observable in practice).
pub fn union_image_refs(
    enabled: &[EnabledImageRef],
    prestaging: &[EnabledImageRef],
) -> Vec<EnabledImageRef> {
    let mut seen: HashSet<ManifestDigest> =
        HashSet::with_capacity(enabled.len() + prestaging.len());
    let mut out = Vec::with_capacity(enabled.len() + prestaging.len());
    for r in enabled.iter().chain(prestaging.iter()) {
        if seen.insert(r.manifest_digest.clone()) {
            out.push(r.clone());
        }
    }
    out
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

/// ADR 0092: population policy for the tmpfs (resume) base-shm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BaseShmMode {
    /// Eagerly pre-warm the full non-hole base at image residency
    /// (today's behavior; pins the whole base in tmpfs).
    Full,
    /// Never pre-warm: the uffd handler creates/sizes the base and
    /// populates canonical pages on first fault from the NVMe cache, so
    /// tmpfs holds only what resumed sessions actually touch. Fresh
    /// creates should pair this with `ENGRAM_FC_FRESH_RESTORE_MODE=file`
    /// (the memfile path) so they don't repopulate it either.
    Lazy,
}

/// `ENGRAM_FC_BASE_SHM_MODE` ∈ {`full` (default), `lazy`}, read once at
/// startup (mirrors `restore_mode_from_env`'s parse/warn shape).
fn base_shm_mode_from_env() -> BaseShmMode {
    static MODE: std::sync::OnceLock<BaseShmMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("ENGRAM_FC_BASE_SHM_MODE") {
        Ok(s) if s.eq_ignore_ascii_case("lazy") => BaseShmMode::Lazy,
        Ok(s) if !s.trim().is_empty() && !s.eq_ignore_ascii_case("full") => {
            tracing::warn!(value = %s, "unrecognised ENGRAM_FC_BASE_SHM_MODE; defaulting to full");
            BaseShmMode::Full
        }
        _ => BaseShmMode::Full,
    })
}

/// ADR 0092: `ENGRAM_FC_BASE_MEMFILE_PIN` ∈ {`1` (default), `0`/`off`} —
/// gates the memfile mlock. Unpinned, the base's residency is ordinary
/// reclaimable page cache: the kernel evicts cold base pages under
/// pressure and re-faults them from NVMe (~4.7 s worst case, measured)
/// instead of the host OOMing — the density configuration the ADR 0092
/// canary runs. Read once at startup.
fn memfile_pin_enabled() -> bool {
    static PIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PIN.get_or_init(|| {
        !matches!(
            std::env::var("ENGRAM_FC_BASE_MEMFILE_PIN"),
            Ok(s) if s == "0" || s.eq_ignore_ascii_case("off")
        )
    })
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
    // Issue #540: register the base-shm prewarm's expected byte charge
    // against the host RAM ledger BEFORE writing, so the very next
    // heartbeat tick's `allocatable_mib` reflects it — instead of minutes
    // later when the multi-GiB write finishes.
    ram_ledger: Arc<crate::ram_ledger::RamLedger>,
    // ADR 0045 addendum (2026-07-10): the base-shm sweeper's enabled-image
    // keep-set. Published (whole-set replace) from every heartbeat-ack
    // reconcile so the GC can never sweep an enabled image's base file;
    // left untouched on the pre-ack initial tick so the sweeper keeps
    // deferring deletes until the enabled view is authoritative.
    protected: Arc<crate::base_shm_gc::ProtectedPaths>,
) -> (
    watch::Sender<Vec<EnabledImageRef>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = watch::channel(Vec::<EnabledImageRef>::new());
    let permits = concurrency_from_env();
    let semaphore = Arc::new(Semaphore::new(permits));
    // ADR 0095: requester-side peer health cache — shared across every
    // prefetch task so one dead seed is skipped fleet-wide on this host
    // for the lost-window instead of once per image.
    let peer_health = crate::peer_fill::PeerHealth::new();
    tracing::info!(
        permits,
        recheck_secs = RECHECK_INTERVAL.as_secs(),
        base_memfile = base_memfile_dir.is_some(),
        "image prefetch supervisor starting (base-snapshot only; no OCI prefetch)",
    );
    let handle = tokio::spawn(async move {
        // ADR 0022: digest → per-template base memfile (path for reclaim on
        // disable + the mlock pin once materialized). Lives across ticks.
        // Only populated when `base_memfile_dir` is `Some`.
        let mut memfiles: HashMap<ManifestDigest, MemfileState> = HashMap::new();
        // ADR 0039: digest → the base-manifest chunk hashes pinned in the
        // cache, so a later disable can unpin exactly that batch. Lives
        // across ticks; shared into the per-image prefetch tasks (which
        // pin) and read by reconcile's disable loop (which unpins).
        let pinned_manifests: PinnedManifests = Arc::new(Mutex::new(HashMap::new()));
        // ADR 0045 addendum: digest → the base shm file readiness rests on.
        let tracked_base_shm: TrackedBaseShm = Arc::new(Mutex::new(HashMap::new()));
        // ADR 0092: headroom-skipped pre-warms owed a retry (see recheck).
        let headroom_skipped: HeadroomSkipped = Arc::new(Mutex::new(HashSet::new()));
        tracing::info!(
            base_shm_mode = ?base_shm_mode_from_env(),
            memfile_pin = memfile_pin_enabled(),
            "image prefetch: ADR 0092 residency policy",
        );
        // The watch channel's initial value is an empty Vec, NOT a
        // heartbeat ack — only iterations after the first `changed()`
        // carry an authoritative enabled set the GC may act on.
        let mut saw_ack = false;
        loop {
            let enabled = rx.borrow_and_update().clone();
            if saw_ack {
                protected.replace(enabled_base_shm_paths(&enabled));
            }
            reconcile(
                &enabled,
                readiness.clone(),
                chunk_store.clone(),
                chunk_cache.clone(),
                semaphore.clone(),
                peer_health.clone(),
                base_memfile_dir.as_ref(),
                &mut memfiles,
                pinned_manifests.clone(),
                ram_ledger.clone(),
                tracked_base_shm.clone(),
                headroom_skipped.clone(),
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
                    saw_ack = true;
                }
                _ = tokio::time::sleep(RECHECK_INTERVAL) => {}
            }
        }
    });
    (tx, handle)
}

/// The substrate base-file paths the enabled set claims — the base-shm
/// sweeper's keep-set. Empty when the substrate is off (no base dir) or
/// no enabled image carries a memory manifest.
fn enabled_base_shm_paths(enabled: &[EnabledImageRef]) -> HashSet<PathBuf> {
    let Some(base_dir) = engram_sandbox_firecracker::uffd_base_dir_from_env() else {
        return HashSet::new();
    };
    enabled
        .iter()
        .filter_map(|img| img.base_snapshot_memory_manifest.as_ref())
        .map(|mref| engram_sandbox_firecracker::uffd_base_path_in(&base_dir, mref))
        .collect()
}

/// One reconciliation pass: compute the ready/enabled delta, drop
/// disabled-but-still-ready digests, kick off prefetch for any
/// enabled-but-not-yet-ready digest.
#[allow(clippy::too_many_arguments)]
async fn reconcile(
    enabled: &[EnabledImageRef],
    readiness: Arc<ImageReadiness>,
    chunk_store: ChunkStore,
    chunk_cache: ChunkCache,
    semaphore: Arc<Semaphore>,
    peer_health: Arc<crate::peer_fill::PeerHealth>,
    base_memfile_dir: Option<&SnapshotDirResolver>,
    memfiles: &mut HashMap<ManifestDigest, MemfileState>,
    pinned_manifests: PinnedManifests,
    ram_ledger: Arc<crate::ram_ledger::RamLedger>,
    tracked_base_shm: TrackedBaseShm,
    headroom_skipped: HeadroomSkipped,
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
        // ADR 0039: release this image's base-manifest pins so its
        // canonical chunks become LRU-evictable again — but only those
        // not still pinned by another enabled image (refcounted in the
        // cache). The chunks themselves stay on NVMe; only the never-evict
        // guard is dropped.
        let unpin_batch = pinned_manifests.lock().remove(digest);
        if let Some(hashes) = unpin_batch {
            let n = hashes.len();
            chunk_cache.unpin_all(hashes);
            tracing::info!(
                digest = digest.as_str(),
                chunks = n,
                "image disabled; unpinned base-manifest chunks",
            );
        }
        // ADR 0022: reclaim the per-template base memfile (a
        // guest-RAM-sized file) when its image is disabled. Best-effort
        // unlink; a live sharer keeps the inode alive via its MAP_PRIVATE
        // mapping even after the dentry is gone, so this is safe to do
        // while sessions are still running.
        if let Some(state) = memfiles.remove(digest) {
            // Dropping `state` releases the mlock pin (munmap) before we
            // unlink. A live sharer keeps the inode alive via its
            // MAP_PRIVATE mapping even after both the unlink and our
            // munmap, so this is safe while sessions are still running.
            let p = state.path.clone();
            drop(state);
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
        // The base shm file itself is left for the sweeper: dropping the
        // digest here removes it from the tracked set, and the supervisor's
        // next keep-set publish drops its path — after which the normal
        // mtime + open-FD sweep rules reclaim it.
        tracked_base_shm.lock().remove(digest);
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
            // ADR 0039 (honest readiness): a `ready` image must still have
            // ALL its base chunks locally resolvable. Re-verify against the
            // pinned set (the disk + memory hashes recorded at prefetch) —
            // if the LRU evicted any, or a `write_local` silently failed so a
            // pinned hash never actually landed on NVMe, flip to not-ready so
            // this round re-prefetches. Without this the host keeps
            // advertising `ready` while a restore cold-fetches (or EIOs) the
            // base — the dev-engrams base-restore incident. (Cheap: a
            // `try_exists` per pinned hash; the pin set is hundreds of
            // entries.)
            let still_warm = {
                let guard = pinned_manifests.lock();
                guard
                    .get(&image.manifest_digest)
                    .map(|hashes| hashes.iter().all(|h| chunk_cache.contains_on_disk(*h)))
                    .unwrap_or(false)
            };
            // ADR 0045 addendum: the same honesty for the substrate base shm
            // file — if readiness rests on it (tracked) and it's gone (a
            // sweep, a tmpfs remount, an operator rm), this host would serve
            // the next session's entire resume storm through lazy population.
            // Treat exactly like an evicted chunk: not-ready + re-prefetch
            // (the file is absent, so the pre-warm arm re-creates it).
            let base_shm_intact = tracked_base_shm
                .lock()
                .get(&image.manifest_digest)
                .is_none_or(|p| p.exists());
            // ADR 0092: the same honesty for the File-mode base memfile —
            // an out-of-band rm (even under a live mlock, which pins the
            // unlinked inode invisibly) must re-materialize, not leave
            // fresh creates opening a ghost path.
            let memfile_path = memfiles.get(&image.manifest_digest).map(|s| s.path.clone());
            let memfile_intact = match &memfile_path {
                Some(p) => tokio::fs::try_exists(p).await.unwrap_or(true),
                None => true,
            };
            // ADR 0092: a headroom-skipped pre-warm retries on every
            // recheck tick until it lands.
            let headroom_retry = headroom_skipped.lock().contains(&image.manifest_digest);
            if still_warm && base_shm_intact && memfile_intact && !headroom_retry {
                continue;
            }
            if still_warm && base_shm_intact && memfile_intact {
                // Retry-only pass: readiness is NOT flipped — sessions keep
                // flowing on the handler's lazy backstop while we re-run
                // `prefetch_one`, whose pre-warm arm re-checks headroom and
                // is exists-idempotent.
                tracing::info!(
                    image_uri = %image.image_uri,
                    digest = image.manifest_digest.as_str(),
                    "retrying headroom-skipped base shm pre-warm",
                );
            } else {
                if !memfile_intact {
                    tracing::warn!(
                        image_uri = %image.image_uri,
                        digest = image.manifest_digest.as_str(),
                        "ready image's base memfile vanished; flipping to not-ready to re-materialize",
                    );
                    // Drop the stale entry — its pin (if any) holds the
                    // unlinked inode; the re-prefetch re-materializes and
                    // the pin loop re-pins the new file.
                    memfiles.remove(&image.manifest_digest);
                } else if base_shm_intact {
                    tracing::warn!(
                        image_uri = %image.image_uri,
                        digest = image.manifest_digest.as_str(),
                        "ready image's base chunks no longer resolvable locally; flipping to not-ready for re-prefetch",
                    );
                } else {
                    tracing::warn!(
                        image_uri = %image.image_uri,
                        digest = image.manifest_digest.as_str(),
                        "ready image's base shm file vanished; flipping to not-ready to re-warm it",
                    );
                }
                readiness.mark_unready(&image.manifest_digest);
            }
            // fall through to re-prefetch (prefetch_one re-warms the chunks;
            // the pin set is already recorded so it skips re-pinning).
        }
        // ADR 0022: where this image's contiguous per-template base
        // memfile must land — the SAME path a base session.create restore
        // reads (`snapshot_path_for(base_snapshot_id)/memory.bin`). `None`
        // when density is off. Recorded for disable-time reclaim.
        let base_memfile =
            base_memfile_dir.map(|resolver| resolver(image.base_snapshot_id).join("memory.bin"));
        if let Some(ref path) = base_memfile {
            memfiles
                .entry(image.manifest_digest.clone())
                .or_insert_with(|| MemfileState {
                    path: path.clone(),
                    pin: None,
                });
        }
        // Clone the whole ref into the task — it carries everything
        // prefetch_one needs (uri, digest, base-snapshot disk + memory
        // manifests). Two Strings + two Copy refs; cheap per reconcile.
        let image = image.clone();
        let readiness = readiness.clone();
        let chunk_store = chunk_store.clone();
        let chunk_cache = chunk_cache.clone();
        let semaphore = semaphore.clone();
        let peer_health = peer_health.clone();
        let pinned_manifests = pinned_manifests.clone();
        let ram_ledger = ram_ledger.clone();
        let tracked_base_shm = tracked_base_shm.clone();
        let headroom_skipped = headroom_skipped.clone();
        tokio::spawn(async move {
            match prefetch_one(
                &image,
                &chunk_store,
                &chunk_cache,
                &semaphore,
                &peer_health,
                base_memfile,
                &ram_ledger,
            )
            .await
            {
                Ok(warmed) => {
                    // ADR 0039: pin the canonical base manifest (disk +
                    // memory) so the LRU never evicts the shared base while
                    // this image is enabled. Pins are refcounted; record the
                    // batch so the disable path unpins exactly it. Skip if a
                    // prior tick already pinned this digest (the prefetch is
                    // idempotent but we must not double-pin and leave a
                    // dangling refcount on disable).
                    let mut guard = pinned_manifests.lock();
                    if !guard.contains_key(&image.manifest_digest) {
                        chunk_cache.pin_all(warmed.hashes.iter().copied());
                        guard.insert(image.manifest_digest.clone(), warmed.hashes);
                    }
                    drop(guard);
                    // Record (or clear) the base shm file this readiness
                    // claim rests on. Overwrite semantics matter: a
                    // pre-warm that fit last round but was headroom-skipped
                    // this round must DROP the stale tracking entry, or the
                    // recheck would flip the image unready forever over a
                    // file nothing is going to write.
                    match warmed.base_shm {
                        Some(path) => {
                            tracked_base_shm
                                .lock()
                                .insert(image.manifest_digest.clone(), path);
                        }
                        None => {
                            tracked_base_shm.lock().remove(&image.manifest_digest);
                        }
                    }
                    // ADR 0092: record (or clear) the retry debt. Same
                    // overwrite semantics as the tracking entry above.
                    if warmed.base_shm_headroom_skipped {
                        headroom_skipped
                            .lock()
                            .insert(image.manifest_digest.clone());
                    } else {
                        headroom_skipped.lock().remove(&image.manifest_digest);
                    }
                    readiness.mark_ready(image.manifest_digest.clone());
                    tracing::info!(
                        image_uri = %image.image_uri,
                        digest = image.manifest_digest.as_str(),
                        chunks = warmed.chunk_count,
                        "image base snapshot prefetched + pinned; marked ready",
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

    // ADR 0022 cold-restore mitigation: pin each materialized base memfile
    // resident (mlock) so File-backend siblings never pay a cold multi-GiB
    // read under page-cache pressure. Runs every tick (including the
    // LRU-recheck) so it catches a memfile the prefetch above just wrote
    // and pins it once it exists — a tick after materialization, which is
    // fine (it's warm from the write; pinning guards against *later*
    // eviction). Best-effort + idempotent (skips already-pinned entries);
    // a no-op when density is off (`memfiles` stays empty). `spawn_blocking`
    // keeps a multi-GiB `mlock` off the supervisor's async reactor.
    for image in enabled {
        let digest = &image.manifest_digest;
        let path = match memfiles.get(digest) {
            Some(state) if state.pin.is_none() => state.path.clone(),
            _ => continue, // untracked (density off) or already pinned
        };
        if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
            continue; // not materialized yet; a later tick will pin it
        }
        let p = path.clone();
        // ADR 0092: `ENGRAM_FC_BASE_MEMFILE_PIN=0` leaves the memfile's
        // residency to the kernel (reclaimable page cache) — no mlock.
        let pin = if memfile_pin_enabled() {
            tokio::task::spawn_blocking(move || pin_memfile(&p))
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        if pin.is_some() {
            if let Some(state) = memfiles.get_mut(digest) {
                state.pin = pin;
            }
        }
    }

    // ADR 0070: gauge summed on-disk bytes of every currently-tracked
    // base memfile — disk the chunk sweeper can't evict (reclaimed only
    // on image-disable; mlock'd too when the pin is on), part of the
    // same "floor the budget can't touch" accounting as pinned chunk
    // bytes (see engram-chunk-store's engram_chunk_cache_pinned_bytes).
    // Allocated bytes (st_blocks), not apparent length: the memfiles are
    // sparse (dev-brain: 24 GiB apparent, ~21 GiB non-hole) and both the
    // gauge and the reserve below account real disk consumption. Runs
    // every tick, including the LRU-recheck; reads 0 when density is off
    // (`memfiles` stays empty). A metadata() failure (not yet
    // materialized, or racing the disable reclaim above) just skips that
    // entry — best-effort, same tolerance as the pin loop above.
    //
    // Collect owned paths FIRST, then await: `MemfileState::pin` holds a
    // raw `*mut libc::c_void` (only `unsafe impl Send`, never `Sync`), so
    // holding `memfiles.values()`'s borrow across an `.await` makes this
    // whole async fn's future non-Send — invisible on macOS (no UFFD, no
    // MemfilePin materializes there) but a hard compile error on the
    // Linux target `spawn_supervisor` actually runs on.
    let memfile_paths: Vec<std::path::PathBuf> =
        memfiles.values().map(|state| state.path.clone()).collect();
    let mut memfile_bytes: u64 = 0;
    for path in memfile_paths {
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            memfile_bytes += allocated_bytes(&meta);
        }
    }
    ::metrics::gauge!(crate::metrics::HOST_BASE_MEMFILE_BYTES).set(memfile_bytes as f64);
    // ADR 0092: the memfiles live on the same filesystem as the chunk
    // cache (both under the work dir) and its sweeper can't evict them —
    // reserve their allocated bytes out of the cache's ceiling so the
    // cache yields the space instead of racing the memfiles to the
    // kubelet eviction line (the 2026-07-14 w8wq DiskPressure incident:
    // a warm at-budget cache + 40 GB of unbudgeted memfiles).
    chunk_cache.set_co_tenant_reserved(memfile_bytes);
}

/// Allocated (on-disk) bytes of a file: `st_blocks × 512` on unix, so a
/// sparse memfile reserves what it actually consumes; apparent length
/// elsewhere (VZ/macOS dev, where the memfile path is never taken).
fn allocated_bytes(meta: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

/// The outcome of [`prefetch_one`]: the total chunk count warmed (for
/// logging) plus the full set of base-manifest chunk hashes (disk +
/// memory) the supervisor pins against eviction (ADR 0039). The hashes
/// are deduped — a chunk shared between the disk and memory manifests is
/// pinned once — so the recorded batch matches what `pin_all` actually
/// pinned and `unpin_all` will release.
struct WarmedManifest {
    chunk_count: usize,
    hashes: Vec<ChunkHash>,
    /// ADR 0045 addendum: the substrate base shm file this image's
    /// readiness rests on — present when the pre-warm arm left the file
    /// in place (freshly written, or already there from a prior pod's
    /// life). `None` when the substrate is off, the image is disk-only,
    /// or the pre-warm was headroom-skipped / failed (lazy-path images
    /// must not gate readiness on a file nothing will write).
    base_shm: Option<PathBuf>,
    /// ADR 0092: the pre-warm was wanted but skipped for tmpfs headroom —
    /// the supervisor records a retry debt so the recheck re-attempts it
    /// (the skip used to be sticky).
    base_shm_headroom_skipped: bool,
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
/// chunk count + the deduped base-manifest hash set for pinning.
/// ADR 0095: best-effort peer pre-pass. Bulk-pull a manifest's
/// locally-missing chunks from a coordinator-hinted warm sibling (LAN,
/// CRC32C landings) BEFORE the verifying per-chunk loop — which then
/// finds them resident and completes only the remainder through GCS.
/// Every failure mode degrades to that loop untouched:
/// - no hints / all peers health-cached lost ⇒ no dial, pure GCS;
/// - dial/stream failure ⇒ peer marked lost, try the one alternate
///   hint (bounded: ≤2 peers × ≤2 pulls);
/// - `RESOURCE_EXHAUSTED` backpressure ⇒ one jittered retry (the seed
///   was busy, not dead), then GCS;
/// - per-chunk `missing` ⇒ that chunk stays for the GCS loop.
async fn peer_prepass(
    image: &EnabledImageRef,
    manifest: &Manifest,
    chunk_cache: &ChunkCache,
    peer_health: &Arc<crate::peer_fill::PeerHealth>,
) {
    use engram_protocol::grpc_client::PeerChunkScope;
    if image.warm_peers.is_empty() {
        return;
    }
    let missing_set = |cache: &ChunkCache, manifest: &Manifest| {
        let cache = cache.clone();
        let hashes: Vec<ChunkHash> = manifest.chunks.iter().map(|c| c.hash).collect();
        async move {
            let mut missing = Vec::new();
            for h in hashes {
                if !cache.contains(h).await {
                    missing.push(h);
                }
            }
            missing
        }
    };
    let mut missing = missing_set(chunk_cache, manifest).await;
    if missing.is_empty() {
        return;
    }
    let scope = PeerChunkScope::BaseImage(image.manifest_digest.as_str().to_string());
    for peer in image.warm_peers.iter().take(2) {
        let started = std::time::Instant::now();
        let mut stats = crate::peer_fill::pull_chunks_from_peer(
            &peer.addr,
            scope.clone(),
            &missing,
            chunk_cache,
            peer_health,
        )
        .await;
        if stats.backpressure && !stats.failed {
            // The seed's serve semaphore was full — busy, not dead. One
            // jittered retry (pid-keyed so concurrent warmers spread),
            // then whatever is left goes to GCS. Never a retry ladder.
            let jitter_ms = 2_000 + (std::process::id() as u64 % 3_000);
            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;
            missing = missing_set(chunk_cache, manifest).await;
            if missing.is_empty() {
                return;
            }
            stats = crate::peer_fill::pull_chunks_from_peer(
                &peer.addr,
                scope.clone(),
                &missing,
                chunk_cache,
                peer_health,
            )
            .await;
        }
        tracing::info!(
            image = %image.manifest_digest,
            peer = %peer.addr,
            landed = stats.landed,
            landed_bytes = stats.landed_bytes,
            missing_on_peer = stats.missing,
            failed = stats.failed,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "peer pre-pass window complete",
        );
        if !stats.failed {
            return; // success (or honest misses) — GCS loop mops up
        }
        // Peer lost mid-window: recompute what still needs pulling and
        // try the alternate hint (if any).
        missing = missing_set(chunk_cache, manifest).await;
        if missing.is_empty() {
            return;
        }
    }
}

async fn prefetch_one(
    image: &EnabledImageRef,
    chunk_store: &ChunkStore,
    chunk_cache: &ChunkCache,
    semaphore: &Arc<Semaphore>,
    peer_health: &Arc<crate::peer_fill::PeerHealth>,
    // ADR 0022 Option A: when `Some`, after warming the memory chunks,
    // assemble them into the contiguous per-template base memfile at this
    // path so same-template base `session.create` siblings MAP_PRIVATE one
    // resident, page-cache-warm inode (density + faster boot, no UFFD
    // handler). `Some` only for FC images with a memory manifest; gating
    // readiness on this means an image isn't "ready" until the shared
    // memfile exists, so the first session restores against a warm file.
    base_memfile: Option<PathBuf>,
    // Issue #540: the host RAM ledger's pending-charge registry — the
    // prewarm arm below registers the expected write BEFORE it starts and
    // settles it in both the success and failure arms.
    ram_ledger: &crate::ram_ledger::RamLedger,
) -> Result<WarmedManifest, PrefetchError> {
    // ADR 0045 substrate ordering (narrowed by ADR 0068). A memory-bearing
    // (FC) image restores Uffd-against-the-shared-base-shm, which requires
    // the uffd base dir to be a tmpfs/shmem mount before this prefetch
    // writes the shared base memfile there — mounting a tmpfs UNDER an
    // already-open memfile would orphan the write. This early return stays
    // to preserve that ordering (prewarm-after-mount); it is no longer the
    // scheduling gate — `capabilities::probe_base_shm_tmpfs` re-probes the
    // same statfs on every heartbeat and the coordinator's placement filter
    // (`host_meets_capabilities`) now withholds UFFD-substrate placements
    // directly on that vector, so a freshly-rolled K8s node whose node-prep
    // hasn't mounted the tmpfs yet is excluded from placement itself rather
    // than failing a restore that already landed there.
    if image.base_snapshot_memory_manifest.is_some() {
        if let Some(base_dir) = engram_sandbox_firecracker::uffd_base_dir_from_env() {
            // Create the dir first so the statfs reflects the real backing fs:
            // a subdir of an always-present tmpfs (e.g. /dev/shm), or a
            // not-yet-mounted dedicated mountpoint on the overlay. Mounting a
            // tmpfs over an existing dir later is fine.
            let _ = tokio::fs::create_dir_all(&base_dir).await;
            if !crate::capabilities::dir_is_tmpfs(&base_dir) {
                return Err(PrefetchError::SubstrateNotReady(format!(
                    "uffd base dir {} is not a tmpfs/shmem mount yet \
                     (node-prep may not have mounted it)",
                    base_dir.display()
                )));
            }
        }
    }

    // Accumulate the deduped pin set across the disk + memory manifests.
    // `seen` keeps the pin batch unique so a chunk shared by both manifests
    // is pinned once and unpinned once.
    let mut pin_hashes: Vec<ChunkHash> = Vec::new();
    let mut seen: HashSet<ChunkHash> = HashSet::new();
    let mut record_hashes = |manifest: &Manifest| {
        for chunk in &manifest.chunks {
            if seen.insert(chunk.hash) {
                pin_hashes.push(chunk.hash);
            }
        }
    };

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
    record_hashes(&disk_manifest);
    // ADR 0095: LAN peer pre-pass, then the verifying loop completes
    // the remainder (and every chunk on a peer-less fleet) via GCS.
    peer_prepass(image, &disk_manifest, chunk_cache, peer_health).await;
    let mut total =
        prefetch_manifest_chunks(disk_manifest, chunk_store, chunk_cache, semaphore).await?;

    // ADR 0045 addendum: the base shm file readiness will rest on, if any
    // (see `WarmedManifest::base_shm`).
    let mut base_shm: Option<PathBuf> = None;
    let mut base_shm_headroom_skipped = false;

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
        record_hashes(&memory_manifest);
        peer_prepass(image, &memory_manifest, chunk_cache, peer_health).await;
        total +=
            prefetch_manifest_chunks(memory_manifest.clone(), chunk_store, chunk_cache, semaphore)
                .await?;

        // ADR 0045 C1 (the handshake-breakdown follow-up): pre-warm the
        // SUBSTRATE's per-image base shm file from the just-prefetched
        // NVMe-warm chunks. Without this, the FIRST session on a
        // freshly-rolled host pays fetch+pwrite+CONTINUE for every
        // base page it touches (~17s of in-guest first-touch on the
        // prod canary); with it, first sessions CONTINUE against an
        // already-populated page cache like every later sibling.
        // Skip-if-exists: a handler may already own (and be lazily
        // populating) the file — both writers produce byte-identical
        // content at the same offsets, but ceding to the lazy path
        // keeps this arm trivially safe. Manifest-elided ranges stay
        // HOLES (the handler's ZEROPAGE arm owns zero pages — v2b
        // semantics). Best-effort: a tmpfs hiccup must not block image
        // readiness; the lazy path is the backstop.
        // ADR 0092 `lazy`: never pre-warm — the handler populates the
        // base on fault, so tmpfs holds only the resume working set.
        // (`filter` keeps the arm's 70-line body untouched; the mode is
        // logged once at supervisor start.)
        if let Some(base_dir) = engram_sandbox_firecracker::uffd_base_dir_from_env()
            .filter(|_| base_shm_mode_from_env() == BaseShmMode::Full)
        {
            let base_path = engram_sandbox_firecracker::uffd_base_path_in(&base_dir, &memory_ref);
            if tokio::fs::metadata(&base_path).await.is_ok() {
                // Already present — pre-warmed by a prior tick, or a pod
                // roll's node-tmpfs survivor. Readiness rests on it: track
                // it so the recheck notices if it later vanishes.
                base_shm = Some(base_path.clone());
            } else {
                // Issue #540: the non-hole byte total this write is about
                // to land — the same figure `prewarm_base_shm` will
                // actually pwrite (elided ranges stay holes, never
                // written). Registered BEFORE the write so the very next
                // heartbeat tick charges it against `allocatable_mib`.
                let pending_bytes = manifest_non_hole_bytes(&memory_manifest);
                // Headroom pre-check (the 2026-06-28 `pwrite ... No space
                // left on device` incident class): skip the multi-GiB
                // write attempt outright if the tmpfs plainly doesn't have
                // room, instead of discovering it mid-pwrite. `None`
                // (statfs failed, or non-Linux) is treated as "don't
                // skip" — fail-soft, same posture as every other read
                // here; the existing warn-and-continue-then-lazy-backstop
                // still catches it if this check is wrong.
                let headroom_mib = crate::ram_ledger::tmpfs_free_mib(&base_dir);
                let needed_mib = pending_bytes.div_ceil(1024 * 1024);
                if headroom_mib.is_some_and(|free| free < needed_mib) {
                    base_shm_headroom_skipped = true;
                    ::metrics::counter!(
                        crate::metrics::BASE_SHM_PREWARM_SKIPPED_TOTAL,
                        "reason" => "tmpfs_headroom"
                    )
                    .increment(1);
                    tracing::warn!(
                        image_uri = %image.image_uri,
                        path = %base_path.display(),
                        needed_mib,
                        free_mib = ?headroom_mib,
                        "base shm pre-warm skipped: insufficient tmpfs headroom; \
                         the handler's lazy path backstops",
                    );
                } else {
                    ram_ledger.register_pending_base_shm(
                        memory_ref,
                        base_path.clone(),
                        pending_bytes,
                    );
                    let prewarm_result =
                        prewarm_base_shm(&base_path, &memory_manifest, chunk_store, chunk_cache)
                            .await;
                    ram_ledger.settle_pending(&memory_ref);
                    match prewarm_result {
                        Ok(written) => {
                            base_shm = Some(base_path.clone());
                            tracing::info!(
                                image_uri = %image.image_uri,
                                path = %base_path.display(),
                                chunks = written,
                                "per-image base shm pre-warmed at prefetch (ADR 0045 C1)",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                image_uri = %image.image_uri,
                                path = %base_path.display(),
                                error = %e,
                                "base shm pre-warm failed; the handler's lazy path backstops",
                            );
                            let _ = tokio::fs::remove_file(&base_path).await;
                        }
                    }
                }
            }
        }

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

    Ok(WarmedManifest {
        chunk_count: total,
        hashes: pin_hashes,
        base_shm,
        base_shm_headroom_skipped,
    })
}

/// Issue #540: the total bytes a `prewarm_base_shm` call against this
/// manifest will actually `pwrite` — Σ chunk lengths, NOT
/// `manifest.total_bytes` (the virtual/logical size, which includes
/// manifest-elided HOLE ranges the prewarm never writes). Every chunk is
/// `chunk_size` bytes except possibly the last, which is clipped to
/// whatever remains before `total_bytes`.
fn manifest_non_hole_bytes(manifest: &Manifest) -> u64 {
    let chunk_size = manifest.chunk_size.as_u64();
    manifest
        .chunks
        .iter()
        .map(|c| {
            let remaining = manifest.total_bytes.saturating_sub(c.offset);
            remaining.min(chunk_size)
        })
        .sum()
}

/// ADR 0045 C1: populate a per-image base shm file from a memory
/// manifest's chunks (NVMe-warm after the preceding prefetch). Grow-only
/// size like the handler's `BaseShm::open`; chunk bytes land at their
/// manifest offsets; elided ranges stay holes. Returns chunks written.
async fn prewarm_base_shm(
    path: &std::path::Path,
    manifest: &Manifest,
    chunk_store: &ChunkStore,
    chunk_cache: &engram_chunk_store::ChunkCache,
) -> Result<usize, String> {
    use std::os::unix::fs::FileExt;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create base dir: {e}"))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open base file: {e}"))?;
    if file.metadata().map_err(|e| e.to_string())?.len() < manifest.total_bytes {
        file.set_len(manifest.total_bytes)
            .map_err(|e| format!("size base file: {e}"))?;
    }
    let file = std::sync::Arc::new(file);
    let mut written = 0usize;
    for chunk in &manifest.chunks {
        let bytes = chunk_cache
            .get(chunk.hash, || chunk_store.get_chunk(chunk.hash))
            .await
            .map_err(|e| format!("chunk {}: {e}", chunk.hash))?;
        let file = file.clone();
        let offset = chunk.offset;
        tokio::task::spawn_blocking(move || file.write_all_at(&bytes, offset))
            .await
            .map_err(|e| format!("join: {e}"))?
            .map_err(|e| format!("pwrite at {offset}: {e}"))?;
        written += 1;
    }
    Ok(written)
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
    /// ADR 0045: the substrate is configured but its base dir isn't a
    /// tmpfs/shmem mount yet (node-prep hasn't mounted it). Withhold
    /// readiness for memory-bearing images until the mount appears.
    SubstrateNotReady(String),
}

impl std::fmt::Display for PrefetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestLoad(m) => write!(f, "load chunk manifest: {m}"),
            Self::ChunkFetch(m) => write!(f, "chunk fetch: {m}"),
            Self::SemaphoreClosed => write!(f, "prefetch semaphore closed"),
            Self::JoinError(m) => write!(f, "task join: {m}"),
            Self::MemfileMaterialize(m) => write!(f, "materialize base memfile: {m}"),
            Self::SubstrateNotReady(m) => write!(f, "substrate base dir not ready: {m}"),
        }
    }
}

impl std::error::Error for PrefetchError {}

// ADR 0068: `dir_is_tmpfs` moved to `crate::capabilities` — one probe
// implementation now feeds both this readiness-ordering early return and
// the coordinator-visible `base_shm_tmpfs` capability. Its test
// (`base_shm_tmpfs_rejects_a_plain_tempdir` / the /proc/mounts
// cross-check) moved with it.

#[cfg(test)]
mod tests {
    use super::*;

    // ADR 0022: pin a small (16 KiB) temp file resident — exercises the
    // mmap+mlock+drop path on the Linux CI runner (16 KiB fits even a
    // 64 KiB RLIMIT_MEMLOCK, so it doesn't depend on the host-agent's
    // setrlimit). Zero-length → None.
    #[cfg(target_os = "linux")]
    #[test]
    fn pin_memfile_pins_small_file_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        assert!(pin_memfile(&empty).is_none(), "zero-length file → no pin");

        let path = dir.path().join("memory.bin");
        std::fs::write(&path, vec![7u8; 16 * 1024]).unwrap();
        let pin = pin_memfile(&path).expect("16 KiB file should mmap+mlock");
        // Drop releases the mlock+mapping; a second pin then succeeds too.
        drop(pin);
        assert!(pin_memfile(&path).is_some(), "re-pin after release works");
    }

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

    fn ref_with_digest(digest: &str) -> EnabledImageRef {
        EnabledImageRef {
            image_uri: format!("localhost:5001/demo:{digest}"),
            manifest_digest: ManifestDigest::new(digest.to_string()),
            base_snapshot_id: SnapshotId::new(),
            base_snapshot_disk_manifest: ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            },
            base_snapshot_memory_manifest: None,
            warm_peers: Vec::new(),
        }
    }

    // ADR 0036 amendment (issue #538): `union_image_refs` is what the
    // host-agent's heartbeat loop feeds the prefetch supervisor's watch
    // channel — it must dedup by digest (an image that flips
    // prestaging → enabled between two heartbeats must not appear twice)
    // and must include every digest from EITHER side.
    #[test]
    fn union_image_refs_dedups_by_digest() {
        let shared = ref_with_digest("sha256:shared");
        let only_enabled = ref_with_digest("sha256:enabled-only");
        let only_prestaging = ref_with_digest("sha256:prestage-only");

        let enabled = vec![shared.clone(), only_enabled.clone()];
        let prestaging = vec![shared.clone(), only_prestaging.clone()];

        let union = union_image_refs(&enabled, &prestaging);
        let digests: std::collections::HashSet<_> =
            union.iter().map(|r| r.manifest_digest.clone()).collect();
        assert_eq!(union.len(), 3, "3 distinct digests, shared one deduped");
        assert!(digests.contains(&shared.manifest_digest));
        assert!(digests.contains(&only_enabled.manifest_digest));
        assert!(digests.contains(&only_prestaging.manifest_digest));
    }

    #[test]
    fn union_image_refs_empty_inputs_is_empty() {
        assert!(union_image_refs(&[], &[]).is_empty());
    }

    #[test]
    fn union_image_refs_prestaging_only() {
        let r = ref_with_digest("sha256:only-prestaging");
        let union = union_image_refs(&[], std::slice::from_ref(&r));
        assert_eq!(union, vec![r]);
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
        // Hermetic against the HOST machine's disk fullness: the cache's
        // dynamic free-space floor (default: keep the backing fs ≤ ~80%
        // full) is probed via statvfs on every sweep, and these tests run
        // with sweep_debounce_ms=0 — on a dev machine whose disk is past
        // the floor, every sweep evicted every not-yet-pinned chunk and
        // the readiness recheck flipped images unready (found 2026-07-13:
        // deterministic failure on an 81%-full laptop, green on CI).
        // Floor 0 = never floor-evict; the byte budget still governs.
        // Safe under nextest (one process per test).
        std::env::set_var(engram_chunk_store::cache::FREE_FLOOR_PCT_ENV_VAR, "0");
        let dir = tempfile::tempdir().unwrap();
        let blob: StdArc<dyn BlobStorage> =
            StdArc::new(LocalBlobStorage::new(dir.path().join("blob")));
        let store = ChunkStore::new(blob);
        let cache = ChunkCache::new(ChunkCacheConfig {
            root: dir.path().join("cache"),
            budget_bytes: 256 * 1024 * 1024,
            sweep_debounce_ms: 0,
            eviction_enabled: true,
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
            warm_peers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn prefetch_materializes_base_memfile_and_is_idempotent() {
        let (store, cache, dir, disk_ref, mem_ref, mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let base_id = SnapshotId::new();
        let dest = dir
            .path()
            .join("snapshots")
            .join(base_id.to_string())
            .join("memory.bin");

        let img = image_ref(base_id, disk_ref, Some(mem_ref));
        prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            Some(dest.clone()),
            &ledger,
        )
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
        prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            Some(dest.clone()),
            &ledger,
        )
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
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let base_id = SnapshotId::new();
        let dest = dir
            .path()
            .join("snapshots")
            .join(base_id.to_string())
            .join("memory.bin");

        let img = image_ref(base_id, disk_ref, None);
        prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            Some(dest.clone()),
            &ledger,
        )
        .await
        .unwrap();
        assert!(
            tokio::fs::metadata(&dest).await.is_err(),
            "disk-only image must not materialize a memfile",
        );
    }

    // ---- ADR 0039: base-manifest pin bookkeeping ----

    #[tokio::test]
    async fn prefetch_one_returns_deduped_disk_plus_memory_hash_set() {
        // The pin batch is the union of the disk + memory manifests'
        // chunk hashes, deduped. We pin exactly this set; the disable
        // path unpins exactly it. Verify the set matches the manifests'
        // union (a mix-up between disk/memory chunks would surface here).
        let (store, cache, dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let base_id = SnapshotId::new();
        let _ = dir; // tempdir kept alive

        let disk_m = store.get_manifest(disk_ref).await.unwrap();
        let mem_m = store.get_manifest(mem_ref).await.unwrap();
        let expected: HashSet<ChunkHash> = disk_m
            .chunks
            .iter()
            .chain(mem_m.chunks.iter())
            .map(|c| c.hash)
            .collect();

        let img = image_ref(base_id, disk_ref, Some(mem_ref));
        let warmed = prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            None,
            &ledger,
        )
        .await
        .unwrap();

        // No duplicates in the returned batch.
        let got: HashSet<ChunkHash> = warmed.hashes.iter().copied().collect();
        assert_eq!(got.len(), warmed.hashes.len(), "pin batch must be deduped");
        assert_eq!(got, expected, "pin batch = union of disk + memory hashes");
        assert!(
            warmed.base_shm.is_none(),
            "substrate off (no ENGRAM_FC_UFFD_BASE_DIR) ⇒ readiness rests on no base file",
        );
    }

    #[tokio::test]
    async fn reconcile_pins_enabled_then_unpins_on_disable() {
        // End-to-end of the supervisor's pin bookkeeping: an enabled image
        // pins its base manifest; disabling it unpins exactly that set; a
        // chunk shared with a still-enabled image stays pinned (refcount).
        let (store, cache, dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let _ = dir;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let readiness = ImageReadiness::new();
        let pinned: PinnedManifests = Arc::new(Mutex::new(HashMap::new()));
        let tracked: TrackedBaseShm = Arc::new(Mutex::new(HashMap::new()));
        let skipped: HeadroomSkipped = Arc::new(Mutex::new(HashSet::new()));
        let mut memfiles: HashMap<ManifestDigest, MemfileState> = HashMap::new();

        let base_id = SnapshotId::new();
        let img = image_ref(base_id, disk_ref, Some(mem_ref));
        let total_chunks = {
            let disk_m = store.get_manifest(disk_ref).await.unwrap();
            let mem_m = store.get_manifest(mem_ref).await.unwrap();
            disk_m
                .chunks
                .iter()
                .chain(mem_m.chunks.iter())
                .map(|c| c.hash)
                .collect::<HashSet<_>>()
                .len()
        };

        // Enable: reconcile spawns the prefetch task. It marks ready +
        // pins asynchronously; poll until readiness flips (bounded).
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        for _ in 0..200 {
            if readiness.contains(&img.manifest_digest) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            readiness.contains(&img.manifest_digest),
            "image should become ready after prefetch",
        );
        // The pin batch is recorded + the cache reflects it.
        assert_eq!(
            pinned.lock().get(&img.manifest_digest).map(Vec::len),
            Some(total_chunks),
        );
        assert_eq!(cache.pinned_count(), total_chunks, "base manifest pinned");

        // Disable: reconcile with an empty enabled set unpins the batch.
        reconcile(
            &[],
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert!(!readiness.contains(&img.manifest_digest), "now unready");
        assert!(pinned.lock().get(&img.manifest_digest).is_none());
        assert_eq!(cache.pinned_count(), 0, "all base pins released on disable");
    }

    #[tokio::test]
    async fn reconcile_flips_ready_to_unready_when_base_chunk_is_gone() {
        // ADR 0039 (honest readiness): if a `ready` image's base chunks are
        // no longer resolvable on local NVMe (LRU eviction / a silently-
        // failed write_local), the next reconcile must flip it back to
        // not-ready so a restore never lands on a ready-but-cold host.
        let (store, cache, _dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let readiness = ImageReadiness::new();
        let pinned: PinnedManifests = Arc::new(Mutex::new(HashMap::new()));
        let tracked: TrackedBaseShm = Arc::new(Mutex::new(HashMap::new()));
        let skipped: HeadroomSkipped = Arc::new(Mutex::new(HashSet::new()));
        let mut memfiles: HashMap<ManifestDigest, MemfileState> = HashMap::new();
        let base_id = SnapshotId::new();
        let img = image_ref(base_id, disk_ref, Some(mem_ref));

        // Enable + wait for ready.
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        for _ in 0..200 {
            if readiness.contains(&img.manifest_digest) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            readiness.contains(&img.manifest_digest),
            "ready after prefetch"
        );

        // Simulate the chunk going cold: delete one pinned chunk's on-disk
        // cache file out from under the cache (what an LRU sweep or a failed
        // write_local would leave behind — pinned-but-not-resident).
        let victim = *pinned
            .lock()
            .get(&img.manifest_digest)
            .unwrap()
            .first()
            .unwrap();
        cache.evict_on_disk_for_test(victim);
        assert!(!cache.contains_on_disk(victim), "victim chunk now gone");

        // Re-verify on the next reconcile: the image is no longer warm, so it
        // flips to not-ready (then re-prefetches — but it's gone unready,
        // which is the contract the scheduler relies on).
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert!(
            !readiness.contains(&img.manifest_digest),
            "a ready image with an evicted base chunk must flip to not-ready",
        );
    }

    #[tokio::test]
    async fn reconcile_flips_ready_to_unready_when_tracked_base_shm_vanishes() {
        // ADR 0045 addendum: readiness resting on a substrate base shm file
        // must notice the file vanishing (a sweep, a tmpfs remount, an
        // operator rm) exactly like an evicted chunk — flip to not-ready so
        // the scheduler stops sending sessions that would pay full lazy
        // population.
        let (store, cache, dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let readiness = ImageReadiness::new();
        let pinned: PinnedManifests = Arc::new(Mutex::new(HashMap::new()));
        let tracked: TrackedBaseShm = Arc::new(Mutex::new(HashMap::new()));
        let skipped: HeadroomSkipped = Arc::new(Mutex::new(HashSet::new()));
        let mut memfiles: HashMap<ManifestDigest, MemfileState> = HashMap::new();
        let base_id = SnapshotId::new();
        let img = image_ref(base_id, disk_ref, Some(mem_ref));

        // Enable + wait for ready (substrate env is unset here, so the
        // prefetch itself tracks nothing — the base file is injected below,
        // standing in for what the pre-warm arm records in production).
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        for _ in 0..200 {
            if readiness.contains(&img.manifest_digest) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(readiness.contains(&img.manifest_digest), "ready");

        // Track a base shm file that exists: readiness must hold.
        let base_file = dir.path().join("mem-manifest-v1.base");
        std::fs::write(&base_file, b"resident").unwrap();
        tracked
            .lock()
            .insert(img.manifest_digest.clone(), base_file.clone());
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert!(
            readiness.contains(&img.manifest_digest),
            "an intact tracked base file must not disturb readiness",
        );

        // The file vanishes → the next reconcile flips the image unready.
        // A zero-permit semaphore parks the fall-through re-prefetch so the
        // flip is observable (in production the re-prefetch re-warms the
        // file and readiness returns on its own).
        std::fs::remove_file(&base_file).unwrap();
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            Arc::new(Semaphore::new(0)),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert!(
            !readiness.contains(&img.manifest_digest),
            "a ready image whose tracked base shm file vanished must flip to not-ready",
        );
    }
    /// ADR 0045 addendum: with the substrate on, `prefetch_one` reports the
    /// base shm file readiness rests on — for a fresh pre-warm AND for a
    /// file already present (a pod roll's node-tmpfs survivor) — and
    /// re-creates it after a deletion (the recheck's re-prefetch heal).
    ///
    /// The base dir must be a tmpfs on Linux (`prefetch_one`'s
    /// substrate-ordering probe), so it lives under /dev/shm there. Env
    /// mutation is safe: nextest runs each test in its own process.
    #[tokio::test]
    async fn prefetch_one_tracks_and_reheals_the_base_shm_file() {
        #[cfg(target_os = "linux")]
        let base_dir = tempfile::Builder::new()
            .prefix("engram-prewarm-test-")
            .tempdir_in("/dev/shm")
            .unwrap();
        #[cfg(not(target_os = "linux"))]
        let base_dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest process-per-test; no concurrent env readers.
        unsafe { std::env::set_var("ENGRAM_FC_UFFD_BASE_DIR", base_dir.path()) };

        let (store, cache, _dir, disk_ref, mem_ref, mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let img = image_ref(SnapshotId::new(), disk_ref, Some(mem_ref));

        let expected_path =
            engram_sandbox_firecracker::uffd_base_path_in(base_dir.path(), &mem_ref);

        // The sweeper keep-set derivation names the same path (the single
        // naming authority is `uffd_base_path_in`; drift here would let
        // the GC sweep what readiness tracks).
        assert_eq!(
            enabled_base_shm_paths(std::slice::from_ref(&img)),
            [expected_path.clone()].into_iter().collect::<HashSet<_>>(),
        );

        // Fresh pre-warm: file written + reported.
        let warmed = prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            None,
            &ledger,
        )
        .await
        .unwrap();
        assert_eq!(warmed.base_shm.as_deref(), Some(expected_path.as_path()));
        assert_eq!(
            std::fs::read(&expected_path).unwrap(),
            mem_bytes,
            "pre-warm materializes the full memory image",
        );

        // Already present (the roll-survivor shape): reported, not rewritten.
        let mtime = std::fs::metadata(&expected_path)
            .unwrap()
            .modified()
            .unwrap();
        let warmed = prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            None,
            &ledger,
        )
        .await
        .unwrap();
        assert_eq!(warmed.base_shm.as_deref(), Some(expected_path.as_path()));
        assert_eq!(
            std::fs::metadata(&expected_path)
                .unwrap()
                .modified()
                .unwrap(),
            mtime,
            "an existing base file is adopted, not rewritten",
        );

        // Deleted out from under readiness (a sweep): re-prefetch re-warms.
        std::fs::remove_file(&expected_path).unwrap();
        let warmed = prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            None,
            &ledger,
        )
        .await
        .unwrap();
        assert_eq!(warmed.base_shm.as_deref(), Some(expected_path.as_path()));
        assert_eq!(std::fs::read(&expected_path).unwrap(), mem_bytes);

        unsafe { std::env::remove_var("ENGRAM_FC_UFFD_BASE_DIR") };
    }

    /// ADR 0092 `lazy`: the pre-warm arm is skipped entirely — no file, no
    /// tracking, no pending charge — the handler populates on fault.
    /// (Env mutation is safe: nextest is process-per-test, and the mode
    /// OnceLock is first read inside this test's process.)
    #[tokio::test]
    async fn lazy_mode_skips_base_shm_prewarm() {
        #[cfg(target_os = "linux")]
        let base_dir = tempfile::Builder::new()
            .prefix("engram-lazy-test-")
            .tempdir_in("/dev/shm")
            .unwrap();
        #[cfg(not(target_os = "linux"))]
        let base_dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest process-per-test; no concurrent env readers.
        unsafe { std::env::set_var("ENGRAM_FC_UFFD_BASE_DIR", base_dir.path()) };
        unsafe { std::env::set_var("ENGRAM_FC_BASE_SHM_MODE", "lazy") };

        let (store, cache, _dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let img = image_ref(SnapshotId::new(), disk_ref, Some(mem_ref));
        let expected_path =
            engram_sandbox_firecracker::uffd_base_path_in(base_dir.path(), &mem_ref);

        let warmed = prefetch_one(
            &img,
            &store,
            &cache,
            &sem,
            &crate::peer_fill::PeerHealth::new(),
            None,
            &ledger,
        )
        .await
        .unwrap();
        assert_eq!(warmed.base_shm, None, "lazy: nothing tracked");
        assert!(!warmed.base_shm_headroom_skipped, "lazy is not a skip-debt");
        assert!(
            !expected_path.exists(),
            "lazy: the base file is the handler's to create, not the pre-warm's",
        );
        // The NVMe warm (chunk pins) is untouched by the mode.
        assert!(warmed.chunk_count > 0);

        unsafe { std::env::remove_var("ENGRAM_FC_UFFD_BASE_DIR") };
        unsafe { std::env::remove_var("ENGRAM_FC_BASE_SHM_MODE") };
    }

    /// ADR 0092: a headroom-skip is a retry debt, not a readiness event —
    /// the recheck re-runs `prefetch_one` while the image STAYS ready
    /// (sessions keep flowing on the lazy backstop), and the debt clears
    /// once a pre-warm lands.
    #[tokio::test]
    async fn headroom_retry_reprefetches_without_flipping_readiness() {
        let (store, cache, _dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let readiness = ImageReadiness::new();
        let pinned: PinnedManifests = Arc::new(Mutex::new(HashMap::new()));
        let tracked: TrackedBaseShm = Arc::new(Mutex::new(HashMap::new()));
        let skipped: HeadroomSkipped = Arc::new(Mutex::new(HashSet::new()));
        let mut memfiles: HashMap<ManifestDigest, MemfileState> = HashMap::new();
        let img = image_ref(SnapshotId::new(), disk_ref, Some(mem_ref));

        // Seed ready.
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        for _ in 0..200 {
            if readiness.contains(&img.manifest_digest) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(readiness.contains(&img.manifest_digest), "ready");

        // Simulate a headroom-skipped pre-warm from a prior tick. The
        // retry pass must NOT flip readiness (zero-permit semaphore parks
        // the re-prefetch so the non-flip is observable).
        skipped.lock().insert(img.manifest_digest.clone());
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            Arc::new(Semaphore::new(0)),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert!(
            readiness.contains(&img.manifest_digest),
            "a headroom retry must keep the image READY (the lazy path backstops)",
        );

        // With permits, the retry completes and clears the debt (the
        // substrate env is unset here, so the pre-warm arm no-ops and
        // reports no skip — the marker must still be dropped).
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        for _ in 0..200 {
            if !skipped.lock().contains(&img.manifest_digest) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !skipped.lock().contains(&img.manifest_digest),
            "a completed retry clears the debt",
        );
        assert!(readiness.contains(&img.manifest_digest), "still ready");
    }

    /// ADR 0092: a vanished base memfile (out-of-band rm — even under a
    /// live mlock, which silently pins the unlinked inode) flips the image
    /// unready and drops the stale `memfiles` entry so the re-prefetch
    /// re-materializes.
    #[tokio::test]
    async fn reconcile_flips_ready_when_base_memfile_vanishes() {
        let (store, cache, dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let readiness = ImageReadiness::new();
        let pinned: PinnedManifests = Arc::new(Mutex::new(HashMap::new()));
        let tracked: TrackedBaseShm = Arc::new(Mutex::new(HashMap::new()));
        let skipped: HeadroomSkipped = Arc::new(Mutex::new(HashSet::new()));
        let mut memfiles: HashMap<ManifestDigest, MemfileState> = HashMap::new();
        let img = image_ref(SnapshotId::new(), disk_ref, Some(mem_ref));

        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        for _ in 0..200 {
            if readiness.contains(&img.manifest_digest) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(readiness.contains(&img.manifest_digest), "ready");

        // Track a memfile that exists: readiness holds.
        let memfile = dir.path().join("memory.bin");
        std::fs::write(&memfile, b"resident").unwrap();
        memfiles.insert(
            img.manifest_digest.clone(),
            MemfileState {
                path: memfile.clone(),
                pin: None,
            },
        );
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert!(
            readiness.contains(&img.manifest_digest),
            "an intact memfile must not disturb readiness",
        );

        // rm the memfile → unready + entry dropped for re-materialization.
        std::fs::remove_file(&memfile).unwrap();
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            Arc::new(Semaphore::new(0)),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert!(
            !readiness.contains(&img.manifest_digest),
            "a vanished memfile must flip the image unready",
        );
        assert!(
            !memfiles.contains_key(&img.manifest_digest),
            "the stale memfiles entry must be dropped so re-prefetch re-materializes",
        );
    }

    #[tokio::test]
    async fn reconcile_publishes_memfile_bytes_as_the_cache_co_tenant_reserve() {
        // ADR 0092: the memfiles share the chunk cache's filesystem and
        // its sweeper can't evict them — every reconcile tick publishes
        // their allocated bytes as the cache's co-tenant reserve so the
        // cache stops aiming for disk the memfiles occupy (the 2026-07-14
        // w8wq DiskPressure incident: at-budget warm cache + 40 GB of
        // unbudgeted memfiles).
        let (store, cache, dir, disk_ref, mem_ref, _mem_bytes) = seed().await;
        let sem = Arc::new(Semaphore::new(8));
        let ledger = Arc::new(crate::ram_ledger::RamLedger::new());
        let readiness = ImageReadiness::new();
        let pinned: PinnedManifests = Arc::new(Mutex::new(HashMap::new()));
        let tracked: TrackedBaseShm = Arc::new(Mutex::new(HashMap::new()));
        let skipped: HeadroomSkipped = Arc::new(Mutex::new(HashSet::new()));
        let mut memfiles: HashMap<ManifestDigest, MemfileState> = HashMap::new();
        let img = image_ref(SnapshotId::new(), disk_ref, Some(mem_ref));

        // Make the image ready first — the disable-reclaim arm below keys
        // on readiness (same order as the vanish test above).
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        for _ in 0..200 {
            if readiness.contains(&img.manifest_digest) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(readiness.contains(&img.manifest_digest), "ready");

        let memfile = dir.path().join("memory.bin");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&memfile).unwrap();
            f.write_all(&[0xAB; 8192]).unwrap();
            // Force block allocation: APFS/ext4 delayed allocation can
            // report st_blocks=0 until the write is flushed, making the
            // expected value racy against reconcile's later read.
            f.sync_all().unwrap();
        }
        let expected = allocated_bytes(&std::fs::metadata(&memfile).unwrap());
        assert!(expected > 0, "allocated bytes must be visible after sync");
        memfiles.insert(
            img.manifest_digest.clone(),
            MemfileState {
                path: memfile.clone(),
                pin: None,
            },
        );
        reconcile(
            std::slice::from_ref(&img),
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert_eq!(
            cache.co_tenant_reserved(),
            expected,
            "the tick must reserve the memfile's allocated bytes out of the cache ceiling",
        );

        // Image disabled → memfile reclaimed → the reserve returns to 0.
        reconcile(
            &[],
            readiness.clone(),
            store.clone(),
            cache.clone(),
            sem.clone(),
            crate::peer_fill::PeerHealth::new(),
            None,
            &mut memfiles,
            pinned.clone(),
            ledger.clone(),
            tracked.clone(),
            skipped.clone(),
        )
        .await;
        assert_eq!(
            cache.co_tenant_reserved(),
            0,
            "disabling the image must release its reserve",
        );
    }

    /// ADR 0092 env parsers (one env value per test — the getters cache in
    /// a OnceLock and nextest is process-per-test).
    #[test]
    fn base_shm_mode_env_parses_lazy() {
        unsafe { std::env::set_var("ENGRAM_FC_BASE_SHM_MODE", "LaZy") };
        assert_eq!(base_shm_mode_from_env(), BaseShmMode::Lazy);
    }

    #[test]
    fn memfile_pin_env_disables_the_mlock() {
        unsafe { std::env::set_var("ENGRAM_FC_BASE_MEMFILE_PIN", "0") };
        assert!(!memfile_pin_enabled());
    }

    #[test]
    fn residency_env_defaults_are_todays_behavior() {
        unsafe { std::env::remove_var("ENGRAM_FC_BASE_SHM_MODE") };
        unsafe { std::env::remove_var("ENGRAM_FC_BASE_MEMFILE_PIN") };
        assert_eq!(base_shm_mode_from_env(), BaseShmMode::Full);
        assert!(memfile_pin_enabled());
    }
}

#[cfg(test)]
mod prewarm_tests {
    use super::*;
    use std::sync::Arc;

    /// ADR 0045 C1: the pre-warm writes each manifest chunk at its
    /// offset, sizes the file to total_bytes, and leaves elided ranges
    /// as holes (the handler's ZEROPAGE arm owns zeros).
    #[tokio::test]
    async fn prewarm_base_shm_writes_chunks_and_preserves_holes() {
        let tmp = tempfile::tempdir().unwrap();
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(tmp.path().join("blob")),
        );
        let store = ChunkStore::new(blob);
        let cache = engram_chunk_store::ChunkCache::new(
            engram_chunk_store::cache::ChunkCacheConfig::new(tmp.path().join("cache")),
        );

        // A sparse image: chunk at 0 (0xAA), HOLE at 4096, chunk at 8192 (0xBB).
        let img = tmp.path().join("img.bin");
        let mut bytes = vec![0u8; 3 * 4096];
        bytes[..4096].fill(0xAA);
        bytes[2 * 4096..].fill(0xBB);
        std::fs::write(&img, &bytes).unwrap();
        let manifest = store
            .chunk_file(&img, engram_chunk_store::ManifestKind::Memory, Some(4096))
            .await
            .unwrap();
        assert_eq!(manifest.chunks.len(), 2, "the zero chunk is elided");

        let base = tmp.path().join("shm").join("base.base");
        let written = prewarm_base_shm(&base, &manifest, &store, &cache)
            .await
            .expect("prewarm");
        assert_eq!(written, 2);

        let got = std::fs::read(&base).unwrap();
        assert_eq!(got.len() as u64, manifest.total_bytes);
        assert_eq!(&got[..4096], &bytes[..4096]);
        assert_eq!(&got[2 * 4096..], &bytes[2 * 4096..]);
        assert!(
            got[4096..2 * 4096].iter().all(|b| *b == 0),
            "hole reads as zeros"
        );
    }
}
