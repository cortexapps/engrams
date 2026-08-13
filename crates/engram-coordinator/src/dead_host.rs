//! Dead-host auto-detector.
//!
//! ADR 0116 A-D4: the death path keys on the host **binding lease**
//! (`hosts.lease_expires_at`) and nothing else. Register writes the
//! lease, every heartbeat renews it, and a planned operation (an
//! operator roll, a SIGTERM ladder) extends it with an explicit
//! handoff deadline sized to cover the operation — so an expired
//! lease is not "the host is quiet", it is "the host's coverage ran
//! out with no planned reason". The pre-0116 staleness heuristics
//! (stale threshold, cordon multiplier, probe strikes, rescue grace)
//! existed to soften inference from silence; with the lease they are
//! retired, not tuned.
//!
//! Background task that polls for lease-expired hosts and races other
//! coordinator replicas — via a PG leasing row (`dead_host_inflight`,
//! ADR 0098 D4; the repo convention: leasing row over advisory lock)
//! — for the right to evacuate each candidate. The winner:
//!
//! 1. Atomically marks the host `Dead` in Postgres and transitions
//!    every non-terminal session pointed at it to `HostLost` with
//!    `host_id` / `sandbox_id` cleared (ADR 0015 M2). `HostLost` is
//!    the explicit "host went away, decide what to do next" state —
//!    M4 (session migration) will turn it into a re-pick on a peer
//!    host; until then we drive the second-stage transition here.
//! 2. For each session: looks up the latest snapshot. If a
//!    recoverable snapshot exists → `HostLost -> Idle` (the user can
//!    /resume from the snapshot); otherwise → `HostLost -> Dead`.
//! 3. Emits per-session `StatusChanged` events for both transitions
//!    using the honest `from` returned by the bulk + the
//!    second-stage transition_session calls.
//! 4. Broadcasts `host_dead` via `MetadataStore::notify_host_dead`
//!    (Postgres: `pg_notify`) so other replicas drop the host from
//!    their in-memory `HostRegistry` (handled in `pg_listener`).
//! 5. Unregisters the host locally.
//!
//! Active execs running on the dead host don't need explicit
//! synthesis: dropping the host's `RemoteSandboxBackend` cascades
//! through the WS demuxer's `Closed` state, the per-exec stream
//! channel drops, and `api/exec.rs` surfaces the end-without-Exit as
//! a retryable `Unavailable` (ADR 0103) — no completion is recorded,
//! so a caller holding a durable ticket can re-attach once the
//! session is recovered onto a live host.
//!
//! Without this detector, sessions on a dead host stay `Active`
//! forever (with a `host_id` pointing at a host that won't respond);
//! operators can still `POST /sessions/:id/migrate` by hand.
//!
//! **The probe rescue (issue #231, rk28, now durable):** before the
//! winner orphans anything it dials the host once. A host that answers
//! is alive — its lease lapsed because heartbeat *persistence* failed
//! (a peer pod's PG pool saturated), not the host — so the rescue
//! writes a fresh lease into the row (`renew_host_lease`) and skips.
//! The written renewal is what retired the per-replica strike counters
//! and the rescue-grace window: every replica's detector honors a
//! durable fact instead of each keeping private memory, and the rk28
//! shape (a single failed Ping seconds after a rescue) cannot kill a
//! host whose rescue bought it a full TTL. The mark itself re-checks
//! the lease under the row lock and aborts on a renewal that raced in
//! (`Conflict`) — the coordinator never revokes a binding whose lease
//! is live.
//!
//! **ADR 0045 Phase A (retire reactive evac):** the second stage
//! routes a recoverable session (snapshot row OR
//! `sessions.live_disk_manifest_*`) to `HostLost → Idle` for lazy
//! `/resume` on next access, and `HostLost → Dead` when there is no
//! recoverable state. The detector no longer routes into
//! `Evacuating` — the reactive auto-evac was the documented bug
//! source (the resume-from-idle wedge, the deploy-storm cascade), so
//! `Evacuating` is now reached *only* via operator drain (ADR 0044
//! K3), and the `evac_resumer` scanner relocates only those.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use engram_core::traits::{MetadataStore, SessionFence};
use engram_core::types::BindingDisposition;
use engram_core::types::SessionState;
use engram_core::{HostId, MetaError};

