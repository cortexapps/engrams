//! Dead-host auto-detector.
//!
//! Background task that polls for hosts whose `last_heartbeat_at` is
//! older than the configured threshold and races other coordinator
//! replicas (via Postgres advisory locks) for the right to evacuate
//! each candidate. The winner:
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
//! 4. Fires `pg_notify('host_dead', host_id::text)` so other replicas
//!    drop the host from their in-memory `HostRegistry` (handled in
//!    `pg_listener`).
//! 5. Unregisters the host locally.
//!
//! Active execs running on the dead host don't need explicit
//! synthesis: dropping the host's `RemoteSandboxBackend` cascades
//! through the WS demuxer's `Closed` state, the per-exec stream
//! channel drops, and the SSE handler in `api/exec.rs` emits
//! `SessionEvent::ExecCompleted{exit_status: None}` at the natural
//! end of its event loop — same path as a clean exec exit.
//!
//! Without this detector, sessions on a dead host stay `Active`
//! forever (with a `host_id` pointing at a host that won't respond);
//! operators can still `POST /sessions/:id/migrate` by hand.
//!
//! **ADR 0018 Phase B (auto-evac):** when
//! `ENGRAM_DEAD_HOST_AUTO_EVAC=1`, the second-stage routes
//! `HostLost → Created` on a peer host via
//! `evacuation::evacuate_dead_source` whenever recoverable state
//! exists (snapshot row OR `sessions.live_disk_manifest_*`). The
//! fall-through to `HostLost → Dead` stays for the no-state case;
//! the legacy `HostLost → Idle` path stays as the default until the
//! /resume-from-Created completion path lands (a follow-up that
//! extends `api/snapshot.rs::resume_session`). Until then, auto-
//! evac'd sessions end at Created on a peer and require operator-
//! initiated restart of start_agent to reach Active.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::SessionState;
use engram_core::HostId;
use sqlx::postgres::PgPool;

use crate::evacuation::{evacuate_dead_source, EvacError};
use crate::state::{IndexedEvent, SessionEvent, SessionEventBus, SharedState};
use engram_core::SessionId;

/// Read the auto-evac flag. Defaults to **true** as of commit 10 —
/// the resume-from-Created completion path now lands, so an auto-
/// evac'd session reaches Active on a peer via the shared
/// `finish_resume_to_active` primitive instead of stranding at
/// Created. Operators can flip `ENGRAM_DEAD_HOST_AUTO_EVAC=0` to
/// roll back to the legacy HostLost → Idle (user /resumes) path if
/// a regression surfaces.
fn auto_evac_enabled() -> bool {
    std::env::var("ENGRAM_DEAD_HOST_AUTO_EVAC")
        .ok()
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
}

/// Persist + publish a `StatusChanged{from, to}` event. Inline here
/// (rather than via `SharedState::emit`) so the dead-host detector
/// can run without a full Services struct — same shape the reconcile
/// pass uses. Errors are logged and swallowed because the persisted
/// state already changed; rolling back the eviction is worse than a
/// missed SSE event.
async fn emit_status_changed(
    meta: &Arc<dyn MetadataStore>,
    events: &Arc<SessionEventBus>,
    session_id: SessionId,
    from: SessionState,
    to: SessionState,
) {
    let event = SessionEvent::StatusChanged {
        from,
        to,
        at: Utc::now(),
    };
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
            events.publish(session_id, IndexedEvent { idx, event });
        }
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "persist StatusChanged failed");
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeadHostConfig {
    /// How often to poll for stale hosts. The dead-host detection
    /// latency is `poll_interval + threshold` worst-case; with
    /// defaults that's 30s + 10s = 40s, just inside the
    /// DESIGN.md:700 deliverable target of 30s migration on
    /// `kill -9`. Tighten for stricter targets.
    pub poll_interval: Duration,
    /// A host is considered dead when its `last_heartbeat_at` is
    /// older than this. Default 30s (~6× the 5s heartbeat cadence)
    /// — comfortable margin for transient network blips, fast
    /// enough to catch real failures.
    pub stale_threshold: Duration,
}

impl Default for DeadHostConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            stale_threshold: Duration::from_secs(30),
        }
    }
}

