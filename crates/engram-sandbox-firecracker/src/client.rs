//! HTTP-over-Unix-socket client for the Firecracker control API.
//!
//! Phase 2 will fill this in. Each `FirecrackerBackend`-managed VM has
//! its own Firecracker process listening on a per-sandbox Unix socket;
//! this client speaks the swagger-defined API documented in
//! `firecracker/swagger/firecracker.yaml`.
//!
//! The endpoints we need:
//!
//! | Method  | Path                     | When                             |
//! |---------|--------------------------|----------------------------------|
//! | `PUT`   | `/machine-config`        | configure on create              |
//! | `PUT`   | `/boot-source`           | configure on create              |
//! | `PUT`   | `/drives/{id}`           | attach rootfs / scratch disks    |
//! | `PUT`   | `/network-interfaces/{id}` | attach TAP                     |
//! | `PUT`   | `/vsock`                 | attach virtio-vsock              |
//! | `PUT`   | `/actions`               | `InstanceStart`, `SendCtrlAltDel` |
//! | `PATCH` | `/vm`                    | `Paused` / `Resumed`             |
//! | `PUT`   | `/snapshot/create`       | take a Full or Diff snapshot     |
//! | `PUT`   | `/snapshot/load`         | resume from snapshot (with UFFD) |
//!
//! Implementation will use `hyper` over a `tokio::net::UnixStream`
//! (the `hyperlocal` crate is the canonical wrapper). We avoid pulling
//! it in at stub stage so the workspace doesn't carry an unused dep.

use std::path::{Path, PathBuf};

use engram_core::SandboxError;
use serde::Serialize;

/// Client bound to one Firecracker control socket. Cheap to clone —
/// the actual HTTP connection is established per-request.
#[derive(Clone, Debug)]
pub struct FirecrackerClient {
    socket: PathBuf,
}

impl FirecrackerClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Pause the VM, write a Full snapshot to `dir`, then resume.
    /// Phase 2 wires this up.
    pub async fn create_snapshot(&self, _dir: &Path) -> Result<SnapshotPaths, SandboxError> {
        Err(SandboxError::Snapshot(
            "FirecrackerClient::create_snapshot not yet implemented \
             (target: PATCH /vm Paused + PUT /snapshot/create + PATCH /vm Resumed)"
                .into(),
        ))
    }

    /// Restore from `paths` with UFFD-backed memory. Returns once the
    /// guest is paused and the UFFD handler is registered; pages are
    /// streamed in lazily as the guest faults on them.
    pub async fn load_snapshot(&self, _paths: &SnapshotPaths) -> Result<(), SandboxError> {
        Err(SandboxError::Snapshot(
            "FirecrackerClient::load_snapshot not yet implemented \
             (target: PUT /snapshot/load with backend_type=Uffd)"
                .into(),
        ))
    }

    pub async fn resume(&self) -> Result<(), SandboxError> {
        Err(SandboxError::Snapshot(
            "FirecrackerClient::resume not yet implemented (target: PATCH /vm Resumed)".into(),
        ))
    }
}

/// Pair of files Firecracker writes for a Full snapshot. `state_path`
/// is small (KBs); `mem_path` is the size of the guest's RAM.
#[derive(Clone, Debug)]
pub struct SnapshotPaths {
    pub state_path: PathBuf,
    pub mem_path: PathBuf,
}

// Wire types kept here so the schema is documented in code; the real
// implementation will populate these and POST them.
//
// The `#[allow(dead_code)]` is removed when Phase 2 starts using them.

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct MachineConfig {
    vcpu_count: u8,
    mem_size_mib: u32,
    smt: bool,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct BootSource<'a> {
    kernel_image_path: &'a str,
    boot_args: &'a str,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct DriveConfig<'a> {
    drive_id: &'a str,
    path_on_host: &'a str,
    is_root_device: bool,
    is_read_only: bool,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct VsockConfig<'a> {
    guest_cid: u32,
    uds_path: &'a str,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct VmStatePatch<'a> {
    state: &'a str, // "Paused" | "Resumed"
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct SnapshotCreateBody<'a> {
    snapshot_path: &'a str,
    mem_file_path: &'a str,
    snapshot_type: &'a str, // "Full" | "Diff"
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct SnapshotLoadBody<'a> {
    snapshot_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    mem_backend: Option<MemBackend<'a>>,
    enable_diff_snapshots: bool,
    resume_vm: bool,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct MemBackend<'a> {
    backend_type: &'a str, // "File" | "Uffd"
    backend_path: &'a str,
}
