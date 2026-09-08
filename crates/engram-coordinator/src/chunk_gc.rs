//! ADR 0016 Phase C: chunk-GC sweep orchestrator.
//!
//! One tick, under the single-writer sweep lease:
//!
//! 1. Claim the lease and read the shard cursor
//!    (`claim_chunk_gc_sweep`). A replica that loses the claim sits the
//!    tick out — two pods advancing one cursor would skip shards.
//! 2. **Mark pass**: collect the pin set once, then walk hash-prefix
//!    shards (`chunks/sha256/<2 hex>/`) from the cursor,
//!    `shard_concurrency` at a time, paging each shard and batching the
//!    candidate upserts. Stops at a shard boundary when `mark_budget`
//!    expires; the cursor persists so the next tick resumes there.
//! 3. **Promote pass**: delete candidates past `grace_period`, after
//!    re-verifying the live pin set, until the backlog clears or
//!    `promote_budget` expires.
//! 4. Persist the cursor and release the lease.
//!
//! The two passes are INDEPENDENT: mark walks storage, promote only
//! needs rows an earlier tick wrote, so a mark failure degrades the tick
//! but never voids it.
//!
//! **Why it is shaped this way** — three production failures, each of
//! which the previous shape could not have survived:
//!
//! - Marking the whole key space at once is not BOUNDED work; it grows
//!   with garbage. Sharding under a budget is what bounds it. Slicing
//!   stays correct because every slice is classified against the
//!   COMPLETE pin set — only the key space is partitioned.
//! - The whole-listing `list_prefix` blew its 300s deadline on every
//!   sweep from 2026-07-16; nothing was collected for 43 days while the
//!   bucket grew 7.9 TB -> 120.7 TB. Hence the paged walk.
//! - Chaining promote behind mark's `?` meant one failing list disabled
//!   deletion entirely, stranding 8.9M expired candidates that needed no
//!   listing to delete. Hence the split.
//! - One round trip per unpinned chunk metered the mark pass at ~335
//!   inserts/s, so the first sweep had not finished after 46 hours and
//!   the promote pass never ran. Hence the batched upsert.
//!
//! Each sweep is its own task ([`spawn_gc_loops`]): bundle and
//! snapshot-blob finish in ~2s and must never be hostage to the chunk
//! sweep, which is exactly what happened for two days in 2026-08.

use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::{ChunkHash, ChunkStore, GcError, PinSet};
use engram_core::traits::{BlobStorage, MetadataStore};

use crate::state::SharedState;

/// Blob-storage prefix the chunk sweep walks.
const CHUNK_PREFIX: &str = "chunks/sha256/";

/// Hash-prefix shards the chunk space splits into. Keys are
/// `chunks/sha256/<2 hex>/<62 hex>`, so the first byte gives 256
/// independent slices that can be walked separately and in parallel.
///
/// Slicing is what makes the sweep bounded. A slice is still classified
/// against the COMPLETE pin set, so "chunk X is unpinned" stays a sound
/// conclusion — only the key space is partitioned, never the pin set.
const SHARD_COUNT: usize = 256;

/// Sweep configuration. Defaults match the active-development
/// posture: GC enabled, hourly cadence, 24h candidate grace.
#[derive(Clone, Debug)]
pub struct ChunkGcConfig {
    /// Whether `gc_sweep_loop` runs at all. `false` skips the
    /// background loop entirely (admin endpoints still work).
    /// Defaults to `true`. Override via `ENGRAM_CHUNK_GC_ENABLED`
    /// (`0`/`false`/`off` → disable).
    pub enabled: bool,
    /// Interval between background sweeps. Defaults to 3600s (1h).
    /// Override via `ENGRAM_CHUNK_GC_INTERVAL_SECS`.
    pub interval: Duration,
    /// How long a candidate must live in the table before promote-
    /// pass deletes it from BlobStorage. Defaults to 86400s (24h).
    /// Override via `ENGRAM_CHUNK_GC_GRACE_SECS`.
    pub grace_period: Duration,
    /// Max barrier-restart attempts inside a single sweep, for the
    /// bundle + snapshot-blob sweeps. Those walk key spaces of a few
    /// hundred entries, so a restart is cheap and keeps their
    /// classification exact. The chunk sweep does NOT restart — see
    /// `run_mark_pass`. Defaults to 3.
    pub max_restart_attempts: u32,
    /// Keys fetched per `list_prefix_page` call in the mark pass.
    /// 1000 is the GCS per-page ceiling, so asking for more buys
    /// nothing. Defaults to 1000. Override via
    /// `ENGRAM_CHUNK_GC_LIST_PAGE_SIZE`.
    pub list_page_size: usize,
    /// Concurrent manifest fetches inside `PinSet::collect`.
    ///
    /// The chunk-store default is 16, whose own doc sizes it for
    /// "fleet-scale (100s of manifests)". Prod is an order of magnitude
    /// past that: the pin set spans ~5,600 manifest refs (2,234
    /// live-session disk, 1,407 recoverable-snapshot disk, 1,430 memory,
    /// 554 cold bases, plus the enabled images), each roughly 722 KiB, so
    /// a 16-way collect moves ~4 GB and runs tens of seconds.
    ///
    /// Collect DURATION is what makes the promote pass's generation
    /// bracket fail: `chunk_generation` ticks ~2.3x/min in prod, so a
    /// window measured in tens of seconds is likely to have a bump land
    /// inside it, and three such attempts in a row exhaust the budget and
    /// skip a drain. Shortening the window is the direct fix — it cuts the
    /// chance of a racing publish roughly proportionally, and speeds the
    /// mark pass too. Defaults to 64. Override via
    /// `ENGRAM_CHUNK_GC_PIN_SET_CONCURRENCY`.
    pub pin_set_concurrency: usize,
    /// Shards this tick covers, starting at the cursor.
    ///
    /// THE memory knob. The pin set is filtered to exactly these shards,
    /// so coordinator heap is `pin_set * shards_per_tick / 256` instead
    /// of the whole ~14M-hash set. Lower it if the sweep approaches its
    /// memory limit; raise it to finish a full cycle in fewer ticks.
    /// Defaults to 32 (12.5% of the pin set). Override via
    /// `ENGRAM_CHUNK_GC_SHARDS_PER_TICK`.
    pub shards_per_tick: usize,
    /// Hash-prefix shards walked CONCURRENTLY inside one mark pass.
    ///
    /// Chunk keys are `chunks/sha256/<2 hex>/<62 hex>`, so the space
    /// splits 256 ways on the first byte and the shards are independent.
    /// Listing is the mark pass's bottleneck — ~113M objects is ~113k
    /// list round-trips, over an hour walked serially — and the shards
    /// are the natural unit of parallelism. Defaults to 16. Override via
    /// `ENGRAM_CHUNK_GC_SHARD_CONCURRENCY`.
    pub shard_concurrency: usize,
    /// Candidate rows per multi-row upsert statement.
    ///
    /// The mark pass used to issue ONE round-trip per unpinned chunk.
    /// That was invisible while `list_prefix` failed first; once the
    /// paged walk worked, it metered the sweep at ~335 inserts/s and the
    /// first sweep had not finished after 46 hours (2026-08-31).
    /// Batching is what makes the write side a non-factor. Defaults to
    /// 1000. Override via `ENGRAM_CHUNK_GC_UPSERT_BATCH`.
    pub upsert_batch_size: usize,
    /// Wall-clock budget for one mark pass. When it expires the pass
    /// stops at a shard boundary and the cursor persists, so the next
    /// tick resumes where this one stopped. THE guarantee that a sweep
    /// makes bounded progress and always ends. Defaults to 300s.
    /// Override via `ENGRAM_CHUNK_GC_MARK_BUDGET_SECS`.
    pub mark_budget: Duration,
    /// Wall-clock budget for one promote pass.
    ///
    /// Replaces a row cap. A cap makes deletion the binding constraint
    /// and can leave a backlog stuck for weeks; a time budget makes
    /// promote THROUGHPUT-bound, which is the property that actually
    /// drains a backlog. Defaults to 900s. Override via
    /// `ENGRAM_CHUNK_GC_PROMOTE_BUDGET_SECS`.
    pub promote_budget: Duration,
    /// Promote-pass batch size — candidates fetched from PG per
    /// page. Defaults to 10_000.
    pub promote_batch_size: i64,
    /// Max concurrent BlobStorage deletes inside a promote batch.
    /// Sequential deletes cap the drain at ~20 chunks/s, which
    /// cannot keep up with a backlog (the 2026-08-28 audit found
    /// 8.9M expired candidates — 5 days of sequential deletes).
    /// Concurrency also SHRINKS the pin-set staleness window: the
    /// window is the batch's wall-clock, so draining a batch faster
    /// is strictly safer per chunk deleted. Defaults to 64.
    /// Override via `ENGRAM_CHUNK_GC_PROMOTE_CONCURRENCY`.
    pub promote_concurrency: usize,
}

