//! Production [`SandboxBackend`] driving [Firecracker][firecracker]
//! microVMs over its HTTP-over-Unix-socket API.
//!
//! [firecracker]: https://github.com/firecracker-microvm/firecracker
//!
//! # Status
//!
//! `create`, `destroy`, `list`, `snapshot`, and `restore` are wired
//! end-to-end and verified by integration tests
//! (`tests/lifecycle.rs`, `tests/snapshot.rs`). Restore is currently
//! file-backed (synchronous `mem_file_path` read on load); UFFD-backed
//! restore — the load-bearing perf optimisation for sub-100ms resume —
//! lands in its own slice once the userfaultfd handler crate exists.
//! `exec_stream` still requires the in-guest agent + vsock plumbing.
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
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};

pub mod client;

pub use client::{
    ActionType, BootSource, DriveConfig, FirecrackerClient, MachineConfig, SnapshotPaths, VmState,
    VsockConfig,
};

/// Per-sandbox state owned by the host agent: the spec it was launched
/// with, the path to its Firecracker control socket, and the path to
/// the rootfs.ext4 image that was attached. `vsock_cid` is the context
/// ID assigned to the guest's virtio-vsock device; `engram-agentd`
/// inside the guest listens on `(cid, ENGRAM_AGENTD_PORT)`. Reserved
/// for the next slice — no vsock is configured today.
#[derive(Clone, Debug)]
pub struct SandboxState {
    pub spec: SandboxSpec,
    pub firecracker_socket: PathBuf,
    pub rootfs_path: PathBuf,
    pub vsock_cid: u32,
}

/// Reserved vsock port `engram-agentd` listens on inside the guest.
pub const ENGRAM_AGENTD_PORT: u32 = 1024;

/// Host-wide knobs for `FirecrackerBackend`. The kernel image lives
/// here rather than on `SandboxSpec` because it's tied to the host
/// kernel ABI, not to a specific image — every sandbox on this host
/// boots the same vmlinux.
#[derive(Clone, Debug)]
pub struct FirecrackerConfig {
    /// Absolute path to a kernel image (vmlinux) Firecracker can boot.
    pub kernel_image_path: PathBuf,
    /// Boot args appended to the kernel command line. The defaults
    /// (`console=ttyS0 reboot=k panic=1 pci=off`) are the standard
    /// "no PCI bus, triple-fault reboot, panic immediately" combo for
    /// micro-VMs.
    pub default_boot_args: String,
    /// `firecracker` binary on PATH or absolute path. Override for
    /// testing or when a different build is required.
    pub firecracker_bin: PathBuf,
}

impl FirecrackerConfig {
    /// Convenience constructor for production wiring: just the kernel
    /// path; everything else uses defaults.
    pub fn with_kernel(kernel_image_path: impl Into<PathBuf>) -> Self {
        Self {
            kernel_image_path: kernel_image_path.into(),
            default_boot_args: "console=ttyS0 reboot=k panic=1 pci=off".into(),
            firecracker_bin: PathBuf::from("firecracker"),
        }
    }
}

/// Live sandbox handle. Keeps the spawned firecracker `Child` so
/// `destroy` can SIGKILL it; `kill_on_drop` is a backstop in case a
/// `LiveSandbox` is dropped without going through `destroy` (panic).
struct LiveSandbox {
    state: SandboxState,
    child: Child,
}

/// Sidecar JSON file written next to `state.bin` and `memory.bin` to
/// carry fields Firecracker doesn't store itself but our trait surface
/// needs to reconstruct on restore — primarily the original
/// `SandboxSpec`. Not consumed by Firecracker; entirely ours.
///
/// We don't try to make this format stable across major versions
/// — snapshots have an implicit shelf life tied to a release.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FcSnapshotManifest {
    sandbox_id: SandboxId,
    created_at: DateTime<Utc>,
    spec: SandboxSpec,
}

pub struct FirecrackerBackend {
    work_dir: PathBuf,
    config: FirecrackerConfig,
    sandboxes: DashMap<SandboxId, LiveSandbox>,
}

