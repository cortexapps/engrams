//! Idle-session eviction: the snapshot+destroy+mark-Idle *pipeline*
//! plus (ADR 0034) the *eviction scanner* that drives it.
//!
//! ADR 0013 + ADR 0011 follow-up #2 retired the polling driver that
//! lived here. In a stateless coord, no single pod's local
//! `HarnessHub` is authoritative for "is this sandbox idle?" — the
//! host owns that view. The host scans its local hub on a tick and
//! POSTs candidates to `/api/hosts/:id/idle-eviction-candidates`.
//!
//! ADR 0034 split nomination from execution. The receiving handler
//! only flips `Active → Evicting` (the pre-0034 inline pipeline died
//! by cancellation whenever an eviction outlived the host's POST
//! timeout — prod session 0782bea5). [`spawn_eviction_scanner`]
//! sweeps `status='evicting'` on a 10s tick and runs
//! [`evict_idle_session`] to completion outside any request
//! lifetime; its first tick after coord startup is also what
//! recovers rows wedged across a deploy. The pipeline is idempotent
//! (registry guard at the top short-circuits if another pod already
//! evicted the sandbox), so re-entry is safe.
//!
//! Auto-resume on next request is wired separately (`api/sessions.rs`
//! exec/exec_stream/SSE handlers): if status is `Idle`, call the
//! existing `resume` path before routing.

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::SandboxBackend;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::SessionState;
use engram_core::{SandboxId, SessionId};

use crate::state::{SessionEvent, SharedState};

/// Run the suspend pipeline for one sandbox. Pure function over
/// `SharedState`; the loop above is just the driver. Multi-host
/// production refactors the driver onto each host-agent and keeps
/// this function as the canonical pipeline.
///
/// Backwards-compatible wrapper: transitions the session to `Idle`,
/// matching the historical idle-eviction shape. Operator-driven
/// drains (ADR 0018 commit 12) use [`evict_session_to_state`]
/// directly with `target_state = Evacuating`.
pub async fn evict_idle_session(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
) -> Result<(), EvictError> {
    evict_session_to_state(state, session_id, sandbox_id, SessionState::Idle).await
}