impl Default for ChunkGcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: Duration::from_secs(3600),
            grace_period: Duration::from_secs(86400),
            max_restart_attempts: 3,
            pin_set_concurrency: 64,
            list_page_size: 1000,
            shards_per_tick: 32,
            shard_concurrency: 16,
            upsert_batch_size: 1000,
            mark_budget: Duration::from_secs(300),
            promote_budget: Duration::from_secs(900),
            promote_batch_size: 10_000,
            promote_concurrency: 64,
        }
    }
}

impl ChunkGcConfig {
    /// Read overrides from env vars. Unset / unparseable values
    /// fall back to defaults.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_ENABLED") {
            cfg.enabled = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "off" | "");
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_INTERVAL_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.interval = Duration::from_secs(secs);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_GRACE_SECS") {
            if let Ok(secs) = v.parse::<u64>() {
                cfg.grace_period = Duration::from_secs(secs);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_PIN_SET_CONCURRENCY") {
            if let Ok(n) = v.parse::<usize>() {
                cfg.pin_set_concurrency = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_LIST_PAGE_SIZE") {
            if let Ok(n) = v.parse::<usize>() {
                cfg.list_page_size = n.clamp(1, 1000);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_SHARDS_PER_TICK") {
            if let Ok(n) = v.parse::<usize>() {
                cfg.shards_per_tick = n.clamp(1, SHARD_COUNT);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_SHARD_CONCURRENCY") {
            if let Ok(n) = v.parse::<usize>() {
                cfg.shard_concurrency = n.clamp(1, SHARD_COUNT);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_UPSERT_BATCH") {
            if let Ok(n) = v.parse::<usize>() {
                cfg.upsert_batch_size = n.max(1);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_MARK_BUDGET_SECS") {
            if let Ok(n) = v.parse::<u64>() {
                cfg.mark_budget = Duration::from_secs(n);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_PROMOTE_BUDGET_SECS") {
            if let Ok(n) = v.parse::<u64>() {
                cfg.promote_budget = Duration::from_secs(n);
            }
        }
        if let Ok(v) = std::env::var("ENGRAM_CHUNK_GC_PROMOTE_CONCURRENCY") {
            if let Ok(n) = v.parse::<usize>() {
                cfg.promote_concurrency = n.max(1);
            }
        }
        cfg
    }
}

/// Whether [`run_one_sweep`] writes anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepMode {
    /// List + classify only. No candidate upserts, no deletes.
    /// Used by `POST /api/admin/chunk-gc/dry-run`.
    DryRun,
    /// Full pipeline: upsert unpinned chunks into
    /// `chunk_gc_candidates`, then run the promote pass.
    Full,
}

/// Outcome of a single sweep. Returned by the admin endpoints +
/// logged at the end of each background-loop tick.
#[derive(Clone, Debug, Default)]
pub struct SweepReport {
    /// How many chunks `list_prefix` returned under
    /// `chunks/sha256/`.
    pub listed_chunks: usize,
    /// Subset of `listed_chunks` whose parse-back to `ChunkHash`
    /// failed. Indicates stray non-chunk keys at the prefix
    /// (shouldn't happen; surfaced for diagnostic visibility).
    pub malformed_keys: usize,
    /// Size of the pin set at the time the sweep completed.
    pub pin_set_size: usize,
    /// Number of unpinned chunks observed. In `Full` mode each
    /// was upserted as a candidate; in `DryRun` this is what
    /// *would* be marked.
    pub candidates_marked: usize,
    /// Whether `chunk_generation` moved while the mark pass walked the
    /// chunk space — a flush / enable_image / record_snapshot raced the
    /// sweep. Benign: the mark pass only adds candidate rows, and the
    /// promote pass re-verifies the live pin set before deleting. Kept
    /// as a diagnostic for pin-churn pressure.
    pub generation_moved: bool,
    /// Promote-pass: candidates whose first_seen_at predated the
    /// grace cutoff and were deleted from BlobStorage + the
    /// candidate table. Always 0 in `DryRun`.
    pub promoted_deletes: usize,
    /// Promote-pass: expired candidates found RE-PINNED at delete time
    /// and skipped (blob kept, stale candidate row cleared). `first_seen_at`
    /// is sticky and classification never clears a re-pinned candidate, so
    /// without this re-check a chunk marked while transiently unpinned but
    /// since re-referenced (e.g. an image refresh re-pinning a shared
    /// base-memory chunk) would be wrongly deleted — 404ing the pinning
    /// manifest. `>0` means the durability guard fired. Mirrors the
    /// snapshot-blob-gc promote pass (ADR 0028 addendum).
    pub promote_repinned_skips: usize,
    /// Promote-pass: BlobStorage delete attempts that errored.
    /// The candidate row stays in the table so the next sweep
    /// retries; surfaced for visibility.
    pub promote_delete_errors: usize,
    /// Shard the NEXT tick should resume at. The caller persists it as
    /// the cursor; wrapping past 255 back to 0 is a completed cycle.
    pub next_shard: u32,
    /// Shards walked by this mark pass. `SHARD_COUNT` means the walk
    /// covered the whole key space in one tick.
    pub shards_scanned: usize,
    /// Whether this mark pass finished a full cycle of the key space
    /// (wrapped past every shard) rather than stopping on its budget.
    pub full_cycle_completed: bool,
    /// Set when the promote pass failed. Symmetric with `mark_error`:
    /// the pass is recorded as degraded and the sweep still returns its
    /// report, so the mark pass's cursor progress is never thrown away
    /// by a promote-side fault.
    pub promote_error: Option<String>,
    /// Set when the mark pass failed. The promote pass still ran —
    /// it deletes candidates recorded by EARLIER sweeps and needs no
    /// listing — so a mark failure degrades the sweep instead of
    /// voiding it. `Some` means `listed_chunks` / `candidates_marked`
    /// are not authoritative for this sweep.
    pub mark_error: Option<String>,
}

/// Convenience wrapper that pulls `meta` / `blob` / `chunk_store`
/// off the shared state. Background loop + admin handlers call this;
/// integration tests in commit 6 call [`run_one_sweep_inner`]
/// directly with their own service handles (no SharedState fixture
/// needed).
pub async fn run_one_sweep(
    state: &SharedState,
    cfg: &ChunkGcConfig,
    mode: SweepMode,
    start_shard: u32,
) -> SweepReport {
    run_one_sweep_inner(
        state.services.meta.clone(),
        state.services.blob.clone(),
        &state.services.chunk_store,
        cfg,
        mode,
        &state.services.clock,
        start_shard,
    )
    .await
}

/// Run one sweep cycle against a given set of services. The
/// SharedState-taking [`run_one_sweep`] above wraps this for the
/// production callers; tests bypass the wrapper to build the
/// services they need from a real PG pool + LocalBlobStorage.
///
/// `clock` is the sweep's time source (ADR 0098 D1: time is an
/// injected input). The promote pass reads it FRESH, after the mark
/// pass: candidates are stamped `first_seen_at DEFAULT now()` by PG
/// during this same call, so a cutoff captured at sweep start would
/// never see a same-sweep candidate as expired under zero grace.
/// (The host-vs-PG cross-clock comparison predates this seam; D3's
/// bind-param `now()` unifies it.)
///
/// `start_shard` is the hash-prefix shard the mark pass resumes at. The
/// CALLER owns the cursor and the single-writer lease — this function
/// stays pure so admin dry-runs and tests can drive it without either.
pub async fn run_one_sweep_inner(
    meta: Arc<dyn MetadataStore>,
    blob: Arc<dyn BlobStorage>,
    chunk_store: &ChunkStore,
    cfg: &ChunkGcConfig,
    mode: SweepMode,
    clock: &Arc<dyn engram_core::traits::Clock>,
    start_shard: u32,
) -> SweepReport {
    let mut report = SweepReport {
        next_shard: start_shard,
        ..Default::default()
    };

    // The mark pass and the promote pass are INDEPENDENT. Mark needs a
    // full `list_prefix` of the chunk space; promote only needs rows an
    // earlier sweep already wrote. Chaining promote behind a `?` on the
    // mark pass meant one failing list disabled deletion entirely: the
    // 2026-08-28 audit found `list_prefix` had exceeded its 300s deadline
    // on EVERY sweep since 2026-07-16, stranding 8.9M expired candidates
    // (~9.5 TB) that needed no listing to delete, while the bucket grew
    // 7.9 TB -> 120.7 TB. Degrade the sweep; never void it.
    let mark_ctx = MarkCtx {
        meta: meta.as_ref(),
        blob: blob.as_ref(),
        chunk_store,
        cfg,
        mode,
        clock,
    };
    let (shard_list, shard_mask) = tick_shards(start_shard, cfg.shards_per_tick);
    if let Err(e) = run_mark_pass(
        &mark_ctx,
        start_shard,
        &shard_list,
        &shard_mask,
        &mut report,
    )
    .await
    {
        tracing::warn!(
            error = %e,
            "chunk-gc mark pass failed; promote pass still runs on existing candidates"
        );
        report.mark_error = Some(e.to_string());
    }

    // -------- promote pass (Full only) --------
    //
    // Caught, not propagated — symmetric with the mark pass above. A `?`
    // here discarded the whole report, so `chunk_gc_run_once` could not
    // read the advanced cursor and wrote back the shard it started at,
    // throwing away the mark pass's progress. Under a recurring promote
    // fault with a budget-limited walk, the cursor would never advance
    // and the unreached shards would never be scanned — the exact
    // outcome the cursor exists to prevent.
    if mode == SweepMode::Full {
        match promote_expired(
            meta.as_ref(),
            blob.as_ref(),
            chunk_store,
            cfg,
            clock,
            &shard_list,
            &shard_mask,
        )
        .await
        {
            Ok((deletes, repinned, errors)) => {
                report.promoted_deletes = deletes;
                report.promote_repinned_skips = repinned;
                report.promote_delete_errors = errors;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "chunk-gc promote pass failed; the mark pass's cursor progress still stands"
                );
                report.promote_error = Some(e.to_string());
            }
        }
    }

    report
}

/// Mark pass: classify every stored chunk against the live pin set and
/// record the unpinned ones as candidates. Writes only to
/// `chunk_gc_candidates` (never to BlobStorage), so a failure here is
/// recoverable — the next sweep re-classifies from scratch.
/// The immutable inputs a mark pass and its shard walks share. Bundled
/// so both take a handful of arguments instead of a long positional
/// list, and so `walk_shard` can be handed one borrow.
struct MarkCtx<'a> {
    meta: &'a dyn MetadataStore,
    blob: &'a dyn BlobStorage,
    chunk_store: &'a ChunkStore,
    cfg: &'a ChunkGcConfig,
    mode: SweepMode,
    clock: &'a Arc<dyn engram_core::traits::Clock>,
}

async fn run_mark_pass(
    ctx: &MarkCtx<'_>,
    start_shard: u32,
    shard_list: &[usize],
    shard_mask: &[bool; SHARD_COUNT],
    report: &mut SweepReport,
) -> Result<(), GcError> {
    let MarkCtx {
        meta,
        chunk_store,
        cfg,
        clock,
        ..
    } = *ctx;
    // `now_mono`, not `now_utc`: this is an elapsed-time budget, and a
    // monotonic Duration is what a fake clock can drive (ADR 0098 D1 —
    // an opaque `Instant` cannot be minted by a simulated clock).
    let deadline = clock.now_mono() + cfg.mark_budget;

    let gen_before = meta.chunk_generation().await?;
    // Filtered to this tick's shards only — see `collect_pin_set`. The
    // set is dropped before the promote pass collects its own, so the two
    // are never resident at once (that 2x spike is what OOMKilled the
    // coordinator on 2026-09-03).
    let pin_set = collect_pin_set(meta, chunk_store, cfg, shard_mask).await?;
    report.pin_set_size = pin_set.len();

    // Walk the key space in hash-prefix shards, resuming at the cursor.
    //
    // Slicing is what makes the sweep BOUNDED. Marking the whole space
    // in one tick is not a bounded amount of work — it grows with
    // accumulated garbage, and in 2026-08 it outgrew a tick entirely:
    // the mark pass ran 46 hours without finishing, so the promote pass
    // never ran and nothing was ever deleted.
    //
    // Slicing stays CORRECT because each shard is classified against the
    // COMPLETE pin set collected above. Only the key space is
    // partitioned, never the pin set, so "chunk X is unpinned" remains a
    // sound conclusion from one shard.
    let mut shard = (start_shard as usize) % SHARD_COUNT;
    let mut scanned = 0usize;
    let group_len = shard_list.len();
    while scanned < group_len {
        if clock.now_mono() >= deadline {
            tracing::info!(
                scanned,
                next_shard = shard,
                budget_secs = cfg.mark_budget.as_secs(),
                "chunk-gc mark: budget spent; cursor persists and the next tick resumes"
            );
            break;
        }
        let group: Vec<usize> = (0..cfg.shard_concurrency.min(group_len - scanned))
            .map(|i| (shard + i) % SHARD_COUNT)
            .collect();

        // Shards are independent, so they are the natural unit of
        // parallelism. Listing dominates the mark pass — ~113M objects
        // is ~113k list round-trips, over an hour walked serially — and
        // this is what turns a full cycle into minutes.
        let scans =
            futures::future::join_all(group.iter().map(|&sh| walk_shard(ctx, &pin_set, sh))).await;

        // Fold in whatever completed BEFORE surfacing an error, so a
        // failed shard still leaves the cursor and counters reflecting
        // real progress (`report` is `&mut`, so the caller sees it even
        // on the error return).
        let mut failure = None;
        for scan in scans {
            match scan {
                Ok(s) => {
                    report.listed_chunks += s.listed;
                    report.malformed_keys += s.malformed;
                    report.candidates_marked += s.marked;
                }
                Err(e) => failure = Some(e),
            }
        }
        scanned += group.len();
        shard = (shard + group.len()) % SHARD_COUNT;
        report.shards_scanned = scanned;
        report.next_shard = shard as u32;
        if let Some(e) = failure {
            return Err(e);
        }
    }
    // A tick covers its group, not the whole space; a full cycle is when
    // the cursor wraps past the last shard.
    report.full_cycle_completed = (shard as u32) < start_shard || group_len >= SHARD_COUNT;

    // The barrier is a DIAGNOSTIC, not a restart trigger. The mark pass
    // only ever ADDS candidate rows, and the promote pass re-verifies
    // the live pin set before it deletes anything — that is the actual
    // durability guard. A chunk pinned mid-walk is marked, then rescued
    // at promote time, exactly as a chunk pinned between sweeps was.
    let gen_after = meta.chunk_generation().await?;
    if gen_after != gen_before {
        report.generation_moved = true;
        tracing::debug!(
            gen_before,
            gen_after,
            "chunk-gc mark pass raced a pin change; promote-time re-verification covers it"
        );
    }

    Ok(())
}

/// What one shard walk observed. Kept separate from `SweepReport` so
/// shards can run concurrently without sharing a mutable borrow.
#[derive(Debug, Default)]
struct ShardScan {
    listed: usize,
    malformed: usize,
    marked: usize,
}

/// Walk one hash-prefix shard, batching candidate upserts.
///
/// The batching is the other half of the 2026-08 failure: the mark pass
/// issued ONE DB round trip per unpinned chunk, which metered the sweep
/// at ~335 inserts/s. A page's worth per statement makes the write side
/// a non-factor.
async fn walk_shard(
    ctx: &MarkCtx<'_>,
    pin_set: &PinSet,
    shard: usize,
) -> Result<ShardScan, GcError> {
    let MarkCtx {
        meta,
        blob,
        cfg,
        mode,
        ..
    } = *ctx;
    let prefix = format!("{CHUNK_PREFIX}{shard:02x}/");
    let mut out = ShardScan::default();
    let mut cursor: Option<String> = None;
    let mut batch: Vec<[u8; 32]> = Vec::with_capacity(cfg.upsert_batch_size);

    loop {
        let page = blob
            .list_prefix_page(&prefix, cursor.as_deref(), cfg.list_page_size)
            .await
            .map_err(|e| GcError::ChunkStore(engram_chunk_store::ChunkStoreError::Blob(e)))?;

        for key in &page.keys {
            let Some(hash) = ChunkHash::from_storage_key(key) else {
                out.malformed += 1;
                continue;
            };
            if pin_set.contains(&hash) {
                continue;
            }
            out.marked += 1;
            if mode == SweepMode::Full {
                batch.push(*hash.as_bytes());
                if batch.len() >= cfg.upsert_batch_size {
                    meta.upsert_chunk_gc_candidates(&batch).await?;
                    batch.clear();
                }
            }
        }
        out.listed += page.keys.len();

        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    if mode == SweepMode::Full && !batch.is_empty() {
        meta.upsert_chunk_gc_candidates(&batch).await?;
    }
    Ok(out)
}

/// Promote-pass: delete candidates older than `grace_period` from
/// BlobStorage + the candidate table — AFTER re-verifying the live pin
/// set, so a candidate that was re-pinned since it was marked is skipped
/// and cleared, never deleted.
///
/// Drains in `cfg.promote_batch_size` pages until the backlog is empty or
/// `cfg.promote_budget` expires. Returns
/// `(deleted, repinned_skipped, errors)`.
async fn promote_expired(
    meta: &dyn MetadataStore,
    blob: &dyn BlobStorage,
    chunk_store: &ChunkStore,
    cfg: &ChunkGcConfig,
    clock: &Arc<dyn engram_core::traits::Clock>,
    shard_list: &[usize],
    shard_mask: &[bool; SHARD_COUNT],
) -> Result<(usize, usize, usize), GcError> {
    let now = clock.now_utc();
    // Monotonic, like the mark pass: a fake clock can drive an elapsed
    // Duration but cannot mint an opaque Instant (ADR 0098 D1).
    let deadline = clock.now_mono() + cfg.promote_budget;
    let cutoff = now
        - chrono::Duration::from_std(cfg.grace_period)
            .unwrap_or_else(|_| chrono::Duration::seconds(86_400));

    let mut deleted = 0usize;
    let mut repinned = 0usize;
    let mut errors = 0usize;
    let mut processed = 0usize;

    // Collected AFTER the mark pass dropped its own, so only ONE filtered
    // pin set is ever resident — the 2x spike is what OOMKilled the
    // coordinator on 2026-09-03.
    //
    // Reused across batches only while `chunk_generation` is unchanged.
    // The generation moves in the same transaction as every flush /
    // enable_image / record_snapshot, so an unchanged generation is proof
    // that no manifest — and therefore no pin — has moved since the
    // collect. When it moves, re-collect BEFORE the next batch.
    //
    // This re-check is load-bearing, not an optimisation. The drain runs
    // up to `promote_budget` (900s by default) across every shard in the
    // tick, and the generation ticks ~2.3x/min in prod. Holding one
    // snapshot for that whole window would delete a chunk that got
    // re-pinned mid-drain — the content-addressed "image refresh
    // re-pins a shared base-memory chunk" case — and 404 the manifest
    // that now references it.
    let mut pin_set = collect_pin_set(meta, chunk_store, cfg, shard_mask).await?;
    let mut pin_gen = meta.chunk_generation().await?;
    let mut pin_refreshes = 0usize;

    'shards: for &shard in shard_list {
        let (lo, hi) = engram_chunk_store::shard_hash_bounds(shard as u8);
        loop {
            if clock.now_mono() >= deadline {
                tracing::info!(
                    processed,
                    budget_secs = cfg.promote_budget.as_secs(),
                    "chunk-gc promote: budget spent; backlog continues next tick"
                );
                break 'shards;
            }
            let expired = meta
                .list_expired_gc_candidates_in_range(cutoff, &lo, &hi, cfg.promote_batch_size)
                .await?;
            if expired.is_empty() {
                break;
            }
            let batch_len = expired.len();

            let gen_now = meta.chunk_generation().await?;
            if gen_now != pin_gen {
                // Free the stale set BEFORE collecting, so the two are
                // never resident together — that is what keeps the
                // memory property while restoring the freshness check.
                // `take` for the drop side effect, not the value.
                drop(std::mem::take(&mut pin_set));
                pin_set = collect_pin_set(meta, chunk_store, cfg, shard_mask).await?;
                pin_gen = meta.chunk_generation().await?;
                pin_refreshes += 1;
            }

            let (batch_deleted, batch_repinned, batch_errors, resolved) =
                promote_batch(blob, &pin_set, expired, cfg.promote_concurrency).await;
            meta.delete_gc_candidates(&resolved).await?;

            deleted += batch_deleted;
            repinned += batch_repinned;
            errors += batch_errors;
            processed += batch_len;

            if resolved.is_empty() {
                tracing::warn!(
                    batch_errors,
                    "chunk-gc promote: whole batch failed to delete; stopping drain this sweep"
                );
                break 'shards;
            }
            if (batch_len as i64) < cfg.promote_batch_size {
                break;
            }
        }
    }

    if pin_refreshes > 0 {
        tracing::debug!(
            pin_refreshes,
            "chunk-gc promote: re-collected the shard-filtered pin set on generation bumps"
        );
    }
    if repinned > 0 {
        tracing::info!(
            repinned,
            "chunk-gc promote: skipped + cleared re-pinned candidates \
             (live chunks the stale rows would have wrongly deleted)"
        );
    }

    Ok((deleted, repinned, errors))
}

/// The shards this tick covers, and the mask that filters the pin set to
/// them. Wraps past 255 back to 0.
fn tick_shards(start_shard: u32, count: usize) -> (Vec<usize>, [bool; SHARD_COUNT]) {
    let count = count.clamp(1, SHARD_COUNT);
    let mut mask = [false; SHARD_COUNT];
    let mut list = Vec::with_capacity(count);
    for i in 0..count {
        let sh = ((start_shard as usize) + i) % SHARD_COUNT;
        mask[sh] = true;
        list.push(sh);
    }
    (list, mask)
}

/// Collect a pin set for the promote pass.
///
/// Delegates to [`PinSet::collect_converging`], which converges on the
/// REF SET instead of on `chunk_generation`. The old bracket retried
/// whenever the generation moved, but the generation ticks on every
/// flush / enable_image / record_snapshot — most of which cannot change
/// the pin set — so it fired constantly, and each false positive cost a
/// full ~4 GB manifest re-fetch. In prod 43% of sweeps exhausted the
/// budget and skipped their drain outright (2026-09-01).
///
/// A ref is `(manifest_id, version)`, so re-reading the ref set detects
/// exactly the publishes that matter, for seven cheap SQL queries. The
/// result is a superset of the live pin set, which is the safe direction
/// for both passes.
///
/// There is deliberately NO skip path any more. A non-converged collect
/// has still fetched every ref it saw, so the set remains valid as of
/// the last read — declining to drain would forfeit reclamation for no
/// safety gain.
async fn collect_pin_set(
    meta: &dyn MetadataStore,
    chunk_store: &ChunkStore,
    cfg: &ChunkGcConfig,
    shards: &[bool; SHARD_COUNT],
) -> Result<PinSet, GcError> {
    let collected = PinSet::collect_converging_for_shards(
        meta,
        chunk_store,
        cfg.pin_set_concurrency,
        cfg.max_restart_attempts as usize,
        shards,
    )
    .await?;
    if !collected.converged {
        ::metrics::counter!(
            crate::metrics::GC_PIN_SET_UNCONVERGED_TOTAL,
            "sweep" => "chunk",
        )
        .increment(1);
        tracing::info!(
            rounds = collected.rounds,
            "chunk-gc: ref set still moving at the round cap; the pin set is a valid superset \
             as of the last read, so the drain proceeds"
        );
    }
    Ok(collected.pin_set)
}

/// Delete one batch of expired candidates, up to `concurrency` deletes in
/// flight. Returns `(deleted, repinned_skipped, errors, resolved_hashes)`
/// where `resolved` is every candidate whose row can now be cleared —
/// blobs actually deleted plus re-pinned rescues.
async fn promote_batch(
    blob: &dyn BlobStorage,
    pin_set: &PinSet,
    expired: Vec<[u8; 32]>,
    concurrency: usize,
) -> (usize, usize, usize, Vec<[u8; 32]>) {
    use futures::stream::{FuturesUnordered, StreamExt};

    let mut resolved: Vec<[u8; 32]> = Vec::with_capacity(expired.len());
    let mut deleted = 0usize;
    let mut repinned = 0usize;
    let mut errors = 0usize;

    // Rescues need no I/O — settle them first so only real deletes occupy
    // a concurrency slot.
    let mut to_delete: Vec<[u8; 32]> = Vec::with_capacity(expired.len());
    for hash_bytes in expired {
        if pin_set.contains(&ChunkHash::from_bytes(hash_bytes)) {
            // Re-pinned since it was marked — rescue: drop the stale
            // candidate row, keep the blob.
            repinned += 1;
            resolved.push(hash_bytes);
        } else {
            to_delete.push(hash_bytes);
        }
    }

    let mut pending = FuturesUnordered::new();
    let mut queue = to_delete.into_iter();
    let settle = |r: (Result<(), engram_core::BlobError>, [u8; 32]),
                  resolved: &mut Vec<[u8; 32]>,
                  deleted: &mut usize,
                  errors: &mut usize| {
        let (outcome, hash_bytes) = r;
        match outcome {
            Ok(()) => {
                resolved.push(hash_bytes);
                *deleted += 1;
            }
            Err(e) => {
                tracing::warn!(
                    chunk_hash = %ChunkHash::from_bytes(hash_bytes).to_hex(),
                    error = %e,
                    "chunk-gc promote: BlobStorage delete failed; candidate row stays for retry"
                );
                *errors += 1;
            }
        }
    };

    for _ in 0..concurrency.max(1) {
        let Some(hash_bytes) = queue.next() else {
            break;
        };
        pending.push(delete_one(blob, hash_bytes));
    }
    while let Some(done) = pending.next().await {
        settle(done, &mut resolved, &mut deleted, &mut errors);
        if let Some(hash_bytes) = queue.next() {
            pending.push(delete_one(blob, hash_bytes));
        }
    }

    (deleted, repinned, errors, resolved)
}

/// One promote delete, carrying its hash through so the caller can settle
/// the result without tracking join order.
async fn delete_one(
    blob: &dyn BlobStorage,
    hash_bytes: [u8; 32],
) -> (Result<(), engram_core::BlobError>, [u8; 32]) {
    let key = ChunkHash::from_bytes(hash_bytes).storage_key();
    (blob.delete(&key).await, hash_bytes)
}

/// What one sweep did, in the shape the metrics care about. The three
/// sweeps have different report types but the same observable outcome,
/// so they converge here rather than each growing its own emission
/// block.
pub(crate) struct SweepMetrics {
    /// `chunk` / `bundle` / `snapshot_blob`.
    pub sweep: &'static str,
    /// `success` / `mark_failed` / `failed`.
    pub outcome: &'static str,
    pub listed: usize,
    pub pinned: usize,
    pub promoted: usize,
    pub repinned_skips: usize,
    pub promote_errors: usize,
    pub elapsed: Duration,
}

/// The `outcome` label for a chunk sweep.
///
/// Four values, because the two passes fail independently and a sweep
/// where one worked is genuinely different from one where neither did.
/// `mark_failed` and `promote_failed` are each REAL work plus a real
/// gap: collapsing either into `success` would hide exactly the class of
/// outage this whole subsystem keeps producing — a pass that has
/// silently stopped doing anything while the sweep still reports fine.
pub(crate) fn chunk_sweep_outcome(report: &SweepReport) -> &'static str {
    match (report.mark_error.is_some(), report.promote_error.is_some()) {
        (false, false) => "success",
        (true, false) => "mark_failed",
        (false, true) => "promote_failed",
        (true, true) => "failed",
    }
}

