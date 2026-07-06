//! ADR 0048 C6: the session queue scanner.
//!
//! When `reserve_placement` finds no host with capacity, the create
//! handler enqueues the session (`status='queued'`) instead of 503-ing,
//! and a resume that hits the same wall parks the idle session back in
//! the queue. This background task is the continuation: each sweep it
//! walks the queue — per fit class, oldest-first — and either places +
//! boots a session (capacity arrived / the fleet scaled up) or fails it
//! on a generous timeout.
//!
//! ## Why a scanner (not an inline retry)
//!
//! Per `[async_via_state_machine]`: the create handler must not block for
//! minutes while a node provisions (the idle-evict inline-handler
//! cancellation incident is the cautionary tale). The handler writes
//! "this session wants capacity" durably and returns; the scanner — on
//! any coord replica — owns the placement retry. Each per-session advance
//! is `SessionLeaseGuard`-guarded, so two replicas can't double-place the
//! same session.
//!
//! ## Per-fit-class FIFO (queue-fairness follow-up to ADR 0048)
//!
//! The placement predicate is exactly the 2D `(mem_budget_mib,
//! cpu_budget_vcpus)` fit — image readiness isn't even consulted on the
//! queue path (`ScheduleContext::required_image_digest` is always `None`
//! in [`place_create`] / [`resume_has_capacity`]). Two queued sessions
//! with equal budgets are therefore equi-placeable by construction, so
//! [`FitClass`] partitions the FIFO-ordered queue by that pair
//! ([`partition_queue`]) and each class sweeps strict FIFO independently.
//! Classes are attempted oldest-head-first (the most-starved class gets
//! first claim on freed capacity this tick); a create that hits
//! `NoCapacity` stops only its class, not the whole sweep — a session
//! behind an unplaceable big head from a DIFFERENT class no longer
//! inherits that head's wait. Within a class, FIFO order is unchanged.
//!
//! One stop remains global: a resume-origin `NoCapacity` means
//! `candidates_for` returned zero schedulable hosts fleet-wide (not a fit
//! failure for one class — the fleet itself has no room), so it
//! legitimately breaks the whole sweep. Everything else (the boot
//! `JoinSet`, `boot_concurrency`, the stale-pending crash recovery, and
//! the timeout check) is unchanged and still applies fleet-/sweep-wide.
//!
//! **Starvation, deliberately unaddressed:** oldest-head-first class
//! ordering gives a starved big class the FIRST placement attempt every
//! sweep, but small classes may keep consuming the capacity a big head
//! would need to accumulate. We don't add reservation/backfill
//! accounting for this — on this fleet a session either fits in freed
//! capacity or times out at 30 minutes, and the real fix for a
//! never-fitting head is more capacity (fleet sizing/autoscaling — out of
//! scope here). The timeout remains the backstop.
//!
//! ## Push-driven, not polled
//!
//! `spawn` parks on a shared [`tokio::sync::Notify`] that `pg_listener`
//! fires on every `placement_changed` NOTIFY — emitted by `engram-postgres`
//! at every discrete placement-feasibility event (a reservation freed, a
//! `pending` reservation released, a host (re)registered or uncordoned, a
//! session freshly enqueued; see `PostgresStore::notify_placement_changed`).
//! `ENGRAM_QUEUE_POLL_SECS` (default 30) is now only the fallback for a
//! dropped NOTIFY, a `draining → ready` flip (arrives via heartbeat, which
//! deliberately gets no NOTIFY of its own — a per-heartbeat NOTIFY would be
//! a busy-loop), or allocatable drift. `ENGRAM_QUEUE_RETRY_SECS` (default
//! 5) is a one-shot re-arm: a sweep that requeued or errored a session
//! schedules a short retry so transient boot/prepare failures keep the
//! old ≤5s retry latency instead of waiting for the 30s fallback.
//! `spawn` also parks on the same `wake`-or-`poll_interval` race before
//! its very first sweep (not just between sweeps) — a beat for hosts to
//! heartbeat back in on a cold coordinator start, same rationale as
//! `evac_resumer::spawn`'s skip-the-first-tick.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::types::session::{QueueOrigin, QueuedSession, SessionState};
use engram_core::SessionId;
use tokio::sync::Notify;

use crate::idle_evictor::SessionLeaseGuard;
use crate::state::{SessionEvent, SharedState};