impl FirecrackerBackend {
    pub fn new(work_dir: impl Into<PathBuf>, config: FirecrackerConfig) -> Self {
        Self {
            work_dir: work_dir.into(),
            config,
            sandboxes: DashMap::new(),
        }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn config(&self) -> &FirecrackerConfig {
        &self.config
    }

    /// Used by the host agent for inspection / heartbeat reporting.
    pub fn snapshot_state(&self, id: SandboxId) -> Option<SandboxState> {
        self.sandboxes.get(&id).map(|r| r.state.clone())
    }

    /// Allocate the jail dir, spawn `firecracker --api-sock <sock>`
    /// inside it, and wait until the socket is ready (or the process
    /// dies during startup). Common to `create_in_jail` and
    /// `restore_in_jail`. Returns the socket path and the live `Child`.
    /// On any error after this call, the caller drops the Child —
    /// `kill_on_drop=true` cleans up.
    async fn spawn_firecracker(&self, jail_dir: &Path) -> Result<(PathBuf, Child), SandboxError> {
        tokio::fs::create_dir_all(jail_dir)
            .await
            .map_err(|e| vm_err(format!("create jail dir {}: {e}", jail_dir.display())))?;

        let socket = jail_dir.join("firecracker.sock");
        // Firecracker refuses to start if the socket already exists.
        let _ = tokio::fs::remove_file(&socket).await;
        let log_path = jail_dir.join("firecracker.log");

        // stdout (serial console) + stderr (firecracker's own logs)
        // go to a per-sandbox log file so they're recoverable for
        // diagnostics without polluting the host-agent's stdout.
        let log = std::fs::File::create(&log_path)
            .map_err(|e| vm_err(format!("open log {}: {e}", log_path.display())))?;
        let log_clone = log
            .try_clone()
            .map_err(|e| vm_err(format!("dup log fd: {e}")))?;

        let mut child = Command::new(&self.config.firecracker_bin)
            .args(["--api-sock", socket.to_string_lossy().as_ref()])
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_clone))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                vm_err(format!(
                    "spawn {}: {e}",
                    self.config.firecracker_bin.display()
                ))
            })?;

        // Race the socket appearing against the process exiting. If
        // firecracker dies during startup, surface that with the log.
        if let Err(e) = wait_for_socket(&socket, Duration::from_secs(5), &mut child).await {
            let _ = child.kill().await;
            let log_tail = read_tail(&log_path, 4096).await.unwrap_or_default();
            return Err(vm_err(format!(
                "firecracker did not open API socket: {e}\n--- firecracker log ---\n{log_tail}"
            )));
        }

        Ok((socket, child))
    }

    /// Run the create lifecycle inside a per-sandbox jail dir. Split out
    /// so the outer `create` can do unconditional cleanup on failure.
    /// On any error, the spawned firecracker `Child` is dropped (and
    /// SIGKILL'd via `kill_on_drop`), and the jail dir is removed.
    async fn create_in_jail(
        &self,
        sandbox_id: SandboxId,
        jail_dir: &Path,
        spec: SandboxSpec,
    ) -> Result<(), SandboxError> {
        // Validate the spec carries a usable rootfs. Today we only
        // accept an ext4 image; a directory rootfs would need to be
        // packed into ext4 first (image-builder's Phase 2 work).
        let rootfs = spec.rootfs_source.clone().ok_or_else(|| {
            SandboxError::InvalidSpec(
                "FirecrackerBackend.create requires rootfs_source pointing at an ext4 image".into(),
            )
        })?;
        if rootfs.is_dir() {
            return Err(SandboxError::InvalidSpec(format!(
                "rootfs_source {} is a directory; FirecrackerBackend needs an ext4 image",
                rootfs.display()
            )));
        }
        if !rootfs.exists() {
            return Err(SandboxError::InvalidSpec(format!(
                "rootfs_source {} does not exist",
                rootfs.display()
            )));
        }
        if !self.config.kernel_image_path.exists() {
            return Err(SandboxError::InvalidSpec(format!(
                "kernel_image_path {} does not exist",
                self.config.kernel_image_path.display()
            )));
        }

        let (socket, child) = self.spawn_firecracker(jail_dir).await?;

        // Configure + start. Any failure here means the Child gets
        // dropped (and SIGKILL'd via kill_on_drop) by the outer
        // `create` cleanup, so we don't need explicit teardown.
        let api = FirecrackerClient::new(&socket);
        api.put_machine_config(&MachineConfig {
            vcpu_count: spec.cpu.vcpus.try_into().map_err(|_| {
                SandboxError::InvalidSpec(format!("cpu.vcpus {} doesn't fit in u8", spec.cpu.vcpus))
            })?,
            mem_size_mib: spec.memory.max_mib,
            smt: false,
        })
        .await?;
        api.put_boot_source(&BootSource {
            kernel_image_path: self.config.kernel_image_path.to_string_lossy().into_owned(),
            boot_args: self.config.default_boot_args.clone(),
            initrd_path: None,
        })
        .await?;
        api.put_drive(&DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: rootfs.to_string_lossy().into_owned(),
            is_root_device: true,
            // Read-write so a future in-guest agent can write workspace
            // state. Snapshots will pin this to read-only via overlay.
            is_read_only: false,
        })
        .await?;
        api.put_action(ActionType::InstanceStart).await?;

        // Vsock + network are deferred to the next slice; document the
        // shape we want by carrying vsock_cid in state even though we
        // don't configure /vsock yet.
        let state = SandboxState {
            spec,
            firecracker_socket: socket,
            rootfs_path: rootfs,
            vsock_cid: 0,
        };
        self.sandboxes
            .insert(sandbox_id, LiveSandbox { state, child });
        tracing::info!(%sandbox_id, jail = %jail_dir.display(), "firecracker microVM started");
        Ok(())
    }

    /// Stand up a fresh Firecracker process and load `snapshot_dir`'s
    /// `state.bin`/`memory.bin` into it. Symmetric with `create_in_jail`
    /// — same outer cleanup contract on error.
    async fn restore_in_jail(
        &self,
        sandbox_id: SandboxId,
        jail_dir: &Path,
        snapshot_dir: &Path,
        manifest: &FcSnapshotManifest,
    ) -> Result<(), SandboxError> {
        let state_path = snapshot_dir.join("state.bin");
        let mem_path = snapshot_dir.join("memory.bin");
        for (label, p) in [("state.bin", &state_path), ("memory.bin", &mem_path)] {
            if !p.exists() {
                return Err(SandboxError::Snapshot(format!(
                    "snapshot {label} missing at {}",
                    p.display()
                )));
            }
        }

        let (socket, child) = self.spawn_firecracker(jail_dir).await?;
        let api = FirecrackerClient::new(&socket);

        // resume_vm: true inside load_snapshot makes Firecracker start
        // executing the guest immediately — no separate PATCH /vm.
        api.load_snapshot(&SnapshotPaths {
            state_path: state_path.clone(),
            mem_path: mem_path.clone(),
        })
        .await?;

        // Carry the manifest's spec forward so SandboxState reflects
        // what the snapshot was taken from. rootfs_path mirrors what
        // the original VM had attached — Firecracker reopens that
        // path on load, so it must still be valid on disk.
        let rootfs_path = manifest.spec.rootfs_source.clone().unwrap_or_default();
        let state = SandboxState {
            spec: manifest.spec.clone(),
            firecracker_socket: socket,
            rootfs_path,
            vsock_cid: 0,
        };
        self.sandboxes
            .insert(sandbox_id, LiveSandbox { state, child });
        tracing::info!(
            %sandbox_id,
            jail = %jail_dir.display(),
            from = %snapshot_dir.display(),
            "firecracker microVM restored from snapshot",
        );
        Ok(())
    }
}