/// Emit one sweep's metrics.
///
/// Before this existed the GC had NO metric surface at all, which is why
/// the chunk sweep could fail on every tick for 43 days unnoticed: a
/// sweep that never succeeds produced exactly the same dashboard as one
/// that works. `engram_gc_sweep_total{sweep,outcome}` is the liveness
/// signal to alert on.
pub(crate) fn record_sweep_metrics(m: &SweepMetrics) {
    ::metrics::counter!(
        crate::metrics::GC_SWEEP_TOTAL,
        "sweep" => m.sweep,
        "outcome" => m.outcome,
    )
    .increment(1);
    ::metrics::histogram!(crate::metrics::GC_SWEEP_SECONDS, "sweep" => m.sweep)
        .record(m.elapsed.as_secs_f64());
    ::metrics::counter!(crate::metrics::GC_PROMOTED_TOTAL, "sweep" => m.sweep)
        .increment(m.promoted as u64);
    ::metrics::counter!(crate::metrics::GC_PROMOTE_ERRORS_TOTAL, "sweep" => m.sweep)
        .increment(m.promote_errors as u64);
    ::metrics::counter!(crate::metrics::GC_REPINNED_SKIPS_TOTAL, "sweep" => m.sweep)
        .increment(m.repinned_skips as u64);
    // Only meaningful when the mark pass completed; a failed mark leaves
    // both at whatever it walked before erroring.
    if m.outcome == "success" {
        ::metrics::gauge!(crate::metrics::GC_LISTED_KEYS, "sweep" => m.sweep).set(m.listed as f64);
        ::metrics::gauge!(crate::metrics::GC_PINNED_KEYS, "sweep" => m.sweep).set(m.pinned as f64);
    }
}

