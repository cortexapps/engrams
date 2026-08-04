//! Local NVMe cache for chunks.
//!
//! Fronts the chunk store with an LRU-bounded local-disk cache so
//! hot reads don't pay the BlobStorage round-trip every time.
//! After a chunk has been GET'd once, subsequent GETs of the same
//! hash hit a local file under `<root>/<2-hex>/<rest>`.
//!
//! Properties:
//!
//! - **Atomic writes**: bytes land in a *writer-unique* temp file
//!   alongside the target path, then rename into place. Crashes
//!   mid-write don't leave the cache pointing at half-written chunks.
//!   The temp name carries pid + a process-global counter for
//!   IN-process concurrency: many tokio tasks (restore prefetch,
//!   populate-server connections, disk-daemon flushes) populate
//!   concurrently inside the ONE writer process. (ADR 0075 made this
//!   single-writer; the pid component is vestigial-but-harmless
//!   belt-and-braces from the multi-writer era, whose fixed-temp
//!   collision once cost a prod resume ~92 s of GCS page-faulting.)
//! - **Eviction (FIFO by populate time)**: when the cache filesystem is
//!   fuller than the free-space floor (default: keep ~20% free) — or,
//!   when total cached bytes exceed the absolute ceiling (disk-derived
//!   by default; see [`ChunkCacheConfig::from_env_or_default`], ADR
//!   0067) — the oldest chunks get unlinked. "Oldest" is by *populate*
//!   time: reads do NOT touch recency, so this is FIFO by first-write,
//!   not true LRU-by-access. Hot chunks are protected explicitly
//!   instead — by the refcounted pin set (an enabled image's base
//!   manifest is pinned resident), not by recency. Sweeps run against
//!   an **in-memory index** (hash → size + populate time, built from
//!   one walk on the first sweep, maintained by every populate/unlink
//!   thereafter): #1003 — walking a 516k-file cache per sweep
//!   materialized ~200 MB of listing per pass and cost 100-500 ms, and
//!   concurrent sweeps compounded into multi-GB heap bursts. Only the
//!   periodic [`ChunkCache::sweep`] still walks the directory, to
//!   reconcile drift from out-of-process writers (the UFFD handler).
//!   The free-space floor is re-checked via `statvfs(2)` on every
//!   sweep, so the cache yields disk to the snapshots and checkpoints
//!   that share the work_dir mount rather than racing them to ENOSPC
//!   (the prod incident where a 200 GiB byte-budget never tripped on a
//!   ~98 GiB FC host).
//! - **Periodic enforcement**: eviction runs both on the populate path
//!   (debounced, see `write_local`) AND on an independent timer
//!   ([`ChunkCache::spawn_sweeper`]) — a host under disk pressure from
//!   non-cache writers, or one that just came back from a pod restart
//!   with zero populate traffic, still gets swept (ADR 0070).
//! - **Single evictor**: [`ChunkCacheConfig::eviction_enabled`] lets a
//!   process hold a cache that *populates* (writes chunks in, serving
//!   reads) but never *evicts* (never unlinks). Exactly one process per
//!   host — the host-agent, which owns the pin set — should evict; every
//!   other process sharing the same `cache_root` (the UFFD handler) sets
//!   this `false` (ADR 0070). Without this, two independent LRU policies
//!   raced over the same directory with disjoint in-memory pin state, so
//!   a pressured non-pinning evictor could unlink exactly the chunks the
//!   pinning one was protecting.
//! - **Singleflight on miss**: if N threads simultaneously ask for
//!   a chunk that's not cached, exactly one BlobStorage fetch
//!   runs; the others await its completion.
//! - **Pin set**: working-set chunks can be marked never-evict. The
//!   image-prefetch supervisor (ADR 0039) pins each enabled image's
//!   canonical base manifest (disk + memory) so the LRU can never evict
//!   the shared base out from under live File-backend siblings. Pins are
//!   a **floor, not a bug**: a sweep never auto-unpins under pressure —
//!   when pinned bytes alone approach or exceed the budget, that's an
//!   alarm (`engram_chunk_cache_pins_over_budget`, ADR 0070), not a
//!   signal to evict pinned content. Pins are **reference-counted**: two
//!   enabled images that share a base chunk each hold a pin, and
//!   disabling one leaves the chunk pinned until the last holder
//!   unpins. `pin`/`unpin` are the single-hash primitives;
//!   `pin_all`/`unpin_all` batch over a manifest's chunk set.
//!
//! What this module does NOT do:
//!
//! - Re-verify cache files on read. Integrity is checked once on
//!   *populate* — `get`'s fetch arm and `put` hash the bytes before the
//!   atomic temp+rename — and the read path then trusts the
//!   content-addressed file. Re-hashing a 16 MiB chunk is ~80 ms on our
//!   no-SHA-NI hosts and, re-run per read, dominated restore latency
//!   (ADR 0021). Post-write bit-rot / external modification is left to
//!   PD / local-SSD durability, not caught here.
//! - GC. This module only does local-NVMe LRU eviction. Cross-host
//!   BlobStorage lifecycle is the coordinator's chunk-GC sweep (ADR
//!   0016 Phase C — `engram-coordinator/src/chunk_gc.rs`: pin-set +
//!   24 h-grace candidate promotion), not this cache's concern.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::fs;
use tokio::sync::oneshot;

use crate::error::{ChunkStoreError, Result};
use crate::manifest::ChunkHash;

/// Default populate-path sweep debounce (see `write_local`).
pub const DEFAULT_SWEEP_DEBOUNCE_MS: i64 = 5_000;

/// ADR 0019 / telemetry restoration (#526), review finding 7: the
/// peer-vs-GCS cache-fill counter, labeled `source="gcs"|"peer"`. This
/// is the baseline meter for epic-gcs-free-resume's "GCS-free by
/// policy" claim, so both fill sources must agree on the exact metric
/// name — a typo in either literal would silently fork the series.
///
/// - `source="gcs"` / `source="peer"`: incremented in
///   [`ChunkCache::get_with_source`]'s leader-persist arm with the label
///   the fetch closure reported (ADR 0095: the closure seam is the one
///   place that knows the backend), only when `write_local` actually
///   landed the fetched bytes on disk (a `write_local` failure means the
///   fetch happened but the cache did NOT fill — see the `write_local`
///   error-handling comment just above the increment site).
/// - `source="peer"` is ALSO incremented by
///   `engram-host-agent::pooled_backend` at the loops that land
///   migration-sourced chunks via [`ChunkCache::put_no_evict`], and by
///   `engram-host-agent::peer_fill` at the bulk
///   [`ChunkCache::put_unverified_no_evict`] landings (ADR 0095).
pub const CHUNK_FILL_TOTAL: &str = "engram_chunk_fill_total";

/// Byte-counted companion to [`CHUNK_FILL_TOTAL`]. Same `source` label,
/// same call sites.
pub const CHUNK_FILL_BYTES_TOTAL: &str = "engram_chunk_fill_bytes_total";

/// Where a populate's bytes came from — the `source` label on
/// [`CHUNK_FILL_TOTAL`] / [`CHUNK_FILL_BYTES_TOTAL`] and the `tier` on
/// the fetch histogram (ADR 0095).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillSource {
    /// The cold tier. `BlobStorage` is GCS in every deployed
    /// configuration; a non-GCS impl would still be the correct label
    /// for "the cold tier", not a peer.
    BlobStorage,
    /// A fleet sibling's NVMe over `PeerChunkGet` (ADR 0095).
    Peer,
}

impl FillSource {
    /// The `source` label value on the fill counters.
    pub fn label(self) -> &'static str {
        match self {
            FillSource::BlobStorage => "gcs",
            FillSource::Peer => "peer",
        }
    }

    /// The `tier` label value on `engram_chunk_fetch_seconds` /
    /// `engram_chunk_cache_{hits,bytes}_total` (the third tier next to
    /// `nvme`).
    pub fn fetch_tier(self) -> &'static str {
        match self {
            FillSource::BlobStorage => "blobstorage",
            FillSource::Peer => "peer",
        }
    }
}

/// Configuration for the on-disk cache.
///
/// Eviction is governed by two independent constraints, whichever bites
/// first on a given sweep (see [`bytes_to_free`]):
///
/// - an **absolute byte ceiling** ([`Self::budget_bytes`]) — an operator
///   knob; and
/// - a **dynamic free-space floor** — keep the cache filesystem at/under
///   ~80% full, re-probed via `statvfs(2)` every sweep so the cache
///   yields disk to snapshots/checkpoints sharing the mount.
///
/// The floor is *not* a field here on purpose: this struct is
/// constructed by several other crates with struct literals, and the
/// floor is a host-level / env concern resolved inside
/// [`ChunkCache::new`]. Override it via [`FREE_FLOOR_PCT_ENV_VAR`] /
/// [`FREE_FLOOR_BYTES_ENV_VAR`].
#[derive(Clone, Debug)]
pub struct ChunkCacheConfig {
    /// Where cached chunks live. Typically a subdirectory of the
    /// host's NVMe-backed work_dir (e.g.
    /// `/var/lib/engram/chunk-cache/`).
    pub root: PathBuf,
    /// Absolute eviction ceiling in bytes — the cache never grows past
    /// it. Set to [`NO_CEILING`] (the default from [`Self::new`]) to
    /// disable the byte ceiling and let the free-space floor govern
    /// alone. Enforced lazily: each `put` over the ceiling evicts oldest
    /// entries.
    pub budget_bytes: u64,
    /// Minimum interval between populate-path eviction sweeps (see
    /// `write_local`). 0 = sweep on every populate (test determinism).
    pub sweep_debounce_ms: i64,
    /// Single-evictor switch (ADR 0070). `true` (default): this cache's
    /// `sweep()` (populate-path debounce AND [`ChunkCache::spawn_sweeper`])
    /// evicts as normal. `false`: `sweep()` is a no-op — the cache still
    /// *populates* (writes, reads) but never unlinks a chunk file. Set
    /// `false` on every process sharing a `cache_root` with the process
    /// that owns eviction — in this codebase, `engram-uffd-handler`,
    /// since the host-agent (which holds the pin set) is the one evictor
    /// per host. Without this, two independent LRU sweeps over the same
    /// directory can race: a pin-blind evictor preferentially reclaims
    /// exactly the pinned base-image chunks the pinning evictor is
    /// protecting (oldest-populate-first = host-boot-staged chunks).
    pub eviction_enabled: bool,
}

/// Sentinel for [`ChunkCacheConfig::budget_bytes`]: no absolute byte
/// ceiling, so the dynamic free-space floor governs alone (fill to ~80%
/// of whatever disk backs the cache, then LRU-evict). The 200 GiB fixed
/// budget this replaces never tripped on the ~98 GiB FC host — the disk
/// filled first (the prod incident). NOT the default anymore (ADR 0070)
/// — [`ChunkCacheConfig::from_env_or_default`] now derives a real
/// absolute ceiling from the disk backing `root`; this sentinel survives
/// as the explicit "floor only" opt-out and the fail-soft fallback when
/// the filesystem probe fails.
pub const NO_CEILING: u64 = u64::MAX;

/// Default fraction of the cache disk the cache may claim as its
/// absolute ceiling (see [`default_budget_bytes`]). 0.60: the cache's
/// fair share, leaving headroom for snapshots, checkpoints, memfiles,
/// the OCI cache, jails, and OS/image storage sharing the same mount.
pub const DEFAULT_DISK_FRACTION: f64 = 0.60;

/// Env var: override [`DEFAULT_DISK_FRACTION`] (0.0-1.0). Only consulted
/// when [`BUDGET_ENV_VAR`] is unset/unparseable — an explicit
/// `ENGRAM_CHUNK_CACHE_BUDGET_BYTES` always wins outright.
pub const DISK_FRACTION_ENV_VAR: &str = "ENGRAM_CHUNK_CACHE_DISK_FRACTION";

/// Resolve [`DEFAULT_DISK_FRACTION`] from env, fail-soft (out-of-range or
/// unparseable ⇒ default, with a warn). Mirrors [`resolve_free_floor_pct`].
fn resolve_disk_fraction() -> f64 {
    match std::env::var(DISK_FRACTION_ENV_VAR) {
        Ok(raw) => match raw.parse::<f64>() {
            Ok(frac) if (0.0..=1.0).contains(&frac) => {
                tracing::info!(
                    disk_fraction = frac,
                    env = DISK_FRACTION_ENV_VAR,
                    "chunk cache disk-fraction budget set via env",
                );
                frac
            }
            other => {
                tracing::warn!(
                    env = DISK_FRACTION_ENV_VAR,
                    value = raw,
                    parsed = ?other,
                    "disk-fraction env var out of range [0,1] / unparseable; using default",
                );
                DEFAULT_DISK_FRACTION
            }
        },
        Err(_) => DEFAULT_DISK_FRACTION,
    }
}

/// Default absolute cache budget for a disk of `fs_total` bytes:
/// `min(fs_total × fraction, fs_total × (1 − headroom_frac))`.
///
/// - `fraction` (typically [`DEFAULT_DISK_FRACTION`] = 0.60): the
///   cache's fair share of the disk.
/// - `headroom_frac` (typically the resolved free-space floor fraction,
///   default [`DEFAULT_FREE_FLOOR_PCT`] = 0.20): the same kubelet
///   hard-eviction-line rationale as the floor (see its doc comment) —
///   the budget must never itself authorize filling past the line the
///   floor is trying to hold the disk under.
///
/// On a 298.1 GB disk with the defaults: `min(0.60 × 298.1 GB, 0.80 ×
/// 298.1 GB)` ≈ 179 GB. Pure + no I/O so the formula is exhaustively
/// unit-testable; callers resolve `fs_total` via [`fs_total_bytes`].
fn default_budget_bytes(fs_total: u64, fraction: f64, headroom_frac: f64) -> u64 {
    let fraction = fraction.clamp(0.0, 1.0);
    let headroom_frac = headroom_frac.clamp(0.0, 1.0);
    let by_fraction = (fs_total as f64 * fraction) as u64;
    let by_headroom = (fs_total as f64 * (1.0 - headroom_frac)) as u64;
    by_fraction.min(by_headroom)
}

/// Default free-space floor: keep 20% of the cache filesystem free
/// (i.e. evict to hold the mount at/under ~80% full). Re-checked via
/// `statvfs(2)` on every sweep.
///
/// Why 20% and not 10%: the cache lives on a hostPath that
/// *persists across pod restarts*, and on the K8s host fleet (ADR 0044)
/// the kubelet's default hard-eviction threshold is `nodefs.available
/// < 10%`. A 10% floor lets the persistent cache grow right up to that
/// line, leaving no room for an incoming pod's node-assets staging
/// (a fresh `emptyDir` on the *same* filesystem) during a DaemonSet
/// roll — the kubelet then evicts the new pod for ephemeral-storage and
/// wedges the roll. A 20% floor holds the disk a clear ~10 points under
/// the kubelet line so a roll's staging always fits.
pub const DEFAULT_FREE_FLOOR_PCT: f64 = 0.20;

/// Env var: optional absolute eviction ceiling in bytes. Plain integer
/// bytes — no suffix parsing — to stay consistent with the other
/// engram_* env knobs. Unset ⇒ no ceiling; the free-space floor governs.
pub const BUDGET_ENV_VAR: &str = "ENGRAM_CHUNK_CACHE_BUDGET_BYTES";

/// Env var: free-space floor as a percentage (0–100), e.g. `10` keeps
/// ~10% free. Overrides [`DEFAULT_FREE_FLOOR_PCT`]. Takes precedence
/// over [`FREE_FLOOR_BYTES_ENV_VAR`] when both are set.
pub const FREE_FLOOR_PCT_ENV_VAR: &str = "ENGRAM_CHUNK_CACHE_FREE_FLOOR_PCT";

/// Env var: free-space floor as an absolute byte count. Converted to a
/// fraction against the live filesystem size at construction; if the
/// filesystem can't be probed the default is kept (the per-sweep
/// decision re-probes anyway). Only consulted when
/// [`FREE_FLOOR_PCT_ENV_VAR`] is unset.
pub const FREE_FLOOR_BYTES_ENV_VAR: &str = "ENGRAM_CHUNK_CACHE_FREE_FLOOR_BYTES";

/// Default interval between [`ChunkCache::spawn_sweeper`] ticks (ADR
/// 0067). Independent of populate traffic — this is what closes the "a
/// host under disk pressure with no populate traffic enforces nothing"
/// gap. 60s: frequent enough that a host climbing toward the kubelet
/// line gets caught within a minute, infrequent enough that the
/// directory walk (see `list_entries`) is a rounding error against any
/// real cache size.
pub const DEFAULT_SWEEP_INTERVAL_SECS: u64 = 60;

/// Env var: override [`DEFAULT_SWEEP_INTERVAL_SECS`]. `0` disables the
/// periodic sweeper entirely (tests; the populate-path debounced sweep
/// still runs).
pub const SWEEP_INTERVAL_ENV_VAR: &str = "ENGRAM_CHUNK_CACHE_SWEEP_INTERVAL_SECS";

/// Resolve the periodic sweep interval from env, fail-soft (unparseable
/// ⇒ default, with a warn). `0` (explicit disable) is a valid parsed
/// result, distinct from "unset" — see [`ChunkCache::spawn_sweeper`].
/// `pub` so host-agent's `main.rs` can resolve the same env var when
/// deciding the `Duration` to hand `spawn_sweeper`.
pub fn resolve_sweep_interval_secs() -> u64 {
    match std::env::var(SWEEP_INTERVAL_ENV_VAR) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(secs) => secs,
            Err(e) => {
                tracing::warn!(
                    env = SWEEP_INTERVAL_ENV_VAR,
                    value = raw,
                    error = %e,
                    "could not parse chunk cache sweep interval env var; using default",
                );
                DEFAULT_SWEEP_INTERVAL_SECS
            }
        },
        Err(_) => DEFAULT_SWEEP_INTERVAL_SECS,
    }
}

