//! ADR 0050 E: host-local teardown reconcile — the pure debounce.
//!
//! The coordinator drives every sandbox `destroy` as a best-effort gRPC
//! call; when that call fails (transient gRPC to a loaded/fresh host) the
//! firecracker VM leaks, and nothing reaps it (ADR 0009's reconcile only
//! sweeps the *other* direction, session-alive → sandbox-missing). This
//! module's reconcile (driven in `lib.rs`, which owns the async coord +
//! backend handles) generalizes the migration source-ownership rule
//! (ADR 0045 C1) to all sandboxes: a sandbox whose session no longer owns
//! it (`sessions.sandbox_id` — PG, ADR 0047's sole authority — no longer
//! points at it) is destroyed **locally**, where the destroy can't be
//! defeated by the same gRPC flakiness that leaked it.
//!
//! The one subtlety is *when* to act, and that's the testable part kept
//! here: a sandbox is reaped only after `ORPHAN_STRIKES` CONSECUTIVE
//! orphan verdicts, so an in-flight `create` (whose session binding
//! hasn't been published yet) or a single-tick coordinator blip never
//! reaps a live VM.
//!
//! # Flow C extraction (ADR 0098 Phase 2, P3)
//!
//! The tick used to live inline in `lib.rs` as a ~90-line detached spawn.
//! P3 splits it into a **pure decision core** ([`classify`], mapping a
//! [`ReconcileInput`] to a [`ReapVerdict`]) and a **one-tick driver**
//! ([`reconcile_once`]) over a focused [`ReconcileBackend`] seam — the same
//! `run_once`/pure-core split every coordinator driver already has, so the
//! host-internal simulator (`engram-dst-host`) can drive the REAL tick
//! against an adversarial coordinator. `lib.rs` keeps only a thin interval
//! wrapper. The pure core is table-tested below; in particular the None-arm
//! rule — **only a coordinator-CONFIRMED absence is an orphan** (the
//! 2026-07-11 mis-reap fix) — is one auditable `match` arm.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::error::SandboxError;
use engram_core::{HostId, SandboxId, SessionId};
use engram_host_core::CoordControlPlane;

/// How often the reconcile sweeps. 30s matches the migration TTL sweep
/// cadence; well below the cost of a leaked multi-hundred-MiB VM, well
/// above any create→bind latency.
pub const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

/// Consecutive orphan verdicts required before a local destroy. Two ticks
/// (~60s) clears the create→bind window (a fresh sandbox publishes its
/// session binding within a tick) and rides out a one-tick coord outage.
pub const ORPHAN_STRIKES: u32 = 2;

/// Apply one reconcile tick's verdict for a single sandbox to the strike
/// ledger, returning `true` iff it should be destroyed NOW.
///
/// - `is_orphan == false` (still owned, or coordinator unreachable so we
///   conservatively assume owned) resets the count and returns `false`.
/// - `is_orphan == true` increments the count; once it reaches
///   `threshold`, returns `true` (destroy).
///
/// The caller removes the entry after a successful destroy (and prunes
/// entries for sandboxes that vanished); a destroy that fails leaves the
/// count at/over threshold so the next tick retries immediately.
pub fn orphan_strike(
    strikes: &mut HashMap<SandboxId, u32>,
    sandbox: SandboxId,
    is_orphan: bool,
    threshold: u32,
) -> bool {
    if !is_orphan {
        strikes.remove(&sandbox);
        return false;
    }
    let n = strikes.entry(sandbox).or_insert(0);
    *n += 1;
    *n >= threshold
}

/// Marker for "the coordinator was unreachable" — a transient control-plane
/// blip, distinct from an authoritative answer. Both arms treat it as
/// "assume still owned": we NEVER reap on the same coord→host flakiness that
/// could have leaked the sandbox in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoordUnreachable;

