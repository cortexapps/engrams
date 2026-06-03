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
use crate::state::{SessionEvent, SharedState};

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
    };
    state.services.meta.record_snapshot(record).await?;

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
        // loser anyway, but only after a wasted restore attempt). The
        // eviction lands at Idle within a scanner tick or two, after
        // which the next call auto-resumes.
        SessionState::Evicting => Err(ApiError::Conflict(
            "session is mid-eviction; retry shortly (it will land at idle and auto-resume)"
                .into(),
        )),
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
    let record = state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .ok_or_else(|| {
            tracing::warn!(
                session_id = %id,
                "resume requested but no snapshot row found — marking Dead",
            );
            ApiError::Gone(
                "snapshot_invalidated: session can't be revived; \
                 use `engram session fork <id>` to continue"
                    .into(),
            )
        })?;
    let _ = transition_to_dead_if_no_snapshot(&state, id).await;
    resume_from_fc_snapshot(state, session, record).await
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

/// ADR 0018 commit 10 — the "session is bound on a new host at
/// `Created`, finish bringing it back to `Active`" primitive shared
/// across every code path that drops a session into `Created` with a
/// fresh sandbox bound:
///
/// - [`resume_from_fc_snapshot`] (user-initiated `/resume` from Idle).
/// - [`crate::api::admin::evacuate_session`] (operator drain via the
///   admin endpoint).
/// - [`crate::dead_host::evict_host`] (auto-evac on heartbeat loss).
/// - [`crate::nbd_loss_trigger::process_unhealthy`] (auto-evac on
///   NBD degradation).
/// - [`resume_from_created`] dispatcher arm (manual recovery of an
///   auto-evac'd session).
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
        crate::api::sessions::inject_forge_env(state, id, b.manifest.git.as_ref(), &mut agent.env);
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
    let effective_disk_manifest =
        effective_resume_disk_manifest(session.live_disk_manifest, record.disk_manifest);
    if effective_disk_manifest != record.disk_manifest {
        tracing::info!(
            session_id = %id,
            snapshot_disk_manifest = ?record.disk_manifest,
            live_disk_manifest = ?session.live_disk_manifest,
            effective_disk_manifest = ?effective_disk_manifest,
            "resume: preferred live_disk_manifest over snapshot's stale lineage",
        );
    }
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
        // ADR 0014: portable-snapshot refs aren't yet plumbed onto
        // SnapshotRecord — the existing idle-resume path stays
        // same-host. Warm-pool restore will carry these via gRPC
        // request fields (M1.5), bypassing the SnapshotRecord
        // shape.
        source_sandbox_id: None,
        state_blob_key: None,
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
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
/// `pub(crate)` so the admin evac endpoint and the dead_host.rs /
/// nbd_loss_trigger auto-trigger paths can fire the same shape.
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
