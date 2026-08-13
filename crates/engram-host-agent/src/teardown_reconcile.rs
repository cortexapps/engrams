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

/// ADR 0116 A5: how long a sandbox must have been visible to this
/// process before a coordinator-confirmed no-owner answer may destroy
/// it — the create→bind grace, re-keyed on AGE instead of the retired
/// consecutive-verdict strikes. Two reconcile intervals clears the
/// create→bind window (a fresh sandbox publishes its binding within a
/// tick). First-seen is in-memory: a host-agent restart re-arms the
/// grace for every survivor — a bounded, conservative delay (one
/// minute), never a mis-reap.
pub const ORPHAN_GRACE: Duration = Duration::from_secs(2 * RECONCILE_INTERVAL.as_secs());

/// The age gate: `true` iff `sandbox` has been continuously visible for
/// at least [`ORPHAN_GRACE`]. Stamps first sight; the caller prunes
/// entries for sandboxes that vanished.
pub fn past_orphan_grace(
    first_seen: &mut HashMap<SandboxId, Duration>,
    sandbox: SandboxId,
    now_mono: Duration,
) -> bool {
    let first = *first_seen.entry(sandbox).or_insert(now_mono);
    now_mono.saturating_sub(first) >= ORPHAN_GRACE
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
    /// The sandbox has a LOCAL session binding. ADR 0116 A5: the bound
    /// arm's `sandbox_ownership` PG poll is RETIRED — a locally bound
    /// sandbox is owned until the coordinator says otherwise through
    /// the tombstone push (the heartbeat's `tombstoned_sandboxes` arm
    /// destroys it explicitly). No coordinator call, no inference.
    LocallyBound,
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
        // Locally-bound arm (ADR 0116 A5): owned, period — revocation
        // arrives as a tombstone, never as a poll verdict.
        ReconcileInput::LocallyBound => ReapVerdict::Owned,
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
    /// Is a capture / eviction-finalize in flight for this sandbox
    /// (`PooledBackend::capture_in_flight` — the capture lock is held)?
    ///
    /// ADR 0098 R-CoSim / issue #570: the ADR 0045 D5 idle-eviction fast
    /// path marks the session `Idle` and clears `sessions.sandbox_id` the
    /// instant `snapshot_begin` returns, while the snapshot upload
    /// finalizes in a host-owned background job. During that window
    /// `sandbox_ownership` / `sandbox_owner` answer "no owner" for a
    /// sandbox whose capture is still running — so the orphan-strike arm
    /// would SIGKILL the VM mid-upload, cancelling the finalize and losing
    /// the eviction snapshot (resume then rewinds to a stale checkpoint).
    /// The signal already exists on the host — the periodic checkpointer
    /// consults exactly this method to skip a sandbox with a capture in
    /// flight — the reconcile ownership check just never did. An in-flight
    /// capture is therefore an exemption, like a migration role or a live
    /// base-capture VM: the coordinator's transient unbind is not orphan
    /// truth while the host is mid-capture.
    fn capture_in_flight(&self, id: SandboxId) -> bool;
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
    if backend.migration_role_present(sandbox_id)
        || backend.is_live_capture(sandbox_id)
        || backend.capture_in_flight(sandbox_id)
    {
        return ReconcileInput::Exempt;
    }
    match session {
        Some(_) => ReconcileInput::LocallyBound,
        None => ReconcileInput::Unbound(
            coord
                .sandbox_owner(host_id, sandbox_id)
                .await
                .map_err(|_| CoordUnreachable),
        ),
    }
}