fn unimplemented(method: &'static str, endpoint: &'static str) -> SandboxError {
    SandboxError::Vm(
        format!("FirecrackerBackend::{method} not yet implemented (target: {endpoint})").into(),
    )
}

fn vm_err(msg: impl Into<String>) -> SandboxError {
    SandboxError::Vm(msg.into().into())
}

/// Wait until either:
///   - the API socket appears (success), OR
///   - the firecracker process exits (failure — surface its exit
///     status), OR
///   - `budget` elapses (timeout).
async fn wait_for_socket(path: &Path, budget: Duration, child: &mut Child) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if path.exists() {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("firecracker exited early with status {status}"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "socket {} did not appear within {:?}",
                path.display(),
                budget
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Read up to `max` bytes from the END of `path`. Used to surface a
/// firecracker log tail in error messages without spamming on success.
async fn read_tail(path: &Path, max: u64) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut f = tokio::fs::File::open(path).await.ok()?;
    let len = f.metadata().await.ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).await.ok()?;
    }
    let mut buf = String::new();
    f.read_to_string(&mut buf).await.ok()?;
    Some(buf)
}

#[async_trait]
impl SandboxBackend for FirecrackerBackend {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        let sandbox_id = SandboxId::new();
        let jail_dir = self.work_dir.join(sandbox_id.to_string());

