//! Snapshot / evict / resume endpoints.
//!
//! State machine:
//!
//! ```text
//!   POST /sessions             -> Active   (sandbox live, registry bound)
//!   POST /sessions/:id/snapshot -> Active   (snapshot taken, sandbox stays live)
//!   DELETE /sessions/:id/local -> Idle     (sandbox destroyed, requires snapshot)
//!   POST /sessions/:id/resume   -> Active   (restored from snapshot, registry rebound)
//!   DELETE /sessions/:id        -> Completed (terminal)
//! ```
//!
//! `snapshot` is intentionally separate from `evict_local`: snapshot is a
//! save-point that keeps the live sandbox running, evict drops it. Most
//! callers that want both should snapshot then evict in two requests, or
//! evict then resume across the lifecycle of a session.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::traits::storage::BlobStorage;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{Session, SessionState};
use engram_core::{SandboxError, SandboxId, SessionId};
use serde::Serialize;

use crate::error::ApiError;
use crate::host_registry::ScheduleContext;
use crate::state::{RecoveryCause, SessionEvent, SharedState};

/// ADR 0039 follow-up #20: how long `ensure_active` will HOLD a
/// request that arrived mid-eviction (session `Evicting`) waiting for
/// the eviction pipeline to land the session at `Idle` before falling
/// back to the retryable 409. The eviction scanner sweeps on a 10s
/// tick and a single pipeline is sub-second once it starts, so a few
/// seconds covers the common "message races the snapshot" case
/// without pinning a request for the full scanner cadence. Tunable via
/// `ENGRAM_RESUME_EVICTING_HOLD_SECS` (0 disables the hold → immediate
/// 409, the pre-follow-up behaviour).
const DEFAULT_RESUME_EVICTING_HOLD_SECS: u64 = 8;

/// Poll cadence while holding inside the `Evicting` arm. Short so a
/// fast-settling eviction is observed promptly; the bound above caps
/// the total wait.
const RESUME_EVICTING_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Read `ENGRAM_RESUME_EVICTING_HOLD_SECS` — falls through to
/// [`DEFAULT_RESUME_EVICTING_HOLD_SECS`]. `0` disables the hold.
fn resume_evicting_hold_from_env() -> Duration {
    std::env::var("ENGRAM_RESUME_EVICTING_HOLD_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_RESUME_EVICTING_HOLD_SECS))
}

/// ADR 0016 §A.1.7 fallback: synthesize an unspecified-IP
/// `SessionEgressPolicy`. Used only when
/// `build_resume_egress_policy` returns `None` (no manifest bundle,
/// host has no guest IP, or IP unparseable). The host treats a
/// zero-IP policy as "no policy applied" — same observable behavior
/// as the pre-fix code path. SecretMode is `Broker` because Broker
/// is the benign default (zero secrets means broker mode does
/// nothing); see the matching create-path placeholder in
/// `api/sessions.rs`.
fn placeholder_egress_policy(
    session_id: SessionId,
    sandbox_id: SandboxId,
) -> engram_core::types::egress::SessionEgressPolicy {
    engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id,
        guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
        network_allow_hosts: vec![],
        network_allow_host_patterns: vec![],
        secrets: vec![],
        secret_mode: engram_core::types::image::SecretMode::Broker,
    }
}

#[derive(Serialize)]
pub struct SnapshotResponse {
    pub session_id: SessionId,
    pub snapshot_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub note: &'static str,
}

pub async fn snapshot(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    state.services.meta.get_session(id).await?;

    let sandbox_id = state.registry.get(id).ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to snapshot — create or resume first".into(),
        )
    })?;

    // ADR 0007 Phase 6: backend owns its staging dir. Coord no
    // longer pre-allocates a path — the backend's
    // `snapshot_path_for(metadata.id)` is the canonical reference
    // for the on-disk location. Cross-host durability flows
    // through the chunked manifests on `SnapshotMetadata`, not
    // through the local path.
    let metadata = state.services.host.snapshot(sandbox_id).await?;

    let now = Utc::now();
    // Record the host that wrote this snapshot to its local disk so
    // the resume path's snapshot-affinity scheduler can route back to
    // it (zero-cost hot-tier hit). ADR 0007: durability lives in the
    // chunk store (`disk_manifest` / `memory_manifest`); the local dir
    // is a per-host cache the same-host fast-resume reads from.
    let host_id = state.host_registry.host_of(sandbox_id);
    // ADR 0009 Phase 2: HEAD-verify the chunked manifests are
    // durable in BlobStorage before flipping `recoverable=true`.
    // Backends that produced no manifest (Process; VZ memory) flip
    // to false, which means a sandbox-loss reconcile will Dead them
    // — correct, since there's no chunked artifact to resume from.
    let recoverable = verify_snapshot_recoverable(
        state.services.blob.as_ref(),
        metadata.disk_manifest.as_ref(),
        metadata.memory_manifest.as_ref(),
    )
    .await;
    let record = SnapshotRecord {
        id: metadata.id,
        session_id: Some(id),
        host_id,
        image_version: metadata.image_version,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
        // ADR 0007: chunked manifests are the durability primitive.
        // FC backends produce both fields via the PooledBackend wrap;
        // VZ produces disk_manifest only; Process produces neither.
        disk_manifest: metadata.disk_manifest,
        memory_manifest: metadata.memory_manifest,
        recoverable,
        // ADR 0035: pin the generations this snapshot's device model
        // references (host-reported; reflects any fresh-create swap).
        aux_bundles: metadata.aux_bundles.clone(),
        // ADR 0028 A.log: best-effort cursor at the capture instant
        // (the guest pauses inside the snapshot RPC; sub-second skew
        // accepted, documented on `latest_event_idx_at_or_before`).
        events_cursor: state
            .services
            .meta
            .latest_event_idx_at_or_before(id, now)
            .await
            .unwrap_or_default(),
    };
    state.services.meta.record_snapshot(record).await?;

    // ADR 0034 durability: the live sandbox keeps running (see the note
    // below), so commit the host's in-flight snapshot now — otherwise
    // the periodic checkpoint driver's next tick calls
    // `abort_prior_inflight_snapshot` and deletes this snapshot's
    // state.bin/sidecar from BlobStorage within one interval, leaving
    // the `recoverable = true` row we just wrote pointing at nothing.
    // Same defect class as the idle-eviction commit-after-destroy bug,
    // but here it's deterministic (not a race) because the sandbox
    // stays alive and the driver is guaranteed to fire. Best-effort:
    // a host RPC failure is backstopped by resume-time blob
    // verification (`resume_from_idle`).
    if let Err(e) = state.services.host.commit_snapshot(sandbox_id).await {
        tracing::warn!(
            session_id = %id,
            sandbox_id = %sandbox_id,
            error = %e,
            "snapshot: commit_snapshot failed; periodic checkpoint may abort this \
             snapshot's artifacts (resume-time verification will catch it)",
        );
    }

    state
        .emit(
            id,
            SessionEvent::SnapshotTaken {
                snapshot_id: metadata.id,
                size_bytes: metadata.size_bytes,
                at: now,
            },
        )
        .await?;

    // Snapshot does NOT change session state — the live sandbox keeps
    // running. evict_local is the explicit "drop from RAM" action.
    Ok(Json(SnapshotResponse {
        session_id: id,
        snapshot_id: Some(metadata.id.to_string()),
        size_bytes: Some(metadata.size_bytes),
        note: "snapshot recorded; live sandbox still running",
    }))
}

pub async fn resume(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    resume_session(state, id).await.map(Json)
}

