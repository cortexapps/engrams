//! ADR 0080: adopt the agentd bundle generation attached at the
//! reserved agentd slot.
//!
//! agentd is not baked into the rootfs: the stage-1 init copies it out
//! of its bundle mount (`/opt/engram/dyn/<i>`, carrying `engram-agentd`
//! and `agentd.sha256`) to tmpfs (`/run/engram/`) and execs the copy,
//! so nothing ever executes from the bundle drive — which is what
//! makes the host's paused-window `patch_drive` swap safe under a
//! running agentd (the same asymmetry as the harness slot, ADR 0062).
//!
//! On a fresh-create restore the host sends `WireRequest::RefreshAgent`
//! before any session state binds. We re-mount the bundle mounts (the
//! device may carry swapped bytes), compare the slot's stamp against
//! the one we booted from, and — on mismatch — stage the new binary
//! over the tmpfs copy (temp + rename; the running inode is untouched)
//! and `execv` ourselves onto it. PID 1 exec: same pid, same argv, same
//! env; the vsock listener re-binds in the new image and the host
//! re-polls readiness.

use std::io;
use std::path::PathBuf;

/// tmpfs home of the running agentd (populated by the stage-1 init).
pub const RUN_DIR: &str = "/run/engram";
/// The binary we're executing (a tmpfs copy of the bundle's).
pub const RUN_BIN: &str = "/run/engram/engram-agentd";
/// Content stamp of the copy at `RUN_BIN` — written by the stage-1
/// init at boot and by [`stage`] on refresh.
pub const RUN_STAMP: &str = "/run/engram/agentd.sha256";

/// File names inside the agentd bundle mount.
pub const BUNDLE_BIN: &str = "engram-agentd";
pub const BUNDLE_STAMP: &str = "agentd.sha256";

/// Where the init shim mounts the reserved dynamic slots (mirrors
/// `AuxRoDrive::slot_guest_mount`; agentd probes rather than trusting
/// an index because VZ compacts resolved drives onto sequential
/// mounts).
const DYN_MOUNT_ROOT: &str = "/opt/engram/dyn";

/// What a `RefreshAgent` round decided.
#[derive(Debug)]
pub enum Refresh {
    /// Running copy matches the slot stamp (or there's nothing to
    /// compare — no agentd bundle mounted / no stamp; logged).
    UpToDate { sha256: Option<String> },
    /// The slot carries a different generation; it has been staged
    /// over the tmpfs copy. The caller replies to the host, flushes,
    /// and then calls [`exec_staged`] — which does not return.
    Staged { sha256: String },
}

/// Locate the agentd bundle mount by probing the dyn slots for the
/// binary. Position-independent: FC keeps `dyn/<i> == slot i`, VZ
/// compacts resolved drives, and either way exactly one mount carries
/// `engram-agentd`.
pub fn find_bundle_mount() -> Option<PathBuf> {
    let entries = std::fs::read_dir(DYN_MOUNT_ROOT).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.join(BUNDLE_BIN).is_file() {
            return Some(dir);
        }
    }
    None
}

/// Compare the slot's stamp against the running copy's and stage the
/// new binary when they differ. Never execs — the caller sequences the
/// wire reply between [`Refresh::Staged`] and [`exec_staged`].
pub fn check_and_stage() -> io::Result<Refresh> {
    let Some(bundle) = find_bundle_mount() else {
        // No agentd bundle attached: an old-model image whose agentd is
        // baked into the rootfs (transitional), or a backend without
        // bundle drives. Nothing to adopt.
        tracing::info!(
            "RefreshAgent: no agentd bundle mounted under {DYN_MOUNT_ROOT}; keeping the running agentd"
        );
        return Ok(Refresh::UpToDate {
            sha256: running_stamp(),
        });
    };
    let staged = std::fs::read_to_string(bundle.join(BUNDLE_STAMP))
        .map(|s| s.trim().to_string())
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "agentd bundle at {} carries no readable {BUNDLE_STAMP}: {e}",
                    bundle.display()
                ),
            )
        })?;
    if running_stamp().as_deref() == Some(staged.as_str()) {
        return Ok(Refresh::UpToDate {
            sha256: Some(staged),
        });
    }
    stage(&bundle, &staged)?;
    Ok(Refresh::Staged { sha256: staged })
}

/// The stamp the running copy booted from. `None` (with a log) when the
/// stage-1 init didn't record one — comparison then always stages,
/// which is the safe direction.
fn running_stamp() -> Option<String> {
    match std::fs::read_to_string(RUN_STAMP) {
        Ok(s) => Some(s.trim().to_string()),
        Err(e) => {
            tracing::warn!(error = %e, "RefreshAgent: no running stamp at {RUN_STAMP}");
            None
        }
    }
}

/// Copy the bundle's binary + stamp over the tmpfs copy, temp + rename.
/// Rename keeps the running inode intact (no ETXTBSY: we never write
/// through the executing path) and makes the swap atomic — a crash
/// between the two renames leaves a runnable binary either way.
fn stage(bundle: &std::path::Path, sha256: &str) -> io::Result<()> {
    std::fs::create_dir_all(RUN_DIR)?;
    let bin_tmp = format!("{RUN_BIN}.next");
    std::fs::copy(bundle.join(BUNDLE_BIN), &bin_tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin_tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&bin_tmp, RUN_BIN)?;
    let stamp_tmp = format!("{RUN_STAMP}.next");
    std::fs::write(&stamp_tmp, format!("{sha256}\n"))?;
    std::fs::rename(&stamp_tmp, RUN_STAMP)?;
    Ok(())
}

/// `execv` the staged binary with this process's own argv (env rides
/// along — `execv` keeps `environ`). PID 1 exec: the process image is
/// replaced in place; every fd is CLOEXEC (Rust std default) so the
/// wire connection and the old listener close, and the new agentd
/// re-binds and answers the host's readiness re-poll. Only returns on
/// error.
#[cfg(target_os = "linux")]
pub fn exec_staged() -> io::Error {
    use std::ffi::CString;
    let argv: Vec<CString> = std::env::args_os()
        .enumerate()
        .filter_map(|(i, a)| {
            let bytes = if i == 0 {
                RUN_BIN.as_bytes().to_vec()
            } else {
                use std::os::unix::ffi::OsStrExt;
                a.as_bytes().to_vec()
            };
            CString::new(bytes).ok()
        })
        .collect();
    let path = CString::new(RUN_BIN).expect("RUN_BIN has no interior NUL");
    match nix::unistd::execv(&path, &argv) {
        Err(e) => io::Error::other(format!("execv {RUN_BIN}: {e}")),
        Ok(infallible) => match infallible {},
    }
}

#[cfg(not(target_os = "linux"))]
pub fn exec_staged() -> io::Error {
    io::Error::other("exec_staged is Linux-only (agentd runs in Linux guests)")
}
