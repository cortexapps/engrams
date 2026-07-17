//! The NBD slot/reattach DECISIONS (ADR 0098 Phase 2, Flow B).
//!
//! Flow B is the survivor-rehydrate lifecycle: after a host-agent pod roll
//! the successor generation re-serves the `/dev/nbdN` devices its
//! predecessor left configured (the kernel parks guest I/O under
//! `dead_conn_timeout` across the gap), and a stale-binding sweep
//! disconnects the devices whose configuring process is genuinely dead. The
//! 2026-07-17 session-731df805 corruption (PR #739) lived here: a coord-list
//! gap left a *parked* survivor's device unclaimed, the sweep disconnected
//! its live rootfs, and an un-pause landed on a dead data plane.
//!
//! This module holds the **pure decisions** the flow makes, extracted out of
//! the Linux driver (`disk_daemon::runtime` / `pooled_backend`) so the
//! host-internal simulator drives them on macOS and pins the #739 hazard.
//! Like Flow A's `shutdown` module, every function here touches only `std`
//! types (+ `uuid`), so the honest home is the portable crate. The kernel
//! effects themselves (CONNECT/RECONFIGURE/DISCONNECT, the sysfs
//! backend-identifier read) sit behind [`NbdKernel`](crate::NbdKernel); the
//! driver sequences those effects off the verdicts below.

use uuid::Uuid;

/// One ordered step of the survivor-rehydrate sequence. Declaration order is
/// the required execution order: the predecessor's shutdown-spool dirty tier
/// MUST be adopted BEFORE the RECONFIGURE, because the kernel releases the
/// guest's parked I/O the instant it adopts our serve socket — a read served
/// between RECONFIGURE and the seed would observe the rolled-back base
/// instead of the acked bytes (the seed-dirty-before-RECONFIGURE ordering the
/// 2026-07-16 RCA made load-bearing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReattachStep {
    /// Adopt the predecessor's acked-but-un-uploaded chunks into the fresh
    /// backend's dirty tier. Present only when a spool was carried over.
    SeedDirtyTier,
    /// `NBD_CMD_RECONFIGURE`: hand the kernel the fresh serve socket, which
    /// releases the parked guest I/O.
    Reconfigure,
}

/// The auditable plan `reattach_manifest` executes: the resolved kernel
/// backend identifier (with the fallback flagged for the warn) plus the
/// ordered steps. The survivor's device + geometry are NOT part of the plan
/// — they are the caller's already-claimed slot; the only decisions are the
/// identifier resolution and the seed-before-RECONFIGURE ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReattachPlan {
    /// The identifier handed to `NBD_CMD_RECONFIGURE` (the kernel
    /// strcmp-verifies it against the CONNECT-time value).
    pub backend_id: String,
    /// True when the kernel had no recorded `/sys/block/nbdN/backend` and we
    /// fell back to the rehydrate ref's manifest id — the driver logs a warn.
    pub used_identifier_fallback: bool,
    /// The steps, in execution order.
    pub steps: Vec<ReattachStep>,
}

impl ReattachPlan {
    /// The invariant the whole flow turns on: whenever a seed is present it is
    /// adopted strictly BEFORE the RECONFIGURE. Always true for a
    /// [`plan_reattach`] output — the assertion is the point (a future edit
    /// that reorders the steps trips this).
    pub fn seed_precedes_reconfigure(&self) -> bool {
        match (
            self.steps
                .iter()
                .position(|s| *s == ReattachStep::SeedDirtyTier),
            self.steps
                .iter()
                .position(|s| *s == ReattachStep::Reconfigure),
        ) {
            (Some(seed), Some(reconfigure)) => seed < reconfigure,
            // No seed → the ordering constraint is vacuous.
            (None, _) => true,
            // Reconfigure is always present; this arm is unreachable.
            (Some(_), None) => false,
        }
    }
}

/// The pure reattach decision core. Resolves the backend identifier the way
/// `reattach_manifest` must (echo the kernel's own recorded value, else fall
/// back to the rehydrate ref's manifest id — the 2026-07-13 dfa0face fix:
/// re-deriving from the live ref EINVAL'd every forked-chain survivor) and
/// lays out the seed-then-RECONFIGURE ordering as explicit steps.
pub fn plan_reattach(
    ref_manifest_id: Uuid,
    kernel_backend_id: Option<String>,
    has_seed: bool,
) -> ReattachPlan {
    let (backend_id, used_identifier_fallback) = match kernel_backend_id {
        Some(id) => (id, false),
        None => (ref_manifest_id.to_string(), true),
    };
    let mut steps = Vec::with_capacity(2);
    if has_seed {
        steps.push(ReattachStep::SeedDirtyTier);
    }
    steps.push(ReattachStep::Reconfigure);
    ReattachPlan {
        backend_id,
        used_identifier_fallback,
        steps,
    }
}