/// Spawn the three GC drivers as INDEPENDENT tasks.
///
/// They used to share one loop body, run back to back on one tick. That
/// coupling took the fleet down twice over: when the chunk sweep hung on
/// its 46-hour mark pass (2026-08-29), the bundle and snapshot-blob
/// sweeps — both healthy, both finishing in ~2s — ran ZERO times for two
/// days because they sat behind it in the same body. A cheap sweep must
/// never be hostage to an expensive one.
///
/// Each driver is a thin timer wrapper over its own `run_once` step
/// (ADR 0098 D2), so tests and the simulator drive the step directly.
pub fn spawn_gc_loops(state: SharedState, cfg: ChunkGcConfig) -> Vec<tokio::task::JoinHandle<()>> {
    if !cfg.enabled {
        tracing::info!("chunk-gc disabled by config; no sweep loop will run");
        return Vec::new();
    }
    vec![
        tokio::spawn(chunk_gc_loop(state.clone(), cfg.clone())),
        tokio::spawn(bundle_gc_loop(state.clone(), cfg.clone())),
        tokio::spawn(snapshot_blob_gc_loop(state, cfg)),
    ]
}

/// Thin timer wrapper. Errors log and the loop continues — a transient PG
/// or BlobStorage hiccup must not take GC down permanently.
async fn chunk_gc_loop(state: SharedState, cfg: ChunkGcConfig) {
    // Same claimant convention as the dead-host detector: the pod's
    // HOSTNAME, minted once at spawn.
    let claimant = std::env::var("HOSTNAME").unwrap_or_else(|_| "coord".into());
    tracing::info!(
        interval_secs = cfg.interval.as_secs(),
        grace_secs = cfg.grace_period.as_secs(),
        mark_budget_secs = cfg.mark_budget.as_secs(),
        promote_budget_secs = cfg.promote_budget.as_secs(),
        shard_concurrency = cfg.shard_concurrency,
        "chunk-gc sweep loop starting"
    );
    let mut ticker = tokio::time::interval(cfg.interval);
    // Skip the immediate tick — let coord finish boot before the first
    // sweep fires.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        chunk_gc_run_once(&state, &cfg, &claimant).await;
    }
}