/// The coordinator's already-fetched answer for one sandbox, normalized
/// across the two lookup calls the two arms make. The driver builds this
/// (deciding exemption + which coord call); [`classify`] maps it to a
/// [`ReapVerdict`]. Keeping the whole decision pure means the 2026-07-11
/// mis-reap fix lives in one table-tested place instead of tangled into an
/// async spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileInput {
    /// A migration-role sandbox (ADR 0045 C1 runs its own ownership rules)
    /// or a live base-snapshot capture VM (ADR 0084 P1b — host-local +
    /// transient, never session-owned; a slow `[warm]` hook must not be
    /// reaped mid-capture). Exempt WITHOUT a coordinator call.
    Exempt,
    /// The sandbox had a LOCAL session binding, so the driver asked
    /// `sandbox_ownership`. `Ok(true)` = still owned, `Ok(false)` =
    /// ownership moved on, `Err` = coord unreachable.
    Bound(Result<bool, CoordUnreachable>),
    /// The sandbox had NO local binding, so the driver asked `sandbox_owner`
    /// ("does ANY session own this on me?"). `Ok(Some)` = coord owns it
    /// (repair the missing local binding), `Ok(None)` = coord-CONFIRMED no
    /// owner, `Err` = coord unreachable. ADR 0090: a missing local binding
    /// is NOT ownership truth — a fresh generation whose NBD rehydrate
    /// failed has no entry for a legitimately-owned, pidfd-reattached
    /// survivor, and this arm's old unconditional "orphan" SIGKILLed exactly
    /// such a VM mid-build (2026-07-11 campaign).
    Unbound(Result<Option<SessionId>, CoordUnreachable>),
}

/// The reconcile verdict for a single sandbox in one tick — the pure
/// classification of the coordinator's answer (plus exemptions) into an
/// action. `orphan_strike` is fed `matches!(verdict, Orphan)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapVerdict {
    /// Migration-role / live-capture sandbox — never reaped here; strike
    /// ledger cleared.
    Exempt,
    /// Coordinator-confirmed still owned, OR the coordinator was unreachable
    /// so we conservatively assume ownership. Strike ledger cleared.
    Owned,
    /// The coordinator owns this sandbox under `session`, but the local
    /// binding table had no entry — repopulate it (ADR 0090). Not an orphan;
    /// strike ledger cleared.
    RepairBinding(SessionId),
    /// Coordinator-CONFIRMED that no session owns this sandbox — an orphan.
    /// Strikes toward a local destroy. **The ONLY path to a destroy.**
    Orphan,
}

/// The pure decision core: map the coordinator's answer to a verdict. This
/// is the whole of Flow C's decision logic — every arm below preserves the
/// pre-extraction inline semantics byte-for-byte, and the two "coord
/// unreachable ⇒ Owned" arms plus the "only `Unbound(Ok(None))` is an
/// orphan" rule ARE the 2026-07-11 mis-reap fix.
pub fn classify(input: ReconcileInput) -> ReapVerdict {
    match input {
        ReconcileInput::Exempt => ReapVerdict::Exempt,
        // Locally-bound arm.
        ReconcileInput::Bound(Ok(true)) => ReapVerdict::Owned,
        ReconcileInput::Bound(Ok(false)) => ReapVerdict::Orphan,
        ReconcileInput::Bound(Err(CoordUnreachable)) => ReapVerdict::Owned,
        // Unbound arm (ADR 0090): a coord-owned answer repairs the binding;
        // ONLY a coordinator-confirmed absence is an orphan.
        ReconcileInput::Unbound(Ok(Some(session))) => ReapVerdict::RepairBinding(session),
        ReconcileInput::Unbound(Ok(None)) => ReapVerdict::Orphan,
        ReconcileInput::Unbound(Err(CoordUnreachable)) => ReapVerdict::Owned,
    }
}

/// The host-side surface [`reconcile_once`] drives — the minimal slice of
/// the pooled backend + capture-job registry the tick touches, behind a
/// trait so the simulator can supply its own world. Prod: the
/// [`PooledReconcileBackend`] adapter over `PooledBackend` +
/// `CaptureJobExecutor`. All methods mirror the exact inline calls the
/// pre-extraction loop made.
#[async_trait]
pub trait ReconcileBackend: Send + Sync {
    /// The live sandboxes on this host (`SandboxBackend::list`). An `Err`
    /// aborts the tick (the wrapper logs + skips) — never a partial sweep.
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;
    /// Does a migration role note exist for this sandbox? (`migration_role`
    /// `.is_some()`.)
    fn migration_role_present(&self, id: SandboxId) -> bool;
    /// Is this the live sandbox of a non-terminal capture job?
    /// (`CaptureJobExecutor::is_live_sandbox`.)
    fn is_live_capture(&self, id: SandboxId) -> bool;
    /// The local session binding, if any (`session_for_sandbox`).
    fn session_for_sandbox(&self, id: SandboxId) -> Option<SessionId>;
    /// Repopulate a local binding the table missed (`record_session_binding`).
    fn record_session_binding(&self, id: SandboxId, session: SessionId);
    /// Destroy a confirmed-orphaned sandbox locally (`SandboxBackend::destroy`).
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
}