/// Auto-resume an `Idle` session if needed, before routing an
/// exec / exec_stream / events request to it. Track B's pack-hosts
/// counterpart: the idle evictor hot-suspends inactive sessions;
/// this helper brings them back transparently on the next request.
///
/// `Active` sessions are a no-op (Ok). `Idle` sessions are resumed
/// via the existing `/resume` flow and the function returns once the
/// session is Active again.
///
/// ADR 0015 M2: every other state returns a typed error rather than
/// falling through to a downstream handler that would race against
/// agentd readiness. `Created` / `GuestReady` (session still mid-
/// create or mid-resume; harness not yet running) return 409;
/// `HostLost` / `Dead` are unrecoverable from this entry point and
/// return 410; terminal `Completed` / `Failed` return 409 (no work
/// is left to dispatch).
pub async fn ensure_active(state: &SharedState, id: SessionId) -> Result<(), ApiError> {
    let session = state.services.meta.get_session(id).await?;
    match session.status {
        SessionState::Active => Ok(()),
        SessionState::Idle | SessionState::Evacuating => {
            // Both states are "snapshotted, sandbox destroyed, ready to
            // resume." `Evacuating` differs from `Idle` only in
            // intent (scanner-driven vs user-driven); the resume code
            // path is the same. The `evac_resumer` (ADR 0018 commit 12)
            // is the auto-driver for Evacuating; a synchronous
            // /exec-triggered ensure_active beats it by inline-resuming
            // here, which is fine — both end at Active.
            resume_session(state.clone(), id).await?;
            Ok(())
        }
        // ADR 0034: mid-eviction. The sandbox may still be live (the
        // eviction scanner is snapshotting it), so this is explicitly
        // NOT the auto-resume arm above — kicking off a restore here
        // would race the pipeline (the session lease would 409 the
        // loser anyway, but only after a wasted restore attempt).
        //
        // ADR 0039 follow-up #20: rather than bounce an immediate 409
        // (prod observed a follow-up message stranded 4m32s waiting on
        // the next client retry), HOLD briefly for the eviction to
        // land the session at Idle, then auto-resume inline. The
        // bounded poll caps the wait; if it doesn't settle we fall
        // through to the same retryable 409 as before. We do NOT race
        // the pipeline: the hold only *observes* the status flip and
        // then takes the standard resume path, whose session lease +
        // status gate serialize against the eviction (no double-
        // resume, no orphaned sandbox).
        SessionState::Evicting => ensure_active_after_evicting_hold(state, id).await,
        SessionState::Created | SessionState::GuestReady => Err(ApiError::Conflict(format!(
            "session is {} — agentd is not yet ready. \
             Wait for the session to reach Active (subscribe to /sessions/:id/events) \
             and retry.",
            session.status.as_str()
        ))),
        SessionState::Pending => Err(ApiError::Conflict(
            "session is pending — scheduling has not completed".into(),
        )),
        SessionState::HostLost => Err(ApiError::HostLost(
            "session's host went away; resume from a snapshot if one exists".into(),
        )),
        SessionState::Dead => Err(ApiError::Gone(
            "session is dead — chunked manifests are gone or never existed".into(),
        )),
        SessionState::Failed | SessionState::Completed => Err(ApiError::Conflict(format!(
            "session is {} (terminal); no work to dispatch",
            session.status.as_str()
        ))),
    }
}

/// ADR 0039 follow-up #20: the `Evicting` arm of [`ensure_active`].
/// HOLD the request for up to `ENGRAM_RESUME_EVICTING_HOLD_SECS`
/// (re-reading the session row on a short poll) for the eviction
/// pipeline to land the session at a resumable state, then resume
/// inline. The fallback when the hold expires is the same retryable
/// 409 the pre-follow-up code returned — semantics preserved, just
/// after a bounded wait instead of immediately.
///
/// Why this is race-safe and never double-resumes:
/// - We only *observe* the status here; the actual resume goes through
///   [`resume_session`], which acquires the per-session `session_lease`
///   for the whole restore. The eviction pipeline holds that same lease
///   end-to-end, so the resume can't begin until the pipeline has fully
///   released — at which point the session is already `Idle`.
/// - The status gate inside `resume_session` re-reads the row under the
///   lease, so even if two callers escape the hold simultaneously, only
///   the first finds a resumable state; the rest hit the lease 409 or
///   the no-longer-Idle gate.
/// - Terminal/raced transitions (a DELETE flips `Evicting → Completed`,
///   or the scanner exhausts its budget to `HostLost`) drop out of the
///   poll and re-dispatch through `ensure_active` for the honest typed
///   error for that state.
async fn ensure_active_after_evicting_hold(
    state: &SharedState,
    id: SessionId,
) -> Result<(), ApiError> {
    ensure_active_after_evicting_hold_for(state, id, resume_evicting_hold_from_env()).await
}

/// Inner [`ensure_active_after_evicting_hold`] with the hold duration
/// passed explicitly so tests can drive the bound deterministically
/// (a `0` hold for the immediate-409 fallback, a short non-zero hold
/// for the settle-then-resume path) without mutating the
/// process-global `ENGRAM_RESUME_EVICTING_HOLD_SECS` env — which would
/// race other parallel tests.
async fn ensure_active_after_evicting_hold_for(
    state: &SharedState,
    id: SessionId,
    hold: Duration,
) -> Result<(), ApiError> {
    let deadline = std::time::Instant::now() + hold;
    loop {
        // Sleep first: we were just told the session is Evicting, so an
        // immediate re-read would almost always still be Evicting. The
        // bound is the hold duration; a `0` env value means the very
        // first check sees the deadline already passed → immediate 409
        // (the pre-follow-up behaviour, preserved as an opt-out).
        if std::time::Instant::now() >= deadline {
            return Err(ApiError::Conflict(
                "session is mid-eviction; retry shortly (it will land at idle and auto-resume)"
                    .into(),
            ));
        }
        tokio::time::sleep(RESUME_EVICTING_POLL_INTERVAL).await;

        let session = state.services.meta.get_session(id).await?;
        match session.status {
            // Settled to a resumable state — take the standard resume
            // path (lease-serialized; see the doc comment).
            SessionState::Idle | SessionState::Evacuating => {
                resume_session(state.clone(), id).await?;
                return Ok(());
            }
            // Already back to Active (e.g. a concurrent resume won) —
            // nothing to do.
            SessionState::Active => return Ok(()),
            // Still mid-eviction — keep holding until the deadline.
            SessionState::Evicting => continue,
            // The eviction raced to a terminal / unrecoverable state
            // (DELETE → Completed, scanner budget → HostLost, …). Re-
            // dispatch through ensure_active so the caller gets that
            // state's honest typed error rather than a misleading 409.
            _ => return Box::pin(ensure_active(state, id)).await,
        }
    }
}

async fn resume_session(state: SharedState, id: SessionId) -> Result<SnapshotResponse, ApiError> {
    // Serialize resume against concurrent resume + eviction for this session.
    // Reuses the per-session `session_lease` lease (keyed on session_id).
    // Without it, two concurrent resumes — e.g. the prompt path's auto-resume
    // (`ensure_active`) racing a manual `/resume`, or two prompts on one Idle
    // session — each call `restore_for_session`, creating a live VM *before*
    // the `Idle → Created` transition that would serialize them. The loser
    // orphans its sandbox (and last-writer-wins on `sessions.sandbox_id` can
    // leave the row bound to the wrong one). Holding the lease for the whole
    // resume makes resume + eviction mutually exclusive per session. The
    // lease's `sandbox_id` is a diagnostic-only column; resume has no sandbox
    // at acquire time (it's about to create one), so we pass `None`.
    let _lease = match crate::idle_evictor::SessionLeaseGuard::try_acquire(&state, id, None).await {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            return Err(ApiError::Conflict(format!(
                "session {id} is already mid-resume or mid-eviction; retry shortly",
            )))
        }
        Err(e) => {
            return Err(ApiError::Internal(format!(
                "resume lease acquire failed: {e}"
            )))
        }
    };

    let session = state.services.meta.get_session(id).await?;

    // ADR 0007: single-tier dispatcher.
    //   Idle  → restore from the snapshot's chunked manifests
    //           (snapshot-affinity-scheduled to the host that
    //           captured it; cross-host materialization is a
    //           follow-up).
    //   Dead  → 410 Gone (no recoverable manifests).
    //   any other status → 409.
    match session.status {
        SessionState::Idle => resume_from_idle(state, session).await,
        // ADR 0018: an auto-evac'd session is left at `Created` on
        // the new host with the VM restored but the harness stale
        // (vsock to source's agentd is dead). `/resume` against
        // `Created` finishes the harness rebuild via the shared
        // `finish_resume_to_active` primitive. This is the path that
        // closes the "session survives a MIG roll" loop —
        // `dead_host.rs` rebinds + restores, the user (or an
        // automated layer) hits `/resume` to bring it to Active.
        SessionState::Created => resume_from_created(state, session).await,
        SessionState::Dead => Err(ApiError::Gone(
            "snapshot_invalidated: session is terminal; chunked manifests are gone or never existed".into(),
        )),
        // ADR 0034: a direct /resume mid-eviction gets the same
        // honest retryable 409 as ensure_active (between pipeline
        // attempts the session lease is free, so the status gate —
        // not the lease — is what catches this).
        SessionState::Evicting => Err(ApiError::Conflict(
            "session is mid-eviction; retry shortly (it will land at idle and become resumable)"
                .into(),
        )),
        other => Err(ApiError::Conflict(format!(
            "session is {} — only Idle / Created sessions can be resumed",
            other.as_str()
        ))),
    }
}

