//! The startup NBD-slot CLASSIFICATION BARRIER (ADR 0098 §Phase 3, Wave 7b,
//! #784 layers 2–3).
//!
//! Layer 1 (R6/#806) inverted the stale-binding sweep's kill test: a dead-owner
//! device is DISCONNECTed only with proof of death. Layers 2–3 close the gap
//! ABOVE that verdict — the gap where a survivor invisible to the tracked
//! records ("rehydrate skipped: survivor has no block-device rootfs to
//! re-serve", the #769 gap-A skip) never reached the sweep's protection at all,
//! because the sweep only ever probed the FREE slot pool derived from those same
//! records.
//!
//! **Layer 2 — kernel-derived inventory.** The work-list is no longer the
//! tracked records; it is KERNEL GROUND TRUTH — the `/dev/nbdN` devices the
//! kernel currently has CONNECTED ([`crate::NbdKernel::connected_devices`]),
//! crossed with the surviving guest processes (the holder scan). Tracked records
//! are RECONCILED AGAINST that inventory, never the reverse: a kernel-connected
//! device with no matching record becomes [`SlotClass::QuarantinedUnknown`] +
//! an alert, never a silent skip.
//!
//! **Layer 3 — the classification barrier.** Every kernel-connected slot is
//! classified into EXACTLY ONE [`SlotClass`] via a wildcard-free match over
//! `(liveness × holder × has_record)` — the exhaustive-classification
//! discipline: a new variant on [`PidLiveness`](crate::PidLiveness) or
//! [`DeviceHolder`](crate::DeviceHolder) is a compile error, not a silent gap.
//! The ORDERING CONTRACT is enforced STRUCTURALLY, not by comment: the
//! destructive stale-binding sweep accepts only a [`ReapList`], which is
//! constructible ONLY by [`classify_startup_slots`]. A device therefore reaches
//! a `NBD_CMD_DISCONNECT` iff classification put it in
//! [`SlotClass::TerminalSafeToReap`]; the sweep can touch no other class.

use crate::{DeviceHolder, PidLiveness};

/// The class assigned to one kernel-connected NBD slot at startup. Exactly one
/// per slot; the sweep may act on [`SlotClass::TerminalSafeToReap`] and nothing
/// else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotClass {
    /// A live owner already serves this device — this generation (a fresh
    /// create / a just-re-served survivor, kernel owner == self) or another live
    /// process. Leave it entirely alone.
    Serving,
    /// A dead-owner device that a tracked record maps to a re-servable resident
    /// survivor. The rehydrate passes (coord-list / #739 local ChainHeadRecord)
    /// own re-serving it; the sweep must NEVER reap it (a record exists — a
    /// failed serve is retryable, not stale). Left RECONNECTABLE.
    ReconnectMe,
    /// A dead owner with PROOF OF DEATH (a completed holder scan found no live
    /// process) AND no tracked record. The genuine stale binding — the ONLY
    /// class the destructive sweep may DISCONNECT.
    TerminalSafeToReap,
    /// A kernel-connected device the reconcile could NOT account for: a
    /// dead-owner device whose node a live (or unprovable) process still holds
    /// open, but which NO tracked record references — the #769 gap-A survivor,
    /// invisible to the records. Parked (left kernel-bound, RECONNECTABLE) and
    /// ALERTED (`rehydrate-unknown-device`); never severed, never silently
    /// skipped.
    QuarantinedUnknown,
}

/// One kernel-connected NBD slot, reconciled against the tracked records, ready
/// to classify. `device` is the caller's downstream handle — a `/dev/nbdN`
/// [`std::path::PathBuf`] in the prod driver, a `SandboxId` in the simulators —
/// carried through so [`classify_startup_slots`] can hand the reapable subset
/// straight to the sweep.
#[derive(Clone, Debug)]
pub struct StartupSlot<D> {
    /// The caller's device handle, threaded through classification unchanged.
    pub device: D,
    /// The recorded owner's liveness (`/sys/block/nbdN/pid` vs this generation's
    /// pid, in prod; `kernel_owner` vs `generation` in the sim). A connected
    /// device is never [`PidLiveness::NoPid`], but the arm is enumerated so the
    /// classifier stays total over the whole enum.
    pub liveness: PidLiveness,
    /// Whether a live process still holds the device node open (#806 proof of
    /// death). Only consulted for a dead owner.
    pub holder: DeviceHolder,
    /// Whether a tracked record (the coord-list rehydrate ref OR the #739 local
    /// ChainHeadRecord) references this device — i.e. we have a `(session,
    /// manifest)` to re-serve it under. THE LAYER-2 RECONCILE: the kernel
    /// inventory is the spine, records are matched AGAINST it. A connected device
    /// with `has_record == false` is a survivor invisible to the records.
    pub has_record: bool,
}