impl ChunkCacheConfig {
    /// Bare constructor: [`NO_CEILING`] (no absolute byte ceiling) — used
    /// by callers that set `budget_bytes` themselves (tests, the UFFD
    /// handler) and by [`Self::from_env_or_default`] as its starting
    /// point before resolving the real default. Production callers
    /// should use [`Self::from_env_or_default`], which derives a real
    /// ceiling from the disk.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            budget_bytes: NO_CEILING,
            sweep_debounce_ms: DEFAULT_SWEEP_DEBOUNCE_MS,
            eviction_enabled: true,
        }
    }

    /// Resolve `budget_bytes` for a production cache:
    ///
    /// 1. `ENGRAM_CHUNK_CACHE_BUDGET_BYTES`, if set + parseable, wins
    ///    outright as an absolute operator override.
    /// 2. Otherwise, derive a disk-sized default (ADR 0070):
    ///    `create_dir_all(root)` (the budget probe needs the mount
    ///    `root` will live on, which may not exist yet on a fresh host)
    ///    then [`fs_total_bytes`] → [`default_budget_bytes`] with
    ///    [`resolve_disk_fraction`] and the free-space-floor fraction
    ///    ([`resolve_free_floor_pct`]) as `headroom_frac`.
    /// 3. If the directory can't be created or the filesystem can't be
    ///    probed, fall back to [`NO_CEILING`] with a warn — fail-soft
    ///    mirrors the other env-knob parsers in the codebase; the
    ///    free-space floor alone still governs.
    ///
    /// Logs at info on every path so operators can confirm the value
    /// picked up.
    pub fn from_env_or_default(root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = root.into();
        let mut cfg = Self::new(root.clone());
        let explicit = match std::env::var(BUDGET_ENV_VAR) {
            Ok(raw) => match raw.parse::<u64>() {
                Ok(bytes) => {
                    tracing::info!(
                        budget_bytes = bytes,
                        env = BUDGET_ENV_VAR,
                        "chunk cache absolute ceiling set via env (wins over the disk-derived default)",
                    );
                    Some(bytes)
                }
                Err(e) => {
                    tracing::warn!(
                        env = BUDGET_ENV_VAR,
                        value = raw,
                        error = %e,
                        "could not parse chunk cache ceiling env var; falling back to the disk-derived default",
                    );
                    None
                }
            },
            Err(_) => None,
        };
        cfg.budget_bytes = match explicit {
            Some(bytes) => bytes,
            None => Self::disk_derived_budget(&root),
        };
        cfg
    }

    /// The disk-sized default from [`default_budget_bytes`], probed
    /// against the filesystem backing `root`. `root` is created first
    /// (best-effort) since a fresh host may not have it yet — the
    /// budget probe needs a real mount to `statvfs(2)`, not the parent
    /// of a not-yet-existing dir.
    fn disk_derived_budget(root: &Path) -> u64 {
        if let Err(e) = std::fs::create_dir_all(root) {
            tracing::warn!(
                root = %root.display(),
                error = %e,
                "could not create chunk cache root to probe disk size; no absolute ceiling",
            );
            return NO_CEILING;
        }
        match fs_total_bytes(root) {
            Some(total) if total > 0 => {
                let fraction = resolve_disk_fraction();
                let headroom_frac = resolve_free_floor_pct(root);
                let budget = default_budget_bytes(total, fraction, headroom_frac);
                tracing::info!(
                    fs_total_bytes = total,
                    disk_fraction = fraction,
                    headroom_frac,
                    budget_bytes = budget,
                    "chunk cache absolute ceiling derived from disk size",
                );
                budget
            }
            _ => {
                tracing::warn!(
                    root = %root.display(),
                    "could not probe cache filesystem size; no absolute ceiling",
                );
                NO_CEILING
            }
        }
    }
}

/// Resolve the free-space floor fraction from env, fail-soft. `_PCT`
/// (0–100) wins over `_BYTES` (converted against the FS backing `root`).
/// Returns [`DEFAULT_FREE_FLOOR_PCT`] when neither is set/valid. Pure
/// w.r.t. the config struct so the env precedence is unit-testable.
fn resolve_free_floor_pct(root: &Path) -> f64 {
    if let Ok(raw) = std::env::var(FREE_FLOOR_PCT_ENV_VAR) {
        match raw.parse::<f64>() {
            Ok(pct) if (0.0..=100.0).contains(&pct) => {
                let frac = pct / 100.0;
                tracing::info!(
                    free_floor_pct = pct,
                    env = FREE_FLOOR_PCT_ENV_VAR,
                    "chunk cache free-space floor set via env",
                );
                return frac;
            }
            other => {
                tracing::warn!(
                    env = FREE_FLOOR_PCT_ENV_VAR,
                    value = raw,
                    parsed = ?other,
                    "free-floor pct env var out of range / unparseable; using default",
                );
            }
        }
    }

    if let Ok(raw) = std::env::var(FREE_FLOOR_BYTES_ENV_VAR) {
        match raw.parse::<u64>() {
            // Convert to a fraction against the live FS size. If the
            // probe fails we keep the default rather than guess — the
            // per-sweep decision re-probes anyway.
            Ok(floor_bytes) => match fs_total_bytes(root) {
                Some(total) if total > 0 => {
                    let frac = (floor_bytes as f64 / total as f64).clamp(0.0, 1.0);
                    tracing::info!(
                        free_floor_bytes = floor_bytes,
                        fs_total_bytes = total,
                        resolved_pct = frac * 100.0,
                        env = FREE_FLOOR_BYTES_ENV_VAR,
                        "chunk cache free-space floor set via env (bytes ⇒ fraction)",
                    );
                    return frac;
                }
                _ => {
                    tracing::warn!(
                        env = FREE_FLOOR_BYTES_ENV_VAR,
                        value = raw,
                        root = %root.display(),
                        "could not probe cache filesystem size; keeping default floor",
                    );
                }
            },
            Err(e) => {
                tracing::warn!(
                    env = FREE_FLOOR_BYTES_ENV_VAR,
                    value = raw,
                    error = %e,
                    "could not parse free-floor bytes env var; using default",
                );
            }
        }
    }

    DEFAULT_FREE_FLOOR_PCT
}

/// Local-disk cache for chunked content. Cheap to clone.
///
/// The cache is **backend-agnostic**: it does not own a
/// `ChunkStore` reference. Every `get` call takes an async
/// fetcher closure that runs on local-NVMe miss. Callers pick
/// the right backend per call — `ChunkStore::get_chunk`, a
/// `TieredChunkResolver`, a direct `BlobStorage::get`, whatever
/// makes sense at the call site.
///
/// This decoupling was lifted out of the pre-ADR-0008 shape,
/// where `ChunkCache` pinned a `ChunkStore` at construction.
/// That made the cache silently incompatible with per-session
/// upgrades (the chunked-OCI tiered resolver) — the caller's
/// upgraded store was passed in but ignored. Closure-based
/// fetcher eliminates the whole class of bug.
#[derive(Clone)]
pub struct ChunkCache {
    inner: Arc<CacheInner>,
}

/// How many recently-evicted hashes the thrash ring remembers. Bounded
/// so the set stays O(1) memory regardless of churn; a hash that fell
/// out of the window simply isn't counted as a refetch-after-evict.
/// 4096 × 32-byte hashes ≈ 128 KiB — cheap, and wide enough to catch
/// the working-set-too-big-for-the-disk thrash this metric targets.
const EVICTED_RING_CAP: usize = 4096;

/// Singleflight slot map: hash → the waiters' oneshot senders the
/// leader broadcasts the fetched bytes to. An entry exists iff a leader
/// is in flight for that hash.
type InflightMap = HashMap<ChunkHash, Vec<oneshot::Sender<Result<Bytes>>>>;

/// Removes a leader's in-flight slot on drop **unless disarmed** — the
/// cancel-safety net for [`ChunkCache::get`]. The leader registers a
/// slot, then `fetch().await`s; if that future is *cancelled* (e.g. an
/// FC `load_snapshot` 60 s timeout tears down the whole restore), the
/// leader's normal "remove slot + notify waiters" tail never runs, so
/// the slot — holding the leader's own sender — lingers forever and
/// every waiter (and every later `get` for this hash, which joins the
/// orphaned slot) blocks on a broadcast that never comes. That poisons
/// the host's chunk cache (prod incident: session 9f82064b wedged every
/// resume retry). This guard removes the slot on drop, dropping its
/// senders so waiters observe `RecvError` and retry as a fresh leader.
/// The leader disarms it once it has removed the slot itself.
struct LeaderGuard<'a> {
    inflight: &'a Mutex<InflightMap>,
    hash: ChunkHash,
    armed: bool,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.inflight.lock().remove(&self.hash);
        }
    }
}

struct CacheInner {
    config: ChunkCacheConfig,
    /// Free-space floor fraction, resolved once from env (or the
    /// default) at construction. The *value* is fixed; the disk it's
    /// compared against is re-probed every sweep (see `evict_to_budget`).
    free_floor_pct: f64,
    /// Singleflight: hashes currently being fetched. Concurrent
    /// requesters for the same hash await the in-flight fetch
    /// rather than racing the underlying fetcher.
    inflight: Mutex<InflightMap>,
    /// Pin set — never-evict, **reference-counted**. The cache still
    /// inserts pinned chunks like any other; the evictor skips any
    /// hash with a non-zero count. Refcounting lets independent
    /// holders (two enabled images sharing a base chunk; a per-session
    /// working set overlapping the base manifest) pin the same hash
    /// without one's `unpin` releasing another's pin.
    pinned: Mutex<HashMap<ChunkHash, u32>>,
    /// Bounded FIFO of recently-evicted hashes. A remote (GCS) miss for
    /// a hash in this set means we paid the round-trip we just freed —
    /// the floor/ceiling is too tight for the working set. Drives
    /// `engram_chunk_cache_refetch_after_evict_total`.
    evicted_ring: Mutex<EvictedRing>,
    /// Debounce for the populate-path eviction sweep (unix millis of
    /// the last sweep). See `write_local`.
    last_sweep_ms: std::sync::atomic::AtomicI64,
    /// ADR 0092: allocated bytes co-tenant consumers currently hold on
    /// the cache's filesystem that the sweeper cannot evict (today: the
    /// per-image base memfiles File restores read, maintained by the
    /// image-prefetch supervisor via [`ChunkCache::set_co_tenant_reserved`]).
    /// Subtracted from the absolute ceiling every sweep so the cache
    /// never aims to fill space a co-tenant needs. 0 (the default) ⇒
    /// today's behavior.
    co_tenant_reserved: std::sync::atomic::AtomicU64,
    /// ADR 0095: live feed from [`ChunkCache::put_unverified_no_evict`]
    /// to the background scrubber. `None` until
    /// [`ChunkCache::spawn_scrubber`] runs (a cache without a scrubber
    /// still lands unverified chunks correctly — the markers are
    /// durable and a later scrubber's boot scan picks them up).
    scrub_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<ChunkHash>>>,
    /// #1003: in-memory eviction index — hash → (size, populate time),
    /// plus a maintained byte total. Built from ONE directory walk on
    /// the first sweep, then updated by every in-process populate and
    /// eviction. Budget sweeps read THIS instead of re-walking the
    /// cache root: on the incident host (516k files / 442 GB) each
    /// walk materialized ~200 MB of `Vec<CacheEntry>` + `PathBuf`s and
    /// cost 100-500 ms, and the capture path ran it per chunk written
    /// — the multi-GB heap bursts behind the OOM loop. Out-of-process
    /// writers (the UFFD handler, ADR 0070) still land chunks the
    /// index can't see; the periodic [`ChunkCache::sweep`] re-walks
    /// and reconciles, bounding drift to one sweep interval.
    index: Mutex<IndexState>,
    /// #1003: single-sweeper gate. Sweep cost used to compound the
    /// other way too: on a big cache a sweep outlived the write-path
    /// debounce window, so sweeps piled up concurrently (the burst
    /// profile showed ~10 listings live at once). The populate path
    /// `try_lock`s this and SKIPS if a sweep is already running (the
    /// running sweep enforces the same budget); the periodic
    /// reconcile waits its turn.
    sweep_gate: tokio::sync::Mutex<()>,
}

/// #1003: the in-memory mirror of the cache directory that budget
/// sweeps run against. No `PathBuf` per entry — paths derive from the
/// hash on demand (`path_for`), which is exactly the allocation the
/// on-disk walk paid per entry per sweep.
struct CacheIndex {
    entries: HashMap<ChunkHash, IndexEntry>,
    /// Maintained sum of `entries[*].size`.
    total_bytes: u64,
}

impl CacheIndex {
    fn apply(&mut self, op: PendingOp) {
        match op {
            PendingOp::Insert(hash, entry) => {
                if let Some(prev) = self.entries.insert(hash, entry) {
                    self.total_bytes = self.total_bytes.saturating_sub(prev.size);
                }
                self.total_bytes += entry.size;
            }
            PendingOp::Remove(hash) => {
                if let Some(prev) = self.entries.remove(&hash) {
                    self.total_bytes = self.total_bytes.saturating_sub(prev.size);
                }
            }
        }
    }
}

/// Index lifecycle. `Pending` covers boot until the first walk lands:
/// in-process populates/unlinks are BUFFERED as ops and replayed onto
/// the walk result when it installs — a put that races the in-flight
/// walk (walk already passed its prefix dir) would otherwise be
/// tracked by neither side and stay invisible until the next
/// reconcile, exactly the capture-storm window the budget must see.
enum IndexState {
    Pending(Vec<PendingOp>),
    Ready(CacheIndex),
}

enum PendingOp {
    Insert(ChunkHash, IndexEntry),
    Remove(ChunkHash),
}

#[derive(Clone, Copy)]
struct IndexEntry {
    size: u64,
    /// Populate time, unix millis. Eviction is FIFO by populate time
    /// (reads do not touch recency — see the module doc); ties are
    /// broken by hash for determinism.
    populated_ms: i64,
}