use crate::state::{IndexedEvent, SessionEvent, SessionEventBus, SharedState};
use engram_core::SessionId;

/// Persist + publish a `StatusChanged{from, to}` event. Inline here
/// (rather than via `SharedState::emit`) so the dead-host detector
/// can run without a full Services struct — same shape the reconcile
/// pass uses. Errors are logged and swallowed because the persisted
/// state already changed; rolling back the eviction is worse than a
/// missed SSE event.
async fn emit_status_changed(
    meta: &dyn MetadataStore,
    events: &SessionEventBus,
    session_id: SessionId,
    from: SessionState,
    to: SessionState,
    now: DateTime<Utc>,
) {
    let event = SessionEvent::StatusChanged { from, to, at: now };
    let kind = event.kind();
    let payload = match serde_json::to_value(&event) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "serialize StatusChanged failed");
            return;
        }
    };
    match meta.append_session_event(session_id, kind, payload).await {
        Ok(idx) => {
            events.publish(
                session_id,
                IndexedEvent {
                    idx,
                    event,
                    ephemeral: false,
                },
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "persist StatusChanged failed");
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeadHostConfig {
    /// How often to poll for lease-expired hosts. Worst-case detection
    /// latency is `host_lease_ttl + poll_interval` (45s + 10s at
    /// defaults) for a `kill -9`; a planned operation never enters the
    /// death path at all (its handoff deadline covers it).
    pub poll_interval: Duration,
    /// An eviction lease older than this is presumed abandoned (the
    /// claiming pod crashed mid-eviction) and may be taken over by any
    /// replica. Comfortably larger than a full eviction pass; small
    /// enough that a crashed pod delays a genuinely-dead host's
    /// eviction by at most this long. (This is the `dead_host_inflight`
    /// executor lease — replica failover, a different domain from the
    /// binding lease; ADR 0116 non-goals.)
    pub lease_stale_after: Duration,
}

impl Default for DeadHostConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            lease_stale_after: Duration::from_secs(180),
        }
    }
}

/// Spawn the detector as a background task. Returns a JoinHandle the
/// caller can drop on shutdown. Runs forever; logs and continues on
/// per-tick errors so a transient Postgres blip doesn't stop the loop.
pub fn spawn(cfg: DeadHostConfig, state: SharedState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Same claimant identity convention as the enable scanner: the
        // pod hostname, falling back for local/dev runs.
        let claimant = std::env::var("HOSTNAME").unwrap_or_else(|_| "coord".into());
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick — the coordinator just
        // started and no host's lease has had time to expire.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &state, &claimant).await {
                tracing::warn!(error = %e, "dead-host detector tick failed; will retry");
            }
        }
    })
}

/// One detector sweep. `pub` so tests and the DST harness (engram-dst,
/// ADR 0098 D5) drive it directly without the timer loop.
pub async fn run_once(
    cfg: &DeadHostConfig,
    state: &SharedState,
    claimant: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ADR 0116 A-D4: the lease predicate is the sole candidate source.
    let candidates = state.services.meta.list_lease_expired_hosts().await?;
    host_lost_straggler_sweep(state).await?;
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "dead-host detector found lease-expired candidates"
    );
    for host in candidates {
        let host_addr = host.host_addr.clone();
        if let Err(e) = evict_host(cfg, state, claimant, host.id, host_addr).await {
            tracing::warn!(host_id = %host.id, error = %e, "evict failed; another replica may have it");
        }
    }
    Ok(())
}

