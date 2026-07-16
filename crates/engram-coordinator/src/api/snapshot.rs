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

use engram_core::traits::storage::BlobStorage;
use engram_core::traits::SessionFence;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{Session, SessionState};
use engram_core::{MetaError, SandboxError, SandboxId, SessionId};
use serde::Serialize;
use tracing::Instrument;

use crate::error::ApiError;
use crate::placement::ScheduleContext;
use crate::state::{RecoveryCause, SessionEvent, SharedState};

/// ADR 0079: how long the wire-compat entry points (`/resume`,
/// `ensure_active`, `EvictIdle`, `/local`) OBSERVE a just-enqueued op
/// before returning the retryable "in flight" 409. The op keeps running
/// regardless — observation is read-only; a timeout loses nothing but
/// the caller's synchronous answer.
const OP_OBSERVE_TIMEOUT: Duration = Duration::from_secs(90);

/// Poll cadence for the bounded observe.
const OP_OBSERVE_POLL: Duration = Duration::from_millis(250);

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
        allow_all: false,
        secrets: vec![],
        injects: vec![],
        observes: vec![],
        secret_mode: engram_core::types::image::SecretMode::Broker,
    }
}

/// Build the resume-shape `AgentSpec` + `SessionEgressPolicy` to (re)attach a
/// harness to `sandbox_id` for `session`. Loads the image manifest bundle +
/// secrets ONCE and reuses it for both the spec and the policy.
///
/// The spec is **resume-shaped**: `resolve_harness(prompt = None)`. Prompt-less
/// is load-bearing — the initial prompt rides the harness env, so a boot-shape
/// respawn of an *exited* harness would re-inject it mid-conversation; the
/// resume shape just `--resume`s the existing claude session and goes `Idle`.
///
/// Shared by [`finish_resume_to_active`] (a fresh post-restore sandbox) and the
/// ADR 0034 Track A desync watchdog's in-place reattach (the session's existing
/// LIVE sandbox). `None` when the manifest bundle can't load (dev-VM / process
/// backend) — callers skip the agent attach, exactly as resume did before.
pub(crate) async fn resolve_resume_agent_and_policy(
    state: &SharedState,
    session: &Session,
    sandbox_id: SandboxId,
) -> Option<(
    engram_core::types::sandbox::AgentSpec,
    engram_core::types::egress::SessionEgressPolicy,
)> {
    let id = session.id;
    // ADR 0016 §A.1.7: load manifest + SecretBundle + env once; reused for the
    // launch env AND the egress policy (avoids a second SecretStore round-trip).
    let (resume_bundle, resume_base_env) =
        crate::api::sessions::resolve_session_env(state, session).await;
    let b = resume_bundle.as_ref()?;
    // Same split as create: agentd holds the durable session env (image env +
    // secrets + session id); the harness gets the forge broker token as a
    // per-spawn extra, from the PG-sealed row (ADR 0047) — same token across
    // coord restarts and replicas.
    let mut session_env = resume_base_env.clone();
    session_env.insert("ENGRAM_SESSION_ID".into(), id.to_string());
    // ADR 0062: the harness comes from the session's persisted selection (not the
    // baked manifest). The dyn_0 catalog mount is re-anchored from the eviction
    // snapshot's aux_bundles, so we use only the AgentSpec here (the argv still
    // points at /opt/engram/dyn/0/<name>/<exec>); the returned mount is dropped.
    let selected_harness = state
        .services
        .meta
        .get_session_harness(id)
        .await
        .ok()
        .flatten();
    let (mut agent, _harness_mount) = crate::api::sessions::resolve_harness(
        state,
        selected_harness.as_deref(),
        session.mode,
        id,
        session_env,
        b.config.workdir.clone(),
    )
    .await
    .ok()
    .flatten()?;
    crate::api::sessions::inject_harness_env(state, id, &mut agent.env).await;
    // ADR 0073: stamp the CURRENT epoch (this runs after the flow's
    // bind — minted for fresh-spawn resumes, unminted for live moves,
    // where the surviving harness must keep validating).
    agent.binding_epoch = state
        .services
        .meta
        .current_binding_epoch(id)
        .await
        .unwrap_or(0);
    // Rebuild the SessionEgressPolicy for `sandbox_id`. Falls back to the
    // legacy placeholder when the host has no guest IP (process backend, VZ in
    // some configs) or the IP is unparseable — same as the create path.
    let policy =
        crate::api::sessions::build_resume_egress_policy(state, id, sandbox_id, &session.image)
            .await
            .unwrap_or_else(|| placeholder_egress_policy(id, sandbox_id));
    Some((agent, policy))
}

/// Re-establish a session's harness on its EXISTING live sandbox without a
/// teardown — the shared primitive behind the ADR 0034 Track A desync watchdog
/// AND the inline delivery self-heal (a prompt/answer that hit a harness-unbound
/// `SandboxError::NotFound` — the sandbox VM is alive and `/exec` works, but no
/// harness is attached for run delivery; the ADR 0045 C1 "prompts 'sandbox not
/// found' while exec works" desync).
///
/// Re-issues the resume-shape `start_agent`, which drives agentd's ADR 0045 C1
/// reattach arm: SIGUSR1 a live-but-wedged harness to drop + re-dial, or
/// reap + respawn an exited one. Non-destructive — a live in-flight run is
/// preserved (the SIGUSR1 only re-dials the host connection).
///
/// - `Ok(true)`  — reattach issued (`start_agent` succeeded). The harness
///   re-dials and re-binds shortly after; callers that need the binding live
///   (delivery) must poll-retry, since the live-harness arm returns as soon as
///   the SIGUSR1 is sent, not when the re-dial lands.
/// - `Ok(false)` — nothing to do: the session moved off `sandbox_id` since the
///   caller resolved it (a concurrent evict/resume re-bound it — we must never
///   `start_agent` a stale sandbox), or there's no agent to attach (dev-VM /
///   process backend, no manifest bundle).
/// - `Err(_)`    — host/meta error.
///
/// `fence`: the deliver op threads its real epoch (ADR 0079); an out-of-op
/// caller would pass `SessionFence::unfenced()`.
pub(crate) async fn reattach_harness_in_place(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
    fence: SessionFence,
) -> Result<bool, ApiError> {
    let session = state.services.meta.get_session(session_id).await?;
    // Re-confirm the session is still bound to the sandbox the caller resolved —
    // a concurrent eviction/resume may have moved it, and we must never re-issue
    // start_agent against a stale or unbound sandbox.
    if session.sandbox_id != Some(sandbox_id) {
        return Ok(false);
    }
    let Some((agent, policy)) = resolve_resume_agent_and_policy(state, &session, sandbox_id).await
    else {
        return Ok(false);
    };
    state
        .services
        .host
        .start_agent(sandbox_id, agent, policy, fence)
        .await?;
    Ok(true)
}

#[derive(Serialize)]
pub struct SnapshotResponse {
    pub session_id: SessionId,
    pub snapshot_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub note: &'static str,
}