/// ADR 0018 commit 12: parameterized suspend pipeline. Same pause →
/// flush → memory-snapshot → destroy sequence as the legacy
/// `evict_idle_session`, but the terminal state-machine transition
/// is configurable. `Idle` is the user-paused (manual /resume)
/// shape; `Evacuating` is the operator-drain / host-loss shape
/// where the coord-side `evac_resumer` background task will pick
/// the session up and drive it through `Evacuating → Created →
/// Active` on a peer host.
///
/// The non-target-state code paths (snapshot capture, registry
/// unbind, sandbox destroy, event emission, snapshot commit) are
/// IDENTICAL between the two — only the `transition_session`
/// target and the resulting `StatusChanged` event payload differ.
/// Sharing the pipeline guarantees both flows have the same
/// recoverability invariants: snapshot is durable in BlobStorage
/// before destroy, sandbox is force-flushed and paused before the
/// memory dump, and the PG transition commits before the
/// (best-effort) destroy so reconcile can't race the orphan path.
pub async fn evict_session_to_state(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
    target_state: SessionState,
) -> Result<(), EvictError> {
    // The pipeline only knows about Idle and Evacuating as legal
    // targets. Both share the "Active → captured-snapshot → suspended"
    // semantic; any other target would skip half the steps and break
    // the recovery invariants. Reject early with a clean error.
    if !matches!(target_state, SessionState::Idle | SessionState::Evacuating) {
        return Err(EvictError::Meta(format!(
            "evict_session_to_state: target {target_state:?} not supported \
             (only Idle and Evacuating)",
        )));
    }
    // ADR 0016 §A.1.5c: cross-replica re-entry guard backed by the
    // `session_lease` PG table. Replaces A.1.5b's per-pod
    // DashMap. If two callers (different coord pods, operator
    // drain, future evacuation primitive) race into
    // `evict_idle_session` for the same session, the second one's
    // INSERT ... ON CONFLICT DO NOTHING returns 0 rows; we early-
    // return Ok(()) with an info log. The first owns the pipeline.
    // The RAII guard's Drop spawns a best-effort DELETE so the
    // lease releases on every exit path (success, error, panic).
    let _guard = match SessionLeaseGuard::try_acquire(state, session_id, Some(sandbox_id)).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            tracing::info!(
                session_id = %session_id,
                sandbox_id = %sandbox_id,
                "idle eviction skipped: pipeline already in flight (session_lease row held)",
            );
            return Ok(());
        }
        Err(e) => {
            // Lease store unavailable. Honest failure mode: bail out
            // and let the host's tick retry, rather than running an
            // unguarded pipeline that might race a peer pod. The
            // host-side gate (§A.1.5a) suppresses the storm.
            return Err(EvictError::Meta(format!(
                "session lease acquire failed: {e}"
            )));
        }
    };

    // ADR 0044 K5: race-free re-entry guard on the authoritative PG state.
    // The lease above is held until after the PG transition, so once we hold
    // it any concurrent eviction (idle-evict, admin drain, dead-host recovery)
    // has *fully* completed — re-read the state and skip if the session is no
    // longer evictable. `Active` (direct idle-evict / drain) and `Evicting`
    // (backstop-nominated, scanner finishing the job) are the legal inputs;
    // anything else (already `Idle`, mid-`Evacuating`, terminal) means a peer
    // already handled it. The registry guard below catches the stale in-memory
    // binding; this catches the case that wedged a session when an idle-evict
    // completed exactly as a drain dispatched it — already `Idle`, but the
    // drain still drove it to `Evacuating` on a destroyed sandbox.
    match state.services.meta.get_session(session_id).await {
        Ok(s) if !matches!(s.status, SessionState::Active | SessionState::Evicting) => {
            tracing::info!(
                session_id = %session_id,
                state = s.status.as_str(),
                "evict skipped: session no longer evictable (a concurrent eviction won the lease first)",
            );
            return Ok(());
        }
        Ok(_) => {}
        Err(e) => {
            return Err(EvictError::Meta(format!(
                "evict: re-read session state after lease: {e}"
            )));
        }
    }

    // ADR 0016 A.1.1: entry log. Was silent before — a coord pod
    // running the pipeline repeatedly (e.g. retry storm, post-roll
    // race) showed up only as host-side `chunked NBD disk flushed`
    // lines with no coord-side counterpart, making the snapshot
    // source impossible to attribute. Pair this with the
    // "completed"/"skipping" logs below and the per-step warn arms
    // so the full pipeline is auditable end-to-end.
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        "idle eviction pipeline started",
    );

    // Guard: if the registry doesn't think this sandbox is bound to
    // the session anymore, the session was already evicted by some
    // other path (operator, dead-host detector). No-op cleanly.
    if state.registry.get(session_id) != Some(sandbox_id) {
        tracing::info!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            "idle eviction skipped: sandbox no longer bound",
        );
        return Ok(());
    }

    // Step 1: take a snapshot. ADR 0007 Phase 6: backend owns its
    // staging dir; coord no longer pre-allocates one. Durability
    // flows through the chunked manifests on `SnapshotMetadata`.
    //
    // ADR 0014 issue #1/#2: the host tracks this as an in-flight
    // snapshot. Every exit path after this point MUST end with either
    // `commit_snapshot` (full pipeline succeeded) or `abort_snapshot`
    // (anything else). Without this, a coord-side flake leaves the
    // 4 GiB local snapshot dir + per-snapshot blob keys orphaned —
    // host-side `idle_evictor` re-POSTs the candidate on the next tick,
    // a fresh SnapshotId is minted, and we leak ~4 GiB per retry. That
    // was the failure on `engrams-fc-xngk` (25 dirs × 4 GiB in 13 min).
    let metadata = state
        .services
        .host
        .snapshot(sandbox_id)
        .await
        .map_err(EvictError::Sandbox)?;

    let host_id = state.host_registry.host_of(sandbox_id);
    let now = Utc::now();
    // ADR 0028 A.log: the event-log leg of the coherence triple. The
    // guest paused (then gets destroyed) during the capture, so "the
    // newest event as of now" is the cursor at the pause instant up
    // to a sub-second skew. Best-effort: a lookup failure degrades to
    // NULL ("no rewind information"), never fails the eviction.
    let events_cursor = state
        .services
        .meta
        .latest_event_idx_at_or_before(session_id, now)
        .await
        .unwrap_or_default();
    let record = SnapshotRecord {
        id: metadata.id,
        session_id: Some(session_id),
        host_id,
        image_version: metadata.image_version.clone(),
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
        // ADR 0007: chunked manifests are the durability primitive.
        disk_manifest: metadata.disk_manifest,
        memory_manifest: metadata.memory_manifest,
        // ADR 0009 Phase 2: HEAD-verify the chunked manifests so
        // reconcile flips this session to Idle (not Dead) on a
        // future sandbox-loss event.
        recoverable: crate::api::snapshot::verify_snapshot_recoverable(
            state.services.blob.as_ref(),
            metadata.disk_manifest.as_ref(),
            metadata.memory_manifest.as_ref(),
        )
        .await,
        // ADR 0035: pin the generations this snapshot references.
        aux_bundles: metadata.aux_bundles.clone(),
        events_cursor,
    };
    if let Err(e) = state.services.meta.record_snapshot(record.clone()).await {
        abort_inflight_snapshot(state, session_id, sandbox_id, "record_snapshot").await;
        return Err(EvictError::Meta(e.to_string()));
    }

    // ADR 0034 durability: commit the host's in-flight snapshot NOW —
    // while the sandbox is still bound and its owner resolvable, and
    // BEFORE `unbind`/`destroy` below or the racing periodic-checkpoint
    // driver can `abort_prior_inflight_snapshot` the artifacts out from
    // under the `recoverable = true` row we just wrote. The old
    // placement (after destroy()) could NEVER succeed: destroy() plus
    // the `sandbox_id = NULL` transition below make `resolve_owner`
    // return NotFound, so commit no-op'd, the host kept the snapshot
    // "in-flight", and the next checkpoint tick deleted
    // state.bin/sidecar from BlobStorage while PG still advertised the
    // snapshot as recoverable — bricking the resume. Prod incident
    // 89f7984d (2026-06-04). Best-effort: a genuine host RPC failure
    // here is rare and backstopped by resume-time blob verification
    // (see `api::snapshot::resume_from_idle`).
    if let Err(e) = state.services.host.commit_snapshot(sandbox_id).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "idle eviction: commit_snapshot failed; host in-flight tracking not \
             cleared (resume-time verification will catch a deleted snapshot)",
        );
    }

    // ADR 0016 §A.1.6: PG-side transitions happen BEFORE
    // `destroy()`. The reconciler (ADR 0009) runs on every host
    // heartbeat and treats `session.sandbox_id IS NOT NULL` +
    // `host.running_sandboxes does not contain sandbox_id` as an
    // orphan to be recovered via HostLost→Idle. If destroy() ran
    // before transition_session, a heartbeat landing in the window
    // between those two steps would race ahead and flip the
    // session to Idle via the recovery path, leaving this
    // pipeline's later `transition_session(Idle→Idle)` to fail
    // (state-machine rejects same-state), `abort_snapshot` to
    // fire spuriously, and the matched `pipeline completed` log
    // never to appear. Validated on session 1edf09a3 (2026-05-24).
    //
    // Reordering to PG-first means: by the time the host's next
    // heartbeat reports `running_sandboxes` missing this sandbox,
    // `session.status` is already Idle and reconcile's
    // active-only guard no-ops. The destroy() call's host-side
    // bookkeeping (proxy unregister, jail teardown) still runs;
    // a failed destroy() is best-effort the same as before.

    // Step 3a: registry.unbind() — purely in-memory, no coord-
    // visible state change. Safe to run before PG transitions.
    state.registry.unbind(session_id);

    // Step 3b (PG, Idle-before-destroy): clear sandbox_id on the
    // session row.
    if let Err(e) = state
        .services
        .meta
        .assign_session_sandbox(session_id, None)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "idle eviction: assign_session_sandbox(None) failed",
        );
    }
    // Step 3c (PG, Idle-before-destroy): flip to Idle. Once this
    // commits, the reconciler will no-op on every subsequent
    // heartbeat for this session because the reconcile pass keys
    // on Active status only.
    let prev = match state
        .services
        .meta
        .transition_session(session_id, target_state)
        .await
    {
        Ok(prev) => prev,
        Err(e) => {
            abort_inflight_snapshot(state, session_id, sandbox_id, "transition_session").await;
            return Err(EvictError::Meta(e.to_string()));
        }
    };

    // Step 4 (host destroy, post-PG): now the session is Idle,
    // destroy the sandbox. Best-effort — failures don't bubble
    // because the PG state is already correct; the host's
    // orphan_reap background task cleans up a stuck sandbox.
    if let Err(e) = state.services.host.destroy(sandbox_id).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "idle eviction: destroy failed after Idle transition; orphan_reap will clean up",
        );
    }
    // ADR 0006: host-agent unregisters its local proxy entry as
    // part of `destroy`. No coordinator-side cleanup needed.

    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::SnapshotTaken {
                snapshot_id: metadata.id,
                size_bytes: metadata.size_bytes,
                at: now,
            },
        )
        .await
    {
        abort_inflight_snapshot(state, session_id, sandbox_id, "emit SnapshotTaken").await;
        return Err(EvictError::Emit(e.to_string()));
    }
    if let Err(e) = state
        .emit(session_id, SessionEvent::Evicted { at: now })
        .await
    {
        abort_inflight_snapshot(state, session_id, sandbox_id, "emit Evicted").await;
        return Err(EvictError::Emit(e.to_string()));
    }
    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: prev,
                to: target_state,
                at: now,
            },
        )
        .await
    {
        abort_inflight_snapshot(state, session_id, sandbox_id, "emit StatusChanged").await;
        return Err(EvictError::Emit(e.to_string()));
    }

    // ADR 0016 A.1.1: success log. Pairs with the entry log so a
    // pipeline that flushes (host log) without committing (no PG
    // row) shows up as an unmatched start/end pair in a grep.
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        snapshot_id = %metadata.id,
        "idle eviction pipeline completed",
    );

    Ok(())
}