/// Settle HostLost rows whose inline second-stage transition never ran
/// or failed. This is deliberately a delayed backstop, not the normal
/// HostLost path.
///
/// **Convergence (oracle #8's shape):** every arm settles the ROW within
/// bounded cycles — a row younger than the 60s min-age waits for a later
/// cycle; everything else settles this one (ADR 0116 A4 retired the
/// serving-strike deferral: the row no longer waits on the VM). The VM
/// converges separately through its tombstone: a serving VM is destroyed
/// by its OWN host on heartbeat consumption (never by the coordinator);
/// a gone/not-alive one gets the inline belt destroy. No arm parks
/// forever — the #762/#769 eternal-wedge is not reintroduced.
pub async fn host_lost_straggler_sweep(
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    let sessions = meta.list_host_lost_sessions().await?;

    for session in sessions {
        let now = state.services.clock.now_utc();
        // Keep this sweep a backstop: flip_missing and evict_host normally
        // settle HostLost inline. Only rows stranded for more than a tick's
        // grace should be repaired here.
        if now - session.last_active_at <= chrono::Duration::seconds(60) {
            continue;
        }

        if let Some(sandbox_id) = session.sandbox_id {
            // ADR 0116 A-D5: the durable fact comes FIRST — a binding is
            // never cleared without a tombstone recorded, so the VM is
            // owned by an explicit coordinator fact from the moment the
            // row lets go of it. A write failure leaves the row bound
            // for a later cycle rather than orphaning the VM.
            if let Some(host_id) = session.host_id {
                if let Err(e) = meta
                    .record_sandbox_tombstone(host_id, sandbox_id, Some(session.id))
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        session_id = %session.id,
                        %sandbox_id,
                        "host-lost straggler tombstone write failed; leaving the row bound \
                         for a later cycle",
                    );
                    continue;
                }
            }

            state.host_registry.invalidate_sandbox(sandbox_id);

            // ADR 0098 Phase 3 / #777 "ask-the-host", ADR 0116 A-D5: a
            // host that reports the sandbox SERVING is host-affirmed
            // alive — the coordinator NEVER destroys it (the retired
            // strike cap used to, after 3 deferrals). Its tombstone
            // rides the next heartbeat response and the host destroys
            // its own VM; ack-by-absence then clears the row. Only a
            // probe that fails (host gone / unreachable) or an
            // explicitly not-alive process gets the inline belt destroy
            // — covering hosts that predate tombstone consumption.
            let serving = match session
                .host_id
                .and_then(|host_id| state.host_registry.backend_of(host_id))
            {
                Some(backend) => {
                    matches!(backend.probe_sandbox(sandbox_id).await, Ok(p) if p.process_alive)
                }
                None => false,
            };
            if serving {
                ::metrics::counter!(crate::metrics::HOST_LOST_ENTOMBED_SERVING_TOTAL).increment(1);
                tracing::info!(
                    session_id = %session.id,
                    %sandbox_id,
                    "host-lost straggler: sandbox still serving — its tombstone owns it now; \
                     the host destroys it on heartbeat consumption (ADR 0116 A-D5)",
                );
            } else if let Some(backend) = session
                .host_id
                .and_then(|host_id| state.host_registry.backend_of(host_id))
            {
                if let Err(e) = backend.destroy(sandbox_id, SessionFence::unfenced()).await {
                    tracing::warn!(
                        error = %e,
                        session_id = %session.id,
                        %sandbox_id,
                        "host-lost straggler belt destroy failed; the tombstone owns cleanup",
                    );
                }
            }

            if let Err(e) = meta
                .assign_session_sandbox_guarded(session.id, None, Some(Some(sandbox_id)), &[])
                .await
            {
                if !matches!(e, MetaError::Conflict(_)) {
                    tracing::warn!(
                        error = %e,
                        session_id = %session.id,
                        "host-lost straggler sandbox clear failed",
                    );
                }
                continue;
            }
        }

        if session.host_id.is_some() {
            if let Err(e) = meta.assign_session_host(session.id, None).await {
                tracing::warn!(
                    error = %e,
                    session_id = %session.id,
                    "host-lost straggler host clear failed",
                );
                continue;
            }
        }

        settle_host_lost(
            meta.as_ref(),
            &state.events,
            session.id,
            Some(session.live_disk_manifest.is_some()),
            now,
        )
        .await;
    }

    Ok(())
}

