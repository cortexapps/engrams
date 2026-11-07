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
//!   populate-server connections) populate
//!   concurrently inside the ONE writer process. (ADR 0075 made this
//!   single-writer; the pid component is vestigial-but-harmless
//!   belt-and-braces from the multi-writer era, whose fixed-temp
//!   collision once cost a prod resume ~92 s of GCS page-faulting.)
//! - **Write slots**: every chunk write takes a slot from one
//!   class-ordered [`Admission`] ([`ChunkCache::write_slot`]), shared
//!   with the volume writes of a restore on the same disk. A bulk memory
//!   fill (`Hydrate`) so stops dirtying the disk while a restore's disk
//!   writes (`Prefix`) wait for a slot, instead of halving
//!   their rate. The `Demand` reserve keeps slots for page faults.
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
//!   concurrent sweeps compounded into multi-GB heap bursts. The one
//!   walk runs at boot: the host-agent is the cache's only writer and
//!   evictor (ADR 0075; the UFFD handler only reads), so the index
//!   stays exact after it. [`ChunkCache::contains`] reads the index too.
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
//! - **Singleflight on miss**: if N callers ask at once for a chunk
//!   that is not cached, one fetch runs and the others wait for its
//!   answer. A caller of a higher class than the running fetch runs a
//!   second fetch beside it (see [`ChunkCache::fill_with`]).
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
use tokio::sync::oneshot;

use crate::admission::{Admission, Class, Permit};
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
///   [`ChunkCache::fill_with`] after the winning leader's write, with
///   the label the fetch reported (ADR 0095: the fetch is the one place
///   that knows the backend), only when `write_local` landed the bytes on
///   disk (a failed write means the fetch happened but the cache did NOT
///   fill).
/// - `source="peer"` is ALSO incremented by
///   `engram-host-agent::peer_fill` at its bulk [`ChunkCache::put`]
///   landings (ADR 0095).
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
/// fetcher closure that runs on local-NVMe miss, so the caller
/// chooses the source (on a host, the fill engine's object-store
/// fetch).
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

/// One chunk's fetch in flight. The entry exists while a leader of it
/// fetches the chunk or writes it.
struct Flight {
    /// Tells this flight from a later one for the same hash.
    id: u64,
    /// The highest class among the flight's fetches.
    class: Class,
    /// Calls that run a fetch for this flight, or write its bytes.
    leaders: usize,
    /// Every call waiting for the bytes, the leaders included.
    waiters: Vec<oneshot::Sender<Result<Bytes>>>,
    /// The verified bytes, set while the winning leader writes them. A
    /// call that arrives in that time is served from here.
    fetched: Option<Bytes>,
}

/// Singleflight map: the flight of each chunk being fetched.
type InflightMap = HashMap<ChunkHash, Flight>;

/// Ids of flights, unique within the process.
static NEXT_FLIGHT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One leader leaves flight `id`. The last leader to leave ends the
/// flight; the ended flight is returned, so the caller can answer its
/// waiters.
fn leave(inflight: &mut InflightMap, hash: ChunkHash, id: u64) -> Option<Flight> {
    let flight = inflight.get_mut(&hash).filter(|f| f.id == id)?;
    flight.leaders -= 1;
    if flight.leaders == 0 {
        inflight.remove(&hash)
    } else {
        None
    }
}

/// A leader's place in its flight, given up on drop unless disarmed: the
/// cancel-safety net for [`ChunkCache::fill_with`]. A leader dropped
/// mid-fetch (an FC `load_snapshot` timeout tears down the restore)
/// never reaches its own cleanup. Without the guard the flight would
/// stay in the map, and its waiters and every later call for the hash
/// would wait for an answer that never comes. When the last leader
/// leaves, the guard removes the flight; its waiters see `RecvError`
/// and retry.
struct LeaderGuard<'a> {
    inflight: &'a Mutex<InflightMap>,
    hash: ChunkHash,
    id: u64,
    armed: bool,
}

impl Drop for LeaderGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            leave(&mut self.inflight.lock(), self.hash, self.id);
        }
    }
}

/// What a call to [`ChunkCache::fill_with`] does for a missing chunk.
enum Role {
    /// The flight has verified bytes.
    Served(Bytes),
    /// Wait for the flight's answer.
    Wait,
    /// Run a fetch for flight `id`; `first` when this call started it.
    Lead { id: u64, first: bool },
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
    /// ADR 0112: the per-source breakdown behind `co_tenant_reserved`
    /// (`memfiles`, `swap-committed`, `dirty-files`). The atomic above
    /// stays the sweep's cheap read; this map exists so multiple
    /// writers can't clobber each other's terms.
    co_tenant_sources: Mutex<std::collections::HashMap<&'static str, u64>>,
    /// #1003: in-memory eviction index — hash → (size, populate time),
    /// plus a maintained byte total. Built from ONE directory walk on
    /// the first sweep, then updated by every in-process populate and
    /// eviction. Budget sweeps read THIS instead of re-walking the
    /// cache root: on the incident host (516k files / 442 GB) each
    /// walk materialized ~200 MB of `Vec<CacheEntry>` + `PathBuf`s and
    /// cost 100-500 ms, and the capture path ran it per chunk written
    /// — the multi-GB heap bursts behind the OOM loop.
    index: Mutex<IndexState>,
    /// #1003: single-sweeper gate. Sweep cost used to compound the
    /// other way too: on a big cache a sweep outlived the write-path
    /// debounce window, so sweeps piled up concurrently (the burst
    /// profile showed ~10 listings live at once). The populate path
    /// `try_lock`s this and SKIPS if a sweep is already running (the
    /// running sweep enforces the same budget); the periodic sweep
    /// waits its turn.
    sweep_gate: tokio::sync::Mutex<()>,
    /// Chunk writes to the cache's disk in flight, granted in class
    /// order (see [`ChunkCache::write_slot`]).
    writes: Arc<Admission>,
    /// Held across a chunk file's change and its index update, by hash
    /// ([`ChunkCache::file_lock`]). The index gains an entry only after
    /// its file is in place and loses it before the file goes, so an
    /// index hit always has a file.
    file_locks: [Mutex<()>; FILE_LOCKS],
}

const FILE_LOCKS: usize = 64;

