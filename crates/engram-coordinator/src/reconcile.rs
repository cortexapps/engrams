//! ADR 0009 reconciliation pass.
//!
//! Runs synchronously on every inbound `NotifyKind::Heartbeat` (in
//! `api/hosts.rs`). Intersects the host's `running_sandboxes`
//! against expected-`active` `sessions` rows for that host; sessions
//! whose `sandbox_id` is missing from `running_sandboxes` for N
//! consecutive heartbeats (default 3, ~15 s at the 5 s cadence)
//! transition per the missing-sandbox policy:
//!
//! - latest `snapshots` row has `recoverable = true`  → `Idle`
//!   (next user prompt rehydrates via the existing resume path).
//! - else → `Dead` (terminal).
//!
//! This closes case **B** from the failure-mode taxonomy (Active
//! sessions stuck pointing at sandbox_ids that no longer exist
//! anywhere). The grace window (3 heartbeats) is symmetric with
//! `dead_host.rs`'s 30 s heartbeat timeout and absorbs transient
//! `backend.list()` failures on the host side.
//!
//! Multi-replica safety (issue #211): there is intentionally NO
//! advisory lock here (an earlier version of this doc claimed one —
//! it never existed; only `dead_host.rs` takes a `pg_try_advisory_lock`).
//! Two replicas reconciling the same host concurrently are made safe by
//! idempotent, guarded writes instead: `transition_session` is a legality-
//! checked atomic CAS (a loser sees `Conflict`), and the `sandbox_id`
//! clear in [`flip_missing`] is a compare-and-swap on the EXACT sandbox
//! that struck out — so a replica acting on a stale strike (or racing a
//! live-migration `rebind_session`) can't null a freshly-landed binding.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::SessionState;
use engram_core::{HostId, SandboxId, SessionId};
use tokio::task::JoinHandle;

use crate::host_registry::HostRegistry;
use crate::state::{SessionEvent, SessionEventBus, SharedState};

/// Default number of consecutive heartbeats a sandbox must be
/// missing-from-host before the reconcile pass transitions the
/// session. 3 × 5 s heartbeat = 15 s grace, in the same ballpark as
/// `dead_host.rs`'s 30 s heartbeat timeout — symmetric with how the
/// other host-loss detector treats transient absence.
pub const DEFAULT_GRACE_TICKS: u8 = 3;

/// Override via `ENGRAM_RECONCILE_GRACE_TICKS`. Reads at coord
/// startup; `0` is clamped to 1 (the strike-N-times pattern is
/// load-bearing — without it a single transient `backend.list()`
/// hiccup would mass-flip sessions).
pub fn grace_ticks_from_env() -> u8 {
    std::env::var("ENGRAM_RECONCILE_GRACE_TICKS")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .map(|n| n.max(1))
        .unwrap_or(DEFAULT_GRACE_TICKS)
}

/// Strike-driven flipper. ADR 0047: the counter itself lives on the
/// session row (`sessions.missing_strikes`,
/// `MetadataStore::apply_missing_sandbox_strikes`) so the "missing N
/// CONSECUTIVE heartbeats" semantics hold when a host's heartbeats
/// round-robin across coordinator replicas — per-pod counters would
/// miss the resets that land on siblings and flip healthy sessions.
#[derive(Clone, Default)]
pub struct Reconciler {
    grace_ticks: u8,
}

impl Reconciler {
    pub fn new(grace_ticks: u8) -> Self {
        Self {
            grace_ticks: grace_ticks.max(1),
        }
    }

    /// Reconcile this host's view via the live `SharedState`. Thin
    /// wrapper around [`Self::reconcile_with_deps`] for the heartbeat
    /// handler in `api/hosts.rs`. Tests bypass this in favour of
    /// `reconcile_with_deps` so they don't have to stand up a full
    /// Services struct.
    pub async fn reconcile_host(
        &self,
        state: &SharedState,
        host_id: HostId,
        running_sandboxes: &[SandboxId],
    ) -> Vec<SessionId> {
        self.reconcile_with_deps(
            state.services.meta.as_ref(),
            &state.events,
            &state.host_registry,
            host_id,
            running_sandboxes,
        )
        .await
    }