/// The reapable device set — the ONLY input a destructive stale-binding sweep
/// accepts. Its inner `Vec` is PRIVATE and it is constructible solely by
/// [`classify_startup_slots`], so the ordering contract ("no destructive pass
/// before classification; a destructive pass touches only
/// [`SlotClass::TerminalSafeToReap`]") is a TYPE constraint, not a comment: you
/// cannot obtain a `ReapList` without having classified.
#[derive(Debug)]
pub struct ReapList<D> {
    devices: Vec<D>,
}

impl<D> ReapList<D> {
    /// The devices classified [`SlotClass::TerminalSafeToReap`] — safe to
    /// DISCONNECT.
    pub fn devices(&self) -> &[D] {
        &self.devices
    }

    /// Consume the list, yielding the owned reapable devices.
    pub fn into_devices(self) -> Vec<D> {
        self.devices
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// The classification outcome, partitioned by [`SlotClass`]. `reap` is the
/// [`ReapList`] the destructive sweep consumes; `reconnect`/`quarantined` are
/// the caller's non-destructive follow-ups (serve the reconnectables, alert the
/// quarantined). `serving` is carried for completeness/observability.
#[derive(Debug)]
pub struct StartupClassification<D> {
    /// [`SlotClass::Serving`] devices — nothing to do.
    pub serving: Vec<D>,
    /// [`SlotClass::ReconnectMe`] devices — the rehydrate passes re-serve these;
    /// the sweep must not reap them.
    pub reconnect: Vec<D>,
    /// [`SlotClass::QuarantinedUnknown`] devices — park + alert
    /// (`rehydrate-unknown-device`).
    pub quarantined: Vec<D>,
    /// [`SlotClass::TerminalSafeToReap`] devices — the sweep's sole input.
    pub reap: ReapList<D>,
}

/// The pure per-slot classifier: assign one [`SlotClass`] from `(liveness ×
/// holder × has_record)`. Wildcard-free over both enums — a new
/// [`PidLiveness`](crate::PidLiveness) or [`DeviceHolder`](crate::DeviceHolder)
/// variant breaks this match at compile time.
///
/// A dead-owner device is only ever the destructive
/// [`SlotClass::TerminalSafeToReap`] when BOTH proof of death (a completed
/// no-holder scan) AND absence of a record hold; a record makes it
/// [`SlotClass::ReconnectMe`] (retryable, never reaped), and a live/unprovable
/// holder with no record makes it [`SlotClass::QuarantinedUnknown`] (the gap-A
/// survivor). This is a strict REFINEMENT of
/// [`sweep_verdict`](crate::sweep_verdict): every `TerminalSafeToReap` is a
/// `sweep_verdict` `Disconnect`; the barrier only ever narrows what may be
/// reaped, never widens it.
pub fn classify_startup_slot(
    liveness: PidLiveness,
    holder: DeviceHolder,
    has_record: bool,
) -> SlotClass {
    use DeviceHolder::{LiveHolder, NoHolder, Unknown};
    use PidLiveness::{Alive, Dead, NoPid, SelfPid};
    match (liveness, holder, has_record) {
        // A live / self / no-binding owner already serves the device (or there
        // is nothing bound to reap) — leave it. The holder and the record are
        // irrelevant, but every combination is enumerated so neither enum can
        // grow a silently-uncovered variant.
        (NoPid | SelfPid | Alive, LiveHolder | NoHolder | Unknown, true | false) => {
            SlotClass::Serving
        }
        // Dead owner WITH a tracked record: a known resident survivor. The
        // rehydrate passes re-serve it; the sweep never reaps a device we hold a
        // record for (a failed serve is retryable). Holds regardless of the
        // holder scan.
        (Dead, LiveHolder | NoHolder | Unknown, true) => SlotClass::ReconnectMe,
        // Dead owner, NO record, PROOF OF DEATH (a completed scan found no live
        // holder): the genuine stale binding — the sole reapable class.
        (Dead, NoHolder, false) => SlotClass::TerminalSafeToReap,
        // Dead owner, NO record, but a live (or unprovable) process still holds
        // the node open: the gap-A survivor invisible to the records. Quarantine
        // + alert; never sever.
        (Dead, LiveHolder, false) => SlotClass::QuarantinedUnknown,
        (Dead, Unknown, false) => SlotClass::QuarantinedUnknown,
    }
}

/// Classify a kernel-derived inventory of connected slots. Partitions the input
/// into [`StartupClassification`], producing the [`ReapList`] the destructive
/// sweep consumes. Order-preserving within each class (the caller feeds a
/// deterministically-ordered inventory — sysfs-sorted in prod, `BTreeMap` in the
/// sim — ADR 0098 D5).
pub fn classify_startup_slots<D>(slots: Vec<StartupSlot<D>>) -> StartupClassification<D> {
    let mut serving = Vec::new();
    let mut reconnect = Vec::new();
    let mut quarantined = Vec::new();
    let mut reap = Vec::new();
    for slot in slots {
        match classify_startup_slot(slot.liveness, slot.holder, slot.has_record) {
            SlotClass::Serving => serving.push(slot.device),
            SlotClass::ReconnectMe => reconnect.push(slot.device),
            SlotClass::QuarantinedUnknown => quarantined.push(slot.device),
            SlotClass::TerminalSafeToReap => reap.push(slot.device),
        }
    }
    StartupClassification {
        serving,
        reconnect,
        quarantined,
        reap: ReapList { devices: reap },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{sweep_verdict, SweepAction};

    /// The full `(liveness × holder × has_record)` truth table, and the
    /// REFINEMENT property: a slot is `TerminalSafeToReap` iff `sweep_verdict`
    /// would `Disconnect` AND no record exists — the barrier only ever narrows
    /// what may be reaped.
    #[test]
    fn classify_covers_the_whole_product_and_refines_sweep_verdict() {
        let livenesses = [
            PidLiveness::NoPid,
            PidLiveness::SelfPid,
            PidLiveness::Alive,
            PidLiveness::Dead,
        ];
        let holders = [
            DeviceHolder::LiveHolder,
            DeviceHolder::NoHolder,
            DeviceHolder::Unknown,
        ];
        for liveness in livenesses {
            for holder in holders {
                for has_record in [true, false] {
                    let class = classify_startup_slot(liveness, holder, has_record);
                    // A live/self/no owner is always Serving.
                    if !matches!(liveness, PidLiveness::Dead) {
                        assert_eq!(class, SlotClass::Serving, "{liveness:?}/{holder:?}");
                        continue;
                    }
                    // Dead owner: a record always wins (ReconnectMe), never reap.
                    if has_record {
                        assert_eq!(class, SlotClass::ReconnectMe, "{holder:?}");
                        continue;
                    }
                    // Dead + no record: the reap arm matches sweep_verdict's
                    // Disconnect exactly; a Park becomes QuarantinedUnknown.
                    match sweep_verdict(liveness, holder) {
                        SweepAction::Disconnect => {
                            assert_eq!(class, SlotClass::TerminalSafeToReap, "{holder:?}")
                        }
                        SweepAction::Park => {
                            assert_eq!(class, SlotClass::QuarantinedUnknown, "{holder:?}")
                        }
                        SweepAction::NotStuck => {
                            unreachable!("a dead owner is never NotStuck")
                        }
                    }
                }
            }
        }
    }

    /// The gap-A property: a dead-owner device a live guest still holds open,
    /// with NO record, is Quarantined — CLASSIFIED, never reaped.
    #[test]
    fn gap_a_live_survivor_without_a_record_is_quarantined_never_reaped() {
        assert_eq!(
            classify_startup_slot(PidLiveness::Dead, DeviceHolder::LiveHolder, false),
            SlotClass::QuarantinedUnknown,
        );
        // An unprovable holder (scan error) is likewise quarantined, never
        // reaped — absence of proof is not proof of death.
        assert_eq!(
            classify_startup_slot(PidLiveness::Dead, DeviceHolder::Unknown, false),
            SlotClass::QuarantinedUnknown,
        );
    }

    /// `classify_startup_slots` partitions and only `TerminalSafeToReap` reaches
    /// the `ReapList`.
    #[test]
    fn classify_slots_partitions_and_only_terminal_reaches_reap() {
        let slots = vec![
            // Serving (self owner).
            StartupSlot {
                device: 1u32,
                liveness: PidLiveness::SelfPid,
                holder: DeviceHolder::LiveHolder,
                has_record: true,
            },
            // ReconnectMe (dead owner, record).
            StartupSlot {
                device: 2,
                liveness: PidLiveness::Dead,
                holder: DeviceHolder::LiveHolder,
                has_record: true,
            },
            // QuarantinedUnknown (dead owner, live holder, no record — gap A).
            StartupSlot {
                device: 3,
                liveness: PidLiveness::Dead,
                holder: DeviceHolder::LiveHolder,
                has_record: false,
            },
            // TerminalSafeToReap (dead owner, proof of death, no record).
            StartupSlot {
                device: 4,
                liveness: PidLiveness::Dead,
                holder: DeviceHolder::NoHolder,
                has_record: false,
            },
        ];
        let c = classify_startup_slots(slots);
        assert_eq!(c.serving, vec![1]);
        assert_eq!(c.reconnect, vec![2]);
        assert_eq!(c.quarantined, vec![3]);
        assert_eq!(c.reap.devices(), &[4]);
        assert_eq!(c.reap.len(), 1);
        assert!(!c.reap.is_empty());
    }
}