/// ADR 0051: transport-agnostic snapshot core (gRPC `Snapshot` + axum
/// `/snapshot`). Spawn-detached and recoverable-flip semantics preserved
/// from the legacy handler; ADR 0079 replaced the session-lease fence
/// with an inline op-log claim (`checkpoint_finalize` — the manual
/// snapshot becomes a real verb when the ADR 0069/0077 finalize
/// machinery migrates).
pub(crate) async fn snapshot_core(
    state: &SharedState,
    id: SessionId,
) -> Result<SnapshotResponse, ApiError> {
    state.services.meta.get_session(id).await?;

    let sandbox_id = state.resolve_sandbox(id).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to snapshot — create or resume first".into(),
        )
    })?;

    // Issue #213 / ADR 0079: serialize the manual snapshot against the
    // eviction pipeline, resumes, and live migration via the op-log
    // claim (one running op per session). Without it, an idle-nominated
    // eviction snapshotting and destroying THIS sandbox could race the
    // manual capture, and the record→commit pair below could interleave
    // with another pipeline's `abort_prior_inflight_snapshot`.
    let claim = crate::session_ops::OpClaim::try_acquire(
        state,
        id,
        engram_core::types::session_op::OpKind::CheckpointFinalize,
        serde_json::json!({ "flavor": "manual_snapshot" }),
    )
    .await
    .map_err(|e| ApiError::Internal(format!("op claim acquire failed: {e}")))?
    .ok_or_else(|| {
        ApiError::Conflict(format!(
            "session {id} is busy (an op is in flight — mid-snapshot, mid-resume, or \
             mid-eviction); retry shortly",
        ))
    })?;

    // Issue #213: DETACH the record→commit body from the cancellable
    // request future. The original handler ran inline in the request task,
    // so a client disconnect (CLI Ctrl-C) between `record_snapshot` and
    // `commit_snapshot` dropped the commit future: the PG row was durable
    // but the host-side snapshot was never committed. The next periodic
    // checkpoint tick then `abort_prior_inflight_snapshot`s and deletes
    // this snapshot's `state.bin`/sidecar within one interval, leaving a
    // `recoverable = true` row pointing at nothing — the exact 89f7984d
    // phantom-snapshot state. Mirror the op-executor pattern: run the
    // body in a `tokio::spawn`ed task that MOVES the op claim in, so the
    // capture always runs to a terminal arm regardless of the request's
    // fate, and the handler only awaits the JoinHandle to relay the result.
    // ADR 0019 / telemetry restoration (#526): re-parent this detached
    // capture body onto the request span (`Span::current()` at spawn time)
    // so it's a child of the manual-capture trace instead of an orphaned
    // root — span context only, the #213 detach-for-cancellation-safety
    // task lifetime is unchanged.
    let st = state.clone();
    let capture_span = tracing::Span::current();
    let handle = tokio::spawn(
        async move {
            let claim = claim;
            let heartbeat = claim.spawn_heartbeat("capture");
            let fence = claim.fence();
            let result = async {
                // ADR 0007 Phase 6: backend owns its staging dir. Coord no
                // longer pre-allocates a path — the backend's
                // `snapshot_path_for(metadata.id)` is the canonical reference
                // for the on-disk location. Cross-host durability flows
                // through the chunked manifests on `SnapshotMetadata`, not
                // through the local path.
                // ADR 0065: the in-guest browser stack is ephemeral and must never be
                // frozen into a snapshot (Chrome RAM + dead sockets on resume). Reap it
                // best-effort before the capture; the next EnsureBrowser re-lazy-starts it.
                let _ = st.services.host.stop_browser(sandbox_id).await;
                // ADR 0085: same for the IDE — a snapshotted code-server's listeners
                // resurrect wedged after restore (issue #567's lesson). Reap it
                // best-effort; the next EnsureIde re-lazy-starts it.
                let _ = st.services.host.stop_ide(sandbox_id).await;
                let metadata = st.services.host.snapshot(sandbox_id, fence).await?;

                let now = st.services.clock.now_utc();
                // Record the host that wrote this snapshot to its local disk so
                // the resume path's snapshot-affinity scheduler can route back to
                // it (zero-cost hot-tier hit). ADR 0007: durability lives in the
                // chunk store (`disk_manifest` / `memory_manifest`); the local dir
                // is a per-host cache the same-host fast-resume reads from.
                let host_id = st.host_registry.host_of(sandbox_id);
                let events_cursor = st
                    .services
                    .meta
                    .latest_event_idx_at_or_before(id, now)
                    .await
                    .unwrap_or_default();
                // ADR 0068: stamp the capturing host's FC snapshot-version so a
                // later restore can be paired against it at placement — same
                // best-effort lookup the idle-evictor's periodic-checkpoint path
                // uses (`idle_evictor.rs`). A lookup failure degrades to NULL
                // (unconstrained restore, today's behavior), never fails the
                // snapshot over it.
                let fc_snapshot_version = match host_id {
                    Some(h) => st
                        .services
                        .meta
                        .fc_snapshot_version_for_host(h)
                        .await
                        .unwrap_or_default(),
                    None => None,
                };
                // Issue #213: write the row `recoverable = false` FIRST, then flip
                // to true only after `commit_snapshot` succeeds. This makes the
                // phantom state ("PG says recoverable but the blobs were never
                // committed / got aborted") unrepresentable: even if this task is
                // somehow torn down between `record_snapshot` and the post-commit
                // flip, the worst residue is a `recoverable = false` row, which
                // resume / reconcile already treat as "don't trust" and skip — the
                // safe direction. The previous code computed `recoverable` from a
                // pre-commit HEAD and stored true before committing, so a dropped
                // commit left a `recoverable = true` row over uncommitted artifacts.
                let record = SnapshotRecord {
                    id: metadata.id,
                    session_id: Some(id),
                    host_id,
                    image_version: metadata.image_version.clone(),
                    size_bytes: metadata.size_bytes,
                    created_at: metadata.created_at,
                    last_accessed_at: now,
                    // ADR 0007: chunked manifests are the durability primitive.
                    // FC backends produce both fields via the PooledBackend wrap;
                    // VZ produces disk_manifest only; Process produces neither.
                    disk_manifest: metadata.disk_manifest,
                    memory_manifest: metadata.memory_manifest,
                    recoverable: false,
                    // ADR 0035: pin the generations this snapshot's device model
                    // references (host-reported; reflects any fresh-create swap).
                    aux_bundles: metadata.aux_bundles.clone(),
                    // ADR 0028 A.log: best-effort cursor at the capture instant
                    // (the guest pauses inside the snapshot RPC; sub-second skew
                    // accepted, documented on `latest_event_idx_at_or_before`).
                    events_cursor,
                    fc_snapshot_version,
                };
                st.services.meta.record_snapshot(record.clone()).await?;

                // ADR 0034 durability: the live sandbox keeps running, so commit
                // the host's in-flight snapshot now — otherwise the periodic
                // checkpoint driver's next tick calls `abort_prior_inflight_snapshot`
                // and deletes this snapshot's state.bin/sidecar from BlobStorage
                // within one interval.
                let committed = match st.services.host.commit_snapshot(sandbox_id, fence).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(
                            session_id = %id,
                            sandbox_id = %sandbox_id,
                            error = %e,
                            "snapshot: commit_snapshot failed; leaving snapshot row \
                             recoverable=false (resume-time verification would catch it anyway)",
                        );
                        false
                    }
                };

                // Issue #213: flip `recoverable=true` ONLY after the artifacts are
                // committed AND HEAD-verifiable. ADR 0009 Phase 2: HEAD-verify the
                // chunked manifests are durable in BlobStorage before promoting.
                // Backends that produced no manifest (Process; VZ memory) stay
                // false, which means a sandbox-loss reconcile will Dead them —
                // correct, since there's no chunked artifact to resume from.
                if committed {
                    let recoverable = verify_snapshot_recoverable(
                        st.services.blob.as_ref(),
                        metadata.disk_manifest.as_ref(),
                        metadata.memory_manifest.as_ref(),
                    )
                    .await;
                    if recoverable {
                        let mut promoted = record;
                        promoted.recoverable = true;
                        // ADR 0079 (re-review findings #3/#4): promote UNDER OUR
                        // FENCE. This manual-snapshot claim can be reclaimed if a
                        // coord↔PG partition outlasts `RECLAIM_STALE` mid-capture;
                        // a plain insert would then land a phantom `recoverable=true`
                        // row a resume could pick (the 89f7984d durability-lie
                        // class). `Ok(false)` = fenced out → leave the row
                        // recoverable=false; the successor op owns the session.
                        match st
                            .services
                            .meta
                            .fenced_record_snapshot(promoted, fence.epoch as i64)
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                crate::metrics::note_fenced_write();
                                tracing::info!(
                                    session_id = %id,
                                    snapshot_id = %metadata.id,
                                    "snapshot: fenced by a successor op before promote; \
                                     leaving row recoverable=false and stopping",
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    session_id = %id,
                                    snapshot_id = %metadata.id,
                                    error = %e,
                                    "snapshot: failed to promote recoverable=true after commit; \
                                     row stays recoverable=false (resume-time verification will \
                                     re-promote on the next capture)",
                                );
                            }
                        }
                    }
                }

                st.emit(
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
                Ok::<_, ApiError>(SnapshotResponse {
                    session_id: id,
                    snapshot_id: Some(metadata.id.to_string()),
                    size_bytes: Some(metadata.size_bytes),
                    note: "snapshot recorded; live sandbox still running",
                })
            }
            .await;
            drop(heartbeat);
            match &result {
                Ok(_) => {
                    claim
                        .finish(engram_core::types::session_op::OpState::Done, None)
                        .await
                }
                Err(e) => {
                    claim
                        .finish(
                            engram_core::types::session_op::OpState::Failed,
                            Some(&e.to_string()),
                        )
                        .await
                }
            }
            result
        }
        .instrument(capture_span),
    );

    handle.await.map_err(|join_err| {
        ApiError::Internal(format!("snapshot pipeline task panicked: {join_err}"))
    })?
}

/// ADR 0051: transport-agnostic resume core (gRPC `Resume` + axum
/// `/resume`). ADR 0079: enqueues the resume verb on the session op log
/// and bounded-observes the row for wire compatibility — the op is the
/// durable owner of the pipeline; this handler only relays its outcome.
pub(crate) async fn resume_core(
    state: &SharedState,
    id: SessionId,
) -> Result<SnapshotResponse, ApiError> {
    let observed = enqueue_and_observe_resume(state, id).await?;
    Ok(SnapshotResponse {
        session_id: id,
        snapshot_id: None,
        size_bytes: None,
        note: match observed {
            ObservedResume::Active => "resumed",
            ObservedResume::CreatedHarnessFailed => {
                "resume attempted; harness reattach failed — session left in Created"
            }
        },
    })
}

/// What the bounded observe saw the resume op land at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservedResume {
    Active,
    /// The op completed but the harness rebuild failed — the session is
    /// parked at `Created` (the `/exec`-409 resting contract); a
    /// follow-up `/resume` retries from there.
    CreatedHarnessFailed,
}

/// Map a failed op's recorded error back to a wire `ApiError`. The
/// verbs tag terminal-gone failures with a `gone:` prefix
/// (`session_verbs::outcome_from_api_error`); everything else is the
/// retryable 409 the pre-op entry points returned.
fn api_error_from_op_failure(msg: &str) -> ApiError {
    match msg.strip_prefix("gone: ") {
        Some(rest) => ApiError::Gone(rest.to_string()),
        None => ApiError::Conflict(msg.to_string()),
    }
}

/// ADR 0079: the shared resume entry point — enqueue the resume verb
/// (it queues BEHIND any in-flight op for the session: ordering by log,
/// not poll) and observe the row for up to [`OP_OBSERVE_TIMEOUT`].
pub(crate) async fn enqueue_and_observe_resume(
    state: &SharedState,
    id: SessionId,
) -> Result<ObservedResume, ApiError> {
    enqueue_and_observe_resume_for(state, id, OP_OBSERVE_TIMEOUT).await
}

/// Inner [`enqueue_and_observe_resume`] with the observe bound passed
/// explicitly so tests can drive the timeout arm deterministically.
pub(crate) async fn enqueue_and_observe_resume_for(
    state: &SharedState,
    id: SessionId,
    timeout: Duration,
) -> Result<ObservedResume, ApiError> {
    use engram_core::types::session_op::{EnqueueOutcome, OpKind};
    let outcome =
        crate::session_ops::enqueue(state, id, OpKind::Resume, serde_json::json!({}), None)
            .await
            .map_err(|e| ApiError::Internal(format!("resume enqueue failed: {e}")))?;
    let op_id = match outcome {
        EnqueueOutcome::Claimed(op) | EnqueueOutcome::Queued(op) => Some(op.id),
        // Key-less enqueues never dedup; defensive arm only.
        EnqueueOutcome::Duplicate => None,
    };
    observe_resume_op(state, id, op_id, timeout).await
}