/// ADR 0016 §A.1.5c: RAII guard around the `session_lease` PG
/// row. `try_acquire` does INSERT ... ON CONFLICT DO NOTHING; on
/// 1 row affected returns `Ok(Some(Self))`, on 0 rows affected
/// returns `Ok(None)` (lease held by another caller). Drop spawns
/// a best-effort DELETE so the lease releases on every exit path
/// (success, error, panic). A stale row that survives a panic /
/// pod crash is reaped by `spawn_session_lease_reaper` at 180s.
///
/// Held by both the idle-eviction pipeline (`evict_session_to_state`)
/// and the resume path (`api::snapshot::resume_session`) so the two
/// can never drive one session concurrently — without it, a resume
/// racing an eviction (or two resumes) builds a live VM before the
/// serializing state transition and orphans a sandbox. `sandbox_id`
/// is diagnostic: `Some` for an eviction, `None` for a resume (no
/// sandbox exists yet).
pub(crate) struct SessionLeaseGuard {
    meta: Arc<dyn engram_core::traits::MetadataStore>,
    session_id: SessionId,
}

impl SessionLeaseGuard {
    pub(crate) async fn try_acquire(
        state: &SharedState,
        session_id: SessionId,
        sandbox_id: Option<SandboxId>,
    ) -> Result<Option<Self>, engram_core::MetaError> {
        let acquired = state
            .services
            .meta
            .try_acquire_session_lease(session_id, sandbox_id, state.pod_id.as_str())
            .await?;
        if acquired {
            Ok(Some(Self {
                meta: state.services.meta.clone(),
                session_id,
            }))
        } else {
            Ok(None)
        }
    }
}

impl Drop for SessionLeaseGuard {
    fn drop(&mut self) {
        // Drop is sync; release runs as a detached tokio task. Best-
        // effort: if the runtime is shutting down or the DELETE fails,
        // the stale-lease reaper (`spawn_session_lease_reaper`)
        // cleans it up at the next 180s tick.
        let meta = self.meta.clone();
        let session_id = self.session_id;
        tokio::spawn(async move {
            if let Err(e) = meta.release_session_lease(session_id).await {
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "session lease release failed; stale-lease reaper will retry",
                );
            }
        });
    }
}

/// ADR 0016 §A.1.5c stale-lease reaper. Background task spawned at
/// coord startup. Every 30s, deletes `session_lease` rows older
/// than `max_age` (default 180s — 6× the prior host-side timeout,
/// matches the §A.1.5a sweep threshold). One `tracing::warn!` per
/// reaped row carries `(session_id, sandbox_id, locked_by,
/// locked_at)` for operator postmortems.
///
/// Returns the spawned `JoinHandle` so the caller can hold it for
/// the process lifetime; dropping the handle aborts the loop.
pub fn spawn_session_lease_reaper(
    meta: Arc<dyn engram_core::traits::MetadataStore>,
    max_age: std::time::Duration,
    poll_interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(poll_interval);
        // Skip the first immediate tick — gives PG a moment to
        // settle after coord startup before we start sweeping.
        tick.tick().await;
        loop {
            tick.tick().await;
            match meta.sweep_stale_session_leases(max_age).await {
                Ok(reaped) => {
                    for lease in reaped {
                        tracing::warn!(
                            session_id = %lease.session_id,
                            sandbox_id = ?lease.sandbox_id,
                            locked_by = %lease.locked_by,
                            locked_at = %lease.locked_at,
                            max_age_secs = max_age.as_secs(),
                            "ADR 0016 §A.1.5c: reaped stale session lease",
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "session lease reaper sweep failed; will retry next tick",
                    );
                }
            }
        }
    })
}