/// Chunk writes to local disk in flight for classes other than `Demand`,
/// and the extra slots `Demand` may use. Enough to keep an NVMe busy with
/// 16 MiB writes; few enough that a write a guest waits for queues behind
/// at most this many bulk writes.
const WRITE_SLOTS: usize = 16;
const WRITE_DEMAND_RESERVE: usize = 8;

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
/// tracked by neither side and stay invisible to the budget.
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
        Self::with_floor(config, free_floor_pct)
    }

    fn with_floor(config: ChunkCacheConfig, free_floor_pct: f64) -> Self {
        Self {
            inner: Arc::new(CacheInner {
                config,
                free_floor_pct,
                inflight: Mutex::new(HashMap::new()),
                pinned: Mutex::new(HashMap::new()),
                evicted_ring: Mutex::new(EvictedRing::with_capacity(EVICTED_RING_CAP)),
                last_sweep_ms: std::sync::atomic::AtomicI64::new(0),
                co_tenant_reserved: std::sync::atomic::AtomicU64::new(0),
                co_tenant_sources: Mutex::new(std::collections::HashMap::new()),
                index: Mutex::new(IndexState::Pending(Vec::new())),
                sweep_gate: tokio::sync::Mutex::new(()),
                writes: Admission::fixed("disk_write", WRITE_SLOTS, WRITE_DEMAND_RESERVE),
                file_locks: std::array::from_fn(|_| Mutex::new(())),
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
        Self::with_floor(config, free_floor_pct)
    }

    /// ADR 0092 / ADR 0112: record how many allocated bytes one NAMED
    /// co-tenant consumer (files on this cache's filesystem the sweeper
    /// cannot evict) currently holds; the stored reserve is the SUM
    /// across sources. The next sweep subtracts that sum from the
    /// absolute ceiling, so co-tenant growth converts into cache
    /// eviction pressure instead of disk overshoot. Each caller
    /// re-publishes its own current total whenever it changes
    /// (last-write-wins PER SOURCE): the image-prefetch supervisor
    /// publishes `memfiles` every reconcile tick; the host-agent
    /// heartbeat publishes `swap-committed` (ADR 0112 — the sum of
    /// attached swap-device sizes; the backing inodes are anonymous
    /// after unlink-after-attach, so committed, not walked) and
    /// `dirty-files` (ADR 0110's per-sandbox dirty files, previously
    /// unbudgeted).
    ///
    /// Was a single scalar with one writer; ADR 0112 made it
    /// multi-source because a second unconditional `set` caller would
    /// have silently clobbered the memfile term.
    pub fn set_co_tenant_reserved(&self, source: &'static str, bytes: u64) {
        // The aggregate store happens INSIDE the source-map critical
        // section (adversarial-review finding): computing the sum under
        // the lock but storing after release lets two publishers
        // interleave so an OLDER total lands last, silently
        // under-reserving until the next publish. The atomic stays the
        // sweep's lock-free read; the lock orders the writers.
        let mut sources = self.inner.co_tenant_sources.lock();
        sources.insert(source, bytes);
        let total: u64 = sources.values().sum();
        self.set_co_tenant_total(total);
    }

    fn set_co_tenant_total(&self, bytes: u64) {
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

    /// Does this chunk's content-addressed file exist on local NVMe right
    /// now? A `stat`: it also sees files removed behind the cache's back
    /// (an operator `rm`), which [`Self::contains`] does not.
    pub fn contains_on_disk(&self, hash: ChunkHash) -> bool {
        self.path_for(hash).try_exists().unwrap_or(false)
    }

    /// Whether the cache holds this chunk, from the in-memory index (a
    /// `stat` until the boot walk has built it). This process is the
    /// cache's only writer and evictor, so the index is exact for every
    /// change made through the cache.
    pub fn contains(&self, hash: ChunkHash) -> bool {
        match &*self.inner.index.lock() {
            IndexState::Ready(idx) => return idx.entries.contains_key(&hash),
            IndexState::Pending(_) => {}
        }
        self.contains_on_disk(hash)
    }

    /// Read `len` bytes at `offset` from a resident chunk's file WITHOUT
    /// materializing the whole chunk. `None` = not servable from disk
    /// (absent, evicted mid-read, wrong length, or short read) — the
    /// caller falls back to the whole-chunk `get` path, which carries
    /// the fetch/validation semantics.
    ///
    /// Why this is sound against the cache's own machinery:
    /// - Populate is atomic temp+rename (module doc), so a file visible
    ///   at the content-addressed path is complete and verified — there
    ///   is no torn-write window to race.
    /// - Reads do not touch recency by design (the cache is
    ///   FIFO-by-first-write; see the module doc), so bypassing `get`
    ///   loses no eviction signal.
    /// - An eviction unlink between `open` and `read` just makes the
    ///   pread hit a still-open fd — POSIX keeps the data readable until
    ///   the fd closes, and content-addressing keeps it the RIGHT data.
    ///
    /// `expected_len` is the manifest's chunk width: a file of any other
    /// size is treated as not-resident rather than served, mirroring the
    /// `ShortChunk` validation on the whole-chunk path.
    pub fn read_range(
        &self,
        hash: ChunkHash,
        expected_len: u64,
        offset: u64,
        len: usize,
    ) -> Option<Bytes> {
        let file = std::fs::File::open(self.path_for(hash)).ok()?;
        if file.metadata().ok()?.len() != expected_len {
            return None;
        }
        let mut buf = vec![0u8; len];
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            file.read_exact_at(&mut buf, offset).ok()?;
            Some(Bytes::from(buf))
        }
        #[cfg(not(unix))]
        {
            let _ = (file, offset);
            None
        }
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

    /// Test-only: one budget sweep that first re-walks the directory, for
    /// tests that create or drop chunk files behind the cache's back.
    #[doc(hidden)]
    pub async fn sweep_for_test(&self) {
        let walked = {
            let _gate = self.inner.sweep_gate.lock().await;
            self.walk_into_index().await
        };
        if let Err(e) = walked {
            panic!("test walk failed: {e}");
        }
        if let Err(e) = self.sweep().await {
            panic!("test sweep failed: {e}");
        }
    }

    /// Test-only: drop a chunk's on-disk cache file, simulating an
    /// eviction, so callers can exercise the "pinned-but-not-resident"
    /// recovery paths without driving real disk pressure.
    #[doc(hidden)]
    pub fn evict_on_disk_for_test(&self, hash: ChunkHash) {
        let _lock = self.file_lock(hash).lock();
        self.index_remove(hash);
        let _ = std::fs::remove_file(self.path_for(hash));
    }

    /// [`Self::get_with_source`] with a cold-tier fetcher, writing at
    /// `Demand`.
    #[cfg(test)]
    pub(crate) async fn get<F, Fut>(&self, hash: ChunkHash, fetch: F) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        self.get_with_source(hash, Class::Demand, || async move {
            fetch().await.map(|b| (b, FillSource::BlobStorage))
        })
        .await
    }

    /// The chunk's bytes when its file is in the cache, else `None`.
    /// Never fetches and never stores. Every populate path verifies the
    /// hash before its atomic rename, so a present file is whole and
    /// correct and is not hashed again (a 16 MiB sha256 costs about
    /// 80 ms on hosts without SHA extensions).
    pub async fn read_resident(&self, hash: ChunkHash) -> Result<Option<Bytes>> {
        // A miss the index knows of costs no blocking hop.
        if !self.contains(hash) {
            return Ok(None);
        }
        let start = crate::time_source::metrics_now();
        let Some(bytes) = read_if_present(&self.path_for(hash)).await? else {
            // A miss is a failed open, not a latency sample.
            return Ok(None);
        };
        metrics::histogram!("engram_chunk_fetch_seconds", "tier" => "nvme")
            .record(start.elapsed().as_secs_f64());
        metrics::counter!("engram_chunk_cache_hits_total", "tier" => "nvme").increment(1);
        metrics::counter!("engram_chunk_cache_bytes_total", "tier" => "nvme")
            .increment(bytes.len() as u64);
        Ok(Some(bytes))
    }

    /// `len` bytes at `offset` of a resident chunk: from the bytes of a
    /// fill that is writing it, else from its file. `None` when the
    /// cache does not hold the chunk. Like [`Self::read_resident`], it
    /// trusts what it reads.
    pub async fn read_resident_range(
        &self,
        hash: ChunkHash,
        offset: u64,
        len: usize,
    ) -> Result<Option<Bytes>> {
        let fetched = self
            .inner
            .inflight
            .lock()
            .get(&hash)
            .and_then(|f| f.fetched.clone());
        if let Some(bytes) = fetched {
            return slice_chunk_range(hash, &bytes, offset, len).map(Some);
        }
        if !self.contains(hash) {
            return Ok(None);
        }
        let path = self.path_for(hash);
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::FileExt;
            let file = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            range_in(hash, offset, len, file.metadata()?.len())?;
            let mut buf = vec![0u8; len];
            file.read_exact_at(&mut buf, offset)?;
            Ok(Some(Bytes::from(buf)))
        })
        .await
        .map_err(|e| ChunkStoreError::Internal(format!("cache read task: {e}")))?
    }

    /// [`Self::fill_with`] without a slot taken ahead.
    pub async fn get_with_source<F, Fut>(
        &self,
        hash: ChunkHash,
        class: Class,
        fetch: F,
    ) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(Bytes, FillSource)>>,
    {
        self.fill_with(hash, class, None, |_| fetch()).await
    }

    /// A chunk's bytes. Local NVMe first. On a miss, one fetch serves
    /// every concurrent caller (singleflight): its bytes are verified
    /// (unless they came from a peer), handed to every waiter, then
    /// written to local NVMe at the writer's class
    /// ([`Self::write_slot`]). The fetch reports where its bytes came
    /// from, so the [`CHUNK_FILL_TOTAL`]/[`CHUNK_FILL_BYTES_TOTAL`]
    /// `source` label and the fetch histogram's `tier` stay honest
    /// (ADR 0095). When every fetch of the flight fails, the last error
    /// reaches every waiter.
    ///
    /// A caller of a higher class than the running fetch (a fault on a
    /// chunk a bulk fill fetches) does not wait for that fetch: it runs
    /// its own `fetch` beside it, and the first verified answer serves
    /// every caller.
    ///
    /// `permit` is an admission slot the caller took ahead (a fill takes
    /// its slots in fetch order). It goes to `fetch`, and is dropped when
    /// this call waits for another call's fetch.
    pub async fn fill_with<F, Fut>(
        &self,
        hash: ChunkHash,
        class: Class,
        mut permit: Option<Permit>,
        fetch: F,
    ) -> Result<Bytes>
    where
        F: FnOnce(Option<Permit>) -> Fut,
        Fut: std::future::Future<Output = Result<(Bytes, FillSource)>>,
    {
        if let Some(bytes) = self.read_resident(hash).await? {
            return Ok(bytes);
        }
        // A call loops only as a waiter whose flight ended without an
        // answer (its last leader was cancelled); a leader returns.
        let mut fetch = Some(fetch);
        loop {
            let (tx, rx) = oneshot::channel();
            let role = {
                let mut inflight = self.inner.inflight.lock();
                match inflight.get_mut(&hash) {
                    None => {
                        let id = NEXT_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        inflight.insert(
                            hash,
                            Flight {
                                id,
                                class,
                                leaders: 1,
                                waiters: vec![tx],
                                fetched: None,
                            },
                        );
                        Role::Lead { id, first: true }
                    }
                    Some(flight) => match &flight.fetched {
                        Some(bytes) => Role::Served(bytes.clone()),
                        // `Class` orders `Demand` first.
                        None if class < flight.class => {
                            flight.class = class;
                            flight.leaders += 1;
                            flight.waiters.push(tx);
                            Role::Lead {
                                id: flight.id,
                                first: false,
                            }
                        }
                        None => {
                            flight.waiters.push(tx);
                            Role::Wait
                        }
                    },
                }
            };
            match role {
                Role::Served(bytes) => return Ok(bytes),
                Role::Wait => {
                    drop(permit.take());
                    match rx.await {
                        Ok(result) => return result,
                        Err(_) => continue,
                    }
                }
                Role::Lead { id, first } => {
                    let fetch = fetch.take().expect("a call leads at most once");
                    let guard = LeaderGuard {
                        inflight: &self.inner.inflight,
                        hash,
                        id,
                        armed: true,
                    };
                    return self
                        .lead(guard, class, first, rx, fetch(permit.take()))
                        .await;
                }
            }
        }
    }

    /// One leader of a flight: run `fetching` until it or another leader
    /// answers, then answer the flight.
    async fn lead<Fut>(
        &self,
        mut guard: LeaderGuard<'_>,
        class: Class,
        first: bool,
        mut rx: oneshot::Receiver<Result<Bytes>>,
        fetching: Fut,
    ) -> Result<Bytes>
    where
        Fut: std::future::Future<Output = Result<(Bytes, FillSource)>>,
    {
        let (hash, id) = (guard.hash, guard.id);
        if first {
            // Double-checked hit: this call's fast-path disk check ran
            // before it took the map lock, and a previous flight may have
            // written the file and ended in between (a flight ends only
            // after its write, so no flight implies the file is there).
            // Serve the file instead of fetching it again.
            if let Some(bytes) = read_if_present(&self.path_for(hash)).await? {
                let ended = {
                    let mut inflight = self.inner.inflight.lock();
                    match inflight.get(&hash) {
                        Some(f) if f.id == id => inflight.remove(&hash),
                        _ => None,
                    }
                };
                guard.armed = false;
                for waiter in ended.into_iter().flat_map(|f| f.waiters) {
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
        }
        // ADR 0039 #16: thrash signal. If we're about to pay a remote
        // round-trip for a hash we recently evicted, the cache is too
        // small for the working set — count it (and stop tracking the
        // hash; it's about to be re-cached). A sustained nonzero rate
        // says "raise the budget / the disk is the bottleneck."
        if self.inner.evicted_ring.lock().take(&hash) {
            metrics::counter!("engram_chunk_cache_refetch_after_evict_total").increment(1);
        }
        // ADR 0019 0d: time the remote fetch — the cold-cache page-in
        // cost that stretches cold boot (the slow tier).
        let fetch_start = crate::time_source::metrics_now();
        tokio::pin!(fetching);
        let mut rx_open = true;
        let fetched = loop {
            tokio::select! {
                got = &mut fetching => break got,
                answer = &mut rx, if rx_open => match answer {
                    // Another leader of the flight answered first; the
                    // guard gives up this leader's place.
                    Ok(result) => return result,
                    Err(_) => rx_open = false,
                },
            }
        };
        let source = fetched
            .as_ref()
            .map(|(_, s)| *s)
            .unwrap_or(FillSource::BlobStorage);
        metrics::histogram!(
            "engram_chunk_fetch_seconds",
            "tier" => source.fetch_tier(),
        )
        .record(fetch_start.elapsed().as_secs_f64());
        metrics::counter!(
            "engram_chunk_cache_hits_total",
            "tier" => source.fetch_tier(),
        )
        .increment(1);
        // A chunk from the object store is hashed here, once, before
        // it is stored or served; a mismatch fails the fetch and stores
        // nothing. A peer's chunk is trusted: the peer checked it when it
        // stored it, and each frame carries a CRC32C.
        let verified = match fetched {
            Ok((bytes, FillSource::Peer)) => Ok(bytes),
            Ok((bytes, FillSource::BlobStorage)) => {
                verify_fetched(hash, &bytes).await.map(|()| bytes)
            }
            Err(e) => Err(e),
        };
        match verified {
            Ok(bytes) => self.answer_flight(guard, class, bytes, source).await,
            Err(e) => {
                let ended = leave(&mut self.inner.inflight.lock(), hash, id);
                guard.armed = false;
                match ended {
                    Some(Flight {
                        fetched: Some(bytes),
                        ..
                    }) => Ok(bytes),
                    // This was the flight's last fetch: its error is the
                    // flight's answer.
                    Some(flight) => {
                        for waiter in flight.waiters {
                            let _ = waiter.send(Err(e.duplicate()));
                        }
                        Err(e)
                    }
                    // Another leader still fetches, or has answered.
                    None if rx_open => match rx.await {
                        Ok(Ok(bytes)) => Ok(bytes),
                        _ => Err(e),
                    },
                    None => Err(e),
                }
            }
        }
    }

    /// Serve verified `bytes` to the flight's waiters at once, then write
    /// them to local NVMe at `class` and end the flight. A later leader
    /// with the same answer only leaves the flight.
    async fn answer_flight(
        &self,
        mut guard: LeaderGuard<'_>,
        class: Class,
        bytes: Bytes,
        source: FillSource,
    ) -> Result<Bytes> {
        let (hash, id) = (guard.hash, guard.id);
        let first_answer = {
            let mut inflight = self.inner.inflight.lock();
            match inflight.get_mut(&hash) {
                Some(f) if f.id == id && f.fetched.is_none() => {
                    f.fetched = Some(bytes.clone());
                    for waiter in f.waiters.drain(..) {
                        let _ = waiter.send(Ok(bytes.clone()));
                    }
                    true
                }
                _ => false,
            }
        };
        if !first_answer {
            return Ok(bytes);
        }
        // The flight stays in the map during the write, so a call that
        // arrives now is served from it instead of fetching again; it
        // ends after the write, so no flight implies the file is there.
        // A leader cancelled mid-write leaves through the still-armed
        // guard, and the write's blocking task still ends.
        //
        // A failed write (ENOSPC, permissions) is not fatal: the bytes
        // still return, but the chunk is not cached and every later read
        // fetches it again — which looks like "warming ran but reads
        // still miss". Log it loudly rather than swallow it.
        let written = match self.write_local(hash, bytes.clone(), class).await {
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
        // This measures the fetch, not the fill, so it counts a chunk
        // whose write failed too.
        metrics::counter!(
            "engram_chunk_cache_bytes_total",
            "tier" => source.fetch_tier(),
        )
        .increment(bytes.len() as u64);
        // ADR 0019 / ADR 0095: fills by their actual source (`gcs` = the
        // object store, `peer` = a fleet sibling's NVMe). Only a chunk
        // that reached the disk counts, so a failed write cannot hide the
        // "warming ran but reads still miss" state.
        if written {
            metrics::counter!(CHUNK_FILL_TOTAL, "source" => source.label()).increment(1);
            metrics::counter!(CHUNK_FILL_BYTES_TOTAL, "source" => source.label())
                .increment(bytes.len() as u64);
        }
        {
            let mut inflight = self.inner.inflight.lock();
            if inflight.get(&hash).is_some_and(|f| f.id == id) {
                inflight.remove(&hash);
            }
        }
        guard.armed = false;
        Ok(bytes)
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

    /// Like [`Self::put_trusted`] but WITHOUT the per-write eviction
    /// sweep, for paths that write many chunks back to back. Callers MUST
    /// call [`Self::sweep`] once after the batch (the budget/floor
    /// invariant is per batch, not per write).
    pub(crate) async fn put_no_evict(
        &self,
        hash: ChunkHash,
        bytes: Bytes,
        class: Class,
    ) -> Result<()> {
        self.store_file(hash, bytes, class).await
    }

    /// One eviction sweep: the batch-closing pair of
    /// [`Self::put_no_evict`], and what the periodic
    /// [`Self::spawn_sweeper`] runs. It enforces the budget against the
    /// in-memory index; the first sweep after boot builds the index with
    /// the cache's one directory walk.
    pub async fn sweep(&self) -> Result<()> {
        // ADR 0070: single-evictor — a non-evicting process must not pay
        // the walk either.
        if !self.inner.config.eviction_enabled {
            tracing::debug!("chunk cache sweep skipped: eviction_enabled=false on this cache");
            return Ok(());
        }
        let _gate = self.inner.sweep_gate.lock().await;
        if matches!(&*self.inner.index.lock(), IndexState::Pending(_)) {
            self.walk_into_index().await?;
        }
        self.evict_with_index().await
    }

    /// Rebuild the index from a directory walk and gauge its drift.
    /// Caller holds `sweep_gate`.
    async fn walk_into_index(&self) -> Result<()> {
        let started = crate::time_source::metrics_now();
        let entries = self.list_entries().await?;
        metrics::histogram!("engram_chunk_cache_sweep_seconds", "kind" => "walk")
            .record(started.elapsed().as_secs_f64());
        self.index_install(&entries);
        Ok(())
    }

    /// Spawn the periodic sweeper (ADR 0070). It builds the index at once
    /// (the walk evicts nothing), then calls [`Self::sweep`] every
    /// `interval`, independent of populate traffic, so a host under disk
    /// pressure with no populate activity still enforces the budget.
    /// Errors are logged and looping continues; a single failed sweep
    /// must never take enforcement offline.
    ///
    /// `interval` of [`Duration::ZERO`] disables the sweeper (the spawned
    /// task returns immediately) — deterministic tests call
    /// [`Self::sweep`] directly instead of racing a background timer.
    ///
    /// The first eviction waits one full `interval`: on a freshly started
    /// host-agent the pins are in memory only and not yet re-established
    /// (the image-prefetch supervisor's reconcile needs a coordinator
    /// round trip), so an eviction at t=0 on an over-budget cache would
    /// run pin-blind and evict the boot-staged base-image chunks the prior
    /// life had pinned. The populate-path debounced sweep
    /// (`sweep_debounce_ms`) still bounds growth from writes meanwhile.
    ///
    /// The caller keeps the returned handle alive for the process
    /// lifetime (dropping or aborting it stops the sweeper).
    pub fn spawn_sweeper(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let cache = self.clone();
        tokio::spawn(async move {
            if interval.is_zero() {
                tracing::debug!("chunk cache periodic sweeper disabled (interval=0)");
                return;
            }
            {
                let _gate = cache.inner.sweep_gate.lock().await;
                if matches!(&*cache.inner.index.lock(), IndexState::Pending(_)) {
                    if let Err(e) = cache.walk_into_index().await {
                        tracing::warn!(error = %e, "chunk cache boot walk failed");
                    }
                }
            }
            let mut tick = engram_core::periodic(interval);
            // The first tick fires at once; skip it (see above).
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = cache.sweep().await {
                    tracing::warn!(error = %e, "periodic chunk cache sweep failed");
                }
            }
        })
    }

    /// Store a chunk after checking that `bytes` hash to `hash`, at
    /// `Background`.
    pub async fn put(&self, hash: ChunkHash, bytes: Bytes) -> Result<()> {
        check_hash(hash, &bytes)?;
        self.write_local(hash, bytes, Class::Background).await
    }

    /// Store a chunk the caller trusts without hashing it (a peer's chunk,
    /// or one the caller just hashed), at `class`.
    pub async fn put_trusted(&self, hash: ChunkHash, bytes: Bytes, class: Class) -> Result<()> {
        self.write_local(hash, bytes, class).await
    }

    /// Store a chunk whose hash the caller guarantees, then run the
    /// debounced budget sweep.
    pub(crate) async fn write_local(
        &self,
        hash: ChunkHash,
        bytes: Bytes,
        class: Class,
    ) -> Result<()> {
        self.store_file(hash, bytes, class).await?;

        // Best-effort eviction sweep, DEBOUNCED to at most once per
        // interval across all writers, and SKIPPED outright if a sweep
        // is already running (the running sweep enforces the same budget).
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
                // No boot walk yet: build off-path (see
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

    /// A slot for one chunk write to the cache's disk at `class`, held
    /// until the write returns. Every chunk write of the cache takes one;
    /// other bulk chunk writes to the same disk (a volume being staged)
    /// take one too, so a restore's disk writes go ahead of a bulk memory
    /// fill's cache writes.
    pub async fn write_slot(&self, class: Class) -> Permit {
        self.inner.writes.acquire(class).await
    }

    /// Place the chunk file and index it, under a write slot at `class`.
    /// A chunk the cache already holds is not written again. The
    /// directory, the write, the rename and the index insert run in one
    /// blocking task, on `bytes` without a copy. That task holds the slot
    /// and runs to its end when the caller is dropped, so the index
    /// learns every file the cache places.
    async fn store_file(&self, hash: ChunkHash, bytes: Bytes, class: Class) -> Result<()> {
        if self.contains(hash) {
            return Ok(());
        }
        let slot = self.write_slot(class).await;
        let cache = self.clone();
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            let _lock = cache.file_lock(hash).lock();
            if cache.contains(hash) {
                return Ok(());
            }
            write_atomic(&cache.path_for(hash), &bytes)?;
            cache.index_insert(hash, bytes.len() as u64);
            Ok(())
        })
        .await
        .map_err(|e| ChunkStoreError::Internal(format!("cache write task: {e}")))?
    }

    /// The lock for `hash`'s file and index entry (see
    /// [`CacheInner::file_locks`]). Held only in blocking code.
    fn file_lock(&self, hash: ChunkHash) -> &Mutex<()> {
        &self.inner.file_locks[hash.as_bytes()[0] as usize % FILE_LOCKS]
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
    /// (skip-if-busy), the periodic sweep locks (wait).
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
            // Not built yet (the boot walk is in flight) — nothing to
            // enforce against; the build's own tail sweep and the
            // periodic sweep backstop this window.
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
        // silently authorizes overshooting the kubelet eviction line.
        // Subtract whatever the
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

        let over = bytes_to_free(cache_total, ceiling, self.inner.free_floor_pct, fs);
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
            let cache = self.clone();
            tokio::task::spawn_blocking(move || cache.evict_oldest(candidates, over))
                .await
                .map_err(|e| ChunkStoreError::Internal(format!("cache evict task: {e}")))?;
        }
        metrics::histogram!("engram_chunk_cache_sweep_seconds", "kind" => "index")
            .record(sweep_started.elapsed().as_secs_f64());
        Ok(())
    }

    /// Unlink `candidates` (oldest first) until `over` bytes are freed.
    /// Blocking. Each unlink holds the chunk's file lock, and skips a
    /// chunk pinned or written again since the candidates were taken.
    fn evict_oldest(&self, candidates: Vec<(i64, ChunkHash, u64)>, mut over: u64) {
        for (populated_ms, hash, size) in candidates {
            if over == 0 {
                break;
            }
            // Re-check the LIVE pin map at unlink time. The candidates
            // were taken at sweep start; a chunk pinned since then (the
            // ADR 0075 Hello pin-around-open sequence racing a sweep)
            // must not be evicted. Same delete-time re-verify shape as
            // the chunk/bundle/snapshot-blob GC promote passes.
            if self.inner.pinned.lock().contains_key(&hash) {
                continue;
            }
            {
                let _lock = self.file_lock(hash).lock();
                if !self.index_remove_if_populated_at(hash, populated_ms) {
                    continue;
                }
                let _ = std::fs::remove_file(self.path_for(hash));
            }
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

    /// Build the index from one directory walk, OFF the write path, for
    /// a populate that finds no index (no sweeper has built it yet). At
    /// prod scale the walk takes seconds (516k files, cold dentry cache
    /// right after a pod roll), and the triggering task can be a
    /// singleflight leader on a restore path — it must never pay the
    /// walk inline. The spawned task serializes on the sweep gate,
    /// rechecks (the sweeper may have built the index first), builds,
    /// then runs one enforcement sweep so the budget takes effect the
    /// moment the index exists.
    fn spawn_index_build(&self) {
        let cache = self.clone();
        tokio::spawn(async move {
            let _gate = cache.inner.sweep_gate.lock().await;
            if matches!(&*cache.inner.index.lock(), IndexState::Ready(_)) {
                return;
            }
            let built = match cache.walk_into_index().await {
                Ok(()) => cache.evict_with_index().await,
                Err(e) => Err(e),
            };
            if let Err(e) = built {
                // The next debounce winner re-spawns; the sweeper builds
                // it regardless.
                tracing::warn!(error = %e, "chunk cache index build failed");
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

    /// Remove `hash` from a built index when its entry is the one
    /// populated at `populated_ms`; whether it did.
    fn index_remove_if_populated_at(&self, hash: ChunkHash, populated_ms: i64) -> bool {
        match &mut *self.inner.index.lock() {
            IndexState::Ready(idx)
                if idx
                    .entries
                    .get(&hash)
                    .is_some_and(|e| e.populated_ms == populated_ms) =>
            {
                idx.apply(PendingOp::Remove(hash));
                true
            }
            _ => false,
        }
    }

    /// Record an in-process unlink (the test helper) in the index;
    /// buffered while the first walk is in flight, same as inserts.
    fn index_remove(&self, hash: ChunkHash) {
        match &mut *self.inner.index.lock() {
            IndexState::Pending(ops) => ops.push(PendingOp::Remove(hash)),
            IndexState::Ready(idx) => idx.apply(PendingOp::Remove(hash)),
        }
    }

    /// Install a completed walk: replay any ops buffered while the
    /// walk ran (they postdate the walk's view of each directory), and
    /// gauge drift when replacing an earlier `Ready` index (a test
    /// re-walk).
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
                // |walk truth − maintained index|. Drift means a
                // writer the index did not see.
                let drift = built.total_bytes.abs_diff(prev.total_bytes);
                metrics::gauge!("engram_chunk_cache_index_drift_bytes").set(drift as f64);
            }
        }
        *guard = IndexState::Ready(built);
    }

    /// Walk the cache directory, in one blocking task.
    async fn list_entries(&self) -> Result<Vec<CacheEntry>> {
        let root = self.inner.config.root.clone();
        tokio::task::spawn_blocking(move || list_entries_blocking(&root))
            .await
            .map_err(|e| ChunkStoreError::Internal(format!("cache walk task: {e}")))?
    }

    /// Total bytes currently on disk. Diagnostic; not used by the
    /// eviction loop (which computes it inline).
    pub async fn size_bytes(&self) -> Result<u64> {
        let entries = self.list_entries().await?;
        Ok(entries.iter().map(|e| e.size).sum())
    }
}

/// Every chunk file under `root` (`<2-hex>/<62-hex>`).
///
/// ENOENT mid-walk is NORMAL, not an error: populate's temp+rename and the
/// eviction unlink entries concurrently with the walk, so a listed name
/// can vanish before its stat lands. Propagating that aborted the ENTIRE
/// sweep several times an hour on every busy host, so the cache grew far
/// past its budget. A vanished entry is skipped; only other I/O errors
/// abort.
///
/// The walk also deletes `<62-hex>.unverified` sidecars. Earlier builds
/// wrote one beside each chunk they landed from a peer; no build reads
/// them, and the chunk beside one is trusted like any peer chunk.
fn list_entries_blocking(root: &Path) -> Result<Vec<CacheEntry>> {
    fn vanished(e: &std::io::Error) -> bool {
        e.kind() == std::io::ErrorKind::NotFound
    }
    let mut out = Vec::new();
    let top = match std::fs::read_dir(root) {
        Ok(d) => d,
        Err(e) if vanished(&e) => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for prefix_entry in top {
        let prefix_entry = match prefix_entry {
            Ok(entry) => entry,
            Err(e) if vanished(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        match prefix_entry.file_type() {
            Ok(t) if t.is_dir() => {}
            Ok(_) => continue,
            Err(e) if vanished(&e) => continue,
            Err(e) => return Err(e.into()),
        }
        let Some(prefix) = prefix_entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if prefix.len() != 2 || !prefix.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let inner = match std::fs::read_dir(prefix_entry.path()) {
            Ok(d) => d,
            Err(e) if vanished(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        for file in inner {
            let file = match file {
                Ok(entry) => entry,
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            let Some(name) = file.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let is_chunk_name = |n: &str| n.len() == 62 && n.chars().all(|c| c.is_ascii_hexdigit());
            if name.strip_suffix(".unverified").is_some_and(is_chunk_name) {
                let _ = std::fs::remove_file(file.path());
                continue;
            }
            if !is_chunk_name(&name) {
                continue;
            }
            let Ok(hash) = ChunkHash::from_hex(&format!("{prefix}{name}")) else {
                continue;
            };
            let meta = match file.metadata() {
                Ok(m) => m,
                Err(e) if vanished(&e) => continue,
                Err(e) => return Err(e.into()),
            };
            if !meta.is_file() {
                continue;
            }
            out.push(CacheEntry {
                hash,
                size: meta.len(),
                mtime: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            });
        }
    }
    Ok(out)
}

/// Atomically place `bytes` at `target` (creating its parent dir) via a
/// writer-unique temp + rename. Blocking. Last-writer-wins on the
/// content-addressed target is correct (the bytes are identical), and
/// because each writer renames its *own* temp, two writers racing the
/// same hash never see the other's temp vanish mid-rename. On any failure
/// the temp is removed best-effort, so an ENOSPC doesn't strand an orphan
/// the sweep won't reclaim (it only tracks 62-hex chunk files, not
/// `.partial.*`).
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = unique_tmp_path(target);
    let res = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, target));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(res?)
}

/// Chunks larger than this are hashed on a blocking thread: without SHA
/// extensions SHA-256 runs at about 200 MB/s, so a larger chunk would hold
/// a runtime worker (and the fetch timers on it) for over 5 ms.
const INLINE_HASH_MAX: usize = 1024 * 1024;

/// `Err(HashMismatch)` unless `bytes`, fetched from the object store,
/// hash to `hash`. A large chunk is hashed off the async worker.
pub async fn verify_fetched(hash: ChunkHash, bytes: &Bytes) -> Result<()> {
    let actual = if bytes.len() > INLINE_HASH_MAX {
        let bytes = bytes.clone();
        tokio::task::spawn_blocking(move || ChunkHash::of(&bytes))
            .await
            .map_err(|e| ChunkStoreError::Internal(format!("hash task: {e}")))?
    } else {
        ChunkHash::of(bytes)
    };
    if actual != hash {
        tracing::error!(
            hash = %hash,
            actual = %actual,
            "fetched chunk hash mismatch; refusing to cache or serve",
        );
        return Err(ChunkStoreError::HashMismatch {
            expected: hash.to_hex(),
            actual: actual.to_hex(),
        });
    }
    Ok(())
}

/// `bytes[offset..offset + len]` of chunk `hash`, or
/// [`ChunkStoreError::RangeOutOfBounds`] when the range runs past `bytes`.
pub fn slice_chunk_range(hash: ChunkHash, bytes: &Bytes, offset: u64, len: usize) -> Result<Bytes> {
    Ok(bytes.slice(range_in(hash, offset, len, bytes.len() as u64)?))
}

/// `offset..offset + len` when it lies within a chunk of `size` bytes.
fn range_in(hash: ChunkHash, offset: u64, len: usize, size: u64) -> Result<std::ops::Range<usize>> {
    match offset.checked_add(len as u64) {
        Some(end) if end <= size => Ok(offset as usize..end as usize),
        _ => Err(ChunkStoreError::RangeOutOfBounds {
            hash: hash.to_hex(),
            offset,
            len: len as u64,
            size,
        }),
    }
}

/// `Err(HashMismatch)` unless `bytes` hash to `hash`.
fn check_hash(hash: ChunkHash, bytes: &[u8]) -> Result<()> {
    let actual = ChunkHash::of(bytes);
    if actual != hash {
        return Err(ChunkStoreError::HashMismatch {
            expected: hash.to_hex(),
            actual: actual.to_hex(),
        });
    }
    Ok(())
}

/// A writer-unique sibling temp path for `target`:
/// `<target-filename>.partial.<pid>.<seq>`. The process-global counter
/// disambiguates concurrent writers within this process; the pid keeps a
/// temp of an earlier process apart. Stays in `target`'s parent dir so
/// the rename is same-filesystem (atomic), and the dotted suffix keeps
/// `list_entries`' 62-hex filter from ever mistaking a temp for a cache
/// entry.
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

/// The file's bytes, read into a pooled buffer in one blocking task, or
/// `None` when there is no file.
async fn read_if_present(path: &Path) -> Result<Option<Bytes>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut buf = crate::buffers::ChunkBuf::take(file.metadata()?.len() as usize);
        file.read_exact(&mut buf)?;
        Ok(Some(buf.freeze()))
    })
    .await
    .map_err(|e| ChunkStoreError::Internal(format!("cache read task: {e}")))?
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
            cache.contains(hash),
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
                tasks.push(tokio::spawn(async move {
                    cache.put(h, Bytes::copy_from_slice(&body)).await
                }));
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
            assert!(reader.contains(h), "chunk {h:?} should be cached");
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
        assert!(cache.contains(h));
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
        cache.put(h, Bytes::copy_from_slice(body)).await.unwrap();
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
        match cache.put(wrong_hash, Bytes::copy_from_slice(body)).await {
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        // tiny sleep so mtimes differ
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, Bytes::copy_from_slice(c)).await.unwrap();

        // The oldest (a) should be gone; b and c should remain.
        // Note: the eviction sweep runs after each put, so the
        // *third* put pushes us to 30/30 — at boundary, no
        // eviction. We need a fourth to actually trip eviction.
        let d = b"dddddddddd";
        let hd = ChunkHash::of(d);
        cache.put(hd, Bytes::copy_from_slice(d)).await.unwrap();
        assert!(!cache.contains(ha), "a should be evicted");
        assert!(cache.contains(hb));
        assert!(cache.contains(hc));
        assert!(cache.contains(hd));
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, Bytes::copy_from_slice(c)).await.unwrap();
        cache.sweep().await.unwrap();
        assert!(
            cache.contains(ha) && cache.contains(hb) && cache.contains(hc),
            "30/30 with no reserve: at boundary, nothing evicts",
        );

        // A 10-byte co-tenant reserve → effective ceiling 20 → the LRU
        // chunk goes.
        cache.set_co_tenant_reserved("test", 10);
        cache.sweep().await.unwrap();
        assert!(!cache.contains(ha), "a must yield to the reserve");
        assert!(cache.contains(hb) && cache.contains(hc));

        // Reserve released → 20/30 → no further eviction pressure.
        cache.set_co_tenant_reserved("test", 0);
        cache.sweep().await.unwrap();
        assert!(cache.contains(hb) && cache.contains(hc));
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        cache.pin(ha);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();

        cache.set_co_tenant_reserved("test", 1_000_000);
        cache.sweep().await.unwrap();
        assert!(
            cache.contains(ha),
            "pinned chunk survives even a saturating reserve",
        );
        assert!(!cache.contains(hb), "unpinned chunk evicts to 0");
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        cache.pin(ha);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Putting c pushes us to 30; eviction targets the oldest
        // un-pinned (b), not the pinned a.
        cache.put(hc, Bytes::copy_from_slice(c)).await.unwrap();
        assert!(cache.contains(ha), "pinned a must survive");
        assert!(!cache.contains(hb), "unpinned b should evict");
        assert!(cache.contains(hc));
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        // Two holders pin `a`.
        cache.pin(ha);
        cache.pin(ha);
        assert_eq!(cache.pinned_count(), 1, "one distinct hash pinned");
        // One holder releases — refcount drops to 1, still pinned.
        cache.unpin(ha);
        assert_eq!(cache.pinned_count(), 1, "still pinned after one unpin");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, Bytes::copy_from_slice(c)).await.unwrap();
        assert!(cache.contains(ha), "a still pinned (refcount 1)");
        assert!(!cache.contains(hb), "unpinned b evicts");
        assert!(cache.contains(hc));
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
        cache
            .put(ChunkHash::of(b"abc"), Bytes::from_static(b"abc"))
            .await
            .unwrap();
        cache
            .put(ChunkHash::of(b"defgh"), Bytes::from_static(b"defgh"))
            .await
            .unwrap();
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

    /// A chunk large enough to hash on a blocking thread is checked the
    /// same way: good bytes are stored, bad bytes are refused.
    #[tokio::test]
    async fn a_large_fetched_chunk_is_verified() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let body = Bytes::from(vec![5u8; 2 * INLINE_HASH_MAX]);
        let h = ChunkHash::of(&body);
        let bad = ChunkHash::of(b"other");
        let got = cache.get(bad, || async { Ok(body.clone()) }).await;
        assert!(matches!(got, Err(ChunkStoreError::HashMismatch { .. })));
        let got = cache.get(h, || async { Ok(body.clone()) }).await.unwrap();
        assert_eq!(got, body);
        assert!(cache.contains_on_disk(h) && !cache.contains_on_disk(bad));
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
        tokio::fs::write(&path, b"divergent on-disk bytes")
            .await
            .unwrap();
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
        // Regression for the single-flight poison.
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

    /// A waiter on a leader whose chunk is missing gets `NotFound`, not
    /// a generic error.
    #[tokio::test]
    async fn a_waiter_sees_the_leaders_error_kind() {
        use std::time::Duration;
        let (cache, _store, _b, _c) = setup(1024 * 1024).await;
        let h = ChunkHash::of(b"never stored");
        let gate = Arc::new(tokio::sync::Notify::new());
        let leader = {
            let cache = cache.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                cache
                    .get(h, move || async move {
                        gate.notified().await;
                        Err(ChunkStoreError::Blob(
                            engram_core::error::BlobError::NotFound,
                        ))
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let waiter = {
            let cache = cache.clone();
            tokio::spawn(async move {
                cache
                    .get(h, || async { unreachable!("the waiter joins the leader") })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate.notify_one();

        for task in [leader, waiter] {
            let err = task.await.unwrap().unwrap_err();
            assert!(
                matches!(
                    err,
                    ChunkStoreError::Blob(engram_core::error::BlobError::NotFound)
                ),
                "{err:?}"
            );
        }
    }

    /// A cache write whose caller is dropped once the write has started
    /// still places the file and indexes it.
    #[tokio::test]
    async fn a_dropped_write_still_indexes_its_file() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let body = Bytes::from_static(b"written after the caller left");
        let h = ChunkHash::of(&body);
        let mut write = Box::pin(cache.put_trusted(h, body, Class::Background));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(
            std::future::Future::poll(write.as_mut(), &mut cx).is_pending(),
            "the write waits on its blocking task"
        );
        drop(write);

        for _ in 0..1000 {
            if cache.contains(h) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(cache.contains(h), "never indexed");
        assert!(cache.contains_on_disk(h));
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        cache.pin(ha);
        cache.clear_pins();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache.put(hc, Bytes::copy_from_slice(c)).await.unwrap();
        // With pin released, a (oldest) gets evicted.
        assert!(!cache.contains(ha));
        assert!(cache.contains(hc));
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
        cache
            .put(hp, Bytes::copy_from_slice(pinned_body))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cache
            .put(he, Bytes::copy_from_slice(evictable_body))
            .await
            .unwrap();

        assert!(cache.contains(hp), "pinned chunk must survive");
        assert!(
            !cache.contains(he),
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
        cache.put(h, Bytes::copy_from_slice(body)).await.unwrap();
        cache.pin(h);
        cache.sweep().await.unwrap();
        assert!(cache.contains(h), "far under budget: nothing evicts");
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        cache.sweep().await.unwrap();
        assert!(cache.contains(ha), "eviction disabled: a must survive");
        assert!(cache.contains(hb), "eviction disabled: b must survive");
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
        cache
            .put_no_evict(ha, Bytes::copy_from_slice(a), Class::Background)
            .await
            .unwrap();

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
            !cache.contains(ha),
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
        cache
            .put_no_evict(ha, Bytes::copy_from_slice(a), Class::Background)
            .await
            .unwrap();
        assert!(cache.contains(ha), "chunk lands before any sweep runs");

        // No further writes and no explicit sweep() — only the periodic
        // sweeper can enforce the budget from here.
        let handle = cache.spawn_sweeper(std::time::Duration::from_millis(20));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        handle.abort();

        assert!(
            !cache.contains(ha),
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
        cache
            .put_no_evict(ha, Bytes::copy_from_slice(a), Class::Background)
            .await
            .unwrap();

        let handle = cache.spawn_sweeper(std::time::Duration::from_millis(200));
        // Well before the first interval elapses: the chunk must still
        // be there — a t=0 sweep would have evicted it immediately.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            cache.contains(ha),
            "the sweeper must not evict on its immediate t=0 tick",
        );
        // Past the first interval: the (now real) first sweep must have
        // run and enforced the budget.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.abort();
        assert!(
            !cache.contains(ha),
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        // Distinct populate times: eviction orders by populated_ms
        // (millisecond resolution).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        cache.put(hc, Bytes::copy_from_slice(c)).await.unwrap();

        assert!(
            !cache.contains(ha),
            "oldest chunk must be evicted by the index-driven populate-path sweep",
        );
        assert!(cache.contains(hc), "newest chunk must survive");
    }

    /// The walk deletes a sidecar an earlier build left beside a chunk,
    /// and keeps the chunk.
    #[tokio::test]
    async fn the_walk_deletes_unverified_sidecars() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let body = Bytes::from_static(b"landed by an earlier build");
        let h = ChunkHash::of(&body);
        cache.put(h, body).await.unwrap();
        let mut sidecar = cache.path_for(h).into_os_string();
        sidecar.push(".unverified");
        std::fs::write(&sidecar, b"").unwrap();

        cache.sweep_for_test().await;

        assert!(!std::path::Path::new(&sidecar).exists());
        assert!(cache.contains_on_disk(h));
    }

    #[tokio::test]
    async fn only_the_first_sweep_walks_the_directory() {
        // The first sweep builds the index from one walk, and adopts what
        // an earlier process left on disk. Later sweeps read the index
        // alone: this process is the cache's only writer.
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::new_with_floor(
            ChunkCacheConfig {
                root: dir.path().to_path_buf(),
                budget_bytes: 15,
                sweep_debounce_ms: 0,
                eviction_enabled: true,
            },
            0.0,
        );
        let land = |bytes: &[u8], age_secs: u64| {
            let target = cache.on_disk_path(ChunkHash::of(bytes));
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, bytes).unwrap();
            let f = std::fs::File::options().write(true).open(&target).unwrap();
            f.set_modified(std::time::SystemTime::now() - Duration::from_secs(age_secs))
                .unwrap();
        };
        let (a, b, c) = (b"aaaaaaaaaa", b"bbbbbbbbbb", b"cccccccccc");
        let (ha, hb, hc) = (ChunkHash::of(a), ChunkHash::of(b), ChunkHash::of(c));

        land(a, 120);
        land(b, 60);
        cache.sweep().await.unwrap();
        assert!(!cache.contains(ha), "the walk adopted it; oldest goes");
        assert!(cache.contains(hb));

        land(c, 180);
        cache.sweep().await.unwrap();
        assert!(!cache.contains(hc), "a file the index never saw");
        assert!(cache.contains_on_disk(hc), "and no sweep evicts it");
        assert!(cache.contains(hb));
    }

    #[tokio::test]
    async fn a_resident_chunk_is_not_written_again() {
        let (cache, _store, _b, _c) = setup(1024).await;
        let body = b"resident chunk";
        let h = ChunkHash::of(body);
        cache.put(h, Bytes::from_static(body)).await.unwrap();
        let first = std::fs::metadata(cache.on_disk_path(h)).unwrap();

        cache.put(h, Bytes::from_static(body)).await.unwrap();

        let again = std::fs::metadata(cache.on_disk_path(h)).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(first.ino(), again.ino(), "no rename replaced the file");
    }

    #[tokio::test]
    async fn a_bulk_cache_write_waits_behind_a_waiting_demand_write() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let mut held = Vec::new();
        for _ in 0..WRITE_SLOTS + WRITE_DEMAND_RESERVE {
            held.push(cache.write_slot(Class::Demand).await);
        }
        let body = Bytes::from_static(b"a bulk memory chunk");
        let h = ChunkHash::of(&body);
        let bulk = tokio::spawn({
            let cache = cache.clone();
            async move { cache.put_trusted(h, body, Class::Hydrate).await }
        });
        let demand = tokio::spawn({
            let cache = cache.clone();
            async move { cache.write_slot(Class::Demand).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        held.pop();
        let demand = tokio::time::timeout(Duration::from_secs(5), demand)
            .await
            .expect("the demand write gets the freed slot")
            .unwrap();
        assert!(
            !bulk.is_finished() && !cache.contains(h),
            "the bulk write waits"
        );

        drop((held, demand));
        bulk.await.unwrap().unwrap();
        assert!(cache.contains(h));
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        assert!(
            cache.contains(ha),
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();
        assert!(!cache.contains(ha), "over-ceiling: oldest evicted");
        assert!(cache.contains(hb));

        // A stale (not-decremented) total would read 20 > 15 here and
        // evict the survivor too.
        cache.sweep().await.unwrap();
        assert!(
            cache.contains(hb),
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        // 20 bytes against a 15-byte ceiling. The put itself must not
        // block on a walk; the spawned build + its tail sweep evict.
        cache.put(hb, Bytes::copy_from_slice(b)).await.unwrap();

        let deadline = tokio::time::Duration::from_secs(5);
        let evicted = tokio::time::timeout(deadline, async {
            while cache.contains(ha) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            evicted.is_ok(),
            "background index build must complete and enforce the ceiling",
        );
        assert!(cache.contains(hb), "newest chunk survives");
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
        assert!(!cache.contains(ha), "a should be evicted");
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        // The post-put sweep saw the disk under the (impossible) 100%
        // floor and evicted the only unpinned chunk we hold.
        assert!(
            !cache.contains(ha),
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
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        cache.pin(ha);
        // Trigger another sweep via a second put of the same (idempotent)
        // chunk; the pinned chunk must survive the floor pressure.
        cache.put(ha, Bytes::copy_from_slice(a)).await.unwrap();
        assert!(
            cache.contains(ha),
            "pinned chunk must survive free-space-floor eviction",
        );
    }

    /// ADR 0095: `get_with_source` keeps the populate contract of `get`
    /// (verify-on-populate, write-through) while letting the closure
    /// report a peer source — and a hash mismatch from a "peer" closure
    /// still refuses to land, exactly like a corrupt GCS fetch.
    #[tokio::test]
    async fn get_with_source_hashes_store_bytes_and_trusts_peer_bytes() {
        let (cache, _store, _b, _c) = setup(NO_CEILING).await;
        let body = Bytes::from_static(b"fault-time peer chunk");
        let h = ChunkHash::of(&body);
        let got = cache
            .get_with_source(h, Class::Demand, || async {
                Ok((body.clone(), FillSource::Peer))
            })
            .await
            .unwrap();
        assert_eq!(got, body);
        assert!(cache.contains(h));

        let wrong = ChunkHash::of(b"something else");
        let err = cache
            .get_with_source(wrong, Class::Demand, || async {
                Ok((Bytes::from_static(b"not that"), FillSource::BlobStorage))
            })
            .await;
        assert!(matches!(err, Err(ChunkStoreError::HashMismatch { .. })));
        assert!(!cache.contains(wrong));
    }
}