/// ADR 0018 commit 10: finish bringing an auto-evac'd session back
/// to Active. Preconditions: session at `Created` with `host_id` +
/// `sandbox_id` already bound (the evac primitive did the bind +
/// HostLost → Created drive). All this needs to do is run the harness
/// rebuild + the Created → Active transition.
async fn resume_from_created(
    state: SharedState,
    session: Session,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(ApiError::Conflict(format!(
            "session {id} is Created but has no bound sandbox — cannot resume",
        )));
    };
    if session.host_id.is_none() {
        return Err(ApiError::Conflict(format!(
            "session {id} is Created but has no bound host — cannot resume",
        )));
    }
    let outcome = finish_resume_to_active(&state, &session, sandbox_id).await?;
    let note = match outcome {
        FinishResumeOutcome::Active => "resumed from Created (auto-evac completion)",
        FinishResumeOutcome::CreatedHarnessFailed => {
            "resume attempted from Created; harness reattach failed — session still Created"
        }
    };
    // No SnapshotResponse.snapshot_id — the session might've been
    // relocated via live_disk_manifest-only path (no snapshot row
    // backed the restore). The caller cares about session_id +
    // note; size_bytes is unknown here.
    Ok(SnapshotResponse {
        session_id: id,
        snapshot_id: None,
        size_bytes: None,
        note,
    })
}

async fn resume_from_idle(
    state: SharedState,
    session: Session,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;

    // ADR 0034 durability: walk the session's snapshots newest-first and
    // resume from the first one whose backing artifacts still exist. The
    // stored `recoverable` flag is set at capture time and can go stale
    // — the eviction/checkpoint abort race deletes a committed
    // snapshot's `state.bin`/`sidecar.json` out from under a
    // `recoverable = true` row (prod incident 89f7984d). Re-verifying at
    // the point of use and falling back through the ADR-0028 checkpoint
    // chain means a single bricked row no longer dooms the session — it
    // self-heals to the previous good checkpoint. A demoted row is
    // written back `recoverable = false` so reconcile / GC-pin / future
    // resumes stop trusting it.
    let snapshots = state.services.meta.list_snapshots_for_session(id).await?;
    for record in snapshots {
        if snapshot_artifacts_present(state.services.blob.as_ref(), &record).await {
            return resume_from_fc_snapshot(state, session, record).await;
        }
        tracing::warn!(
            session_id = %id,
            snapshot_id = %record.id,
            "resume: snapshot artifacts missing; demoting recoverable=false and \
             falling back to the previous checkpoint",
        );
        if record.recoverable {
            let mut demoted = record.clone();
            demoted.recoverable = false;
            if let Err(e) = state.services.meta.record_snapshot(demoted).await {
                tracing::warn!(
                    session_id = %id,
                    snapshot_id = %record.id,
                    error = %e,
                    "resume: failed to demote unrecoverable snapshot row (best-effort)",
                );
            }
        }
    }

    // No snapshot row had live artifacts. ADR 0028 Fix B: a continuous-
    // sync disk manifest → recover via the disk-only cold boot (rung 2)
    // instead of declaring the session dead. This is also the documented
    // recovery path for `ColdBootUnavailable` Idle fallbacks: re-enable
    // the image, then /resume lands here.
    if session.live_disk_manifest.is_some() {
        return resume_disk_only_cold_boot(state, session).await;
    }
    tracing::warn!(
        session_id = %id,
        "resume requested but no snapshot row had recoverable artifacts — marking Dead",
    );
    let _ = transition_to_dead_if_no_snapshot(&state, id).await;
    Err(ApiError::Gone(
        "snapshot_invalidated: session can't be revived; \
         use `engram session fork <id>` to continue"
            .into(),
    ))
}

/// ADR 0028 Fix B — manual-resume flavor of the disk-only cold boot:
/// fresh kernel boot mounting the session's `live_disk_manifest` on
/// whichever host can take it, fresh harness. On-disk work survives;
/// in-RAM context does not (this path only exists because no coherent
/// memory snapshot was ever recorded).
async fn resume_disk_only_cold_boot(
    state: SharedState,
    session: Session,
) -> Result<SnapshotResponse, ApiError> {
    use crate::evacuation::{evacuate_dead_source, resolve_cold_boot_spec, EvacError};

    let id = session.id;
    let Some(spec) = resolve_cold_boot_spec(&state.services.meta, &session).await else {
        return Err(ApiError::Conflict(format!(
            "session {id} has only a live disk manifest and its image `{}` is no \
             longer enabled — re-enable it (POST /api/enabled-images), then retry /resume",
            session.image,
        )));
    };

    // The previous host isn't dead here (Idle = the sandbox was
    // destroyed); clearing host_id disables `exclude_host` so a
    // single-host deployment can recover onto itself.
    let mut relocatable = session.clone();
    relocatable.host_id = None;

    let receipt = evacuate_dead_source(
        &state.host_registry,
        &state.services.meta,
        relocatable,
        None,
        Some(spec),
        None,
    )
    .await
    .map_err(|e| match &e {
        EvacError::NoTargetAvailable(_) => ApiError::Unavailable(format!(
            "no host can take the disk-only cold-boot recovery right now: {e}. \
             Retry shortly.",
        )),
        _ => ApiError::Internal(format!("disk-only cold-boot recovery failed: {e}")),
    })?;

    tracing::info!(
        session_id = %id,
        new_host = %receipt.new_host_id,
        new_sandbox = %receipt.new_sandbox_id,
        loss = receipt.loss.as_str(),
        "resume: disk-only cold boot relocated session to Created — finishing harness rebuild",
    );
    let _ = state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: SessionState::Idle,
                to: SessionState::Created,
                at: Utc::now(),
            },
        )
        .await;

    bind_session_routing(&state, id, receipt.new_sandbox_id).await;
    let refreshed = state.services.meta.get_session(id).await?;
    let outcome = finish_resume_to_active(&state, &refreshed, receipt.new_sandbox_id).await?;
    let note = match outcome {
        FinishResumeOutcome::Active => {
            "resumed via disk-only cold boot (fresh kernel on latest disk; in-RAM context lost)"
        }
        FinishResumeOutcome::CreatedHarnessFailed => {
            "disk-only cold boot relocated the session; harness spawn failed — still Created"
        }
    };
    Ok(SnapshotResponse {
        session_id: id,
        snapshot_id: None,
        size_bytes: None,
        note,
    })
}

/// Look up the session's snapshot one more time and, if there's
/// truly nothing recoverable, set status = Dead. Best-effort —
/// failure here just means a future request will redo the same
/// check. Called from `resume_session`'s no-snapshot path.
async fn transition_to_dead_if_no_snapshot(
    state: &SharedState,
    id: SessionId,
) -> Result<(), ApiError> {
    if state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .is_some()
    {
        return Ok(());
    }
    match state
        .services
        .meta
        .transition_session(id, SessionState::Dead)
        .await
    {
        Ok(prev) => {
            let _ = state
                .emit(
                    id,
                    SessionEvent::StatusChanged {
                        from: prev,
                        to: SessionState::Dead,
                        at: Utc::now(),
                    },
                )
                .await;
        }
        Err(e) => {
            tracing::warn!(
                session_id = %id,
                error = %e,
                "transition_to_dead_if_no_snapshot: transition failed (likely already terminal); leaving as-is"
            );
        }
    }
    Ok(())
}

/// Same-host resume path: the snapshot's chunked manifests are
/// durable in `BlobStorage`, and the host that captured the snapshot
/// still holds the dir under
/// `<cfg.local_path>/snapshots/<session>/<snapshot_id>`. The scheduler
/// routes restore back to that host via snapshot-affinity.
///
/// Cross-host materialization (where the picked host doesn't have a
/// local copy and reconstructs from chunks) is a follow-up task —
/// the chunked manifests are the portable form, but FC's restore
/// API still wants a directory on local disk today.
/// ADR 0016 Phase B commit 6 — pick the disk manifest that should
/// drive the resume. Returns the newer of `live_disk_manifest`
/// (the host's last-published FlushScheduler manifest) and
/// `snapshot_disk_manifest` (the lineage recorded at snapshot
/// time).
///
/// Behaviour matrix:
///
/// | live      | snapshot  | result           | rationale                            |
/// |-----------|-----------|------------------|--------------------------------------|
/// | None      | None      | None             | Legacy session, no chunked manifest. |
/// | None      | Some(s)   | Some(s)          | No live publish; snapshot wins.      |
/// | Some(l)   | None      | Some(l)          | No snapshot manifest; live wins.     |
/// | Some(l)   | Some(s)   | Some(newer)      | Same manifest_id → max(version).     |
/// |           |           |                  | Different manifest_id → snapshot     |
/// |           |           |                  | (live can't supersede a different    |
/// |           |           |                  | lineage; safer fallback).            |
///
/// The "different manifest_id" branch is defensive: today both
/// columns track the same `manifest_id` over a session's
/// lifetime (the chunk-store version chain keeps the id stable;
/// only `version` ticks). If we ever see them diverge, the
/// snapshot lineage is authoritative because that's the
/// (memory, disk) pair the user actually paused at.
fn effective_resume_disk_manifest(
    live: Option<engram_core::types::manifest::ManifestRef>,
    snapshot: Option<engram_core::types::manifest::ManifestRef>,
) -> Option<engram_core::types::manifest::ManifestRef> {
    match (live, snapshot) {
        (None, snap) => snap,
        (Some(l), None) => Some(l),
        (Some(l), Some(s)) => {
            if l.manifest_id == s.manifest_id && l.version > s.version {
                Some(l)
            } else {
                Some(s)
            }
        }
    }
}