/// ADR 0014 issue #1/#2: best-effort `abort_snapshot` after a
/// downstream pipeline failure in [`evict_idle_session`]. Logs but
/// never propagates — the caller's pipeline error is what surfaces.
/// Hosts implement abort idempotently so spurious double-calls are
/// safe.
async fn abort_inflight_snapshot(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
    scope: &'static str,
) {
    if let Err(e) = state.services.host.abort_snapshot(sandbox_id).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            scope,
            error = %e,
            "idle eviction: abort_snapshot failed after pipeline failure \
             (orphan local dir + blobs may persist until next retry)",
        );
    }
}

#[derive(Debug)]
pub enum EvictError {
    Io(String),
    Sandbox(engram_core::SandboxError),
    Meta(String),
    Emit(String),
}

impl std::fmt::Display for EvictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "idle evict io: {m}"),
            Self::Sandbox(e) => write!(f, "idle evict sandbox: {e}"),
            Self::Meta(m) => write!(f, "idle evict meta: {m}"),
            Self::Emit(m) => write!(f, "idle evict event emit: {m}"),
        }
    }
}

impl std::error::Error for EvictError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sandbox(e) => Some(e),
            _ => None,
        }
    }
}

// The TTL env helpers + the soft/hard-TTL detection driver live on
// the host-agent (`engram_host_agent::idle_evictor`). The coord owns
// the `evict_idle_session` pipeline above and (ADR 0034) the
// eviction scanner below that drives it for `Evicting` rows.

// ─── ADR 0034: eviction scanner ──────────────────────────────────
//
// Same shape as `evac_resumer`: spawn loop → per-tick list-by-status
// → per-session attempt-bump + budget → primitive. The nomination
// side (handler / detection backstop) only flips Active → Evicting;
// everything heavy happens here, detached from any request lifetime.

#[derive(Clone, Debug)]
pub struct EvictionScannerConfig {
    /// How often to sweep for Evicting sessions. 10s matches
    /// `EvacResumerConfig::poll_interval` — same operational cadence
    /// for all session-lifecycle scanners.
    pub poll_interval: std::time::Duration,
    /// Retry budget per session before falling back to `HostLost`.
    /// At the 10s cadence, 20 attempts is ~3 minutes of continuous
    /// pipeline failure (blob-store flake, host gRPC errors). The
    /// fallback is HostLost — NOT Active (would re-nominate forever),
    /// NOT Idle (lies: no durable snapshot exists and the sandbox is
    /// still running), NOT Dead (destroys a healthy runtime over a
    /// coord-side failure). See ADR 0034.
    pub max_attempts: u32,
}

impl Default for EvictionScannerConfig {
    fn default() -> Self {
        Self {
            poll_interval: std::time::Duration::from_secs(10),
            max_attempts: 20,
        }
    }
}

/// Spawn the eviction scanner as a background task. Caller holds the
/// JoinHandle for the process lifetime; dropping aborts the loop.
/// Mirrors [`crate::evac_resumer::spawn`].
///
/// The first sweep after coord startup is the deploy-recovery story:
/// a row left `Evicting` by a pod that died mid-pipeline is picked
/// up here and re-driven (the pipeline is idempotent; the session
/// lease serializes against any surviving peer pod).
pub fn spawn_eviction_scanner(
    cfg: EvictionScannerConfig,
    state: SharedState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        loop {
            tick.tick().await;
            if let Err(e) = scanner_run_once(&cfg, &state).await {
                tracing::warn!(error = %e, "eviction scanner tick failed; will retry");
            }
        }
    })
}

/// Single scanner tick. `pub(crate)` so tests can drive the scanner
/// deterministically without `tokio::spawn`-ing the loop.
pub(crate) async fn scanner_run_once(
    cfg: &EvictionScannerConfig,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state.services.meta.list_evicting_sessions().await?;
    // Queue-depth gauge even when 0 — a flatline at 0 is the healthy
    // signal; a climbing value means evictions arrive faster than
    // pipelines complete.
    ::metrics::gauge!(crate::metrics::EVICTION_SCANNER_QUEUE).set(candidates.len() as f64);
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "eviction scanner found Evicting sessions"
    );
    for (session, attempts) in candidates {
        if let Err(e) = scanner_advance_one(cfg, state, session, attempts).await {
            // Keep going — one wedged session shouldn't stall the
            // sweep.
            tracing::warn!(error = %e, "eviction scanner per-session advance failed");
        }
    }
    Ok(())
}