/// The first seeded chunk to probe post-RECONFIGURE (verify-on-read rider):
/// `(chunk_index, expected_bytes)`. `None` when no spool was adopted — the
/// probe is gated on adoption so a clean (no-seed) rehydrate pays nothing.
/// Picking the FIRST seeded chunk (not a full-disk scan) keeps the probe a
/// single in-RAM read: latency is non-negotiable.
pub fn first_seeded_probe(seed_dirty: Option<&[(usize, Vec<u8>)]>) -> Option<(usize, &[u8])> {
    seed_dirty
        .and_then(|chunks| chunks.first())
        .map(|(idx, bytes)| (*idx, bytes.as_slice()))
}

/// The verify-on-read comparison: the bytes the device served back for the
/// probed chunk must equal the seeded acked bytes (not the rolled-back base).
/// A prefix compare (`read_back` may be padded to the block size) — the seed
/// content is the authority on length.
pub fn probe_matches(read_back: &[u8], expected: &[u8]) -> bool {
    read_back.len() >= expected.len() && &read_back[..expected.len()] == expected
}

/// The liveness of the process the kernel recorded as a device's owner
/// (`/sys/block/nbdN/pid`), normalized for the stale-binding sweep verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PidLiveness {
    /// No pid file / empty — the device is not netlink-configured.
    NoPid,
    /// The pid is THIS host-agent generation's own pid — a device we just
    /// CONNECTed after the free-pool snapshot (never stale).
    SelfPid,
    /// The recorded pid is a live process (`kill(pid, 0)` ok, or `EPERM`).
    Alive,
    /// The recorded pid is dead (`kill(pid, 0) == ESRCH`).
    Dead,
}

/// The stale-binding sweep's action for one candidate device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepAction {
    /// Leave it alone — not a genuine stale binding.
    NotStuck,
    /// Netlink `NBD_CMD_DISCONNECT` — a genuinely dead owner.
    Disconnect,
}

/// The pure stale-binding verdict: **only a dead recorded owner is stale.**
/// A no-pid, self-pid, or live-pid device is left serving — the 731df805
/// class was exactly a *live* (parked-survivor) device being swept, which the
/// `free-in-pool` gate (the caller's `try_claim`) and this verdict together
/// prevent: the sweep only reaches devices free in the pool AND dead here.
pub fn sweep_verdict(pid_liveness: PidLiveness) -> SweepAction {
    match pid_liveness {
        PidLiveness::NoPid | PidLiveness::SelfPid | PidLiveness::Alive => SweepAction::NotStuck,
        PidLiveness::Dead => SweepAction::Disconnect,
    }
}

/// The #739 local-survivor candidate predicate: a durable chain-head record is
/// a local-rehydrate candidate iff its sandbox is **live** (the reattach pass
/// found the FC config), is **not already served** (the coord-list pass got
/// there first), and the record **knows its bound session** (a session-less
/// record predates binding — nothing sound to rehydrate under; the coord list
/// stays its only path). This is the decision core of
/// `pooled_backend::local_survivor_candidates`; the driver applies it per
/// record and the simulator drives the #739 park→roll→register scenario over
/// it.
pub fn is_local_survivor_candidate(live: bool, served: bool, has_session: bool) -> bool {
    live && !served && has_session
}