/// One chunk-gc tick, under the single-writer lease.
///
/// The lease is what makes the shard cursor safe. Both coordinator
/// replicas run this loop; two pods advancing one cursor would skip
/// shards, and those shards would then silently never be scanned. A
/// replica that loses the claim simply sits the tick out.
///
/// `stale_after` is the interval plus both budgets: long enough that a
/// healthy holder is never taken over mid-sweep, short enough that a pod
/// which dies mid-sweep does not wedge GC for long.
pub async fn chunk_gc_run_once(state: &SharedState, cfg: &ChunkGcConfig, claimant: &str) {
    let stale_after = cfg.interval + cfg.mark_budget + cfg.promote_budget;
    let start_shard = match state
        .services
        .meta
        .claim_chunk_gc_sweep(claimant, stale_after)
        .await
    {
        Ok(Some(shard)) => shard,
        Ok(None) => {
            tracing::debug!("chunk-gc: another replica holds the sweep lease; skipping this tick");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "chunk-gc: could not claim the sweep lease; skipping tick");
            return;
        }
    };

    let started = state.services.clock.now_utc();
    let report = run_one_sweep(state, cfg, SweepMode::Full, start_shard).await;
    let elapsed = (state.services.clock.now_utc() - started)
        .to_std()
        .unwrap_or_default();

    // Always persist the cursor. A sweep ALWAYS yields a report — both
    // passes record their failure into it rather than discarding it — so
    // whatever shards the mark pass did reach are never re-walked, no
    // matter which pass degraded.
    if let Err(e) = state
        .services
        .meta
        .release_chunk_gc_sweep(claimant, report.next_shard)
        .await
    {
        tracing::warn!(error = %e, "chunk-gc: could not release the sweep lease");
    }

    record_sweep_metrics(&SweepMetrics {
        sweep: "chunk",
        outcome: chunk_sweep_outcome(&report),
        listed: report.listed_chunks,
        pinned: report.pin_set_size,
        promoted: report.promoted_deletes,
        repinned_skips: report.promote_repinned_skips,
        promote_errors: report.promote_delete_errors,
        elapsed,
    });
    record_backlog_gauge(state).await;
    tracing::info!(
        listed = report.listed_chunks,
        malformed = report.malformed_keys,
        pinned = report.pin_set_size,
        candidates = report.candidates_marked,
        shards_scanned = report.shards_scanned,
        next_shard = report.next_shard,
        full_cycle = report.full_cycle_completed,
        promoted = report.promoted_deletes,
        repinned_skips = report.promote_repinned_skips,
        promote_errors = report.promote_delete_errors,
        generation_moved = report.generation_moved,
        mark_error = report.mark_error.as_deref().unwrap_or(""),
        promote_error = report.promote_error.as_deref().unwrap_or(""),
        "chunk-gc sweep done"
    );
}