/// One full teardown-reconcile tick: list → classify every sandbox (pure
/// core; ADR 0116 A5: only the UNBOUND arm makes a coordinator call) →
/// destroy age-cleared confirmed orphans / repair recovered bindings.
/// The `first_seen` ledger is a caller-owned parameter (the sim owns its
/// lifetime across ticks, exactly like the interval wrapper in
/// `lib.rs`); `now_mono` is the caller's injected monotonic mark.
///
/// Returns `Err` only if the initial `list()` fails — the caller logs it
/// and skips the tick (unchanged from the inline loop). A destroy
/// failure is logged and left to retry next tick.
pub async fn reconcile_once(
    backend: &dyn ReconcileBackend,
    coord: &dyn CoordControlPlane,
    host_id: HostId,
    first_seen: &mut HashMap<SandboxId, Duration>,
    now_mono: Duration,
) -> Result<(), SandboxError> {
    let sandboxes = backend.list().await?;
    let live: std::collections::HashSet<SandboxId> = sandboxes.iter().copied().collect();
    first_seen.retain(|id, _| live.contains(id));
    for sandbox_id in sandboxes {
        let session = backend.session_for_sandbox(sandbox_id);
        let verdict = classify(gather_input(backend, coord, host_id, sandbox_id, session).await);
        match &verdict {
            ReapVerdict::Exempt | ReapVerdict::Owned => {}
            ReapVerdict::RepairBinding(session) => {
                tracing::info!(%sandbox_id, session_id = %session,
                    "teardown reconcile: coordinator owns this sandbox; \
                     repopulating the local binding");
                backend.record_session_binding(sandbox_id, *session);
            }
            ReapVerdict::Orphan => {
                // ADR 0116 A5: a coordinator-CONFIRMED no-owner answer
                // destroys as soon as the create→bind age grace clears —
                // no verdict-counting.
                if !past_orphan_grace(first_seen, sandbox_id, now_mono) {
                    continue;
                }
                tracing::warn!(%sandbox_id,
                    "teardown reconcile: coordinator confirms no owner; \
                     destroying locally");
                if let Err(e) = backend.destroy(sandbox_id).await {
                    tracing::warn!(%sandbox_id, error = %e,
                        "teardown reconcile: local destroy failed; retrying next tick");
                }
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
    fn capture_in_flight(&self, id: SandboxId) -> bool {
        // The exact signal the periodic checkpointer consults to skip a
        // sandbox mid-capture (issue #570): the capture lock is held for
        // the whole snapshot_begin → background-finalize window.
        self.pooled.capture_in_flight(id)
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
    fn classify_locally_bound_is_owned_without_a_coord_call() {
        // ADR 0116 A5: a local binding IS ownership until a tombstone
        // says otherwise — the polling bound arm is retired.
        assert_eq!(classify(ReconcileInput::LocallyBound), ReapVerdict::Owned);
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
    fn classify_orphan_only_via_confirmed_absence() {
        // Exactly ONE input maps to Orphan: the coordinator-confirmed
        // no-owner answer on an unbound sandbox.
        assert_eq!(
            classify(ReconcileInput::Unbound(Ok(None))),
            ReapVerdict::Orphan
        );
        let never_orphan = [
            ReconcileInput::Exempt,
            ReconcileInput::LocallyBound,
            ReconcileInput::Unbound(Ok(Some(SessionId::new()))),
            ReconcileInput::Unbound(Err(CoordUnreachable)),
        ];
        for input in never_orphan {
            assert_ne!(classify(input), ReapVerdict::Orphan);
        }
    }

    #[test]
    fn orphan_grace_gates_young_sandboxes_then_clears() {
        // ADR 0116 A5: the create→bind window is an AGE grace, not a
        // verdict count. A confirmed orphan younger than ORPHAN_GRACE
        // is spared; the same sandbox past the grace is destroyed.
        let mut seen = HashMap::new();
        let id = SandboxId::new();
        let t0 = Duration::from_secs(1_000);
        assert!(!past_orphan_grace(&mut seen, id, t0));
        assert!(!past_orphan_grace(&mut seen, id, t0 + ORPHAN_GRACE / 2));
        assert!(past_orphan_grace(&mut seen, id, t0 + ORPHAN_GRACE));
    }

    // ---- ADR 0098 R-CoSim / issue #570: an in-flight capture is exempt ----
    //
    // The D5 idle-eviction window: the coordinator has cleared
    // `sessions.sandbox_id` (so both ownership calls answer "no owner"),
    // but the host is still finalizing the snapshot upload (capture lock
    // held). The reconcile tick must NOT reap the sandbox — doing so
    // cancels the upload and loses the eviction snapshot. The full
    // co-simulated reproduction (real coordinator + real host) lives in
    // `engram-dst-cosim`; this pins the exemption at the unit boundary.

    use engram_host_core::{CoordError, LiveManifestPublishRequest, LiveManifestPublishResponse};

    struct MidCaptureBackend {
        sandbox: SandboxId,
        session: SessionId,
        capture_in_flight: bool,
        bound: bool,
        destroyed: parking_lot::Mutex<Vec<SandboxId>>,
    }

    #[async_trait]
    impl ReconcileBackend for MidCaptureBackend {
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(vec![self.sandbox])
        }
        fn migration_role_present(&self, _: SandboxId) -> bool {
            false
        }
        fn is_live_capture(&self, _: SandboxId) -> bool {
            false
        }
        fn capture_in_flight(&self, _: SandboxId) -> bool {
            self.capture_in_flight
        }
        fn session_for_sandbox(&self, _: SandboxId) -> Option<SessionId> {
            // ADR 0116 A5: the reap-eligible shape is UNBOUND (a bound
            // sandbox is owned until its tombstone arrives; `bound`
            // models the mid-capture local binding).
            if self.bound {
                Some(self.session)
            } else {
                None
            }
        }
        fn record_session_binding(&self, _: SandboxId, _: SessionId) {}
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.destroyed.lock().push(id);
            Ok(())
        }
    }

    /// The coordinator during the D5 window: `sessions.sandbox_id` cleared,
    /// so ownership is `false` and the owner lookup is `None`.
    struct UnownedCoord;

    #[async_trait]
    impl CoordControlPlane for UnownedCoord {
        async fn publish_live_manifest(
            &self,
            _: HostId,
            _: &LiveManifestPublishRequest,
        ) -> Result<LiveManifestPublishResponse, CoordError> {
            unreachable!("reconcile never publishes")
        }
        async fn sandbox_ownership(
            &self,
            _: HostId,
            _: SessionId,
            _: SandboxId,
        ) -> Result<bool, CoordError> {
            Ok(false)
        }
        async fn sandbox_owner(
            &self,
            _: HostId,
            _: SandboxId,
        ) -> Result<Option<SessionId>, CoordError> {
            Ok(None)
        }
    }

    async fn ticks_to_destroy(capture_in_flight: bool, bound: bool) -> usize {
        let backend = MidCaptureBackend {
            sandbox: SandboxId::new(),
            session: SessionId::new(),
            capture_in_flight,
            bound,
            destroyed: parking_lot::Mutex::new(Vec::new()),
        };
        let coord = UnownedCoord;
        let host = HostId::new();
        let mut first_seen = HashMap::new();
        // Ticks at the real cadence: age at tick k is (k-1)*interval, so
        // the ORPHAN_GRACE (2*interval) clears at tick 3.
        for tick in 1..=6usize {
            let now = Duration::from_secs(1_000) + RECONCILE_INTERVAL * (tick as u32 - 1);
            reconcile_once(&backend, &coord, host, &mut first_seen, now)
                .await
                .expect("tick");
            if !backend.destroyed.lock().is_empty() {
                return tick;
            }
        }
        usize::MAX
    }

    #[tokio::test]
    async fn capture_in_flight_sandbox_is_never_reaped() {
        // The fix: mid-capture, the unowned sandbox is exempt — no destroy
        // however long the D5 window lasts.
        assert_eq!(
            ticks_to_destroy(true, false).await,
            usize::MAX,
            "a sandbox with a capture in flight must never be reaped (issue #570)",
        );
    }

    #[tokio::test]
    async fn locally_bound_sandbox_is_never_reaped_by_polling() {
        // ADR 0116 A5: a bound sandbox is owned until its tombstone
        // arrives — the coordinator's "no owner" poll answer no longer
        // reaps it (the retired bound arm did).
        assert_eq!(
            ticks_to_destroy(false, true).await,
            usize::MAX,
            "a locally bound sandbox must never be reaped by the poll",
        );
    }

    #[tokio::test]
    async fn unbound_confirmed_orphan_reaps_after_the_age_grace() {
        // The reconciler's real job survives: an unbound sandbox the
        // coordinator confirms nobody owns is reaped once the
        // create->bind age grace clears (tick 3 at the real cadence).
        assert_eq!(
            ticks_to_destroy(false, false).await,
            3,
            "an unbound confirmed orphan reaps at the first post-grace tick",
        );
    }
}
