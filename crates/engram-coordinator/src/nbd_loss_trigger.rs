//! ADR 0018 Phase B: NBD-loss-triggered evacuation.
//!
//! Heartbeat handler (api/host_http.rs) calls
//! [`process_unhealthy`] on every tick where
//! `hb.nbd_unhealthy` is non-empty. We treat the affected sandboxes
//! as source-disk-only-available — the source host is still alive but
//! its `/dev/nbdN` is degraded, so a fresh `host.snapshot()` can't
//! reliably read disk to capture memory. Per ADR 0016 §"What gets
//! easier", we relocate via the dead-source primitive: existing
//! snapshot row + `sessions.live_disk_manifest_*` carry the state to
//! a peer, with `EvacLoss::Memory` recorded when no fresh memory
//! manifest is available.
//!
//! Gated by `ENGRAM_NBD_AUTO_EVAC=1` (default off, matching commit 3's
//! `ENGRAM_DEAD_HOST_AUTO_EVAC` gate). Until the
//! resume-from-Created path lands and auto-evac becomes stuck-free,
//! the relocate leaves the session at `Created` on the new host —
//! ship the trigger behind a flag so prod can validate gradually.
//!
//! Per `[explicit_admin_triggers_for_testability]`: the wire seam is
//! the heartbeat field; tests inject via the host-agent's
//! `NbdHealthMonitor`. Commit 7's admin endpoint will route admin
//! traffic through the same primitive.

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::SessionState;
use engram_core::{HostId, SandboxId, SessionId};

use crate::evacuation::{evacuate_dead_source, EvacError};
use crate::state::{IndexedEvent, SessionEvent, SessionEventBus, SharedState};

/// ADR 0018 commit 10: defaults to **true** now that the
/// resume-from-Created completion path is in place. The handler
/// drives auto-evac'd sessions through `finish_resume_to_active`
/// after the dead-source primitive returns, so the session reaches
/// Active on the peer rather than stranding at Created. Operators
/// can flip `ENGRAM_NBD_AUTO_EVAC=0` to roll back to "leave at
/// HostLost; user /resumes" if a regression surfaces.
fn auto_evac_enabled() -> bool {
    std::env::var("ENGRAM_NBD_AUTO_EVAC")
        .ok()
        .map(|v| !(v == "0" || v.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
}

/// Emit a `StatusChanged{from, to}` for an evac'd session. Same shape
/// the dead_host.rs second-stage uses — kept inline so the trigger
/// module doesn't take a full SharedState dependency.
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

/// Resolve `sandbox_id` → `session_id` by walking active sessions.
/// O(active_sessions) per lookup; the hot path is empty
/// `nbd_unhealthy` so this only fires when the host reports
/// degradation. Falls back to None on PG hiccup.
async fn session_for_sandbox(
    meta: &Arc<dyn MetadataStore>,
    sandbox_id: SandboxId,
) -> Option<SessionId> {
    let sessions = match meta.list_active_sessions().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                %sandbox_id,
                error = %e,
                "session_for_sandbox: list_active_sessions failed",
            );
            return None;
        }
    };
    sessions
        .into_iter()
        .find(|s| s.sandbox_id == Some(sandbox_id))
        .map(|s| s.id)
}