/// Outcome of [`finish_resume_to_active`]. The session is either
/// fully back at `Active` (`Active`) or the rebuilt VM is bound at
/// `Created` because the harness rebuild failed (`CreatedHarnessFailed`).
/// Callers map both to a 200 — `/exec` against `CreatedHarnessFailed`
/// returns 409 with the honest state, the user can retry resume.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FinishResumeOutcome {
    Active,
    CreatedHarnessFailed,
}

/// ADR 0028 A.log: rung-1 recovery rewind. Called after a coherent
/// (memory, disk) checkpoint was restored — the restored guest has no
/// memory of events in `(events_cursor, crash]`, so we rewind the
/// live transcript head to the cursor (tombstoning that span, not
/// deleting it) and emit an honest, legible boundary the web renders.
///
/// No-op when the record carries no `events_cursor` (pre-0053 / never
/// resolved) or when nothing is past it (the checkpoint was already
/// the head). Best-effort: a rewind failure logs and leaves the
/// transcript as-is rather than blocking the resume — the guest is
/// already coherent; the worst case is a confusing-but-intact log.
///
/// Shared by [`resume_from_fc_snapshot`] (manual `/resume`) and
/// `evac_resumer::run_resume_pipeline` (operator drain / teleport) — the
/// two rung-1 entry points. The `cause` distinguishes them for the web
/// copy (ADR 0045 F1): the manual-`/resume` path resumes a session that
/// was idled after its host died, so it carries
/// [`RecoveryCause::HostFailureRecovery`]; the evac-resumer path is an
/// operator-initiated relocation, so it carries
/// [`RecoveryCause::PlannedRelocation`].
pub async fn apply_rung1_rewind(
    state: &SharedState,
    session_id: SessionId,
    events_cursor: Option<i64>,
    cause: RecoveryCause,
) {
    // Only coherent checkpoints (memory present) rewind; a disk-only
    // record never reaches here (rung 2 cold-boots fresh, no rewind).
    // `events_cursor` is the checkpoint's resolved cursor (None =
    // pre-0053 / unresolved → no rewind information).
    let Some(cursor) = events_cursor else {
        return;
    };
    let summary = match state
        .services
        .meta
        .rewind_session_to_cursor(session_id, cursor)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                %session_id,
                error = %e,
                "rung-1 transcript rewind failed; resume proceeds with the unrewound log",
            );
            return;
        }
    };
    if summary.rolled_back == 0 {
        return; // checkpoint was the head — nothing rolled back.
    }
    tracing::info!(
        %session_id,
        rolled_back = summary.rolled_back,
        recovery_epoch = summary.recovery_epoch,
        surviving_side_effects = summary.surviving_side_effects.len(),
        "rung-1 recovery rewound the transcript to the checkpoint cursor",
    );
    // The boundary event is the first of the new epoch — it appends
    // AFTER the tombstoned span (higher idx) and carries the new
    // epoch (append_session_event reads the just-bumped value).
    let _ = state
        .emit(
            session_id,
            SessionEvent::RecoveredFromCheckpoint {
                recovery_epoch: summary.recovery_epoch,
                through_idx: summary.through_idx,
                rolled_back: summary.rolled_back,
                surviving_side_effects: summary.surviving_side_effects,
                cause,
                at: Utc::now(),
            },
        )
        .await;
}

/// ADR 0018 commit 10 — the "session is bound on a new host at
/// `Created`, finish bringing it back to `Active`" primitive shared
/// across every code path that drops a session into `Created` with a
/// fresh sandbox bound:
///
/// - [`resume_from_fc_snapshot`] (user-initiated `/resume` from Idle).
/// - [`crate::api::admin::evacuate_session`] (operator drain via the
///   admin endpoint).
/// - [`crate::evac_resumer`] scanner (drives `Evacuating → Created`
///   for sessions an operator drain marked; ADR 0044 K3).
/// - [`resume_from_created`] dispatcher arm (manual recovery of a
///   drained session).
///
/// Preconditions: the session row is at `Created` state, `host_id` +
/// `sandbox_id` are bound to the target (caller's responsibility).
///
/// Steps:
/// 1. Load the image's manifest bundle + per-request secrets to
///    rebuild the launch env.
/// 2. Resolve the harness AgentSpec. `harness=None` skips
///    `start_agent` entirely.
/// 3. Rebuild the SessionEgressPolicy for the new sandbox (fresh
///    guest_ip from the restored VM).
/// 4. Call `start_agent` — re-attaches the in-VM agentd's harness
///    supervisor to the post-restore sandbox.
/// 5. Transition `Created → Active` + emit `Resumed` + `StatusChanged`.
///
/// Failure modes are honest per ADR 0015 M2:
/// - `start_agent` fails → session stays at `Created`, returns
///   `CreatedHarnessFailed`. `/exec` / `/shell` / `/prompt` will return
///   409 against this state until a follow-up `/resume` succeeds.
/// - PG transition fails → returns `Err(ApiError)`.
#[tracing::instrument(name = "coord.finish_resume_to_active", skip_all, fields(session_id = %session.id))]
pub async fn finish_resume_to_active(
    state: &SharedState,
    session: &Session,
    new_sandbox_id: SandboxId,
) -> Result<FinishResumeOutcome, ApiError> {
    let id = session.id;
    // ADR 0016 §A.1.7: load the full bundle (manifest + SecretBundle
    // + env-with-placeholders) once and reuse it both for the launch
    // env below AND for the post-resume egress policy rebuild. Avoids
    // a second SecretStore round-trip on the resume hot path.
    // `resolve_session_env` folds the manifest env + secrets + the
    // per-request overrides identically to the `/exec` path.
    let (resume_bundle, resume_base_env) =
        crate::api::sessions::resolve_session_env(state, session).await;
    // ADR 0021 P1.3: resolve_harness reads the image manifest's
    // [harness] block + the session's mode, not a per-session
    // HarnessSpec. The resume bundle already loaded the manifest;
    // a None bundle (manifest fetch failed above) means we skip the
    // agent re-attach, same as the dev-VM path.
    let agent_opt = resume_bundle.as_ref().and_then(|b| {
        // Same split as create: agentd holds the durable session env (image
        // env + secrets + session id); the harness gets the forge broker
        // token as a per-spawn extra — re-minted here so it's valid even
        // after a coord restart dropped the in-memory token map.
        let mut session_env = resume_base_env.clone();
        session_env.insert("ENGRAM_SESSION_ID".into(), id.to_string());
        let mut agent = crate::api::sessions::resolve_harness(
            state,
            b.manifest.harness.as_ref(),
            session.mode,
            id,
            None,
            session_env,
            b.manifest.workdir.clone(),
        )
        .ok()
        .flatten()?;
        crate::api::sessions::inject_harness_env(
            state,
            id,
            b.manifest.git.as_ref(),
            &mut agent.env,
        );
        Some(agent)
    });
    let mut start_agent_failed = false;
    if let Some(agent) = agent_opt {
        // Rebuild the SessionEgressPolicy for the new sandbox.
        // Three failure modes fall back to the legacy placeholder so
        // we never regress to "resume errors out": (a) the manifest
        // bundle load failed above (already warn-logged); (b) the
        // host has no guest IP for this sandbox (process backend,
        // VZ in some configs — `build_resume_egress_policy` returns
        // None); (c) the IP is unparseable.
        let policy = match resume_bundle.as_ref() {
            Some(b) => crate::api::sessions::build_resume_egress_policy(
                state,
                id,
                new_sandbox_id,
                &b.bundle,
                &b.manifest,
                &b.env,
            )
            .await
            .unwrap_or_else(|| placeholder_egress_policy(id, new_sandbox_id)),
            None => placeholder_egress_policy(id, new_sandbox_id),
        };
        if let Err(e) = state
            .services
            .host
            .start_agent(new_sandbox_id, agent, policy)
            .await
        {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %new_sandbox_id,
                error = %e,
                "post-resume start_agent failed; leaving session at Created so /exec returns 409",
            );
            start_agent_failed = true;
        }
    }
    if start_agent_failed {
        return Ok(FinishResumeOutcome::CreatedHarnessFailed);
    }
    let prev_for_active = state
        .services
        .meta
        .transition_session(id, SessionState::Active)
        .await?;
    let now = Utc::now();
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: prev_for_active,
                to: SessionState::Active,
                at: now,
            },
        )
        .await?;
    Ok(FinishResumeOutcome::Active)
}