        match self.create_in_jail(sandbox_id, &jail_dir, spec).await {
            Ok(()) => Ok(sandbox_id),
            Err(e) => {
                // Best-effort cleanup so a failed create doesn't leave
                // dangling jail dirs and (via kill_on_drop) processes.
                let _ = tokio::fs::remove_dir_all(&jail_dir).await;
                Err(e)
            }
        }
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

    async fn snapshot(&self, id: SandboxId, dest: &Path) -> Result<SnapshotMetadata, SandboxError> {
        // Read sandbox state under the dashmap guard, drop guard before
        // any await so we don't hold the read lock across an HTTP call.
        let (socket, spec) = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            (
                live.state.firecracker_socket.clone(),
                live.state.spec.clone(),
            )
        };

        tokio::fs::create_dir_all(dest).await.map_err(|e| {
            SandboxError::Snapshot(format!("create snapshot dir {}: {e}", dest.display()))
        })?;

        // pause → PUT /snapshot/create → resume happens inside the
        // client; a failure mid-sequence still tries to resume the
        // VM rather than leaving it stuck Paused.
        let api = FirecrackerClient::new(&socket);
        let paths = api.create_snapshot(dest).await?;

        let created_at = Utc::now();
        let manifest = FcSnapshotManifest {
            sandbox_id: id,
            created_at,
            spec: spec.clone(),
        };
        let manifest_path = dest.join("manifest.json");
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| SandboxError::Snapshot(format!("manifest serialize: {e}")))?;
        tokio::fs::write(&manifest_path, manifest_bytes)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("write manifest: {e}")))?;

        // Sum the three artefacts so callers know the size_bytes that
        // landed in `dest`. memory.bin dominates (= guest RAM size).
        let mut size_bytes = 0u64;
        for p in [&paths.state_path, &paths.mem_path, &manifest_path] {
            size_bytes += tokio::fs::metadata(p)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("stat {}: {e}", p.display())))?
                .len();
        }

        Ok(SnapshotMetadata {
            id: SnapshotId::new(),
            size_bytes,
            created_at,
            image_version: spec.image,
        })
    }

    async fn restore(&self, src: PathBuf) -> Result<SandboxId, SandboxError> {
        let manifest_bytes = tokio::fs::read(src.join("manifest.json"))
            .await
            .map_err(|e| SandboxError::Snapshot(format!("read manifest: {e}")))?;
        let manifest: FcSnapshotManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| SandboxError::Snapshot(format!("manifest parse: {e}")))?;

        // Always allocate a *fresh* sandbox id — same on-disk state,
        // different lifecycle handle.
        let sandbox_id = SandboxId::new();
        let jail_dir = self.work_dir.join(sandbox_id.to_string());

        match self
            .restore_in_jail(sandbox_id, &jail_dir, &src, &manifest)
            .await
        {
            Ok(()) => Ok(sandbox_id),
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&jail_dir).await;
                Err(e)
            }
        }
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // Idempotent: removing an unknown id is a no-op, matching the
        // contract ProcessBackend follows. SIGKILL is fine for now —
        // graceful shutdown via SendCtrlAltDel + timeout is a Phase 3
        // refinement once the in-guest agent can ack the shutdown.
        let Some((_, mut live)) = self.sandboxes.remove(&id) else {
            return Ok(());
        };
        if let Err(e) = live.child.kill().await {
            tracing::warn!(sandbox_id = %id, error = %e, "firecracker kill failed");
        }
        let _ = live.child.wait().await;

        let jail_dir = self.work_dir.join(id.to_string());
        if let Err(e) = tokio::fs::remove_dir_all(&jail_dir).await {
            // Don't fail destroy on a stale dir — the VM is gone, which
            // is what mattered. Log and move on.
            tracing::warn!(
                sandbox_id = %id,
                jail = %jail_dir.display(),
                error = %e,
                "removing jail dir failed; leaving for ops cleanup",
            );
        }
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

    /// Default backend for negative-path unit tests: kernel/firecracker
    /// paths point at non-existent files so `create()` fails early at
    /// spec validation rather than trying to spawn anything. The
    /// integration test in `tests/lifecycle.rs` exercises the real path.
    fn backend() -> (FirecrackerBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FirecrackerConfig {
            kernel_image_path: dir.path().join("nonexistent-vmlinux"),
            default_boot_args: "console=ttyS0".into(),
            firecracker_bin: PathBuf::from("/nonexistent/firecracker"),
        };
        (FirecrackerBackend::new(dir.path(), cfg), dir)
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

    #[tokio::test]
    async fn create_rejects_spec_without_rootfs_source() {
        // SandboxSpec.rootfs_source is Optional in the type but
        // mandatory for FirecrackerBackend — we need a block device to
        // attach as the root drive. A clear InvalidSpec at the surface
        // is much more useful than a Firecracker 400 from a deeper
        // call.
        let (b, _d) = backend();
        match b.create(spec()).await {
            Err(SandboxError::InvalidSpec(msg)) => {
                assert!(msg.contains("rootfs_source"), "got: {msg}");
            }
            other => panic!("expected InvalidSpec, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_rejects_spec_with_directory_rootfs() {
        // ProcessBackend can copy a directory rootfs; FirecrackerBackend
        // can't — the kernel mounts a block device at /. Loud failure
        // beats a confusing kernel panic on boot.
        let (b, dir) = backend();
        let mut s = spec();
        s.rootfs_source = Some(dir.path().to_path_buf());
        match b.create(s).await {
            Err(SandboxError::InvalidSpec(msg)) => {
                assert!(msg.contains("directory"), "got: {msg}");
            }
            other => panic!("expected InvalidSpec, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_rejects_spec_with_missing_rootfs_file() {
        let (b, dir) = backend();
        let mut s = spec();
        s.rootfs_source = Some(dir.path().join("does-not-exist.ext4"));
        match b.create(s).await {
            Err(SandboxError::InvalidSpec(msg)) => {
                assert!(msg.contains("does not exist"), "got: {msg}");
            }
            other => panic!("expected InvalidSpec, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn snapshot_unknown_id_is_not_found() {
        // Same contract ProcessBackend follows. Surfaces as NotFound
        // (not a Firecracker API error) because we never spawned a
        // VM to talk to.
        let (b, _d) = backend();
        match b.snapshot(SandboxId::new(), Path::new("/tmp/x")).await {
            Err(SandboxError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn restore_with_missing_manifest_errors_cleanly() {
        // Restoring from a non-existent dir shouldn't try to spawn
        // firecracker — manifest read is the first step and it should
        // fail loudly with a Snapshot error.
        let (b, _d) = backend();
        match b.restore(PathBuf::from("/nonexistent/snapshot/dir")).await {
            Err(SandboxError::Snapshot(msg)) => {
                assert!(
                    msg.contains("manifest"),
                    "error should reference the missing manifest: {msg}",
                );
            }
            other => panic!("expected Snapshot error, got {other:?}"),
        }
    }
}
