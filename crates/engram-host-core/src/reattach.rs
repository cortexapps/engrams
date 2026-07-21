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
/// 2026-07-16 RCA made load-bearing). The verify probe sits strictly between
/// the two for the same reason, mirrored: once the RECONFIGURE releases the
/// guest, the guest's own writes race any content check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReattachStep {
    /// Adopt the predecessor's acked-but-un-uploaded chunks into the fresh
    /// backend's dirty tier. Present only when a spool was carried over.
    SeedDirtyTier,
    /// Verify-on-read (ADR 0098 P7 rider): probe the first seeded chunk
    /// through the backend and require the adopted acked bytes back. Present
    /// only when a spool was carried over, and strictly BEFORE `Reconfigure`:
    /// the kernel releases the guest's parked I/O the moment it adopts our
    /// socket, so a post-RECONFIGURE probe races the live guest's own writes
    /// to the probed chunk — and the probed (first-spooled) chunk is exactly
    /// the guest's hottest. 2026-07-21 incident: on four consecutive host
    /// rolls, every survivor with a non-empty spool had its probe read back
    /// the guest's fresh post-resume write, mis-read it as a rolled-back
    /// base, and a healthy VM was parked, destroyed, and rewound — the
    /// acked-write loss was manufactured by the guard itself. Pre-RECONFIGURE
    /// the probe is race-free by construction, and a failure parks the
    /// survivor with the kernel config untouched (still dead-parked, so a
    /// later rehydrate attempt can still recover the device).
    VerifySeed,
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

    /// The 2026-07-21 companion invariant: the verify probe runs strictly
    /// BETWEEN the seed and the RECONFIGURE — after the seed so there is
    /// something to verify, before the RECONFIGURE so it can never race the
    /// guest I/O the RECONFIGURE releases. A seed without a probe is a
    /// coverage hole; a probe without a seed has nothing to check.
    pub fn verify_between_seed_and_reconfigure(&self) -> bool {
        let pos = |step: ReattachStep| self.steps.iter().position(|s| *s == step);
        match (
            pos(ReattachStep::SeedDirtyTier),
            pos(ReattachStep::VerifySeed),
            pos(ReattachStep::Reconfigure),
        ) {
            (Some(seed), Some(verify), Some(reconfigure)) => seed < verify && verify < reconfigure,
            // No seed → no probe (it is gated on adoption).
            (None, verify, _) => verify.is_none(),
            // A seed with a missing probe or a missing Reconfigure never
            // comes out of `plan_reattach`.
            (Some(_), None, _) | (Some(_), Some(_), None) => false,
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
    let mut steps = Vec::with_capacity(3);
    if has_seed {
        steps.push(ReattachStep::SeedDirtyTier);
        steps.push(ReattachStep::VerifySeed);
    }
    steps.push(ReattachStep::Reconfigure);
    ReattachPlan {
        backend_id,
        used_identifier_fallback,
        steps,
    }
}

/// The first seeded chunk to probe at the [`ReattachStep::VerifySeed`] step
/// (verify-on-read rider): `(chunk_index, expected_bytes)`. `None` when no
/// spool was adopted (or the spool carried zero chunks) — the probe is gated
/// on adoption so a clean (no-seed) rehydrate pays nothing. Picking the FIRST
/// seeded chunk (not a full-disk scan) keeps the probe a single in-RAM read:
/// latency is non-negotiable.
pub fn first_seeded_probe(seed_dirty: Option<&[(usize, Vec<u8>)]>) -> Option<(usize, &[u8])> {
    seed_dirty
        .and_then(|chunks| chunks.first())
        .map(|(idx, bytes)| (*idx, bytes.as_slice()))
}

/// The verify-on-read comparison: the bytes the backend read back for the
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

/// Whether a live process still holds the device node (`/dev/nbdN`) open,
/// independent of the netlink server pid. This is the **proof-of-death** input
/// the R6 layer-1 guard adds (ADR 0098 §Phase 3, #784 / #769 gap A): the
/// stale-binding sweep must never disconnect a device a surviving guest is
/// actively reading, even when the configuring server pid is dead. In prod
/// this is a `/proc/*/fd` readlink scan for the device node
/// (`device_has_live_holder`); in the sim it is the world's guest-liveness
/// model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceHolder {
    /// A live process holds an open fd on the device node — the surviving FC
    /// guest is still reading its rootfs. Disconnecting would EIO a live guest.
    LiveHolder,
    /// The scan completed and found no live holder — nothing is reading the
    /// device.
    NoHolder,
    /// The holder could not be determined (a scan error). Treated as
    /// fail-safe: absence of proof is not proof of death, so an Unknown holder
    /// blocks the disconnect exactly like a live one.
    Unknown,
}

/// The stale-binding sweep's action for one candidate device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepAction {
    /// Leave it alone — not a genuine stale binding (the owner is live/self,
    /// or there is no binding to sweep).
    NotStuck,
    /// The dead-owner binding still has a live (or unprovable) holder — leave
    /// the device RECONNECTABLE (do NOT disconnect) and surface it as a
    /// survivor candidate for a later re-serve pass. The R6 proof-of-death
    /// guard: `sweep-blocked-live-holder`.
    Park,
    /// Netlink `NBD_CMD_DISCONNECT` — a genuinely dead owner AND no live holder
    /// (proof of death). This is the only arm that tears the binding down.
    Disconnect,
}