/// Portable FC blob keys (`state.bin` + sidecar) for a snapshot,
/// derived from its id via the canonical key scheme. Only a
/// memory-bearing FC snapshot uploads these to BlobStorage, so return
/// `(None, None)` for anything else — `materialize_state_if_missing`
/// hard-errors on a missing blob, so a relocated restore must not be
/// pointed at artifacts that were never uploaded (VZ disk-only /
/// pre-chunk FC). See ADR 0028 (cross-host recovery) / incident
/// 89f7984d.
fn portable_blob_keys(
    id: engram_core::types::SnapshotId,
    memory_manifest: Option<&ManifestRef>,
) -> (Option<String>, Option<String>) {
    if memory_manifest.is_some() {
        (
            Some(engram_chunk_store::snapshot_blob::state_blob_key(id)),
            Some(engram_chunk_store::snapshot_blob::sidecar_blob_key(id)),
        )
    } else {
        (None, None)
    }
}

async fn resume_from_fc_snapshot(
    state: SharedState,
    session: Session,
    record: SnapshotRecord,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;
    // ADR 0007: reconstruct the snapshot dir from
    // `(snapshot_dir, session_id, snapshot_id)`. The snapshot path
    // wrote the directory at this exact location; the chunked
    // manifests on the row carry the durable copy.
    // ADR 0007 Phase 6: coord no longer pre-resolves a local path.
    // The backend's `snapshot_path_for(record.id)` is the canonical
    // host-local reference; PooledBackend.restore wraps to
    // materialise memory.bin from chunks if it's missing. The
    // "snapshot manifest gone on disk" pre-flight that lived here
    // is now the backend's responsibility — restore() returns a
    // typed SandboxError::Snapshot on missing artifacts, which
    // we map to 410 Gone below.
    let session_for_ctx = session.clone();
    // ScheduleContext carries `(repo, tag)` for telemetry / future
    // affinity hooks; the live scheduler currently keys only on
    // snapshot affinity + capacity. With raw-URI image refs we split
    // here — `repo` becomes `host[:port]/repo[/path]`, `image_version`
    // becomes the tag.
    let (image_repo, image_tag) =
        engram_core::types::session::split_image_ref(&session_for_ctx.image);
    let ctx = ScheduleContext {
        repo: image_repo,
        image_version: image_tag,
        prefer_snapshot_id: Some(record.id),
        memory_mib: None,
        // Restore from a snapshot reuses an existing in-memory image —
        // no chunked-rootfs prefetch needed on the resume path. Snapshot
        // affinity already constrains to a host that has the bytes.
        required_image_digest: None,
        exclude_host: None,
    };
    // ADR 0016 Phase B commit 6: pick the newer of
    // `session.live_disk_manifest` and `record.disk_manifest`.
    // Without this, the first resume after Phase B's continuous
    // flush is enabled silently rolls the session back to the
    // snapshot's stale disk lineage, throwing away every flush
    // since the snapshot.
    //
    // `session.live_disk_manifest` is `None` when:
    // - The session never went through Phase B (warm-pool /
    //   non-NBD host / never had a publish land).
    // - The session is mid-eviction and `assign_session_sandbox(None)`
    //   cleared the column (commit 3's load-bearing race fix).
    //
    // In both `None` cases the resolver falls back to the
    // snapshot's manifest, preserving the pre-Phase-B behaviour.
    //
    // ADR 0028 rung-1 coherence rule: a record with a
    // `memory_manifest` is a COHERENT (memory, disk) checkpoint — its
    // restored RAM describes ITS OWN disk version. Pairing that memory
    // with a newer `live_disk_manifest` corrupts (the same Defect-B
    // incoherence the evac path guards). So when memory is present we
    // restore the checkpoint's own disk — which IS the "rewind to the
    // checkpoint" the recovery ladder prescribes; the post-checkpoint
    // flushes are deliberately discarded for coherence.
    //
    // Why this is newly load-bearing (it wasn't pre-Fix-A): without
    // periodic checkpoints the only snapshot was the eviction one,
    // whose disk == live at the pause instant — live-wins was a no-op.
    // With checkpoints, the latest *recorded* snapshot can be an
    // earlier checkpoint while live advanced (e.g. the cf4d4afd
    // eviction snapshot never recorded), so live-wins would now
    // actively mis-pair. The live-wins branch survives only for the
    // memory-less record (VZ / disk-only), where there's no RAM to be
    // incoherent with.
    let effective_disk_manifest = if record.memory_manifest.is_some() {
        record.disk_manifest
    } else {
        effective_resume_disk_manifest(session.live_disk_manifest, record.disk_manifest)
    };
    if effective_disk_manifest != record.disk_manifest {
        tracing::info!(
            session_id = %id,
            snapshot_disk_manifest = ?record.disk_manifest,
            live_disk_manifest = ?session.live_disk_manifest,
            effective_disk_manifest = ?effective_disk_manifest,
            "resume: preferred live_disk_manifest over snapshot's stale lineage (memory-less record)",
        );
    }
    // Derive the portable state/sidecar blob keys (see the field
    // comment below). Borrow memory_manifest here, before it's moved
    // into the struct.
    let (portable_state_key, portable_sidecar_key) =
        portable_blob_keys(record.id, record.memory_manifest.as_ref());
    // Build the SnapshotMetadata the trait now takes. The record
    // carries every field we need; we just round-trip it back into
    // the engine type the backend expects.
    let restore_metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: record.id,
        size_bytes: record.size_bytes,
        created_at: record.created_at,
        image_version: record.image_version.clone(),
        disk_manifest: effective_disk_manifest,
        memory_manifest: record.memory_manifest,
        // ADR 0028 cross-host recovery: a memory-bearing FC snapshot
        // uploads its VMM `state.bin` + FC sidecar to BlobStorage, so
        // pass the (deterministic, id-derived) blob keys — a resume that
        // relocates to a host WITHOUT this snapshot's local dir then
        // materializes them from GCS instead of dying on a missing
        // `manifest.json`. These were hardcoded `None` on the assumption
        // that idle-resume stays same-host; prod incident 89f7984d
        // disproved it (the self-heal fell back to a good checkpoint,
        // but the resume landed on a non-capturing host and skipped
        // materialization). `rootfs` is rebuilt from `disk_manifest`
        // chunks and `working_set` is a warm-pool-only prefetch hint, so
        // both stay None.
        source_sandbox_id: None,
        state_blob_key: portable_state_key,
        sidecar_blob_key: portable_sidecar_key,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        // ADR 0035: resume keeps the pinned generations (no swap); the
        // host materializes any the receiving host is missing.
        aux_bundles: record.aux_bundles.clone(),
    };
    let (host_id, new_sandbox_id) = match state
        .host_registry
        .restore_for_session(&ctx, restore_metadata)
        .await
    {
        Ok(v) => v,
        Err(SandboxError::Snapshot(msg)) => {
            // Lost local artifacts on every viable host — chunked
            // restore couldn't rehydrate from the manifest either.
            // Mark Dead + surface the same affordance the missing-
            // manifest preflight used to return.
            tracing::warn!(
                session_id = %id,
                error = %msg,
                "snapshot restore failed; marking session Dead",
            );
            let _ = state
                .services
                .meta
                .transition_session(id, SessionState::Dead)
                .await;
            return Err(ApiError::Gone(
                "snapshot_invalidated: session can't be revived; \
                 use `engram session fork <id>` to continue from the workspace"
                    .into(),
            ));
        }
        Err(e) => return Err(ApiError::from(e)),
    };
    bind_resumed_session(&state, id, host_id, new_sandbox_id).await;
    // ADR 0015 M2: resume re-runs the create-shape transitions on
    // the new sandbox — Idle → Created (now that a host + sandbox
    // are bound). [`finish_resume_to_active`] handles the rest
    // (harness rebuild + → Active) so the same primitive is shared
    // with evac (commits land in `dead_host.rs`, the admin endpoint,
    // and the new `/resume from Created` arm).
    let now = Utc::now();
    let prev_for_created = state
        .services
        .meta
        .transition_session(id, SessionState::Created)
        .await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: prev_for_created,
                to: SessionState::Created,
                at: now,
            },
        )
        .await?;
    // ADR 0028 A.log: this is a rung-1 restore (coherent memory
    // checkpoint) — rewind the transcript to the checkpoint's cursor
    // before the harness comes back, so the resumed agent's first
    // events append after an honest recovery boundary, not after
    // messages it never made. No-op for a checkpoint that was the head.
    // ADR 0045 F1: the manual `/resume` path only rewinds when the
    // checkpoint lags the lost live head — an unplanned host-death case.
    apply_rung1_rewind(
        &state,
        id,
        record.events_cursor,
        RecoveryCause::HostFailureRecovery,
    )
    .await;
    // Refresh the session row so finish_resume_to_active sees the
    // freshly-bound host_id + sandbox_id (the caller might've raced
    // a concurrent writer between bind_resumed_session and now).
    let session_refreshed = state.services.meta.get_session(id).await?;
    let outcome = finish_resume_to_active(&state, &session_refreshed, new_sandbox_id).await?;
    match outcome {
        FinishResumeOutcome::CreatedHarnessFailed => Ok(SnapshotResponse {
            session_id: id,
            snapshot_id: Some(record.id.to_string()),
            size_bytes: Some(record.size_bytes),
            note: "resumed; harness reattach failed — session left in Created",
        }),
        FinishResumeOutcome::Active => {
            state
                .emit(
                    id,
                    SessionEvent::Resumed {
                        snapshot_id: record.id,
                        at: Utc::now(),
                    },
                )
                .await?;
            Ok(SnapshotResponse {
                session_id: id,
                snapshot_id: Some(record.id.to_string()),
                size_bytes: Some(record.size_bytes),
                note: "resumed from snapshot",
            })
        }
    }
}

