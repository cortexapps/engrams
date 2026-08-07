//! Per-sandbox APFS-clone-backed disk management.
//!
//! Replaces the previous "every VM attaches the bake rootfs.ext4
//! directly" pattern, which had a latent corruption bug
//! (concurrent sandboxes from the same image would race writes
//! to the shared ext4) and made cold-resume incoherent (a fresh
//! VM would inherit the previous session's mutations rather than
//! its own state).
//!
//! Each sandbox gets its own clone of the bake image at create
//! time. APFS `clonefile(2)` is COW-backed: a 1.7 GB ext4 clones
//! in ~50 ms with no extra disk usage; only diverged blocks (the
//! parts the guest mutates) ever consume real space. macOS-only —
//! we're already in the VZ branch so that's fine.
//!
//! Snapshot: pause VM → APFS-clone the per-sandbox rootfs into
//! the snapshot dir → resume. The clone is the durable record
//! of "session state at idle-evict time."
//!
//! Restore: APFS-clone the snapshot rootfs into a fresh
//! `<work_dir>/<sandbox_id>.rootfs.ext4` and attach that to a
//! cold-booted VM. The previous session's disk-state is
//! preserved; the in-memory state (Claude conversation buffer,
//! page cache, etc.) is discarded but Claude's `--resume
//! <session-id>` rehydrates conversation context against
//! Anthropic's API.

use std::path::Path;

/// Errors from the disk-clone layer.
#[derive(Debug)]
pub(crate) enum DiskError {
    /// Source path doesn't exist.
    SourceMissing(std::path::PathBuf),
    /// `clonefile(2)` failed AND the `fs::copy` fallback also
    /// failed. The first error is the clonefile errno; the
    /// second is the copy error.
    CopyFailed {
        clone_err: std::io::Error,
        copy_err: std::io::Error,
    },
    /// Couldn't pre-remove an existing destination file before
    /// the clone (clonefile fails if the destination exists).
    RemoveFailed(std::io::Error),
    /// Couldn't create the destination's parent directory.
    MkdirFailed(std::io::Error),
}

impl std::fmt::Display for DiskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceMissing(p) => write!(f, "source rootfs missing: {}", p.display()),
            Self::CopyFailed {
                clone_err,
                copy_err,
            } => {
                write!(
                    f,
                    "clonefile failed ({clone_err}); fs::copy fallback also failed ({copy_err})"
                )
            }
            Self::RemoveFailed(e) => write!(f, "remove existing destination failed: {e}"),
            Self::MkdirFailed(e) => write!(f, "create destination parent dir failed: {e}"),
        }
    }
}

impl std::error::Error for DiskError {}

impl From<DiskError> for engram_core::SandboxError {
    fn from(e: DiskError) -> Self {
        engram_core::SandboxError::Vm(Box::new(e))
    }
}

/// APFS-clone `src` to `dst`. The file is duplicated COW-style
/// — both paths reference the same on-disk blocks until one
/// side writes, at which point only the diverged blocks consume
/// real disk. ~50 ms even for 1.7 GB images on Apple Silicon.
///
/// Falls back to `std::fs::copy` if clonefile fails (e.g., when
/// `src` and `dst` are on different volumes — APFS clonefile
/// requires same-volume). The fallback isn't COW so it does a
/// real byte-for-byte copy; loud `tracing::warn` so the operator
/// knows performance dropped.
///
/// Async: `clonefile(2)` is fast but synchronous, so we run it
/// on the blocking pool. The fallback `std::fs::copy` is also
/// blocking, hence both go through `spawn_blocking`.
pub(crate) async fn clone_or_copy(src: &Path, dst: &Path) -> Result<(), DiskError> {
    if !src.exists() {
        return Err(DiskError::SourceMissing(src.to_path_buf()));
    }
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(DiskError::MkdirFailed)?;
    }
    if dst.exists() {
        tokio::fs::remove_file(dst)
            .await
            .map_err(DiskError::RemoveFailed)?;
    }
    let src_owned = src.to_path_buf();
    let dst_owned = dst.to_path_buf();
    tokio::task::spawn_blocking(move || clone_or_copy_blocking(&src_owned, &dst_owned))
        .await
        .map_err(|e| DiskError::CopyFailed {
            clone_err: std::io::Error::other(format!("spawn_blocking join: {e}")),
            copy_err: std::io::Error::other("n/a"),
        })?
}

