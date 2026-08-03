//! Survivor capture and resume decisions (ADR 0098 coverage-gap G2).
//!
//! Session 03e6535e (#743) showed that an untracked survivor can produce an
//! incomplete capture, and that a resume without a prepared NBD attach can
//! boot Firecracker on a stale literal `/dev/nbdN` path. The host simulator
//! drives both pure decisions.

/// The capture disk-drain verdict for one sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureDrainPlan {
    /// The data plane is tracked. Drain it under the pause.
    Drain,
    /// The rootfs is NBD-backed but no data-plane record exists.
    RefuseUntracked,
    /// The sandbox has no NBD disk tier.
    NoNbdDisk,
}

/// Decide the capture disk-drain arm.
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
        assert_eq!(
            plan_capture_disk_drain(true, true, true),
            CaptureDrainPlan::Drain
        );
        assert_eq!(
            plan_capture_disk_drain(true, false, false),
            CaptureDrainPlan::Drain
        );
        assert_eq!(
            plan_capture_disk_drain(false, true, true),
            CaptureDrainPlan::RefuseUntracked
        );
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
