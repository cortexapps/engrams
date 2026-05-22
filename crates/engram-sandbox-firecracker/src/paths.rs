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

/// `<work_dir>/rootfs/<sandbox_id>.dev` — canonical rootfs symlink
/// path. FC `put_drive`'s `path_on_host` and the embedded path
/// inside `state.bin` are both this. Lives **outside** the jail by
/// design: `destroy()`'s `remove_dir_all(jail_dir)` doesn't touch
/// it, so a snapshot taken from this sandbox stays restorable
/// after the sandbox is gone. The symlink itself is removed on
/// destroy via [`canonical_dirs_for_destroy`]; on restore, the
/// receiver re-creates it pointing at whatever it has materialized
/// locally.
pub fn rootfs_canonical(work_dir: &Path, sandbox_id: SandboxId) -> PathBuf {
    work_dir.join("rootfs").join(format!("{sandbox_id}.dev"))
}

/// `<work_dir>/harness/<sandbox_id>.ext4` — canonical harness-
/// substrate symlink path. Same shape as [`rootfs_canonical`].
/// Present only when the spec had a harness substrate.
pub fn harness_canonical(work_dir: &Path, sandbox_id: SandboxId) -> PathBuf {
    work_dir.join("harness").join(format!("{sandbox_id}.ext4"))
}

/// Parent dirs that must exist before [`install_symlink`] can land
/// the canonical-path entries. Idempotent — `create_dir_all` is
/// the right primitive at the callsite.
pub fn canonical_parent_dirs(work_dir: &Path) -> [PathBuf; 2] {
    [work_dir.join("rootfs"), work_dir.join("harness")]
}

/// The canonical-path entries owned by this sandbox (rootfs +
/// harness). `destroy()` should `remove_file` each one — they're
/// symlinks, not directories, so a single `remove_file` is enough
/// and a `NotFound` is benign (caller didn't have one of them).
pub fn canonical_entries_for(work_dir: &Path, sandbox_id: SandboxId) -> [PathBuf; 2] {
    [
        rootfs_canonical(work_dir, sandbox_id),
        harness_canonical(work_dir, sandbox_id),
    ]
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
///
/// Symlink targets are resolved relative to the symlink's *parent
/// directory*, not the process CWD — so a relative target like
/// `./var/host-sandboxes-integration/chunked-rootfs/<m>.ext4`
/// installed at `<work_dir>/rootfs/<sid>.dev` resolves to
/// `<work_dir>/rootfs/./var/...` and ENOENTs at every open. To
/// keep callers position-independent we absolutise the target
/// against the process CWD before symlinking (the target must
/// exist when symlink lands — callers that violate this get an
/// error here rather than a broken link later).
pub async fn install_symlink(canonical: &Path, target: &Path) -> std::io::Result<()> {
    let abs_target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        let cwd = std::env::current_dir()?;
        cwd.join(target)
    };
    // remove_file works for symlinks (it removes the link entry, not
    // the target). Ignore NotFound so first-time creation is a single
    // call.
    match tokio::fs::remove_file(canonical).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    tokio::fs::symlink(&abs_target, canonical).await
}

/// Verify the rootfs canonical symlink exists at
/// `<work_dir>/rootfs/<sandbox_id>.dev` and resolves to a host-
/// visible path. Returns the link target. Snapshot creation calls
/// this before producing the artifact — refusing to snapshot a
/// non-canonical sandbox prevents shipping a blob that won't
/// restore on a sibling host.
pub async fn assert_rootfs_canonical(
    work_dir: &Path,
    sandbox_id: SandboxId,
) -> Result<PathBuf, String> {
    let link = rootfs_canonical(work_dir, sandbox_id);
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
        // Per-jail entries (FC api socket, log, uffd uds) live
        // INSIDE the jail and are removed on destroy.
        assert_eq!(firecracker_socket(&jail), jail.join("firecracker.sock"));
        assert_eq!(firecracker_log(&jail), jail.join("firecracker.log"));
        assert_eq!(uffd_uds(&jail), jail.join("uffd.sock"));
    }

    #[test]
    fn canonical_rootfs_lives_outside_jail() {
        let id = SandboxId::new();
        let work = Path::new("/var/lib/engram/sandboxes");
        let canonical = rootfs_canonical(work, id);
        let jail = jail_dir(work, id);
        // The canonical rootfs path is under <work_dir>/rootfs/, a
        // sibling of the jail. Survives `remove_dir_all(jail)`.
        assert_eq!(canonical, work.join("rootfs").join(format!("{id}.dev")));
        assert!(!canonical.starts_with(&jail));
    }

    #[test]
    fn canonical_harness_lives_outside_jail() {
        let id = SandboxId::new();
        let work = Path::new("/var/lib/engram/sandboxes");
        let canonical = harness_canonical(work, id);
        let jail = jail_dir(work, id);
        assert_eq!(canonical, work.join("harness").join(format!("{id}.ext4")));
        assert!(!canonical.starts_with(&jail));
    }

    #[test]
    fn canonical_entries_for_lists_both_paths() {
        let id = SandboxId::new();
        let work = Path::new("/var/lib/engram/sandboxes");
        let entries = canonical_entries_for(work, id);
        assert_eq!(entries[0], rootfs_canonical(work, id));
        assert_eq!(entries[1], harness_canonical(work, id));
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
        let id = SandboxId::new();
        let err = assert_rootfs_canonical(tmp.path(), id).await.unwrap_err();
        assert!(err.contains("rootfs canonical symlink missing"));
    }

    #[tokio::test]
    async fn assert_rootfs_canonical_rejects_dangling_link() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        for parent in canonical_parent_dirs(tmp.path()) {
            tokio::fs::create_dir_all(&parent).await.unwrap();
        }
        let dangling = tmp.path().join("does-not-exist");
        install_symlink(&rootfs_canonical(tmp.path(), id), &dangling)
            .await
            .unwrap();
        let err = assert_rootfs_canonical(tmp.path(), id).await.unwrap_err();
        assert!(err.contains("not present on host"));
    }

    #[tokio::test]
    async fn assert_rootfs_canonical_accepts_resolvable_link() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        for parent in canonical_parent_dirs(tmp.path()) {
            tokio::fs::create_dir_all(&parent).await.unwrap();
        }
        let target = tmp.path().join("source.ext4");
        tokio::fs::write(&target, b"ext4").await.unwrap();
        install_symlink(&rootfs_canonical(tmp.path(), id), &target)
            .await
            .unwrap();
        let resolved = assert_rootfs_canonical(tmp.path(), id).await.unwrap();
        assert_eq!(resolved, target);
    }
}
