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

use chrono::Utc;
use engram_core::types::SessionStatus;
use engram_core::{HostId, SandboxId, SessionId};
use parking_lot::Mutex;

use crate::state::{SessionEvent, SharedState};

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

    /// Reconcile this host's view. Called from the heartbeat
    /// supervisor in `api/hosts.rs` on every inbound
    /// `NotifyKind::Heartbeat`. Returns the list of session-ids
    /// that crossed the strike threshold and got flipped this
    /// tick — caller can log / emit events / surface in
    /// `/api/admin/reconcile-now` responses.
    pub async fn reconcile_host(
        &self,
        state: &SharedState,
        host_id: HostId,
        running_sandboxes: &[SandboxId],
    ) -> Vec<SessionId> {
        let running: HashSet<SandboxId> = running_sandboxes.iter().copied().collect();
        let assignments = match state
            .services
            .meta
            .list_active_sandbox_assignments_on_host(host_id)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(host_id = %host_id, error = %e, "reconcile: meta query failed; skipping tick");
                return Vec::new();
            }
        };

        let to_flip = {
            let mut strikes = self.strikes.lock();
            apply_strikes(&mut strikes, &assignments, &running, self.grace_ticks)
        };

        for session_id in &to_flip {
            self.flip_missing(state, *session_id, host_id).await;
        }
        to_flip
    }

    async fn flip_missing(&self, state: &SharedState, session_id: SessionId, host_id: HostId) {
        // Recoverability check: latest snapshot's `recoverable`
        // column. Phase 2 of the rollout sets this true after HEAD-
        // verifying the chunked manifests are durable in BlobStorage.
        let recoverable = match state
            .services
            .meta
            .latest_snapshot_for_session(session_id)
            .await
        {
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
            SessionStatus::Idle
        } else {
            SessionStatus::Dead
        };

        // Cheap idempotency: if the session is already in target
        // state (or terminal beyond it), skip. Avoids racing with
        // an operator who manually killed the session between the
        // strike-out and now.
        let session = match state.services.meta.get_session(session_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session_id = %session_id, error = %e, "reconcile: get_session failed; skipping flip");
                return;
            }
        };
        let prev = session.status;
        if matches!(
            prev,
            SessionStatus::Idle
                | SessionStatus::Dead
                | SessionStatus::Completed
                | SessionStatus::Failed
        ) {
            tracing::debug!(
                session_id = %session_id,
                ?prev,
                "reconcile: session already in non-active state; skipping flip"
            );
            return;
        }

        // Clear sandbox_id so coord routing and a future restart's
        // `repopulate_routing` don't try to talk to the dead sandbox.
        if let Err(e) = state
            .services
            .meta
            .assign_session_sandbox(session_id, None)
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "reconcile: clearing sandbox_id failed; continuing with status flip"
            );
        }

        if let Err(e) = state
            .services
            .meta
            .set_session_status(session_id, new_status)
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                ?new_status,
                "reconcile: set_session_status failed; will retry next tick"
            );
            return;
        }

        let now = Utc::now();
        let _ = state
            .emit(
                session_id,
                SessionEvent::StatusChanged {
                    from: prev,
                    to: new_status,
                    at: now,
                },
            )
            .await;

        tracing::info!(
            session_id = %session_id,
            host_id = %host_id,
            from = ?prev,
            to = ?new_status,
            recoverable,
            "ADR 0009 reconcile: flipped session whose sandbox is missing from heartbeat"
        );
    }
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