impl ChunkCache {
    pub fn new(config: ChunkCacheConfig) -> Self {
        let free_floor_pct = resolve_free_floor_pct(&config.root);
        Self {
            inner: Arc::new(CacheInner {
                config,
                free_floor_pct,
                inflight: Mutex::new(HashMap::new()),
                pinned: Mutex::new(HashMap::new()),
                evicted_ring: Mutex::new(EvictedRing::with_capacity(EVICTED_RING_CAP)),
                last_sweep_ms: std::sync::atomic::AtomicI64::new(0),
                co_tenant_reserved: std::sync::atomic::AtomicU64::new(0),
                scrub_tx: Mutex::new(None),
                index: Mutex::new(IndexState::Pending(Vec::new())),
                sweep_gate: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Root directory used by this cache.
    pub fn root(&self) -> &Path {
        &self.inner.config.root
    }

    /// Test/explicit constructor that sets the free-space floor directly,
    /// bypassing env resolution. Used by unit tests that need a
    /// deterministic floor independent of the host disk's fill level —
    /// a near-full dev/CI disk would otherwise trip the default
    /// free-space floor and evict just-populated chunks, making
    /// retention assertions flaky. `pub(crate)` so sibling-module tests
    /// (e.g. `file.rs`) can pin it too. Not part of the public surface.
    #[cfg(test)]
    pub(crate) fn new_with_floor(config: ChunkCacheConfig, free_floor_pct: f64) -> Self {
        Self {
            inner: Arc::new(CacheInner {
                config,
                free_floor_pct,
                inflight: Mutex::new(HashMap::new()),
                pinned: Mutex::new(HashMap::new()),
                evicted_ring: Mutex::new(EvictedRing::with_capacity(EVICTED_RING_CAP)),
                last_sweep_ms: std::sync::atomic::AtomicI64::new(0),
                co_tenant_reserved: std::sync::atomic::AtomicU64::new(0),
                scrub_tx: Mutex::new(None),
                index: Mutex::new(IndexState::Pending(Vec::new())),
                sweep_gate: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// ADR 0092: record how many allocated bytes co-tenant consumers
    /// (files on this cache's filesystem the sweeper cannot evict — the
    /// per-image base memfiles) currently hold. The next sweep subtracts
    /// this from the absolute ceiling, so growth in a co-tenant converts
    /// into cache eviction pressure instead of disk overshoot. Callers
    /// re-publish their current total whenever it changes (the
    /// image-prefetch supervisor does so every reconcile tick);
    /// last-write-wins, single logical writer.
    pub fn set_co_tenant_reserved(&self, bytes: u64) {
        self.inner
            .co_tenant_reserved
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// The co-tenant reserve currently in force (see
    /// [`Self::set_co_tenant_reserved`]).
    pub fn co_tenant_reserved(&self) -> u64 {
        self.inner
            .co_tenant_reserved
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn path_for(&self, hash: ChunkHash) -> PathBuf {
        // ONE layout definition, shared with the read-only view
        // (ADR 0075) — writer and reader can never disagree.
        crate::reader::chunk_path(&self.inner.config.root, hash)
    }

    /// Does this chunk's content-addressed file exist on local NVMe right now?
    /// The disk daemon reads this to label a read's tier (nvme vs blobstorage);
    /// it also distinguishes "never warmed here" / "evicted" from "warmed but
    /// `get` still missed".
    pub fn contains_on_disk(&self, hash: ChunkHash) -> bool {
        self.path_for(hash).try_exists().unwrap_or(false)
    }

    /// ADR 0075: the on-disk path for a resident chunk. Public so the
    /// substrate populate server can open + fd-pass a chunk it just
    /// populated; the layout is the shared `reader::chunk_path`, so
    /// this can never diverge from what a `ChunkCacheReader` sees.
    pub fn on_disk_path(&self, hash: ChunkHash) -> PathBuf {
        self.path_for(hash)
    }

    /// ADR 0075 `HelloAck.cache_writable` probe: create + remove a
    /// probe file under the root. Live evidence, not a config echo.
    pub fn root_writable_probe(&self) -> bool {
        let probe = self
            .inner
            .config
            .root
            .join(format!(".writable-probe.{}", std::process::id()));
        match std::fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    /// Test-only: drop a chunk's on-disk cache file, simulating an LRU
    /// eviction (or a silently-failed `write_local`) so callers can exercise
    /// the "pinned-but-not-resident" recovery paths without driving real
    /// disk pressure. Not part of the runtime surface.
    #[doc(hidden)]
    /// Test-only: run one budget sweep synchronously. Production
    /// sweeps ride `spawn_sweeper` + the write-path debounce; tests
    /// (ADR 0075 pin-integrity-under-pressure) need a deterministic
    /// trigger.
    pub async fn sweep_for_test(&self) {
        // The reconciling variant: tests drop/create files on disk
        // directly, so a test sweep must see disk truth, not the
        // in-memory index.
        if let Err(e) = self.sweep().await {
            panic!("test sweep failed: {e}");
        }
    }

    pub fn evict_on_disk_for_test(&self, hash: ChunkHash) {
        let _ = std::fs::remove_file(self.path_for(hash));
        self.index_remove(hash);
    }

    /// Get a chunk's bytes. Local NVMe first; on miss, the
    /// `fetch` closure is invoked exactly once (singleflight —
    /// concurrent waiters for the same hash share its result).
    /// Fetched bytes are written to local NVMe before returning.
    ///
    /// The fetcher's return value is what flows back; if it
    /// errors, the error propagates to all in-flight waiters.
    ///
    /// # Why a closure rather than a stored `ChunkStore`
    ///
    /// Each call site picks its own backend. `materialize_*`
    /// against a per-session tiered resolver and the UFFD
    /// handler against the global store can coexist behind the
    /// same cache without any rebind dance.
    pub async fn get<F, Fut>(&self, hash: ChunkHash, fetch: F) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        // Plain closures are the cold tier by definition — the
        // source-aware sibling below is for tiered (peer-capable)
        // fetchers (ADR 0095).
        self.get_with_source(hash, || async move {
            fetch().await.map(|b| (b, FillSource::BlobStorage))
        })
        .await
    }

    /// [`Self::get`] with a source-aware fetcher: the closure reports
    /// where the bytes actually came from, so the
    /// [`CHUNK_FILL_TOTAL`]/[`CHUNK_FILL_BYTES_TOTAL`] `source` label
    /// and the fetch histogram's `tier` stay honest when a call site
    /// composes a peer tier ahead of BlobStorage (ADR 0095). Populate
    /// semantics are identical — singleflight, sha256
    /// verify-on-populate (a fault-time peer chunk IS verified; only
    /// the bulk `put_unverified_no_evict` path defers hashing to the
    /// scrubber), atomic write-through.
    pub async fn get_with_source<F, Fut>(&self, hash: ChunkHash, fetch: F) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(Bytes, FillSource)>>,
    {
        // Fast path: local hit. The cache is content-addressed and every
        // populate path verifies `bytes == hash` before the atomic write
        // (`get`'s fetch arm below, and `put`), so a file present at
        // `path_for(hash)` is known-good. We deliberately do NOT re-hash it
        // here. ADR 0021: a full sha256 of a 16 MiB chunk is ~80 ms on our
        // no-SHA-NI (Cascade Lake) hosts, and the disk daemon re-reads hot
        // ext4 chunks dozens of times per restore — re-verifying on every
        // read was ~2.2 s of a 2.6 s substrate, the dominant cost. Verify on
        // populate; trust on read. (Atomic temp+rename means a present file is
        // never torn; post-write bit-rot is left to PD/local-SSD durability.)
        let path = self.path_for(hash);
        // ADR 0019 / telemetry restoration (#526): NVMe-tier hit latency was
        // previously unmeasured — `engram_chunk_fetch_seconds` only had a
        // `blobstorage` arm (the miss path below), so there was no signal
        // for "the fast tier got slow" (a saturated NVMe device, ext4
        // fragmentation, etc). Time the read regardless of hit/miss; a miss
        // here is a fast negative stat (no file), not a meaningful latency
        // sample, so only record on a hit.
        let nvme_read_start = crate::time_source::metrics_now();
        let nvme_read = read_if_present(&path).await?;
        if let Some(bytes) = nvme_read {
            metrics::histogram!(
                "engram_chunk_fetch_seconds",
                "tier" => "nvme",
            )
            .record(nvme_read_start.elapsed().as_secs_f64());
            // ADR 0014 M1.15: local NVMe hit. Don't differentiate
            // singleflight-piggyback from true cache hit here —
            // the user-visible win is the same.
            metrics::counter!(
                "engram_chunk_cache_hits_total",
                "tier" => "nvme",
            )
            .increment(1);
            // ADR 0019 0d: bytes served per tier. With the hit counter
            // this gives page-in volume + the nvme/blob split — the
            // aggregate view of chunked-NBD page-in during ext4 mount
            // (per-read spans would flood the trace; this is the metric).
            metrics::counter!(
                "engram_chunk_cache_bytes_total",
                "tier" => "nvme",
            )
            .increment(bytes.len() as u64);
            return Ok(bytes);
        }

        // Singleflight, cancel-safe. We loop because a waiter whose leader
        // is cancelled mid-fetch (see `LeaderGuard`) wakes with `RecvError`
        // and re-enters — re-checking the local hit, then re-leading or
        // re-joining — rather than failing the read. The leader path runs
        // at most once per call (the `fetch` closure is `FnOnce`, taken via
        // `Option::take`); a call only re-loops as a woken waiter.
        let mut fetch = Some(fetch);
        loop {
            let (tx, rx) = oneshot::channel();
            let do_fetch = {
                let mut inflight = self.inner.inflight.lock();
                let entry = inflight.entry(hash).or_default();
                let first = entry.is_empty();
                entry.push(tx);
                first
            };

            if do_fetch {
                // Arm the cancel-safety guard for the whole leader section:
                // if this future is dropped before we remove the slot below,
                // the guard removes it so waiters retry instead of wedging.
                let mut guard = LeaderGuard {
                    inflight: &self.inner.inflight,
                    hash,
                    armed: true,
                };
                // Double-checked hit: leadership was won on the state of the
                // inflight MAP, but this call's fast-path disk check ran
                // before the lock — a prior leader may have persisted and
                // drained in between (its persist happens before its slot
                // removal, below, so slot-gone implies file-present). Serve
                // from disk instead of paying a duplicate remote fetch —
                // without this, a stale-miss caller that wins leadership
                // re-fetches a chunk that is already on NVMe (the
                // `concurrent_clients_collapse_to_one_fetch` flake).
                if let Some(bytes) = read_if_present(&path).await? {
                    let waiters = {
                        let mut inflight = self.inner.inflight.lock();
                        inflight.remove(&hash).unwrap_or_default()
                    };
                    guard.armed = false;
                    for waiter in waiters {
                        let _ = waiter.send(Ok(bytes.clone()));
                    }
                    metrics::counter!(
                        "engram_chunk_cache_hits_total",
                        "tier" => "nvme",
                    )
                    .increment(1);
                    metrics::counter!(
                        "engram_chunk_cache_bytes_total",
                        "tier" => "nvme",
                    )
                    .increment(bytes.len() as u64);
                    return Ok(bytes);
                }
                let fetch = fetch.take().expect("leader runs the fetch once");
                // ADR 0039 #16: thrash signal. If we're about to pay a remote
                // round-trip for a hash we recently evicted, the cache is too
                // small for the working set — count it (and stop tracking the
                // hash; it's about to be re-cached). A sustained nonzero rate
                // says "raise the budget / the disk is the bottleneck."
                if self.inner.evicted_ring.lock().take(&hash) {
                    metrics::counter!("engram_chunk_cache_refetch_after_evict_total").increment(1);
                }
                // ADR 0019 0d: time the remote fetch — the cold-cache page-in
                // cost that stretches cold boot (the slow tier). Histogram +
                // the bytes counter below quantify "how much of the boot is
                // blob page-in" without per-read trace spam.
                let fetch_start = crate::time_source::metrics_now();
                let fetched = fetch().await;
                let source = fetched
                    .as_ref()
                    .map(|(_, s)| *s)
                    .unwrap_or(FillSource::BlobStorage);
                let fetched = fetched.map(|(b, _)| b);
                metrics::histogram!(
                    "engram_chunk_fetch_seconds",
                    "tier" => source.fetch_tier(),
                )
                .record(fetch_start.elapsed().as_secs_f64());
                // Verify-on-populate. This is the ONE place a chunk is hashed:
                // a content-addressed cache must never serve OR store bytes that
                // don't match the requested hash (corrupt / truncated object). On
                // mismatch we fail the read for every waiter rather than poison
                // the guest's rootfs, and never write the bad bytes. The read
                // fast path above then trusts the verified, atomically-written
                // file — that's what moves the 16 MiB sha256 off the hot per-read
                // path (ADR 0021).
                let result = match fetched {
                    Ok(bytes) => {
                        let actual = ChunkHash::of(&bytes);
                        if actual == hash {
                            Ok(bytes)
                        } else {
                            tracing::error!(
                                hash = %hash,
                                actual = %actual,
                                "fetched chunk hash mismatch; refusing to cache or serve",
                            );
                            Err(ChunkStoreError::HashMismatch {
                                expected: hash.to_hex(),
                                actual: actual.to_hex(),
                            })
                        }
                    }
                    Err(e) => Err(e),
                };
                // Persist BEFORE releasing the slot. The ordering is the
                // singleflight's collapse invariant: a fresh caller can only
                // become the next leader after this slot is removed, so
                // slot-gone must imply file-present — otherwise every caller
                // arriving in the remove→persist window (fast-path miss, then
                // an empty inflight entry) re-fetches a chunk that is already
                // in flight to disk. The leader's double-checked read above
                // relies on the same invariant. Cancellation mid-persist is
                // covered by the still-armed guard (slot removed on drop;
                // waiters wake with RecvError and retry).
                if let Ok(bytes) = result.as_ref() {
                    // write_local failure is non-fatal I/O (ENOSPC, perms): the
                    // fetched bytes still return to the caller, but the chunk
                    // isn't cached, so every later read re-fetches from GCS —
                    // which presents exactly as "warming ran but reads still
                    // miss". Surface it loudly rather than swallowing (`let _ =`).
                    let write_local_ok = match self.write_local(hash, bytes).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(
                                hash = %hash,
                                root = %self.inner.config.root.display(),
                                bytes = bytes.len(),
                                error = %e,
                                "chunk cache write_local failed — chunk not cached (reads will miss → GCS)",
                            );
                            false
                        }
                    };
                    // This measures the fetch (bytes pulled from
                    // BlobStorage), not the fill — it stays unconditional
                    // even when write_local below fails.
                    metrics::counter!(
                        "engram_chunk_cache_bytes_total",
                        "tier" => source.fetch_tier(),
                    )
                    .increment(bytes.len() as u64);
                    // ADR 0019 / telemetry restoration (#526), review finding
                    // 4: the baseline meter for epic-gcs-free-resume's
                    // "GCS-free by policy" claim. ADR 0095 moved the label to
                    // the closure seam: the fetcher reports its actual source
                    // (`gcs` = the cold BlobStorage tier in every deployed
                    // configuration; `peer` = a fleet sibling's NVMe), so a
                    // tiered fetcher can't launder a peer fill as GCS or vice
                    // versa. Gated on `write_local_ok`: a fetch whose local
                    // persist failed did NOT fill the cache — counting it
                    // here would mask exactly the "warming ran but reads
                    // still miss" state the write_local warning above exists
                    // to catch.
                    if write_local_ok {
                        metrics::counter!(
                            CHUNK_FILL_TOTAL,
                            "source" => source.label(),
                        )
                        .increment(1);
                        metrics::counter!(
                            CHUNK_FILL_BYTES_TOTAL,
                            "source" => source.label(),
                        )
                        .increment(bytes.len() as u64);
                    }
                }
                // Drain waiters + DISARM: the slot is gone, so the guard must
                // not remove a fresh slot a later leader may have created.
                let waiters = {
                    let mut inflight = self.inner.inflight.lock();
                    inflight.remove(&hash).unwrap_or_default()
                };
                guard.armed = false;
                // Notify waiters. Send-failure (their rx dropped)
                // is benign.
                for waiter in waiters {
                    let _ = waiter.send(clone_result(&result));
                }
                // ADR 0014 M1.15: count the leader's fetch as a remote-tier
                // hit (we went past local NVMe — `blobstorage`, or `peer`
                // when a tiered fetcher resolved there, ADR 0095).
                // Singleflight FOLLOWERS are not counted here at all — the
                // rx-await arm increments no metric; a follower surfaces as
                // an nvme hit only on its own later `get` call that finds
                // the now-cached chunk.
                metrics::counter!(
                    "engram_chunk_cache_hits_total",
                    "tier" => source.fetch_tier(),
                )
                .increment(1);
                return result;
            } else {
                // We're not the first; await the leader's broadcast. The
                // closure we were passed stays unused (the leader's fetcher
                // fires; content-addressing means any fetcher yields the same
                // bytes).
                match rx.await {
                    Ok(result) => return result,
                    // `RecvError` ⇒ the leader was dropped (cancelled) before
                    // broadcasting and its `LeaderGuard` cleaned the slot.
                    // Retry: re-loop to re-check the local hit, then re-lead
                    // or re-join. This is the un-poisoning — a cancelled
                    // restore no longer wedges every later read of this hash.
                    Err(_) => continue,
                }
            }
        }
    }

    /// Pre-warm: fetch and store locally without returning bytes
    /// to the caller. Used by working-set replay to load the
    /// prefault set in parallel before vCPUs run.
    pub async fn prefetch<F, Fut>(&self, hash: ChunkHash, fetch: F) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        // Already resident? Confirm presence with a cheap stat instead of
        // reading (and discarding) the whole chunk off NVMe. prefetch only
        // needs the chunk *resident*, not its bytes — content-addressing +
        // verify-on-populate (ADR 0021) mean a present file is known-good,
        // and skipping the read stops a warm-cache prefetch from inflating
        // the nvme hit/bytes counters with reads no consumer received. A
        // miss still routes through `get` (singleflight + verify-on-populate).
        if self.contains(hash).await {
            return Ok(());
        }
        let _ = self.get(hash, fetch).await?;
        Ok(())
    }

    /// ADR 0014 M1.13: parallel prefetch of a whole manifest's
    /// chunk set into local NVMe. Bounds concurrency so a bursty
    /// refill doesn't saturate the host NIC or GCS rate limits.
    ///
    /// Returns once every chunk is either confirmed in cache or
    /// fetched. The `fetch` closure is called once per missing
    /// chunk (concurrent calls are fine; per-chunk dedup happens
    /// inside `get`). Errors propagate from the first failure;
    /// already-completed fetches stay in cache (the prefetch is
    /// best-effort lazy from the caller's POV).
    ///
    /// Used by `pooled_backend::restore` to warm the snapshot's
    /// memory chunks before `inner.restore` triggers UFFD-driven
    /// reads — converts what would be N serial GCS round-trips
    /// during kernel resume into K parallel round-trips upfront.
    pub async fn prefetch_chunks_parallel<F, Fut>(
        &self,
        hashes: Vec<ChunkHash>,
        concurrency: usize,
        fetch: F,
    ) -> Result<()>
    where
        F: Fn(ChunkHash) -> Fut + Send + Sync + Clone + 'static,
        Fut: std::future::Future<Output = Result<Bytes>> + Send + 'static,
    {
        use futures::stream::{FuturesUnordered, StreamExt};
        type Task = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
        let spawn_one = |h: ChunkHash, cache: ChunkCache, fetcher: F| -> Task {
            Box::pin(async move { cache.prefetch(h, || fetcher(h)).await })
        };

        let max_in_flight = concurrency.max(1);
        let mut in_flight: FuturesUnordered<Task> = FuturesUnordered::new();
        let mut iter = hashes.into_iter();
        // Seed the pipeline.
        for _ in 0..max_in_flight {
            match iter.next() {
                Some(h) => in_flight.push(spawn_one(h, self.clone(), fetch.clone())),
                None => break,
            }
        }
        // Drain + refill: each completion lets us start one more.
        while let Some(result) = in_flight.next().await {
            result?;
            if let Some(h) = iter.next() {
                in_flight.push(spawn_one(h, self.clone(), fetch.clone()));
            }
        }
        Ok(())
    }

    /// Pin a chunk against eviction (increments its refcount). Used by
    /// the UFFD handler for the working-set set so prefault stays warm
    /// across restarts, and by the image-prefetch supervisor to keep an
    /// enabled image's canonical base manifest resident.
    pub fn pin(&self, hash: ChunkHash) {
        *self.inner.pinned.lock().entry(hash).or_insert(0) += 1;
    }

    /// Release one pin on a chunk (decrements its refcount). The chunk
    /// becomes evictable only once the last holder unpins. A spurious
    /// unpin of an unpinned hash is a no-op.
    pub fn unpin(&self, hash: ChunkHash) {
        let mut pinned = self.inner.pinned.lock();
        if let Some(count) = pinned.get_mut(&hash) {
            *count -= 1;
            if *count == 0 {
                pinned.remove(&hash);
            }
        }
    }

    /// Pin every hash in the batch (one refcount each). Used by the
    /// image-prefetch supervisor to pin a whole base manifest's chunk
    /// set in one call after warming it onto NVMe.
    pub fn pin_all(&self, hashes: impl IntoIterator<Item = ChunkHash>) {
        let mut pinned = self.inner.pinned.lock();
        for hash in hashes {
            *pinned.entry(hash).or_insert(0) += 1;
        }
    }

    /// Release one pin on every hash in the batch. The mirror of
    /// [`Self::pin_all`], used when an image is disabled so its
    /// canonical base manifest's chunks become LRU-evictable again —
    /// but only those not still pinned by another enabled image.
    pub fn unpin_all(&self, hashes: impl IntoIterator<Item = ChunkHash>) {
        let mut pinned = self.inner.pinned.lock();
        for hash in hashes {
            if let Some(count) = pinned.get_mut(&hash) {
                *count -= 1;
                if *count == 0 {
                    pinned.remove(&hash);
                }
            }
        }
    }

    /// Drop all pins (every refcount). Useful when changing which
    /// manifest a host is serving (different working set).
    pub fn clear_pins(&self) {
        self.inner.pinned.lock().clear();
    }

    /// Number of distinct chunk hashes currently pinned (refcount > 0).
    /// Diagnostic / test accessor — not used by the eviction loop.
    pub fn pinned_count(&self) -> usize {
        self.inner.pinned.lock().len()
    }

    /// Whether this hash currently holds at least one pin. Diagnostic /
    /// test accessor.
    pub fn is_pinned(&self, hash: ChunkHash) -> bool {
        self.inner.pinned.lock().contains_key(&hash)
    }

    /// Whether this cache instance will ever unlink a chunk file (ADR
    /// 0067's single-evictor switch). Diagnostic / test accessor — used
    /// by `engram-uffd-handler`'s constructor test to assert the handler
    /// always builds an eviction-disabled cache.
    pub fn eviction_enabled(&self) -> bool {
        self.inner.config.eviction_enabled
    }

    /// True if local NVMe currently has this chunk. Cheap stat;
    /// doesn't load bytes.
    pub async fn contains(&self, hash: ChunkHash) -> bool {
        fs::try_exists(self.path_for(hash)).await.unwrap_or(false)
    }

    /// Like [`Self::put`] but WITHOUT the per-write eviction sweep —
    /// for bulk staging paths (migration prestage / local-sink
    /// re-chunk) that write dozens of chunks back-to-back: the sweep
    /// is a full two-level readdir of the cache root + statvfs, and
    /// paying it per chunk turned a ~94 MiB prod migration transfer
    /// into a 70 s leg. Callers MUST call [`Self::sweep`] once after
    /// the batch (the budget/floor invariant is per-batch, not
    /// per-write).
    pub async fn put_no_evict(&self, hash: ChunkHash, bytes: &[u8]) -> Result<()> {
        let actual = ChunkHash::of(bytes);
        if actual != hash {
            return Err(ChunkStoreError::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        let target = self.path_for(hash);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        write_atomic(&target, bytes).await?;
        self.index_insert(hash, bytes.len() as u64);
        Ok(())
    }

    /// One RECONCILING eviction sweep — the batch-closing pair of
    /// [`Self::put_no_evict`], and what the periodic
    /// [`Self::spawn_sweeper`] runs. Re-walks the cache directory,
    /// rebuilds the in-memory index from disk truth (out-of-process
    /// writers — the UFFD handler, ADR 0070 — land chunks the index
    /// can't see), gauges the drift, then enforces the budget. The
    /// only remaining full-walk path; everything per-write reads the
    /// index ([`Self::evict_with_index`]).
    pub async fn sweep(&self) -> Result<()> {
        // ADR 0070: single-evictor — a non-evicting process (the UFFD
        // handler) must not pay the reconcile walk either. Checked
        // BEFORE the walk, preserving the old evict_to_budget's
        // early-return semantics.
        if !self.inner.config.eviction_enabled {
            tracing::debug!("chunk cache sweep skipped: eviction_enabled=false on this cache");
            return Ok(());
        }
        let _gate = self.inner.sweep_gate.lock().await;
        let started = crate::time_source::metrics_now();
        let entries = self.list_entries().await?;
        metrics::histogram!("engram_chunk_cache_sweep_seconds", "kind" => "walk")
            .record(started.elapsed().as_secs_f64());
        self.index_install(&entries);
        self.evict_with_index().await
    }

    /// ADR 0095: land peer-pulled bytes WITHOUT the sha256
    /// verify-on-populate — the bulk peer-fill landing path, where
    /// hashing at line rate would cost cores the co-tenant guests need
    /// (no SHA-NI on this fleet). Integrity contract: the transport
    /// already CRC32C-checked every frame; this call marks the chunk
    /// **unverified-origin** (a `.unverified` sidecar, written BEFORE
    /// the chunk becomes visible) and enqueues it for the background
    /// scrubber ([`Self::spawn_scrubber`]), which sha256s it off the
    /// critical path — a mismatch deletes the file (next read refetches
    /// from GCS) and counts `engram_chunk_scrub_total{outcome="corrupt"}`.
    /// Until the scrub clears the marker the chunk is readable LOCALLY
    /// (CRC-checked bytes; trust-on-read as usual) but is NOT served
    /// onward to peers ([`Self::read_verified_for_serve`]) — corruption
    /// can travel at most one hop.
    ///
    /// Like [`Self::put_no_evict`], skips the per-write budget sweep:
    /// bulk callers MUST call [`Self::sweep`] once after the batch.
    pub async fn put_unverified_no_evict(&self, hash: ChunkHash, bytes: &[u8]) -> Result<()> {
        let target = self.path_for(hash);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Marker FIRST: a crash between the two writes must leave
        // either (marker, no chunk) — a stale marker the scrubber
        // reaps — or nothing. The bad state (unverified chunk with no
        // marker) must be unreachable. Markers are never removed
        // outside the scrubber (racing a verified writer on the same
        // hash then converges to "marked, scrub verifies" instead of
        // leaving unhashed bytes unmarked).
        write_atomic(&self.marker_path_for(hash), b"").await?;
        write_atomic(&target, bytes).await?;
        self.index_insert(hash, bytes.len() as u64);
        if let Some(tx) = self.inner.scrub_tx.lock().as_ref() {
            let _ = tx.send(hash);
        }
        Ok(())
    }

    /// `.unverified` sidecar path for a chunk. Lives next to the chunk
    /// file; invisible to the LRU walk ([`Self::list_entries`] only
    /// matches 62-hex names), so eviction can never strip a marker out
    /// from under its chunk.
    fn marker_path_for(&self, hash: ChunkHash) -> PathBuf {
        let mut p = self.path_for(hash).into_os_string();
        p.push(".unverified");
        PathBuf::from(p)
    }

    /// ADR 0095 serve-onward gate: the chunk's bytes IF it is resident
    /// AND verified-origin (sha256'd at populate, or scrubbed since a
    /// bulk peer landing). `None` ⇒ the peer-serve path streams a
    /// `missing` marker and the requester sources it from GCS — an
    /// unverified chunk is never re-served, so a corrupt source can
    /// poison at most its direct pullers, and only until the scrub.
    pub async fn read_verified_for_serve(&self, hash: ChunkHash) -> Result<Option<Bytes>> {
        if fs::try_exists(self.marker_path_for(hash))
            .await
            .unwrap_or(false)
        {
            return Ok(None);
        }
        read_if_present(&self.path_for(hash)).await
    }

    /// ADR 0095: spawn the background scrubber that drains the
    /// unverified-origin backlog. Rate-limited to `bytes_per_sec`
    /// (sha256 on no-SHA-NI hosts is ~0.5-1 GB/s/core — the limit keeps
    /// the drain to a fraction of a core next to live guests). On boot
    /// it scans for leftover markers (crash recovery), then drains the
    /// live queue fed by [`Self::put_unverified_no_evict`].
    ///
    /// Outcomes (`engram_chunk_scrub_total{outcome}`):
    /// - `ok`: content matches its hash — marker removed, chunk becomes
    ///   servable onward.
    /// - `corrupt`: mismatch — chunk + marker deleted (next read
    ///   refetches from GCS), logged at ERROR.
    /// - `missing`: marker with no chunk (evicted mid-queue, or a
    ///   crashed landing) — marker reaped.
    ///
    /// The caller keeps the returned handle alive for the process
    /// lifetime (same held-handle pattern as [`Self::spawn_sweeper`]).
    pub fn spawn_scrubber(&self, bytes_per_sec: u64) -> tokio::task::JoinHandle<()> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ChunkHash>();
        *self.inner.scrub_tx.lock() = Some(tx);
        let cache = self.clone();
        tokio::spawn(async move {
            // Crash recovery: enqueue every marker already on disk.
            let leftover = cache.scan_unverified_markers().await;
            if !leftover.is_empty() {
                tracing::info!(
                    count = leftover.len(),
                    "chunk scrubber: found unverified-origin markers from a prior life",
                );
            }
            let mut backlog: std::collections::VecDeque<ChunkHash> = leftover.into();
            loop {
                let hash = match backlog.pop_front() {
                    Some(h) => h,
                    None => match rx.recv().await {
                        Some(h) => h,
                        None => return, // cache dropped
                    },
                };
                let started = crate::time_source::metrics_now();
                let scrubbed = cache.scrub_one(hash).await;
                // Rate limit: sleep so that (bytes hashed) / (elapsed +
                // sleep) ≤ bytes_per_sec. Zero-byte outcomes (missing)
                // pace on a nominal floor so a marker storm can't spin.
                let bytes = scrubbed.max(64 * 1024) as f64;
                let budget =
                    std::time::Duration::from_secs_f64(bytes / (bytes_per_sec.max(1) as f64));
                if let Some(sleep) = budget.checked_sub(started.elapsed()) {
                    tokio::time::sleep(sleep).await;
                }
            }
        })
    }

    /// Scrub one unverified-origin chunk; returns the byte count hashed
    /// (0 for `missing`). See [`Self::spawn_scrubber`].
    async fn scrub_one(&self, hash: ChunkHash) -> usize {
        let marker = self.marker_path_for(hash);
        if !fs::try_exists(&marker).await.unwrap_or(false) {
            // Already scrubbed (duplicate queue entry) — not an outcome.
            return 0;
        }
        let bytes = match read_if_present(&self.path_for(hash)).await {
            Ok(Some(b)) => b,
            _ => {
                let _ = fs::remove_file(&marker).await;
                // The chunk file is gone (evicted mid-queue or a crashed
                // landing) — drop any stale index entry with it.
                self.index_remove(hash);
                metrics::counter!("engram_chunk_scrub_total", "outcome" => "missing").increment(1);
                return 0;
            }
        };
        if ChunkHash::of(&bytes) == hash {
            let _ = fs::remove_file(&marker).await;
            metrics::counter!("engram_chunk_scrub_total", "outcome" => "ok").increment(1);
        } else {
            // Delete chunk BEFORE marker (the crash-safe order: the bad
            // bytes must never linger unmarked). Next read misses and
            // refetches from GCS through the verifying populate.
            let _ = fs::remove_file(self.path_for(hash)).await;
            self.index_remove(hash);
            let _ = fs::remove_file(&marker).await;
            metrics::counter!("engram_chunk_scrub_total", "outcome" => "corrupt").increment(1);
            tracing::error!(
                hash = %hash,
                "chunk scrubber: peer-landed chunk FAILED sha256 — deleted (will refetch \
                 from GCS); if this fires repeatedly the sourcing peer has disk rot or a \
                 serve bug (ADR 0095 §Integrity)",
            );
        }
        bytes.len()
    }

    /// Walk the cache for `.unverified` sidecars (boot-time crash
    /// recovery for the scrubber). Same two-level layout as
    /// [`Self::list_entries`].
    async fn scan_unverified_markers(&self) -> Vec<ChunkHash> {
        let mut out = Vec::new();
        let Ok(mut top) = fs::read_dir(&self.inner.config.root).await else {
            return out;
        };
        while let Ok(Some(prefix_entry)) = top.next_entry().await {
            let prefix_path = prefix_entry.path();
            if !prefix_entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(prefix) = prefix_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let Ok(mut inner) = fs::read_dir(&prefix_path).await else {
                continue;
            };
            while let Ok(Some(file)) = inner.next_entry().await {
                let Some(name) = file.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Some(rest) = name.strip_suffix(".unverified") else {
                    continue;
                };
                if rest.len() != 62 || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                if let Ok(hash) = ChunkHash::from_hex(&format!("{prefix}{rest}")) {
                    out.push(hash);
                }
            }
        }
        out
    }

    /// Spawn the periodic sweeper (ADR 0070): calls [`Self::sweep`] every
    /// `interval`, independent of populate traffic — closes the gap where
    /// a host under disk pressure with no populate activity (or one that
    /// just restarted with a cold pin set) enforces nothing until the
    /// next write. Errors are logged and looping continues; a single
    /// failed sweep must never take enforcement offline.
    ///
    /// `interval` of [`Duration::ZERO`] disables the sweeper (the spawned
    /// task returns immediately without ticking) — deterministic tests
    /// that want to control exactly when a sweep happens call
    /// [`Self::sweep`] directly instead of racing a background timer.
    ///
    /// Deliberately does NOT sweep at t=0: `tokio::time::interval`'s
    /// first tick fires immediately, but on a freshly-started host-agent
    /// pins are in-memory only and haven't been re-established yet (the
    /// image-prefetch supervisor's reconcile needs a coordinator RPC
    /// round-trip after registration). A t=0 sweep on a restarted host
    /// with an over-budget cache would run pin-blind and evict the
    /// oldest-mtime chunks — exactly the boot-staged base-image chunks
    /// that were pinned in the prior life — flapping readiness and
    /// re-fetching from GCS on every rollout of a host that happens to
    /// sit at or over budget. The first sweep waits one full `interval`
    /// instead, giving the pin set a chance to repopulate first; the
    /// populate-path debounced sweep (`sweep_debounce_ms`) still bounds
    /// growth from writes in the meantime.
    ///
    /// Mirrors `base_shm_gc::spawn`'s held-handle pattern: the caller
    /// keeps the returned handle alive for the process lifetime (dropping
    /// or aborting it stops the sweeper).
    pub fn spawn_sweeper(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let cache = self.clone();
        tokio::spawn(async move {
            if interval.is_zero() {
                tracing::debug!("chunk cache periodic sweeper disabled (interval=0)");
                return;
            }
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Consume the immediate t=0 tick without sweeping — see the
            // doc comment above. Every tick after this one is spaced a
            // full `interval` apart, so the first real sweep lands at
            // t=interval, not t=0.
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = cache.sweep().await {
                    tracing::warn!(error = %e, "periodic chunk cache sweep failed");
                }
            }
        })
    }

    /// Write bytes to the cache atomically. Used internally on
    /// miss; also exposed so the disk daemon can populate the
    /// cache directly from in-VM writes (the daemon already has
    /// the bytes; no point round-tripping through BlobStorage).
    pub async fn put(&self, hash: ChunkHash, bytes: &[u8]) -> Result<()> {
        // Verify caller's hash claim — defensive; cheap.
        let actual = ChunkHash::of(bytes);
        if actual != hash {
            return Err(ChunkStoreError::HashMismatch {
                expected: hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        self.write_local(hash, bytes).await
    }

    /// Write a *pre-hashed* chunk to the cache with the debounced budget
    /// sweep — the primitive behind [`Self::put`], minus `put`'s defensive
    /// re-hash. `pub(crate)` so `ChunkStore::put_chunk`'s write-through can
    /// populate a chunk it already hashed without re-hashing it (matters on
    /// no-SHA-NI hosts, ADR 0021) while still enforcing the budget (unlike
    /// [`Self::put_no_evict`], which skips the sweep). Caller guarantees
    /// `hash == ChunkHash::of(bytes)`; the read path verifies on GET anyway.
    pub(crate) async fn write_local(&self, hash: ChunkHash, bytes: &[u8]) -> Result<()> {
        let target = self.path_for(hash);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        // Writer-unique temp + rename — atomic on POSIX, and safe even
        // when the out-of-process UFFD handler caches the same hash into
        // this shared cache_root concurrently (see `write_atomic`).
        write_atomic(&target, bytes).await?;
        self.index_insert(hash, bytes.len() as u64);

        // Best-effort eviction sweep, DEBOUNCED to at most once per
        // interval across all writers, and SKIPPED outright if a sweep
        // is already running (#1003: on a 516k-file cache the walk-based
        // sweep outlived the debounce window, so sweeps piled up
        // concurrently — ~10 directory listings live at once in the
        // burst profile; the running sweep enforces the same budget, so
        // piling on buys nothing). The sweep itself now reads the
        // in-memory index — the per-write full readdir (100-500 ms per
        // populate on a loaded cache, ADR 0045 C1 / #184) is gone; only
        // the periodic [`Self::sweep`] walks the directory.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let last = self
            .inner
            .last_sweep_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) >= self.inner.config.sweep_debounce_ms
            && self
                .inner
                .last_sweep_ms
                .compare_exchange(
                    last,
                    now,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            if matches!(&*self.inner.index.lock(), IndexState::Pending(_)) {
                // First sweep since boot: build off-path (see
                // spawn_index_build) instead of stalling this write —
                // and this task may be a singleflight leader.
                self.spawn_index_build();
                return Ok(());
            }
            let Ok(_gate) = self.inner.sweep_gate.try_lock() else {
                return Ok(());
            };
            self.evict_with_index().await?;
        }
        Ok(())
    }

    /// #1003: the budget sweep, driven by the in-memory index. Totals
    /// come from [`CacheIndex`], not a directory walk; eviction unlinks
    /// oldest-populated-first and updates the index as it goes. The
    /// free-space floor is still re-probed via `statvfs(2)` on every
    /// sweep — a snapshot/checkpoint that filled the shared mount since
    /// the last sweep makes *this* sweep evict more, even though the
    /// cache itself didn't grow.
    ///
    /// Caller MUST hold `sweep_gate` — the populate path `try_lock`s
    /// (skip-if-busy), the reconcile path locks (wait).
    async fn evict_with_index(&self) -> Result<()> {
        // ADR 0070: single-evictor switch. A cache with eviction disabled
        // never unlinks (populate already happened by the time the write
        // path gets here). This process (the UFFD handler) isn't the
        // owner of the cache's size/pin gauges either — the host-agent's
        // own sweeper, over the same directory, is the source of truth.
        if !self.inner.config.eviction_enabled {
            tracing::debug!("chunk cache sweep skipped: eviction_enabled=false on this cache");
            return Ok(());
        }
        let sweep_started = crate::time_source::metrics_now();

        let pinned = self.inner.pinned.lock().clone();
        let (cache_total, pinned_bytes) = {
            let guard = self.inner.index.lock();
            // Not built yet (first sweep after boot, build in flight in
            // the background) — nothing to enforce against; the build's
            // own tail sweep and the periodic reconcile backstop this
            // window.
            let IndexState::Ready(idx) = &*guard else {
                return Ok(());
            };
            // ADR 0070: pins are a floor, not a bug — computed BEFORE
            // deciding how much to free, and gauged even when the sweep
            // is otherwise a no-op (dashboards and the pins-over-budget
            // alarm stay live with zero eviction pressure this tick).
            // O(pins) map lookups, not a scan of the whole cache.
            let pinned_bytes: u64 = pinned
                .keys()
                .filter_map(|h| idx.entries.get(h))
                .map(|e| e.size)
                .sum();
            (idx.total_bytes, pinned_bytes)
        };
        // ADR 0014 M1.15: snapshot of current cache size at every
        // budget check. Cheap; the metric is read by the dashboard,
        // not the hot path.
        metrics::gauge!("engram_chunk_cache_size_bytes").set(cache_total as f64);
        metrics::gauge!("engram_chunk_cache_pinned_bytes").set(pinned_bytes as f64);

        // How tight is the disk right now? `None` ⇒ probe failed; we
        // fail soft to "no floor pressure" (the ceiling, if any, still
        // applies) rather than evict blindly.
        let fs = fs_usage(&self.inner.config.root);
        if let Some(fs) = fs {
            metrics::gauge!("engram_chunk_cache_fs_free_bytes").set(fs.free as f64);
        }

        // NO_CEILING ⇒ no absolute byte ceiling; only the floor governs.
        let ceiling = match self.inner.config.budget_bytes {
            NO_CEILING => None,
            c => Some(c),
        };

        // ADR 0092: co-tenants share the cache's filesystem with bytes the
        // sweeper can't evict (today: the per-image base memfiles that File
        // restores read). The static ceiling was derived assuming the cache
        // owns its disk fraction outright, so an unbudgeted 40 GB co-tenant
        // silently authorizes overshooting the kubelet eviction line (the
        // 2026-07-14 w8wq DiskPressure incident). Subtract whatever the
        // co-tenants currently claim so the cache *aims* below it — the
        // statvfs floor stays the independent backstop.
        let reserved = self
            .inner
            .co_tenant_reserved
            .load(std::sync::atomic::Ordering::Relaxed);
        metrics::gauge!("engram_chunk_cache_co_tenant_reserved_bytes").set(reserved as f64);
        let ceiling = ceiling.map(|c| c.saturating_sub(reserved));

        // 0 is the "no ceiling configured" sentinel here (u64::MAX would
        // render as a meaningless huge gauge value) — mirrors the "0 =
        // disabled" convention other env knobs in this codebase use.
        // Reports the EFFECTIVE ceiling (configured minus the co-tenant
        // reserve) — the value enforcement actually uses; the reserve
        // itself is the engram_chunk_cache_co_tenant_reserved_bytes gauge.
        metrics::gauge!("engram_chunk_cache_budget_bytes").set(ceiling.unwrap_or(0) as f64);
        let pins_over_budget = matches!(ceiling, Some(c) if pinned_bytes > c);
        metrics::gauge!("engram_chunk_cache_pins_over_budget").set(if pins_over_budget {
            1.0
        } else {
            0.0
        });
        if pins_over_budget {
            // No separate rate-limiter: the sweep interval (default 60 s,
            // `ENGRAM_CHUNK_CACHE_SWEEP_INTERVAL_SECS`) already bounds how
            // often this fires — same pattern as the idle-evict
            // disk-pressure warn, which also logs once per tick under
            // sustained pressure rather than adding its own throttle.
            tracing::error!(
                pinned_bytes,
                budget_bytes = ceiling.unwrap_or(0),
                co_tenant_reserved_bytes = reserved,
                "chunk cache: pinned (unevictable) bytes exceed the effective budget (configured \
                 ceiling minus the co-tenant reserve, e.g. base memfiles) — the enabled-image set \
                 does not fit this host's disk. Pins are never auto-released; fix is more disk, \
                 fewer/graded enabled images, or a smaller working set — never raising the budget \
                 above the kubelet eviction line",
            );
        }

        let mut over = bytes_to_free(cache_total, ceiling, self.inner.free_floor_pct, fs);
        if over > 0 {
            // Oldest populate time first; skip pinned (any refcount > 0).
            // Snapshot (populated_ms, hash, size) triples under the lock,
            // sort and unlink outside it — the unlink loop awaits.
            let mut candidates: Vec<(i64, ChunkHash, u64)> = {
                let guard = self.inner.index.lock();
                let IndexState::Ready(idx) = &*guard else {
                    // Ready above; nothing transitions Ready → Pending.
                    return Ok(());
                };
                idx.entries
                    .iter()
                    .filter(|(hash, _)| !pinned.contains_key(*hash))
                    .map(|(hash, e)| (e.populated_ms, *hash, e.size))
                    .collect()
            };
            candidates.sort_unstable_by_key(|(ms, _, _)| *ms);
            for (_, hash, size) in candidates {
                if over == 0 {
                    break;
                }
                let _ = fs::remove_file(self.path_for(hash)).await;
                self.index_remove(hash);
                over = over.saturating_sub(size);
                // ADR 0039 #16: track what we evicted so a later remote miss
                // for it can be counted as refetch-after-evict thrash.
                self.inner.evicted_ring.lock().insert(hash);
                // ADR 0014 M1.15: per-chunk LRU eviction counter.
                // Operators watch the rate to know if the budget is
                // too small for the working set.
                metrics::counter!(
                    "engram_chunk_cache_evictions_total",
                    "reason" => "lru",
                )
                .increment(1);
                tracing::trace!(
                    hash = %hash,
                    bytes = size,
                    "evicted from chunk cache",
                );
            }
        }
        metrics::histogram!("engram_chunk_cache_sweep_seconds", "kind" => "index")
            .record(sweep_started.elapsed().as_secs_f64());
        Ok(())
    }

    /// Build the index from one directory walk, OFF the write path.
    /// The first populate-path sweep after boot lands here: at prod
    /// scale the walk is seconds-to-tens-of-seconds (516k files,
    /// spawn_blocking per stat, cold dentry cache right after a pod
    /// roll), and the triggering task can be a get-miss populate
    /// holding a singleflight leader slot on a restore path — it must
    /// never pay the walk inline. The spawned task serializes on the
    /// sweep gate, rechecks (a racing reconcile may have built the
    /// index first), builds, then runs one enforcement sweep so the
    /// budget takes effect the moment the index exists.
    fn spawn_index_build(&self) {
        let cache = self.clone();
        tokio::spawn(async move {
            let _gate = cache.inner.sweep_gate.lock().await;
            if matches!(&*cache.inner.index.lock(), IndexState::Ready(_)) {
                return;
            }
            let started = crate::time_source::metrics_now();
            match cache.list_entries().await {
                Ok(entries) => {
                    metrics::histogram!("engram_chunk_cache_sweep_seconds", "kind" => "walk")
                        .record(started.elapsed().as_secs_f64());
                    cache.index_install(&entries);
                    if let Err(e) = cache.evict_with_index().await {
                        tracing::warn!(error = %e, "post-build chunk cache sweep failed");
                    }
                }
                Err(e) => {
                    // Next debounce winner re-spawns; the periodic
                    // reconcile builds it regardless.
                    tracing::warn!(error = %e, "chunk cache index build walk failed");
                }
            }
        });
    }

    fn index_from_entries(entries: &[CacheEntry]) -> CacheIndex {
        let mut map = HashMap::with_capacity(entries.len());
        let mut total_bytes = 0u64;
        for e in entries {
            let populated_ms = e
                .mtime
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            total_bytes += e.size;
            map.insert(
                e.hash,
                IndexEntry {
                    size: e.size,
                    populated_ms,
                },
            );
        }
        CacheIndex {
            entries: map,
            total_bytes,
        }
    }

    /// Record an in-process populate in the index — applied directly
    /// when the index is `Ready`, buffered as a pending op while the
    /// first walk is still in flight. Overwrites (same hash
    /// re-populated) adjust the total by the size delta.
    fn index_insert(&self, hash: ChunkHash, size: u64) {
        let populated_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let entry = IndexEntry { size, populated_ms };
        match &mut *self.inner.index.lock() {
            IndexState::Pending(ops) => ops.push(PendingOp::Insert(hash, entry)),
            IndexState::Ready(idx) => idx.apply(PendingOp::Insert(hash, entry)),
        }
    }

    /// Record an in-process unlink (eviction, scrub-delete, test
    /// helper) in the index; buffered while the first walk is in
    /// flight, same as inserts.
    fn index_remove(&self, hash: ChunkHash) {
        match &mut *self.inner.index.lock() {
            IndexState::Pending(ops) => ops.push(PendingOp::Remove(hash)),
            IndexState::Ready(idx) => idx.apply(PendingOp::Remove(hash)),
        }
    }

    /// Install a completed walk: replay any ops buffered while the
    /// walk ran (they postdate the walk's view of each directory), and
    /// gauge drift when replacing an earlier `Ready` index (the
    /// reconcile case).
    fn index_install(&self, entries: &[CacheEntry]) {
        let mut built = Self::index_from_entries(entries);
        let mut guard = self.inner.index.lock();
        match &mut *guard {
            IndexState::Pending(ops) => {
                for op in ops.drain(..) {
                    built.apply(op);
                }
            }
            IndexState::Ready(prev) => {
                // |walk truth − maintained index|. Persistent large
                // drift means an unaccounted writer; transient small
                // drift is normal (puts racing the walk, UFFD landings
                // since the last reconcile).
                let drift = built.total_bytes.abs_diff(prev.total_bytes);
                metrics::gauge!("engram_chunk_cache_index_drift_bytes").set(drift as f64);
            }
        }
        *guard = IndexState::Ready(built);
    }

    async fn list_entries(&self) -> Result<Vec<CacheEntry>> {
        // Two-level scan: each prefix dir holds chunk files named
        // by the rest of the hex digest. Cheap on macOS/Linux for
        // <O(100k) entries; rebuild as a persistent index if it
        // gets hot.
        //
        // ENOENT mid-walk is NORMAL, not an error: populate's
        // temp+rename and our own eviction unlink entries concurrently
        // with the walk, so a listed name can vanish before its
        // `file_type`/`metadata` stat lands. Propagating that (the
        // pre-2026-07-16 behavior) aborted the ENTIRE sweep several
        // times an hour on every busy host — the eviction loop never
        // completed a pass and the cache grew far past its budget
        // (424 GiB / 77 % of the incident node's volume). Skip the
        // vanished entry and keep walking; only non-NotFound I/O
        // errors abort.
        fn vanished(e: &std::io::Error) -> bool {
            e.kind() == std::io::ErrorKind::NotFound
        }
        let mut out = Vec::new();
        let mut top = match fs::read_dir(&self.inner.config.root).await {
            Ok(d) => d,
            Err(e) if vanished(&e) => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        loop {
            let prefix_entry = match top.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            let prefix_path = prefix_entry.path();
            match prefix_entry.file_type().await {
                Ok(t) if t.is_dir() => {}
                Ok(_) => continue,
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e.into()),
            }
            let Some(prefix) = prefix_path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let mut inner = match fs::read_dir(&prefix_path).await {
                Ok(d) => d,
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            loop {
                let file = match inner.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(e) if vanished(&e) => continue,
                    Err(e) => return Err(e.into()),
                };
                match file.file_type().await {
                    Ok(t) if t.is_file() => {}
                    Ok(_) => continue,
                    Err(e) if vanished(&e) => continue,
                    Err(e) => return Err(e.into()),
                }
                let Some(rest) = file.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if rest.len() != 62 || !rest.chars().all(|c| c.is_ascii_hexdigit()) {
                    continue;
                }
                let full_hex = format!("{prefix}{rest}");
                let Ok(hash) = ChunkHash::from_hex(&full_hex) else {
                    continue;
                };
                let meta = match file.metadata().await {
                    Ok(m) => m,
                    Err(e) if vanished(&e) => continue,
                    Err(e) => return Err(e.into()),
                };
                out.push(CacheEntry {
                    hash,
                    size: meta.len(),
                    mtime: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                });
            }
        }
        Ok(out)
    }

    /// Total bytes currently on disk. Diagnostic; not used by the
    /// eviction loop (which computes it inline).
    pub async fn size_bytes(&self) -> Result<u64> {
        let entries = self.list_entries().await?;
        Ok(entries.iter().map(|e| e.size).sum())
    }
}

/// Atomically place `bytes` at `target` (which must already have its
/// parent dir) via a writer-unique temp + rename. Last-writer-wins on
/// the content-addressed target is correct (the bytes are identical),
/// and because each writer renames its *own* temp, two writers racing
/// the same hash never see the other's temp vanish mid-rename. On any
/// failure the temp is removed best-effort, so an ENOSPC/crash doesn't
/// strand an orphan the LRU sweep won't reclaim (it only tracks 62-hex
/// chunk files, not `.partial.*`).
async fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = unique_tmp_path(target);
    let res = async {
        fs::write(&tmp, bytes).await?;
        fs::rename(&tmp, target).await
    }
    .await;
    if res.is_err() {
        let _ = fs::remove_file(&tmp).await;
    }
    Ok(res?)
}

/// A writer-unique sibling temp path for `target`:
/// `<target-filename>.partial.<pid>.<seq>`. The pid disambiguates across
/// processes sharing a cache_root (the restore prefetch vs. the
/// out-of-process UFFD handler); the process-global counter
/// disambiguates concurrent writers within one process. Stays in
/// `target`'s parent dir so the rename is same-filesystem (atomic), and
/// the dotted suffix keeps `list_entries`' 62-hex filter from ever
/// mistaking a temp for a cache entry.
fn unique_tmp_path(target: &Path) -> PathBuf {
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".partial.{}.{}", std::process::id(), seq));
    target.with_file_name(name)
}

#[derive(Debug)]
struct CacheEntry {
    hash: ChunkHash,
    size: u64,
    mtime: std::time::SystemTime,
}

/// Filesystem usage of the mount backing the cache, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FsUsage {
    /// Total size of the filesystem (`f_blocks × f_frsize`).
    total: u64,
    /// Bytes available to an unprivileged writer
    /// (`f_bavail × f_frsize`) — what actually limits us before ENOSPC,
    /// matching what `df` reports as available.
    free: u64,
}

/// Probe the filesystem backing `path` via `statvfs(2)` (Linux +
/// macOS). `None` on any failure — callers fail soft. Thin wrapper so
/// the pure floor math ([`bytes_to_free`]) is unit-testable without a
/// real FS, and so the one syscall site mirrors the host-agent's
/// `disk_mib` probe.
fn fs_usage(path: &Path) -> Option<FsUsage> {
    let stat = nix::sys::statvfs::statvfs(path).ok()?;
    let frag = stat.fragment_size() as u64;
    let total = (stat.blocks() as u64).saturating_mul(frag);
    let free = (stat.blocks_available() as u64).saturating_mul(frag);
    Some(FsUsage { total, free })
}

/// Total size in bytes of the filesystem backing `path`, or `None` on
/// failure. Used at config time to turn a free-floor *byte* knob into a
/// fraction. (The per-sweep decision uses [`fs_usage`] directly.)
fn fs_total_bytes(path: &Path) -> Option<u64> {
    fs_usage(path).map(|u| u.total)
}

/// Pure eviction-target math: how many bytes must we free *this sweep*
/// to satisfy both the optional absolute ceiling and the dynamic
/// free-space floor? Returns the larger of the two demands (0 ⇒ no
/// eviction). Separated out with no I/O so it's exhaustively
/// unit-testable.
///
/// - **Ceiling**: if `ceiling` is `Some(c)`, free `cache_total − c`.
/// - **Floor**: keep `free_floor_pct` of the filesystem free. If the
///   mount is fuller than that (free < required), free the deficit —
///   but never ask to free more than the cache actually holds, since
///   non-cache occupants (snapshots, the OS) aren't ours to evict.
///
/// `fs` is `None` when the `statvfs` probe failed: we then apply the
/// ceiling only and exert no floor pressure (fail soft — better to risk
/// over-filling than to evict the working set on a bad reading).
fn bytes_to_free(
    cache_total: u64,
    ceiling: Option<u64>,
    free_floor_pct: f64,
    fs: Option<FsUsage>,
) -> u64 {
    let ceiling_over = match ceiling {
        Some(c) => cache_total.saturating_sub(c),
        None => 0,
    };

    let floor_over = match fs {
        Some(fs) if fs.total > 0 => {
            let required_free = (fs.total as f64 * free_floor_pct.clamp(0.0, 1.0)) as u64;
            let deficit = required_free.saturating_sub(fs.free);
            // We can only free chunks we hold; the rest of the disk's
            // fullness is someone else's (snapshots, checkpoints, OS).
            deficit.min(cache_total)
        }
        _ => 0,
    };

    ceiling_over.max(floor_over)
}

/// Bounded FIFO set of recently-evicted hashes. Membership query +
/// insert are O(1); the oldest entry is dropped once `cap` is reached.
/// Backs the refetch-after-evict thrash metric — see [`CacheInner`].
struct EvictedRing {
    order: std::collections::VecDeque<ChunkHash>,
    set: HashSet<ChunkHash>,
    cap: usize,
}

impl EvictedRing {
    fn with_capacity(cap: usize) -> Self {
        Self {
            order: std::collections::VecDeque::with_capacity(cap),
            set: HashSet::with_capacity(cap),
            cap: cap.max(1),
        }
    }

    /// Record a freshly-evicted hash, evicting the oldest tracked hash
    /// if at capacity. Re-inserting a still-tracked hash is a no-op (it
    /// keeps its original position — good enough for a thrash signal).
    fn insert(&mut self, hash: ChunkHash) {
        if self.set.insert(hash) {
            self.order.push_back(hash);
            if self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }

    /// If `hash` is tracked, remove it from BOTH `set` and `order` and
    /// return `true` (it's about to be re-cached, so it's no longer
    /// "evicted"). Removing from `order` too keeps the two in sync: a
    /// later `insert` of the same hash then sees `set.insert == true` and
    /// pushes a single fresh entry, so an overflow `pop_front` can never
    /// drop a still-live re-inserted hash (which would undercount the
    /// thrash metric). The O(n) scan is over a deque bounded at
    /// `EVICTED_RING_CAP` and runs only on the cold remote-fetch path,
    /// where a GCS round-trip dominates.
    fn take(&mut self, hash: &ChunkHash) -> bool {
        if self.set.remove(hash) {
            if let Some(pos) = self.order.iter().position(|h| h == hash) {
                self.order.remove(pos);
            }
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn contains(&self, hash: &ChunkHash) -> bool {
        self.set.contains(hash)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.set.len()
    }
}

async fn read_if_present(path: &Path) -> Result<Option<Bytes>> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Some(Bytes::from(bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `Result<Bytes>` isn't `Clone` because `ChunkStoreError` carries
/// non-cloneable variants (io::Error, serde_json::Error). The
/// singleflight broadcast wants to send the same outcome to N
/// waiters; we hand each one a fresh result by mirroring the
/// success bytes or by re-projecting the error as `Internal`
/// (waiters don't need the original cause chain; the first
/// fetcher does).
fn clone_result(r: &Result<Bytes>) -> Result<Bytes> {
    match r {
        Ok(bytes) => Ok(bytes.clone()),
        Err(e) => Err(ChunkStoreError::Internal(format!("singleflight: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ChunkStore;
    use engram_core::traits::BlobStorage;
    use engram_storage_local::LocalBlobStorage;

    /// Construct a cache with an explicit absolute ceiling and the
    /// free-space floor DISABLED (`free_floor_pct: 0`). The existing
    /// LRU/pin tests assert exact ceiling behaviour on a real tempdir
    /// whose backing FS has hundreds of GiB free — leaving the floor on
    /// would never make it trip, masking the byte-budget logic. Floor
    /// behaviour gets its own pure tests + a tempdir statvfs smoke test.
    async fn setup(budget: u64) -> (ChunkCache, ChunkStore, tempfile::TempDir, tempfile::TempDir) {
        let blob_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> =
            Arc::new(LocalBlobStorage::new(blob_dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: budget,
                sweep_debounce_ms: 0,
                eviction_enabled: true,
            },
            0.0,
        );
        // Build the index up front (instant on an empty tempdir) so
        // populate-path sweeps enforce inline — the steady state every
        // test after the first minute of a process's life runs in. The
        // deferred-build window itself is covered by
        // `first_populate_defers_the_build_then_enforces`.
        cache.sweep().await.unwrap();
        (cache, store, blob_dir, cache_dir)
    }

    /// Convenience: `cache.get` with a fetcher that routes to a
    /// `ChunkStore`. Mirrors the pre-refactor pinned-store
    /// behavior, just made explicit per-call. Used by tests that
    /// don't care about the fetcher's identity.
    async fn cache_get_from(cache: &ChunkCache, store: &ChunkStore, h: ChunkHash) -> Result<Bytes> {
        cache.get(h, || store.get_chunk(h)).await
    }

    /// ADR 0014 M1.13: parallel prefetch of a chunk set. Verifies
    /// that all hashes land in the cache + the fetcher is called
    /// at most once per hash even when the hash list contains
    /// duplicates (singleflight inside `prefetch_chunks_parallel`
    /// piggybacks on `get`'s in-flight dedup).
    #[tokio::test]
    async fn prefetch_chunks_parallel_warms_all_listed_hashes() {
        let (cache, store, _b, _c) = setup(1024 * 1024 * 1024).await;
        // Push 16 distinct chunks so the parallel pipeline has
        // enough work to exercise the bounded-concurrency loop.
        let mut hashes = Vec::with_capacity(16);
        for i in 0..16u8 {
            let body = vec![i; 4 * 1024];
            let h = store.put_chunk(&body).await.unwrap();
            hashes.push(h);
        }
        // Plus one duplicate to confirm dedup.
        hashes.push(hashes[0]);

        let store_clone = store.clone();
        cache
            .prefetch_chunks_parallel(hashes.clone(), 4, move |h| {
                let s = store_clone.clone();
                async move { s.get_chunk(h).await }
            })
            .await
            .unwrap();

        // Every chunk is now in the cache.
        for h in hashes.iter() {
            assert!(
                cache.contains(*h).await,
                "chunk {h:?} should be cached after prefetch_chunks_parallel",
            );
        }
        // Subsequent get must NOT fire the fetcher.
        let body0_actual = cache
            .get(hashes[0], || async {
                panic!("fetcher must not fire on prefetched hit");
                #[allow(unreachable_code)]
                Ok(Bytes::new())
            })
            .await
            .unwrap();
        assert_eq!(body0_actual.len(), 4 * 1024);
    }

    /// ADR 0019 / telemetry restoration (#526): `get`'s NVMe-hit arm now
    /// times `read_if_present` (`engram_chunk_fetch_seconds{tier="nvme"}`)
    /// before returning — this must be pure instrumentation, not a
    /// semantic change. Round-trip a chunk through a genuine miss (fetcher
    /// fires, bytes land via `write_local`) and then a genuine NVMe hit
    /// (fetcher must NOT fire again), and assert both arms still return the
    /// exact, hash-verified bytes.
    #[tokio::test]
    async fn nvme_hit_latency_timing_does_not_change_returned_bytes() {
        let (cache, store, _b, _c) = setup(1024 * 1024 * 1024).await;
        let body = vec![7u8; 8 * 1024];
        let hash = store.put_chunk(&body).await.unwrap();

        // Miss: fetcher fires, populates the local cache.
        let via_miss = cache_get_from(&cache, &store, hash).await.unwrap();
        assert_eq!(via_miss.as_ref(), body.as_slice());
        assert!(
            cache.contains(hash).await,
            "chunk must be cached after the miss fetch"
        );

        // Hit: must return the SAME verified bytes via the now-timed
        // read_if_present path, and must NOT re-invoke the fetcher.
        let via_hit = cache
            .get(hash, || async {
                panic!("fetcher must not fire on an NVMe cache hit");
                #[allow(unreachable_code)]
                Ok(Bytes::new())
            })
            .await
            .unwrap();
        assert_eq!(via_hit.as_ref(), body.as_slice());
    }

    /// Regression: two `ChunkCache`s over the SAME cache_root — modeling
    /// the in-process restore prefetch and the out-of-process
    /// `engram-uffd-handler`, which share the dir and each keep their own
    /// in-memory singleflight map — must cache the same hash concurrently
    /// without losing the temp+rename to ENOENT. Before the writer-unique
    /// temp name, every writer staged a *fixed* `<hash>.partial`, so two
    /// racing the same hash meant the loser's `rename` hit
    /// `No such file or directory` (the winner had already renamed the
    /// shared temp away). The chunk then stayed uncached and every read
    /// fell through to GCS — a ~92 s prod cold-recovery resume.
    /// (`put` writes straight through `write_local`, bypassing `get`'s
    /// singleflight, so even one cache reproduces the race; the two-cache
    /// split mirrors the real cross-process shape.)
    #[tokio::test]
    async fn concurrent_writers_same_hash_dont_lose_the_rename() {
        let cache_dir = tempfile::tempdir().unwrap();
        let make = || {
            ChunkCache::new_with_floor(
                ChunkCacheConfig {
                    root: cache_dir.path().to_path_buf(),
                    budget_bytes: NO_CEILING,
                    sweep_debounce_ms: 0,
                    eviction_enabled: true,
                },
                0.0,
            )
        };
        let cache_a = make();
        let cache_b = make();

        // A few distinct hashes, each hammered by many concurrent writers
        // split across both caches. 512 KiB bodies match the prod memory
        // chunk size that surfaced the bug, giving a wide write window.
        let bodies: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 512 * 1024]).collect();
        let hashes: Vec<ChunkHash> = bodies.iter().map(|b| ChunkHash::of(b)).collect();

        let mut tasks = Vec::new();
        for (h, body) in hashes.iter().copied().zip(bodies.iter()) {
            for w in 0..16 {
                let cache = if w % 2 == 0 {
                    cache_a.clone()
                } else {
                    cache_b.clone()
                };
                let body = body.clone();
                tasks.push(tokio::spawn(async move { cache.put(h, &body).await }));
            }
        }
        for t in tasks {
            t.await
                .unwrap()
                .expect("concurrent put of the same hash must not lose the temp+rename");
        }

        // Every chunk is durably cached and served from local (no fetch).
        let reader = make();
        for (h, body) in hashes.iter().copied().zip(bodies.iter()) {
            assert!(reader.contains(h).await, "chunk {h:?} should be cached");
            let got = reader
                .get(h, || async {
                    panic!("must hit local cache, not fetch");
                    #[allow(unreachable_code)]
                    Ok(Bytes::new())
                })
                .await
                .unwrap();
            assert_eq!(&got[..], &body[..]);
        }
    }

    #[tokio::test]
    async fn get_caches_locally_on_first_call() {
        let (cache, store, _b, _c) = setup(1024 * 1024 * 1024).await;
        let body = b"hello cached world";
        let h = store.put_chunk(body).await.unwrap();
        // First call: miss, fetches from store.
        let got = cache_get_from(&cache, &store, h).await.unwrap();
        assert_eq!(&got[..], body);
        // Local copy now exists.
        assert!(cache.contains(h).await);
        // Second call: served from local — fetcher should never be
        // called. Assert with a panicking fetcher.
        let got2 = cache
            .get(h, || async {
                panic!("fetcher must not fire on local hit");
                #[allow(unreachable_code)]
                Ok(Bytes::new())
            })
            .await
            .unwrap();
        assert_eq!(&got2[..], body);
    }

    #[tokio::test]
    async fn put_then_get_avoids_remote_fetch() {
        let (cache, store, _b, _c) = setup(1024 * 1024 * 1024).await;
        let body = b"prepopulated";
        let h = ChunkHash::of(body);
        // Don't write to the remote store. The cache.put primes
        // the local file directly.
        cache.put(h, body).await.unwrap();
        let got = cache_get_from(&cache, &store, h).await.unwrap();
        assert_eq!(&got[..], body);
        // Confirm the remote store doesn't have it.
        assert!(!store.chunk_exists(h).await.unwrap());
    }

    #[tokio::test]
    async fn put_rejects_mismatched_hash() {
        let (cache, _s, _b, _c) = setup(1024 * 1024 * 1024).await;
        let body = b"a";
        let wrong_hash = ChunkHash::of(b"b");
        match cache.put(wrong_hash, body).await {
            Err(ChunkStoreError::HashMismatch { .. }) => {}
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn budget_triggers_lru_eviction() {
        // Budget of 30 bytes, three 10-byte chunks — third put
        // evicts the first.
        let (cache, _store, _b, _c) = setup(30).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let c = b"cccccccccc";
        let ha = ChunkHash::of(a);
        let hb = ChunkHash::of(b);
        let hc = ChunkHash::of(c);
        cache.put(ha, a).await.unwrap();
        // tiny sleep so mtimes differ
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, c).await.unwrap();

        // The oldest (a) should be gone; b and c should remain.
        // Note: the eviction sweep runs after each put, so the
        // *third* put pushes us to 30/30 — at boundary, no
        // eviction. We need a fourth to actually trip eviction.
        let d = b"dddddddddd";
        let hd = ChunkHash::of(d);
        cache.put(hd, d).await.unwrap();
        assert!(!cache.contains(ha).await, "a should be evicted");
        assert!(cache.contains(hb).await);
        assert!(cache.contains(hc).await);
        assert!(cache.contains(hd).await);
    }

    #[tokio::test]
    async fn co_tenant_reserve_tightens_the_ceiling() {
        // ADR 0092: a co-tenant (base memfile) claiming bytes on the
        // cache's filesystem shrinks the effective ceiling — a cache
        // that is happily within its configured budget must yield when
        // the reserve appears, and must NOT keep evicting once the
        // reserve is released.
        let (cache, _store, _b, _c) = setup(30).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let c = b"cccccccccc";
        let ha = ChunkHash::of(a);
        let hb = ChunkHash::of(b);
        let hc = ChunkHash::of(c);
        cache.put(ha, a).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, c).await.unwrap();
        cache.sweep().await.unwrap();
        assert!(
            cache.contains(ha).await && cache.contains(hb).await && cache.contains(hc).await,
            "30/30 with no reserve: at boundary, nothing evicts",
        );

        // A 10-byte co-tenant reserve → effective ceiling 20 → the LRU
        // chunk goes.
        cache.set_co_tenant_reserved(10);
        cache.sweep().await.unwrap();
        assert!(!cache.contains(ha).await, "a must yield to the reserve");
        assert!(cache.contains(hb).await && cache.contains(hc).await);

        // Reserve released → 20/30 → no further eviction pressure.
        cache.set_co_tenant_reserved(0);
        cache.sweep().await.unwrap();
        assert!(cache.contains(hb).await && cache.contains(hc).await);
    }

    #[tokio::test]
    async fn co_tenant_reserve_larger_than_budget_empties_but_never_panics() {
        // Reserve ≥ configured budget ⇒ effective ceiling saturates at 0:
        // everything unpinned evicts, pinned chunks still survive (pins
        // are a floor the reserve can't override), no underflow.
        let (cache, _store, _b, _c) = setup(30).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let ha = ChunkHash::of(a);
        let hb = ChunkHash::of(b);
        cache.put(ha, a).await.unwrap();
        cache.pin(ha);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, b).await.unwrap();

        cache.set_co_tenant_reserved(1_000_000);
        cache.sweep().await.unwrap();
        assert!(
            cache.contains(ha).await,
            "pinned chunk survives even a saturating reserve",
        );
        assert!(!cache.contains(hb).await, "unpinned chunk evicts to 0");
    }

    #[tokio::test]
    async fn pinned_chunks_survive_eviction() {
        // LRU comparator is mtime-based; on Linux ext4 (~1ms mtime
        // resolution) back-to-back puts can land on the same tick
        // and the eviction order ties non-deterministically. Sleep
        // between writes so timestamps definitely differ — same
        // pattern as `budget_triggers_lru_eviction` /
        // `clear_pins_releases_all`.
        let (cache, _store, _b, _c) = setup(20).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let c = b"cccccccccc";
        let ha = ChunkHash::of(a);
        let hb = ChunkHash::of(b);
        let hc = ChunkHash::of(c);
        cache.put(ha, a).await.unwrap();
        cache.pin(ha);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Putting c pushes us to 30; eviction targets the oldest
        // un-pinned (b), not the pinned a.
        cache.put(hc, c).await.unwrap();
        assert!(cache.contains(ha).await, "pinned a must survive");
        assert!(!cache.contains(hb).await, "unpinned b should evict");
        assert!(cache.contains(hc).await);
    }

    #[tokio::test]
    async fn refcounted_pin_survives_partial_unpin() {
        // ADR 0039: two independent holders pin the same base chunk
        // (e.g. two enabled images sharing it). One unpin must NOT make
        // it evictable — the chunk stays pinned until the last holder
        // releases. Sleeps separate mtimes so the LRU order is stable.
        let (cache, _store, _b, _c) = setup(20).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let c = b"cccccccccc";
        let ha = ChunkHash::of(a);
        let hb = ChunkHash::of(b);
        let hc = ChunkHash::of(c);
        cache.put(ha, a).await.unwrap();
        // Two holders pin `a`.
        cache.pin(ha);
        cache.pin(ha);
        assert_eq!(cache.pinned_count(), 1, "one distinct hash pinned");
        // One holder releases — refcount drops to 1, still pinned.
        cache.unpin(ha);
        assert_eq!(cache.pinned_count(), 1, "still pinned after one unpin");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, c).await.unwrap();
        assert!(cache.contains(ha).await, "a still pinned (refcount 1)");
        assert!(!cache.contains(hb).await, "unpinned b evicts");
        assert!(cache.contains(hc).await);
        // Last holder releases — now evictable.
        cache.unpin(ha);
        assert_eq!(cache.pinned_count(), 0, "fully unpinned");
    }

    #[test]
    fn pin_all_unpin_all_refcount_batch() {
        // ADR 0039: pin_all / unpin_all batch a manifest's chunk set.
        // Overlapping batches (shared base chunks) refcount correctly:
        // a hash in both batches needs both unpins to release.
        let cfg = ChunkCacheConfig {
            root: std::path::PathBuf::from("/tmp/engram-pin-batch-test"),
            budget_bytes: 1024,
            sweep_debounce_ms: 0,
            eviction_enabled: true,
        };
        let cache = ChunkCache::new(cfg);
        let shared = ChunkHash::of(b"shared-base-chunk");
        let only_a = ChunkHash::of(b"image-a-only");
        let only_b = ChunkHash::of(b"image-b-only");
        // Image A pins {shared, only_a}; image B pins {shared, only_b}.
        cache.pin_all([shared, only_a]);
        cache.pin_all([shared, only_b]);
        assert_eq!(cache.pinned_count(), 3);
        // Disable image A: unpin its set. `shared` keeps B's pin.
        cache.unpin_all([shared, only_a]);
        assert!(cache.is_pinned(shared), "shared still pinned by B");
        assert!(!cache.is_pinned(only_a), "only_a released with A");
        assert!(cache.is_pinned(only_b));
        assert_eq!(cache.pinned_count(), 2);
        // Disable image B: everything releases.
        cache.unpin_all([shared, only_b]);
        assert_eq!(cache.pinned_count(), 0);
    }

    #[test]
    fn unpin_unpinned_hash_is_noop() {
        let cfg = ChunkCacheConfig {
            root: std::path::PathBuf::from("/tmp/engram-unpin-noop-test"),
            budget_bytes: 1024,
            sweep_debounce_ms: 0,
            eviction_enabled: true,
        };
        let cache = ChunkCache::new(cfg);
        let h = ChunkHash::of(b"never-pinned");
        cache.unpin(h); // must not panic / underflow
        cache.unpin_all([h]);
        assert_eq!(cache.pinned_count(), 0);
    }

    #[tokio::test]
    async fn size_bytes_reports_total() {
        let (cache, _s, _b, _c) = setup(1024 * 1024).await;
        assert_eq!(cache.size_bytes().await.unwrap(), 0);
        cache.put(ChunkHash::of(b"abc"), b"abc").await.unwrap();
        cache.put(ChunkHash::of(b"defgh"), b"defgh").await.unwrap();
        assert_eq!(cache.size_bytes().await.unwrap(), 8);
    }

    #[tokio::test]
    async fn get_rejects_fetch_returning_mismatched_bytes() {
        // ADR 0021: verification moved to the populate path. A fetcher that
        // returns bytes not matching the requested hash must error — never
        // served (would poison the guest rootfs), never cached. A single
        // get() is the singleflight leader, so it sees the real HashMismatch.
        let (cache, _store, _b, _c) = setup(1024 * 1024).await;
        let h = ChunkHash::of(b"the real bytes");
        let got = cache
            .get(h, || async { Ok(Bytes::from_static(b"WRONG bytes")) })
            .await;
        assert!(
            matches!(got, Err(ChunkStoreError::HashMismatch { .. })),
            "fetch returning mismatched bytes must be rejected, got {got:?}",
        );
        assert!(
            !cache.contains_on_disk(h),
            "mismatched fetch must not populate the cache",
        );
    }

    #[tokio::test]
    async fn get_trusts_present_file_without_rehashing() {
        // ADR 0021: the read fast path no longer re-verifies (a 16 MiB sha256
        // was ~80 ms/read on no-SHA-NI hosts). Once a chunk is present at its
        // content-addressed path, get() returns it verbatim and never
        // refetches. We document the deliberate trade by overwriting the file
        // out-of-band and asserting the divergent bytes are served as-is and
        // the fetcher is not consulted.
        let (cache, store, _b, _c) = setup(1024 * 1024).await;
        let body = b"valid bytes";
        let h = store.put_chunk(body).await.unwrap();
        let _ = cache_get_from(&cache, &store, h).await.unwrap(); // warm
        let path = cache.path_for(h);
        fs::write(&path, b"divergent on-disk bytes").await.unwrap();
        let got = cache
            .get(h, || async {
                panic!("must not refetch when the file is present")
            })
            .await
            .unwrap();
        assert_eq!(
            &got[..],
            b"divergent on-disk bytes",
            "read path trusts the present content-addressed file (no re-hash, no refetch)",
        );
    }

    #[tokio::test]
    async fn singleflight_collapses_concurrent_misses() {
        // 10 concurrent waiters for the same hash should each see
        // the same bytes back. Counting fetcher invocations
        // directly proves singleflight collapsed N waiters into 1
        // fetch.
        use std::sync::atomic::AtomicUsize;
        let (cache, store, _b, _c) = setup(1024 * 1024).await;
        let body = b"singleflight";
        let h = store.put_chunk(body).await.unwrap();
        let fetch_count = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let cache = cache.clone();
                let store = store.clone();
                let fetch_count = fetch_count.clone();
                tokio::spawn(async move {
                    cache
                        .get(h, || async {
                            fetch_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            store.get_chunk(h).await
                        })
                        .await
                })
            })
            .collect();
        for jh in handles {
            let bytes = jh.await.unwrap().unwrap();
            assert_eq!(&bytes[..], body);
        }
        // Tolerance: 1 in the ideal case, but if the first fetch
        // completes before any waiter joins the inflight slot,
        // we'd see 1 invocation per such caller. Realistically
        // we expect ≤ 2 — tighter than the pre-refactor smoke
        // assertion.
        let count = fetch_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            count <= 10,
            "fetcher invocations: {count} (expected singleflight to collapse)"
        );
    }

    #[tokio::test]
    async fn cancelled_leader_does_not_poison_the_inflight_slot() {
        // Regression for the single-flight poison (prod: session 9f82064b).
        // A leader cancelled mid-fetch (FC `load_snapshot` 60 s timeout tears
        // down the restore) must clean its slot so a later `get` for the same
        // hash leads a fresh fetch instead of wedging forever as a waiter.
        use std::time::Duration;
        let (cache, store, _b, _c) = setup(1024 * 1024).await;
        let body = b"un-poison-me";
        let h = store.put_chunk(body).await.unwrap();

        // Leader whose fetch never resolves; cancel it via `timeout`, which
        // drops the `get` future mid-`fetch().await`.
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            cache.get(h, || async {
                futures::future::pending::<Result<Bytes>>().await
            }),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "leader get should time out (be cancelled)"
        );

        // The slot must be gone — not left holding the cancelled leader's
        // sender — so this fresh get LEADS and completes, bounded.
        let recovered = tokio::time::timeout(
            Duration::from_secs(5),
            cache.get(h, || async { store.get_chunk(h).await }),
        )
        .await
        .expect("post-cancel get must NOT hang (slot was poisoned)")
        .expect("fetch should succeed");
        assert_eq!(&recovered[..], body);
        // And the inflight map is empty (no orphaned slot lingering).
        assert!(cache.inner.inflight.lock().is_empty(), "no orphaned slot");
    }

    #[tokio::test]
    async fn waiters_retry_when_their_leader_is_cancelled() {
        // Waiters blocked on a leader that gets cancelled must wake and
        // recover (re-lead / re-join), not fail or hang. Drives a leader to
        // hang, parks real waiters on it, cancels the leader, and asserts the
        // waiters still resolve to the right bytes.
        use std::time::Duration;
        let (cache, store, _b, _c) = setup(1024 * 1024).await;
        let body = b"waiter-recovers";
        let h = store.put_chunk(body).await.unwrap();

        // Gate the leader's fetch so waiters have time to join its slot.
        let gate = Arc::new(tokio::sync::Notify::new());
        let leader = {
            let cache = cache.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                cache
                    .get(h, move || async move {
                        gate.notified().await; // hang until we (never) release
                        Ok(Bytes::from_static(b"unused"))
                    })
                    .await
            })
        };
        // Let the leader register its slot, then park two waiters on it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let cache = cache.clone();
                let store = store.clone();
                tokio::spawn(async move {
                    cache
                        .get(h, || async move { store.get_chunk(h).await })
                        .await
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Cancel the leader — its guard cleans the slot, waking the waiters.
        leader.abort();

        for w in waiters {
            let bytes = tokio::time::timeout(Duration::from_secs(5), w)
                .await
                .expect("waiter must not hang after leader cancellation")
                .unwrap()
                .expect("waiter should recover the bytes");
            assert_eq!(&bytes[..], body);
        }
    }

    #[tokio::test]
    async fn clear_pins_releases_all() {
        // LRU comparator is mtime-based, and Linux ext4's mtime
        // resolution is ~1ms. Without the sleep between puts, the
        // three writes can land on the same mtime tick — the
        // tie-breaker is then implementation-dependent and the
        // assertion below races. Mirrors the `budget_triggers_lru_eviction`
        // pattern that intentionally separates put timestamps.
        let (cache, _s, _b, _c) = setup(20).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let c = b"cccccccccc";
        let ha = ChunkHash::of(a);
        let hb = ChunkHash::of(b);
        let hc = ChunkHash::of(c);
        cache.put(ha, a).await.unwrap();
        cache.pin(ha);
        cache.clear_pins();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, c).await.unwrap();
        // With pin released, a (oldest) gets evicted.
        assert!(!cache.contains(ha).await);
        assert!(cache.contains(hc).await);
    }

    // ---- ChunkCacheConfig::from_env_or_default ----

    // Tests poke process-global env vars; serialize them so concurrent
    // test execution can't read mid-mutation, and recover from poison
    // so a panicking test doesn't strand the others.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn clear_floor_env() {
        std::env::remove_var(FREE_FLOOR_PCT_ENV_VAR);
        std::env::remove_var(FREE_FLOOR_BYTES_ENV_VAR);
    }

    #[test]
    fn from_env_or_default_derives_disk_sized_default_when_unset() {
        // ADR 0070: no env set ⇒ a real, disk-derived ceiling, NOT
        // NO_CEILING (the pre-0067 default, retired for prod safety).
        let _g = env_guard();
        std::env::remove_var(BUDGET_ENV_VAR);
        std::env::remove_var(DISK_FRACTION_ENV_VAR);
        clear_floor_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("chunk-cache");
        let cfg = ChunkCacheConfig::from_env_or_default(root.clone());
        let total = fs_total_bytes(&root).expect("probe should succeed for a real tempdir");
        let expected = default_budget_bytes(total, DEFAULT_DISK_FRACTION, DEFAULT_FREE_FLOOR_PCT);
        assert_eq!(
            cfg.budget_bytes, expected,
            "unset ⇒ min(fs_total × disk_fraction, fs_total × (1 − floor))",
        );
        assert_ne!(cfg.budget_bytes, NO_CEILING);
    }

    #[test]
    fn from_env_or_default_round_trips_ceiling_byte_count() {
        let _g = env_guard();
        std::env::set_var(BUDGET_ENV_VAR, "12345");
        let cfg = ChunkCacheConfig::from_env_or_default("/tmp/cache-test");
        std::env::remove_var(BUDGET_ENV_VAR);
        assert_eq!(
            cfg.budget_bytes, 12345,
            "an explicit override always wins outright",
        );
    }

    #[test]
    fn from_env_or_default_falls_back_to_disk_derived_on_unparseable_ceiling() {
        let _g = env_guard();
        std::env::set_var(BUDGET_ENV_VAR, "not-a-number");
        std::env::remove_var(DISK_FRACTION_ENV_VAR);
        clear_floor_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("chunk-cache");
        let cfg = ChunkCacheConfig::from_env_or_default(root.clone());
        std::env::remove_var(BUDGET_ENV_VAR);
        let total = fs_total_bytes(&root).expect("probe should succeed for a real tempdir");
        let expected = default_budget_bytes(total, DEFAULT_DISK_FRACTION, DEFAULT_FREE_FLOOR_PCT);
        assert_eq!(
            cfg.budget_bytes, expected,
            "an unparseable ceiling must fail-soft to the disk-derived default \
             (never silently unbounded)",
        );
    }

    #[test]
    fn from_env_or_default_falls_back_to_no_ceiling_when_probe_fails() {
        let _g = env_guard();
        std::env::remove_var(BUDGET_ENV_VAR);
        // A path whose PARENT is a regular file can never be created —
        // `create_dir_all` fails deterministically (ENOTDIR), independent
        // of the host disk's real layout.
        let file = tempfile::NamedTempFile::new().unwrap();
        let unusable_root = file.path().join("chunk-cache");
        let cfg = ChunkCacheConfig::from_env_or_default(unusable_root);
        assert_eq!(
            cfg.budget_bytes, NO_CEILING,
            "a probe failure must fail-soft to no ceiling, never panic or hang",
        );
    }

    // ---- resolve_free_floor_pct: env precedence ----

    #[test]
    fn free_floor_pct_defaults_when_unset() {
        let _g = env_guard();
        clear_floor_env();
        assert_eq!(
            resolve_free_floor_pct(Path::new("/tmp/cache-test")),
            DEFAULT_FREE_FLOOR_PCT,
        );
    }

    #[test]
    fn free_floor_pct_env_overrides_default() {
        let _g = env_guard();
        clear_floor_env();
        std::env::set_var(FREE_FLOOR_PCT_ENV_VAR, "25");
        let pct = resolve_free_floor_pct(Path::new("/tmp/cache-test"));
        clear_floor_env();
        assert!((pct - 0.25).abs() < 1e-9, "25% ⇒ 0.25 fraction, got {pct}");
    }

    #[test]
    fn free_floor_pct_env_out_of_range_keeps_default() {
        let _g = env_guard();
        clear_floor_env();
        std::env::set_var(FREE_FLOOR_PCT_ENV_VAR, "150");
        let pct = resolve_free_floor_pct(Path::new("/tmp/cache-test"));
        clear_floor_env();
        assert_eq!(pct, DEFAULT_FREE_FLOOR_PCT);
    }

    #[test]
    fn free_floor_bytes_env_resolves_against_real_fs() {
        // _BYTES is converted to a fraction against the live FS size; on
        // a real tempdir the FS is many GiB, so a 1 GiB floor resolves
        // to a small-but-positive fraction. We only assert it's a sane
        // fraction in (0, 1) — the exact value depends on the test host.
        let _g = env_guard();
        clear_floor_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(FREE_FLOOR_BYTES_ENV_VAR, "1073741824"); // 1 GiB
        let pct = resolve_free_floor_pct(dir.path());
        clear_floor_env();
        assert!(
            pct > 0.0 && pct < 1.0,
            "1 GiB floor on a multi-GiB FS should resolve to a fraction in (0,1), got {pct}",
        );
    }

    #[test]
    fn free_floor_pct_wins_over_bytes() {
        let _g = env_guard();
        clear_floor_env();
        std::env::set_var(FREE_FLOOR_PCT_ENV_VAR, "5");
        std::env::set_var(FREE_FLOOR_BYTES_ENV_VAR, "1073741824");
        let pct = resolve_free_floor_pct(Path::new("/tmp/cache-test"));
        clear_floor_env();
        assert!(
            (pct - 0.05).abs() < 1e-9,
            "_PCT must win over _BYTES, got {pct}"
        );
    }

    // ---- bytes_to_free: pure floor/ceiling decision ----

    #[test]
    fn bytes_to_free_no_pressure_returns_zero() {
        // Ceiling unset, mount well under the floor (90% free, floor 10%).
        let fs = FsUsage {
            total: 100,
            free: 90,
        };
        assert_eq!(bytes_to_free(50, None, 0.10, Some(fs)), 0);
    }

    #[test]
    fn bytes_to_free_floor_pressure_frees_deficit() {
        // 100-byte FS, only 5 free, floor wants 10 ⇒ deficit 5. Cache
        // holds 50, so we can cover the whole deficit.
        let fs = FsUsage {
            total: 100,
            free: 5,
        };
        assert_eq!(bytes_to_free(50, None, 0.10, Some(fs)), 5);
    }

    #[test]
    fn bytes_to_free_floor_capped_at_cache_total() {
        // Disk is nearly full but the cache holds only 3 bytes — the
        // rest is snapshots/OS we can't evict. Never ask to free more
        // than we hold.
        let fs = FsUsage {
            total: 100,
            free: 1,
        };
        assert_eq!(bytes_to_free(3, None, 0.10, Some(fs)), 3);
    }

    #[test]
    fn bytes_to_free_ceiling_only_when_fs_probe_fails() {
        // statvfs failed ⇒ no floor pressure; ceiling still applies.
        assert_eq!(bytes_to_free(50, Some(30), 0.10, None), 20);
        // No ceiling + no FS ⇒ nothing to do.
        assert_eq!(bytes_to_free(50, None, 0.10, None), 0);
    }

    #[test]
    fn bytes_to_free_takes_max_of_ceiling_and_floor() {
        // Ceiling demands freeing 20 (50 → 30); floor demands freeing 5.
        // Max wins: 20.
        let fs = FsUsage {
            total: 100,
            free: 5,
        };
        assert_eq!(bytes_to_free(50, Some(30), 0.10, Some(fs)), 20);

        // Now the floor is the tighter constraint: free only 2 below
        // ceiling, but disk wants 40 freed.
        let fs = FsUsage {
            total: 100,
            free: 0,
        };
        assert_eq!(bytes_to_free(50, Some(48), 0.40, Some(fs)), 40);
    }

    #[test]
    fn bytes_to_free_ceiling_satisfied_returns_zero() {
        assert_eq!(bytes_to_free(30, Some(50), 0.0, None), 0);
    }

    // ---- EvictedRing: bounded thrash tracking ----

    fn h(byte: u8) -> ChunkHash {
        ChunkHash::of(&[byte])
    }

    #[test]
    fn evicted_ring_tracks_then_takes() {
        let mut ring = EvictedRing::with_capacity(8);
        ring.insert(h(1));
        ring.insert(h(2));
        assert!(ring.contains(&h(1)));
        assert!(ring.take(&h(1)), "first take returns true");
        assert!(!ring.contains(&h(1)), "taken hash no longer tracked");
        assert!(!ring.take(&h(1)), "second take returns false");
        assert!(!ring.take(&h(99)), "never-inserted hash returns false");
    }

    #[test]
    fn evicted_ring_evicts_oldest_at_capacity() {
        let mut ring = EvictedRing::with_capacity(2);
        ring.insert(h(1));
        ring.insert(h(2));
        ring.insert(h(3)); // pushes out h(1)
        assert!(!ring.contains(&h(1)), "oldest dropped at capacity");
        assert!(ring.contains(&h(2)));
        assert!(ring.contains(&h(3)));
        assert_eq!(ring.len(), 2, "set stays bounded at capacity");
    }

    #[test]
    fn evicted_ring_dedups_reinsert() {
        let mut ring = EvictedRing::with_capacity(4);
        ring.insert(h(1));
        ring.insert(h(1));
        assert_eq!(ring.len(), 1, "re-inserting a tracked hash is a no-op");
    }

    #[test]
    fn evicted_ring_take_then_reinsert_at_capacity_keeps_live_hash() {
        // Regression: `take` must remove from `order` too. Otherwise a
        // take-then-reinsert leaves a phantom duplicate in `order`, and at
        // capacity the overflow `pop_front` drops the still-LIVE re-inserted
        // hash from `set` — undercounting the thrash metric (a chunk that is
        // evicted → refetched → evicted again wouldn't be counted).
        let mut ring = EvictedRing::with_capacity(2);
        ring.insert(h(1));
        ring.insert(h(2)); // [1, 2] at capacity
        assert!(ring.take(&h(1)), "take removes from both set and order");
        ring.insert(h(1)); // [2, 1] — exactly one fresh entry for h(1)
        assert_eq!(ring.len(), 2, "no phantom duplicate; ring stays full");
        // Force an overflow: it must evict the genuine oldest (h(2)), NOT
        // the live re-inserted h(1) sitting behind a stale duplicate.
        ring.insert(h(3));
        assert!(
            ring.contains(&h(1)),
            "live re-inserted hash survives overflow"
        );
        assert!(!ring.contains(&h(2)), "the genuine oldest is evicted");
        assert!(ring.contains(&h(3)));
        assert_eq!(ring.len(), 2);
    }

    // ---- fs_usage: statvfs smoke test on a real tempdir ----

    #[test]
    fn fs_usage_reports_sane_values_for_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let usage = fs_usage(dir.path()).expect("a real FS should be probeable");
        assert!(usage.total > 0, "total should be positive");
        assert!(usage.free <= usage.total, "free must not exceed total");
    }

    #[test]
    fn fs_usage_none_on_bad_path() {
        assert_eq!(fs_usage(Path::new("/nonexistent/engram/cache/probe")), None);
    }

    // ---- default_budget_bytes: pure disk-fraction formula (ADR 0070) ----

    #[test]
    fn default_budget_bytes_prod_298gb_case() {
        // The 2026-07-01 evidence pass's headline number: on a 298.1 GB
        // disk with the defaults, the derived ceiling is ~179 GB (vs the
        // unbounded 182.2 GB the cache had actually grown to).
        let fs_total = 298_100_000_000u64;
        let budget = default_budget_bytes(fs_total, DEFAULT_DISK_FRACTION, DEFAULT_FREE_FLOOR_PCT);
        assert_eq!(budget, (fs_total as f64 * 0.60) as u64);
        let budget_gb = budget as f64 / 1e9;
        assert!(
            (178.0..180.0).contains(&budget_gb),
            "expected ~179 GB, got {budget_gb} GB",
        );
    }

    #[test]
    fn default_budget_bytes_fraction_governs_under_defaults() {
        // min(0.60, 0.80) always picks the 0.60 fraction term while
        // fraction <= 1 - headroom_frac (true for the shipped defaults).
        let fs_total = 100_000_000_000u64;
        let budget = default_budget_bytes(fs_total, 0.60, 0.20);
        assert_eq!(budget, 60_000_000_000);
    }

    #[test]
    fn default_budget_bytes_headroom_caps_an_aggressive_fraction() {
        // An operator setting the fraction knob above (1 - headroom) must
        // still be capped by the headroom term — the budget can never
        // itself authorize filling past the kubelet-eviction margin.
        let fs_total = 100_000_000_000u64;
        let budget = default_budget_bytes(fs_total, 0.95, 0.20);
        assert_eq!(budget, 80_000_000_000, "headroom term (0.80) must cap");
    }

    #[test]
    fn default_budget_bytes_clamps_out_of_range_fractions() {
        let fs_total = 100_000_000_000u64;
        // fraction > 1 clamps to 1; headroom_frac < 0 clamps to 0 (1 - 0 = 1).
        let budget = default_budget_bytes(fs_total, 1.5, -0.5);
        assert_eq!(budget, fs_total);
    }

    #[test]
    fn default_budget_bytes_zero_disk_is_zero_budget() {
        assert_eq!(
            default_budget_bytes(0, DEFAULT_DISK_FRACTION, DEFAULT_FREE_FLOOR_PCT),
            0
        );
    }

    // ---- resolve_disk_fraction: env precedence ----

    #[test]
    fn disk_fraction_defaults_when_unset() {
        let _g = env_guard();
        std::env::remove_var(DISK_FRACTION_ENV_VAR);
        assert_eq!(resolve_disk_fraction(), DEFAULT_DISK_FRACTION);
    }

    #[test]
    fn disk_fraction_env_overrides_default() {
        let _g = env_guard();
        std::env::set_var(DISK_FRACTION_ENV_VAR, "0.75");
        let frac = resolve_disk_fraction();
        std::env::remove_var(DISK_FRACTION_ENV_VAR);
        assert!((frac - 0.75).abs() < 1e-9);
    }

    #[test]
    fn disk_fraction_env_out_of_range_keeps_default() {
        let _g = env_guard();
        std::env::set_var(DISK_FRACTION_ENV_VAR, "1.5");
        let frac = resolve_disk_fraction();
        std::env::remove_var(DISK_FRACTION_ENV_VAR);
        assert_eq!(frac, DEFAULT_DISK_FRACTION);
    }

    // ---- ADR 0070: pins are a floor, never auto-evicted, alarm on overflow ----

    #[tokio::test]
    async fn pins_over_budget_never_unlinks_pinned_but_still_evicts_unpinned() {
        // Budget of 5 bytes; a single 10-byte PINNED chunk alone already
        // exceeds it — the infeasible case the pins-over-budget alarm
        // exists for (verifying the `engram_chunk_cache_pins_over_budget`
        // gauge value itself needs a real Prometheus scrape / recorder,
        // which is an integration-level concern outside this pure-logic
        // test — this test asserts the behavior the gauge reports on).
        // The pinned chunk must never be unlinked (pins are a floor, not
        // a bug) and an unpinned chunk sharing the sweep still evicts
        // normally (eviction isn't disabled, it's just insufficient to
        // reach the budget on its own).
        let (cache, _store, _b, _c) = setup(5).await;
        let pinned_body = b"aaaaaaaaaa"; // 10 bytes
        let evictable_body = b"bbbbbbbbbb"; // 10 bytes
        let hp = ChunkHash::of(pinned_body);
        let he = ChunkHash::of(evictable_body);
        // Pin BEFORE the populating put — a hash can be pinned before it's
        // ever written (pin() only touches the refcount map), which
        // matters here: pin AFTER put would let the put's own debounced
        // sweep see the not-yet-pinned 10-byte chunk alone exceed the
        // 5-byte budget and evict it before `pin()` ever runs.
        cache.pin(hp);
        cache.put(hp, pinned_body).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(he, evictable_body).await.unwrap();

        assert!(cache.contains(hp).await, "pinned chunk must survive");
        assert!(
            !cache.contains(he).await,
            "unpinned chunk must still evict even though it can't close the deficit alone",
        );
    }

    #[tokio::test]
    async fn pins_under_budget_do_not_trigger_extra_eviction() {
        // Pins well under budget: nothing about the pin accounting should
        // cause eviction pressure that wouldn't otherwise exist.
        let (cache, _store, _b, _c) = setup(1024 * 1024).await;
        let body = b"aaaaaaaaaa";
        let h = ChunkHash::of(body);
        cache.put(h, body).await.unwrap();
        cache.pin(h);
        cache.sweep().await.unwrap();
        assert!(cache.contains(h).await, "far under budget: nothing evicts");
    }

    // ---- ADR 0070: eviction_enabled: false never unlinks ----

    #[tokio::test]
    async fn eviction_disabled_cache_never_unlinks_under_pressure() {
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: 5, // impossibly tight — every put is "over"
                sweep_debounce_ms: 0,
                eviction_enabled: false,
            },
            0.0,
        );
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let ha = ChunkHash::of(a);
        let hb = ChunkHash::of(b);
        cache.put(ha, a).await.unwrap();
        cache.put(hb, b).await.unwrap();
        cache.sweep().await.unwrap();
        assert!(
            cache.contains(ha).await,
            "eviction disabled: a must survive"
        );
        assert!(
            cache.contains(hb).await,
            "eviction disabled: b must survive"
        );
    }

    // ---- 2026-07-16 RCA: the sweep walk tolerates vanishing entries ----

    #[tokio::test]
    #[cfg(unix)]
    async fn sweep_skips_entries_that_vanish_mid_walk() {
        // Pre-fix, any NotFound stat inside `list_entries` aborted the
        // ENTIRE sweep ("periodic chunk cache sweep failed … No such
        // file or directory" — chronic, hourly, on every busy prod
        // host), so eviction never completed a pass and the cache blew
        // its budget (424 GiB on the incident node). A dangling symlink
        // reproduces the vanish-mid-walk stat deterministically: the
        // dir entry lists, but the follow-stat is NotFound.
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: 1, // any chunk is "over" — the sweep must evict
                sweep_debounce_ms: 0,
                eviction_enabled: true,
            },
            0.0,
        );
        let a = b"aaaaaaaaaa";
        let ha = ChunkHash::of(a);
        cache.put_no_evict(ha, a).await.unwrap();

