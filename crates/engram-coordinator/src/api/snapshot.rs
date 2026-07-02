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

use chrono::Utc;
use engram_core::traits::storage::BlobStorage;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{Session, SessionState};
use engram_core::{MetaError, SandboxError, SandboxId, SessionId};
use serde::Serialize;
use tracing::Instrument;

use crate::error::ApiError;
use crate::placement::ScheduleContext;
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
        None,
        session_env,
        b.manifest.workdir.clone(),
    )
    .await
    .ok()
    .flatten()?;
    crate::api::sessions::inject_harness_env(state, id, &mut agent.env).await;
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
pub(crate) async fn reattach_harness_in_place(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
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
        .start_agent(sandbox_id, agent, policy)
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

/// Issue #213: acquire the per-session `session_lease` with the same
/// bounded-retry shape `resume_session` uses (8 attempts, 3s apart),
/// so the manual operator endpoints (`snapshot`, `evict_local`)
/// serialize against the eviction scanner, host idle-nominations,
/// resume, and live migration exactly as the rest of the lifecycle
/// does. Returns the held guard, or a retryable `Conflict` once the
/// retries are exhausted (the lease is genuinely held by another
/// holder — a capture / resume / eviction in flight).
///
/// Before this, both endpoints touched the host + PG with NO lease, so
/// a host idle-nomination flipping `Active → Evicting` (with the
/// scanner's capture in flight) could have its sandbox destroyed
/// mid-capture, and a manual snapshot could race the periodic
/// checkpoint driver's `abort_prior_inflight_snapshot`.
async fn acquire_session_lease(
    state: &SharedState,
    id: SessionId,
) -> Result<crate::idle_evictor::SessionLeaseGuard, ApiError> {
    // The inter-attempt backoff is env-tunable purely so tests can drive
    // the lease-contention path (a permanently-held lease → Conflict)
    // without the 21s wall the production 3s × 7 cadence would impose.
    // Production never sets it.
    let backoff = std::env::var("ENGRAM_LEASE_ACQUIRE_BACKOFF_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(std::time::Duration::from_secs(3));
    for attempt in 0..8u32 {
        match crate::idle_evictor::SessionLeaseGuard::try_acquire(state, id, None).await {
            Ok(Some(guard)) => return Ok(guard),
            Ok(None) if attempt < 7 => {
                tokio::time::sleep(backoff).await;
            }
            Ok(None) => break,
            Err(e) => {
                return Err(ApiError::Internal(format!(
                    "session lease acquire failed: {e}"
                )))
            }
        }
    }
    Err(ApiError::Conflict(format!(
        "session {id} is busy (mid-snapshot, mid-resume, or mid-eviction); retry shortly",
    )))
}

/// ADR 0051: transport-agnostic snapshot core (gRPC `Snapshot` + axum
/// `/snapshot`). Body moved verbatim from the legacy axum `snapshot`
/// handler — every lease-fence, spawn-detach, and recoverable-flip line
/// is preserved EXACTLY; only the return shape changed
/// (`Json<SnapshotResponse>` → `SnapshotResponse`) and the axum
/// extractors became plain params.
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

    // Issue #213: serialize the manual snapshot against the eviction
    // scanner, host idle-nominations, resume, and live migration. Without
    // the lease, a host idle-nomination (or operator drain) snapshotting
    // and destroying THIS sandbox could race the manual capture, and the
    // record→commit pair below could interleave with another pipeline's
    // `abort_prior_inflight_snapshot`.
    let lease = acquire_session_lease(state, id).await?;

    // Issue #213: DETACH the record→commit body from the cancellable
    // request future. The original handler ran inline in the request task,
    // so a client disconnect (CLI Ctrl-C) between `record_snapshot` and
    // `commit_snapshot` dropped the commit future: the PG row was durable
    // but the host-side snapshot was never committed. The next periodic
    // checkpoint tick then `abort_prior_inflight_snapshot`s and deletes
    // this snapshot's `state.bin`/sidecar within one interval, leaving a
    // `recoverable = true` row pointing at nothing — the exact 89f7984d
    // phantom-snapshot state. Mirror the resume/eviction pattern: run the
    // body in a `tokio::spawn`ed task that MOVES the lease in, so the
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
            let lease = lease;
            let _heartbeat = lease.spawn_heartbeat(std::time::Duration::from_secs(60));

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
            let metadata = st.services.host.snapshot(sandbox_id).await?;

            let now = Utc::now();
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
            };
            st.services.meta.record_snapshot(record.clone()).await?;

            // ADR 0034 durability: the live sandbox keeps running, so commit
            // the host's in-flight snapshot now — otherwise the periodic
            // checkpoint driver's next tick calls `abort_prior_inflight_snapshot`
            // and deletes this snapshot's state.bin/sidecar from BlobStorage
            // within one interval.
            let committed = match st.services.host.commit_snapshot(sandbox_id).await {
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
                    if let Err(e) = st.services.meta.record_snapshot(promoted).await {
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
        .instrument(capture_span),
    );

    handle.await.map_err(|join_err| {
        ApiError::Internal(format!("snapshot pipeline task panicked: {join_err}"))
    })?
}

/// ADR 0051: transport-agnostic resume core (gRPC `Resume` + axum
/// `/resume`). Thin pass-through to the shared, hardened
/// [`resume_session`] primitive (lease-fenced + spawn-detached) that
/// the legacy axum `/resume` handler already drove — only the return
/// shape differs (`Json<SnapshotResponse>` → `SnapshotResponse`).
pub(crate) async fn resume_core(
    state: &SharedState,
    id: SessionId,
) -> Result<SnapshotResponse, ApiError> {
    resume_session(state.clone(), id).await
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
        SessionState::Idle => {
            // Snapshotted, sandbox destroyed, ready to resume. Take the
            // standard lease-serialized resume path inline.
            resume_session(state.clone(), id).await?;
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
            // Settled to Idle — take the standard resume path
            // (lease-serialized; see the doc comment). An eviction settling
            // into Evacuating (an operator drain landed mid-hold) is NOT
            // inline-resumable; it falls through to the `_` arm below, which
            // re-dispatches through `ensure_active` for the retryable 409.
            SessionState::Idle => {
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

pub(crate) async fn resume_session(
    state: SharedState,
    id: SessionId,
) -> Result<SnapshotResponse, ApiError> {
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
    // ADR 0045 D5: an eviction's finalize task holds the lease while its
    // upload completes in the background (typically seconds for a
    // diff-chain capture). A resume landing in that window WAITS briefly
    // instead of 409ing — the common evict-then-immediately-resume shape
    // — and only surfaces the Conflict if the lease stays held (a long
    // full-seed upload, or a genuinely concurrent resume).
    let mut lease = None;
    for attempt in 0..8u32 {
        match crate::idle_evictor::SessionLeaseGuard::try_acquire(&state, id, None).await {
            Ok(Some(guard)) => {
                lease = Some(guard);
                break;
            }
            Ok(None) if attempt < 7 => {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            Ok(None) => {}
            Err(e) => {
                return Err(ApiError::Internal(format!(
                    "resume lease acquire failed: {e}"
                )))
            }
        }
    }
    let Some(lease) = lease else {
        return Err(ApiError::Conflict(format!(
            "session {id} is already mid-resume or mid-eviction; retry shortly",
        )));
    };

    // Issue #210: DETACH the resume pipeline from the cancellable request
    // future. Between `restore_for_session` (a live VM exists from there)
    // and the PG writes that record it (`bind_resumed_session`,
    // `transition(Created)`, `finish_resume_to_active`) sit multi-second
    // host RPCs (restore deadline 240s). If the HTTP client disconnects in
    // that window — a CLI Ctrl-C while a prompt "hangs" on a slow resume —
    // axum drops the handler future at the current await. Awaited INLINE,
    // that drops the pipeline mid-flight AND drops the `SessionLeaseGuard`,
    // releasing the lease while the session is physically mid-resume. A
    // retry then re-takes the freed lease, sees the row still `Idle` (the
    // bind never landed), and restores a SECOND VM from the same snapshot —
    // two guests executing the same restored state, the first orphaned (no
    // PG row references it). Cancelling one step later instead strands the
    // row at `Created` (no scanner arm; #194).
    //
    // Mirror the ADR 0034 idle-eviction pattern (and the live-teleport
    // detachment in #208): run the pipeline body in a `tokio::spawn`ed task
    // that MOVES the lease guard into itself, so the lease is held — and the
    // pipeline always runs to a terminal arm — regardless of the request's
    // fate. The handler awaits the JoinHandle only to OBSERVE the result for
    // the connected client; a disconnect drops that await, not the work.
    //
    // ADR 0019 / telemetry restoration (#526): re-parent this detached
    // resume body onto the request span so it stitches under the caller's
    // trace instead of exporting as a childless root (span context only).
    let st = state.clone();
    let resume_span = tracing::Span::current();
    let handle = tokio::spawn(
        async move {
        // Hold the lease for the WHOLE pipeline by moving it in here.
        let lease = lease;
        // Issue #212: the resume pipeline is a straight-line sequence of
        // multi-second host RPCs (restore deadline 240s) with no touch loop
        // of its own — unlike the eviction/migration finalize loops, it
        // never refreshed the lease. A resume that crossed the 180s reap
        // window was deleted out from under itself; another holder then
        // re-acquired and drove the same session concurrently (#212's
        // double-driver). Refresh `locked_at` every 60s for the pipeline's
        // lifetime so a healthy holder is never reaped. The heartbeat is
        // dropped (aborted) when `lease` drops at task exit.
        let _heartbeat = lease.spawn_heartbeat(std::time::Duration::from_secs(60));
        let session = st.services.meta.get_session(id).await?;

        // ADR 0007: single-tier dispatcher.
        //   Idle  → restore from the snapshot's chunked manifests
        //           (snapshot-affinity-scheduled to the host that
        //           captured it; cross-host materialization is a
        //           follow-up).
        //   Dead  → 410 Gone (no recoverable manifests).
        //   any other status → 409.
        match session.status {
            SessionState::Idle => resume_from_idle(st, session).await,
            // ADR 0018: an auto-evac'd session is left at `Created` on
            // the new host with the VM restored but the harness stale
            // (vsock to source's agentd is dead). `/resume` against
            // `Created` finishes the harness rebuild via the shared
            // `finish_resume_to_active` primitive. This is the path that
            // closes the "session survives a MIG roll" loop —
            // `dead_host.rs` rebinds + restores, the user (or an
            // automated layer) hits `/resume` to bring it to Active.
            SessionState::Created => resume_from_created(st, session).await,
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
            // ADR 0018: operator drain / live teleport. Not directly
            // resumable — the `evac_resumer` relocates it and drives it back
            // to Active. Retryable, like Evicting above (and matches the
            // `ensure_active` Evacuating arm).
            SessionState::Evacuating => Err(ApiError::Conflict(
                "session is relocating (operator drain / teleport); it will \
                 resume automatically — retry shortly."
                    .into(),
            )),
            other => Err(ApiError::Conflict(format!(
                "session is {} — only Idle / Created sessions can be resumed",
                other.as_str()
            ))),
        }
    }
    .instrument(resume_span));

    handle.await.map_err(|join_err| {
        // The pipeline task panicked. It did NOT release the lease cleanly
        // through a normal exit, so let the stale-lease reaper own recovery
        // (180s) rather than surface a half-state to the client as success.
        ApiError::Internal(format!("resume pipeline task panicked: {join_err}"))
    })?
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
    let outcome = finish_resume_to_active(&state, &session, sandbox_id, true).await?;
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

    // Issue #210 idempotency: an `Idle` row normally has `sandbox_id = NULL`
    // (evict_local clears it). A residual binding here means a PRIOR resume
    // attempt restored a VM and bound it but never finished advancing the
    // row off `Idle` — e.g. a coordinator crash between `bind_resumed_session`
    // and `transition(Created)`, before the lease/spawn detachment could see
    // it through. (The normal cancellation path is now handled by the spawn
    // in `resume_session`; this guards the crash residue the spawn can't.)
    // Restoring fresh on top of it would leak that earlier VM as an orphan
    // (no row references it once we overwrite the binding) and produce the
    // double-restore the issue describes. We can't cheaply prove the residual
    // sandbox is healthy and mid-restore vs. half-dead, so take the
    // conservative, idempotent path: destroy it and clear the binding, then
    // restore cleanly below. `host.destroy` is idempotent (a GC'd/absent
    // sandbox is a no-op), so this is safe even if the host already reaped it.
    if let Some(stale_sandbox) = session.sandbox_id {
        tracing::warn!(
            session_id = %id,
            stale_sandbox = %stale_sandbox,
            "resume_from_idle: Idle session has a residual sandbox binding from a \
             prior unfinished resume — destroying it before restoring fresh to avoid \
             a double-restore orphan",
        );
        if let Err(e) = state.services.host.destroy(stale_sandbox).await {
            // Best-effort: the host reconcile GCs an unreachable sandbox. We
            // still clear the binding so we don't restore on top of it.
            tracing::warn!(
                session_id = %id,
                stale_sandbox = %stale_sandbox,
                error = %e,
                "resume_from_idle: residual sandbox destroy failed; clearing binding \
                 and continuing (host reconcile will GC)",
            );
        }
        if let Err(e) = state.services.meta.assign_session_sandbox(id, None).await {
            return Err(ApiError::Internal(format!(
                "resume_from_idle: failed to clear residual sandbox binding: {e}"
            )));
        }
    }

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
    let origin = relocatable.host_id.take();

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
    let outcome = finish_resume_to_active(&state, &refreshed, receipt.new_sandbox_id, true).await?;
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
    // Emit the terminal StatusChanged here? The live-migration verb
    // suppresses it and emits a single `evacuating -> active` itself —
    // the `created -> active` hop is an internal FSM step there, not a
    // user-meaningful state (the session can't take messages yet).
    emit_status: bool,
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
    let mut start_agent_failed = false;
    if let Some((agent, policy)) =
        resolve_resume_agent_and_policy(state, session, new_sandbox_id).await
    {
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
    if emit_status {
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
        cpu_budget_vcpus: None,
        // Restore from a snapshot reuses an existing in-memory image —
        // no chunked-rootfs prefetch needed on the resume path. Snapshot
        // affinity already constrains to a host that has the bytes.
        required_image_digest: None,
        exclude_host: None,
        // ADR 0045 D4 / ADR 0039: soft preference for the host whose
        // chunk cache + per-image base shm are warm — the capturing
        // host first, else wherever the session last ran.
        prefer_host: record.host_id.or(session.host_id),
    };

    // ADR 0048 C7: if NO host can take this resume (the fleet is fully
    // cordoned for a scale-down wave, or scaled to zero), QUEUE it
    // (Idle → queued) instead of erroring. The queue scanner resumes it
    // once capacity returns / the fleet scales up. Only triggers on an
    // empty candidate set — a present-but-full fleet still soft-picks
    // (the pre-existing ADR 0046 resume-isn't-reserved posture).
    if matches!(session.status, SessionState::Idle) {
        match crate::placement::candidates_for(state.services.meta.as_ref(), &ctx).await {
            Ok(c) if c.hosts.is_empty() => {
                state
                    .services
                    .meta
                    .enqueue_session_resume(id)
                    .await
                    .map_err(|e| ApiError::Internal(format!("enqueue_session_resume: {e}")))?;
                let _ = state
                    .emit(
                        id,
                        SessionEvent::StatusChanged {
                            from: SessionState::Idle,
                            to: SessionState::Queued,
                            at: Utc::now(),
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
    let base_memory_manifest = base_memory_manifest_for_image(&state, &session.image).await;
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
    };
    let (host_id, new_sandbox_id) = match crate::placement::restore_for_session(
        state.services.meta.as_ref(),
        &state.host_registry,
        &ctx,
        restore_metadata,
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
    bind_resumed_session(&state, id, host_id, new_sandbox_id).await?;
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
    let outcome = finish_resume_to_active(&state, &session_refreshed, new_sandbox_id, true).await?;
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
    // `Idle`, unbound row we dispatched on (resume_from_idle cleared any
    // residual sandbox to NULL above). One atomic CAS rebind so host +
    // sandbox flip together.
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
            if let Err(e) = state.services.host.destroy(sandbox_id).await {
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
            if let Err(de) = state.services.host.destroy(sandbox_id).await {
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
    bind_session_routing(state, id, sandbox_id).await;
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
) {
    state.services.host.bind_session(id, sandbox_id).await;
}

/// ADR 0051: transport-agnostic evict core (gRPC `EvictLocal` + axum
/// `/local`). Body moved verbatim from the legacy axum `evict_local`
/// handler — every lease-fence, under-lease status re-read (CAS guard),
/// and spawn-detached PG-first teardown line is preserved EXACTLY; only
/// the final `Ok(StatusCode::ACCEPTED)` moved up to the axum wrapper so
/// the core returns the transport-neutral `Ok(())`.
pub(crate) async fn evict_local_core(state: &SharedState, id: SessionId) -> Result<(), ApiError> {
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

    // Issue #213: serialize against the eviction scanner, host
    // idle-nominations, resume, and live migration. Without the lease, a
    // host idle-nomination flipping `Active → Evicting` (with the
    // scanner's capture in flight) could have its sandbox destroyed
    // mid-capture here — `Evicting → Idle` is a legal edge, so the final
    // transition would NOT reject the stomp. Holding the lease for the
    // whole teardown makes manual evict + scanner eviction mutually
    // exclusive per session.
    let lease = acquire_session_lease(state, id).await?;

    // Issue #213: re-read the session UNDER the lease. The status gate
    // above ran before we held the lease, so a concurrent eviction /
    // resume could have moved the session in between. Only `Active` with a
    // bound sandbox is evictable from here; anything else means a peer
    // already handled it (mirrors the eviction pipeline's K5 re-entry
    // guard, idle_evictor.rs).
    let session = state.services.meta.get_session(id).await?;
    if session.status != SessionState::Active {
        return Err(ApiError::Conflict(format!(
            "session is {} — only Active sessions can be evicted (it moved while \
             acquiring the lease)",
            session.status.as_str()
        )));
    }

    // Issue #213: DETACH the destroy→transition body from the cancellable
    // request future. The original handler ran inline, destroying the
    // sandbox BEFORE the PG transition; a client disconnect between the
    // two left an `Active` row pointing at a destroyed sandbox (→ spurious
    // HostLost via reconcile). Mirror the resume/eviction pattern: run the
    // body in a `tokio::spawn`ed task that MOVES the lease in, so the
    // teardown always runs to a terminal arm regardless of the request's
    // fate, and the handler only awaits the JoinHandle to relay the result.
    //
    // ADR 0019 / telemetry restoration (#526): re-parent this detached
    // evict body onto the request span so it stitches under the caller's
    // trace instead of exporting as a childless root (span context only).
    let st = state.clone();
    let evict_span = tracing::Span::current();
    let handle = tokio::spawn(
        async move {
            let lease = lease;
            let _heartbeat = lease.spawn_heartbeat(std::time::Duration::from_secs(60));
            let now = Utc::now();

            // Issue #213: PG-FIRST, matching the eviction pipeline's documented
            // discipline (idle_evictor.rs:289-339). The reconciler treats
            // `sandbox_id IS NOT NULL` + `host.running_sandboxes ∌ sandbox_id`
            // (Active status) as an orphan to recover via HostLost→Idle. If
            // `destroy()` ran before the transition (as it used to), a heartbeat
            // landing in that window would race ahead and flip the session to
            // Idle via the recovery path, leaving this handler's later
            // `transition(Idle→Idle)` to fail. Clearing `sandbox_id` and
            // flipping to Idle FIRST means reconcile's active-only guard no-ops
            // by the time the host reports the sandbox gone.
            if let Err(e) = st.services.meta.assign_session_sandbox(id, None).await {
                tracing::warn!(
                    session_id = %id,
                    error = %e,
                    "evict_local: assign_session_sandbox(None) failed",
                );
            }
            let prev = st
                .services
                .meta
                .transition_session(id, SessionState::Idle)
                .await?;

            // Now that the session is Idle, destroy the sandbox. Resolve it
            // from the row we read under the lease (PG authority, ADR 0047) so
            // teardown works on any replica. Best-effort: the PG state is
            // already correct; a failed destroy is cleaned up by the host's
            // orphan_reap — the sandbox is the cache, not the source of truth.
            if let Some(sandbox_id) = session.sandbox_id {
                if let Err(e) = st.services.host.destroy(sandbox_id).await {
                    tracing::warn!(
                        session_id = %id,
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "evict_local: destroy failed after Idle transition; orphan_reap \
                         will clean up",
                    );
                }
                // ADR 0006: the host-agent unregisters its local proxy
                // entry as part of `destroy`. No coordinator-side cleanup.
            }

            st.emit(id, SessionEvent::Evicted { at: now }).await?;
            st.emit(
                id,
                SessionEvent::StatusChanged {
                    from: prev,
                    to: SessionState::Idle,
                    at: now,
                },
            )
            .await?;
            Ok::<_, ApiError>(())
        }
        .instrument(evict_span),
    );

    handle.await.map_err(|join_err| {
        ApiError::Internal(format!("evict_local pipeline task panicked: {join_err}"))
    })??;
    Ok(())
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

        let err = match resume_core(&state, id).await {
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

        crate::api::sessions::delete_session_core(&state, id)
            .await
            .expect("delete mid-eviction");
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

    /// Issue #210 idempotency guard: a backend that records every
    /// `destroy` so the resume path's residual-sandbox teardown is
    /// observable. All non-default methods delegate to a real
    /// [`ProcessBackend`] except `destroy`, which records the id (and
    /// still succeeds) so a resume can't double-restore over a residual
    /// binding without us seeing the cleanup.
    struct DestroyRecordingBackend {
        inner: ProcessBackend,
        destroyed: Arc<parking_lot::Mutex<Vec<SandboxId>>>,
    }

    #[async_trait::async_trait]
    impl SandboxBackend for DestroyRecordingBackend {
        async fn create(
            &self,
            spec: engram_core::types::sandbox::SandboxSpec,
        ) -> Result<SandboxId, SandboxError> {
            self.inner.create(spec).await
        }
        async fn snapshot(
            &self,
            id: SandboxId,
        ) -> Result<engram_core::types::snapshot::SnapshotMetadata, SandboxError> {
            self.inner.snapshot(id).await
        }
        async fn restore(
            &self,
            metadata: engram_core::types::snapshot::SnapshotMetadata,
        ) -> Result<SandboxId, SandboxError> {
            self.inner.restore(metadata).await
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.destroyed.lock().push(id);
            // Delegate too — a ProcessBackend destroy of an unknown
            // sandbox is an idempotent no-op, matching the real host.
            let _ = self.inner.destroy(id).await;
            Ok(())
        }
        async fn exec_stream(
            &self,
            id: SandboxId,
            cmd: engram_core::types::sandbox::ExecRequest,
        ) -> Result<engram_core::types::sandbox::ExecStream, SandboxError> {
            self.inner.exec_stream(id, cmd).await
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            self.inner.list().await
        }
        fn snapshot_path_for(
            &self,
            snapshot_id: engram_core::types::SnapshotId,
        ) -> std::path::PathBuf {
            self.inner.snapshot_path_for(snapshot_id)
        }
    }

    fn idle_session_with_residual_sandbox(
        id: SessionId,
        host: engram_core::HostId,
        residual: SandboxId,
    ) -> Session {
        Session {
            id,
            // Idle, but still carrying a host + sandbox binding — the
            // residue of a prior resume that restored a VM and bound it
            // (`bind_resumed_session` writes host then sandbox) and then
            // died before advancing the row off Idle (the crash window the
            // spawn detachment can't cover). The owner is therefore
            // resolvable, exactly as it would be in production.
            status: SessionState::Idle,
            host_id: Some(host),
            sandbox_id: Some(residual),
            image: "test/repo:residual-resume".into(),
            mode: SessionMode::Agent,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            live_disk_manifest: None,
        }
    }

    /// Issue #210: resuming an `Idle` session that still carries a
    /// residual `sandbox_id` (a prior resume restored a VM, bound it,
    /// then crashed before leaving `Idle`) MUST destroy that residual
    /// sandbox and clear the binding BEFORE restoring fresh — otherwise
    /// the fresh restore overwrites the binding and orphans the first VM
    /// (double-restore: two guests executing the same restored state).
    ///
    /// We seed no recoverable snapshot, so after the cleanup the resume
    /// legitimately fails `Gone` (snapshot_invalidated) — but the
    /// observable cleanup (destroy of the residual + binding cleared) is
    /// the proof the idempotency guard ran ahead of any fresh restore.
    /// On the pre-fix code the guard does not exist: `destroy` is never
    /// called and the binding survives into the (attempted) fresh
    /// restore.
    #[tokio::test]
    async fn resume_from_idle_destroys_residual_sandbox_before_restoring() {
        let id = SessionId::new();
        let host = engram_core::HostId::new();
        let residual = SandboxId::new();
        let destroyed = Arc::new(parking_lot::Mutex::new(Vec::new()));

        // Build state with the recording backend.
        let local = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> = Arc::new(DestroyRecordingBackend {
            inner: ProcessBackend::new(local.path().join("sandboxes")),
            destroyed: destroyed.clone(),
        });
        let meta = Arc::new(MiniMeta::new(idle_session_with_residual_sandbox(
            id, host, residual,
        )));
        // Stage the residual sandbox's owner so `host.destroy` resolves to
        // our recording backend (read-through to PG's host_for_sandbox,
        // which matches the session's host_id + sandbox_id).
        meta.add_ready_host(host);
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        let local_host: Arc<dyn engram_core::traits::HostClient> = Arc::new(
            engram_host_agent::LocalHostClient::with_noop_hub(backend.clone()),
        );
        host_registry.register(host, local_host);
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
        let state: SharedState =
            Arc::new(AppState::new_with_registry(cfg, services, host_registry));

        // Drive the full resume (acquires the lease, spawns the pipeline,
        // dispatches Idle → resume_from_idle).
        let err = match resume_session(state.clone(), id).await {
            Err(e) => e,
            Ok(_) => panic!("no snapshot seeded → resume must fail Gone after cleanup"),
        };
        assert!(
            err.to_string().contains("snapshot_invalidated"),
            "expected the no-snapshot resume failure after cleanup, got: {err}",
        );

        // The residual sandbox MUST have been destroyed before the fresh
        // restore was attempted. Snapshot the guarded state into owned
        // values and drop the guard before the awaits below — a
        // (parking_lot) `MutexGuard` held across an `.await` is a deadlock
        // hazard (`clippy::await_holding_lock`).
        let (saw_residual, destroyed_snapshot) = {
            let destroyed = destroyed.lock();
            (destroyed.contains(&residual), destroyed.clone())
        };
        assert!(
            saw_residual,
            "resume_from_idle must destroy the residual sandbox {residual} before \
             restoring fresh; destroyed = {destroyed_snapshot:?}",
        );

        // And the binding MUST have been cleared so nothing routes to the
        // torn-down sandbox (and so a fresh restore doesn't overwrite a
        // live binding).
        let after = state.services.meta.get_session(id).await.unwrap();
        assert_eq!(
            after.sandbox_id, None,
            "residual sandbox binding must be cleared during resume_from_idle cleanup",
        );
    }
}