async fn bind_resumed_session(
    state: &SharedState,
    id: SessionId,
    host_id: engram_core::HostId,
    sandbox_id: SandboxId,
) {
    if let Err(e) = state
        .services
        .meta
        .assign_session_host(id, Some(host_id))
        .await
    {
        tracing::warn!(
            session_id = %id,
            host_id = %host_id,
            error = %e,
            "assign_session_host on resume failed; HostRegistry routing still works",
        );
    }
    if let Err(e) = state
        .services
        .meta
        .assign_session_sandbox(id, Some(sandbox_id))
        .await
    {
        tracing::warn!(
            session_id = %id,
            sandbox_id = %sandbox_id,
            error = %e,
            "assign_session_sandbox on resume failed; live routing still works (in-memory only)",
        );
    }
    bind_session_routing(state, id, sandbox_id).await;
}

/// ADR 0018 commit 10: coord-side session→sandbox cache update +
/// host-agent-side `bind_session` RPC. Both are critical for
/// post-relocate routing:
///
/// - `state.registry.bind(id, sandbox_id)` keeps `/exec` /
///   `/shell` / `/prompt` handlers (which look up sandbox_id by
///   session_id via this in-memory map) pointing at the new
///   sandbox. Without it, the next /exec dispatches to the OLD
///   sandbox_id, hits `host_for_sandbox` returning None (PG was
///   rebound), and 404s.
/// - `host.bind_session(id, sandbox_id)` registers the
///   session→sandbox mapping on the target host-agent. This is
///   what the in-VM adapter's vsock-accept path uses to route
///   reconnects, and what the FlushScheduler's live-manifest
///   publisher uses to attach session_id to the publish RPC.
///
/// Shared with `bind_resumed_session` (the /resume path); exposed
/// `pub(crate)` so the admin evac endpoint and the `evac_resumer`
/// scanner (driving operator-drained sessions) can fire the same shape.
pub(crate) async fn bind_session_routing(
    state: &SharedState,
    id: SessionId,
    sandbox_id: SandboxId,
) {
    state.registry.bind(id, sandbox_id);
    state.services.host.bind_session(id, sandbox_id).await;
}

pub async fn evict_local(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    if session.status != SessionState::Active {
        return Err(ApiError::Conflict(format!(
            "session is {} — only Active sessions can be evicted",
            session.status.as_str()
        )));
    }

    // Refuse to evict if there's no snapshot to come back to. Without
    // this check the in-flight state would just be lost when the
    // sandbox gets destroyed.
    if state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .is_none()
    {
        return Err(ApiError::Conflict(
            "no snapshot exists for this session — take a snapshot before evicting".into(),
        ));
    }

    if let Some(sandbox_id) = state.registry.unbind(id) {
        if let Err(e) = state.services.host.destroy(sandbox_id).await {
            // Best-effort: even if destroy fails we drop the binding
            // and mark Idle. The sandbox is the cache, not source of truth.
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox destroy failed during evict_local; continuing",
            );
        }
        // ADR 0006: the host-agent unregisters its local proxy
        // entry as part of `destroy`. No coordinator-side cleanup.
    }

    // Clear the persisted sandbox_id so a coordinator restart
    // doesn't repopulate routing for a sandbox that no longer
    // exists. host_id stays so resume's snapshot affinity still
    // prefers the same host.
    let _ = state.services.meta.assign_session_sandbox(id, None).await;
    let prev = state
        .services
        .meta
        .transition_session(id, SessionState::Idle)
        .await?;
    let now = Utc::now();
    state.emit(id, SessionEvent::Evicted { at: now }).await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: prev,
                to: SessionState::Idle,
                at: now,
            },
        )
        .await?;
    Ok(StatusCode::ACCEPTED)
}

/// ADR 0009 Phase 2: HEAD-verify the chunked manifests are durable
/// in BlobStorage. Returns `true` only when every present manifest
/// ref responds HEAD-ok; a backend that produced no manifests at
/// all (process; legacy) returns `false` so a sandbox-loss
/// reconcile transitions the session to `Dead` rather than promising
/// an Idle/resume path that can't be delivered.
///
/// Transient blob outages flip the column false, which means the
/// session would Dead-on-sandbox-loss instead of Idle. That's the
/// safe direction: a snapshot that was uploaded a few seconds ago
/// but failed HEAD here may still be recoverable, but until the
/// chunk-store GC actually reaps it the column will be re-flipped
/// to true on the next snapshot. Idle-when-not-actually-recoverable
/// is the worse failure (user clicks resume, gets `blob not found`).
pub async fn verify_snapshot_recoverable(
    blob: &(dyn BlobStorage + 'static),
    disk: Option<&ManifestRef>,
    memory: Option<&ManifestRef>,
) -> bool {
    let mut any_present = false;
    if let Some(r) = disk {
        any_present = true;
        match blob.head(&r.storage_key()).await {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    manifest_key = %r.storage_key(),
                    error = %e,
                    "disk manifest HEAD failed; snapshot recorded as not-recoverable"
                );
                return false;
            }
        }
    }
    if let Some(r) = memory {
        any_present = true;
        match blob.head(&r.storage_key()).await {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    manifest_key = %r.storage_key(),
                    error = %e,
                    "memory manifest HEAD failed; snapshot recorded as not-recoverable"
                );
                return false;
            }
        }
    }
    any_present
}

