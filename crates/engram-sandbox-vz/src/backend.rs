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

/// Per-sandbox state owned by `VzBackend`. The actual `VZVirtualMachine`
/// pointer goes here once task 27 lands; until then this carries the
/// spec + UDS paths so other lifecycle calls have a place to look.
#[allow(dead_code)]
struct VzSandboxState {
    spec: SandboxSpec,
    /// `<work_dir>/<sandbox_id>.vsock` — base path. The vsock UDS
    /// bridge (task 28) binds `_1024`, `_1025`, `_1026` listeners
    /// next to it.
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

    // Used by `create()` (task 27) and a unit test today. The
    // `allow(dead_code)` lasts until task 27 wires up the real
    // create path.
    #[allow(dead_code)]
    fn vsock_uds_path_for(&self, id: SandboxId) -> PathBuf {
        self.work_dir.join(format!("{id}.vsock"))
    }
}

// Helper to keep the "not yet implemented" message short and
// uniform. Each method body lands in its target task; once they all
// land this helper goes away.
fn unimpl(method: &str) -> SandboxError {
    SandboxError::Vm(
        format!(
            "VzBackend::{method} not yet implemented (see plan tasks 27/28/29)"
        )
        .into(),
    )
}

#[async_trait]
impl SandboxBackend for VzBackend {
    async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        Err(unimpl("create"))
    }

    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: AgentSpec,
    ) -> Result<(), SandboxError> {
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

    async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Err(unimpl("destroy"))
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.sandboxes.iter().map(|kv| *kv.key()).collect())
    }
}

// Quiet `clippy::needless_pass_by_ref` and similar on the empty
// impls without disabling the lints workspace-wide.
#[allow(dead_code)]
fn _force_arc_use_so_the_skeleton_compiles_with_set_harness_sink(_: &Arc<()>) {}

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