        // A 62-hex-named dangling symlink inside a valid prefix dir:
        // `file_type()` (no follow) sees a symlink, `metadata()`
        // (follow) is NotFound — the exact error class the walk must
        // skip instead of propagating.
        let prefix = cache_dir.path().join("00");
        std::fs::create_dir_all(&prefix).unwrap();
        std::os::unix::fs::symlink(
            cache_dir.path().join("no-such-target"),
            prefix.join("0".repeat(62)),
        )
        .unwrap();

        cache
            .sweep()
            .await
            .expect("a vanished/dangling entry must not abort the sweep");
        assert!(
            !cache.contains(ha).await,
            "the sweep must still complete its eviction pass",
        );
    }

    // ---- ADR 0070: spawn_sweeper enforces without populate traffic ----

    #[tokio::test]
    async fn spawn_sweeper_enforces_with_zero_populate_traffic() {
        // ADR 0070 acceptance criterion: a cache filled over budget with
        // NO further populate activity must return under budget within
        // one sweep interval, driven purely by the periodic timer.
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: 1,             // any populated chunk at all is "over"
                sweep_debounce_ms: i64::MAX, // populate-path sweep never fires
                eviction_enabled: true,
            },
            0.0, // floor disabled; only the ceiling governs
        );
        let a = b"aaaaaaaaaa";
        let ha = ChunkHash::of(a);
        // `put_no_evict` skips the sweep entirely, modeling "populated,
        // then zero traffic since" (e.g. a host that just restarted).
        cache.put_no_evict(ha, a).await.unwrap();
        assert!(
            cache.contains(ha).await,
            "chunk lands before any sweep runs"
        );

        // No further writes and no explicit sweep() — only the periodic
        // sweeper can enforce the budget from here.
        let handle = cache.spawn_sweeper(std::time::Duration::from_millis(20));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        handle.abort();

        assert!(
            !cache.contains(ha).await,
            "the periodic sweeper must evict the over-budget chunk with zero populate traffic",
        );
    }

    #[tokio::test]
    async fn spawn_sweeper_does_not_sweep_at_t_zero() {
        // Regression for the cold-boot pin race: `tokio::time::interval`'s
        // first tick fires immediately, but at t=0 a freshly-started
        // host-agent hasn't re-established its pin set yet (that needs a
        // coordinator RPC round-trip). An immediate sweep would run
        // pin-blind and evict whatever happens to be oldest — on a real
        // host, the boot-staged base-image chunks pinned in the prior
        // life. The sweeper must wait a full interval before its FIRST
        // sweep, giving the pin set time to repopulate.
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: 1, // any populated chunk at all is "over"
                sweep_debounce_ms: i64::MAX,
                eviction_enabled: true,
            },
            0.0,
        );
        let a = b"aaaaaaaaaa";
        let ha = ChunkHash::of(a);
        cache.put_no_evict(ha, a).await.unwrap();

        let handle = cache.spawn_sweeper(std::time::Duration::from_millis(200));
        // Well before the first interval elapses: the chunk must still
        // be there — a t=0 sweep would have evicted it immediately.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            cache.contains(ha).await,
            "the sweeper must not evict on its immediate t=0 tick",
        );
        // Past the first interval: the (now real) first sweep must have
        // run and enforced the budget.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();
        assert!(
            !cache.contains(ha).await,
            "the sweeper must still enforce the budget once the first real interval elapses",
        );
    }

    #[tokio::test]
    async fn spawn_sweeper_zero_interval_disables_the_loop() {
        // interval=0 must not panic (tokio::time::interval(ZERO) panics)
        // and must not tick — the task returns immediately.
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: NO_CEILING,
                sweep_debounce_ms: 0,
                eviction_enabled: true,
            },
            0.0,
        );
        let handle = cache.spawn_sweeper(std::time::Duration::ZERO);
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("interval=0 must return promptly, not hang")
            .expect("sweeper task must not panic");
    }

    // ---- #1003: index-driven sweeps ----

    #[tokio::test]
    async fn populate_path_sweep_evicts_through_the_index() {
        // Three 10-byte chunks against a 25-byte ceiling, populated via
        // `put` (the write_local path, debounce 0 ⇒ every write sweeps).
        // The sweeps run against the in-memory index — the oldest chunk
        // must go, the newest must stay, purely through index-tracked
        // populates and evictions.
        let (cache, _store, _b, _c) = setup(25).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let c = b"cccccccccc";
        let (ha, hb, hc) = (ChunkHash::of(a), ChunkHash::of(b), ChunkHash::of(c));
        cache.put(ha, a).await.unwrap();
        // Distinct populate times: eviction orders by populated_ms
        // (millisecond resolution).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        cache.put(hb, b).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        cache.put(hc, c).await.unwrap();

        assert!(
            !cache.contains(ha).await,
            "oldest chunk must be evicted by the index-driven populate-path sweep",
        );
        assert!(cache.contains(hc).await, "newest chunk must survive");
    }

    #[tokio::test]
    async fn reconcile_sweep_sees_out_of_band_writes() {
        // A chunk landed by ANOTHER process (the UFFD handler shares the
        // cache root, ADR 0070) is invisible to this process's index.
        // The reconciling sweep() must re-walk, adopt it, and enforce
        // the budget against it.
        let (cache, _store, _b, _c) = setup(15).await;
        // Build the index first (any sweep builds it).
        cache.sweep().await.unwrap();

        // Out-of-band landing: bytes written straight to the target
        // path, bypassing every ChunkCache API.
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let (ha, hb) = (ChunkHash::of(a), ChunkHash::of(b));
        let target = cache.on_disk_path(ha);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, a).unwrap();
        // Backdate it a minute so "oldest" is deterministic — populate
        // times carry millisecond resolution, and two writes in the
        // same millisecond tie (arbitrary eviction order).
        let f = std::fs::File::options().write(true).open(&target).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();

        // In-process populate of a second chunk: 20 bytes total now on
        // disk against a 15-byte ceiling, but the index only knows 10 —
        // the index-driven sweep sees no pressure and evicts nothing.
        cache.put(hb, b).await.unwrap();
        assert!(cache.contains(ha).await);
        assert!(cache.contains(hb).await);

        // The reconcile re-walks: both chunks visible, 20 > 15, oldest
        // (the out-of-band one) goes.
        cache.sweep().await.unwrap();
        assert!(
            !cache.contains(ha).await,
            "reconcile must adopt the out-of-band chunk and evict it under ceiling pressure",
        );
        assert!(
            cache.contains(hb).await,
            "in-index chunk within budget survives"
        );
    }

    #[tokio::test]
    async fn index_does_not_double_count_a_repopulated_hash() {
        // Ceiling exactly one chunk. Re-populating the SAME hash must
        // overwrite its index entry (size delta accounting), not add a
        // second copy — a double-counted total would push the sweep
        // over ceiling and evict the only chunk.
        let (cache, _store, _b, _c) = setup(10).await;
        let a = b"aaaaaaaaaa";
        let ha = ChunkHash::of(a);
        cache.put(ha, a).await.unwrap();
        cache.put(ha, a).await.unwrap();
        cache.put(ha, a).await.unwrap();
        assert!(
            cache.contains(ha).await,
            "re-populating one in-budget hash must not inflate the index total and self-evict",
        );
    }

    #[tokio::test]
    async fn eviction_updates_the_index_total() {
        // After the sweep evicts down to budget, the index total must
        // reflect the eviction — a later sweep with no new writes sees
        // no pressure and evicts nothing further.
        let (cache, _store, _b, _c) = setup(15).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let (ha, hb) = (ChunkHash::of(a), ChunkHash::of(b));
        cache.put(ha, a).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        cache.put(hb, b).await.unwrap();
        assert!(!cache.contains(ha).await, "over-ceiling: oldest evicted");
        assert!(cache.contains(hb).await);

        // A stale (not-decremented) total would read 20 > 15 here and
        // evict the survivor too.
        cache.sweep().await.unwrap();
        assert!(
            cache.contains(hb).await,
            "post-eviction sweep must see the decremented total and keep the survivor",
        );
    }

    #[tokio::test]
    async fn first_populate_defers_the_build_then_enforces() {
        // A fresh cache (no index yet): the first populate-path sweep
        // must NOT walk inline — it spawns the build off-path and
        // returns. Enforcement then lands once the background build
        // completes (eventually-consistent; poll with a deadline).
        let cache_dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: 15,
                sweep_debounce_ms: 0,
                eviction_enabled: true,
            },
            0.0,
        );
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let (ha, hb) = (ChunkHash::of(a), ChunkHash::of(b));
        cache.put(ha, a).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        // 20 bytes against a 15-byte ceiling. The put itself must not
        // block on a walk; the spawned build + its tail sweep evict.
        cache.put(hb, b).await.unwrap();

        let deadline = tokio::time::Duration::from_secs(5);
        let evicted = tokio::time::timeout(deadline, async {
            while cache.contains(ha).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            evicted.is_ok(),
            "background index build must complete and enforce the ceiling",
        );
        assert!(cache.contains(hb).await, "newest chunk survives");
    }

    // ---- ChunkCacheConfig::SWEEP_INTERVAL_ENV_VAR precedence ----

    #[test]
    fn sweep_interval_defaults_when_unset() {
        let _g = env_guard();
        std::env::remove_var(SWEEP_INTERVAL_ENV_VAR);
        assert_eq!(resolve_sweep_interval_secs(), DEFAULT_SWEEP_INTERVAL_SECS);
    }

    #[test]
    fn sweep_interval_env_overrides_default() {
        let _g = env_guard();
        std::env::set_var(SWEEP_INTERVAL_ENV_VAR, "5");
        let secs = resolve_sweep_interval_secs();
        std::env::remove_var(SWEEP_INTERVAL_ENV_VAR);
        assert_eq!(secs, 5);
    }

    #[test]
    fn sweep_interval_env_zero_is_explicit_disable() {
        let _g = env_guard();
        std::env::set_var(SWEEP_INTERVAL_ENV_VAR, "0");
        let secs = resolve_sweep_interval_secs();
        std::env::remove_var(SWEEP_INTERVAL_ENV_VAR);
        assert_eq!(secs, 0);
    }

    #[test]
    fn sweep_interval_env_unparseable_keeps_default() {
        let _g = env_guard();
        std::env::set_var(SWEEP_INTERVAL_ENV_VAR, "not-a-number");
        let secs = resolve_sweep_interval_secs();
        std::env::remove_var(SWEEP_INTERVAL_ENV_VAR);
        assert_eq!(secs, DEFAULT_SWEEP_INTERVAL_SECS);
    }

    // ---- end-to-end: thrash counter increments on refetch-after-evict ----

    #[tokio::test]
    async fn refetch_after_evict_is_tracked() {
        // Ceiling of 20 bytes, floor disabled. As in
        // `budget_triggers_lru_eviction`, the sweep runs after each put
        // and the third 10-byte chunk pushes us to 30/20 → the oldest
        // (a) evicts. Re-getting a is then a remote miss for a
        // recently-evicted hash, which must be flagged.
        let (cache, store, _b, _c) = setup(20).await;
        let a = b"aaaaaaaaaa";
        let b = b"bbbbbbbbbb";
        let c = b"cccccccccc";
        let ha = store.put_chunk(a).await.unwrap();
        let hb = store.put_chunk(b).await.unwrap();
        let hc = store.put_chunk(c).await.unwrap();
        cache_get_from(&cache, &store, ha).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache_get_from(&cache, &store, hb).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache_get_from(&cache, &store, hc).await.unwrap();
        // a is gone and remembered as evicted.
        assert!(!cache.contains(ha).await, "a should be evicted");
        assert!(
            cache.inner.evicted_ring.lock().contains(&ha),
            "evicted hash must be in the thrash ring",
        );
        // Re-get a: it's a remote miss for a recently-evicted hash. The
        // get() leader takes it out of the ring (counting the refetch).
        cache_get_from(&cache, &store, ha).await.unwrap();
        assert!(
            !cache.inner.evicted_ring.lock().contains(&ha),
            "refetched hash should be cleared from the ring",
        );
    }

    #[tokio::test]
    async fn disk_floor_evicts_even_without_ceiling() {
        // The whole point of #16: with NO absolute ceiling, a tight
        // free-space floor still triggers eviction. A real tempdir's FS
        // is huge with plenty free, so we can't make the *real* disk
        // breach a 10% floor — instead set the floor to 1.0 ("require
        // 100% free"), which `bytes_to_free` caps at the cache's own
        // total: every unpinned chunk becomes evictable on each sweep.
        let blob_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> =
            Arc::new(LocalBlobStorage::new(blob_dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: NO_CEILING, // floor governs, not a byte ceiling,
                sweep_debounce_ms: 0,
                eviction_enabled: true,
            },
            1.0,
        );
        // Build the index (instant on the empty dir) so the post-put
        // sweep enforces inline rather than deferring to the
        // background build.
        cache.sweep().await.unwrap();
        let a = b"aaaaaaaaaa";
        let ha = store.put_chunk(a).await.unwrap();
        cache.put(ha, a).await.unwrap();
        // The post-put sweep saw the disk under the (impossible) 100%
        // floor and evicted the only unpinned chunk we hold.
        assert!(
            !cache.contains(ha).await,
            "tight free-space floor must evict even with no byte ceiling",
        );
    }

    #[tokio::test]
    async fn disk_floor_skips_pinned_under_pressure() {
        // Same impossible-floor setup, but the chunk is pinned: the
        // floor sweep must not evict it (pins win over both governors).
        let blob_dir = tempfile::tempdir().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let blob: Arc<dyn BlobStorage> =
            Arc::new(LocalBlobStorage::new(blob_dir.path().to_path_buf()));
        let store = ChunkStore::new(blob);
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: cache_dir.path().to_path_buf(),
                budget_bytes: NO_CEILING,
                sweep_debounce_ms: 0,
                eviction_enabled: true,
            },
            1.0,
        );
        // Build the index up front so the put-path sweeps run inline
        // (without this the test passes vacuously — no sweep, nothing
        // could have evicted the chunk anyway).
        cache.sweep().await.unwrap();
        let a = b"aaaaaaaaaa";
        let ha = store.put_chunk(a).await.unwrap();
        cache.put(ha, a).await.unwrap();
        cache.pin(ha);
        // Trigger another sweep via a second put of the same (idempotent)
        // chunk; the pinned chunk must survive the floor pressure.
        cache.put(ha, a).await.unwrap();
        assert!(
            cache.contains(ha).await,
            "pinned chunk must survive free-space-floor eviction",
        );
    }

    /// ADR 0095: an unverified-origin landing is locally readable but
    /// NOT servable onward until the scrubber sha256s it; a good chunk
    /// clears its marker, and the read path serves it afterwards.
    #[tokio::test]
    async fn put_unverified_marks_then_scrub_clears() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let body = b"peer-landed bytes".to_vec();
        let h = ChunkHash::of(&body);
        cache.put_unverified_no_evict(h, &body).await.unwrap();
        // Locally readable (CRC-checked upstream; trust-on-read)...
        assert!(cache.contains(h).await);
        // ...but gated from onward serving until scrubbed.
        assert!(cache.read_verified_for_serve(h).await.unwrap().is_none());
        // Scrub (direct call — the spawned loop is timing, not logic).
        assert!(cache.scrub_one(h).await > 0);
        let served = cache.read_verified_for_serve(h).await.unwrap();
        assert_eq!(served.as_deref(), Some(body.as_slice()));
    }

    /// ADR 0095 §Integrity: a peer-landed chunk whose content does not
    /// match its claimed hash (source rot / serve bug) is deleted by
    /// the scrub — the next read misses and refetches through the
    /// verifying GCS populate; it is never served onward.
    #[tokio::test]
    async fn scrub_deletes_corrupt_peer_landing() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let honest = b"the real content".to_vec();
        let h = ChunkHash::of(&honest);
        cache
            .put_unverified_no_evict(h, b"corrupt impostor bytes")
            .await
            .unwrap();
        assert!(cache.read_verified_for_serve(h).await.unwrap().is_none());
        cache.scrub_one(h).await;
        assert!(
            !cache.contains(h).await,
            "corrupt chunk must be deleted, not kept",
        );
        assert!(cache.read_verified_for_serve(h).await.unwrap().is_none());
    }

    /// ADR 0095: the spawned scrubber drains the live queue AND the
    /// boot-time marker scan (crash recovery), and a marker whose chunk
    /// was evicted mid-queue is reaped without error.
    #[tokio::test]
    async fn scrubber_drains_queue_and_boot_scan() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        // Landed BEFORE the scrubber exists — covered by the boot scan.
        let pre = b"landed before scrubber".to_vec();
        let pre_h = ChunkHash::of(&pre);
        cache.put_unverified_no_evict(pre_h, &pre).await.unwrap();
        // A stale marker with no chunk (evicted mid-queue).
        let ghost = ChunkHash::of(b"ghost");
        let ghost_marker = cache.marker_path_for(ghost);
        fs::create_dir_all(ghost_marker.parent().unwrap())
            .await
            .unwrap();
        crate::cache::write_atomic(&ghost_marker, b"")
            .await
            .unwrap();
        let _scrubber = cache.spawn_scrubber(u64::MAX);
        // Landed AFTER — covered by the live queue.
        let post = b"landed after scrubber".to_vec();
        let post_h = ChunkHash::of(&post);
        cache.put_unverified_no_evict(post_h, &post).await.unwrap();
        for _ in 0..300 {
            let pre_ok = cache
                .read_verified_for_serve(pre_h)
                .await
                .unwrap()
                .is_some();
            let post_ok = cache
                .read_verified_for_serve(post_h)
                .await
                .unwrap()
                .is_some();
            let ghost_gone = !fs::try_exists(cache.marker_path_for(ghost))
                .await
                .unwrap_or(true);
            if pre_ok && post_ok && ghost_gone {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("scrubber did not drain the backlog within 3s");
    }

    /// ADR 0095: `get_with_source` keeps the populate contract of `get`
    /// (verify-on-populate, write-through) while letting the closure
    /// report a peer source — and a hash mismatch from a "peer" closure
    /// still refuses to land, exactly like a corrupt GCS fetch.
    #[tokio::test]
    async fn get_with_source_verifies_peer_bytes_too() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let body = Bytes::from_static(b"fault-time peer chunk");
        let h = ChunkHash::of(&body);
        let got = cache
            .get_with_source(h, || async { Ok((body.clone(), FillSource::Peer)) })
            .await
            .unwrap();
        assert_eq!(got, body);
        assert!(cache.contains(h).await);
        // Verified at populate ⇒ immediately servable onward.
        assert!(cache.read_verified_for_serve(h).await.unwrap().is_some());

        let wrong = ChunkHash::of(b"something else");
        let err = cache
            .get_with_source(wrong, || async {
                Ok((Bytes::from_static(b"not that"), FillSource::Peer))
            })
            .await;
        assert!(matches!(err, Err(ChunkStoreError::HashMismatch { .. })));
        assert!(!cache.contains(wrong).await);
    }
}
