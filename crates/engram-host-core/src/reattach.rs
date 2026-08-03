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

/// One ordered step of the survivor-rehydrate sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReattachStep {
    /// `NBD_CMD_RECONFIGURE`: hand the kernel the fresh serve socket, which
    /// releases the parked guest I/O.
    Reconfigure,
}

/// The auditable plan `reattach_manifest` executes: the resolved kernel
/// backend identifier (with the fallback flagged for the warn) plus the
/// ordered steps. The survivor's device + geometry are NOT part of the plan
/// — they are the caller's already-claimed slot; the only decisions are the
/// identifier resolution and the reconfigure operation.
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

/// The pure reattach decision core. Resolves the backend identifier the way
/// `reattach_manifest` must (echo the kernel's own recorded value, else fall
/// back to the rehydrate ref's manifest id — the 2026-07-13 dfa0face fix:
/// re-deriving from the live ref EINVAL'd every forked-chain survivor) and
/// emits the reconfigure step.
pub fn plan_reattach(ref_manifest_id: Uuid, kernel_backend_id: Option<String>) -> ReattachPlan {
    let (backend_id, used_identifier_fallback) = match kernel_backend_id {
        Some(id) => (id, false),
        None => (ref_manifest_id.to_string(), true),
    };
    ReattachPlan {
        backend_id,
        used_identifier_fallback,
        steps: vec![ReattachStep::Reconfigure],
    }
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
        let plan = plan_reattach(ref_id, Some("kernel-recorded-id".to_string()));
        assert_eq!(plan.backend_id, "kernel-recorded-id");
        assert!(!plan.used_identifier_fallback);
        assert_eq!(plan.steps, vec![ReattachStep::Reconfigure]);
    }

    #[test]
    fn plan_reattach_falls_back_to_ref_manifest_id() {
        let ref_id = Uuid::from_u128(0x1234);
        let plan = plan_reattach(ref_id, None);
        // The dfa0face fix: fall back to the ref's manifest id, flagged for
        // the warn.
        assert_eq!(plan.backend_id, ref_id.to_string());
        assert!(plan.used_identifier_fallback);
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
