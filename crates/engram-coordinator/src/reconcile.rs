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
//! Multi-replica coord deployments use `pg_try_advisory_lock` (the
//! same primitive as the dead-host detector) so two replicas can't
//! both flip the same session simultaneously.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::SessionState;
use engram_core::{HostId, SandboxId, SessionId};
use parking_lot::Mutex;
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

/// Per-coord strikes counter. A session that's present in the
/// host's heartbeat resets its entry to zero; a missing session
/// increments; reaching `grace_ticks` triggers the flip + clears
/// the entry so a re-emerging sandbox starts fresh.
///
/// In-memory + per-coord. On coord restart the counter resets and
/// the next 3 heartbeats re-build it from scratch — at worst a 15 s
/// delay after a coord redeploy before the first flip. Acceptable.
#[derive(Clone, Default)]
pub struct Reconciler {
    strikes: Arc<Mutex<HashMap<SessionId, u8>>>,
    grace_ticks: u8,
}

impl Reconciler {
    pub fn new(grace_ticks: u8) -> Self {
        Self {
            strikes: Arc::new(Mutex::new(HashMap::new())),
            grace_ticks: grace_ticks.max(1),
        }
    }

    /// Snapshot the strikes map. Useful for telemetry / tests; not
    /// hot-path.
    #[allow(dead_code)]
    pub fn snapshot_strikes(&self) -> HashMap<SessionId, u8> {
        self.strikes.lock().clone()
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

        let to_flip = {
            let mut strikes = self.strikes.lock();
            apply_strikes(&mut strikes, &assignments, &running, self.grace_ticks)
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
    if let Err(e) = meta.assign_session_sandbox(session_id, None).await {
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
            events.publish(session_id, crate::state::IndexedEvent { idx, event });
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
                let running = match backend.list().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            host_id = %host_id,
                            error = %e,
                            "in-proc reconcile: backend.list() failed; reporting empty (strike grace absorbs)"
                        );
                        Vec::new()
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

/// Pure strikes-counter update. Extracted from `reconcile_host` so
/// the load-bearing decision logic is testable without standing up
/// a full `SharedState`. Mutates `strikes`; returns the set of
/// sessions whose strike count just hit `grace_ticks` and should be
/// flipped (and entries removed from `strikes` to start fresh on a
/// re-emerging sandbox).
fn apply_strikes(
    strikes: &mut HashMap<SessionId, u8>,
    assignments: &[(SessionId, SandboxId)],
    running: &HashSet<SandboxId>,
    grace_ticks: u8,
) -> Vec<SessionId> {
    let mut to_flip = Vec::new();
    for (session_id, sandbox_id) in assignments {
        if running.contains(sandbox_id) {
            strikes.remove(session_id);
        } else {
            let s = strikes.entry(*session_id).or_insert(0);
            *s = s.saturating_add(1);
            if *s >= grace_ticks {
                to_flip.push(*session_id);
                strikes.remove(session_id);
            }
        }
    }
    to_flip
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

    fn assignment() -> (SessionId, SandboxId) {
        (SessionId::new(), SandboxId::new())
    }

    #[test]
    fn present_sandbox_resets_strikes() {
        // Even if the sandbox was missing for a few ticks, a single
        // present-heartbeat resets to zero (no flip carry-over).
        let (sid, sb) = assignment();
        let mut strikes = HashMap::new();
        strikes.insert(sid, 2);
        let running: HashSet<_> = [sb].into_iter().collect();
        let flipped = apply_strikes(&mut strikes, &[(sid, sb)], &running, 3);
        assert!(flipped.is_empty());
        assert!(!strikes.contains_key(&sid), "present resets to zero");
    }

    #[test]
    fn missing_sandbox_accumulates_one_strike_per_tick() {
        let (sid, sb) = assignment();
        let mut strikes = HashMap::new();
        let running = HashSet::new();
        apply_strikes(&mut strikes, &[(sid, sb)], &running, 5);
        assert_eq!(strikes.get(&sid).copied(), Some(1));
        apply_strikes(&mut strikes, &[(sid, sb)], &running, 5);
        assert_eq!(strikes.get(&sid).copied(), Some(2));
    }

    #[test]
    fn flips_on_third_consecutive_missing_with_default_grace() {
        let (sid, sb) = assignment();
        let mut strikes = HashMap::new();
        let running = HashSet::new();
        let flipped1 = apply_strikes(&mut strikes, &[(sid, sb)], &running, 3);
        let flipped2 = apply_strikes(&mut strikes, &[(sid, sb)], &running, 3);
        let flipped3 = apply_strikes(&mut strikes, &[(sid, sb)], &running, 3);
        assert!(flipped1.is_empty());
        assert!(flipped2.is_empty());
        assert_eq!(flipped3, vec![sid]);
        // After flip, the entry is cleared so a re-emerging sandbox
        // starts fresh — and a never-resolved missing won't keep
        // accumulating strikes (we already flipped it once).
        assert!(!strikes.contains_key(&sid));
    }

    #[test]
    fn re_present_after_two_strikes_resets_and_no_flip() {
        // Common in-the-wild scenario: backend.list() blips for two
        // ticks then recovers. Reconcile must NOT flip.
        let (sid, sb) = assignment();
        let mut strikes = HashMap::new();
        let empty = HashSet::new();
        let present: HashSet<_> = [sb].into_iter().collect();
        apply_strikes(&mut strikes, &[(sid, sb)], &empty, 3);
        apply_strikes(&mut strikes, &[(sid, sb)], &empty, 3);
        let flipped = apply_strikes(&mut strikes, &[(sid, sb)], &present, 3);
        assert!(flipped.is_empty(), "recovery before grace must not flip");
        assert!(!strikes.contains_key(&sid));
    }

    #[test]
    fn many_sessions_strike_independently() {
        // Per-session strike counters are independent: one session's
        // strikes do not affect another's.
        let (sid_a, sb_a) = assignment();
        let (sid_b, sb_b) = assignment();
        let mut strikes = HashMap::new();
        // sb_a missing for two ticks; sb_b present every tick.
        let present_b: HashSet<_> = [sb_b].into_iter().collect();
        apply_strikes(&mut strikes, &[(sid_a, sb_a), (sid_b, sb_b)], &present_b, 3);
        apply_strikes(&mut strikes, &[(sid_a, sb_a), (sid_b, sb_b)], &present_b, 3);
        assert_eq!(strikes.get(&sid_a).copied(), Some(2));
        assert!(!strikes.contains_key(&sid_b));
        // Third tick: sb_a flips, sb_b unaffected.
        let flipped = apply_strikes(&mut strikes, &[(sid_a, sb_a), (sid_b, sb_b)], &present_b, 3);
        assert_eq!(flipped, vec![sid_a]);
    }

    #[test]
    fn grace_one_flips_on_first_missing() {
        // grace_ticks=1 is the no-grace mode — useful for the future
        // operator endpoint `POST /api/admin/reconcile-now`.
        let (sid, sb) = assignment();
        let mut strikes = HashMap::new();
        let running = HashSet::new();
        let flipped = apply_strikes(&mut strikes, &[(sid, sb)], &running, 1);
        assert_eq!(flipped, vec![sid]);
    }

    #[test]
    fn unknown_sandbox_in_running_set_is_ignored() {
        // Host reporting sandboxes the coord doesn't know about (e.g.
        // sessions on a different host, or sandboxes spawned outside
        // platform control) must not affect the strikes counter for
        // sessions we ARE tracking.
        let (sid, sb) = assignment();
        let mut strikes = HashMap::new();
        let foreign_sb = SandboxId::new();
        let running: HashSet<_> = [foreign_sb].into_iter().collect();
        let flipped = apply_strikes(&mut strikes, &[(sid, sb)], &running, 3);
        assert!(flipped.is_empty());
        assert_eq!(strikes.get(&sid).copied(), Some(1));
    }
}