async fn scanner_advance_one(
    cfg: &EvictionScannerConfig,
    state: &SharedState,
    session: engram_core::types::Session,
    attempts: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session_id = session.id;

    // Budget exhausted, or an inconsistent row (Evicting without a
    // bound sandbox — nothing to evict): fall back to HostLost. The
    // existing HostLost machinery (dead-host second stage, host-side
    // orphan reap, manual /resume) owns recovery from there, and
    // HostLost is not Active so neither detector re-nominates — the
    // loop is broken by construction.
    let fallback_reason = if attempts >= cfg.max_attempts {
        Some("retry budget exhausted")
    } else if session.sandbox_id.is_none() {
        Some("Evicting row has no bound sandbox")
    } else {
        None
    };
    if let Some(reason) = fallback_reason {
        match state
            .services
            .meta
            .transition_session(session_id, SessionState::HostLost)
            .await
        {
            Ok(prev) => {
                ::metrics::counter!(crate::metrics::EVICTION_BUDGET_EXHAUSTED_TOTAL).increment(1);
                tracing::warn!(
                    %session_id,
                    attempts,
                    max_attempts = cfg.max_attempts,
                    reason,
                    "eviction scanner gave up; session falls back to HostLost",
                );
                let _ = state
                    .emit(
                        session_id,
                        SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::HostLost,
                            at: Utc::now(),
                        },
                    )
                    .await;
            }
            Err(e) => {
                // Already moved (delete raced us, host died) — fine;
                // anything else logs and retries next tick.
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "eviction scanner fallback transition Evicting→HostLost failed",
                );
            }
        }
        return Ok(());
    }
    // Checked Some() above via fallback_reason.
    let sandbox_id = session.sandbox_id.expect("checked above");

    // Bump pre-pipeline: a failure leaves the counter incremented and
    // the session at Evicting — next tick retries until the budget
    // runs out. `transition_session(Evicting)` resets the counter on
    // every (re-)entry per migration 0050's CASE expression.
    let new_attempts = state.services.meta.bump_evict_attempts(session_id).await?;
    tracing::info!(
        %session_id,
        %sandbox_id,
        attempt = new_attempts,
        max_attempts = cfg.max_attempts,
        "eviction scanner: starting pipeline attempt",
    );

    let started = std::time::Instant::now();
    evict_idle_session(state, session_id, sandbox_id).await?;
    // Only successful runs are recorded — the histogram answers "how
    // long does a completed eviction take" (the pre-0034 bug would
    // reappear as nominations without completions, not as a latency
    // shift).
    ::metrics::histogram!(crate::metrics::EVICTION_PIPELINE_SECONDS)
        .record(started.elapsed().as_secs_f64());
    Ok(())
}