fn clone_or_copy_blocking(src: &Path, dst: &Path) -> Result<(), DiskError> {
    let src_c = std::ffi::CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
        DiskError::CopyFailed {
            clone_err: std::io::Error::other(format!("src CString: {e}")),
            copy_err: std::io::Error::other("n/a"),
        }
    })?;
    let dst_c = std::ffi::CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
        DiskError::CopyFailed {
            clone_err: std::io::Error::other(format!("dst CString: {e}")),
            copy_err: std::io::Error::other("n/a"),
        }
    })?;
    // SAFETY: both paths are valid C strings; libc::clonefile
    // accepts NULL flags as 0. On success returns 0; on error
    // returns -1 and sets errno.
    let r = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if r == 0 {
        return Ok(());
    }
    let clone_err = std::io::Error::last_os_error();
    tracing::warn!(
        src = %src.display(),
        dst = %dst.display(),
        error = %clone_err,
        "clonefile failed; falling back to fs::copy (slower, not COW)"
    );
    match std::fs::copy(src, dst) {
        Ok(_) => Ok(()),
        Err(copy_err) => Err(DiskError::CopyFailed {
            clone_err,
            copy_err,
        }),
    }
}

/// Compute the per-sandbox rootfs path under the backend's
/// `work_dir`, named `<sid>.rootfs.ext4` — flat, without a
/// per-sandbox subdir explosion. (The bridge's `<sid>.vsock_*` UDS
/// files live in a short SUN_LEN-safe dir, not here; see
/// `engram_core::socket`.)
pub(crate) fn per_sandbox_rootfs_path(
    work_dir: &Path,
    sandbox_id: engram_core::types::ids::SandboxId,
) -> std::path::PathBuf {
    work_dir.join(format!("{sandbox_id}.rootfs.ext4"))
}

/// ADR 0112: the per-residence sparse swap backing file, sibling of the
/// rootfs clone. FRESH every create/restore (swap contents are
/// discarded at capture by contract); removed with the rootfs at
/// destroy.
pub(crate) fn per_sandbox_swap_path(
    work_dir: &Path,
    sandbox_id: engram_core::types::ids::SandboxId,
) -> std::path::PathBuf {
    work_dir.join(format!("{sandbox_id}.swap.img"))
}

/// ADR 0112: create (truncate) the sparse swap backing, sized to
/// exactly `swap_mib` — the virtio device size IS the per-sandbox cap.
pub(crate) async fn create_sparse_swap(path: &Path, swap_mib: u32) -> Result<(), std::io::Error> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        // 0600, never the ambient umask (adversarial-review finding):
        // unlike FC's unlinked-after-attach backing, this file keeps
        // its name for the VM's whole lifetime and holds guest memory
        // in plaintext — the FC implementation sets the same mode.
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.set_len(u64::from(swap_mib) * 1024 * 1024)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Filename of the rootfs clone inside a snapshot directory.
/// Sibling to `manifest.json`. The snapshot's rootfs replaces the
/// previous (broken) `state.bin` — we don't save VZ memory state
/// at all on this backend, just the disk.
pub(crate) const SNAPSHOT_ROOTFS_FILENAME: &str = "rootfs.ext4";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clone_round_trips_a_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        let payload: Vec<u8> = (0u8..=200).collect();
        std::fs::write(&src, &payload).unwrap();
        clone_or_copy(&src, &dst).await.expect("clone");
        let read_back = std::fs::read(&dst).unwrap();
        assert_eq!(read_back, payload);
    }

    #[tokio::test]
    async fn clone_overwrites_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"OLD-CONTENT-SHOULD-BE-WIPED").unwrap();
        clone_or_copy(&src, &dst).await.expect("clone");
        assert_eq!(std::fs::read(&dst).unwrap(), b"new");
    }

    #[tokio::test]
    async fn clone_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("nested/sub/dst.bin");
        std::fs::write(&src, b"hi").unwrap();
        clone_or_copy(&src, &dst).await.expect("clone w/ parent");
        assert_eq!(std::fs::read(&dst).unwrap(), b"hi");
    }

    #[tokio::test]
    async fn clone_errors_cleanly_when_source_missing() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("dst.bin");
        let err = clone_or_copy(&dir.path().join("does-not-exist"), &dst)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("does-not-exist"), "{msg}");
    }
}