    /// Dependency-injected version. Called from `reconcile_host` and
    /// tests. Returns the list of session-ids that crossed the strike
    /// threshold and got flipped this tick — caller can log / emit
    /// events / surface in `/api/admin/reconcile-now` responses.
    ///
    /// `host_registry` is consulted on every flip so the M3
    /// invariant "PG and the in-memory cache move together" holds:
    /// the sandbox-owner row gets dropped before we clear the DB,
    /// avoiding the window where a concurrent reader sees a stale
    /// in-memory route after PG already knows the session is
    /// HostLost.
    pub async fn reconcile_with_deps(
        &self,
        meta: &dyn MetadataStore,
        events: &SessionEventBus,
        host_registry: &HostRegistry,
        host_id: HostId,
        running_sandboxes: &[SandboxId],
    ) -> Vec<SessionId> {
        let running: HashSet<SandboxId> = running_sandboxes.iter().copied().collect();
        let assignments = match meta.list_active_sandbox_assignments_on_host(host_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(host_id = %host_id, error = %e, "reconcile: meta query failed; skipping tick");
                return Vec::new();
            }
        };

        // Precompute the per-session sandbox_id from the assignments
        // we already pulled — `flip_missing` needs it to invalidate
        // the HostRegistry cache, and an extra `get_session` round-
        // trip just to recover it would be wasteful.
        let sandbox_by_session: HashMap<SessionId, SandboxId> =
            assignments.iter().copied().collect();

        // ADR 0047: strike accounting happens on the session rows so
        // every replica shares one counter.
        let mut present: Vec<SessionId> = Vec::new();
        let mut missing: Vec<SessionId> = Vec::new();
        for (session_id, sandbox_id) in &assignments {
            if running.contains(sandbox_id) {
                present.push(*session_id);
            } else {
                missing.push(*session_id);
            }
        }
        let to_flip = match meta
            .apply_missing_sandbox_strikes(&present, &missing, self.grace_ticks as i32)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(host_id = %host_id, error = %e,
                    "reconcile: strike accounting failed; skipping tick");
                return Vec::new();
            }
        };

        for session_id in &to_flip {
            let sb = sandbox_by_session.get(session_id).copied();
            flip_missing(meta, events, host_registry, *session_id, host_id, sb).await;
        }
        to_flip
    }
}

async fn flip_missing(
    meta: &dyn MetadataStore,
    events: &SessionEventBus,
    host_registry: &HostRegistry,
    session_id: SessionId,
    host_id: HostId,
    sandbox_id: Option<SandboxId>,
) {
    // Cheap idempotency: if the session is already in target state
    // (or terminal beyond it), skip. Avoids racing with an operator
    // who manually killed the session between the strike-out and now.
    let session = match meta.get_session(session_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(session_id = %session_id, error = %e, "reconcile: get_session failed; skipping flip");
            return;
        }
    };
    let prev = session.status;
    if prev.is_terminal() || matches!(prev, SessionState::Idle | SessionState::HostLost) {
        tracing::debug!(
            session_id = %session_id,
            ?prev,
            "reconcile: session already in non-active state; skipping flip"
        );
        return;
    }

    // ADR 0015 M3: drop the HostRegistry cache row *before* we
    // touch PG. Order matters — a reader that races us either:
    //   - takes the in-memory miss path and asks PG (which still
    //     says Active, but the host already failed reconcile so
    //     subsequent RPCs error out fast), or
    //   - sees the fresh PG state we're about to write (HostLost
    //     → SandboxError::HostLost → 410).
    // The reverse order would briefly admit a window where a
    // reader hits the cache fast path against a host the
    // reconciler already knows is missing this sandbox.
    let sandbox_id = sandbox_id.or(session.sandbox_id);
    if let Some(sb) = sandbox_id {
        if let Some(prev_host) = host_registry.invalidate_sandbox(sb) {
            tracing::debug!(
                session_id = %session_id,
                sandbox_id = %sb,
                prev_host = %prev_host,
                "reconcile: invalidated HostRegistry cache before HostLost transition"
            );
        }
    }

    // Clear sandbox_id so coord routing and a future restart's
    // `repopulate_routing` don't try to talk to the dead sandbox.
    //
    // Issue #211: this MUST be a compare-and-swap on the EXACT sandbox
    // that struck out, not a blind `WHERE id = $1` clear. A live
    // migration's `rebind_session` can land a FRESH sandbox onto this
    // row between our strike-out decision and this clear; a blind null
    // would wipe that healthy binding and drive a just-migrated session
    // to HostLost→Idle/Dead. With the CAS, if the row no longer points
    // at the struck-out sandbox we abort the whole flip (the binding
    // moved on — the session is not orphaned). When `sandbox_id` is
    // None (the row already had no binding) we fall back to the blind
    // clear: there is nothing for a rebind to have replaced.
    let clear_result = match sandbox_id {
        Some(struck) => {
            meta.assign_session_sandbox_guarded(session_id, None, Some(Some(struck)), &[])
                .await
        }
        None => meta.assign_session_sandbox(session_id, None).await,
    };
    if let Err(e) = clear_result {
        if matches!(e, engram_core::MetaError::Conflict(_)) {
            tracing::info!(
                session_id = %session_id,
                error = %e,
                "reconcile: sandbox binding changed since strike-out (likely a fresh \
                 rebind) — aborting flip so we don't null a healthy binding"
            );
            return;
        }
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "reconcile: clearing sandbox_id failed; continuing with status flip"
        );
    }

    // ADR 0015 M2 stage 1: Active -> HostLost (host went away).
    let prev = match meta
        .transition_session(session_id, SessionState::HostLost)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "reconcile: transition to HostLost failed; will retry next tick"
            );
            return;
        }
    };
    emit_status_changed(meta, events, session_id, prev, SessionState::HostLost).await;

    // ADR 0015 M2 stage 2: HostLost -> {Idle if recoverable
    // snapshot, Dead otherwise}. The `recoverable` column carries the
    // result of the BlobStorage HEAD check at snapshot-take time —
    // false here means even an Idle-ready snapshot wouldn't survive a
    // /resume request.
    let recoverable = match meta.latest_snapshot_for_session(session_id).await {
        Ok(Some(s)) => s.recoverable,
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "reconcile: latest_snapshot_for_session failed; treating as not-recoverable"
            );
            false
        }
    };
    let new_status = if recoverable {
        SessionState::Idle
    } else {
        SessionState::Dead
    };
    match meta.transition_session(session_id, new_status).await {
        Ok(host_lost_prev) => {
            emit_status_changed(meta, events, session_id, host_lost_prev, new_status).await;
            tracing::info!(
                session_id = %session_id,
                host_id = %host_id,
                final_state = ?new_status,
                recoverable,
                "ADR 0009 reconcile: orphaned session moved through HostLost"
            );
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                ?new_status,
                "reconcile: HostLost second-stage transition failed; leaving at HostLost"
            );
        }
    }
}