#[derive(Clone, Debug)]
pub struct QueueScannerConfig {
    /// Fallback poll interval — the backstop for a dropped NOTIFY /
    /// heartbeat-only `draining→ready` flips / allocatable drift.
    /// `ENGRAM_QUEUE_POLL_SECS`, default 30s. The happy-path dequeue
    /// latency is push-driven (see the module doc); this is no longer
    /// the primary drive mechanism.
    pub poll_interval: Duration,
    /// One-shot re-arm after a sweep that requeued or errored a session
    /// (transient boot/prepare failure). `ENGRAM_QUEUE_RETRY_SECS`,
    /// default 5s — keeps the pre-NOTIFY retry latency for that case
    /// instead of waiting for `poll_interval`.
    pub retry_interval: Duration,
    /// How long a session may wait before it's failed (create) / returned
    /// to Idle (resume). `ENGRAM_QUEUE_TIMEOUT_SECS`, default 1800 (30
    /// min). Deliberately generous — a backstop for the permanently-stuck
    /// case (maxHosts hit, cloud quota/stockout), NOT a budget for normal
    /// scale-up (node provision + image prefetch is minutes). We prefer a
    /// long queue to dropping a request.
    pub timeout: Duration,
    /// Crash-recovery horizon: a `pending` placed-queued row whose boot
    /// stalled past this (a coord died mid-boot) is requeued.
    pub stale_pending: Duration,
    /// Max concurrent boots per tick, shared across every fit class.
    pub boot_concurrency: usize,
}

impl Default for QueueScannerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(env_secs("ENGRAM_QUEUE_POLL_SECS", 30)),
            retry_interval: Duration::from_secs(env_secs("ENGRAM_QUEUE_RETRY_SECS", 5)),
            timeout: Duration::from_secs(env_secs("ENGRAM_QUEUE_TIMEOUT_SECS", 1800)),
            stale_pending: Duration::from_secs(env_secs("ENGRAM_QUEUE_STALE_PENDING_SECS", 600)),
            boot_concurrency: env_secs("ENGRAM_QUEUE_BOOT_CONCURRENCY", 4) as usize,
        }
    }
}

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// A queue partition key: the exact 2D fit the placement predicate uses.
/// Two queued sessions with the same class are equi-placeable by
/// construction, so per-class FIFO-stop preserves the queue's fairness
/// invariant within a class while removing cross-class head-of-line
/// blocking. See the module doc.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct FitClass {
    mem_budget_mib: i64,
    cpu_budget_vcpus: i32,
}

impl FitClass {
    fn of(q: &QueuedSession) -> Self {
        Self {
            mem_budget_mib: q.mem_budget_mib,
            cpu_budget_vcpus: q.cpu_budget_vcpus,
        }
    }
}

/// Pure: partition a `queued_at`-ascending list into classes, each class
/// FIFO-ordered internally (preserving input order), classes ordered by
/// their head's `queued_at` (oldest head first — the most-starved class
/// gets first claim on freed capacity this sweep).
///
/// Correct by construction from the input contract alone: since `queued`
/// arrives sorted `queued_at ASC` (`list_queued_sessions_fifo`), the
/// first time a class is encountered while walking the list IS its
/// oldest (head) member, so classes-in-first-encounter-order is exactly
/// classes-ordered-by-head-queued_at — no separate sort needed.
fn partition_queue(queued: Vec<QueuedSession>) -> Vec<Vec<QueuedSession>> {
    let mut index: HashMap<FitClass, usize> = HashMap::new();
    let mut classes: Vec<Vec<QueuedSession>> = Vec::new();
    for q in queued {
        let key = FitClass::of(&q);
        let idx = *index.entry(key).or_insert_with(|| {
            classes.push(Vec::new());
            classes.len() - 1
        });
        classes[idx].push(q);
    }
    classes
}

/// Spawn the queue scanner. Mirrors [`crate::evac_resumer::spawn`], plus
/// the shared `wake` handle: `pg_listener` fires it on `placement_changed`
/// NOTIFYs so a sweep runs as soon as capacity might have freed, instead
/// of waiting for `poll_interval`.
pub fn spawn(
    cfg: QueueScannerConfig,
    state: SharedState,
    wake: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Mirrors `evac_resumer::spawn`'s skip-the-first-immediate-tick:
        // coord just started, so don't sweep at the very first instant.
        // `wake` still lets a real `placement_changed` NOTIFY (a host
        // registering, a session enqueuing — all written straight to the
        // shared Postgres this replica already reads) cut the wait short,
        // same as every later iteration; only a truly cold, event-free
        // start (e.g. fleet-wide coordinator restart, no host has
        // heartbeated to ANY replica yet) rides out the full
        // `poll_interval` before the first sweep, giving hosts a beat to
        // land before the scanner judges what fits.
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(cfg.poll_interval) => {}
        }
        loop {
            let retry_needed = match run_once(&cfg, &state).await {
                Ok(summary) => summary.needs_retry(),
                Err(e) => {
                    tracing::warn!(error = %e, "queue-scanner tick failed; will retry");
                    true
                }
            };
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(cfg.poll_interval) => {}
                _ = retry_sleep(retry_needed, cfg.retry_interval) => {}
            }
        }
    })
}

