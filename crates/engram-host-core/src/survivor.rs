//! The survivor-invisibility DECISIONS (ADR 0098 coverage-gap G2).
//!
//! Two incidents in two days shared one mechanism: **a lookup keyed on a
//! tracking map that a post-roll survivor isn't in, causing a silent skip
//! that corrupts.** Session 731df805 (#739) was the register/sweep legs —
//! their pure cores live in [`crate::reattach`]
//! ([`is_local_survivor_candidate`](crate::is_local_survivor_candidate),
//! [`sweep_verdict`](crate::sweep_verdict)). Session 03e6535e (#743) was
//! the *capture* and *resume* legs: `nbd_sandboxes.get(id)` missing for a
//! never-rehydrated survivor made capture record a `recoverable` snapshot
//! with `disk_manifest=None` (dropping every acked disk write) and made
//! resume boot FC onto the capture-time literal `/dev/nbdN` (a dead — or
//! worse, foreign — device). This module holds those two legs' pure
//! verdicts, wired into `PooledBackend::snapshot`'s drain arm and
//! `PooledBackend::restore`'s non-NBD fallback; the host simulator drives
//! the same fns and pins both the refusals and the pre-#743 corruption
//! (`engram-dst-host` survivor-invisibility seeds).

/// The capture disk-drain verdict for one sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureDrainPlan {
    /// The data plane is tracked — drain it under the pause (the normal
    /// NBD capture leg).
    Drain,
    /// NBD-backed rootfs but NO tracking entry: a post-roll survivor
    /// whose disk server died un-rehydrated. Refuse the capture — a
    /// silent skip records `disk_manifest=None` + `recoverable=true`,
    /// dropping the session's acked writes and poisoning its lineage
    /// (the 03e6535e class). The error requeues the eviction op.
    RefuseUntracked,
    /// Legitimately no NBD disk tier (macOS/dev/flat-file rootfs, or a
    /// host that doesn't run the data plane): skipping the drain is
    /// correct, not silent loss.
    NoNbdDisk,
}

/// Decide the capture disk-drain arm. `tracked` = the sandbox has a live
/// tracking entry (`nbd_sandboxes.get(id)` is `Some`);
/// `host_runs_nbd_data_plane` = this host wires the NBD data plane at
/// all; `rootfs_is_nbd_device` = the sandbox's rootfs actually sits on a
/// `/dev/nbd*` device.
pub fn plan_capture_disk_drain(
    tracked: bool,
    host_runs_nbd_data_plane: bool,
    rootfs_is_nbd_device: bool,
) -> CaptureDrainPlan {
    if tracked {
        return CaptureDrainPlan::Drain;
    }
    if host_runs_nbd_data_plane && rootfs_is_nbd_device {
        return CaptureDrainPlan::RefuseUntracked;
    }
    CaptureDrainPlan::NoNbdDisk
}

/// The resume rootfs-attach verdict for one snapshot restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeAttachPlan {
    /// A chunked NBD attach was prepared — FC restores against the
    /// freshly-served device (the normal path).
    Attach,
    /// No attach was prepared and the sidecar still names a literal
    /// `/dev/nbd*` rootfs on a data-plane host: restoring would boot
    /// onto a stale/foreign device (EIO → guest SIGBUS at best, another
    /// session's live disk at worst — the 03e6535e class). Refuse; the
    /// resume op requeues and the poisoned lineage surfaces loudly.
    RefuseStaleLiteral,
    /// No NBD attach and no literal-device hazard: the
    /// materialize-to-file fallback is legitimate (flat-file rootfs,
    /// macOS/dev, or a host without the data plane).
    Materialize,
}

/// Decide the resume attach arm. `nbd_attach_prepared` = the chunked
/// attach path took (`prepare_resume_nbd_attach` produced a pending
/// state); `host_runs_nbd_data_plane` as above;
/// `sidecar_rootfs_is_nbd_literal` = the snapshot sidecar's
/// `spec.rootfs_source` still names a literal `/dev/nbd*` device.
pub fn plan_resume_attach(
    nbd_attach_prepared: bool,
    host_runs_nbd_data_plane: bool,
    sidecar_rootfs_is_nbd_literal: bool,
) -> ResumeAttachPlan {
    if nbd_attach_prepared {
        return ResumeAttachPlan::Attach;
    }
    if host_runs_nbd_data_plane && sidecar_rootfs_is_nbd_literal {
        return ResumeAttachPlan::RefuseStaleLiteral;
    }
    ResumeAttachPlan::Materialize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_drain_verdicts() {
        // Tracked always drains, regardless of the other axes.
        assert_eq!(
            plan_capture_disk_drain(true, true, true),
            CaptureDrainPlan::Drain
        );
        assert_eq!(
            plan_capture_disk_drain(true, false, false),
            CaptureDrainPlan::Drain
        );
        // The 03e6535e refusal: untracked + data-plane host + nbd rootfs.
        assert_eq!(
            plan_capture_disk_drain(false, true, true),
            CaptureDrainPlan::RefuseUntracked
        );
        // Legitimate skips: no data plane, or a non-NBD rootfs.
        assert_eq!(
            plan_capture_disk_drain(false, false, true),
            CaptureDrainPlan::NoNbdDisk
        );
        assert_eq!(
            plan_capture_disk_drain(false, true, false),
            CaptureDrainPlan::NoNbdDisk
        );
    }

    #[test]
    fn resume_attach_verdicts() {
        assert_eq!(
            plan_resume_attach(true, true, true),
            ResumeAttachPlan::Attach
        );
        // The 03e6535e refusal: no attach + data-plane host + literal nbd
        // sidecar rootfs.
        assert_eq!(
            plan_resume_attach(false, true, true),
            ResumeAttachPlan::RefuseStaleLiteral
        );
        // Legitimate materialize: non-NBD sidecar rootfs, or no data plane.
        assert_eq!(
            plan_resume_attach(false, true, false),
            ResumeAttachPlan::Materialize
        );
        assert_eq!(
            plan_resume_attach(false, false, true),
            ResumeAttachPlan::Materialize
        );
    }
}