async fn emit_status_changed(
    meta: &dyn MetadataStore,
    events: &SessionEventBus,
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
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "reconcile: status-changed event serialize failed"
            );
            return;
        }
    };
    match meta.append_session_event(session_id, kind, payload).await {
        Ok(idx) => {
            events.publish(
                session_id,
                crate::state::IndexedEvent {
                    idx,
                    event,
                    ephemeral: false,
                },
            );
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "reconcile: append_session_event failed; flip recorded but no event emitted"
            );
        }
    }
}

/// Default cadence for the in-process reconciliation driver
/// (`spawn_in_proc`). Matches the heartbeat interval — both paths
/// run reconcile every 5 s.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn the in-process reconciliation driver. Required in
/// `--mode=all` (single-process coord + host) where there's no WS
/// path delivering `NotifyKind::Heartbeat` — the WS-handler reconcile
/// hook (`api/hosts.rs`) never fires, and without this driver the
/// pass would never run. In `--mode=coordinator` the WS handler
/// already drives reconcile for each remote host; this driver is
/// redundant there and the spawn site should skip it.
///
/// Walks every host in the registry on each tick, calls
/// `backend.list()`, and feeds the result through the shared
/// `Reconciler`. Errors during `list()` are logged and absorbed by
/// the strike-counter grace window (the host's sandboxes drop to an
/// empty set for one tick; next tick recovers).
pub fn spawn_in_proc(state: SharedState, tick: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        // Skip the immediate-fire first tick so a coord that's just
        // bound the local backend doesn't insta-strike a session
        // whose create() is mid-flight.
        interval.tick().await;
        loop {
            interval.tick().await;
            let host_ids = state.host_registry.host_ids();
            for host_id in host_ids {
                let Some(backend) = state.host_registry.backend_of(host_id) else {
                    continue;
                };
                // Issue #215: a `backend.list()` error is "no
                // information", NOT "no sandboxes running". Feeding an
                // empty set into `reconcile_host` would strike EVERY
                // active session on this host that tick — a single RPC
                // hiccup eats a grace tick from all of them. An RPC
                // failure is not evidence the sandboxes are gone, so we
                // skip this host's tick entirely and recover next tick.
                let running = match backend.list().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            host_id = %host_id,
                            error = %e,
                            "in-proc reconcile: backend.list() failed; skipping tick (no info — not striking)"
                        );
                        continue;
                    }
                };
                let flipped = state
                    .reconciler
                    .reconcile_host(&state, host_id, &running)
                    .await;
                if !flipped.is_empty() {
                    tracing::info!(
                        host_id = %host_id,
                        count = flipped.len(),
                        "in-proc reconcile flipped missing-sandbox sessions"
                    );
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grace_ticks_clamps_to_one() {
        let r = Reconciler::new(0);
        assert_eq!(r.grace_ticks, 1);
    }

    #[test]
    fn default_grace_is_three() {
        assert_eq!(DEFAULT_GRACE_TICKS, 3);
    }
}
