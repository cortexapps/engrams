//! macOS-only implementation. The non-macOS shell in `lib.rs` shadows
//! these types so the workspace compiles everywhere.
//!
//! This module is intentionally a near-empty skeleton at the moment:
//! the `SandboxBackend` impl returns structured "not yet implemented"
//! errors so the type can be wired into the coordinator's runtime
//! `match` cleanly. Each method's real body lands in a follow-up
//! commit (see plan tasks 27 / 28 / 29).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::sandbox::{HarnessSink, SandboxBackend};
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use parking_lot::Mutex;

use crate::vm::{VmConfig, VzVm};
use crate::vsock_bridge::VsockBridge;

/// Static config for the VZ backend — values that are the same for
/// every sandbox the backend creates. Per-sandbox overrides ride on
/// `SandboxSpec` (cpu/memory/disk).
#[derive(Clone, Debug)]
pub struct VzConfig {
    /// Path to the arm64 Linux kernel image VZ will boot. Must have
    /// `CONFIG_VIRTIO_VSOCK=y`, `CONFIG_VIRTIO_BLK=y`,
    /// `CONFIG_VIRTIO_NET=y`, `CONFIG_VIRTIO_CONSOLE=y`. Cached at
    /// `~/.cache/engram-vz-test/vmlinuz-arm64` by default.
    pub kernel_path: PathBuf,
    /// Default RAM in MiB applied when `SandboxSpec::memory.max_mib`
    /// is zero or unset. VZ minimum is 128 MiB.
    pub default_memory_mib: u32,
    /// Default vCPU count applied when `SandboxSpec::cpu.vcpus` is
    /// zero or unset.
    pub default_vcpus: u32,
}

impl VzConfig {
    pub fn with_kernel(kernel_path: impl Into<PathBuf>) -> Self {
        Self {
            kernel_path: kernel_path.into(),
            default_memory_mib: 512,
            default_vcpus: 1,
        }
    }
}

/// Per-sandbox state owned by `VzBackend`.
struct VzSandboxState {
    #[allow(dead_code)]
    spec: SandboxSpec,
    /// Live VM. Dropping this releases the underlying ObjC objects
    /// (config, devices, queue) once any in-flight dispatched work
    /// completes.
    vm: Arc<VzVm>,
    /// vsock-as-UDS bridge tasks. Held in a Mutex so `destroy` can
    /// take it out and call its async `stop`. None after stop.
    bridge: parking_lot::Mutex<Option<VsockBridge>>,
    /// `<work_dir>/<sandbox_id>.vsock` — base path. The vsock bridge
    /// binds `_1024`, `_1025` UDS listeners next to it. Stored on
    /// the state so future `start_agent` / `exec_stream` calls can
    /// look up the per-port paths without recomputing them.
    #[allow(dead_code)]
    vsock_uds_path: PathBuf,
}

pub struct VzBackend {
    work_dir: PathBuf,
    cfg: VzConfig,
    sandboxes: DashMap<SandboxId, VzSandboxState>,
    /// Latest harness sink (set by `set_harness_sink`). The vsock
    /// bridge passes guest-initiated 1026 connections to this sink
    /// in the same way `engram-sandbox-firecracker` does.
    harness_sink: Mutex<Option<HarnessSink>>,
}

impl VzBackend {
    pub fn new(
        work_dir: impl Into<PathBuf>,
        cfg: VzConfig,
    ) -> Result<Self, SandboxError> {
        let work_dir = work_dir.into();
        // Validate the kernel exists up-front so misconfiguration
        // surfaces at coord boot rather than at first session create.
        // Don't *open* it (VZ does that) — just stat.
        if !cfg.kernel_path.exists() {
            return Err(SandboxError::InvalidSpec(format!(
                "vz kernel image not found at {} (set ENGRAM_VZ_KERNEL_PATH or run \
                 `just vz-bake-kernel`)",
                cfg.kernel_path.display()
            )));
        }
        Ok(Self {
            work_dir,
            cfg,
            sandboxes: DashMap::new(),
            harness_sink: Mutex::new(None),
        })
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn config(&self) -> &VzConfig {
        &self.cfg
    }

    fn vsock_uds_path_for(&self, id: SandboxId) -> PathBuf {
        self.work_dir.join(format!("{id}.vsock"))
    }
}

// Helper for the methods that still aren't implemented (snapshot,
// restore, exec_stream, start_agent — each lands in tasks 28/29).
fn unimpl(method: &str) -> SandboxError {
    SandboxError::Vm(
        format!("VzBackend::{method} not yet implemented (see plan tasks 28/29)").into(),
    )
}

#[async_trait]
impl SandboxBackend for VzBackend {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        let rootfs = spec.rootfs_source.clone().ok_or_else(|| {
            SandboxError::InvalidSpec(
                "VzBackend requires SandboxSpec.rootfs_source — point it at the ext4 \
                 rootfs produced by `just vz-bake-claude`"
                    .into(),
            )
        })?;
        // Validate up front rather than letting VZ surface a less
        // specific NSError later.
        if !rootfs.exists() {
            return Err(SandboxError::InvalidSpec(format!(
                "vz rootfs not found at {} — bake an image with `just vz-bake-claude` and \
                 point SandboxSpec.rootfs_source at it",
                rootfs.display()
            )));
        }

        let memory_mib = if spec.memory.max_mib > 0 {
            spec.memory.max_mib
        } else {
            self.cfg.default_memory_mib
        };
        let vcpus = if spec.cpu.vcpus > 0 {
            spec.cpu.vcpus
        } else {
            self.cfg.default_vcpus
        };

