//! Canonical jail layout for Firecracker sandboxes (ADR 0014).
//!
//! Contract module: documents the host-side path scheme that makes
//! FC snapshots portable across hosts. The wiring (FC backend calls
//! these helpers, snapshots embed canonical paths in `state.bin`,
//! receivers materialize at canonical paths) lands in M1.2 alongside
//! the BlobStorage upload primitive — the two changes are coupled
//! because the canonical-path scheme is only useful when the
//! receiver-side materialization is also in place.
//!
//! ## The portability problem
//!
//! FC's `state.bin` embeds every drive's `path_on_host`, every Unix
//! socket path, every TAP name. There is **no FC API to rewrite
//! state.bin** post-capture. For a snapshot to be portable across
//! hosts, every host must materialize the artifacts the snapshot
//! references at *identical* paths.
//!
//! ## The scheme
//!
//! - `<work_dir>` is identical fleet-wide (packer-installed value:
//!   `/var/lib/engram/sandboxes`). FC backend takes it via
//!   `FirecrackerBackend::new(work_dir, ...)`.
//! - Jail dir: `<work_dir>/<sandbox_id>/` — per-FC-process runtime
//!   state (api socket, log, uffd uds). Removed on destroy.
//! - vsock UDS: `<work_dir>/<sandbox_id>.vsock` — outside the jail
//!   on purpose; destroy's `remove_dir_all(jail_dir)` doesn't break
//!   a subsequent restore that reopens the same UDS path.
//! - **Rootfs / harness paths embedded in state.bin** (M1.2 wiring):
//!   keyed by **snapshot_id**, NOT sandbox_id, so multiple sandboxes
//!   restored from the same snapshot all reference the same
//!   materialized files (with mount-namespace isolation for the
//!   writable case — N>1 warm slots from one template). The
//!   snapshot id is allocated at capture time and survives across
//!   sandbox destroy/restore, which is the property that makes
//!   per-sandbox-id paths fail.
//! - FC API socket: `<jail>/firecracker.sock`. Never embedded in
//!   state.bin (only used host-side, not captured).
//! - UFFD handler UDS: `<jail>/uffd.sock`. Re-created per restore;
//!   not embedded in state.bin either.
//!
//! Helpers here are split into "uncontroversial" (jail dir,
//! firecracker socket, vsock path — all already in use in lib.rs
//! literally) and "M1.2 wiring" (rootfs/harness symlink + assertion
//! helpers, callsites land later). The latter are kept here so the
//! M1.2 PR is a single coherent change.

use std::path::{Path, PathBuf};

use engram_core::types::SandboxId;

/// `<work_dir>/<sandbox_id>/`.
pub fn jail_dir(work_dir: &Path, sandbox_id: SandboxId) -> PathBuf {
    work_dir.join(sandbox_id.to_string())
}

/// `<work_dir>/<sandbox_id>.vsock`. Outside the jail by design — see
/// module docs.
pub fn vsock_uds_path(work_dir: &Path, sandbox_id: SandboxId) -> PathBuf {
    work_dir.join(format!("{sandbox_id}.vsock"))
}

/// `<jail>/rootfs.dev` — canonical rootfs symlink target. FC
/// `put_drive` and the embedded path inside `state.bin` are both
/// this.
pub fn rootfs_link(jail_dir: &Path) -> PathBuf {
    jail_dir.join("rootfs.dev")
}

/// `<jail>/harness.ext4` — canonical harness-substrate symlink
/// target. Present only when the spec had a harness substrate.
pub fn harness_link(jail_dir: &Path) -> PathBuf {
    jail_dir.join("harness.ext4")
}

/// `<jail>/firecracker.sock`.
pub fn firecracker_socket(jail_dir: &Path) -> PathBuf {
    jail_dir.join("firecracker.sock")
}

/// `<jail>/firecracker.log`.
pub fn firecracker_log(jail_dir: &Path) -> PathBuf {
    jail_dir.join("firecracker.log")
}

/// `<jail>/uffd.sock`.
pub fn uffd_uds(jail_dir: &Path) -> PathBuf {
    jail_dir.join("uffd.sock")
}