/// The pure stale-binding verdict: **a dead recorded owner is disconnected
/// only with proof of death** — i.e. a completed holder scan that found NO
/// live process holding the device node open. A no-pid, self-pid, or live-pid
/// device is never stale (the 731df805 class); a dead-owner device whose node
/// is still held open by a surviving guest — or whose holder is unprovable —
/// is PARKED (left reconnectable), never severed (the #769 gap-A class).
///
/// The match is the full `(liveness × holder)` transition table, wildcard-free
/// so a new variant on EITHER enum is a compile error, not a silent gap.
pub fn sweep_verdict(pid_liveness: PidLiveness, holder: DeviceHolder) -> SweepAction {
    use DeviceHolder::{LiveHolder, NoHolder, Unknown};
    use PidLiveness::{Alive, Dead, NoPid, SelfPid};
    match (pid_liveness, holder) {
        // No binding / a live or self owner is never stale — the holder is
        // irrelevant, but every combination is enumerated so neither enum can
        // grow a silently-uncovered variant.
        (NoPid, NoHolder) | (NoPid, LiveHolder) | (NoPid, Unknown) => SweepAction::NotStuck,
        (SelfPid, NoHolder) | (SelfPid, LiveHolder) | (SelfPid, Unknown) => SweepAction::NotStuck,
        (Alive, NoHolder) | (Alive, LiveHolder) | (Alive, Unknown) => SweepAction::NotStuck,
        // Dead owner: the disconnect requires PROOF OF DEATH (a scan that found
        // no holder). A live holder — or an unprovable one — parks instead.
        (Dead, NoHolder) => SweepAction::Disconnect,
        (Dead, LiveHolder) => SweepAction::Park,
        (Dead, Unknown) => SweepAction::Park,
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
    fn plan_reattach_seeds_then_verifies_strictly_before_reconfigure() {
        let plan = plan_reattach(Uuid::from_u128(1), Some("id".to_string()), true);
        assert_eq!(
            plan.steps,
            vec![
                ReattachStep::SeedDirtyTier,
                ReattachStep::VerifySeed,
                ReattachStep::Reconfigure,
            ],
            "a seed is adopted, then verified, before the RECONFIGURE — the \
             probe must never run after the RECONFIGURE releases the guest's \
             parked I/O (2026-07-21: a post-RECONFIGURE probe raced the live \
             guest's writes and parked healthy survivors)",
        );
        assert!(plan.seed_precedes_reconfigure());
        assert!(plan.verify_between_seed_and_reconfigure());
        // Both properties hold for the no-seed plan too (vacuously).
        let no_seed = plan_reattach(Uuid::from_u128(1), Some("id".to_string()), false);
        assert!(no_seed.seed_precedes_reconfigure());
        assert!(no_seed.verify_between_seed_and_reconfigure());
    }

    #[test]
    fn verify_between_seed_and_reconfigure_rejects_hand_built_misorders() {
        // Not `plan_reattach` outputs — the invariant method itself must
        // reject a future edit that reorders or drops the probe.
        let base = plan_reattach(Uuid::from_u128(1), Some("id".to_string()), true);
        let mut probe_after_reconfigure = base.clone();
        probe_after_reconfigure.steps = vec![
            ReattachStep::SeedDirtyTier,
            ReattachStep::Reconfigure,
            ReattachStep::VerifySeed,
        ];
        assert!(!probe_after_reconfigure.verify_between_seed_and_reconfigure());
        let mut probe_dropped = base.clone();
        probe_dropped.steps = vec![ReattachStep::SeedDirtyTier, ReattachStep::Reconfigure];
        assert!(!probe_dropped.verify_between_seed_and_reconfigure());
        let mut probe_without_seed = base;
        probe_without_seed.steps = vec![ReattachStep::VerifySeed, ReattachStep::Reconfigure];
        assert!(!probe_without_seed.verify_between_seed_and_reconfigure());
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
    fn sweep_disconnects_only_dead_owners_with_proof_of_death() {
        // The 731df805 protections: a live/self/no-pid owner is never stale,
        // regardless of the holder scan.
        for holder in [
            DeviceHolder::NoHolder,
            DeviceHolder::LiveHolder,
            DeviceHolder::Unknown,
        ] {
            assert_eq!(
                sweep_verdict(PidLiveness::NoPid, holder),
                SweepAction::NotStuck
            );
            assert_eq!(
                sweep_verdict(PidLiveness::SelfPid, holder),
                SweepAction::NotStuck
            );
            assert_eq!(
                sweep_verdict(PidLiveness::Alive, holder),
                SweepAction::NotStuck
            );
        }
        // The R6 proof-of-death rule (#769 gap A): a dead owner disconnects
        // ONLY when the holder scan completed and found no live holder.
        assert_eq!(
            sweep_verdict(PidLiveness::Dead, DeviceHolder::NoHolder),
            SweepAction::Disconnect,
            "a dead owner with a completed no-holder scan is a genuine stale binding",
        );
        // A live holder — a surviving guest still reading its rootfs — parks
        // instead of severing (the #769 gap-A class).
        assert_eq!(
            sweep_verdict(PidLiveness::Dead, DeviceHolder::LiveHolder),
            SweepAction::Park,
            "a dead owner whose device a live guest still holds must NEVER disconnect",
        );
        // Fail-safe: an unprovable holder is not proof of death — park, never
        // sever.
        assert_eq!(
            sweep_verdict(PidLiveness::Dead, DeviceHolder::Unknown),
            SweepAction::Park,
            "absence of proof (a scan error) is not proof of death — park, never sever",
        );
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