/// Resolves after `dur` when `armed`; never resolves otherwise. Lets the
/// `select!` in [`spawn`] carry a third, conditionally-armed branch
/// without an `Option<Sleep>` / pinning dance.
async fn retry_sleep(armed: bool, dur: Duration) {
    if armed {
        tokio::time::sleep(dur).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Per-tick outcome counts, used by [`spawn`] to decide whether to arm
/// the short retry re-sleep (see the module doc).
#[derive(Clone, Copy, Debug, Default)]
pub struct RunSummary {
    pub placed: u32,
    pub requeued: u32,
    pub errored: u32,
}

impl RunSummary {
    fn needs_retry(&self) -> bool {
        self.requeued > 0 || self.errored > 0
    }
}

/// One sweep. `pub` (not `pub(crate)`) so live-PG integration tests in
/// `tests/queue_scanner_live_pg.rs` can drive it deterministically against
/// real Postgres — the per-class fit logic ultimately rests on
/// `place_queued_session`'s SQL, which only a live database can exercise.
pub async fn run_once(
    cfg: &QueueScannerConfig,
    state: &SharedState,
) -> Result<RunSummary, Box<dyn std::error::Error + Send + Sync>> {
    // Crash recovery first: reclaim placed-queued rows whose boot stalled.
    match state
        .services
        .meta
        .requeue_stale_pending(cfg.stale_pending)
        .await
    {
        Ok(n) if n > 0 => {
            tracing::warn!(
                count = n,
                "queue-scanner: requeued stale pending (coord died mid-boot)"
            )
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "queue-scanner: requeue_stale_pending failed"),
    }

    let queued = state.services.meta.list_queued_sessions_fifo().await?;
    sample_queue_metrics(state, &queued).await;
    if queued.is_empty() {
        return Ok(RunSummary::default());
    }

    let mut summary = RunSummary::default();
    let mut boots: tokio::task::JoinSet<BootOutcomeKind> = tokio::task::JoinSet::new();
    'classes: for class in partition_queue(queued) {
        for q in class {
            // Timeouts are checked first so a stuck head is failed out
            // rather than blocking its class forever.
            let now = Utc::now();
            if now
                .signed_duration_since(q.queued_at)
                .to_std()
                .unwrap_or_default()
                > cfg.timeout
            {
                time_out_session(state, &q).await;
                continue;
            }

            // One serialized placement attempt at the head of this class,
            // under the lease.
            let lease = match SessionLeaseGuard::try_acquire(state, q.session.id, None).await {
                Ok(Some(g)) => g,
                Ok(None) => continue, // a sibling replica owns it this tick
                Err(e) => {
                    tracing::warn!(session_id = %q.session.id, error = %e,
                        "queue-scanner: lease acquire failed; skipping");
                    continue;
                }
            };

            match q.origin {
                QueueOrigin::Create => {
                    match place_create(state, &q).await {
                        PlaceOutcome::Placed(host_id) => {
                            // The durable `queued → pending` flip IS the
                            // placement moment — record here, not after
                            // boot, so a later boot failure/requeue
                            // doesn't lose the sample (the requeued
                            // session's NEXT placement records its own,
                            // cumulative-since-original-queued_at, sample).
                            ::metrics::histogram!(
                                crate::metrics::QUEUE_WAIT_SECONDS,
                                "origin" => "create",
                                "outcome" => "placed",
                            )
                            .record(wait_duration(&q).as_secs_f64());
                            // Hand the boot to the bounded pool; the lease
                            // rides into the task so it's held for the
                            // whole boot.
                            if boots.len() >= cfg.boot_concurrency {
                                // Drain one before queuing more — bounds
                                // concurrency (global across classes).
                                if let Some(res) = boots.join_next().await {
                                    record_boot_outcome(res, &mut summary);
                                }
                            }
                            let st = state.clone();
                            let q2 = q.clone();
                            boots.spawn(async move {
                                boot_placed_create(&st, q2, host_id, lease).await
                            });
                        }
                        PlaceOutcome::NoCapacity => {
                            // This class's head doesn't fit → stop THIS
                            // class only (strict FIFO within the class);
                            // the outer loop moves on to the next class.
                            drop(lease);
                            break;
                        }
                        PlaceOutcome::Error => {
                            drop(lease);
                            summary.errored += 1;
                            // Skip this one; don't starve the rest on a transient.
                            continue;
                        }
                        PlaceOutcome::ImageGone => {
                            // `place_create` already terminally failed the
                            // session (review finding 3) — just release and
                            // keep sweeping; an image-gone head must not block
                            // the rest of the queue.
                            drop(lease);
                            continue;
                        }
                    }
                }
                QueueOrigin::Resume => {
                    // Only dequeue when a host is actually available —
                    // otherwise leave it queued (don't churn Idle↔Queued,
                    // which would reset the timeout clock). Empty
                    // candidates = no schedulable host fleet-wide (not a
                    // per-class fit failure), so — unlike create's
                    // NoCapacity — this is the one legitimate GLOBAL stop:
                    // no class's resume can dequeue if the fleet has zero
                    // schedulable hosts.
                    match resume_has_capacity(state, &q).await {
                        Some(true) => {
                            dequeue_resume(state, &q).await;
                            drop(lease);
                        }
                        Some(false) => {
                            drop(lease);
                            break 'classes;
                        }
                        None => {
                            drop(lease);
                            summary.errored += 1;
                            continue; // transient read error; don't starve the rest
                        }
                    }
                }
            }
        }
    }
    // Let in-flight boots finish (bounded; the next tick re-sweeps anyway).
    while let Some(res) = boots.join_next().await {
        record_boot_outcome(res, &mut summary);
    }
    Ok(summary)
}

/// Wall-clock elapsed since `q` was queued (never negative).
fn wait_duration(q: &QueuedSession) -> Duration {
    Utc::now()
        .signed_duration_since(q.queued_at)
        .to_std()
        .unwrap_or_default()
}

enum PlaceOutcome {
    Placed(engram_core::HostId),
    NoCapacity,
    Error,
    /// Review finding 3 (PR #565): the image was live when this session
    /// queued but was disabled while it waited. `place_create` already
    /// terminally failed the session (image-shaped, not a queue timeout);
    /// the caller just drops the lease and moves on.
    ImageGone,
}

/// Outcome of a spawned [`boot_placed_create`] task, folded into the
/// sweep's [`RunSummary`] by [`record_boot_outcome`].
enum BootOutcomeKind {
    Placed,
    Requeued,
    Failed,
}

fn record_boot_outcome(
    res: Result<BootOutcomeKind, tokio::task::JoinError>,
    summary: &mut RunSummary,
) {
    match res {
        Ok(BootOutcomeKind::Placed) => summary.placed += 1,
        Ok(BootOutcomeKind::Requeued) => summary.requeued += 1,
        Ok(BootOutcomeKind::Failed) => summary.errored += 1,
        Err(e) => {
            tracing::warn!(error = %e, "queue-scanner: boot task panicked");
            summary.errored += 1;
        }
    }
}

/// Re-attempt placement for a create-origin queued session: rank
/// candidates, then atomically flip `queued → pending` on a fitting host.
async fn place_create(state: &SharedState, q: &QueuedSession) -> PlaceOutcome {
    let (repo, tag) = engram_core::types::session::split_image_ref(&q.session.image);
    // ADR 0036 amendment (issue #538): same digest gate the live create
    // path applies (`api/sessions.rs::boot_prepared`) — a queued create is
    // still a CREATE, so it must not place onto a host that hasn't staged
    // this image's base snapshot.
    //
    // Review finding 3 (PR #565): this MUST be the live-only lookup
    // (`get_enabled_image`, matching the fresh-create path at
    // `sessions.rs::prepare_from_grpc`), not the tolerant
    // `get_enabled_image_any` `prepare_from_row` uses. That tolerant
    // lookup's rationale is the RESUME path's invariant (a session pins its
    // own lineage, so a soft-deleted image is still fine to resume onto) —
    // it does not hold for a CREATE that hasn't placed yet. With the
    // tolerant lookup, disabling an image out from under a queued create
    // pins `required_image_digest` to a digest every host's prefetch
    // supervisor has already unpinned (`image_prefetch.rs`), so candidates
    // are empty on every sweep forever and the session dies at queue
    // timeout with a misleading capacity-shaped error instead of a crisp
    // image-shaped one.
    let required_image_digest = match state
        .services
        .meta
        .get_enabled_image(&q.session.image)
        .await
    {
        Ok(Some(row)) => Some(engram_protocol::heartbeat::ManifestDigest::new(
            row.manifest_digest,
        )),
        Ok(None) => {
            // Live lookup miss: the image was disabled while this session
            // queued. Fail it now, image-shaped, instead of wedging until
            // the (much longer) queue timeout.
            return fail_queued_create_image_gone(state, q).await;
        }
        Err(e) => {
            // Transient PG error: keep the old degrade-to-no-gate behavior
            // rather than failing a session over a blip — `place_queued_session`
            // still enforces capacity, and `boot_placed_create`'s
            // `prepare_from_row` (tolerant, correctly) will 404 on a
            // genuinely-gone image right after if this guess was wrong.
            tracing::debug!(session_id = %q.session.id, error = %e,
                "queue-scanner: enabled-image lookup failed for digest gate; placing without it");
            None
        }
    };
    // ADR 0068: same derivation `prepare_inner` uses for a live create —
    // the enabled image's base snapshot carrying a memory manifest means
    // this create needs the FC UFFD substrate. Tolerant lookup (mirrors
    // `prepare_from_row`): a transient PG hiccup or a since-disabled
    // image degrades to "no substrate requirement" rather than blocking
    // the whole re-place attempt — `boot_on_reserved_host` re-resolves
    // the enabled image properly and fails there if it's truly gone.
    let needs_uffd_substrate = state
        .services
        .meta
        .get_enabled_image_any(&q.session.image)
        .await
        .ok()
        .flatten()
        .map(|e| e.base_snapshot_memory_manifest.is_some())
        .unwrap_or(false);
    let ctx = crate::placement::ScheduleContext {
        repo,
        image_version: tag,
        prefer_snapshot_id: None,
        memory_mib: Some(q.mem_budget_mib.max(0) as u32),
        cpu_budget_vcpus: Some(q.cpu_budget_vcpus.max(0) as u32),
        required_image_digest,
        exclude_host: None,
        prefer_host: None,
        caps: crate::placement::CapabilityRequirements {
            needs_uffd_substrate,
            fc_snapshot_version: None,
        },
    };
    let candidates =
        match crate::placement::candidates_for(state.services.meta.as_ref(), &ctx).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(session_id = %q.session.id, error = ?e,
                "queue-scanner: candidates_for failed");
                return PlaceOutcome::Error;
            }
        };
    match state
        .services
        .meta
        .place_queued_session(
            q.session.id,
            q.mem_budget_mib,
            q.cpu_budget_vcpus,
            &candidates.hosts,
            candidates.affinity_len,
        )
        .await
    {
        Ok(Some(host_id)) => PlaceOutcome::Placed(host_id),
        Ok(None) => PlaceOutcome::NoCapacity,
        Err(e) => {
            tracing::warn!(session_id = %q.session.id, error = %e,
                "queue-scanner: place_queued_session failed");
            PlaceOutcome::Error
        }
    }
}