/// Gather the coordinator's answer for one sandbox: exemption short-circuits
/// before any call; otherwise the local binding decides which of the two
/// lookup calls to make. This is the one impure step (it awaits the coord);
/// the verdict itself is [`classify`].
async fn gather_input(
    backend: &dyn ReconcileBackend,
    coord: &dyn CoordControlPlane,
    host_id: HostId,
    sandbox_id: SandboxId,
    session: Option<SessionId>,
) -> ReconcileInput {
    if backend.migration_role_present(sandbox_id) || backend.is_live_capture(sandbox_id) {
        return ReconcileInput::Exempt;
    }
    match session {
        Some(sid) => ReconcileInput::Bound(
            coord
                .sandbox_ownership(host_id, sid, sandbox_id)
                .await
                .map_err(|_| CoordUnreachable),
        ),
        None => ReconcileInput::Unbound(
            coord
                .sandbox_owner(host_id, sandbox_id)
                .await
                .map_err(|_| CoordUnreachable),
        ),
    }
}

/// One full teardown-reconcile tick: list → classify every sandbox (pure
/// core + one coordinator call each) → strike-debounce → destroy confirmed
/// orphans / repair recovered bindings. The `strikes` ledger is a
/// caller-owned parameter (the sim owns its lifetime across ticks, exactly
/// like the interval wrapper in `lib.rs`).
///
/// Returns `Err` only if the initial `list()` fails — the caller logs it and
/// skips the tick (unchanged from the inline loop). A destroy failure is
/// logged and left to retry next tick (the strike stays at/over threshold).
pub async fn reconcile_once(
    backend: &dyn ReconcileBackend,
    coord: &dyn CoordControlPlane,
    host_id: HostId,
    strikes: &mut HashMap<SandboxId, u32>,
) -> Result<(), SandboxError> {
    let sandboxes = backend.list().await?;
    let live: std::collections::HashSet<SandboxId> = sandboxes.iter().copied().collect();
    strikes.retain(|id, _| live.contains(id));
    for sandbox_id in sandboxes {
        let session = backend.session_for_sandbox(sandbox_id);
        let verdict = classify(gather_input(backend, coord, host_id, sandbox_id, session).await);
        let orphan = match &verdict {
            ReapVerdict::Exempt | ReapVerdict::Owned => false,
            ReapVerdict::RepairBinding(session) => {
                tracing::info!(%sandbox_id, session_id = %session,
                    "teardown reconcile: coordinator owns this sandbox; \
                     repopulating the local binding");
                backend.record_session_binding(sandbox_id, *session);
                false
            }
            ReapVerdict::Orphan => true,
        };
        if orphan_strike(strikes, sandbox_id, orphan, ORPHAN_STRIKES) {
            tracing::warn!(%sandbox_id, ?session,
                "teardown reconcile: sandbox no longer owned by its session; \
                 destroying locally");
            if let Err(e) = backend.destroy(sandbox_id).await {
                tracing::warn!(%sandbox_id, error = %e,
                    "teardown reconcile: local destroy failed; retrying next tick");
            } else {
                strikes.remove(&sandbox_id);
            }
        }
    }
    Ok(())
}

/// The production [`ReconcileBackend`]: a thin adapter over the shared
/// `PooledBackend` (the binding table + `SandboxBackend` list/destroy) and
/// the `CaptureJobExecutor` (the live-capture exemption). Holds `Arc` clones
/// so the reconcile spawn owns cheap handles.
pub struct PooledReconcileBackend {
    pooled: std::sync::Arc<crate::pooled_backend::PooledBackend>,
    capture_jobs: std::sync::Arc<crate::capture_job::CaptureJobExecutor>,
}

impl PooledReconcileBackend {
    pub fn new(
        pooled: std::sync::Arc<crate::pooled_backend::PooledBackend>,
        capture_jobs: std::sync::Arc<crate::capture_job::CaptureJobExecutor>,
    ) -> Self {
        Self {
            pooled,
            capture_jobs,
        }
    }
}

