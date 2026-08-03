//! The SIGTERM shutdown-ladder DECISIONS (ADR 0098 Phase 2, Flow A).
//!
//! When a host-agent pod receives SIGTERM it walks a **linear ladder**:
//! abort the background tasks, quiesce captures and drain (bounded) any
//! capture still in flight, run a bounded final disk-flush over the
//! surviving NBD data planes, abandon those planes (leaving the kernel-side
//! devices alive for the successor), export any still-un-uploaded dirty tier
//! to the node-local shutdown spool, and detach the microVMs. The 2026-07-16
//! session-85e0298a acked-write-loss corruption lived in exactly this path.
//!
//! This module holds the **pure decisions** the ladder makes, extracted out
//! of the driver so the host-internal simulator (`engram-dst-host`) drives
//! the real ordering/classification on macOS and a seeded crash injector can
//! reason about them. It lives in `engram-host-core` — not the host-agent —
//! precisely because these functions touch only `std` types (`Duration`,
//! `usize`, `u64`, `bool`): they are cleanly portable, so the honest home is
//! the portable crate (contrast Flow C's `classify`, which is coupled to the
//! host-agent's `ReconcileBackend` and stays host-agent-local).
//!
//! **What stays in the driver (concurrency gates, NOT decisions):** the
//! `abandoning` SeqCst flag raise-first ordering, the drain-twice sweep over
//! `nbd_sandboxes`, the per-survivor `tokio::spawn` fan-out + the
//! `tokio::time::timeout` deadline, the `spawn_blocking` device sync (now via
//! [`DeviceSync::sync_device`](crate::DeviceSync)), and
//! `NbdSandboxState::abandon_for_shutdown`'s ownership-consuming semantics.
//! Those are #224's correctness gates; this module owns only what to DECIDE,
//! never how to sequence the effects.

use std::time::Duration;

/// Default final-flush budget (seconds) when `ENGRAM_SHUTDOWN_FLUSH_BUDGET_SECS`
/// is unset, unparseable, or non-positive. Budgeted against the pod's
/// `terminationGracePeriodSeconds` minus headroom for the abandon sweep +
/// detach.
pub const DEFAULT_FLUSH_BUDGET_SECS: f64 = 20.0;

/// Default capture-drain budget (seconds) when
/// `ENGRAM_SHUTDOWN_CAPTURE_DRAIN_BUDGET_SECS` is unset, unparseable, or
/// non-positive. A diff capture in flight at SIGTERM has already consumed
/// the KVM dirty bitmap; if the process exits under it, the runtime
/// teardown cancels its post-processing tasks and the checkpoint chain is
/// poisoned (2026-08-03 alert: `chain_poisoned` fired on every rollout
/// wave that caught a capture mid-finalize). The drain waits for in-flight
/// captures to complete so the chain-head record lands and the successor
/// rehydrates a Diff-capable chain. 30 s covers the normal diff finalize
/// (~1–6 s post-ADR-0101-Phase-A) with room for a slow upload; together
/// with the 20 s flush budget it fits the DaemonSet's 120 s
/// `terminationGracePeriodSeconds` with headroom. A Full re-chunk that
/// outlives the budget is logged as a straggler and poisons at exit —
/// same as today, now the rare case.
pub const DEFAULT_CAPTURE_DRAIN_BUDGET_SECS: f64 = 30.0;

/// The SIGTERM ladder's stages, in ladder order. Declaration order **is** the
/// progression order (the derived [`Ord`] compares by it), so the ordering is
/// auditable: `Signaled < TasksAborted < CaptureDrain < FinalFlush < Abandon <
/// SpoolExport < Detached`. The ladder is strictly linear — there are no
/// branches.
///
/// The enum is wildcard-free at its use sites and has an explicit [`LADDER`]
/// array, so a new stage is a compile error rather than a silent gap.
///
/// [`LADDER`]: ShutdownStage::LADDER
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ShutdownStage {
    /// SIGTERM received; the shutdown handler has begun.
    Signaled,
    /// The background tasks (heartbeat, gRPC, registration/rehydrate) are
    /// aborted — the largest insert-after-sweep source is removed (#224).
    TasksAborted,
    /// The bounded drain of in-flight captures. New captures were refused
    /// from `Signaled` on (the quiesce flag); a capture already past FC's
    /// `PUT /snapshot/create` has consumed the KVM dirty bitmap, so the
    /// ladder waits for it here — killing it at process exit would poison
    /// its checkpoint chain (2026-08-03 `chain_poisoned` alert).
    CaptureDrain,
    /// The bounded final disk-flush pass over the surviving NBD data planes.
    FinalFlush,
    /// The terminal abandon sweep: raise `abandoning`, drain-twice, leave the
    /// kernel devices alive for the successor.
    Abandon,
    /// Export every still-un-uploaded dirty tier to the node-local shutdown
    /// spool (the 85e0298a store-ahead leg).
    SpoolExport,
    /// Detach the microVMs (left alive for the successor to pidfd-reattach);
    /// the process exits.
    Detached,
}