/// The "is GC keeping up" gauge. It reached 8.9M before anyone looked,
/// and 59M before anyone looked again. An estimate past the cap: a gauge
/// at that scale does not need the last digit, and an exact count of
/// 22.7M rows was a 30s scan every sweep.
async fn record_backlog_gauge(state: &SharedState) {
    match state
        .services
        .meta
        .count_gc_candidates(engram_core::traits::GC_CANDIDATE_EXACT_CAP)
        .await
    {
        Ok(backlog) => {
            ::metrics::gauge!(crate::metrics::GC_CANDIDATE_BACKLOG, "sweep" => "chunk")
                .set(backlog.count as f64);
        }
        Err(e) => {
            tracing::warn!(error = %e, "chunk-gc: could not read the candidate backlog");
        }
    }
}

/// ADR 0035 §5: bundle generations. A few hundred keys, ~2s a sweep — its
/// own task so the chunk sweep can never starve it.
async fn bundle_gc_loop(state: SharedState, cfg: ChunkGcConfig) {
    let mut ticker = tokio::time::interval(cfg.interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let started = state.services.clock.now_utc();
        let result = crate::bundle_gc::run_one_bundle_sweep(
            state.services.meta.clone(),
            state.services.blob.clone(),
            &cfg,
            SweepMode::Full,
            &state.services.clock,
        )
        .await;
        let elapsed = (state.services.clock.now_utc() - started)
            .to_std()
            .unwrap_or_default();
        let r = result.as_ref().ok();
        record_sweep_metrics(&SweepMetrics {
            sweep: "bundle",
            outcome: if result.is_ok() { "success" } else { "failed" },
            listed: r.map(|r| r.listed).unwrap_or(0),
            pinned: r.map(|r| r.pin_set_size).unwrap_or(0),
            promoted: r.map(|r| r.promoted_deletes).unwrap_or(0),
            repinned_skips: 0,
            promote_errors: r.map(|r| r.promote_delete_errors).unwrap_or(0),
            elapsed,
        });
        match result {
            Ok(report) => {
                tracing::info!(
                    listed = report.listed,
                    pinned = report.pin_set_size,
                    candidates = report.candidates_marked,
                    promoted = report.promoted_deletes,
                    promote_errors = report.promote_delete_errors,
                    restart_count = report.restart_count,
                    "bundle-gc sweep done"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "bundle-gc sweep failed; will retry on next interval");
            }
        }
    }
}

