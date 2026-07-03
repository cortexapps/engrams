//! ADR 0048 C6: the session queue scanner.
//!
//! When `reserve_placement` finds no host with capacity, the create
//! handler enqueues the session (`status='queued'`) instead of 503-ing,
//! and a resume that hits the same wall parks the idle session back in
//! the queue. This background task is the continuation: each tick it
//! sweeps the queue oldest-first and either places + boots a session
//! (capacity arrived / the fleet scaled up) or fails it on a generous
//! timeout.
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
//! ## FIFO, place-until-first-failure
//!
//! The sweep is strict FIFO and stops at the first session that can't be
//! placed this tick — head-of-line blocking is deliberate (fairness + a
//! simple invariant: the operator scales the fleet to fit the head,
//! because `queued_demand` includes it). A placed session's boot runs on
//! a bounded `JoinSet` so the sweep keeps placing while boots proceed.

use std::time::Duration;

use chrono::Utc;
use engram_core::types::session::{QueueOrigin, QueuedSession, SessionState};
use engram_core::SessionId;

use crate::idle_evictor::SessionLeaseGuard;
use crate::state::{SessionEvent, SharedState};

#[derive(Clone, Debug)]
pub struct QueueScannerConfig {
    /// How often to sweep the queue. `ENGRAM_QUEUE_POLL_SECS`, default 5s
    /// — tighter than the evac/dead-host scanners (10s) because a queued
    /// session is a user actively waiting to start.
    pub poll_interval: Duration,
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
    /// Max concurrent boots per tick.
    pub boot_concurrency: usize,
}

impl Default for QueueScannerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(env_secs("ENGRAM_QUEUE_POLL_SECS", 5)),
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

/// Spawn the queue scanner. Mirrors [`crate::evac_resumer::spawn`].
pub fn spawn(cfg: QueueScannerConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        tick.tick().await; // skip the immediate first tick
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "queue-scanner tick failed; will retry");
            }
        }
    })
}

/// One sweep. `pub(crate)` so live-PG tests drive it deterministically.
pub(crate) async fn run_once(
    cfg: &QueueScannerConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    sample_queue_metrics(state).await;
    if queued.is_empty() {
        return Ok(());
    }

    let mut boots = tokio::task::JoinSet::new();
    for q in queued {
        // FIFO: stop at the first session we can't place this tick (the
        // operator scales to fit the head). Timeouts are checked first so
        // a stuck head is failed out rather than blocking forever.
        let now = Utc::now();
        if now
            .signed_duration_since(q.queued_at)
            .to_std()
            .unwrap_or_default()
            > cfg.timeout
        {
            time_out_session(state, &q).await;
            continue; // a timed-out head shouldn't block the rest
        }

        // One serialized placement attempt at the head, under the lease.
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
                        // Hand the boot to the bounded pool; the lease rides
                        // into the task so it's held for the whole boot.
                        if boots.len() >= cfg.boot_concurrency {
                            // Drain one before queuing more — bounds concurrency.
                            let _ = boots.join_next().await;
                        }
                        let st = state.clone();
                        let q2 = q.clone();
                        boots.spawn(async move {
                            boot_placed_create(&st, q2, host_id, lease).await;
                        });
                    }
                    PlaceOutcome::NoCapacity => {
                        // Head doesn't fit → stop the sweep (strict FIFO).
                        drop(lease);
                        break;
                    }
                    PlaceOutcome::Error => {
                        drop(lease);
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
                // Only dequeue when a host is actually available — otherwise
                // leave it queued (don't churn Idle↔Queued, which would reset
                // the timeout clock). Empty candidates = no schedulable host
                // (fleet cordoned / scaled to zero) → FIFO stop, same as a
                // create that doesn't fit.
                match resume_has_capacity(state, &q).await {
                    Some(true) => {
                        dequeue_resume(state, &q).await;
                        drop(lease);
                    }
                    Some(false) => {
                        drop(lease);
                        break;
                    }
                    None => {
                        drop(lease);
                        continue; // transient read error; don't starve the rest
                    }
                }
            }
        }
    }
    // Let in-flight boots finish (bounded; the next tick re-sweeps anyway).
    while boots.join_next().await.is_some() {}
    Ok(())
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
    let ctx = crate::placement::ScheduleContext {
        repo,
        image_version: tag,
        prefer_snapshot_id: None,
        memory_mib: Some(q.mem_budget_mib.max(0) as u32),
        cpu_budget_vcpus: Some(q.cpu_budget_vcpus.max(0) as u32),
        required_image_digest,
        exclude_host: None,
        prefer_host: None,
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
async fn boot_placed_create(
    state: &SharedState,
    q: QueuedSession,
    host_id: engram_core::HostId,
    _lease: SessionLeaseGuard,
) {
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
                return;
            }
        };
    match crate::session_boot::boot_on_reserved_host(state, prepared.inputs, host_id).await {
        Ok(()) => {
            ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "placed")
                .increment(1);
            tracing::info!(%session_id, %host_id, "queue-scanner: placed + booted");
        }
        Err(crate::session_boot::BootError::NotStarted(e)) => {
            // Row still `pending` — return it to the queue (timeout clock
            // keeps the original queued_at, since requeue doesn't reset it).
            tracing::warn!(%session_id, error = %e, "queue-scanner: boot not-started; requeueing");
            let _ = state.services.meta.requeue_session(session_id).await;
            ::metrics::counter!(crate::metrics::QUEUE_OUTCOME_TOTAL, "outcome" => "requeued")
                .increment(1);
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
        }
    }
}

/// Is there a schedulable host for a resume-origin session? `Some(true)`
/// = a candidate exists (resume's own soft placement will pick one),
/// `Some(false)` = none (stay queued), `None` = transient read error.
async fn resume_has_capacity(state: &SharedState, q: &QueuedSession) -> Option<bool> {
    let (repo, tag) = engram_core::types::session::split_image_ref(&q.session.image);
    // ADR 0036 amendment (issue #538): deliberately `None`, NOT the
    // create-path digest gate. This is the RESUME-origin arm (a session
    // that was Idle and got queued for capacity, ADR 0048 C7) — resume
    // places by snapshot affinity, not base-image residency, same as
    // every other resume/evac/admin call site (`api/snapshot.rs`,
    // `evacuation.rs`, `api/admin.rs`). The issue text that seeded this
    // change named `queue_scanner.rs:218,321` together, but 321 is this
    // function, not the create-origin `place_create` above (218) — gating
    // it would incorrectly block a resume's re-queue check on base-image
    // prestage status for a session that already has its own snapshot.
    let ctx = crate::placement::ScheduleContext {
        repo,
        image_version: tag,
        prefer_snapshot_id: None,
        memory_mib: Some(q.mem_budget_mib.max(0) as u32),
        cpu_budget_vcpus: Some(q.cpu_budget_vcpus.max(0) as u32),
        required_image_digest: None,
        exclude_host: None,
        prefer_host: None,
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
        Ok(prev) => emit_from(state, session_id, prev, SessionState::Idle).await,
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
    tracing::info!(%session_id, waited_secs = waited, origin = ?q.origin,
        "queue-scanner: session timed out waiting for capacity");
}

async fn sample_queue_metrics(state: &SharedState) {
    if let Ok(d) = state.services.meta.queued_demand().await {
        ::metrics::gauge!(crate::metrics::SESSIONS_QUEUED).set(d.sessions as f64);
        ::metrics::gauge!(crate::metrics::SESSIONS_QUEUED_MIB).set(d.mem_mib as f64);
    }
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