#[async_trait]
impl ReconcileBackend for PooledReconcileBackend {
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        use engram_core::traits::sandbox::SandboxBackend as _;
        self.pooled.list().await
    }
    fn migration_role_present(&self, id: SandboxId) -> bool {
        self.pooled.migration_role(id).is_some()
    }
    fn is_live_capture(&self, id: SandboxId) -> bool {
        self.capture_jobs.is_live_sandbox(id)
    }
    fn session_for_sandbox(&self, id: SandboxId) -> Option<SessionId> {
        self.pooled.session_for_sandbox(id)
    }
    fn record_session_binding(&self, id: SandboxId, session: SessionId) {
        self.pooled.record_session_binding(id, session);
    }
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        use engram_core::traits::sandbox::SandboxBackend as _;
        self.pooled.destroy(id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classify: the pure Flow C decision core (ADR 0098 P3) ----
    //
    // The full 7-arm truth table. These pin the exact pre-extraction inline
    // semantics; the two `CoordUnreachable ⇒ Owned` arms and the single
    // `Unbound(Ok(None)) ⇒ Orphan` arm ARE the 2026-07-11 mis-reap fix, so a
    // regression here is a real bug, not a test to update.

    #[test]
    fn classify_exempt_is_never_reaped() {
        assert_eq!(classify(ReconcileInput::Exempt), ReapVerdict::Exempt);
    }

    #[test]
    fn classify_bound_owned_is_owned() {
        assert_eq!(
            classify(ReconcileInput::Bound(Ok(true))),
            ReapVerdict::Owned
        );
    }

    #[test]
    fn classify_bound_not_owned_is_orphan() {
        assert_eq!(
            classify(ReconcileInput::Bound(Ok(false))),
            ReapVerdict::Orphan
        );
    }

    #[test]
    fn classify_bound_coord_unreachable_assumes_owned() {
        // A transient coord blip on a locally-bound sandbox never reaps.
        assert_eq!(
            classify(ReconcileInput::Bound(Err(CoordUnreachable))),
            ReapVerdict::Owned
        );
    }

    #[test]
    fn classify_unbound_coord_owns_repairs_binding() {
        // The ADR 0090 survivor: no local binding, coord owns it → repair,
        // NEVER reap. (The arm whose old unconditional `true` SIGKILLed a
        // live VM mid-build.)
        let sid = SessionId::new();
        assert_eq!(
            classify(ReconcileInput::Unbound(Ok(Some(sid)))),
            ReapVerdict::RepairBinding(sid)
        );
    }

    #[test]
    fn classify_unbound_coord_confirms_no_owner_is_orphan() {
        // The ONLY None-arm path to a destroy: a coordinator-CONFIRMED
        // absence.
        assert_eq!(
            classify(ReconcileInput::Unbound(Ok(None))),
            ReapVerdict::Orphan
        );
    }

    #[test]
    fn classify_unbound_coord_unreachable_assumes_owned() {
        // A transient coord blip on an unbound sandbox also never reaps —
        // same posture as the bound arm.
        assert_eq!(
            classify(ReconcileInput::Unbound(Err(CoordUnreachable))),
            ReapVerdict::Owned
        );
    }

    #[test]
    fn classify_orphan_is_the_only_verdict_that_strikes() {
        // Exactly one of the seven inputs maps to Orphan-via-Bound and one
        // via Unbound; every other input clears the strike ledger.
        let orphaning = [
            ReconcileInput::Bound(Ok(false)),
            ReconcileInput::Unbound(Ok(None)),
        ];
        for input in orphaning {
            assert_eq!(classify(input), ReapVerdict::Orphan);
        }
        let never_orphan = [
            ReconcileInput::Exempt,
            ReconcileInput::Bound(Ok(true)),
            ReconcileInput::Bound(Err(CoordUnreachable)),
            ReconcileInput::Unbound(Ok(Some(SessionId::new()))),
            ReconcileInput::Unbound(Err(CoordUnreachable)),
        ];
        for input in never_orphan {
            assert_ne!(classify(input), ReapVerdict::Orphan);
        }
    }

    #[test]
    fn owned_sandbox_never_strikes() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        for _ in 0..5 {
            assert!(!orphan_strike(&mut s, id, false, ORPHAN_STRIKES));
        }
        assert!(s.is_empty(), "an owned sandbox leaves no ledger entry");
    }

    #[test]
    fn orphan_destroyed_only_after_consecutive_strikes() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        // First orphan tick: one strike, below threshold — don't destroy
        // (protects an in-flight create whose binding hasn't landed).
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES));
        // Second consecutive orphan tick crosses the threshold.
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES));
    }

    #[test]
    fn a_single_non_orphan_tick_resets_the_count() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 1
        assert!(!orphan_strike(&mut s, id, false, ORPHAN_STRIKES)); // owned again → reset
                                                                    // The next orphan run must start the count over, not destroy on its
                                                                    // first strike.
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 1 again
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 2 → destroy
    }

    #[test]
    fn a_failed_destroy_retries_immediately_next_tick() {
        let mut s = HashMap::new();
        let id = SandboxId::new();
        assert!(!orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 1
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES)); // strike 2 → destroy attempt
                                                                  // The caller's destroy failed, so it did NOT remove the entry: the
                                                                  // count stays at/over threshold and re-fires on the next tick.
        assert!(orphan_strike(&mut s, id, true, ORPHAN_STRIKES));
    }
}