/// Marker that this module exists so unused-arg checkers don't
/// flag the `Arc<dyn SandboxBackend>` we explicitly take below.
#[allow(dead_code)]
fn _backend_unused_check<B: SandboxBackend>(_: Arc<B>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
    use engram_core::types::session::SessionMode;
    use engram_core::types::{Session, SessionState};
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::path::Path;
    use std::time::Duration;
    use tempfile::TempDir;

    fn build_state_with_session(session: Session, sandbox_root: &Path) -> SharedState {
        build_state_and_meta(session, sandbox_root).0
    }

    /// Variant that also hands back the MiniMeta so tests can reach
    /// its failure-injection toggles (`fail_next_record_snapshot`).
    fn build_state_and_meta(session: Session, sandbox_root: &Path) -> (SharedState, Arc<MiniMeta>) {
        let local_path = sandbox_root.join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: std::sync::Arc::new(engram_oci::AnonymousResolver),
            blob: std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: std::sync::Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path,
            ..CoordinatorConfig::default()
        };
        (
            Arc::new(AppState::new_with_registry(cfg, services, host_registry)),
            meta,
        )
    }

    fn process_spec() -> SandboxSpec {
        SandboxSpec {
            image: "evict-test".into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        }
    }

    #[tokio::test]
    async fn evict_idle_session_runs_full_pipeline() {
        // ADR 0005: the eviction pipeline is now snapshot + destroy +
        // mark-Idle only. The auto-checkpoint pre-step is gone — git
        // is no longer the platform's durability primitive.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "echo agent > out.txt".into()],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        state.services.host.exec(sandbox_id, req).await.unwrap();

        let mut sub = state.events.subscribe(session_id);

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("eviction should succeed");

        // Session is Idle, sandbox_id cleared, registry unbound.
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Idle);
        assert_eq!(after.sandbox_id, None);
        assert_eq!(state.registry.get(session_id), None);

        // A snapshot was recorded.
        let snaps = state
            .services
            .meta
            .list_snapshots_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(snaps.len(), 1, "exactly one snapshot recorded");

        // Drain the bus and confirm the post-checkpoint sequence:
        // SnapshotTaken / Evicted / StatusChanged.
        let mut kinds: Vec<String> = Vec::new();
        while let Ok(ev) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
            if let Ok(indexed) = ev {
                kinds.push(indexed.event.kind().to_string());
            }
        }
        for required in ["snapshot_taken", "evicted", "status_changed"] {
            assert!(
                kinds.iter().any(|k| k == required),
                "missing event {required} in {kinds:?}"
            );
        }
        assert!(
            !kinds.iter().any(|k| k == "checkpoint_pushed"),
            "ADR 0005: checkpoint_pushed must no longer be emitted (got {kinds:?})"
        );
    }

    /// ADR 0014 issue #1/#2 regression guard. When `record_snapshot`
    /// fails post-snapshot, the pipeline MUST call
    /// `host.abort_snapshot(sandbox_id)` before bubbling — without
    /// this, the host leaks the per-snapshot dir (4 GiB on FC), which
    /// is exactly the failure mode that filled `engrams-fc-xngk` in
    /// 13 minutes.
    #[tokio::test]
    async fn evict_idle_session_aborts_snapshot_when_record_snapshot_fails() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        // Spy that wraps the real HostRegistry and counts
        // commit_snapshot + abort_snapshot calls.
        struct SpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            aborts: StdArc<AtomicU32>,
            commits: StdArc<AtomicU32>,
        }

        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for SpyHost {
            // Pass through all required methods to inner.
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.commits.fetch_add(1, Ordering::SeqCst);
                self.inner.commit_snapshot(id).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.aborts.fetch_add(1, Ordering::SeqCst);
                self.inner.abort_snapshot(id).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<String> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
            ) {
                self.inner.bind_session(session_id, sandbox_id).await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, text).await
            }
            async fn acquire_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.acquire_shell(sandbox_id).await
            }
            async fn release_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.release_shell(sandbox_id).await
            }
        }

        // Wire up state with the spy wrapping the standard
        // HostRegistry → LocalHostClient → ProcessBackend stack, then
        // toggle MiniMeta to fail the next record_snapshot.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        *meta.fail_next_record_snapshot.lock() = true;
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let aborts = StdArc::new(AtomicU32::new(0));
        let commits = StdArc::new(AtomicU32::new(0));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(SpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            aborts: aborts.clone(),
            commits: commits.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-abort-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-abort-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);

        let err = evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect_err("record_snapshot failure must bubble out of evict_idle_session");
        match err {
            EvictError::Meta(_) => {}
            other => panic!("expected EvictError::Meta, got {other:?}"),
        }

        assert_eq!(
            aborts.load(Ordering::SeqCst),
            1,
            "host.abort_snapshot must fire exactly once after record_snapshot failure",
        );
        assert_eq!(
            commits.load(Ordering::SeqCst),
            0,
            "host.commit_snapshot must NOT fire when pipeline failed",
        );
    }

    /// Sanity inverse: full pipeline success → commit fires, no abort.
    #[tokio::test]
    async fn evict_idle_session_commits_snapshot_on_full_success() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        // Tiny duplicate of the spy from the abort test — keeps the
        // tests independently readable.
        struct SpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            aborts: StdArc<AtomicU32>,
            commits: StdArc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for SpyHost {
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.commits.fetch_add(1, Ordering::SeqCst);
                self.inner.commit_snapshot(id).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.aborts.fetch_add(1, Ordering::SeqCst);
                self.inner.abort_snapshot(id).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<String> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
            ) {
                self.inner.bind_session(session_id, sandbox_id).await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, text).await
            }
            async fn acquire_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.acquire_shell(sandbox_id).await
            }
            async fn release_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.release_shell(sandbox_id).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let aborts = StdArc::new(AtomicU32::new(0));
        let commits = StdArc::new(AtomicU32::new(0));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(SpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            aborts: aborts.clone(),
            commits: commits.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-commit-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-commit-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("full pipeline must succeed");

        assert_eq!(
            commits.load(Ordering::SeqCst),
            1,
            "commit_snapshot must fire exactly once on full pipeline success",
        );
        assert_eq!(
            aborts.load(Ordering::SeqCst),
            0,
            "abort_snapshot must NOT fire on full pipeline success",
        );
    }

    /// ADR 0034 durability regression guard. `commit_snapshot` MUST run
    /// while the sandbox is still bound — BEFORE `unbind`/`destroy`.
    /// With the old ordering (commit after destroy), `resolve_owner`
    /// returned NotFound, the host never cleared its in-flight tracking,
    /// and the racing periodic-checkpoint driver
    /// `abort_prior_inflight_snapshot`-ed the committed blobs out of
    /// BlobStorage while PG still advertised the snapshot as
    /// `recoverable=true` — bricking the resume (prod incident
    /// 89f7984d). Asserts the call order snapshot → commit → destroy.
    #[tokio::test]
    async fn evict_idle_session_commits_before_destroy() {
        use std::sync::Arc as StdArc;
        use std::sync::Mutex as StdMutex;

        // Spy that records the order of the lifecycle calls we care
        // about; everything else passes straight through to inner.
        struct OrderSpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            seq: StdArc<StdMutex<Vec<&'static str>>>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for OrderSpyHost {
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.seq.lock().unwrap().push("destroy");
                self.inner.destroy(id).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.seq.lock().unwrap().push("snapshot");
                self.inner.snapshot(id).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.seq.lock().unwrap().push("commit");
                self.inner.commit_snapshot(id).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.seq.lock().unwrap().push("abort");
                self.inner.abort_snapshot(id).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<String> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
            ) {
                self.inner.bind_session(session_id, sandbox_id).await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, text).await
            }
            async fn acquire_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.acquire_shell(sandbox_id).await
            }
            async fn release_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.release_shell(sandbox_id).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-order".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let seq = StdArc::new(StdMutex::new(Vec::new()));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(OrderSpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            seq: seq.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-order-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-order-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("full pipeline must succeed");

        let seq = seq.lock().unwrap().clone();
        let snapshot_pos = seq
            .iter()
            .position(|&s| s == "snapshot")
            .expect("snapshot must be called");
        let commit_pos = seq
            .iter()
            .position(|&s| s == "commit")
            .expect("commit_snapshot must be called");
        let destroy_pos = seq
            .iter()
            .position(|&s| s == "destroy")
            .expect("destroy must be called");
        assert!(
            snapshot_pos < commit_pos,
            "snapshot must precede commit; seq={seq:?}",
        );
        assert!(
            commit_pos < destroy_pos,
            "commit_snapshot must run BEFORE destroy (else resolve_owner NotFound \
             → committed blobs aborted while recoverable=true); seq={seq:?}",
        );
        assert!(
            !seq.contains(&"abort"),
            "no abort on the happy path; seq={seq:?}",
        );
    }

    /// ADR 0016 §A.1.5c regression guard. With a peer pod's lease
    /// pre-installed in the `session_lease` table (simulated
    /// via MiniMeta's in-memory mirror), a fresh
    /// `evict_idle_session` call must short-circuit to Ok(())
    /// without touching the session. Once the peer's lease is
    /// released, a follow-on call must acquire cleanly and run
    /// the pipeline to completion.
    #[tokio::test]
    async fn evict_idle_session_lease_serializes_concurrent_calls() {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:reentry".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // Simulate a peer coord pod holding the lease — direct
        // insert into MiniMeta's in-memory mirror is the
        // equivalent of another pod's prior
        // `try_acquire_session_lease` having returned `true`.
        state
            .services
            .meta
            .try_acquire_session_lease(session_id, Some(sandbox_id), "peer-pod-fixture")
            .await
            .expect("MiniMeta lease acquire never errors")
            .then_some(())
            .expect("peer's lease must acquire cleanly");

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("contention must short-circuit to Ok(()), not error");

        // Session was untouched — the early return happened before
        // any pipeline work.
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Active,
            "lease guard must short-circuit before transitioning session",
        );
        assert!(
            state.registry.get(session_id).is_some(),
            "lease guard must short-circuit before unbinding",
        );

        // Release the peer's lease and verify a fresh call now
        // runs the full pipeline. The successful call's RAII
        // Drop releases its own lease, so a third call would also
        // succeed (asserted via the post-state assertion below).
        state
            .services
            .meta
            .release_session_lease(session_id)
            .await
            .unwrap();

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("call with no contention must run the pipeline");

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Idle,
            "after the peer lease releases, a fresh eviction must complete",
        );

        // Yield once so the detached release tokio::spawn from
        // the successful call's RAII Drop has a chance to run
        // before we observe the in-memory mirror.
        tokio::task::yield_now().await;
        // 5ms is overkill but cheap; covers slower CI hosts.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        // Sniff MiniMeta's mirror via the trait surface: a fresh
        // try_acquire should succeed (lease was released by Drop).
        let could_acquire_again = state
            .services
            .meta
            .try_acquire_session_lease(session_id, Some(sandbox_id), "post-test-probe")
            .await
            .unwrap();
        assert!(
            could_acquire_again,
            "guard's Drop must release the lease on the success path",
        );
    }

    /// ADR 0016 §A.1.6 regression guard. The eviction pipeline
    /// must commit `transition_session(Idle)` to PG **before**
    /// calling `host.destroy(sandbox_id)`. Pre-fix ordering put
    /// destroy() at step 3 with transition_session at step 6 —
    /// the ~ms-to-seconds gap let the heartbeat-driven reconciler
    /// observe the now-orphan session, run HostLost→Idle, and
    /// race ahead of the pipeline's own PG flip.
    ///
    /// This test wraps the standard HostClient stack with a spy
    /// whose `destroy()` reads `MiniMeta`'s current session status
    /// at call time and stashes it. After `evict_idle_session`
    /// completes, the stashed status must be Idle — proving the
    /// transition committed before destroy() was invoked.
    #[tokio::test]
    async fn evict_idle_session_transitions_to_idle_before_destroy() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        struct OrderedSpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            meta: StdArc<MiniMeta>,
            destroys: StdArc<AtomicU32>,
            status_at_destroy: StdArc<parking_lot::Mutex<Option<SessionState>>>,
            session_id: SessionId,
        }

        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for OrderedSpyHost {
            async fn create(
                &self,
                spec: engram_core::types::sandbox::SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.destroys.fetch_add(1, Ordering::SeqCst);
                // Read the session's status straight from MiniMeta
                // at the precise moment destroy() is invoked. The
                // pipeline reorder means this must already be Idle.
                let snapshot = self.meta.session.lock().clone();
                if snapshot.id == self.session_id {
                    *self.status_at_destroy.lock() = Some(snapshot.status);
                }
                self.inner.destroy(id).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.commit_snapshot(id).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.abort_snapshot(id).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<String> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
            ) {
                self.inner.bind_session(session_id, sandbox_id).await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, text).await
            }
            async fn acquire_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.acquire_shell(sandbox_id).await
            }
            async fn release_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.release_shell(sandbox_id).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:reorder".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let destroys = StdArc::new(AtomicU32::new(0));
        let status_at_destroy: StdArc<parking_lot::Mutex<Option<SessionState>>> =
            StdArc::new(parking_lot::Mutex::new(None));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(OrderedSpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            meta: meta.clone(),
            destroys: destroys.clone(),
            status_at_destroy: status_at_destroy.clone(),
            session_id,
        });

        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-reorder-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-reorder-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("full pipeline must succeed under the reordered steps");

        assert_eq!(
            destroys.load(Ordering::SeqCst),
            1,
            "destroy must be called exactly once on full success",
        );
        // The crux of A.1.6: at the moment destroy() ran, the
        // session was already Idle in MiniMeta. If this regresses
        // (Idle-after-destroy ordering returns), the snapshot will
        // be Active (or Created/some-mid-state) and the
        // reconciler-race window reopens.
        assert_eq!(
            *status_at_destroy.lock(),
            Some(SessionState::Idle),
            "ADR 0016 §A.1.6: transition_session(Idle) must commit \
             to PG BEFORE host.destroy() is invoked",
        );

        // Final state: still Idle (sanity).
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Idle);
    }

    /// ADR 0016 §A.1.5c stale-lease reaper. Leases older than the
    /// max-age threshold are removed; fresh leases survive. Uses
    /// MiniMeta's in-memory mirror — we backdate one entry by
    /// hand, then call sweep with a small threshold.
    #[tokio::test]
    async fn sweep_stale_session_leases_reaps_old_entries() {
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:reap".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        // Acquire a fresh lease, then backdate it to simulate a
        // wedged pipeline. Need MiniMeta-typed access for the
        // backdate; downcast via `Any` would work but is ugly,
        // so the test reaches into the concrete type by leaning
        // on the fact that `services.meta` was wired as
        // `Arc<MiniMeta>`.
        let sandbox_id = engram_core::SandboxId::new();
        let stale_session = engram_core::SessionId::new();
        let fresh_session = engram_core::SessionId::new();
        state
            .services
            .meta
            .try_acquire_session_lease(stale_session, Some(sandbox_id), "stale-pod")
            .await
            .unwrap();
        state
            .services
            .meta
            .try_acquire_session_lease(fresh_session, Some(sandbox_id), "fresh-pod")
            .await
            .unwrap();

        // Age both leases past the sweep's `max_age` deterministically.
        // The earlier draft of this test passed `max_age = 0s` and
        // relied on `locked_at < now()` being true for both leases
        // by virtue of "the acquires happened before the sweep
        // call". On a fast machine `chrono::Utc::now()` can return
        // the same microsecond for two adjacent calls; the strict
        // `<` in MiniMeta::sweep_stale_session_leases (state.rs)
        // then skipped the second lease and `reaped_ids.contains(&fresh_session)`
        // failed intermittently. Sleeping ≥1 ms past `max_age = 1ms`
        // makes the assertion structurally deterministic regardless
        // of clock tick boundaries — no production-code change
        // needed.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let reaped = state
            .services
            .meta
            .sweep_stale_session_leases(std::time::Duration::from_millis(1))
            .await
            .unwrap();
        let reaped_ids: std::collections::HashSet<_> =
            reaped.iter().map(|l| l.session_id).collect();
        assert!(reaped_ids.contains(&stale_session));
        assert!(reaped_ids.contains(&fresh_session));
        for lease in &reaped {
            assert_eq!(lease.sandbox_id, Some(sandbox_id));
        }

        // After reap, both should be re-acquirable.
        assert!(state
            .services
            .meta
            .try_acquire_session_lease(stale_session, Some(sandbox_id), "post-reap")
            .await
            .unwrap());

        // And a sweep with a huge max_age must reap NOTHING.
        let reaped2 = state
            .services
            .meta
            .sweep_stale_session_leases(std::time::Duration::from_secs(3600))
            .await
            .unwrap();
        assert!(
            reaped2.is_empty(),
            "no fresh lease should be reaped under a long max_age",
        );
    }

    #[tokio::test]
    async fn evict_idle_session_is_a_noop_when_sandbox_already_unbound() {
        // Race-safe path: another evictor / operator already
        // unbound the sandbox. evict_idle_session should return
        // Ok(()) without touching anything else.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        };
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        // Don't bind any sandbox; pass a random SandboxId.
        evict_idle_session(&state, session_id, engram_core::SandboxId::new())
            .await
            .expect("noop on already-unbound");

        // Session stays Active (no eviction happened).
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active);
    }

    // ─── ADR 0034: eviction scanner tests ─────────────────────────

    fn evicting_session(id: engram_core::SessionId) -> Session {
        Session {
            id,
            user_id: None,
            status: SessionState::Evicting,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-scanner".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    /// Happy path: the scanner sweeps an Evicting row and drives the
    /// full pipeline — session lands Idle with a recorded snapshot,
    /// sandbox unbound. This is the nomination handler's other half.
    #[tokio::test]
    async fn scanner_drives_evicting_to_idle() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(evicting_session(session_id), sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        scanner_run_once(&EvictionScannerConfig::default(), &state)
            .await
            .expect("scanner tick");

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Idle);
        assert_eq!(after.sandbox_id, None);
        assert_eq!(state.registry.get(session_id), None);
        let snaps = state
            .services
            .meta
            .list_snapshots_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(snaps.len(), 1, "pipeline recorded its snapshot");
    }

    /// A failed pipeline attempt leaves the row Evicting with the
    /// attempt counter bumped — the next tick retries. (This is the
    /// crash-/flake-tolerant replacement for the pre-0034 silent
    /// cancellation.)
    #[tokio::test]
    async fn scanner_failure_stays_evicting_and_bumps_attempts() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let (state, mini) = build_state_and_meta(evicting_session(session_id), sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        // Force the pipeline's record_snapshot step to fail once.
        *mini.fail_next_record_snapshot.lock() = true;

        scanner_run_once(&EvictionScannerConfig::default(), &state)
            .await
            .expect("tick itself succeeds; per-session failure is swallowed");

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Evicting,
            "stays in lane for retry"
        );
        let listed = state.services.meta.list_evicting_sessions().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1, 1, "attempt counter bumped pre-pipeline");
    }

    /// Budget exhaustion falls back to HostLost (not Active — would
    /// re-nominate forever; not Idle — no durable snapshot exists;
    /// not Dead — the runtime is healthy). max_attempts=0 trips the
    /// arm immediately.
    #[tokio::test]
    async fn scanner_budget_exhaustion_falls_back_to_host_lost() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let mut session = evicting_session(session_id);
        session.sandbox_id = Some(engram_core::SandboxId::new());
        let state = build_state_with_session(session, sandbox_root.path());

        let mut sub = state.events.subscribe(session_id);
        let cfg = EvictionScannerConfig {
            max_attempts: 0,
            ..EvictionScannerConfig::default()
        };
        scanner_run_once(&cfg, &state).await.expect("tick");

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::HostLost);
        // StatusChanged(Evicting → HostLost) emitted for the timeline.
        let indexed = tokio::time::timeout(Duration::from_secs(1), sub.recv())
            .await
            .expect("event within 1s")
            .expect("bus open");
        match indexed.event {
            SessionEvent::StatusChanged { from, to, .. } => {
                assert_eq!(from, SessionState::Evicting);
                assert_eq!(to, SessionState::HostLost);
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    /// An Evicting row with no bound sandbox is structurally
    /// inconsistent — nothing to evict. Falls back to HostLost
    /// rather than burning 20 attempts on a guaranteed failure.
    #[tokio::test]
    async fn scanner_no_sandbox_falls_back_to_host_lost() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(evicting_session(session_id), sandbox_root.path());

        scanner_run_once(&EvictionScannerConfig::default(), &state)
            .await
            .expect("tick");

        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::HostLost);
    }

    /// No Evicting rows → the tick is a cheap no-op.
    #[tokio::test]
    async fn scanner_empty_sweep_is_noop() {
        let session_id = engram_core::SessionId::new();
        let sandbox_root = TempDir::new().unwrap();
        let mut session = evicting_session(session_id);
        session.status = SessionState::Active;
        let state = build_state_with_session(session, sandbox_root.path());

        scanner_run_once(&EvictionScannerConfig::default(), &state)
            .await
            .expect("tick");
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active, "untouched");
    }
}