/// Create (or replace) the symlink at `canonical` pointing at
/// `target`. Idempotent: removes any pre-existing entry first.
/// Errors map to `std::io::Error` for caller-side context wrapping.
pub async fn install_symlink(canonical: &Path, target: &Path) -> std::io::Result<()> {
    // remove_file works for symlinks (it removes the link entry, not
    // the target). Ignore NotFound so first-time creation is a single
    // call.
    match tokio::fs::remove_file(canonical).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    tokio::fs::symlink(target, canonical).await
}

/// Verify the rootfs canonical symlink exists at `<jail>/rootfs.dev`
/// and resolves to a host-visible path. Returns the link target.
/// Snapshot creation calls this before producing the artifact —
/// refusing to snapshot a non-canonical sandbox prevents shipping a
/// blob that won't restore on a sibling host.
pub async fn assert_rootfs_canonical(jail_dir: &Path) -> Result<PathBuf, String> {
    let link = rootfs_link(jail_dir);
    let target = tokio::fs::read_link(&link).await.map_err(|e| {
        format!(
            "rootfs canonical symlink missing at {}: {e}",
            link.display()
        )
    })?;
    if !tokio::fs::try_exists(&target).await.unwrap_or(false) {
        return Err(format!(
            "rootfs symlink target {} not present on host",
            target.display()
        ));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::SandboxId;

    #[test]
    fn jail_layout_uses_sandbox_id() {
        let id = SandboxId::new();
        let work = Path::new("/var/lib/engram/sandboxes");
        let jail = jail_dir(work, id);
        assert_eq!(
            jail,
            Path::new("/var/lib/engram/sandboxes").join(id.to_string())
        );
        assert_eq!(rootfs_link(&jail), jail.join("rootfs.dev"));
        assert_eq!(harness_link(&jail), jail.join("harness.ext4"));
        assert_eq!(uffd_uds(&jail), jail.join("uffd.sock"));
    }

    #[test]
    fn vsock_uds_lives_outside_jail() {
        let id = SandboxId::new();
        let work = Path::new("/var/lib/engram/sandboxes");
        let vsock = vsock_uds_path(work, id);
        let jail = jail_dir(work, id);
        // vsock is a sibling of jail, not under it — see module docs.
        assert_eq!(vsock.parent(), Some(work));
        assert!(!vsock.starts_with(&jail));
    }

    #[tokio::test]
    async fn install_symlink_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        tokio::fs::write(&target, b"hello").await.unwrap();
        let link = tmp.path().join("rootfs.dev");

        // First creation: link doesn't exist yet.
        install_symlink(&link, &target).await.unwrap();
        assert_eq!(tokio::fs::read_link(&link).await.unwrap(), target);

        // Second creation with same target: still works.
        install_symlink(&link, &target).await.unwrap();
        assert_eq!(tokio::fs::read_link(&link).await.unwrap(), target);

        // Re-point to a different target.
        let other = tmp.path().join("other");
        tokio::fs::write(&other, b"world").await.unwrap();
        install_symlink(&link, &other).await.unwrap();
        assert_eq!(tokio::fs::read_link(&link).await.unwrap(), other);
    }

    #[tokio::test]
    async fn assert_rootfs_canonical_rejects_missing_link() {
        let tmp = tempfile::tempdir().unwrap();
        let err = assert_rootfs_canonical(tmp.path()).await.unwrap_err();
        assert!(err.contains("rootfs canonical symlink missing"));
    }

    #[tokio::test]
    async fn assert_rootfs_canonical_rejects_dangling_link() {
        let tmp = tempfile::tempdir().unwrap();
        let dangling = tmp.path().join("does-not-exist");
        let link = rootfs_link(tmp.path());
        install_symlink(&link, &dangling).await.unwrap();
        let err = assert_rootfs_canonical(tmp.path()).await.unwrap_err();
        assert!(err.contains("not present on host"));
    }

    #[tokio::test]
    async fn assert_rootfs_canonical_accepts_resolvable_link() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("rootfs.ext4");
        tokio::fs::write(&target, b"ext4").await.unwrap();
        let link = rootfs_link(tmp.path());
        install_symlink(&link, &target).await.unwrap();
        let resolved = assert_rootfs_canonical(tmp.path()).await.unwrap();
        assert_eq!(resolved, target);
    }
}