/// ADR 0116 A-D5: the heartbeat's tombstone leg — ack-by-absence, then
/// advertise what is still outstanding. Called by the HTTP heartbeat
/// handler and driven directly by the DST scheduler's heartbeat step
/// (which bypasses the handler), so sim and prod run the same code.
///
/// Acking requires `running_known`: an unenumerable running set
/// (`backend.list()` failed host-side) is "no information", not "no
/// sandboxes" — deleting tombstones against it would un-obligate
/// destroys on a single host-side blip (the issue-#215 asymmetry,
/// applied to tombstones). Advertising is unconditional and read-only.
/// Errors degrade to "advertise nothing this tick" — the next
/// heartbeat retries; a tombstone is durable precisely so delivery can
/// be lazy.
pub async fn process_sandbox_tombstones(
    meta: &Arc<dyn MetadataStore>,
    host_id: HostId,
    running: &[engram_core::SandboxId],
    running_known: bool,
) -> Vec<engram_core::SandboxId> {
    if running_known {
        match meta
            .ack_sandbox_tombstones_by_absence(host_id, running)
            .await
        {
            Ok(acked) if !acked.is_empty() => {
                tracing::info!(
                    host_id = %host_id,
                    count = acked.len(),
                    "sandbox tombstones acked by absence (host-affirmed destroyed)",
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(host_id = %host_id, error = %e,
                    "tombstone ack-by-absence failed; retrying next heartbeat");
            }
        }
    }
    match meta.sandbox_tombstones_for_host(host_id).await {
        Ok(tombstones) => tombstones,
        Err(e) => {
            tracing::debug!(host_id = %host_id, error = %e,
                "tombstone advertise read failed; advertising none this tick");
            Vec::new()
        }
    }
}

/// THE HostLost stage-2 settle (ADR 0116 A4 consolidation of the three
/// duplicate drivers: the dead-host bulk's per-session loop, the
/// straggler sweep's tail, and reconcile's flip_missing tail).
/// Resolves recoverability and moves the row `HostLost -> Idle | Dead`
/// via [`recovery_target`] — THE predicate.
///
/// Error posture, deliberately uniform: any read error leaves the row
/// at HostLost for a later sweep cycle (never route to `Dead` on a PG
/// blip — the pre-A4 reconcile copy did, a latent honest-Dead
/// violation); a transition `Conflict` is swallowed (a competing
/// replica settled first — the desired outcome); a settle increments
/// `HOST_LOST_STRAGGLERS_SETTLED_TOTAL` and logs the predicate inputs.
///
/// `known_live_manifest` skips the `get_session` round-trip when the
/// caller already holds the row.
pub(crate) async fn settle_host_lost(
    meta: &dyn MetadataStore,
    events: &SessionEventBus,
    session_id: SessionId,
    known_live_manifest: Option<bool>,
    now: DateTime<Utc>,
) {
    let snapshot = match meta.latest_snapshot_for_session(session_id).await {
        Ok(opt) => opt,
        Err(e) => {
            tracing::warn!(error = %e, %session_id,
                "settle_host_lost: snapshot lookup failed; leaving session at HostLost");
            return;
        }
    };
    let has_live_manifest = match known_live_manifest {
        Some(known) => known,
        None => match meta.get_session(session_id).await {
            Ok(s) => s.live_disk_manifest.is_some(),
            Err(e) => {
                tracing::warn!(error = %e, %session_id,
                    "settle_host_lost: get_session failed; leaving session at HostLost");
                return;
            }
        },
    };
    let has_recoverable_snapshot = snapshot.as_ref().is_some_and(|s| s.recoverable);
    let target = recovery_target(has_recoverable_snapshot, has_live_manifest);
    note_unrecoverable_if_dead(target, snapshot.as_ref(), session_id);
    match meta
        .transition_session(session_id, target, BindingDisposition::RequireUnbound)
        .await
    {
        Ok(prev) => {
            emit_status_changed(meta, events, session_id, prev, target, now).await;
            ::metrics::counter!(crate::metrics::HOST_LOST_STRAGGLERS_SETTLED_TOTAL).increment(1);
            tracing::info!(
                %session_id,
                ?target,
                has_recoverable_snapshot,
                has_live_manifest,
                "HostLost settled",
            );
        }
        Err(MetaError::Conflict(_)) => {}
        Err(e) => tracing::warn!(error = %e, %session_id, ?target,
            "settle_host_lost: second-stage transition failed; leaving at HostLost"),
    }
}

/// Defense-in-depth liveness probe for the dead-host detector (issue
/// #231). The detector keys on `last_heartbeat_at`, which the heartbeat
/// handler advances best-effort-no-more. But the failure mode that
/// orphans a *healthy* host is cross-pod: pod A's pool saturates and
/// stops persisting host H's `last_heartbeat_at` (H now also gets 5xx
/// and backs off, but the row is already stale), while pod B's detector
/// — healthy PG — sees the stale row and is about to mark H dead and
/// orphan its sessions. Before doing that, B dials H directly: if H
/// answers a `Ping`, the row is stale but the host is alive, so we skip
/// the eviction and warn. Converts silent data loss into an alert.
///
/// Returns `true` only when the host *answered* the probe. An
/// unreachable host (the genuine dead-host case), or no client to
/// probe with, returns `false` so eviction proceeds as before.
async fn host_responds(client: &Arc<dyn engram_core::traits::HostClient>) -> bool {
    client.ping().await.is_ok()
}

/// THE HostLost stage-2 routing predicate (ADR 0045 Phase A; unified in
/// issue #777, ADR 0098 Phase 3 "honest-Dead"). A session is recoverable
/// — routed to `Idle` for lazy `/resume` on next access — iff it has a
/// **recoverable** memory snapshot OR a live disk manifest (the latter
/// still resumes via the cold-boot path). With nothing recoverable it
/// goes to `Dead` — never an `Idle` that lies about resumability.
///
/// `has_recoverable_snapshot` is the honest predicate: it is the latest
/// snapshot's `recoverable` flag (the BlobStorage HEAD result at
/// snapshot-take time), NOT the mere presence of a snapshot row. Before
/// #777 the two dead-host sites keyed on `snapshot.is_some()`, disagreeing
/// with `reconcile::flip_missing`, which already keyed on `recoverable`;
/// the filtered predicate is now the one true stage-2 decision, shared by
/// every site.
///
/// The detector no longer routes into `Evacuating`; proactive relocation
/// is operator drain only (ADR 0044 K3).
pub(crate) fn recovery_target(
    has_recoverable_snapshot: bool,
    has_live_manifest: bool,
) -> SessionState {
    if has_recoverable_snapshot || has_live_manifest {
        SessionState::Idle
    } else {
        SessionState::Dead
    }
}

/// The "snapshot rows exist but none is recoverable" signal (issue #777,
/// ADR 0098 Phase 3 honest-Dead). When stage 2 routes a session to `Dead`
/// while a snapshot row DID exist, the snapshot was un-recoverable (its
/// BlobStorage HEAD failed at take-time) and there was no live disk
/// manifest either. Emit a distinct warn + counter so a bad-capture
/// pipeline stays visible instead of hiding behind a generic Dead — a
/// no-op when the target is `Idle` (recoverable) or when there was no
/// snapshot at all (a genuinely never-checkpointed session).
pub(crate) fn note_unrecoverable_if_dead(
    target: SessionState,
    latest_snapshot: Option<&engram_core::types::snapshot::SnapshotRecord>,
    session_id: SessionId,
) {
    if target == SessionState::Dead && latest_snapshot.is_some() {
        ::metrics::counter!(crate::metrics::HOST_LOST_UNRECOVERABLE_SNAPSHOT_TOTAL).increment(1);
        tracing::warn!(
            session_id = %session_id,
            "HostLost stage 2: snapshot row(s) exist but none is recoverable and no live disk \
             manifest — routing to Dead (bad-capture signal, issue #777). Check snapshot \
             durability (BlobStorage HEAD at capture time)",
        );
    }
}

async fn evict_host(
    cfg: &DeadHostConfig,
    state: &SharedState,
    claimant: &str,
    host_id: HostId,
    host_addr: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    if !meta
        .try_acquire_dead_host_lease(host_id, claimant, cfg.lease_stale_after)
        .await?
    {
        // Another coordinator replica holds a live lease on this host.
        // It will do the eviction; we just skip.
        tracing::debug!(host_id = %host_id, "eviction lease contested; skipping");
        return Ok(());
    }
    // Run the eviction with the lease held, releasing on EVERY outcome
    // (including errors — the lease is not a lock; a leaked row would
    // only delay a retry by `lease_stale_after`, but there is no reason
    // to pay that on a clean error path).
    let result = evict_host_locked(state, host_id, host_addr).await;
    if let Err(e) = meta.release_dead_host_lease(host_id, claimant).await {
        tracing::warn!(
            host_id = %host_id,
            error = %e,
            "dead-host lease release failed; stale takeover will reap it",
        );
    }
    result
}

async fn evict_host_locked(
    state: &SharedState,
    host_id: HostId,
    host_addr: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    let host_registry = &state.host_registry;
    let events = &state.events;

    // Re-check the host's status *after* taking the lease — another
    // replica that already won may have flipped it to Dead in the
    // window between our `list_lease_expired_hosts` and now.
    let status = meta.host_status(host_id).await?;
    if matches!(
        status,
        Some(engram_core::types::host::HostStatus::Dead) | None
    ) {
        tracing::debug!(host_id = %host_id, "host already dead; skipping");
        return Ok(());
    }

    // Defense-in-depth (issue #231): the lease says expired, but is the
    // host actually gone? Dial it directly with a cheap `Ping` before we
    // orphan its sessions. The asymmetric-PG-failure mode — one coord
    // pod's pool saturates and stops renewing H's lease on heartbeat
    // while THIS pod's detector is healthy — lapsed a *live* host's
    // lease; marking it dead here would orphan every session on a host
    // that's up and loaded. If the host answers, RENEW the lease
    // durably (`renew_host_lease`, ADR 0116 A-D4) and skip: the written
    // reprieve is honored by every replica's detector, which is what
    // retired the per-replica strike/grace memory — and the rk28 shape
    // (one failed Ping seconds after a rescue) cannot kill a host whose
    // rescue bought it a full TTL.
    //
    // Probe via the client THIS pod already holds for the host — the same
    // `host_registry` seam reconcile and the straggler sweep probe through
    // (`backend_of`), not a second, lower-level channel cache. (Before ADR
    // 0098 Phase 3, this path went straight to `host_pool.get`, a seam the
    // DST harness leaves unpopulated, so the probe silently never ran
    // in-sim — issue #787.) Only when the registry has nothing for H (the
    // cross-pod case: this pod never saw H register) do we fall back to
    // warming a fresh dial from the persisted `host_addr`. A dial that
    // fails to warm, or no addr at all, is unreachability evidence — the
    // lease already expired, so eviction proceeds.
    let rescued: bool = if let Some(client) = state.host_registry.backend_of(host_id) {
        host_responds(&client).await
    } else {
        match state.services.host_pool.get(host_id) {
            Ok(c) => {
                let client: Arc<dyn engram_core::traits::HostClient> = Arc::new(c);
                host_responds(&client).await
            }
            Err(_) => match host_addr {
                Some(addr) => match state.services.host_pool.get_or_warm(host_id, addr).await {
                    Ok(c) => {
                        let client: Arc<dyn engram_core::traits::HostClient> = Arc::new(c);
                        host_responds(&client).await
                    }
                    Err(e) => {
                        tracing::debug!(host_id = %host_id, error = %e, "dead-host probe: could not warm a dial; unreachable");
                        false
                    }
                },
                None => false,
            },
        }
    };
    if rescued {
        // ADR 0068 lineage (added in `7fcc4c3c`); made durable in ADR
        // 0116 A-D4. Graphed alongside the reconcile probe's rescue
        // counter (`RECONCILE_PROBE_RESCUES_TOTAL`).
        ::metrics::counter!(crate::metrics::DEAD_HOST_PROBE_RESCUES_TOTAL).increment(1);
        let until = state.services.clock.now_utc() + crate::config::host_lease_ttl();
        if let Err(e) = meta.renew_host_lease(host_id, until).await {
            tracing::warn!(host_id = %host_id, error = %e,
                "probe-rescue lease renewal failed; host stays a candidate next tick");
        }
        tracing::warn!(
            host_id = %host_id,
            "expired lease but live host — host answered Ping; renewed its lease and SKIPPING \
             eviction. Check heartbeat persistence (coord PG pool saturation?) — see \
             engram_heartbeat_persist_failures_total (issue #231)",
        );
        return Ok(());
    }

    let affected = match meta.mark_host_dead_if_lease_expired(host_id).await {
        Ok(affected) => affected,
        Err(MetaError::Conflict(detail)) => {
            // A renewal raced in between our list read and the mark (a
            // late heartbeat landed, or another replica's probe rescued
            // the host). The host is alive; the abort IS the invariant
            // holding — never revoke a binding whose lease is live.
            ::metrics::counter!(crate::metrics::DEAD_HOST_MARK_ABORTED_LEASE_RENEWED_TOTAL)
                .increment(1);
            tracing::info!(host_id = %host_id, %detail,
                "mark-dead aborted: lease renewed mid-flight; host came back");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // Notify other replicas so they drop their HostRegistry entry.
    meta.notify_host_dead(host_id).await?;

    // Stage 1: emit StatusChanged{prev -> HostLost} for every session
    // the bulk touched. The `prev` came back from the UPDATE so the
    // `from` is honest (not a hand-encoded `Active` that would lie if
    // the session had been Idle).
    for (session_id, prev) in &affected {
        emit_status_changed(
            meta.as_ref(),
            events,
            *session_id,
            *prev,
            SessionState::HostLost,
            state.services.clock.now_utc(),
        )
        .await;
    }

    // Stage 2: per-session recoverability-aware second transition.
    //
    // ADR 0045 Phase A: route a recoverable session to `Idle` for
    // lazy `/resume` on next access; `Dead` when there's nothing to
    // recover. The detector no longer routes into `Evacuating` — the
    // reactive auto-evac is retired (it was the source of the
    // resume-from-idle wedge + deploy-storm cascade). Proactive
    // relocation now happens only via operator drain (ADR 0044 K3).
    //
    // Decision matrix (issue #777 honest-Dead: the snapshot column is the
    // `recoverable` FLAG, not mere row presence — an un-recoverable
    // snapshot is NOT resumable and must not route to a lying Idle):
    //
    // | recoverable snap | live_manifest | next state | who recovers it          |
    // |------------------|---------------|------------|--------------------------|
    // | true             | _             | Idle       | user/exec /resume        |
    // | false/none       | Some          | Idle       | /resume (disk-only cold) |
    // | false/none       | None          | Dead       | (no recoverable state)   |
    //
    // Failures of any query/transition are logged and skipped; the
    // row stays at HostLost and a future reconcile pass (or
    // operator action) can move it on. (ADR 0116 A4: the shared
    // `settle_host_lost` drives this — one settle, three callers.)
    for (session_id, _) in &affected {
        settle_host_lost(
            meta.as_ref(),
            events,
            *session_id,
            None,
            state.services.clock.now_utc(),
        )
        .await;
    }

    host_registry.unregister(host_id);
    tracing::info!(
        host_id = %host_id,
        sessions_orphaned = affected.len(),
        "host marked dead; sessions moved through HostLost to Idle/Dead",
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The detector's polling loop and lease dance need a
    // MetadataStore with real lease semantics to test meaningfully
    // (live Postgres, or engram-sim's SimMetadataStore). The trait-layer logic
    // (`mark_host_dead_if_lease_expired` semantics) is covered
    // by Mock-based tests in `tests/dead_host_mock.rs`. End-to-end
    // multi-replica behaviour is the live-Postgres test
    // (`#[ignore]`'d, gated behind dev-VM Docker compose).

    // ADR 0045 Phase A + issue #777 honest-Dead: the stage-2 routing
    // decision. A RECOVERABLE dead-host session goes to Idle (lazy
    // /resume), never Evacuating (the reactive auto-evac is retired);
    // only the no-recoverable-state case is terminal. The first arg is
    // the snapshot's `recoverable` FLAG, not mere row presence — an
    // un-recoverable snapshot alone is NOT resumable.
    #[test]
    fn recovery_target_routes_recoverable_to_idle_never_evacuating() {
        // recoverable snapshot → Idle (memory + disk resume).
        assert_eq!(recovery_target(true, false), SessionState::Idle);
        // disk-only (live manifest, no recoverable snapshot) → Idle (cold-boot resume).
        assert_eq!(recovery_target(false, true), SessionState::Idle);
        // both present → Idle.
        assert_eq!(recovery_target(true, true), SessionState::Idle);
        // nothing recoverable (no recoverable snapshot, no manifest) → Dead.
        assert_eq!(recovery_target(false, false), SessionState::Dead);

        // The reactive auto-evac target is gone: no input combination
        // routes a dead host's session into Evacuating.
        for (snap, manifest) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_ne!(recovery_target(snap, manifest), SessionState::Evacuating);
        }
    }

    // Issue #231: defense-in-depth probe. The detector keys on the
    // stale `last_heartbeat_at` row, but before orphaning a host's
    // sessions it dials the host directly. A host that ANSWERS the
    // probe is alive (the row went stale because some coord pod's PG
    // pool saturated and stopped advancing it) → eviction must be
    // skipped. A host that does NOT answer is the genuine dead-host
    // case → eviction proceeds. `host_responds` is the seam that
    // decides which; these tests pin both arms.
    use engram_core::traits::HostClient;
    use engram_core::types::egress::SessionEgressPolicy;
    use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
    use engram_core::types::snapshot::SnapshotMetadata;
    use engram_core::{SandboxError, SandboxId, SessionId};

    /// Minimal `HostClient` whose `list()` (and thus the default
    /// `ping()`) returns alive/unreachable on demand. Every other
    /// method is unreachable in this test — the probe only calls
    /// `ping()`.
    struct ProbeHost {
        alive: bool,
    }

    #[async_trait::async_trait]
    impl HostClient for ProbeHost {
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            if self.alive {
                Ok(Vec::new())
            } else {
                Err(SandboxError::Unavailable("host down".into()))
            }
        }
        async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            unimplemented!()
        }
        async fn destroy(
            &self,
            _id: SandboxId,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn probe_sandbox(
            &self,
            _id: SandboxId,
        ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
            unimplemented!()
        }
        async fn exec_stream(
            &self,
            _id: SandboxId,
            _cmd: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            unimplemented!()
        }
        async fn snapshot(
            &self,
            _id: SandboxId,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<SnapshotMetadata, SandboxError> {
            unimplemented!()
        }
        async fn restore(
            &self,
            _metadata: SnapshotMetadata,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<SandboxId, SandboxError> {
            unimplemented!()
        }
        async fn start_agent(
            &self,
            _id: SandboxId,
            _agent: AgentSpec,
            _policy: SessionEgressPolicy,
            _fence: engram_core::traits::SessionFence,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
        async fn guest_ip(&self, _id: SandboxId) -> Option<std::net::Ipv4Addr> {
            unimplemented!()
        }
        async fn bind_session(
            &self,
            _session_id: SessionId,
            _sandbox_id: SandboxId,
            _binding_epoch: u64,
        ) {
            unimplemented!()
        }
        async fn unbind_session(&self, _session_id: SessionId) {
            unimplemented!()
        }
        async fn send_prompt(
            &self,
            _sandbox_id: SandboxId,
            _prompt_id: String,
            _text: String,
            _mode: Option<String>,
        ) -> Result<(), SandboxError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn host_responds_skips_eviction_for_a_live_but_stale_host() {
        // The host answers the Ping → it's alive; the stale row is a
        // heartbeat-persistence artifact, NOT a dead host. evict_host
        // uses this to skip the orphaning.
        let live: Arc<dyn HostClient> = Arc::new(ProbeHost { alive: true });
        assert!(
            host_responds(&live).await,
            "a host that answers the probe must be treated as live (skip eviction)",
        );
    }

    #[tokio::test]
    async fn host_does_not_respond_when_unreachable() {
        // The genuine dead-host case: the probe fails → eviction
        // proceeds as before.
        let dead: Arc<dyn HostClient> = Arc::new(ProbeHost { alive: false });
        assert!(
            !host_responds(&dead).await,
            "an unreachable host must not be treated as live (eviction proceeds)",
        );
    }

    // The rk28 property (2026-07-09: a single failed Ping seconds after
    // a rescue orphaned two live sessions) is now held by written state
    // instead of an in-memory verdict: a rescue durably renews the
    // lease (`renew_host_lease`, conformance `t_host_binding_lease`),
    // and the mark re-checks the lease under the row lock and aborts
    // with `Conflict` on a renewal (`mark_host_dead_if_lease_expired`
    // conformance legs). The DST pin `issue_787_dead_host_false_evict_
    // double_boot` replays the false-evict shape end to end.
}