impl ShutdownStage {
    /// Every stage, in ladder order. A new [`ShutdownStage`] variant that is
    /// not added here is a compile error at the array literal.
    pub const LADDER: [ShutdownStage; 7] = [
        ShutdownStage::Signaled,
        ShutdownStage::TasksAborted,
        ShutdownStage::CaptureDrain,
        ShutdownStage::FinalFlush,
        ShutdownStage::Abandon,
        ShutdownStage::SpoolExport,
        ShutdownStage::Detached,
    ];

    /// The next stage in the linear ladder, or `None` at the terminal
    /// [`Detached`](ShutdownStage::Detached).
    pub fn next(self) -> Option<ShutdownStage> {
        let i = Self::LADDER.iter().position(|s| *s == self)?;
        Self::LADDER.get(i + 1).copied()
    }

    /// Legality of a stage transition: the ladder only ever advances by one
    /// step. `from.can_advance_to(to)` iff `to` is `from`'s immediate
    /// successor — the driver walks the ladder in exactly this order, so a
    /// skip or a repeat is a bug.
    pub fn can_advance_to(self, to: ShutdownStage) -> bool {
        self.next() == Some(to)
    }
}

/// Issue #224: once the ladder reaches [`Abandon`](ShutdownStage::Abandon) the
/// terminal `abandoning` flag is raised, and NO new live NBD data plane may be
/// inserted into `nbd_sandboxes` — a late insert (a `create`/`rehydrate`
/// completing in its multi-second await window) must
/// `abandon_for_shutdown()` in-place instead, so process exit's
/// `NbdHandle::Drop` never netlink-disconnects a survivor's device the
/// successor is about to RECONFIGURE. This predicate is the pure ordering
/// gate; the concrete SeqCst flag + drain-twice sweep are the Linux-bound
/// concurrency implementation in the driver.
pub fn admits_new_plane(stage: ShutdownStage) -> bool {
    stage < ShutdownStage::Abandon
}

/// The final-flush deadline the driver budgets its per-survivor fan-out
/// against, from a pre-parsed env value. Non-positive / absent ⇒ the default.
/// Pure so the parse-and-default is unit-tested away from the driver's
/// `std::env::var` read.
pub fn flush_budget(env_value: Option<f64>) -> Duration {
    let secs = env_value
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_FLUSH_BUDGET_SECS);
    Duration::from_secs_f64(secs)
}

/// The capture-drain deadline for the [`CaptureDrain`](ShutdownStage::CaptureDrain)
/// stage, from a pre-parsed `ENGRAM_SHUTDOWN_CAPTURE_DRAIN_BUDGET_SECS` value.
/// Non-positive / absent ⇒ the default. Same pure parse-and-default shape as
/// [`flush_budget`].
pub fn capture_drain_budget(env_value: Option<f64>) -> Duration {
    let secs = env_value
        .filter(|v| *v > 0.0)
        .unwrap_or(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS);
    Duration::from_secs_f64(secs)
}

/// What the final-flush pass observed for one survivor, BEFORE the coord
/// publish executes — the input to [`classify_survivor`]. The `bound` flag is
/// "a coord publisher is wired AND the sandbox is bound to a session" (an
/// unbound / no-coord survivor's chunks are durable in the store but cannot be
/// published from here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushProbe {
    /// `backend.flush()` errored — nothing uploaded this pass.
    FlushError,
    /// `backend.flush()` returned `Ok`: `chunks_flushed` chunks uploaded.
    Flushed { chunks_flushed: usize, bound: bool },
}

/// The per-survivor action the final-flush fan-out takes. This is the decision
/// content that used to be interleaved inline in
/// `flush_nbd_data_planes_for_shutdown`; the driver now sequences the effects
/// off this verdict (exactly like Flow C's `reconcile_once` acts off
/// `classify`'s `ReapVerdict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurvivorAction {
    /// `chunks_flushed == 0` — the survivor was already clean; skip the
    /// publish (there is nothing new to point coord at).
    SkipClean,
    /// The flush errored — the survivor's un-uploaded writes ride the shutdown
    /// spool to the successor (the abandon-sweep spool export is the
    /// unconditional backstop). The same posture covers a *publish* that fails
    /// after a successful upload: the spool's store-ahead ref still points the
    /// successor at the just-uploaded chunks.
    RelyOnSpool,
    /// Chunks were uploaded and are durable in the store, but the survivor is
    /// unbound / no coord publisher is wired — durable in GCS, no publish.
    DurableNoPublish,
    /// Chunks were uploaded and the survivor is bound — publish the new
    /// `live_disk_manifest` so the successor rehydrates from the current ref.
    Publish,
}