/// Boot a placed (now `pending`) create-origin session. Emits
/// `Queued → Pending`, runs the shared boot pipeline, and on failure
/// requeues (NotStarted, still `pending`) or fails (Started, reached
/// `created`). `lease` is held for the whole boot.
///
/// ADR 0019 / telemetry restoration (#526): this runs on a scanner
/// `JoinSet` task with no request span to inherit — an explicit root
/// (carrying `session_id`) so its spans correlate instead of exporting
/// as disconnected roots.
#[tracing::instrument(name = "queue_scanner.boot_placed_create", skip_all, fields(session_id = %q.session.id, %host_id))]
async fn boot_placed_create(
    state: &SharedState,
    q: QueuedSession,
    host_id: engram_core::HostId,
    _lease: SessionLeaseGuard,
) -> BootOutcomeKind {
    let session_id = q.session.id;
    emit(
        state,
        session_id,
        SessionState::Queued,
        SessionState::Pending,
    )
    .await;

    let prepared =
        match crate::api::sessions::prepare_from_row(state, &q.session, q.prompt.clone()).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%session_id, error = %e,
                "queue-scanner: prepare_from_row failed; requeueing");
                let _ = state.services.meta.requeue_session(session_id).await;
                return BootOutcomeKind::Requeued;
            }
        };
    match crate::session_boot::boot_on_reserved_host(state, prepared.inputs, host_id).await {
        Ok(()) => {
            ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "placed")
                .increment(1);
            tracing::info!(%session_id, %host_id, "queue-scanner: placed + booted");
            BootOutcomeKind::Placed
        }
        Err(crate::session_boot::BootError::NotStarted(e)) => {
            // Row still `pending` — return it to the queue (timeout clock
            // keeps the original queued_at, since requeue doesn't reset it).
            tracing::warn!(%session_id, error = %e, "queue-scanner: boot not-started; requeueing");
            let _ = state.services.meta.requeue_session(session_id).await;
            ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "requeued")
                .increment(1);
            BootOutcomeKind::Requeued
        }
        Err(crate::session_boot::BootError::Started(e)) => {
            // Reached `created` then failed — terminal (requeue illegal).
            tracing::warn!(%session_id, error = %e, "queue-scanner: boot failed past created; failing");
            let _ = state
                .services
                .meta
                .transition_session(session_id, SessionState::Failed)
                .await;
            ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "failed")
                .increment(1);
            BootOutcomeKind::Failed
        }
    }
}