/// The un-pause data-plane gate (#739 follow-up rider): a rung-cancel resume
/// may proceed only when the sandbox's rootfs is served by THIS host-agent
/// generation. `is_nbd_backed` is "the sandbox has an NBD `/dev/nbdN` rootfs"
/// (`rootfs_device().is_some()`); `served_by_this_generation` is "this process
/// holds the live serve plane" (`nbd_sandboxes` membership). A non-NBD
/// sandbox is always fine; an NBD one whose plane this generation does not
/// serve must fail fast into `evict_local → resume` rather than un-pause onto
/// a dead device (the 731df805 EIO-on-live-guest outcome).
pub fn resume_data_plane_served(is_nbd_backed: bool, served_by_this_generation: bool) -> bool {
    !is_nbd_backed || served_by_this_generation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_reattach_echoes_kernel_identifier_when_present() {
        let ref_id = Uuid::from_u128(0xABCD);
        let plan = plan_reattach(ref_id, Some("kernel-recorded-id".to_string()), false);
        assert_eq!(plan.backend_id, "kernel-recorded-id");
        assert!(!plan.used_identifier_fallback);
        // No seed → RECONFIGURE only.
        assert_eq!(plan.steps, vec![ReattachStep::Reconfigure]);
        assert!(plan.seed_precedes_reconfigure());
    }

    #[test]
    fn plan_reattach_falls_back_to_ref_manifest_id() {
        let ref_id = Uuid::from_u128(0x1234);
        let plan = plan_reattach(ref_id, None, true);
        // The dfa0face fix: fall back to the ref's manifest id, flagged for
        // the warn.
        assert_eq!(plan.backend_id, ref_id.to_string());
        assert!(plan.used_identifier_fallback);
    }

    #[test]
    fn plan_reattach_seeds_strictly_before_reconfigure() {
        let plan = plan_reattach(Uuid::from_u128(1), Some("id".to_string()), true);
        assert_eq!(
            plan.steps,
            vec![ReattachStep::SeedDirtyTier, ReattachStep::Reconfigure],
            "a seed is adopted before the RECONFIGURE",
        );
        assert!(plan.seed_precedes_reconfigure());
        // The property holds for the no-seed plan too (vacuously).
        let no_seed = plan_reattach(Uuid::from_u128(1), Some("id".to_string()), false);
        assert!(no_seed.seed_precedes_reconfigure());
    }

    #[test]
    fn first_seeded_probe_picks_the_first_chunk_or_none() {
        assert_eq!(first_seeded_probe(None), None);
        assert_eq!(first_seeded_probe(Some(&[])), None);
        let seed = vec![(3usize, vec![1u8, 2, 3]), (5, vec![9, 9])];
        assert_eq!(
            first_seeded_probe(Some(&seed)),
            Some((3, [1u8, 2, 3].as_slice()))
        );
    }

    #[test]
    fn probe_matches_is_a_prefix_compare() {
        // Exact.
        assert!(probe_matches(&[1, 2, 3], &[1, 2, 3]));
        // Padded read (block-sized) still matches the seeded prefix.
        assert!(probe_matches(&[1, 2, 3, 0, 0, 0], &[1, 2, 3]));
        // Mismatched content (rolled-back base) fails.
        assert!(!probe_matches(&[0, 0, 0], &[1, 2, 3]));
        // A short read can't satisfy the seeded length.
        assert!(!probe_matches(&[1, 2], &[1, 2, 3]));
    }

    #[test]
    fn sweep_disconnects_only_dead_owners() {
        assert_eq!(sweep_verdict(PidLiveness::Dead), SweepAction::Disconnect);
        // The 731df805 protections: none of these is swept.
        assert_eq!(sweep_verdict(PidLiveness::NoPid), SweepAction::NotStuck);
        assert_eq!(sweep_verdict(PidLiveness::SelfPid), SweepAction::NotStuck);
        assert_eq!(sweep_verdict(PidLiveness::Alive), SweepAction::NotStuck);
    }

    #[test]
    fn local_survivor_candidate_truth_table() {
        // The only candidate arm: live + unserved + session-bound.
        assert!(is_local_survivor_candidate(true, false, true));
        // Dead (record outlived the VM) → not a candidate.
        assert!(!is_local_survivor_candidate(false, false, true));
        // Already served (coord list got there first) → not a candidate.
        assert!(!is_local_survivor_candidate(true, true, true));
        // Session-less record → not sound to rehydrate under.
        assert!(!is_local_survivor_candidate(true, false, false));
    }

    #[test]
    fn resume_gate_blocks_unserved_nbd_planes_only() {
        // Non-NBD sandbox: always fine.
        assert!(resume_data_plane_served(false, false));
        assert!(resume_data_plane_served(false, true));
        // NBD sandbox served by this generation: fine.
        assert!(resume_data_plane_served(true, true));
        // NBD sandbox NOT served by this generation: the 731df805 gate fires.
        assert!(!resume_data_plane_served(true, false));
    }
}