/// The observe half: poll the op row (and the session status, which can
/// settle Active before the row read) until terminal or the bound.
async fn observe_resume_op(
    state: &SharedState,
    id: SessionId,
    op_id: Option<i64>,
    timeout: Duration,
) -> Result<ObservedResume, ApiError> {
    use engram_core::types::session_op::OpState;
    let deadline = state.services.clock.now_mono() + timeout;
    loop {
        if let Some(op_id) = op_id {
            if let Ok(Some(op)) = state.services.meta.op_get(op_id).await {
                match op.state {
                    OpState::Done => {
                        // Done = Active, already-Active, or the honest
                        // harness-failed park at Created.
                        let session = state.services.meta.get_session(id).await?;
                        return match session.status {
                            SessionState::Active => Ok(ObservedResume::Active),
                            SessionState::Created => Ok(ObservedResume::CreatedHarnessFailed),
                            other => Err(ApiError::Conflict(format!(
                                "resume op completed but the session is {} — a concurrent \
                                 op moved it; retry",
                                other.as_str()
                            ))),
                        };
                    }
                    OpState::Failed => {
                        return Err(api_error_from_op_failure(
                            op.error.as_deref().unwrap_or("resume failed"),
                        ));
                    }
                    OpState::Cancelled => {
                        return Err(ApiError::Conflict(
                            "resume was cancelled; retry if still needed".into(),
                        ));
                    }
                    OpState::Queued | OpState::Running => {}
                }
            }
        } else {
            // No row to watch (Duplicate) — settle on the status alone.
            let session = state.services.meta.get_session(id).await?;
            if session.status == SessionState::Active {
                return Ok(ObservedResume::Active);
            }
        }
        if state.services.clock.now_mono() >= deadline {
            return Err(ApiError::Conflict(
                "resume in flight (op enqueued); retry shortly".into(),
            ));
        }
        tokio::time::sleep(OP_OBSERVE_POLL).await;
    }
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
/// agentd readiness. `Created` (session still mid-create or
/// mid-resume; harness not yet running) returns 409;
/// `HostLost` / `Dead` are unrecoverable from this entry point and
/// return 410; terminal `Completed` / `Failed` return 409 (no work
/// is left to dispatch).
pub async fn ensure_active(state: &SharedState, id: SessionId) -> Result<(), ApiError> {
    let session = state.services.meta.get_session(id).await?;
    match session.status {
        SessionState::Active => {
            // ADR 0074 rung-2 backstop: parked-paused now uniformly means
            // `Evicting` (the park branch transitions the admin path's
            // Active entry too), so Active + `park_rung == 2` only occurs
            // in the crash window between the host `pause` landing and the
            // park's PG bookkeeping committing. Returning Ok would
            // advertise a live session over a PAUSED VM and stall the next
            // prompt on a frozen harness — un-pause it first. No-op for
            // the common not-parked Active session.
            if session.park_rung == 2 {
                let _ = try_cancel_nominated_eviction(state, id).await?;
            }
            Ok(())
        }
        SessionState::Idle => {
            // Snapshotted, sandbox destroyed, ready to resume. Enqueue
            // the resume verb and observe (ADR 0079) — the op executor
            // owns the pipeline; this request only relays the outcome.
            enqueue_and_observe_resume(state, id).await?;
            Ok(())
        }
        // ADR 0091: the guest stopped answering on the control plane
        // (host alive, VM dead/wedged). Forwarding work into it would
        // hang; the honest dispatch is the same checkpoint recovery a
        // dead host gets — the resume verb's Unreachable arm destroys
        // the dead sandbox and re-enters the Idle resume path.
        SessionState::Unreachable => {
            enqueue_and_observe_resume(state, id).await?;
            Ok(())
        }
        // ADR 0018: operator drain / live teleport. Unlike Idle, an
        // Evacuating session is NOT inline-resumable here: `resume_session`
        // has no Evacuating arm (it would 409), and `resume_from_idle`'s
        // rebind CAS only accepts Idle. Recovery is asynchronous — the
        // `evac_resumer` scanner (ADR 0018 commit 12) relocates the session
        // to the destination host and drives it `Evacuating → Created →
        // Active`. Return a retryable 409 (like Queued/Pending) so the
        // client polls /sessions/:id/events for the flip, rather than the
        // misleading "only Idle / Created sessions can be resumed".
        SessionState::Evacuating => Err(ApiError::Conflict(
            "session is relocating (operator drain / teleport); it will \
             resume automatically — retry shortly."
                .into(),
        )),
        // ADR 0034 / ADR 0079: mid-eviction. The sandbox may still be
        // live (the evict op is capturing it), so this is explicitly NOT
        // the auto-resume arm above.
        SessionState::Evicting => {
            // ADR 0074 rung 1: try the one-write cancel first — the
            // nomination window is exactly when the VM is untouched and
            // the user most often returns. This also cancels any QUEUED
            // evict op and flags a RUNNING one for cooperative cancel.
            if try_cancel_nominated_eviction(state, id).await? {
                return Ok(());
            }
            // A running evict op owns the session: enqueue the resume —
            // it queues BEHIND the evict op (ordering by log, replacing
            // the retired ADR 0039 hold-then-poll) — and observe.
            enqueue_and_observe_resume(state, id).await?;
            Ok(())
        }
        SessionState::Created => Err(ApiError::Conflict(format!(
            "session is {} — agentd is not yet ready. \
             Wait for the session to reach Active (subscribe to /sessions/:id/events) \
             and retry.",
            session.status.as_str()
        ))),
        SessionState::Pending => Err(ApiError::Conflict(
            "session is pending — scheduling has not completed".into(),
        )),
        // ADR 0048: accepted but waiting for fleet capacity. Retryable —
        // the queue scanner drives it to Active (or Idle, then resume)
        // once a host frees up / scales in; the client polls
        // /sessions/:id/events for the flip, same as Pending.
        SessionState::Queued => Err(ApiError::Conflict(
            "session is queued — waiting for host capacity (the fleet is \
             scaling up). It will resume automatically once placed."
                .into(),
        )),
        SessionState::HostLost => Err(ApiError::HostLost(
            "session's host went away; resume from a snapshot if one exists".into(),
        )),
        SessionState::Dead => Err(ApiError::Gone(
            "session is dead — chunked manifests are gone or never existed".into(),
        )),
        // Terminal is GONE, not Conflict: a 409 reads as "retry later" to
        // every caller, and the outbox driver in particular deferred a
        // completed session's un-acked rows every backoff-tick forever
        // (prod: 3 rows spinning the driver for hours). Gone maps to the
        // driver's Terminal arm — the row is dropped — and to an honest
        // 410 for exec/upload/relay callers.
        SessionState::Failed | SessionState::Completed => Err(ApiError::Gone(format!(
            "session is {} (terminal); no work to dispatch",
            session.status.as_str()
        ))),
    }
}

/// ADR 0074 rung 1: cancel a NOMINATED eviction — the returning user's
/// prompt un-nominates instead of waiting for a healthy VM to be
/// destroyed and rebuilt. Returns `Ok(true)` when the session is Active
/// again (VM untouched, delivery may proceed immediately).
///
/// ADR 0079: the op log is the fence. A QUEUED evict op is cancelled
/// outright (the nomination window — nothing has touched the VM); a
/// RUNNING op means the capture owns the session, so we set its
/// cooperative cancel flag and return `Ok(false)` — the caller enqueues
/// its resume BEHIND the running op (ordering by log). The evict verb's
/// own entry guard (nominated ops re-check `status == Evicting`) makes
/// the Active flip below authoritative even against an op that claims
/// in the window between our cancel and the transition.
pub(crate) async fn try_cancel_nominated_eviction(
    state: &SharedState,
    id: SessionId,
) -> Result<bool, ApiError> {
    use engram_core::types::session_op::{OpKind, OpState};
    let meta = &state.services.meta;
    // Cancel any still-queued evict ops first (rung 1 proper). This also
    // frees the op lane so the short-lived ascent claim below can land
    // (the exclusive claim requires no queued op).
    let _ = meta.op_cancel_queued(id, OpKind::Evict).await;

    // ADR 0079 (review finding #2): the ascent MUST run under a REAL op
    // claim, not the old `op_running_for == None` probe + unfenced
    // `ascend_evicting_to_active`. That probe→ascend window let an evict
    // op inline-claim in between (a park-reaper descent, the scanner
    // backstop, a fresh nomination) and race the resurrection: a
    // successor could resurrect Idle→Active with a NULL sandbox (→ the
    // deliver Active-arm loops "no live sandbox" forever, wedged) or the
    // un-pause could hit a VM mid-capture. Holding a short-lived
    // `OpClaim` makes the ascent mutually exclusive with any evict op (the
    // `session_ops_one_running` index), and its `host.resume` + `Active`
    // transition carry the claim's REAL fence. We use `Resume` as the
    // claim kind so a holder death mid-ascent is re-drivable by the
    // reclaim sweep as a full resume (not a terminal-fail inline kind).
    match crate::session_ops::OpClaim::try_acquire(
        state,
        id,
        OpKind::Resume,
        serde_json::json!({ "flavor": "rung_ascent" }),
    )
    .await
    {
        Ok(Some(claim)) => {
            let res = ascend_evicting_to_active(state, id, claim.fence()).await;
            match &res {
                Ok(_) => claim.finish(OpState::Done, None).await,
                Err(e) => claim.finish(OpState::Failed, Some(&e.to_string())).await,
            }
            res
        }
        // Busy: an op is running (an evict capturing, or another
        // resume/ascent). The VM is owned — flag a running evict for
        // cooperative cancel (the rung-1 window) and report "not ascended"
        // so the caller enqueues its resume BEHIND the running op.
        Ok(None) => {
            let _ = meta.op_request_cancel_running(id, OpKind::Evict).await;
            Ok(false)
        }
        Err(e) => {
            tracing::debug!(session_id = %id, error = %e, "rung-ascent claim failed");
            Ok(false)
        }
    }
}

/// The ADR 0074 rung-1/rung-2 ascent core: cancel any still-queued evict
/// ops, un-pause a parked-paused VM in place, clear the park stamp, and
/// flip the session back to `Active`. The CALLER guarantees no *foreign*
/// running op owns the session — either it probed `op_running_for` first
/// (the out-of-op wire path, [`try_cancel_nominated_eviction`], which
/// passes `SessionFence::unfenced()`) or it IS the running op (the
/// deliver verb, ADR 0079) and threads its own fence, which also stamps
/// the un-pause host RPC.
pub(crate) async fn ascend_evicting_to_active(
    state: &SharedState,
    id: SessionId,
    fence: SessionFence,
) -> Result<bool, ApiError> {
    use engram_core::types::session_op::OpKind;
    // The user is back: any queued evict op (nomination, rung descent)
    // is stale. Idempotent with the caller's own cancel.
    let _ = state
        .services
        .meta
        .op_cancel_queued(id, OpKind::Evict)
        .await;
    // ADR 0074 rung 2 (parked-paused ascent): if this session was parked
    // by PAUSING the VM in place (park_rung == 2), the sandbox is still
    // bound and alive — un-pause it BEFORE flipping the row back to
    // Active so the harness is running the instant the caller resumes.
    // Only commit the Active transition if the un-pause succeeds; a
    // failed un-pause leaves the session Evicting for the reaper/normal
    // resume path rather than advertising Active over a paused VM.
    let row = state.services.meta.get_session(id).await.ok();
    let parked_paused = row.as_ref().map(|r| r.park_rung).unwrap_or(0) == 2;
    if parked_paused {
        if let Some(sandbox_id) = row.as_ref().and_then(|r| r.sandbox_id) {
            if let Err(e) = state.services.host.resume(sandbox_id, fence).await {
                tracing::warn!(session_id = %id, %sandbox_id, error = %e,
                    "rung-2 ascent: un-pause failed; leaving session Evicting for the standard resume path");
                return Ok(false);
            }
        }
        // Clear the park stamp the moment the un-pause lands, NOT in the
        // transition's Ok arm: the `ensure_active` Active-arm backstop
        // un-parks a session that is already Active (the pause-then-crash
        // window before the park's Evicting transition), and Active→Active
        // below is a same-state Conflict — tying the clear to the Ok arm
        // left `park_rung=2` advertised forever over a running VM. Fenced
        // by the caller's epoch (review finding #6): every ascent caller
        // now holds a real op fence (the deliver verb, or the wire path's
        // short-lived OpClaim), so a fenced-out ascent can't wipe a
        // successor's rung.
        let _ = state
            .services
            .meta
            .fenced_set_session_park_rung(id, fence.epoch as i64, 0, None)
            .await;
        ::metrics::counter!(crate::metrics::EVICTION_UNPARKED_PAUSED_TOTAL).increment(1);
    }
    let result =
        match crate::session_ops::transition_with_fence(state, id, fence, SessionState::Active)
            .await
        {
            Ok(prev) => {
                ::metrics::counter!(crate::metrics::EVICTION_CANCELLED_TOTAL).increment(1);
                tracing::info!(
                    session_id = %id,
                    park_rung = if parked_paused { 2 } else { 1 },
                    "eviction cancelled — the user came back before/at the parking rung",
                );
                let _ = state
                    .emit_fenced(
                        id,
                        fence,
                        crate::state::SessionEvent::StatusChanged {
                            from: prev,
                            to: SessionState::Active,
                            at: state.services.clock.now_utc(),
                        },
                    )
                    .await;
                Ok(true)
            }
            // Raced out of Evicting between our caller's read and the cancel
            // (e.g. the evict op finished to Idle first), or — on the fenced
            // path — a successor op re-claimed the session (the `fenced:`
            // Conflict). Not an error; the caller re-dispatches on the fresh
            // status.
            Err(engram_core::MetaError::Conflict(_)) => Ok(false),
            Err(e) => Err(ApiError::Internal(format!("cancel evict: {e}"))),
        };
    result
}

// ADR 0079: `resume_session` (the lease-acquire + spawn-detach + inline
// dispatcher) is GONE. The dispatch lives in `session_verbs::resume`;
// the pipeline functions below are owned exclusively by that verb and
// take its `OpCtx` for step recording + fencing. The op row is the
// durable owner of the pipeline — there is no wire-lifetime future to
// detach from and no lease heartbeat to keep alive.

/// ADR 0018 commit 10: finish bringing an auto-evac'd session back
/// to Active. Preconditions: session at `Created` with `host_id` +
/// `sandbox_id` already bound (the evac primitive did the bind +
/// HostLost → Created drive). All this needs to do is run the harness
/// rebuild + the Created → Active transition.
pub(crate) async fn resume_from_created(
    ctx: &crate::session_ops::OpCtx<'_>,
    session: Session,
) -> Result<SnapshotResponse, ApiError> {
    let state = ctx.state;
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
    if !ctx.step("finish").await {
        return Err(fenced_error());
    }
    let outcome = finish_resume_to_active(state, &session, sandbox_id, true, ctx.fence()).await?;
    let note = match outcome {
        FinishResumeOutcome::Active => "resumed from Created (auto-evac completion)",
        // ADR 0090: retryable, not a 200 — the resume op's backoff +
        // budget own the retry; Done here re-enqueued fresh ops forever.
        FinishResumeOutcome::CreatedHarnessFailed(e) => {
            return Err(ApiError::Unavailable(format!(
                "harness start failed after resume from Created (will retry): {e}"
            )));
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

/// The op-fenced `Conflict` a pipeline leg returns when a `ctx.step`
/// records 0 rows — a successor re-claimed the op and this executor must
/// stop silently. The verb maps it to Retry, whose requeue write is
/// itself epoch-fenced into a no-op, so the sentinel's exact shape is
/// inert; the `fenced:` marker is for logs.
fn fenced_error() -> ApiError {
    ApiError::Conflict("fenced: a successor op re-claimed this session".into())
}

pub(crate) async fn resume_from_idle(
    ctx: &crate::session_ops::OpCtx<'_>,
    session: Session,
) -> Result<SnapshotResponse, ApiError> {
    let state = ctx.state;
    let id = session.id;

    // ADR 0079 note: the issue-#210 residual-sandbox destroy that lived
    // here is DELETED. A crash between the restore and the bind now
    // resumes at the op's recorded step: step >= "bind" with a live
    // binding skips straight to the finish leg (see
    // `session_verbs::resume`), and a restore whose bind never committed
    // leaves a VM no session row references — the host's ownership-
    // oracle orphan reap GCs it. The compensation (and its blind destroy
    // of a possibly-healthy VM) is unrepresentable under step-resume.

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
            return resume_from_fc_snapshot(ctx, session, record).await;
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
        return resume_disk_only_cold_boot(ctx, session).await;
    }
    tracing::warn!(
        session_id = %id,
        "resume requested but no snapshot row had recoverable artifacts — marking Dead",
    );
    let _ = transition_to_dead_if_no_snapshot(state, id).await;
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
    ctx: &crate::session_ops::OpCtx<'_>,
    session: Session,
) -> Result<SnapshotResponse, ApiError> {
    use crate::evacuation::{evacuate_dead_source, resolve_cold_boot_spec, EvacError};

    let state = ctx.state;
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
    let origin = relocatable.host_id.take();

    // A fresh VM comes up inside `evacuate_dead_source` — record the
    // restore step first (crash-resume boundary).
    if !ctx.step("restore").await {
        return Err(fenced_error());
    }
    let receipt = evacuate_dead_source(
        &state.host_registry,
        &state.services.meta,
        relocatable,
        None,
        Some(spec),
        None,
        // ADR 0045 C2 (E2B fold, origin affinity): prefer the host the
        // session last ran on — its NBD chunk cache (and base shm) are
        // warm there. Soft tier-2: loses to capacity/draining, so this
        // never strands the resume.
        origin,
        ctx.fence(),
        state.services.clock.now_utc(),
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
                at: state.services.clock.now_utc(),
            },
        )
        .await;

    if !ctx.step("bind").await {
        return Err(fenced_error());
    }
    bind_session_routing_minted(state, id, receipt.new_sandbox_id).await;
    if !ctx.step("finish").await {
        return Err(fenced_error());
    }
    let refreshed = state.services.meta.get_session(id).await?;
    let outcome =
        finish_resume_to_active(state, &refreshed, receipt.new_sandbox_id, true, ctx.fence())
            .await?;
    let note = match outcome {
        FinishResumeOutcome::Active => {
            "resumed via disk-only cold boot (fresh kernel on latest disk; in-RAM context lost)"
        }
        // ADR 0090: retryable — see FinishResumeOutcome. This is the exact
        // arm behind campaign B1's wedge (relocated onto a fresh node whose
        // bundle staging lacked the harness; the loop never surfaced).
        FinishResumeOutcome::CreatedHarnessFailed(e) => {
            return Err(ApiError::Unavailable(format!(
                "harness spawn failed after disk-only relocation (will retry): {e}"
            )));
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
                        at: state.services.clock.now_utc(),
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

/// ADR 0019 / telemetry restoration (#526): classify a resume's
/// placement against the snapshot record's capturing host — the
/// `engram_session_resume_total{placement=...}` label. Pure so it's
/// unit-testable without a live scheduler.
fn resume_placement_label(
    record_host_id: Option<engram_core::HostId>,
    chosen_host_id: engram_core::HostId,
) -> &'static str {
    match record_host_id {
        Some(prior) if prior == chosen_host_id => "same_host",
        Some(_) => "cross_host",
        None => "unknown_prior_host",
    }
}

/// Outcome of [`finish_resume_to_active`]. The session is either
/// fully back at `Active` (`Active`) or the rebuilt VM is bound at
/// `Created` because the harness rebuild failed (`CreatedHarnessFailed`,
/// carrying the start_agent error). ADR 0090: callers must surface the
/// failed case as a RETRYABLE error (`ApiError::Unavailable`) so the
/// resume op's backoff + `RESUME_MAX_ATTEMPTS` budget engage — mapping
/// it to a 200/`Done` made the Deliver verb enqueue a fresh Resume op
/// each round: an unbounded, backoff-free spawn loop (5/sec measured,
/// 2026-07-11 campaign B1) that never surfaced to the user.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum FinishResumeOutcome {
    Active,
    CreatedHarnessFailed(String),
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
                at: state.services.clock.now_utc(),
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
    // Emit the terminal StatusChanged here? The live-migration verb
    // suppresses it and emits a single `evacuating -> active` itself —
    // the `created -> active` hop is an internal FSM step there, not a
    // user-meaningful state (the session can't take messages yet).
    emit_status: bool,
    // ADR 0079: the caller's op fence — the resume verb / evac claim /
    // teleport claim thread their epoch so the session-row transitions
    // below are fenced against a successor re-claim. Epoch 0 (no op)
    // takes the plain legality CAS.
    fence: SessionFence,
) -> Result<FinishResumeOutcome, ApiError> {
    let id = session.id;
    // ADR 0016 §A.1.7: load the full bundle (manifest + SecretBundle
    // + env-with-placeholders) once and reuse it both for the launch
    // env below AND for the post-resume egress policy rebuild. Avoids
    // a second SecretStore round-trip on the resume hot path.
    // `resolve_session_env` folds the manifest env + secrets + the
    // per-request overrides identically to the `/exec` path.
    // Build the resume-shape AgentSpec + egress policy and (re)attach the
    // harness to the restored sandbox. `resolve_resume_agent_and_policy`
    // (shared with the ADR 0034 Track A in-place reattach) returns `None`
    // when the manifest bundle can't load (dev-VM / process backend) — we
    // skip the agent re-attach then, same as before.
    let mut start_agent_failed: Option<String> = None;
    if let Some((agent, policy)) =
        resolve_resume_agent_and_policy(state, session, new_sandbox_id).await
    {
        if let Err(e) = state
            .services
            .host
            .start_agent(new_sandbox_id, agent, policy, fence)
            .await
        {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %new_sandbox_id,
                error = %e,
                "post-resume start_agent failed; leaving session at Created so /exec returns 409",
            );
            start_agent_failed = Some(e.to_string());
        }
    }
    if let Some(err) = start_agent_failed {
        // ADR 0077 phase 4: with the Created limbo removed from the
        // happy path, the direct-resume caller arrives here at Idle, so
        // a start_agent failure must PARK the session at Created
        // explicitly (the /exec-409 resting contract). Idempotent for
        // the evac/dead-host callers that pre-transitioned to Created.
        // The park EMITS its StatusChanged like every other transition —
        // the event log is the UI's view, and a silent park left clients
        // offering Idle actions (prompt/resume) the API then 409s. A
        // transition failure PROPAGATES: reporting CreatedHarnessFailed
        // for a row that never left Idle would double the divergence.
        if session.status == SessionState::Idle {
            crate::session_ops::transition_with_fence(state, id, fence, SessionState::Created)
                .await
                .map_err(|e| {
                    ApiError::Internal(format!(
                        "resume: parking harness-failed session at Created failed: {e}"
                    ))
                })?;
            let _ = state
                .emit_fenced(
                    id,
                    fence,
                    SessionEvent::StatusChanged {
                        from: SessionState::Idle,
                        to: SessionState::Created,
                        at: state.services.clock.now_utc(),
                    },
                )
                .await;
        }
        return Ok(FinishResumeOutcome::CreatedHarnessFailed(err));
    }
    let prev_for_active =
        crate::session_ops::transition_with_fence(state, id, fence, SessionState::Active).await?;
    if emit_status {
        let now = state.services.clock.now_utc();
        // Review finding #6: fenced. Ok(None) (a successor re-claimed) is
        // not an error — the transition above committed under our epoch.
        state
            .emit_fenced(
                id,
                fence,
                SessionEvent::StatusChanged {
                    from: prev_for_active,
                    to: SessionState::Active,
                    at: now,
                },
            )
            .await?;
    }
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

/// ADR 0045 D4: the IMAGE's base-snapshot memory manifest for a session's
/// image — the CANONICAL ref for substrate resumes (base-identical pages
/// CONTINUE against the shared per-image base shm). Best-effort: any miss
/// (image disabled, no base snapshot, no memory manifest) returns `None`
/// and the restore falls back to canonical == session (pre-D4 behavior).
pub(crate) async fn base_memory_manifest_for_image(
    state: &SharedState,
    image_uri: &str,
) -> Option<engram_core::types::manifest::ManifestRef> {
    let enabled = state
        .services
        .meta
        .get_enabled_image_any(image_uri)
        .await
        .ok()
        .flatten()?;
    let base_id = enabled.base_snapshot_id?;
    state
        .services
        .meta
        .get_snapshot(base_id)
        .await
        .ok()
        .flatten()?
        .memory_manifest
}

async fn resume_from_fc_snapshot(
    op_ctx: &crate::session_ops::OpCtx<'_>,
    session: Session,
    record: SnapshotRecord,
) -> Result<SnapshotResponse, ApiError> {
    let state = op_ctx.state;
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
    // ADR 0078: feed the tier-0 RAM/CPU veto real budgets instead of
    // `None` (capacity-blind), so "the snapshot-host is hard-full" is a
    // real veto rather than a vibe. Best-effort: if the session's spec
    // can't be resolved (image un-enabled, etc.) we fall back to the
    // pre-0072 soft `None` posture — the tier-0 DISK veto (the primary
    // locality signal) still fires regardless.
    let resume_budget = crate::evacuation::resolve_cold_boot_spec(&state.services.meta, &session)
        .await
        .map(|spec| (spec.memory.max_mib, spec.cpu.vcpus));
    let ctx = ScheduleContext {
        repo: image_repo,
        image_version: image_tag,
        // ADR 0078: authoritative affinity — the host that holds this
        // snapshot's chunks (PG `snapshots.host_id`), NOT the dead
        // `local_snapshots` mirror. `None` if the row's host was deleted.
        snapshot_host: record.host_id,
        memory_mib: resume_budget.map(|(mib, _)| mib),
        cpu_budget_vcpus: resume_budget.map(|(_, vcpus)| vcpus),
        // Restore from a snapshot reuses an existing in-memory image —
        // no chunked-rootfs prefetch needed on the resume path. Snapshot
        // affinity already constrains to a host that has the bytes.
        required_image_digest: None,
        exclude_host: None,
        // ADR 0045 D4 / ADR 0039: soft preference (tier 2) for the host whose
        // chunk cache + per-image base shm are warm — the capturing
        // host first, else wherever the session last ran. Ranked below
        // the authoritative `snapshot_host` tier 0.
        prefer_host: record.host_id.or(session.host_id),
        // ADR 0068: a memory-manifest snapshot restores via the FC UFFD
        // substrate; a candidate host must also match the snapshot's
        // capture-time `fc_snapshot_version` when both are known — the
        // cross-`SNAPSHOT_VERSION` restore-corruption class this issue
        // closes at placement instead of at guest-boot failure.
        caps: crate::placement::CapabilityRequirements {
            needs_uffd_substrate: record.memory_manifest.is_some(),
            fc_snapshot_version: record.fc_snapshot_version.clone(),
        },
        // ADR 0090: prefer hosts already staging the snapshot's pinned
        // aux generations (soft — see rank_hosts).
        prefer_bundles: record.aux_bundles.as_slice(),
    };

    // ADR 0048 C7: if NO host can take this resume (the fleet is fully
    // cordoned for a scale-down wave, or scaled to zero), QUEUE it
    // (Idle → queued) instead of erroring. The queue scanner resumes it
    // once capacity returns / the fleet scales up. Only triggers on an
    // empty candidate set — a present-but-full fleet still soft-picks
    // (the pre-existing ADR 0046 resume-isn't-reserved posture).
    if matches!(session.status, SessionState::Idle) {
        match crate::placement::candidates_for(
            state.services.meta.as_ref(),
            &ctx,
            state.services.clock.now_utc(),
        )
        .await
        {
            Ok(c) if c.hosts.is_empty() => {
                // ADR 0079 (0078 re-review finding #4): fenced, like every
                // sibling write in this pipeline — an unfenced Idle→Queued
                // here let a reclaimed-away zombie executor fork the state
                // machine. `false` = the epoch moved (successor re-claimed)
                // or the row left Idle under us: stop silently, no event
                // (the `fenced:` Conflict convention — the verb's Retry
                // re-reads the session and dispatches on its real state).
                let queued = state
                    .services
                    .meta
                    .enqueue_session_resume(id, op_ctx.epoch)
                    .await
                    .map_err(|e| ApiError::Internal(format!("enqueue_session_resume: {e}")))?;
                if !queued {
                    return Err(fenced_error());
                }
                let _ = state
                    .emit_fenced(
                        id,
                        op_ctx.fence(),
                        SessionEvent::StatusChanged {
                            from: SessionState::Idle,
                            to: SessionState::Queued,
                            at: state.services.clock.now_utc(),
                        },
                    )
                    .await;
                tracing::info!(%id, "resume found no host capacity — queued (ADR 0048)");
                return Ok(SnapshotResponse {
                    session_id: id,
                    snapshot_id: Some(record.id.to_string()),
                    size_bytes: Some(record.size_bytes),
                    note: "queued",
                });
            }
            Ok(_) => {} // a candidate exists — proceed with the soft pick
            Err(e) => {
                // Read error: don't queue blindly, fall through to the
                // restore attempt (which surfaces the real error).
                tracing::warn!(%id, error = ?e, "resume: candidates_for failed; attempting restore");
            }
        }
    }
    // ADR 0016 Phase B commit 6: pick the newer of
    // `session.live_disk_manifest` and `record.disk_manifest`.
    // Without this, the first resume after Phase B's continuous
    // flush is enabled silently rolls the session back to the
    // snapshot's stale disk lineage, throwing away every flush
    // since the snapshot.
    //
    // `session.live_disk_manifest` is `None` when:
    // - The session never went through Phase B (non-NBD host /
    //   never had a publish land).
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
    // ADR 0045 D4: hand the host the image's base manifest so resumed
    // sessions share the per-image base shm with fresh creates.
    let base_memory_manifest = base_memory_manifest_for_image(state, &session.image).await;
    // ADR 0095: peer-fill hint — if the snapshot host is alive and
    // wire-compatible, tell the destination where the chunks are
    // resident. Stamped unconditionally BEFORE placement (the pick
    // happens inside restore_for_session): an affinity-host landing
    // makes the destination's pre-pass a no-op stat walk, a cross-host
    // landing pulls the divergent set over the LAN, and a dead/absent
    // source leaves the hint empty ⇒ the pure-GCS path, unchanged.
    // Best-effort by contract: any lookup failure degrades to no hint.
    let peer_hints = match record.host_id {
        Some(source) => match state.services.meta.list_active_hosts().await {
            Ok(hosts) => {
                let now = state.services.clock.now_utc();
                let ttl = crate::placement::placement_ttl();
                hosts
                    .iter()
                    .find(|h| h.id == source)
                    .and_then(|h| crate::placement::host_can_serve_chunks(h, now, ttl))
                    .map(|addr| vec![addr.to_string()])
                    .unwrap_or_default()
            }
            Err(e) => {
                tracing::debug!(%id, error = %e, "resume: host lookup for peer hint failed; no hint");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    let restore_metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: record.id,
        size_bytes: record.size_bytes,
        created_at: record.created_at,
        image_version: record.image_version.clone(),
        base_memory_manifest,
        migration_source: None,
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
        // chunks and `working_set` is a fresh-restore-only prefetch hint, so
        // both stay None.
        source_sandbox_id: None,
        state_blob_key: portable_state_key,
        sidecar_blob_key: portable_sidecar_key,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        // ADR 0035: resume keeps the pinned generations (no swap); the
        // host materializes any the receiving host is missing.
        aux_bundles: record.aux_bundles.clone(),
        // Issue #529: restore-side reconstruction, not a fresh capture.
        paused_at: None,
        // ADR 0095: assembled above — the live snapshot host, or empty.
        peer_hints,
    };
    // ADR 0079: the restore step boundary — a live VM exists from here.
    if !op_ctx.step("restore").await {
        return Err(fenced_error());
    }
    let (host_id, new_sandbox_id) = match crate::placement::restore_for_session(
        state.services.meta.as_ref(),
        &state.host_registry,
        &ctx,
        restore_metadata,
        op_ctx.fence(),
        state.services.clock.now_utc(),
    )
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
    // ADR 0019 / telemetry restoration (#526): same-host vs cross-host
    // resume split — the baseline for "how often does snapshot affinity
    // actually land the hot-tier hit it's meant to."
    ::metrics::counter!(
        crate::metrics::SESSION_RESUME_PLACEMENT_TOTAL,
        "placement" => resume_placement_label(record.host_id, host_id),
    )
    .increment(1);
    if !op_ctx.step("bind").await {
        // Fenced with a fresh VM up: it is bound to no session row, so
        // the host's ownership-oracle orphan reap GCs it. Never destroy
        // from a fenced executor (the successor may be mid-work).
        return Err(fenced_error());
    }
    bind_resumed_session(state, id, host_id, new_sandbox_id, op_ctx.fence()).await?;
    // ADR 0077 phase 4 (Revive): NO `Idle → Created` pre-transition.
    // The session stays Idle across the restore + harness rebuild and
    // flips STRAIGHT to Active (Idle → Active) in finish_resume_to_active
    // once the harness reattaches — one transition + one event on the
    // happy path instead of Idle→Created→Active, and no window where a
    // resumed-but-not-yet-active session sits at Created. A start_agent
    // failure parks it at Created there (the /exec-409 contract).
    // ADR 0028 A.log: this is a rung-1 restore (coherent memory
    // checkpoint) — rewind the transcript to the checkpoint's cursor
    // before the harness comes back, so the resumed agent's first
    // events append after an honest recovery boundary, not after
    // messages it never made. No-op for a checkpoint that was the head.
    // ADR 0091: a clean idle resume tombstones NOTHING (harness_idle +
    // coordinator facts are rewind-excluded) and thus emits no recovery
    // event; when rows genuinely roll back here the honest cause is
    // CheckpointLag — the session idled normally but its latest usable
    // checkpoint predates real guest activity. This path serves resumes
    // from `Idle`; no host death is implied (the old hardcoded
    // HostFailureRecovery made every campaign resume read as a
    // disaster).
    apply_rung1_rewind(
        state,
        id,
        record.events_cursor,
        RecoveryCause::CheckpointLag,
    )
    .await;
    if !op_ctx.step("finish").await {
        return Err(fenced_error());
    }
    // Refresh the session row so finish_resume_to_active sees the
    // freshly-bound host_id + sandbox_id (the caller might've raced
    // a concurrent writer between bind_resumed_session and now).
    let session_refreshed = state.services.meta.get_session(id).await?;
    let outcome = finish_resume_to_active(
        state,
        &session_refreshed,
        new_sandbox_id,
        true,
        op_ctx.fence(),
    )
    .await?;
    match outcome {
        // ADR 0090: retryable — see FinishResumeOutcome.
        FinishResumeOutcome::CreatedHarnessFailed(e) => Err(ApiError::Unavailable(format!(
            "harness reattach failed after snapshot resume (will retry): {e}"
        ))),
        FinishResumeOutcome::Active => {
            state
                .emit(
                    id,
                    SessionEvent::Resumed {
                        snapshot_id: record.id,
                        at: state.services.clock.now_utc(),
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
    fence: SessionFence,
) -> Result<(), ApiError> {
    // ADR 0047: these are the AUTHORITATIVE routing writes. The
    // coordinator keeps no in-memory session→sandbox binding anymore,
    // so `sessions.{host_id,sandbox_id}` IS the routing — a failed
    // write here would leave every replica unable to dispatch `/exec`
    // to the resumed sandbox. Fail the resume rather than limp on an
    // in-memory fallback that no longer exists.
    //
    // Issue #211: this used to be two blind `WHERE id = $1` UPDATEs. A
    // `DELETE /sessions/:id` (terminate) racing the in-flight restore
    // can flip the row `Idle → Completed` and clear `sandbox_id`; the
    // blind binds would then succeed onto the *terminal* row, the
    // ownership oracle would answer `owned = true`, and the host's
    // stale-sandbox reap — the backstop for every orphan path — never
    // fires. Permanent leak. Guard the bind on the row still being the
    // `Idle`, unbound row we dispatched on. One atomic CAS rebind so
    // host + sandbox flip together.
    //
    // ADR 0079 note (pass 2 verdict): this deliberately stays
    // `rebind_session_guarded` (state-list guard) rather than
    // `fenced_assign_sandbox` (epoch guard) even though terminate now
    // rides the op log — a destroy op queues BEHIND this running resume
    // and its claim bumps `current_epoch`, so the ORIGINAL #211
    // interleaving (DELETE races the restore) is closed by ordering.
    // What is NOT closed: the fleet detectors. `dead_host` can drive an
    // Idle-on-a-dead-host row `HostLost → Dead` (terminal) WITHOUT an
    // epoch bump while this resume op is mid-restore (representable when
    // the row's `recoverable` flags went stale while artifacts survive —
    // the promote-failed edge in `snapshot_core`), and the epoch
    // predicate alone would bind the fresh VM onto that Dead row —
    // #211's leak, reborn. The state guard collapses into the fence once
    // the detectors' session writes become fenced enqueues.
    match state
        .services
        .meta
        .rebind_session_guarded(id, host_id, sandbox_id, Some(None), &[SessionState::Idle])
        .await
    {
        Ok(()) => {}
        Err(MetaError::Conflict(msg)) => {
            // The row went terminal (or a competitor bound it) while we
            // were restoring. Destroy the VM we just created so it does
            // NOT become an orphan, then fail the resume cleanly.
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                conflict = %msg,
                "bind_resumed_session: row no longer the Idle/unbound row we \
                 dispatched on (terminate/competing-resume race) — destroying \
                 the freshly restored sandbox to avoid an orphan",
            );
            if let Err(e) = state.services.host.destroy(sandbox_id, fence).await {
                tracing::warn!(
                    session_id = %id,
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "bind_resumed_session: destroy of unbound sandbox failed; \
                     host reconcile will GC it",
                );
            }
            return Err(ApiError::Conflict(format!(
                "resume aborted: session changed underneath the restore ({msg})"
            )));
        }
        Err(e) => {
            // A real persistence failure. The sandbox is bound to no row;
            // destroy it before surfacing so it can't leak.
            if let Err(de) = state.services.host.destroy(sandbox_id, fence).await {
                tracing::warn!(
                    session_id = %id,
                    sandbox_id = %sandbox_id,
                    error = %de,
                    "bind_resumed_session: destroy after bind failure failed; \
                     host reconcile will GC it",
                );
            }
            return Err(ApiError::Internal(format!("rebind_session on resume: {e}")));
        }
    }
    bind_session_routing_minted(state, id, sandbox_id).await;
    Ok(())
}

/// ADR 0018 commit 10 / ADR 0047: register the post-relocate
/// session→sandbox mapping on the target host-agent.
///
/// `host.bind_session(id, sandbox_id)` is what the in-VM adapter's
/// vsock-accept path uses to route reconnects, and what the
/// FlushScheduler's live-manifest publisher uses to attach session_id
/// to the publish RPC. The coordinator-side binding is NOT updated
/// here — it lives only in `sessions.sandbox_id` (Postgres), written by
/// the caller via `assign_session_sandbox` BEFORE this call, so every
/// replica's `/exec` / `/shell` / `/prompt` resolves the new sandbox by
/// reading that row ([`AppState::resolve_sandbox`]).
///
/// Shared with `bind_resumed_session` (the /resume path); exposed
/// `pub(crate)` so the admin evac endpoint and the `evac_resumer`
/// scanner (driving operator-drained sessions) can fire the same shape.
pub(crate) async fn bind_session_routing(
    state: &SharedState,
    id: SessionId,
    sandbox_id: SandboxId,
    binding_epoch: u64,
) {
    state
        .services
        .host
        .bind_session(id, sandbox_id, binding_epoch)
        .await;
}

/// ADR 0073: mint-then-bind for the NEW-sandbox fresh-spawn flows
/// (idle resume, disk-only cold recovery, evac restore). Live moves
/// must NOT come through here — a teleported harness survives with
/// its generation unchanged; they bind at `current_binding_epoch`.
pub(crate) async fn bind_session_routing_minted(
    state: &SharedState,
    id: SessionId,
    sandbox_id: SandboxId,
) -> u64 {
    let epoch = match state.services.meta.mint_binding_epoch(id).await {
        Ok(e) => e,
        Err(e) => {
            // Loud but non-fatal: with no mint, the spawn-path bind
            // (LocalHostClient::start_agent) still writes a record at
            // the spec's epoch; a 0 here means that attach will bounce
            // UnknownBinding until a later reattach mints properly.
            tracing::error!(session_id = %id, error = %e, "mint binding epoch failed");
            0
        }
    };
    bind_session_routing(state, id, sandbox_id, epoch).await;
    epoch
}

/// ADR 0051: transport-agnostic evict core (gRPC `EvictLocal` + axum
/// `/local`). ADR 0079: enqueues the evict verb (`target = idle`,
/// parking disabled — the caller explicitly asked to drop the sandbox)
/// and bounded-observes the op. The verb runs the FULL eviction pipeline,
/// so the session lands at Idle behind a FRESH capture — strictly safer
/// than the legacy destroy-without-capture body (which relied on the
/// pre-existing snapshot this endpoint still gates on).
pub(crate) async fn evict_local_core(state: &SharedState, id: SessionId) -> Result<(), ApiError> {
    let session = state.services.meta.get_session(id).await?;

    if session.status != SessionState::Active {
        return Err(ApiError::Conflict(format!(
            "session is {} — only Active sessions can be evicted",
            session.status.as_str()
        )));
    }

    // Refuse to evict if there's no snapshot to come back to. (The verb
    // now captures a fresh one anyway; this preserves the endpoint's
    // documented contract for callers that use it as a "did I ever
    // snapshot?" guard.)
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

    match enqueue_and_observe_evict(state, id, /* allow_park = */ false).await? {
        ObservedEvict::Idle => Ok(()),
        // allow_park = false makes this unreachable; honest error if the
        // verb ever changes shape underneath.
        ObservedEvict::ParkedPaused => Err(ApiError::Internal(
            "evict_local: pipeline parked a no-park eviction".into(),
        )),
    }
}

/// What the bounded observe saw the evict op land at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservedEvict {
    /// Full suspend: captured, destroyed, session at Idle.
    Idle,
    /// ADR 0074 rung 2: the VM was paused in place (session Evicting,
    /// `park_rung == 2`).
    ParkedPaused,
}

/// ADR 0079: shared evict entry point for the synchronous admin surfaces
/// (`EvictIdle`, `/local`): enqueue the evict verb and observe the op.
/// The session flips user-visibly (Idle / parked) well before a slow
/// finalize completes, so the observe settles fast on the status; the op
/// itself may keep running through the host-owned finalize.
pub(crate) async fn enqueue_and_observe_evict(
    state: &SharedState,
    id: SessionId,
    allow_park: bool,
) -> Result<ObservedEvict, ApiError> {
    use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState};
    let outcome = crate::session_ops::enqueue(
        state,
        id,
        OpKind::Evict,
        serde_json::json!({ "target": "idle", "allow_park": allow_park, "nominated": false }),
        None,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("evict enqueue failed: {e}")))?;
    let op_id = match outcome {
        EnqueueOutcome::Claimed(op) | EnqueueOutcome::Queued(op) => Some(op.id),
        EnqueueOutcome::Duplicate => None,
    };
    let deadline = state.services.clock.now_mono() + OP_OBSERVE_TIMEOUT;
    loop {
        // Status settles first (mark_idle / the park bookkeeping land
        // before the op row finishes).
        let session = state.services.meta.get_session(id).await?;
        match session.status {
            SessionState::Idle => return Ok(ObservedEvict::Idle),
            SessionState::Evicting if session.park_rung == 2 => {
                return Ok(ObservedEvict::ParkedPaused)
            }
            _ => {}
        }
        if let Some(op_id) = op_id {
            if let Ok(Some(op)) = state.services.meta.op_get(op_id).await {
                match op.state {
                    OpState::Failed => {
                        return Err(api_error_from_op_failure(
                            op.error.as_deref().unwrap_or("evict failed"),
                        ));
                    }
                    OpState::Cancelled => {
                        return Err(ApiError::Conflict(
                            "eviction was cancelled (the user returned); session stays live".into(),
                        ));
                    }
                    // Done without an Idle/parked status = the verb's
                    // re-entry guard skipped (a concurrent op moved the
                    // session first).
                    OpState::Done => {
                        return Err(ApiError::Conflict(format!(
                            "evict op completed as a no-op; session is {} — retry if still \
                             intended",
                            session.status.as_str()
                        )));
                    }
                    OpState::Queued | OpState::Running => {}
                }
            }
        }
        if state.services.clock.now_mono() >= deadline {
            return Err(ApiError::Conflict(
                "eviction in flight (op enqueued); retry shortly".into(),
            ));
        }
        tokio::time::sleep(OP_OBSERVE_POLL).await;
    }
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
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
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
            fc_snapshot_version: None,
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
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
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

/// ADR 0019 / telemetry restoration (#526): `resume_placement_label`
/// covers all three `engram_session_resume_total{placement=...}` values.
#[cfg(test)]
mod resume_placement_label_tests {
    use super::*;

    #[test]
    fn same_host_when_chosen_matches_record() {
        let h = engram_core::HostId::new();
        assert_eq!(resume_placement_label(Some(h), h), "same_host");
    }

    #[test]
    fn cross_host_when_chosen_differs_from_record() {
        let recorded = engram_core::HostId::new();
        let chosen = engram_core::HostId::new();
        assert_eq!(resume_placement_label(Some(recorded), chosen), "cross_host");
    }

    #[test]
    fn unknown_prior_host_when_record_has_no_host() {
        let chosen = engram_core::HostId::new();
        assert_eq!(resume_placement_label(None, chosen), "unknown_prior_host");
    }
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod evicting_gate_tests {
    use super::*;
    use chrono::Utc;
    use engram_core::types::session::SessionMode;
    use tempfile::TempDir;

    /// Shared with `api::prompt`'s emit-ordering tests via
    /// `state::tests::build_state_for_session` (PR #556 review finding #4 —
    /// same-crate unit test modules share `pub(crate)` fns fine, so the
    /// per-file `AppState`/`Services` wiring copy was retired).
    fn build_state_for_session(session: Session) -> (SharedState, TempDir) {
        let (state, _mini, local) = crate::state::tests::build_state_for_session(session);
        (state, local)
    }

    fn evicting_session(id: SessionId) -> Session {
        Session {
            id,
            status: SessionState::Evicting,
            host_id: None,
            sandbox_id: Some(SandboxId::new()),
            image: "test/repo:evicting-gate".into(),
            mode: SessionMode::Agent,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    /// ADR 0074 rung 1: a nominated (lease-free) eviction is cancelled
    /// by one CAS — `ensure_active` returns Ok with the session back at
    /// Active and NO resume machinery invoked (the VM was untouched).
    #[tokio::test]
    async fn ensure_active_cancels_a_nominated_eviction_inline() {
        let id = SessionId::new();
        let (state, mini, _local) =
            crate::state::tests::build_state_for_session(evicting_session(id));

        ensure_active(&state, id).await.expect("cancel path");

        assert_eq!(
            mini.session.lock().status,
            SessionState::Active,
            "rung-1 cancel must land the session back at Active",
        );
        let events = mini.events.lock();
        assert!(
            events.iter().any(|e| e.kind == "status_changed"),
            "the cancel must emit StatusChanged(Evicting -> Active)",
        );
    }

    /// The op fence: with an evict op RUNNING (the capture pipeline owns
    /// the session), the cancel must NOT flip the session — it flags the
    /// running op for cooperative cancel and returns false, so the
    /// caller orders its resume BEHIND the op instead.
    #[tokio::test]
    async fn cancel_is_fenced_out_while_an_evict_op_runs() {
        let id = SessionId::new();
        let (state, mini, _local) =
            crate::state::tests::build_state_for_session(evicting_session(id));
        // A running evict op, as the executor would hold it.
        let running = mini
            .ops
            .seed_running(id, engram_core::types::session_op::OpKind::Evict);

        let cancelled = try_cancel_nominated_eviction(&state, id)
            .await
            .expect("cancel probe");
        assert!(!cancelled, "a running evict op must fence the cancel out");
        assert_eq!(
            mini.session.lock().status,
            SessionState::Evicting,
            "the session must stay Evicting under the op's claim",
        );
        assert!(
            mini.ops.cancel_requested(running.id),
            "the running op must be flagged for cooperative cancel",
        );
    }

    fn evacuating_session(id: SessionId) -> Session {
        Session {
            status: SessionState::Evacuating,
            image: "test/repo:evacuating-gate".into(),
            ..evicting_session(id)
        }
    }

    /// ADR 0018: an `ensure_active` (/exec, /events) landing while an
    /// operator drain / teleport has the session at `Evacuating` must
    /// return a RETRYABLE 409 — the `evac_resumer` relocates it back to
    /// Active asynchronously. It must NOT route to `resume_session` (which
    /// has no Evacuating arm and would 409 with the misleading "only Idle /
    /// Created can be resumed"), and must leave the session untouched.
    #[tokio::test]
    async fn ensure_active_during_evacuating_returns_retryable_conflict() {
        let id = SessionId::new();
        let (state, _local) = build_state_for_session(evacuating_session(id));

        let err = ensure_active(&state, id)
            .await
            .expect_err("Evacuating must not inline-resume");
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        assert!(
            err.to_string().contains("relocating"),
            "must be the retryable relocating 409, not the resume_session \
             'only Idle / Created' message, got: {err}",
        );
        // Untouched — not resumed, not transitioned.
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Evacuating);
    }

    /// ADR 0079: a resume enqueued behind a RUNNING evict op that does
    /// not settle within the observe window surfaces the retryable
    /// "in flight" 409 — never the Idle auto-resume arm (the sandbox may
    /// still be live; the ordering-by-log is what protects it). A ZERO
    /// observe bound exercises the fallback deterministically.
    #[tokio::test]
    async fn resume_observe_during_evicting_falls_back_to_retryable_conflict() {
        let id = SessionId::new();
        let (state, mini, _local) =
            crate::state::tests::build_state_for_session(evicting_session(id));
        // A running evict op owns the session; the resume queues behind.
        let _running = mini
            .ops
            .seed_running(id, engram_core::types::session_op::OpKind::Evict);

        let err = enqueue_and_observe_resume_for(&state, id, Duration::ZERO)
            .await
            .expect_err("an unfinished evict op must not pass the observe");
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        assert!(
            err.to_string().contains("in flight"),
            "fallback must be the honest retryable in-flight 409, got: {err}",
        );
        // The session must be untouched — in particular NOT resumed
        // and NOT transitioned.
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Evicting);
    }

    /// ADR 0079 ordering-by-log (the ADR 0039 hold's successor): a
    /// resume enqueued mid-eviction runs strictly AFTER the evict op —
    /// we finish the running evict (landing the session at Idle) from a
    /// concurrent task and re-drive the queue; the observe must relay
    /// the RESUME's own outcome (with no snapshot seeded, an honest
    /// `Gone` + the session marked Dead), never the Evicting 409.
    #[tokio::test]
    async fn resume_queued_behind_evict_runs_after_it_settles() {
        let id = SessionId::new();
        let (state, mini, _local) =
            crate::state::tests::build_state_for_session(evicting_session(id));
        let running = mini
            .ops
            .seed_running(id, engram_core::types::session_op::OpKind::Evict);

        // Concurrently land the eviction at Idle + finish its op shortly
        // into the observe, then re-drive the session's queue (in prod
        // the executor's completion re-drive does this).
        let flip_state = state.clone();
        let flip_mini = mini.clone();
        let flipper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            flip_state
                .services
                .meta
                .transition_session(id, SessionState::Idle)
                .await
                .expect("Evicting → Idle is a legal transition");
            assert!(flip_mini.ops.finish(
                running.id,
                running.epoch.unwrap(),
                engram_core::types::session_op::OpState::Done,
                None,
            ));
            crate::session_ops::drive_session(&flip_state, id).await;
        });

        // Generous observe so the settle is seen well before the bound.
        let res = enqueue_and_observe_resume_for(&state, id, Duration::from_secs(5)).await;
        flipper.await.unwrap();

        // The observe must relay the resume verb's own outcome — proven
        // by the resume's error path, NOT an Evicting/in-flight 409.
        let err = res.expect_err("no snapshot seeded → resume fails Gone");
        assert!(
            !err.to_string().contains("in flight"),
            "must have relayed the resume op's outcome, got: {err}",
        );
        assert!(
            err.to_string().contains("snapshot_invalidated"),
            "expected the no-snapshot resume failure, got: {err}",
        );
        // resume_from_idle with no recoverable snapshot marks the
        // session Dead — confirming the resume verb actually ran.
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Dead);
    }

    /// ADR 0079: if the eviction races to a TERMINAL state (here
    /// `Evicting → Completed` via a concurrent DELETE), the queued
    /// resume op surfaces that state's honest typed error — the terminal
    /// `gone:` 410 — rather than a misleading retryable 409.
    #[tokio::test]
    async fn resume_observe_surfaces_terminal_state_on_race() {
        let id = SessionId::new();
        let (state, mini, _local) =
            crate::state::tests::build_state_for_session(evicting_session(id));
        let running = mini
            .ops
            .seed_running(id, engram_core::types::session_op::OpKind::Evict);

        let flip_state = state.clone();
        let flip_mini = mini.clone();
        let flipper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            flip_state
                .services
                .meta
                .transition_session(id, SessionState::Completed)
                .await
                .expect("Evicting → Completed is a legal transition");
            assert!(flip_mini.ops.finish(
                running.id,
                running.epoch.unwrap(),
                engram_core::types::session_op::OpState::Done,
                None,
            ));
            crate::session_ops::drive_session(&flip_state, id).await;
        });

        let err = enqueue_and_observe_resume_for(&state, id, Duration::from_secs(5))
            .await
            .expect_err("Completed session has no work to dispatch");
        flipper.await.unwrap();

        // Terminal is GONE (410), not a retryable 409: the outbox driver
        // maps Gone to its Terminal drop arm instead of deferring forever.
        assert_eq!(err.status(), axum::http::StatusCode::GONE);
        assert!(
            err.to_string().contains("terminal"),
            "must surface the terminal-state error, not the mid-eviction 409, got: {err}",
        );
        assert!(
            !err.to_string().contains("mid-eviction"),
            "must not still report mid-eviction once the row went terminal, got: {err}",
        );
    }

    /// Review finding #9: a direct /resume on an Evicting session with NO
    /// running evict op (the nomination window, or a parked-paused VM
    /// whose evict op is already Done) ASCENDS to Active — the resume op
    /// holds the one-running slot, so Evicting can only be the cancelable
    /// nomination/park case. It no longer Retry-forever-behind-a-
    /// nonexistent-evict. (The genuine "an evict op is RUNNING" ordering
    /// case stays a retryable conflict — see
    /// `resume_queued_behind_evict_runs_after_it_settles`.)
    #[tokio::test]
    async fn resume_during_evicting_nomination_ascends_to_active() {
        let id = SessionId::new();
        let (state, _local) = build_state_for_session(evicting_session(id));

        enqueue_and_observe_resume_for(&state, id, Duration::from_secs(5))
            .await
            .expect("a nomination-window Evicting resume must ascend, not conflict");
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Active,
            "the resume verb ascends the nomination back to Active",
        );
    }

    /// DELETE mid-eviction: Evicting → Completed is legal, and the
    /// eviction scanner's racing pipeline then fails its own
    /// transition against the terminal row and exits via the abort
    /// path — the session stays Completed.
    #[tokio::test]
    async fn delete_during_evicting_completes_and_pipeline_backs_off() {
        let id = SessionId::new();
        let (state, mini, _local) =
            crate::state::tests::build_state_for_session(evicting_session(id));
        let sandbox_id = state
            .services
            .meta
            .get_session(id)
            .await
            .unwrap()
            .sandbox_id
            .unwrap();

        crate::api::sessions::delete_session_core(&state, id)
            .await
            .expect("delete mid-eviction");
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(after.status, SessionState::Completed);

        // ADR 0079: the delete rode a DESTROY op; the observe returns on
        // the terminal flip, which lands before the op finishes
        // (sandbox teardown) finishes. Wait for the op to settle so the
        // evict enqueue below deterministically claims the free lane.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let destroy = mini
                .ops
                .all()
                .into_iter()
                .find(|o| o.kind == engram_core::types::session_op::OpKind::Destroy)
                .expect("delete_session_core must have enqueued a destroy op");
            if destroy.state == engram_core::types::session_op::OpState::Done {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "destroy op did not settle: {destroy:?}",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // A racing evict op (a nomination that already swept the row)
        // backs off harmlessly: the pipeline's K5 entry guard sees the
        // terminal row and completes the op as a no-op — the terminal
        // state is untouched.
        let _ = sandbox_id; // the pipeline re-reads the binding itself
        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                id,
                engram_core::types::session_op::OpKind::Evict,
                serde_json::json!({ "target": "idle", "allow_park": true, "nominated": true }),
                None,
                "test-pod",
            )
            .await
            .expect("enqueue+claim")
        {
            engram_core::types::session_op::EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        let op_id = op.id;
        crate::session_ops::drive_claimed(&state, op).await;
        let terminal = state
            .services
            .meta
            .op_get(op_id)
            .await
            .unwrap()
            .expect("op row");
        assert_eq!(
            terminal.state,
            engram_core::types::session_op::OpState::Done,
            "the racing evict op must complete as a no-op",
        );
        let still = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(still.status, SessionState::Completed);
    }

    /// ADR 0079 (the issue-#210 successor): a resume op re-driven at a
    /// recorded `bind`/`finish` step with a live binding must SKIP the
    /// restore and go straight to the finish leg — the crash residue the
    /// deleted residual-sandbox destroy used to compensate for is now a
    /// step-resume. With no snapshot seeded, reaching Active proves no
    /// restore was attempted (a restore would have failed Gone).
    #[tokio::test]
    async fn resume_op_resumes_at_finish_step_with_bound_sandbox() {
        let id = SessionId::new();
        let host = engram_core::HostId::new();
        let residual = SandboxId::new();
        let session = Session {
            id,
            // Idle but still carrying a binding — a prior attempt of THIS
            // op restored + bound and crashed before the finish leg
            // (ADR 0077: the bind happens while the row is still Idle).
            status: SessionState::Idle,
            host_id: Some(host),
            sandbox_id: Some(residual),
            image: "test/repo:step-resume".into(),
            mode: SessionMode::Agent,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let (state, mini, _local) = crate::state::tests::build_state_for_session(session);

        // Claim the resume op and durably record the `bind` step, as the
        // crashed attempt would have.
        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(
                id,
                engram_core::types::session_op::OpKind::Resume,
                serde_json::json!({}),
                None,
                "test-pod",
            )
            .await
            .expect("enqueue+claim")
        {
            engram_core::types::session_op::EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        assert!(
            mini.ops.record_step(op.id, op.epoch.unwrap(), "bind"),
            "step record must land",
        );
        let re_driven = state
            .services
            .meta
            .op_get(op.id)
            .await
            .unwrap()
            .expect("op row");
        assert_eq!(re_driven.step.as_deref(), Some("bind"));

        // Re-drive (the reclaim path hands the row back at its step).
        crate::session_ops::drive_claimed(&state, re_driven).await;

        let terminal = state
            .services
            .meta
            .op_get(op.id)
            .await
            .unwrap()
            .expect("op row");
        assert_eq!(
            terminal.state,
            engram_core::types::session_op::OpState::Done,
            "step-resume must complete without a restore: {:?}",
            terminal.error,
        );
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(
            after.status,
            SessionState::Active,
            "the finish leg must land the session at Active on the residual binding",
        );
        assert_eq!(
            after.sandbox_id,
            Some(residual),
            "the residual binding is the live binding — no destroy, no double-restore",
        );
    }
}

/// ADR 0079 (0078 re-review finding #4): the resume verb's
/// Idle-with-empty-candidates arm (`Idle → Queued` via
/// `enqueue_session_resume`) is FENCED like every sibling write in the op
/// pipeline — a reclaimed-away zombie executor must not fork the state
/// machine or land a stale StatusChanged event.
#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod resume_queue_fence_tests {
    use super::*;
    use chrono::Utc;
    use engram_core::types::session::SessionMode;
    use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState};

    fn idle_session(id: SessionId) -> Session {
        Session {
            id,
            status: SessionState::Idle,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:queue-fence".into(),
            mode: SessionMode::Agent,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    /// A disk-only record: `snapshot_artifacts_present` passes it without
    /// touching blob storage (only memory-bearing FC snapshots re-verify),
    /// so the resume reaches the placement arm deterministically.
    fn disk_only_record(id: SessionId) -> SnapshotRecord {
        SnapshotRecord {
            id: engram_core::types::SnapshotId::new(),
            session_id: Some(id),
            host_id: None,
            image_version: "queue-fence".into(),
            size_bytes: 1,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
            disk_manifest: None,
            memory_manifest: None,
            recoverable: true,
            aux_bundles: Vec::new(),
            events_cursor: None,
            fc_snapshot_version: None,
        }
    }

    /// Happy path: under the CURRENT epoch, an empty candidate set queues
    /// the session (Idle → Queued) and emits the fenced StatusChanged.
    #[tokio::test]
    async fn no_capacity_resume_queues_under_current_fence() {
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));
        mini.snapshots.lock().push(disk_only_record(id));
        // No hosts staged → `candidates_for` yields an empty set.

        let op = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Resume, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue+claim")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        let ctx = crate::session_ops::OpCtx {
            state: &state,
            op: &op,
            epoch: op.epoch.unwrap(),
        };
        let session = state.services.meta.get_session(id).await.unwrap();
        let resp = resume_from_idle(&ctx, session).await.expect("queued");
        assert_eq!(resp.note, "queued");
        assert_eq!(
            mini.session.lock().status,
            SessionState::Queued,
            "no capacity → the resume parks the session in the queue",
        );
        assert!(
            mini.events
                .lock()
                .iter()
                .any(|e| e.kind == "status_changed"),
            "the Idle→Queued flip must emit its StatusChanged event",
        );
    }

    /// A STALE fence (a successor re-claimed the session) must not queue
    /// the session or emit — the zombie stops on the `fenced:` Conflict.
    #[tokio::test]
    async fn stale_fenced_resume_cannot_fork_idle_to_queued() {
        let id = SessionId::new();
        let (state, mini, _local) = crate::state::tests::build_state_for_session(idle_session(id));
        mini.snapshots.lock().push(disk_only_record(id));

        // Claim + finish op1 (epoch 1), then claim op2 (epoch 2) — the
        // mock CAS-bumps `current_epoch` on each claim, so epoch 1 is now
        // a reclaimed-away predecessor's fence.
        let op1 = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Resume, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue+claim op1")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };
        assert!(mini
            .ops
            .finish(op1.id, op1.epoch.unwrap(), OpState::Done, None));
        let _op2 = match state
            .services
            .meta
            .op_enqueue_and_claim(id, OpKind::Resume, serde_json::json!({}), None, "test-pod")
            .await
            .expect("enqueue+claim op2")
        {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("op lane busy: {other:?}"),
        };

        let stale_ctx = crate::session_ops::OpCtx {
            state: &state,
            op: &op1,
            epoch: op1.epoch.unwrap(), // 1 — superseded by op2's claim
        };
        let session = state.services.meta.get_session(id).await.unwrap();
        let err = match resume_from_idle(&stale_ctx, session).await {
            Err(e) => e,
            Ok(resp) => panic!(
                "a stale fence must not queue the session, got Ok({:?})",
                resp.note
            ),
        };
        assert!(
            err.to_string().contains("fenced"),
            "the stop must ride the `fenced:` Conflict convention, got: {err}",
        );
        assert_eq!(
            mini.session.lock().status,
            SessionState::Idle,
            "the zombie executor must NOT fork Idle → Queued",
        );
        assert!(
            !mini
                .events
                .lock()
                .iter()
                .any(|e| e.kind == "status_changed"),
            "no stale StatusChanged event may land",
        );
    }
}