/// Drive evac for every unhealthy sandbox in `sandbox_ids`. Skips
/// when the auto-evac flag is off. Best-effort per-sandbox — a
/// failure on one doesn't block the rest.
///
/// The state-machine drive is Active → HostLost → Created on the
/// new host: this trigger fires from the heartbeat handler while
/// the source host is still considered Active (only its NBD is
/// degraded), so we flip to HostLost ourselves before handing off
/// to `evacuate_dead_source` (which expects the session already at
/// HostLost-class from the dead_host.rs flow).
pub async fn process_unhealthy(
    state: &SharedState,
    source_host_id: HostId,
    sandbox_ids: &[SandboxId],
) {
    if sandbox_ids.is_empty() {
        return;
    }
    if !auto_evac_enabled() {
        tracing::debug!(
            host_id = %source_host_id,
            count = sandbox_ids.len(),
            "nbd-unhealthy sandboxes reported; auto-evac disabled via ENGRAM_NBD_AUTO_EVAC=0",
        );
        return;
    }
    let meta = &state.services.meta;
    let registry = &state.host_registry;
    let events = &state.events;

    for sandbox_id in sandbox_ids {
        let sandbox_id = *sandbox_id;
        let session_id = match session_for_sandbox(meta, sandbox_id).await {
            Some(id) => id,
            None => {
                tracing::warn!(
                    %sandbox_id,
                    host_id = %source_host_id,
                    "nbd-unhealthy: no active session for sandbox; skipping",
                );
                continue;
            }
        };

        // Drive the session into HostLost so evacuate_dead_source's
        // expected entry state is satisfied. The previous state is
        // returned from transition_session; use it as the `from` for
        // the emitted StatusChanged.
        let prev_state = match meta
            .transition_session(session_id, SessionState::HostLost)
            .await
        {
            Ok(prev) => prev,
            Err(e) => {
                tracing::warn!(
                    %session_id,
                    %sandbox_id,
                    error = %e,
                    "nbd-unhealthy: Active→HostLost transition failed; skipping evac for this sandbox",
                );
                continue;
            }
        };
        emit_status_changed(meta, events, session_id, prev_state, SessionState::HostLost).await;

        // Load the session row + latest snapshot for the dead-source
        // primitive. Failures log and skip — the session stays at
        // HostLost and the existing reconcile / operator paths can
        // pick it up.
        let session = match meta.get_session(session_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "nbd-unhealthy: get_session failed during evac",
                );
                continue;
            }
        };
        let snapshot = match meta.latest_snapshot_for_session(session_id).await {
            Ok(opt) => opt,
            Err(e) => {
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "nbd-unhealthy: snapshot lookup failed during evac",
                );
                continue;
            }
        };

        match evacuate_dead_source(registry, meta, session, snapshot).await {
            Ok(receipt) => {
                tracing::info!(
                    %session_id,
                    %sandbox_id,
                    source_host = %source_host_id,
                    new_host = %receipt.new_host_id,
                    new_sandbox = %receipt.new_sandbox_id,
                    loss = receipt.loss.as_str(),
                    "nbd-unhealthy evac: relocated to peer at Created — finishing harness rebuild",
                );
                emit_status_changed(
                    meta,
                    events,
                    session_id,
                    SessionState::HostLost,
                    SessionState::Created,
                )
                .await;
                // Coord-side + host-agent session→sandbox binding.
                // Same shape as admin evac / dead_host's path.
                crate::api::snapshot::bind_session_routing(
                    state,
                    session_id,
                    receipt.new_sandbox_id,
                )
                .await;
                // ADR 0018 commit 10: finish to Active via the
                // shared resume primitive. Same harness rebuild
                // path /resume + admin evac use.
                let session_refreshed = match meta.get_session(session_id).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            %session_id,
                            error = %e,
                            "nbd-unhealthy: get_session after rebind failed; session left at Created",
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
                            "nbd-unhealthy evac: session reached Active on peer host",
                        );
                    }
                    Ok(crate::api::snapshot::FinishResumeOutcome::CreatedHarnessFailed) => {
                        tracing::warn!(
                            %session_id,
                            "nbd-unhealthy evac: harness rebuild failed on peer; session left at Created",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            %session_id,
                            error = %e,
                            "nbd-unhealthy evac: finish_resume_to_active errored; session left at Created",
                        );
                    }
                }
            }
            Err(EvacError::NoRecoverableState) => {
                // No state to relocate. Drive to Dead.
                if let Err(e) = meta
                    .transition_session(session_id, SessionState::Dead)
                    .await
                {
                    tracing::warn!(
                        %session_id,
                        error = %e,
                        "nbd-unhealthy: HostLost→Dead transition failed",
                    );
                } else {
                    emit_status_changed(
                        meta,
                        events,
                        session_id,
                        SessionState::HostLost,
                        SessionState::Dead,
                    )
                    .await;
                }
            }
            Err(e) => {
                tracing::warn!(
                    %session_id,
                    %sandbox_id,
                    error = %e,
                    "nbd-unhealthy evac failed; session left at HostLost",
                );
            }
        }
    }
}

// Tests intentionally minimal here. `auto_evac_enabled` reads an env
// var — env-var unit tests race when cargo runs them concurrently in
// the same process, so we cover the full trigger path via the
// Postgres integration test (commit 8, env-set in-process) and the
// e2e_evac CI test (commit 8a, env-set per process).
//
// The dead-source primitive that `process_unhealthy` dispatches to is
// covered by the 8 `evacuation::tests::evac_dead_source_*` tests in
// commit 3 — those exercise the load-bearing mechanics under a mock
// HostClient + mock MetadataStore.