/// Is there a schedulable host for a resume-origin session? `Some(true)`
/// = a candidate exists (resume's own soft placement will pick one),
/// `Some(false)` = none (stay queued), `None` = transient read error.
async fn resume_has_capacity(state: &SharedState, q: &QueuedSession) -> Option<bool> {
    let (repo, tag) = engram_core::types::session::split_image_ref(&q.session.image);
    // ADR 0036 amendment (issue #538): `required_image_digest` below is
    // deliberately `None`, NOT the create-path digest gate. This is the
    // RESUME-origin arm (a session that was Idle and got queued for
    // capacity, ADR 0048 C7) — resume places by snapshot affinity, not
    // base-image residency, same as every other resume/evac/admin call
    // site (`api/snapshot.rs`, `evacuation.rs`, `api/admin.rs`). The issue
    // text that seeded this change named `queue_scanner.rs:218,321`
    // together, but 321 is this function, not the create-origin
    // `place_create` above (218) — gating it would incorrectly block a
    // resume's re-queue check on base-image prestage status for a
    // session that already has its own snapshot.
    //
    // ADR 0068: same pairing `resume_from_fc_snapshot` uses. This is a
    // soft pre-check (does ANY host look schedulable before we bother
    // dequeueing) — the authoritative gate is the real resume path's own
    // `ScheduleContext`, which re-derives this from the snapshot it
    // actually restores. A stale/missing lookup here just means one
    // extra requeue cycle, not a correctness gap.
    let latest = state
        .services
        .meta
        .latest_snapshot_for_session(q.session.id)
        .await
        .ok()
        .flatten();
    let ctx = crate::placement::ScheduleContext {
        repo,
        image_version: tag,
        prefer_snapshot_id: None,
        memory_mib: Some(q.mem_budget_mib.max(0) as u32),
        cpu_budget_vcpus: Some(q.cpu_budget_vcpus.max(0) as u32),
        required_image_digest: None,
        exclude_host: None,
        prefer_host: None,
        caps: crate::placement::CapabilityRequirements {
            needs_uffd_substrate: latest.as_ref().is_some_and(|s| s.memory_manifest.is_some()),
            fc_snapshot_version: latest.and_then(|s| s.fc_snapshot_version),
        },
    };
    match crate::placement::candidates_for(state.services.meta.as_ref(), &ctx).await {
        Ok(c) => Some(!c.hosts.is_empty()),
        Err(e) => {
            tracing::warn!(session_id = %q.session.id, error = ?e,
                "queue-scanner: resume candidates_for failed");
            None
        }
    }
}