/// ADR 0034 durability: resume-time re-verification that a snapshot's
/// backing artifacts still exist before we commit to restoring from it.
/// Distinct from `verify_snapshot_recoverable` (which runs at capture
/// time and only checks the chunked manifests): this also HEADs the
/// portable `state.bin` + `sidecar.json` blobs a memory-bearing FC
/// snapshot restores from — exactly the blobs the eviction/checkpoint
/// abort path deletes (prod incident 89f7984d), and which the stored
/// `recoverable` flag does not re-check once it went stale. A few HEADs,
/// cheap relative to the chunk prefetch a doomed restore would waste.
async fn snapshot_artifacts_present(
    blob: &(dyn BlobStorage + 'static),
    record: &SnapshotRecord,
) -> bool {
    // The eviction/checkpoint abort race only deletes a memory-bearing
    // FC snapshot's portable artifacts (state.bin/sidecar); the chunk
    // manifests are content-addressed and pin-protected. A capture with
    // no memory manifest (disk-only VZ, or a Process/local snapshot that
    // restores from its on-host dir) is not subject to this failure mode
    // and resumes as it always has — don't second-guess it here, or
    // we'd wrongly skip a perfectly good local snapshot.
    let Some(memory) = record.memory_manifest.as_ref() else {
        return true;
    };
    // Memory-bearing FC snapshot: re-verify the chunk manifests AND the
    // portable state.bin + sidecar blobs the abort path deletes — the
    // 89f7984d failure was exactly "manifests survived, state/sidecar
    // did not, but the row still said recoverable=true".
    if !verify_snapshot_recoverable(blob, record.disk_manifest.as_ref(), Some(memory)).await {
        return false;
    }
    for key in [
        engram_chunk_store::snapshot_blob::state_blob_key(record.id),
        engram_chunk_store::snapshot_blob::sidecar_blob_key(record.id),
    ] {
        if let Err(e) = blob.head(&key).await {
            tracing::warn!(
                snapshot_id = %record.id,
                key = %key,
                error = %e,
                "resume: portable snapshot artifact missing in BlobStorage",
            );
            return false;
        }
    }
    true
}

#[cfg(test)]
mod recoverable_tests {
    use super::*;
    use engram_storage_local::LocalBlobStorage;
    use std::sync::Arc;
    use uuid::Uuid;

    fn make_blob() -> (Arc<LocalBlobStorage>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Arc::new(LocalBlobStorage::new(dir.path())), dir)
    }

    fn make_ref() -> ManifestRef {
        ManifestRef {
            manifest_id: Uuid::new_v4(),
            version: 1,
        }
    }

    #[tokio::test]
    async fn returns_false_when_no_manifests_present() {
        // Process backend produces neither disk nor memory manifest.
        // Phase 2's contract: no manifests → reconcile flips to Dead
        // on sandbox loss, not Idle (no chunked artifact to resume).
        let (blob, _g) = make_blob();
        let r = verify_snapshot_recoverable(blob.as_ref(), None, None).await;
        assert!(!r);
    }

    #[tokio::test]
    async fn returns_true_when_both_manifests_head_ok() {
        let (blob, _g) = make_blob();
        let disk = make_ref();
        let mem = make_ref();
        // Seed the manifest keys with arbitrary bytes; HEAD only
        // cares about existence + size, not content.
        blob.put(&disk.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        blob.put(&mem.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        let r = verify_snapshot_recoverable(blob.as_ref(), Some(&disk), Some(&mem)).await;
        assert!(r, "both manifests durable → recoverable=true");
    }

    #[tokio::test]
    async fn returns_false_when_disk_manifest_missing() {
        let (blob, _g) = make_blob();
        let disk = make_ref();
        // Don't seed; HEAD will fail.
        let r = verify_snapshot_recoverable(blob.as_ref(), Some(&disk), None).await;
        assert!(!r);
    }

    #[tokio::test]
    async fn returns_false_when_memory_manifest_missing_with_present_disk() {
        let (blob, _g) = make_blob();
        let disk = make_ref();
        let mem = make_ref();
        blob.put(&disk.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        // mem not seeded.
        let r = verify_snapshot_recoverable(blob.as_ref(), Some(&disk), Some(&mem)).await;
        assert!(
            !r,
            "any missing manifest disqualifies recoverable (partial recovery is worse than Dead)"
        );
    }

    #[tokio::test]
    async fn returns_true_for_disk_only_when_memory_absent_by_design() {
        // VZ disk-only snapshots intentionally have no memory manifest.
        // recoverable should still be true if the disk manifest is durable.
        let (blob, _g) = make_blob();
        let disk = make_ref();
        blob.put(&disk.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        let r = verify_snapshot_recoverable(blob.as_ref(), Some(&disk), None).await;
        assert!(r, "disk-only snapshots are recoverable on cold-boot");
    }

    fn make_record(
        id: engram_core::types::SnapshotId,
        disk: Option<ManifestRef>,
        memory: Option<ManifestRef>,
    ) -> SnapshotRecord {
        SnapshotRecord {
            id,
            session_id: None,
            host_id: None,
            image_version: "test:v1".into(),
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            last_accessed_at: chrono::Utc::now(),
            disk_manifest: disk,
            memory_manifest: memory,
            recoverable: true,
            aux_bundles: Vec::new(),
            events_cursor: None,
        }
    }

    #[tokio::test]
    async fn artifacts_present_true_when_manifests_and_state_sidecar_seeded() {
        let (blob, _g) = make_blob();
        let id = engram_core::types::SnapshotId::new();
        let disk = make_ref();
        let mem = make_ref();
        blob.put(&disk.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        blob.put(&mem.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        blob.put(
            &engram_chunk_store::snapshot_blob::state_blob_key(id),
            b"x".to_vec().into(),
        )
        .await
        .unwrap();
        blob.put(
            &engram_chunk_store::snapshot_blob::sidecar_blob_key(id),
            b"{}".to_vec().into(),
        )
        .await
        .unwrap();
        let rec = make_record(id, Some(disk), Some(mem));
        assert!(snapshot_artifacts_present(blob.as_ref(), &rec).await);
    }

    #[tokio::test]
    async fn artifacts_present_false_when_sidecar_blob_deleted() {
        // The 89f7984d failure mode: the chunked manifests survive
        // (content-addressed, shared), but the abort path deleted the
        // per-snapshot state.bin/sidecar — so the `recoverable = true`
        // row is stale and the resume would die reading manifest.json.
        let (blob, _g) = make_blob();
        let id = engram_core::types::SnapshotId::new();
        let disk = make_ref();
        let mem = make_ref();
        blob.put(&disk.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        blob.put(&mem.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        // state.bin present but sidecar deliberately NOT seeded.
        blob.put(
            &engram_chunk_store::snapshot_blob::state_blob_key(id),
            b"x".to_vec().into(),
        )
        .await
        .unwrap();
        let rec = make_record(id, Some(disk), Some(mem));
        assert!(
            !snapshot_artifacts_present(blob.as_ref(), &rec).await,
            "a missing sidecar blob must disqualify the snapshot even though its \
             manifests survive — this is what lets resume fall back to the prior checkpoint",
        );
    }

    #[tokio::test]
    async fn artifacts_present_skips_state_sidecar_for_disk_only() {
        // A disk-only snapshot (no memory manifest) takes the cold-boot
        // path and never materializes the FC state.bin/sidecar, so they
        // must not be required.
        let (blob, _g) = make_blob();
        let id = engram_core::types::SnapshotId::new();
        let disk = make_ref();
        blob.put(&disk.storage_key(), b"{}".to_vec().into())
            .await
            .unwrap();
        let rec = make_record(id, Some(disk), None);
        assert!(
            snapshot_artifacts_present(blob.as_ref(), &rec).await,
            "disk-only snapshot must not require FC state/sidecar blobs",
        );
    }

    #[test]
    fn portable_blob_keys_set_only_for_memory_bearing() {
        let id = engram_core::types::SnapshotId::new();
        // Memory-bearing FC snapshot → derive the portable state/sidecar
        // keys so a relocated resume materializes them from BlobStorage.
        let m = make_ref();
        let (s, sc) = portable_blob_keys(id, Some(&m));
        assert!(s.as_deref().unwrap().ends_with("/state.bin"), "{s:?}");
        assert!(s.as_deref().unwrap().contains(&id.to_string()), "{s:?}");
        assert!(sc.as_deref().unwrap().ends_with("/sidecar.json"), "{sc:?}");
        // Disk-only / non-FC → None, so a relocated restore doesn't chase
        // state/sidecar artifacts that were never uploaded (materialize
        // hard-errors on a missing blob).
        assert_eq!(portable_blob_keys(id, None), (None, None));
    }
}

#[cfg(test)]
mod effective_resume_disk_manifest_tests {
    use super::*;
    use engram_core::types::manifest::ManifestRef;
    use uuid::Uuid;

    fn mref(version: u64) -> ManifestRef {
        ManifestRef {
            manifest_id: Uuid::new_v4(),
            version,
        }
    }

    /// Same `manifest_id`, live is newer → resolver picks live.
    /// This is the load-bearing case: post-snapshot continuous
    /// flushes (commit 4's FlushScheduler) advance the chain past
    /// what the snapshot row captured.
    #[test]
    fn live_newer_than_snapshot_wins() {
        let mid = Uuid::new_v4();
        let live = ManifestRef {
            manifest_id: mid,
            version: 7,
        };
        let snap = ManifestRef {
            manifest_id: mid,
            version: 5,
        };
        assert_eq!(
            effective_resume_disk_manifest(Some(live), Some(snap)),
            Some(live),
        );
    }

    /// Live and snapshot at the same version → snapshot wins (or
    /// equivalently, both are valid; the resolver's defensive
    /// `>` not `>=` keeps the choice deterministic). No real flush
    /// landed between snapshot creation and now.
    #[test]
    fn live_equal_to_snapshot_picks_snapshot() {
        let mid = Uuid::new_v4();
        let live = ManifestRef {
            manifest_id: mid,
            version: 5,
        };
        let snap = ManifestRef {
            manifest_id: mid,
            version: 5,
        };
        assert_eq!(
            effective_resume_disk_manifest(Some(live), Some(snap)),
            Some(snap),
        );
    }

    /// Live BEHIND snapshot (live=3, snap=5). Possible if the live
    /// column wasn't refreshed before the snapshot wrote, OR a
    /// stale publish landed pre-Phase-3's unbind-clear (defensive).
    /// Snapshot wins.
    #[test]
    fn live_behind_snapshot_picks_snapshot() {
        let mid = Uuid::new_v4();
        let live = ManifestRef {
            manifest_id: mid,
            version: 3,
        };
        let snap = ManifestRef {
            manifest_id: mid,
            version: 5,
        };
        assert_eq!(
            effective_resume_disk_manifest(Some(live), Some(snap)),
            Some(snap),
        );
    }

    /// Different manifest_ids → snapshot wins. Defensive: live
    /// can't supersede a snapshot belonging to a different
    /// chunked-disk lineage; the (memory, disk) pair the user
    /// paused at is what we restore.
    #[test]
    fn different_manifest_ids_picks_snapshot() {
        let live = mref(99);
        let snap = mref(1);
        assert_eq!(
            effective_resume_disk_manifest(Some(live), Some(snap)),
            Some(snap),
        );
    }

    /// No live publish — never had Phase B running, or unbind
    /// cleared it. Snapshot is the only signal.
    #[test]
    fn live_none_picks_snapshot() {
        let snap = mref(2);
        assert_eq!(effective_resume_disk_manifest(None, Some(snap)), Some(snap),);
    }

    /// No snapshot manifest — legacy snapshot row or a Phase-B-
    /// only recovery path (M4.1 evacuation, future). Live wins.
    #[test]
    fn snapshot_none_picks_live() {
        let live = mref(4);
        assert_eq!(effective_resume_disk_manifest(Some(live), None), Some(live),);
    }

    /// Both None — no chunked-disk lineage on either side.
    /// Resolver returns None; resume falls through to the legacy
    /// materialize/flat-file path (commit 5's else branch).
    #[test]
    fn both_none_passes_through() {
        assert_eq!(effective_resume_disk_manifest(None, None), None);
    }
}

#[cfg(test)]
mod evicting_gate_tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::SandboxBackend;
    use engram_core::types::session::SessionMode;
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn build_state_for_session(session: Session) -> (SharedState, TempDir) {
        let local = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(local.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
            engram_host_agent::LocalHostClient::with_noop_hub(backend.clone()),
        );
        host_registry.register(engram_core::HostId::new(), local_host);
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path: local.path().to_path_buf(),
            ..CoordinatorConfig::default()
        };
        let state = Arc::new(AppState::new_with_registry(cfg, services, host_registry));
        (state, local)
    }

    fn evicting_session(id: SessionId) -> Session {
        Session {
            id,
            user_id: None,
            status: SessionState::Evicting,
            host_id: None,
            sandbox_id: Some(SandboxId::new()),
            image: "test/repo:evicting-gate".into(),
            mode: SessionMode::Agent,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            live_disk_manifest: None,
        }
    }

    /// ADR 0034 / ADR 0039 follow-up #20: a prompt/exec arriving
    /// mid-eviction that does NOT settle within the hold window falls
    /// back to the retryable 409 — NOT the Idle|Evacuating auto-resume
    /// arm (the sandbox may still be live; a restore would race the
    /// pipeline). A `ZERO` hold exercises the immediate-409 fallback
    /// deterministically (the pre-follow-up behaviour, preserved).
    #[tokio::test]
    async fn ensure_active_during_evicting_falls_back_to_retryable_conflict() {
        let id = SessionId::new();
        let (state, _local) = build_state_for_session(evicting_session(id));

        let err = ensure_active_after_evicting_hold_for(&state, id, Duration::ZERO)
            .await
            .expect_err("Evicting that never settles must not pass ensure_active");
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        assert!(
            err.to_string().contains("mid-eviction"),
            "fallback must be the honest retryable mid-eviction 409, got: {err}",
        );
        // The session must be untouched — in particular NOT resumed
        // and NOT transitioned.
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Evicting);
    }

    /// ADR 0039 follow-up #20: a request arriving mid-eviction HOLDS
    /// for the eviction to land the session at Idle, then auto-resumes
    /// inline instead of bouncing a 409 the client has to retry later
    /// (prod stranded a follow-up message 4m32s). We flip the session
    /// `Evicting → Idle` from a concurrent task partway through the
    /// hold; `ensure_active_after_evicting_hold_for` must observe the
    /// flip and dispatch the standard resume path (NOT return the
    /// Evicting "retry shortly" 409). With no snapshot seeded, that
    /// resume legitimately fails `Gone` (snapshot_invalidated) and
    /// marks the session Dead — which is the proof the hold released
    /// into resume rather than 409-bouncing.
    #[tokio::test]
    async fn ensure_active_during_evicting_holds_then_resumes_when_settled() {
        let id = SessionId::new();
        let (state, _local) = build_state_for_session(evicting_session(id));

        // Concurrently land the eviction at Idle shortly into the hold.
        let flip_state = state.clone();
        let flipper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            flip_state
                .services
                .meta
                .transition_session(id, SessionState::Idle)
                .await
                .expect("Evicting → Idle is a legal transition");
        });

        // Generous hold so the flip is observed well before the bound.
        let res = ensure_active_after_evicting_hold_for(&state, id, Duration::from_secs(5)).await;
        flipper.await.unwrap();

        // The hold must have dispatched to resume_session — proven by
        // the resume's own error path, NOT the Evicting 409.
        let err = res.expect_err("no snapshot seeded → resume fails Gone");
        assert!(
            !err.to_string().contains("mid-eviction"),
            "must have left the Evicting hold and taken the resume path, got: {err}",
        );
        assert!(
            err.to_string().contains("snapshot_invalidated"),
            "expected the no-snapshot resume failure, got: {err}",
        );
        // resume_from_idle with no recoverable snapshot marks the
        // session Dead — confirming the resume path actually ran.
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Dead);
    }

    /// ADR 0039 follow-up #20: if the eviction races to a TERMINAL
    /// state during the hold (here `Evicting → Completed` via a
    /// concurrent DELETE), the hold must re-dispatch through
    /// `ensure_active` and surface that state's honest typed error —
    /// the terminal "no work to dispatch" 409 — rather than the
    /// misleading "mid-eviction; retry shortly" message.
    #[tokio::test]
    async fn ensure_active_during_evicting_surfaces_terminal_state_on_race() {
        let id = SessionId::new();
        let (state, _local) = build_state_for_session(evicting_session(id));

        let flip_state = state.clone();
        let flipper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            flip_state
                .services
                .meta
                .transition_session(id, SessionState::Completed)
                .await
                .expect("Evicting → Completed is a legal transition");
        });

        let err = ensure_active_after_evicting_hold_for(&state, id, Duration::from_secs(5))
            .await
            .expect_err("Completed session has no work to dispatch");
        flipper.await.unwrap();

        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        assert!(
            err.to_string().contains("terminal"),
            "must surface the terminal-state error, not the mid-eviction 409, got: {err}",
        );
        assert!(
            !err.to_string().contains("mid-eviction"),
            "must not still report mid-eviction once the row went terminal, got: {err}",
        );
    }

    /// A direct /resume mid-eviction gets the same honest 409.
    #[tokio::test]
    async fn resume_during_evicting_is_retryable_conflict() {
        let id = SessionId::new();
        let (state, _local) = build_state_for_session(evicting_session(id));

        let err = match resume(State(state.clone()), Path(id)).await {
            Err(e) => e,
            Ok(_) => panic!("Evicting must not resume"),
        };
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Evicting);
    }

    /// DELETE mid-eviction: Evicting → Completed is legal, and the
    /// eviction scanner's racing pipeline then fails its own
    /// transition against the terminal row and exits via the abort
    /// path — the session stays Completed.
    #[tokio::test]
    async fn delete_during_evicting_completes_and_pipeline_backs_off() {
        let id = SessionId::new();
        let (state, _local) = build_state_for_session(evicting_session(id));
        let sandbox_id = state
            .services
            .meta
            .get_session(id)
            .await
            .unwrap()
            .sandbox_id
            .unwrap();

        let code = crate::api::sessions::delete_session(State(state.clone()), Path(id))
            .await
            .expect("delete mid-eviction");
        assert_eq!(code, StatusCode::NO_CONTENT);
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Completed);

        // The racing pipeline (a scanner tick that already swept the
        // row) backs off harmlessly: delete_session unbound the
        // registry, so the pipeline's registry guard short-circuits
        // to an idempotent no-op before touching the (gone) sandbox.
        // (If it had already passed the guard, its later
        // transition_session(Idle) would fail legality against the
        // terminal row and exit via abort_inflight_snapshot — that
        // arm is covered by the idle_evictor abort tests.) Either
        // way the terminal state is untouched.
        crate::idle_evictor::evict_idle_session(&state, id, sandbox_id)
            .await
            .expect("registry-guard no-op");
        let still = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(still.status, SessionState::Completed);
    }
}