        // Make sure the work_dir exists so the per-sandbox UDS
        // base path has somewhere to live (the vsock bridge in
        // task 28 binds listeners under it).
        tokio::fs::create_dir_all(&self.work_dir).await?;

        let vm_cfg = VmConfig::new(self.cfg.kernel_path.clone(), rootfs, memory_mib, vcpus);
        let vm = VzVm::new(vm_cfg)?;

        // Start the VM; if start fails, drop the VM via the early
        // return (no half-registered state in the sandboxes map).
        vm.start().await?;

        let id = SandboxId::new();
        let vsock_uds_path = self.vsock_uds_path_for(id);

        // Wire up the vsock UDS bridge. This binds <vsock_uds>_1024
        // and <vsock_uds>_1025 immediately so a subsequent
        // start_agent or exec_stream call can dial without racing
        // a not-yet-bound window. If a harness sink is registered,
        // it also installs the guest-listener for port 1026.
        let harness_sink = self.harness_sink.lock().clone();
        let bridge = VsockBridge::start(
            vm.raw_clone(),
            vm.queue_clone(),
            vsock_uds_path.clone(),
            harness_sink,
        )
        .await
        .map_err(|e| {
            // If bridge bind fails (e.g. EADDRINUSE), tear down the
            // VM we just started so we don't leak it.
            tracing::error!(error = %e, sandbox_id = %id, "vz bridge start failed; tearing down VM");
            // Synchronous drop — vm.stop().await would be cleaner
            // but we're already in an error path; the queue will
            // drain on Retained drop.
            engram_core::SandboxError::from(e)
        })?;

        self.sandboxes.insert(
            id,
            VzSandboxState {
                spec,
                vm: Arc::new(vm),
                bridge: parking_lot::Mutex::new(Some(bridge)),
                vsock_uds_path,
            },
        );
        Ok(id)
    }

    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: AgentSpec,
    ) -> Result<(), SandboxError> {
        // Lands with the vsock UDS bridge in task 28 — sends
        // BootstrapLaunch over `<vsock_uds>_1025` to the in-VM
        // bootstrap supervisor, same wire as Firecracker.
        Err(unimpl("start_agent"))
    }

    fn set_harness_sink(&self, sink: HarnessSink) {
        *self.harness_sink.lock() = Some(sink);
    }

    async fn exec_stream(
        &self,
        _id: SandboxId,
        _cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        // Lands with the vsock UDS bridge in task 28 — dials
        // `<vsock_uds>_1024` for the engram-agentd handshake.
        Err(unimpl("exec_stream"))
    }

    async fn snapshot(
        &self,
        _id: SandboxId,
        _dest: &Path,
    ) -> Result<SnapshotMetadata, SandboxError> {
        Err(unimpl("snapshot"))
    }

    async fn restore(&self, _src: PathBuf) -> Result<SandboxId, SandboxError> {
        Err(unimpl("restore"))
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        let Some((_, state)) = self.sandboxes.remove(&id) else {
            return Err(SandboxError::NotFound);
        };
        // Tear down the bridge first — abort pump tasks, unregister
        // the guest-side listener, and remove the UDS files. Doing
        // this before vm.stop avoids a race where pumps see EOF on
        // their VZ-side fds and try to reach into a dying VM.
        // Take the bridge out of its mutex before awaiting so we
        // don't hold a non-Send guard across the await.
        let bridge = state.bridge.lock().take();
        if let Some(mut bridge) = bridge {
            bridge.stop(state.vm.queue()).await;
        }
        // Best-effort stop. If the VM is already stopped or in a
        // state that can't accept stop (e.g. failed-to-start),
        // VZ surfaces an NSError; we log and continue, since the
        // observable goal of `destroy` is "this sandbox is gone."
        if let Err(e) = state.vm.stop().await {
            tracing::warn!(error = %e, sandbox_id = %id, "vz stop returned an error; releasing handle anyway");
        }
        // `state` (and the Arc<VzVm> inside) drops here — releases
        // ObjC retains.
        drop(state);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.sandboxes.iter().map(|kv| *kv.key()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_missing_kernel_with_actionable_error() {
        let tmp = std::env::temp_dir();
        let cfg = VzConfig::with_kernel("/this/path/definitely/does/not/exist");
        let result = VzBackend::new(tmp, cfg);
        let Err(err) = result else {
            panic!("missing kernel must error");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("vz kernel image not found"),
            "error must name the missing file: got {msg}"
        );
        assert!(
            msg.contains("ENGRAM_VZ_KERNEL_PATH") || msg.contains("vz-bake-kernel"),
            "error must point at the fix: got {msg}"
        );
    }

    #[test]
    fn vsock_uds_path_uses_sandbox_id_under_work_dir() {
        // Path construction doesn't need a real kernel; we only need
        // the existence check in `new` to pass, so a tempfile is fine.
        let kernel = tempfile::NamedTempFile::new().unwrap();
        let cfg = VzConfig::with_kernel(kernel.path());
        let work = std::env::temp_dir().join("engram-vz-test-paths");
        std::fs::create_dir_all(&work).unwrap();
        let backend = match VzBackend::new(&work, cfg) {
            Ok(b) => b,
            Err(e) => panic!("backend construction failed: {e}"),
        };
        let sid = SandboxId::new();
        let path = backend.vsock_uds_path_for(sid);
        assert_eq!(path.parent().unwrap(), work);
        assert!(path.file_name().unwrap().to_string_lossy().ends_with(".vsock"));
    }
}