/// Dequeue a resume-origin session: `Queued → Idle`, then drive a resume
/// inline. The resume path owns its own placement + re-enqueues if it
/// hits no capacity (ADR 0048 C7), so this is self-correcting.
async fn dequeue_resume(state: &SharedState, q: &QueuedSession) {
    let session_id = q.session.id;
    match state
        .services
        .meta
        .transition_session(session_id, SessionState::Idle)
        .await
    {
        Ok(prev) => {
            emit_from(state, session_id, prev, SessionState::Idle).await;
            ::metrics::histogram!(
                crate::metrics::QUEUE_WAIT_SECONDS,
                "origin" => "resume",
                "outcome" => "placed",
            )
            .record(wait_duration(q).as_secs_f64());
        }
        Err(e) => {
            tracing::warn!(%session_id, error = %e,
                "queue-scanner: Queued→Idle failed; leaving queued");
            return;
        }
    }
    // Best-effort inline resume; a no-capacity result re-enqueues itself.
    if let Err(e) = crate::api::snapshot::resume_session(state.clone(), session_id).await {
        tracing::info!(%session_id, error = %e,
            "queue-scanner: resume after dequeue did not complete (may re-queue)");
    }
}

/// Review finding 3 (PR #565): fail a queued create whose target image was
/// disabled while it waited — a crisp image-shaped failure now, instead of
/// silently degrading the digest gate (which would wedge candidates empty
/// on every sweep until the much longer queue timeout). Mirrors
/// `time_out_session`'s `Create` arm: user-visible event before the
/// terminal flip.
async fn fail_queued_create_image_gone(state: &SharedState, q: &QueuedSession) -> PlaceOutcome {
    let session_id = q.session.id;
    if let Err(e) = state
        .services
        .meta
        .append_session_event(
            session_id,
            "queue_image_gone",
            serde_json::json!({
                "image": q.session.image,
                "reason": "image was disabled while the session waited for capacity",
            }),
        )
        .await
    {
        tracing::warn!(%session_id, error = %e, "queue-scanner: queue_image_gone event failed");
    }
    match state
        .services
        .meta
        .transition_session(session_id, SessionState::Failed)
        .await
    {
        Ok(prev) => emit_from(state, session_id, prev, SessionState::Failed).await,
        Err(e) => tracing::warn!(%session_id, error = %e,
            "queue-scanner: image-gone Queued→Failed failed"),
    }
    ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "image_gone")
        .increment(1);
    tracing::warn!(%session_id, image = %q.session.image,
        "queue-scanner: queued create's image was disabled while waiting; failing");
    PlaceOutcome::ImageGone
}