/// The pure final-flush decision core: map one survivor's flush probe to the
/// action. Every arm preserves the pre-extraction inline semantics.
pub fn classify_survivor(probe: FlushProbe) -> SurvivorAction {
    match probe {
        FlushProbe::FlushError => SurvivorAction::RelyOnSpool,
        FlushProbe::Flushed {
            chunks_flushed: 0, ..
        } => SurvivorAction::SkipClean,
        FlushProbe::Flushed { bound: false, .. } => SurvivorAction::DurableNoPublish,
        FlushProbe::Flushed { .. } => SurvivorAction::Publish,
    }
}

/// The deadline-overrun straggler check: a survivor still holding
/// un-uploaded dirty bytes when the flush deadline fires is a loud straggler
/// — no longer a data-loss event (the abandon-sweep spool export catches it),
/// but the GCS-side durability gap on this node stays visible. `true` ⇒ log
/// loudly.
pub fn is_straggler(dirty_bytes: u64) -> bool {
    dirty_bytes > 0
}

/// What an `NbdHandle` drop does with the KERNEL side of its device
/// (2026-08-02 durability-rollback RCA).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NbdDropAction {
    /// Normal operation: a dropped handle netlink-disconnects its device.
    /// Correct for deliberate teardown (destroy), where the guest is gone
    /// and the device must return to the pool.
    Disconnect,
    /// Shutdown is underway: leave the kernel config alive. Between SIGTERM
    /// and process exit, a drop is never a deliberate survivor teardown —
    /// the abandon sweep uses `abandon()` (which skips `Drop`) — so any
    /// handle that reaches `Drop` in that window is unwind or teardown
    /// collateral. A disconnect from that path de-configures a surviving
    /// guest's live device: the successor's RECONFIGURE then meets "not
    /// configured" and the survivor becomes an uncapturable quarantine
    /// (2026-08-02: a panic between the flush pass and the abandon sweep
    /// disconnected four survivors this way; the coordinator then
    /// destroyed them past their acked writes).
    LeaveKernelConfigured,
}

/// The pure drop decision: `shutdown_underway` is the terminal
/// shutdown-abandon flag the driver raises at SIGTERM (before the
/// background-task aborts — an aborted task's dropped locals can hold a
/// live handle) and never lowers. A deliberate destroy that races the
/// shutdown window leaves its device configured-but-unowned; the
/// successor's startup stale-binding sweep reclaims exactly that state,
/// so the conservative arm never leaks a device past one generation.
pub fn nbd_drop_action(shutdown_underway: bool) -> NbdDropAction {
    if shutdown_underway {
        NbdDropAction::LeaveKernelConfigured
    } else {
        NbdDropAction::Disconnect
    }
}

/// The auditable shutdown plan the driver executes: the deadline the
/// final-flush fan-out is budgeted against, plus the (constant, linear)
/// stage ladder. The survivor set is *not* part of the plan — the fan-out
/// runs over every entry in `nbd_sandboxes` with no filtering, so the only
/// arithmetic here is the deadline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownPlan {
    /// The capture-drain deadline (from the capture-drain budget).
    pub capture_drain_deadline: Duration,
    /// The final-flush deadline (from the flush budget).
    pub flush_deadline: Duration,
}

impl ShutdownPlan {
    /// The ladder this plan is walked through, in order.
    pub const LADDER: [ShutdownStage; 7] = ShutdownStage::LADDER;
}

