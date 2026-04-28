//! Production [`SandboxBackend`] driving [Firecracker][firecracker]
//! microVMs over its HTTP-over-Unix-socket API.
//!
//! [firecracker]: https://github.com/firecracker-microvm/firecracker
//!
//! # Status
//!
//! **Stub.** Every method returns a typed error pointing at the
//! Firecracker API endpoint that's still to be wired. The crate
//! compiles, exposes the production trait surface, and is the target
//! of the Phase 2 implementation work; it does not yet boot VMs.
//!
//! # Architecture
//!
//! Each sandbox owns a Firecracker process and a Unix socket on which
//! Firecracker accepts an HTTP-shaped control plane:
//!
//! ```text
//!   host-agent ──► /run/engram/<sandbox-id>/firecracker.sock ──► firecracker
//!     │                        │                                       │
//!     │ PUT /machine-config    │                                       │
//!     │ PUT /boot-source       │     (configure VM)                    │
//!     │ PUT /drives/rootfs     │                                       │
//!     │ PUT /network-interfaces/eth0                                   │
//!     │ PUT /actions {InstanceStart}                                   │
//!     ├────────────────────────┘                                       │
//!     │                                                                ▼
//!     │                                                       ┌──────────────┐
//!     │ vsock CID per VM, port 1024 reserved for engram-agentd│  guest VM   │
//!     ├──────────────────────────────────────────────────────►│ engram-agentd│
//!     │                                                       │ (in-guest   │
//!     │ PATCH /vm {state: Paused}      (snapshot)             │  exec daemon)│
//!     │ PUT /snapshot/create                                  └──────────────┘
//!     │ PATCH /vm {state: Resumed}
//!     │ PUT /snapshot/load             (restore — UFFD-backed)
//!     ▼
//!   per-snapshot: state.bin + memory.bin (or UFFD-backed memory)
//! ```
//!
//! Two non-obvious bits this implementation will need to carry:
//!
//! - **In-guest agent (`engram-agentd`)** — Firecracker has no "exec a
//!   command in a running guest" primitive. Our rootfs images include
//!   a small daemon that listens on vsock and proxies exec/stdin/stdout
//!   for the host agent. `SandboxBackend::exec` becomes "send a command
//!   over vsock and stream the response."
//! - **UFFD-backed restore** — `PUT /snapshot/load` configures
//!   `userfaultfd` on the guest's memory region. Resume returns
//!   immediately; pages stream in lazily on guest fault. This is the
//!   load-bearing economic of the snapshot-evict mechanic.
//!
//! Both are explicit non-goals of this stub — they land with the Phase
//! 2 implementation. See `DESIGN.md` for the full plan.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;

pub mod client;

pub use client::{
    ActionType, BootSource, DriveConfig, FirecrackerClient, MachineConfig, SnapshotPaths,
    VmState, VsockConfig,
};

/// Per-sandbox state owned by the host agent: the spec it was launched
/// with, the path to its Firecracker control socket, and the path to
/// the rootfs.ext4 image that was attached. `vsock_cid` is the context
/// ID assigned to the guest's virtio-vsock device; `engram-agentd`
/// inside the guest listens on `(cid, ENGRAM_AGENTD_PORT)`.
#[derive(Clone, Debug)]
pub struct SandboxState {
    pub spec: SandboxSpec,
    pub firecracker_socket: PathBuf,
    pub rootfs_path: PathBuf,
    pub vsock_cid: u32,
}

/// Reserved vsock port `engram-agentd` listens on inside the guest.
pub const ENGRAM_AGENTD_PORT: u32 = 1024;

pub struct FirecrackerBackend {
    work_dir: PathBuf,
    sandboxes: DashMap<SandboxId, SandboxState>,
}

impl FirecrackerBackend {
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self {
            work_dir: work_dir.into(),
            sandboxes: DashMap::new(),
        }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Used by the host agent for inspection / heartbeat reporting.
    pub fn snapshot_state(&self, id: SandboxId) -> Option<SandboxState> {
        self.sandboxes.get(&id).map(|r| r.clone())
    }
}

fn unimplemented(method: &'static str, endpoint: &'static str) -> SandboxError {
    SandboxError::Vm(
        format!("FirecrackerBackend::{method} not yet implemented (target: {endpoint})").into(),
    )
}