/// Time out a head-of-queue session: create → `Failed` + a user-visible
/// `queue_timeout` event; resume → back to `Idle` (durable, retryable).
async fn time_out_session(state: &SharedState, q: &QueuedSession) {
    let session_id = q.session.id;
    let waited = Utc::now()
        .signed_duration_since(q.queued_at)
        .num_seconds()
        .max(0);
    match q.origin {
        QueueOrigin::Create => {
            // User-visible reason BEFORE the terminal flip so subscribers
            // see why it failed.
            if let Err(e) = state
                .services
                .meta
                .append_session_event(
                    session_id,
                    "queue_timeout",
                    serde_json::json!({
                        "waited_secs": waited,
                        "reason": "no host capacity became available in time",
                    }),
                )
                .await
            {
                tracing::warn!(%session_id, error = %e, "queue-scanner: queue_timeout event failed");
            }
            match state
                .services
                .meta
                .transition_session(session_id, SessionState::Failed)
                .await
            {
                Ok(prev) => emit_from(state, session_id, prev, SessionState::Failed).await,
                Err(e) => tracing::warn!(%session_id, error = %e,
                    "queue-scanner: timeout Queued→Failed failed"),
            }
        }
        QueueOrigin::Resume => match state
            .services
            .meta
            .transition_session(session_id, SessionState::Idle)
            .await
        {
            Ok(prev) => emit_from(state, session_id, prev, SessionState::Idle).await,
            Err(e) => tracing::warn!(%session_id, error = %e,
                "queue-scanner: timeout Queued→Idle failed"),
        },
    }
    ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "timeout").increment(1);
    ::metrics::histogram!(
        crate::metrics::QUEUE_WAIT_SECONDS,
        "origin" => q.origin.as_str(),
        "outcome" => "timeout",
    )
    .record(waited as f64);
    tracing::info!(%session_id, waited_secs = waited, origin = ?q.origin,
        "queue-scanner: session timed out waiting for capacity");
}

async fn sample_queue_metrics(state: &SharedState, queued: &[QueuedSession]) {
    if let Ok(d) = state.services.meta.queued_demand().await {
        ::metrics::gauge!(crate::metrics::SESSIONS_QUEUED).set(d.sessions as f64);
        ::metrics::gauge!(crate::metrics::SESSIONS_QUEUED_MIB).set(d.mem_mib as f64);
    }
    // Age of the oldest queued row this tick — 0 when the queue is empty.
    // `queued` is already `queued_at ASC` (list_queued_sessions_fifo), so
    // the head is `queued[0]`.
    let head_age = queued
        .first()
        .map(|q| wait_duration(q).as_secs_f64())
        .unwrap_or(0.0);
    ::metrics::gauge!(crate::metrics::QUEUE_HEAD_AGE_SECONDS).set(head_age);
}