/// Build the shutdown plan from the pre-parsed budget env values.
pub fn plan_shutdown(flush_env: Option<f64>, capture_drain_env: Option<f64>) -> ShutdownPlan {
    ShutdownPlan {
        capture_drain_deadline: capture_drain_budget(capture_drain_env),
        flush_deadline: flush_budget(flush_env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_budget_defaults_on_absent_or_non_positive() {
        // Absent → default.
        assert_eq!(
            flush_budget(None),
            Duration::from_secs_f64(DEFAULT_FLUSH_BUDGET_SECS)
        );
        // Zero and negative are non-positive → default (mirrors the driver's
        // `filter(|v| *v > 0.0)`).
        assert_eq!(
            flush_budget(Some(0.0)),
            Duration::from_secs_f64(DEFAULT_FLUSH_BUDGET_SECS)
        );
        assert_eq!(
            flush_budget(Some(-5.0)),
            Duration::from_secs_f64(DEFAULT_FLUSH_BUDGET_SECS)
        );
        // A positive value is honored verbatim (incl. sub-second, the #225
        // overrun case).
        assert_eq!(flush_budget(Some(7.5)), Duration::from_secs_f64(7.5));
        assert_eq!(flush_budget(Some(0.001)), Duration::from_secs_f64(0.001));
    }

    #[test]
    fn capture_drain_budget_defaults_on_absent_or_non_positive() {
        assert_eq!(
            capture_drain_budget(None),
            Duration::from_secs_f64(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS)
        );
        assert_eq!(
            capture_drain_budget(Some(0.0)),
            Duration::from_secs_f64(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS)
        );
        assert_eq!(
            capture_drain_budget(Some(-1.0)),
            Duration::from_secs_f64(DEFAULT_CAPTURE_DRAIN_BUDGET_SECS)
        );
        assert_eq!(
            capture_drain_budget(Some(12.5)),
            Duration::from_secs_f64(12.5)
        );
    }

    #[test]
    fn plan_shutdown_carries_the_budgets_and_the_linear_ladder() {
        let plan = plan_shutdown(Some(3.0), Some(9.0));
        assert_eq!(plan.flush_deadline, Duration::from_secs(3));
        assert_eq!(plan.capture_drain_deadline, Duration::from_secs(9));
        assert_eq!(ShutdownPlan::LADDER, ShutdownStage::LADDER);
    }

    #[test]
    fn nbd_drop_disconnects_only_outside_shutdown() {
        // Normal operation: destroy teardown must disconnect.
        assert_eq!(nbd_drop_action(false), NbdDropAction::Disconnect);
        // Shutdown underway: every drop is unwind/teardown collateral — the
        // kernel config must survive for the successor's RECONFIGURE
        // (2026-08-02 durability-rollback RCA).
        assert_eq!(nbd_drop_action(true), NbdDropAction::LeaveKernelConfigured);
    }

    #[test]
    fn ladder_is_linear_and_ordered() {
        // The declared array is exactly the derived-Ord order.
        let mut sorted = ShutdownStage::LADDER;
        sorted.sort();
        assert_eq!(sorted, ShutdownStage::LADDER);
        // `next()` walks it, terminal at Detached.
        assert_eq!(
            ShutdownStage::Signaled.next(),
            Some(ShutdownStage::TasksAborted)
        );
        assert_eq!(
            ShutdownStage::TasksAborted.next(),
            Some(ShutdownStage::CaptureDrain)
        );
        assert_eq!(
            ShutdownStage::CaptureDrain.next(),
            Some(ShutdownStage::FinalFlush)
        );
        assert_eq!(
            ShutdownStage::SpoolExport.next(),
            Some(ShutdownStage::Detached)
        );
        assert_eq!(ShutdownStage::Detached.next(), None);
        // `can_advance_to` only permits the immediate successor.
        assert!(ShutdownStage::FinalFlush.can_advance_to(ShutdownStage::Abandon));
        assert!(!ShutdownStage::FinalFlush.can_advance_to(ShutdownStage::SpoolExport));
        assert!(!ShutdownStage::FinalFlush.can_advance_to(ShutdownStage::FinalFlush));
    }

    #[test]
    fn abandon_stage_closes_the_new_plane_gate() {
        // #224: before Abandon, inserts are admitted; from Abandon on, a late
        // insert must abandon-in-place.
        assert!(admits_new_plane(ShutdownStage::Signaled));
        assert!(admits_new_plane(ShutdownStage::TasksAborted));
        assert!(admits_new_plane(ShutdownStage::CaptureDrain));
        assert!(admits_new_plane(ShutdownStage::FinalFlush));
        assert!(!admits_new_plane(ShutdownStage::Abandon));
        assert!(!admits_new_plane(ShutdownStage::SpoolExport));
        assert!(!admits_new_plane(ShutdownStage::Detached));
    }

    #[test]
    fn classify_survivor_truth_table() {
        // flush errored → the spool store-ahead is the durability path.
        assert_eq!(
            classify_survivor(FlushProbe::FlushError),
            SurvivorAction::RelyOnSpool
        );
        // zero chunks → already clean, skip publish.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 0,
                bound: true,
            }),
            SurvivorAction::SkipClean
        );
        // zero chunks wins even when unbound.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 0,
                bound: false,
            }),
            SurvivorAction::SkipClean
        );
        // uploaded but unbound → durable in the store, no publish.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 4,
                bound: false,
            }),
            SurvivorAction::DurableNoPublish
        );
        // uploaded + bound → publish the new live manifest.
        assert_eq!(
            classify_survivor(FlushProbe::Flushed {
                chunks_flushed: 4,
                bound: true,
            }),
            SurvivorAction::Publish
        );
    }

    #[test]
    fn straggler_iff_dirty_bytes_remain() {
        assert!(!is_straggler(0));
        assert!(is_straggler(1));
        assert!(is_straggler(1 << 20));
    }
}