/// Spawn the detector as a background task. Returns a JoinHandle the
/// caller can drop on shutdown. Runs forever; logs and continues on
/// per-tick errors so a transient Postgres blip doesn't stop the loop.
///
/// Takes the full `SharedState` so the auto-evac path can call into
/// `api::snapshot::finish_resume_to_active` to drive the relocated
/// session all the way through harness rebuild → Active (the same
/// primitive `/resume` uses).
pub fn spawn(
    cfg: DeadHostConfig,
    pool: PgPool,
    state: SharedState,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick — the coordinator just
        // started and no host has had time to be considered stale.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &pool, &state).await {
                tracing::warn!(error = %e, "dead-host detector tick failed; will retry");
            }
        }
    })
}

async fn run_once(
    cfg: &DeadHostConfig,
    pool: &PgPool,
    state: &SharedState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = state
        .services
        .meta
        .list_stale_hosts(cfg.stale_threshold.as_secs())
        .await?;
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "dead-host detector found stale candidates"
    );
    for host in candidates {
        if let Err(e) = evict_host(pool, state, host.id).await {
            tracing::warn!(host_id = %host.id, error = %e, "evict failed; another replica may have it");
        }
    }
    Ok(())
}

async fn evict_host(
    pool: &PgPool,
    state: &SharedState,
    host_id: HostId,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let meta = &state.services.meta;
    let host_registry = &state.host_registry;
    let events = &state.events;
    // Pin a single connection so the advisory lock stays with us for
    // the duration of the eviction. `pg_try_advisory_lock` is a
    // session-scoped lock and auto-releases when the connection
    // closes — so even if we panic mid-eviction, the lock doesn't
    // strand the host.
    let mut conn = pool.acquire().await?;
    let lock_key: String = format!("dead-host:{host_id}");

    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(&lock_key)
        .fetch_one(&mut *conn)
        .await?;
    if !got {
        // Another coordinator replica won the race for this host.
        // It will do the eviction; we just skip.
        tracing::debug!(host_id = %host_id, "advisory lock contested; skipping");
        return Ok(());
    }

    // Re-check the host's status *after* taking the lock — another
    // replica that already won may have flipped it to Dead in the
    // window between our `list_stale_hosts` and now.
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM hosts WHERE id = $1")
        .bind(host_id.as_uuid())
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    if matches!(status.as_deref(), Some("dead") | None) {
        tracing::debug!(host_id = %host_id, "host already dead; releasing lock");
        sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
            .bind(&lock_key)
            .execute(&mut *conn)
            .await?;
        return Ok(());
    }

    let affected = meta.mark_host_dead_and_orphan_sessions(host_id).await?;

    // Notify other replicas so they drop their HostRegistry entry.
    sqlx::query("SELECT pg_notify('host_dead', $1)")
        .bind(host_id.to_string())
        .execute(&mut *conn)
        .await?;

    // Stage 1: emit StatusChanged{prev -> HostLost} for every session
    // the bulk touched. The `prev` came back from the UPDATE so the
    // `from` is honest (not a hand-encoded `Active` that would lie if
    // the session had been Idle).
    for (session_id, prev) in &affected {
        emit_status_changed(meta, events, *session_id, *prev, SessionState::HostLost).await;
    }

    // Stage 2: per-session snapshot-aware second transition.
    //
    // ADR 0018 Phase B: when ENGRAM_DEAD_HOST_AUTO_EVAC=1 and the
    // session has recoverable state (snapshot row OR
    // sessions.live_disk_manifest_*), drive
    // `HostLost → Created` on a peer host via
    // `evacuate_dead_source`. Without the flag, fall back to the
    // legacy `HostLost → Idle` (user /resume) path. Either way, the
    // no-state branch is `HostLost → Dead`.
    //
    // Failures of any query/transition are logged and skipped; the
    // row stays at HostLost and a future reconcile pass (or operator
    // action) can move it on.
    let auto_evac = auto_evac_enabled();
    for (session_id, _) in &affected {
        let snapshot = match meta.latest_snapshot_for_session(*session_id).await {
            Ok(opt) => opt,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %session_id,
                    "snapshot lookup failed; leaving session at HostLost"
                );
                continue;
            }
        };

        if auto_evac {
            // Look up session row to read live_disk_manifest +
            // image. Failure leaves the row at HostLost — operator
            // can drive resolution.
            let session = match meta.get_session(*session_id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        %session_id,
                        "get_session failed during auto-evac; leaving at HostLost",
                    );
                    continue;
                }
            };

            match evacuate_dead_source(host_registry, meta, session, snapshot.clone()).await {
                Ok(receipt) => {
                    tracing::info!(
                        %session_id,
                        new_host = %receipt.new_host_id,
                        new_sandbox = %receipt.new_sandbox_id,
                        loss = receipt.loss.as_str(),
                        "auto-evac succeeded; rebound to peer at Created — finishing harness rebuild",
                    );
                    emit_status_changed(
                        meta,
                        events,
                        *session_id,
                        SessionState::HostLost,
                        SessionState::Created,
                    )
                    .await;
                    // Bind the new sandbox into coord's session→sandbox
                    // cache + the target host-agent. Without this, /exec
                    // against the session 404s (registry still points at
                    // the dead sandbox_id) and the publisher drops
                    // every flush ("sandbox not bound to a session").
                    crate::api::snapshot::bind_session_routing(
                        state,
                        *session_id,
                        receipt.new_sandbox_id,
                    )
                    .await;
                    // ADR 0018 commit 10: finish the resume dance.
                    // Loads the manifest bundle + secrets, resolves
                    // the harness, rebuilds egress policy with the
                    // new guest_ip, runs start_agent, and drives
                    // Created → Active. Failure here logs and leaves
                    // the session at Created — the user can /resume
                    // to retry from there.
                    let session_refreshed = match meta.get_session(*session_id).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(
                                %session_id,
                                error = %e,
                                "auto-evac: get_session after rebind failed; session left at Created",
                            );
                            continue;
                        }
                    };
                    match crate::api::snapshot::finish_resume_to_active(
                        state,
                        &session_refreshed,
                        receipt.new_sandbox_id,
                    )
                    .await
                    {
                        Ok(crate::api::snapshot::FinishResumeOutcome::Active) => {
                            tracing::info!(
                                %session_id,
                                "auto-evac: session reached Active on peer host",
                            );
                        }
                        Ok(crate::api::snapshot::FinishResumeOutcome::CreatedHarnessFailed) => {
                            tracing::warn!(
                                %session_id,
                                "auto-evac: harness rebuild failed on peer; session left at Created — user can /resume",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                %session_id,
                                error = %e,
                                "auto-evac: finish_resume_to_active errored; session left at Created",
                            );
                        }
                    }
                    continue;
                }
                Err(EvacError::NoRecoverableState) => {
                    // Fall through to the HostLost → Dead branch.
                }
                Err(e) => {
                    tracing::warn!(
                        %session_id,
                        error = %e,
                        "auto-evac failed; leaving session at HostLost",
                    );
                    continue;
                }
            }
        }

        // Legacy / no-recoverable-state path: HostLost → Idle (if
        // snapshot exists and auto-evac is off) or HostLost → Dead.
        // When auto-evac is on and we got here, it's because
        // evacuate_dead_source returned NoRecoverableState — go
        // straight to Dead, no point trying Idle (which would just
        // strand the user with no /resume option either).
        let target = match (auto_evac, snapshot.is_some()) {
            (true, _) => SessionState::Dead, // evac said no state; Idle wouldn't help
            (false, true) => SessionState::Idle,
            (false, false) => SessionState::Dead,
        };
        match meta.transition_session(*session_id, target).await {
            Ok(prev) => {
                emit_status_changed(meta, events, *session_id, prev, target).await;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %session_id,
                    ?target,
                    "HostLost second-stage transition failed; leaving session at HostLost"
                );
            }
        }
    }

    host_registry.unregister(host_id);
    tracing::info!(
        host_id = %host_id,
        sessions_orphaned = affected.len(),
        "host marked dead; sessions moved through HostLost to Idle/Dead",
    );

    sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&lock_key)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    // The detector's polling loop and advisory-lock dance are
    // Postgres-specific and require a live database to test
    // meaningfully. The trait-layer logic
    // (`mark_host_dead_and_orphan_sessions` semantics) is covered
    // by Mock-based tests in `tests/dead_host_mock.rs`. End-to-end
    // multi-replica behaviour is the live-Postgres test
    // (`#[ignore]`'d, gated behind dev-VM Docker compose).
}