async fn emit(state: &SharedState, id: SessionId, from: SessionState, to: SessionState) {
    emit_from(state, id, from, to).await;
}

async fn emit_from(state: &SharedState, id: SessionId, from: SessionState, to: SessionState) {
    if let Err(e) = state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from,
                to,
                at: Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(session_id = %id, ?from, ?to, error = %e,
            "queue-scanner: status-changed emit failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::session::{Session, SessionMode};

    fn queued(id_seed: u8, mem: i64, cpu: i32, secs_ago: i64) -> QueuedSession {
        let id = SessionId::new();
        QueuedSession {
            session: Session {
                id,
                status: SessionState::Queued,
                host_id: None,
                sandbox_id: None,
                image: format!("test/repo:class-{id_seed}"),
                mode: SessionMode::Agent,
                created_at: Utc::now(),
                last_active_at: Utc::now(),
                live_disk_manifest: None,
            },
            origin: QueueOrigin::Create,
            prompt: None,
            mem_budget_mib: mem,
            cpu_budget_vcpus: cpu,
            queued_at: Utc::now() - chrono::Duration::seconds(secs_ago),
        }
    }

    #[test]
    fn empty_input_yields_no_classes() {
        assert!(partition_queue(Vec::new()).is_empty());
    }

    #[test]
    fn single_class_stays_one_group_in_order() {
        let a = queued(1, 4096, 2, 30);
        let b = queued(1, 4096, 2, 10);
        let ids = [a.session.id, b.session.id];
        let classes = partition_queue(vec![a, b]);
        assert_eq!(classes.len(), 1);
        assert_eq!(
            classes[0].iter().map(|q| q.session.id).collect::<Vec<_>>(),
            ids,
            "within-class order must be preserved (FIFO)"
        );
    }

    #[test]
    fn mixed_budgets_partition_into_separate_classes() {
        // A big head (queued first) followed by two small sessions of the
        // SAME (small) class, queued after it.
        let big = queued(1, 32_768, 8, 100);
        let small1 = queued(2, 1024, 1, 50);
        let small2 = queued(2, 1024, 1, 10);
        let classes = partition_queue(vec![big.clone(), small1.clone(), small2.clone()]);
        assert_eq!(
            classes.len(),
            2,
            "two distinct (mem, cpu) pairs => two classes"
        );
        // Oldest-head-first: the big session's class was seen first (it's
        // the oldest row overall), so its class comes first.
        assert_eq!(classes[0].len(), 1);
        assert_eq!(classes[0][0].session.id, big.session.id);
        assert_eq!(classes[1].len(), 2);
        assert_eq!(
            classes[1].iter().map(|q| q.session.id).collect::<Vec<_>>(),
            vec![small1.session.id, small2.session.id],
            "within the small class, FIFO order (small1 queued before small2)"
        );
    }

    #[test]
    fn classes_ordered_by_head_queued_at_not_first_literal_occurrence_bias() {
        // Interleaved arrival: class B's head is older than class A's
        // head, but class A's SECOND member interleaves before class B's
        // second member. Class order must still follow each class's own
        // head (first-seen) position, not some other bias.
        let a1 = queued(10, 2048, 1, 40); // class A head (oldest overall... no, see below)
        let b1 = queued(20, 8192, 4, 90); // class B head — oldest overall
        let a2 = queued(10, 2048, 1, 20);
        let b2 = queued(20, 8192, 4, 5);
        // Input must already be queued_at ASC per the store contract:
        // b1 (100) is NOT oldest here on purpose — reorder to respect
        // that contract: b1, a1, a2, b2 by descending secs_ago.
        let input = vec![b1.clone(), a1.clone(), a2.clone(), b2.clone()];
        let classes = partition_queue(input);
        assert_eq!(classes.len(), 2);
        // b1 appeared first in the queued_at-ascending input => class B's
        // group comes first.
        assert_eq!(classes[0][0].session.id, b1.session.id);
        assert_eq!(classes[0][1].session.id, b2.session.id);
        assert_eq!(classes[1][0].session.id, a1.session.id);
        assert_eq!(classes[1][1].session.id, a2.session.id);
    }
}