/// ADR 0028 addendum: portable snapshot blobs (`snapshots/<id>/`), pinned
/// by a live `snapshots` row. Own task, same reason as bundle-gc.
async fn snapshot_blob_gc_loop(state: SharedState, cfg: ChunkGcConfig) {
    let mut ticker = tokio::time::interval(cfg.interval);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let started = state.services.clock.now_utc();
        let result = crate::snapshot_blob_gc::run_one_snapshot_blob_sweep(
            state.services.meta.clone(),
            state.services.blob.clone(),
            &cfg,
            SweepMode::Full,
            &state.services.clock,
        )
        .await;
        let elapsed = (state.services.clock.now_utc() - started)
            .to_std()
            .unwrap_or_default();
        let r = result.as_ref().ok();
        record_sweep_metrics(&SweepMetrics {
            sweep: "snapshot_blob",
            outcome: if result.is_ok() { "success" } else { "failed" },
            listed: r.map(|r| r.listed).unwrap_or(0),
            pinned: r.map(|r| r.pin_set_size).unwrap_or(0),
            promoted: r.map(|r| r.promoted_deletes).unwrap_or(0),
            repinned_skips: r.map(|r| r.promote_repinned_skips).unwrap_or(0),
            promote_errors: r.map(|r| r.promote_delete_errors).unwrap_or(0),
            elapsed,
        });
        match result {
            Ok(report) => {
                tracing::info!(
                    listed = report.listed,
                    malformed = report.malformed,
                    pinned = report.pin_set_size,
                    candidates = report.candidates_marked,
                    promoted = report.promoted_deletes,
                    repinned_skips = report.promote_repinned_skips,
                    promote_errors = report.promote_delete_errors,
                    restart_count = report.restart_count,
                    "snapshot-blob-gc sweep done"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "snapshot-blob-gc sweep failed; will retry on next interval"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_has_gc_on() {
        let cfg = ChunkGcConfig::default();
        assert!(cfg.enabled, "GC defaults ON in active-development posture");
        assert_eq!(cfg.interval, Duration::from_secs(3600));
        assert_eq!(cfg.grace_period, Duration::from_secs(86_400));
        assert_eq!(cfg.max_restart_attempts, 3);
    }

    #[test]
    fn from_env_off_keywords_disable() {
        for v in ["0", "false", "off", ""] {
            // SAFETY: serial test on a process-global env var.
            std::env::set_var("ENGRAM_CHUNK_GC_ENABLED", v);
            let cfg = ChunkGcConfig::from_env();
            assert!(!cfg.enabled, "value {v:?} should disable GC");
        }
        std::env::remove_var("ENGRAM_CHUNK_GC_ENABLED");
    }

    #[test]
    fn from_env_on_or_unset_enables() {
        std::env::remove_var("ENGRAM_CHUNK_GC_ENABLED");
        let cfg = ChunkGcConfig::from_env();
        assert!(cfg.enabled, "unset env var should keep default ON");

        std::env::set_var("ENGRAM_CHUNK_GC_ENABLED", "1");
        let cfg = ChunkGcConfig::from_env();
        assert!(cfg.enabled);
        std::env::remove_var("ENGRAM_CHUNK_GC_ENABLED");
    }

    #[test]
    fn outcome_distinguishes_which_pass_failed() {
        let healthy = SweepReport {
            listed_chunks: 10,
            candidates_marked: 2,
            ..Default::default()
        };
        assert_eq!(chunk_sweep_outcome(&healthy), "success");

        // The shape that ran unnoticed in prod for 43 days: the mark
        // pass is dead, but the promote pass still deletes.
        let mark_dead = SweepReport {
            promoted_deletes: 9000,
            mark_error: Some("attempt deadline 300s exceeded".into()),
            ..Default::default()
        };
        assert_eq!(chunk_sweep_outcome(&mark_dead), "mark_failed");

        // The mirror: marking advances the cursor, promote is broken.
        // Must not report success, or a stalled reclaim hides.
        let promote_dead = SweepReport {
            candidates_marked: 5000,
            next_shard: 32,
            promote_error: Some("list_expired_gc_candidates: pool timed out".into()),
            ..Default::default()
        };
        assert_eq!(chunk_sweep_outcome(&promote_dead), "promote_failed");

        let both_dead = SweepReport {
            mark_error: Some("boom".into()),
            promote_error: Some("boom".into()),
            ..Default::default()
        };
        assert_eq!(chunk_sweep_outcome(&both_dead), "failed");
    }

    #[test]
    fn list_page_size_is_clamped_to_the_backend_ceiling() {
        // GCS and S3 both cap a page at 1000; a larger request would
        // silently return 1000 anyway, and 0 would never advance.
        for (set, want) in [("0", 1), ("50", 50), ("100000", 1000)] {
            std::env::set_var("ENGRAM_CHUNK_GC_LIST_PAGE_SIZE", set);
            assert_eq!(ChunkGcConfig::from_env().list_page_size, want);
        }
        std::env::remove_var("ENGRAM_CHUNK_GC_LIST_PAGE_SIZE");
    }

    #[test]
    fn from_env_interval_override() {
        std::env::set_var("ENGRAM_CHUNK_GC_INTERVAL_SECS", "120");
        let cfg = ChunkGcConfig::from_env();
        assert_eq!(cfg.interval, Duration::from_secs(120));
        std::env::remove_var("ENGRAM_CHUNK_GC_INTERVAL_SECS");
    }
}