#[async_trait]
impl SandboxBackend for FirecrackerBackend {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        // TODO(phase-2):
        //   1. Allocate sandbox_id, jail dir under self.work_dir.
        //   2. Spawn `firecracker-jailer` with the socket path inside
        //      the jail.
        //   3. Configure the VM through the Firecracker API:
        //        PUT /machine-config { vcpu_count, mem_size_mib, smt }
        //        PUT /boot-source    { kernel_image_path, boot_args }
        //        PUT /drives/rootfs  { path_on_host, is_root_device }
        //        PUT /network-interfaces/eth0 { host_dev_name (TAP), ... }
        //        PUT /vsock          { guest_cid }
        //   4. PUT /actions { action_type: InstanceStart }
        //   5. Wait for engram-agentd to report ready over vsock.
        //   6. Insert SandboxState into self.sandboxes.
        Err(unimplemented(
            "create",
            "PUT /machine-config + InstanceStart",
        ))
    }

    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        // TODO(phase-2): connect to (vsock_cid, ENGRAM_AGENTD_PORT),
        // send the ExecRequest, stream stdout/stderr/exit_status back
        // as ExecEvents. This is *not* a Firecracker API call — it's a
        // vsock RPC to engram-agentd inside the guest.
        Err(unimplemented("exec_stream", "vsock to engram-agentd"))
    }

    async fn snapshot(
        &self,
        _id: SandboxId,
        _dest: &Path,
    ) -> Result<SnapshotMetadata, SandboxError> {
        // TODO(phase-2):
        //   1. PATCH /vm { state: "Paused" }
        //   2. PUT /snapshot/create {
        //          snapshot_path: dest/state.bin,
        //          mem_file_path: dest/memory.bin,
        //          snapshot_type: "Full" | "Diff"
        //      }
        //   3. PATCH /vm { state: "Resumed" }   (snapshot keeps VM live)
        // Diff snapshots use KVM_GET_DIRTY_LOG; we'll start with Full
        // and add Diff once the eviction policy needs it.
        Err(unimplemented(
            "snapshot",
            "PATCH /vm Paused + PUT /snapshot/create",
        ))
    }

    async fn restore(&self, _src: PathBuf) -> Result<SandboxId, SandboxError> {
        // TODO(phase-2):
        //   1. Spawn a fresh firecracker-jailer.
        //   2. PUT /snapshot/load {
        //          snapshot_path: src/state.bin,
        //          mem_backend: { backend_type: "Uffd",
        //                         backend_path: <our uffd handler socket> },
        //          enable_diff_snapshots: false,
        //          resume_vm: true
        //      }
        //   3. The handler at `backend_path` pages in 4KiB at a time
        //      from `src/memory.bin` (or BlobStorage) on guest fault.
        //      This is what gives sub-100ms resume.
        Err(unimplemented(
            "restore",
            "PUT /snapshot/load with UFFD memory backend",
        ))
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // TODO(phase-2):
        //   1. PUT /actions { action_type: "SendCtrlAltDel" } (graceful)
        //   2. After timeout, SIGKILL the firecracker-jailer process.
        //   3. Tear down TAP interface, jail directory.
        //   4. Remove from self.sandboxes.
        // For now we honour the test-friendly path so callers can
        // populate state manually if needed.
        self.sandboxes.remove(&id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.sandboxes.iter().map(|r| *r.key()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn backend() -> (FirecrackerBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (FirecrackerBackend::new(dir.path()), dir)
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 256 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
        }
    }

    /// The stub honours the "trait shape stays valid" contract — list
    /// of an empty backend returns an empty vec, not an error.
    #[tokio::test]
    async fn list_on_fresh_backend_is_empty() {
        let (b, _d) = backend();
        assert!(b.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn destroy_unknown_id_is_idempotent_in_stub() {
        // Same contract as ProcessBackend: destroying an unknown id
        // should not error. Tests that depend on this contract should
        // pass against either backend.
        let (b, _d) = backend();
        b.destroy(SandboxId::new()).await.unwrap();
    }

    /// `create` must surface a structured error pointing at the
    /// Firecracker endpoint that needs implementing. This locks down
    /// the stub's diagnostic so we don't accidentally silence it
    /// before the real wiring lands.
    #[tokio::test]
    async fn create_returns_typed_stub_error_referencing_firecracker_api() {
        let (b, _d) = backend();
        match b.create(spec()).await {
            Err(SandboxError::Vm(e)) => {
                let msg = e.to_string();
                assert!(msg.contains("create"), "error names the failing method");
                assert!(
                    msg.contains("/machine-config") || msg.contains("InstanceStart"),
                    "error names the Firecracker endpoint we'd hit",
                );
            }
            other => panic!("expected Vm error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn snapshot_returns_typed_stub_error_referencing_firecracker_api() {
        let (b, _d) = backend();
        match b.snapshot(SandboxId::new(), Path::new("/tmp/x")).await {
            Err(SandboxError::Vm(e)) => {
                let msg = e.to_string();
                assert!(msg.contains("snapshot"));
                assert!(
                    msg.contains("/snapshot/create") || msg.contains("Paused"),
                    "error names the Firecracker snapshot endpoint",
                );
            }
            other => panic!("expected Vm error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn restore_error_references_uffd() {
        let (b, _d) = backend();
        match b.restore(PathBuf::from("/tmp/x")).await {
            Err(SandboxError::Vm(e)) => {
                let msg = e.to_string();
                assert!(
                    msg.to_lowercase().contains("uffd")
                        || msg.contains("/snapshot/load"),
                    "stub must point at the UFFD restore path so the next implementer knows where to wire",
                );
            }
            other => panic!("expected Vm error, got {other:?}"),
        }
    }
}
