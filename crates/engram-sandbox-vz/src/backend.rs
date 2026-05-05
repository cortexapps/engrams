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
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use engram_agentd::{
    read_msg, write_msg, WireExecEvent, WireExecRequest, WireRequest, WireResponse,
};
use engram_core::traits::sandbox::{HarnessSink, SandboxBackend};
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{AgentSpec, ExecEvent, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::console_bridge::{port_uds_path, ConsoleBridge};
use crate::disk::{clone_or_copy, per_sandbox_rootfs_path, SNAPSHOT_ROOTFS_FILENAME};
use crate::vm::{VmConfig, VzVm};

/// Vsock port engram-agentd binds inside the rootfs. Same number FC
/// uses; the in-VM binary doesn't know which VMM is hosting it.
const ENGRAM_AGENTD_PORT: u32 = 1024;

/// Static config for the VZ backend — values that are the same for
/// every sandbox the backend creates. Per-sandbox overrides ride on
/// `SandboxSpec` (cpu/memory/disk).
#[derive(Clone, Debug)]
pub struct VzConfig {
    /// Path to the arm64 Linux kernel image VZ will boot. Must have
    /// `CONFIG_VIRTIO_BLK=y`, `CONFIG_VIRTIO_NET=y`,
    /// `CONFIG_VIRTIO_CONSOLE=y`. Cached at
    /// `~/.cache/engram-vz-test/vmlinux-arm64` by default. The
    /// canonical source is `just vz-pull-kernel`, which fetches
    /// the Kata Containers static kernel.
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
    bridge: parking_lot::Mutex<Option<ConsoleBridge>>,
    /// `<work_dir>/<sandbox_id>.vsock` — base path. The vsock bridge
    /// binds `_1024`, `_1025` UDS listeners next to it. Stored on
    /// the state so future `start_agent` / `exec_stream` calls can
    /// look up the per-port paths without recomputing them.
    #[allow(dead_code)]
    vsock_uds_path: PathBuf,
    /// Per-sandbox APFS clone of the bake (or snapshot) rootfs.
    /// Created at `create()` / `restore()` time and removed at
    /// `destroy()`. The clone is what VZ actually attaches; the
    /// originating bake / snapshot file stays intact.
    rootfs_path: PathBuf,
    /// Cached IPv4 address discovered by querying agentd over the
    /// existing vsock-bridge transport. Populated on first
    /// `guest_ip` call (the agent's eth0 takes a moment to come up
    /// after IP_PNP DHCP, so we don't try at create time). Used by
    /// the coordinator's `GET /sessions/:id/shell` proxy to dial
    /// `ttyd` running inside the guest.
    guest_ip: Mutex<Option<String>>,
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
    pub fn new(work_dir: impl Into<PathBuf>, cfg: VzConfig) -> Result<Self, SandboxError> {
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

/// Adapter that runs the engram-agentd wire protocol against a
/// pre-connected byte stream. Mirrors the FC backend's
/// `drive_exec_protocol` — the protocol is VMM-independent (it's
/// defined in `engram-agentd`'s wire types), so the implementation
/// is identical bar the connection setup. Future refactor:
/// hoist this into a shared crate.
async fn drive_exec_protocol<R, W>(
    sandbox_id: SandboxId,
    mut reader: R,
    mut writer: W,
    cmd: ExecRequest,
) -> Result<ExecStream, SandboxError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let req = WireRequest::Exec(WireExecRequest {
        command: cmd.command,
        stdin: cmd.stdin,
        env: cmd.env,
        workdir: cmd.workdir,
        timeout_ms: cmd.timeout.map(|d| d.as_millis() as u64),
    });
    write_msg(&mut writer, &req)
        .await
        .map_err(|e| SandboxError::Vm(format!("send WireRequest::Exec: {e}").into()))?;

    let exec_id = format!("vz-{}", uuid::Uuid::new_v4().simple());
    let (tx, rx) = mpsc::channel::<ExecEvent>(64);
    tokio::spawn(async move {
        loop {
            match read_msg::<_, WireExecEvent>(&mut reader).await {
                Ok(WireExecEvent::Stdout(b)) => {
                    if tx.send(ExecEvent::Stdout(Bytes::from(b))).await.is_err() {
                        return;
                    }
                }
                Ok(WireExecEvent::Stderr(b)) => {
                    if tx.send(ExecEvent::Stderr(Bytes::from(b))).await.is_err() {
                        return;
                    }
                }
                Ok(WireExecEvent::Exit(code)) => {
                    let _ = tx.send(ExecEvent::Exit(code)).await;
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "vz agent connection ended without explicit Exit",
                    );
                    let _ = tx.send(ExecEvent::Exit(None)).await;
                    return;
                }
            }
        }
    });
    Ok(ExecStream {
        sandbox_id,
        exec_id,
        events: Box::pin(ReceiverStream::new(rx)),
    })
}

/// Logged at-most-once per backend instance when a session asks for
/// egress filtering VZ can't enforce. Apple's
/// `VZNATNetworkDeviceAttachment` is opaque: the host shares its
/// networking stack with the guest with no insertable filter, so a
/// non-empty `manifest.network.allow_hosts` is unenforceable here.
/// Production isolation lives on FC; VZ stays "open egress, warn".
fn warn_vz_ignores_allow_hosts_once(network: &engram_core::types::NetworkPolicy) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    let restrictive = matches!(network.default, engram_core::types::NetworkDefault::Deny)
        && !network.allow_hosts.is_empty();
    if restrictive && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "VZ does not enforce manifest.network.allow_hosts; macOS's NAT path is \
             opaque. Sessions on this backend get open egress. Use the Firecracker \
             backend on Linux for production hard-isolation networking."
        );
    }
}

#[async_trait]
impl SandboxBackend for VzBackend {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        warn_vz_ignores_allow_hosts_once(&spec.network);
        let bake_rootfs = spec.rootfs_source.clone().ok_or_else(|| {
            SandboxError::InvalidSpec(
                "VzBackend requires SandboxSpec.rootfs_source — point it at the ext4 \
                 rootfs produced by `just vz-bake-claude`"
                    .into(),
            )
        })?;
        // Validate up front rather than letting VZ surface a less
        // specific NSError later.
        if !bake_rootfs.exists() {
            return Err(SandboxError::InvalidSpec(format!(
                "vz rootfs not found at {} — bake an image with `just vz-bake-claude` and \
                 point SandboxSpec.rootfs_source at it",
                bake_rootfs.display()
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
        // base path + per-sandbox rootfs clone have somewhere to
        // live.
        tokio::fs::create_dir_all(&self.work_dir).await?;

        // Allocate the sandbox id up front so we can name the
        // per-sandbox rootfs deterministically before VM init.
        let id = SandboxId::new();
        let rootfs_path = per_sandbox_rootfs_path(&self.work_dir, id);

        // APFS-clone the bake image into a per-sandbox rootfs.
        // The clone is COW-backed: ~50 ms even for a 1.7 GB ext4,
        // and the diverged blocks (whatever this sandbox writes)
        // are the only ones that consume real disk. This isolates
        // the sandbox's filesystem from concurrent sandboxes
        // sharing the same image — the previous design had every
        // VM attaching the same writable ext4, which would race
        // and corrupt under concurrency. Also makes clone-based
        // snapshots possible later (snapshot dir holds another
        // clone of this file at evict time).
        clone_or_copy(&bake_rootfs, &rootfs_path).await?;
        tracing::debug!(
            sandbox_id = %id,
            src = %bake_rootfs.display(),
            dst = %rootfs_path.display(),
            "vz: cloned bake rootfs to per-sandbox path"
        );

        let mut vm_cfg = VmConfig::new(
            self.cfg.kernel_path.clone(),
            rootfs_path.clone(),
            memory_mib,
            vcpus,
        );
        vm_cfg.harness_substrate = spec.harness_substrate.clone();
        let (vm, port_fds) = VzVm::new(vm_cfg)?;

        // Start the VM; if start fails, drop the VM via the early
        // return (no half-registered state in the sandboxes map).
        // Also clean up the per-sandbox rootfs we just cloned.
        if let Err(e) = vm.start().await {
            let _ = tokio::fs::remove_file(&rootfs_path).await;
            return Err(e.into());
        }

        let vsock_uds_path = self.vsock_uds_path_for(id);

        // Wire up the virtio-console UDS bridge. This binds
        // <vsock_uds>_1024 and <vsock_uds>_1025 immediately so a
        // subsequent start_agent or exec_stream call can dial
        // without racing a not-yet-bound window. If a harness sink
        // is registered, it also pipes port 1026's guest writes
        // straight to the sink.
        let harness_sink = self.harness_sink.lock().clone();
        let bridge = ConsoleBridge::start(
            vsock_uds_path.clone(),
            port_fds,
            harness_sink,
        )
        .await
        .map_err(|e| {
            tracing::error!(error = %e, sandbox_id = %id, "vz bridge start failed; tearing down VM");
            // Best-effort cleanup of the cloned rootfs on the
            // error path. The VM itself drops on the early
            // return.
            let path = rootfs_path.clone();
            tokio::spawn(async move { let _ = tokio::fs::remove_file(path).await; });
            engram_core::SandboxError::from(e)
        })?;

        self.sandboxes.insert(
            id,
            VzSandboxState {
                spec,
                vm: Arc::new(vm),
                bridge: parking_lot::Mutex::new(Some(bridge)),
                vsock_uds_path,
                rootfs_path,
                guest_ip: Mutex::new(None),
            },
        );
        Ok(id)
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        tracing::debug!(sandbox_id = %id, argv0 = %agent.argv.first().map(|s| s.as_str()).unwrap_or("<empty>"), "vz start_agent: dialing bootstrap UDS");
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vsock_uds_path.clone()
        };
        let bootstrap_uds =
            port_uds_path(&vsock_uds_path, engram_harness_proto::BOOTSTRAP_VSOCK_PORT);
        // The in-VM bootstrap supervisor takes a few seconds to
        // come up after VM boot — same race FC handles. Retry the
        // dial with backoff for ~15s before giving up. Bridge bind
        // already happened in `create`, so the UDS exists; what
        // can fail is the dial-through to the guest until bootstrap
        // accept()s on port 1025.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut backoff = Duration::from_millis(100);
        let mut conn = loop {
            match UnixStream::connect(&bootstrap_uds).await {
                Ok(c) => break c,
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(SandboxError::Vm(
                            format!("connect bootstrap UDS {}: {e}", bootstrap_uds.display())
                                .into(),
                        ));
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                }
            }
        };

        // Wait for bootstrap to write the readiness byte before
        // sending the launch. The UDS dial returns the moment the
        // host pump's UnixListener accepts (which happens at
        // VM-config time, well before the guest is even booted), so
        // a write at that point would race the guest port being
        // opened — on VZ's virtio-console path those early bytes get
        // dropped. Reading the marker first turns this into an
        // ordering guarantee: bootstrap accepted → wrote → we read,
        // so its read pump is definitely consuming.
        use tokio::io::AsyncReadExt;
        let mut marker = [0u8; 1];
        match tokio::time::timeout(Duration::from_secs(15), conn.read_exact(&mut marker)).await {
            Ok(Ok(_)) => {
                if marker[0] != engram_harness_proto::BOOTSTRAP_READY_BYTE {
                    tracing::warn!(
                        sandbox_id = %id,
                        got = marker[0],
                        "vz start_agent: unexpected bootstrap marker; proceeding anyway"
                    );
                }
                tracing::debug!(sandbox_id = %id, "vz start_agent: bootstrap ready");
            }
            Ok(Err(e)) => {
                return Err(SandboxError::Vm(
                    format!("read bootstrap ready marker: {e}").into(),
                ));
            }
            Err(_) => {
                return Err(SandboxError::Vm(
                    "timed out waiting for bootstrap ready marker (15s)".into(),
                ));
            }
        }

        let launch = engram_harness_proto::BootstrapLaunch {
            argv: agent.argv,
            env: agent.env.into_iter().collect(),
        };
        tracing::debug!(sandbox_id = %id, "vz start_agent: writing BootstrapLaunch frame");
        engram_harness_proto::write_msg(&mut conn, &launch)
            .await
            .map_err(|e| SandboxError::Vm(format!("write BootstrapLaunch: {e}").into()))?;
        // Best-effort flush; bootstrap closes its end after exec.
        let _ = conn.shutdown().await;
        tracing::debug!(sandbox_id = %id, "vz start_agent: BootstrapLaunch sent");
        Ok(())
    }

    fn set_harness_sink(&self, sink: HarnessSink) {
        *self.harness_sink.lock() = Some(sink);
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        // Same boot-race retry as start_agent. engram-agentd inside
        // the rootfs takes a couple of seconds to bind on vsock 1024
        // after kernel init.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut backoff = Duration::from_millis(50);
        let conn = loop {
            match UnixStream::connect(&agent_uds).await {
                Ok(c) => break c,
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(SandboxError::Vm(
                            format!("connect engram-agentd UDS {}: {e}", agent_uds.display())
                                .into(),
                        ));
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                }
            }
        };
        let (reader, writer) = tokio::io::split(conn);
        drive_exec_protocol(id, reader, writer, cmd).await
    }

    async fn snapshot(&self, id: SandboxId, dest: &Path) -> Result<SnapshotMetadata, SandboxError> {
        let (vm, spec, rootfs_path) = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            (live.vm.clone(), live.spec.clone(), live.rootfs_path.clone())
        };
        tokio::fs::create_dir_all(dest).await.map_err(|e| {
            SandboxError::Snapshot(format!("create snapshot dir {}: {e}", dest.display()))
        })?;

        // Clone-based snapshot semantics. VZ's
        // `restoreMachineStateFromURL` is broken upstream for
        // arm64 Linux guests on macOS (see UTM #6654, Apple
        // Developer Forum thread 745168, and Apple's own
        // `containerization` framework which avoids it entirely
        // — sub-second cold-boot is the canonical path). We
        // pause the VM (so the guest's page cache settles and
        // ext4's journal is consistent), APFS-clone the
        // per-sandbox rootfs into the snapshot dir, and resume.
        // The clone IS the snapshot — restore re-clones it back
        // to a fresh per-sandbox file and cold-boots a new VM.
        // engram-bootstrap's supervisor pattern + Claude's
        // `--resume <session-id>` (persisted on the rootfs at
        // /workspace/.engram/claude-session-id) recover
        // conversation continuity across the cold boot.
        vm.pause().await?;
        let snapshot_rootfs = dest.join(SNAPSHOT_ROOTFS_FILENAME);
        let clone_result = clone_or_copy(&rootfs_path, &snapshot_rootfs).await;
        let resume_result = vm.resume().await;
        clone_result.map_err(SandboxError::from)?;
        // If clone succeeded but resume failed, the VM is stuck
        // paused — surface the resume error so the caller can
        // retry. The snapshot is durable on disk regardless.
        resume_result?;

        // Manifest. We rewrite spec.rootfs_source to point at
        // the snapshot's clone — that's what `restore` should
        // attach to a fresh VM. The original bake path lives
        // in `spec.image` for audit purposes.
        let mut snapshot_spec = spec.clone();
        snapshot_spec.rootfs_source = Some(snapshot_rootfs.clone());
        let manifest = crate::snapshot::VzSnapshotManifest::new(id, snapshot_spec);
        let manifest_path = dest.join(crate::snapshot::MANIFEST_FILENAME);
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| SandboxError::Snapshot(format!("manifest serialize: {e}")))?;
        tokio::fs::write(&manifest_path, bytes)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("write manifest: {e}")))?;
        crate::snapshot::build_metadata(dest, &spec.image).await
    }

    async fn restore(&self, src: PathBuf) -> Result<SandboxId, SandboxError> {
        let manifest = crate::snapshot::read_manifest(&src).await?;
        let snapshot_rootfs = manifest.spec.rootfs_source.clone().ok_or_else(|| {
            SandboxError::Snapshot(
                "snapshot manifest missing rootfs_source — cannot restore without a \
                 disk image"
                    .into(),
            )
        })?;
        if !snapshot_rootfs.exists() {
            return Err(SandboxError::Snapshot(format!(
                "restore: snapshot rootfs at {} no longer exists",
                snapshot_rootfs.display()
            )));
        }
        let memory_mib = if manifest.spec.memory.max_mib > 0 {
            manifest.spec.memory.max_mib
        } else {
            self.cfg.default_memory_mib
        };
        let vcpus = if manifest.spec.cpu.vcpus > 0 {
            manifest.spec.cpu.vcpus
        } else {
            self.cfg.default_vcpus
        };

        // Allocate the new sandbox id up front so we can name
        // its per-sandbox rootfs deterministically.
        let new_id = SandboxId::new();
        tokio::fs::create_dir_all(&self.work_dir).await?;
        let rootfs_path = per_sandbox_rootfs_path(&self.work_dir, new_id);

        // Clone the snapshot's rootfs into a fresh per-sandbox
        // file. The snapshot's clone stays intact (so a forked
        // session or a re-resume after this one can clone it
        // again); the new sandbox writes only to its own clone.
        clone_or_copy(&snapshot_rootfs, &rootfs_path).await?;
        tracing::debug!(
            sandbox_id = %new_id,
            src = %snapshot_rootfs.display(),
            dst = %rootfs_path.display(),
            "vz: cloned snapshot rootfs to fresh per-sandbox path for cold-resume"
        );

        // Cold-resume: build a fresh VM with the snapshot's
        // disk-state and start it. VZ's
        // restoreMachineStateFromURL is not actually functional
        // for arm64 Linux guests (see snapshot() comment).
        // Cold-boot is the canonical path — Apple's own
        // containerization framework uses it. The
        // bootstrap-supervisor pattern + Claude's `--resume`
        // hand off conversation continuity across the boot.
        let mut vm_cfg = VmConfig::new(
            self.cfg.kernel_path.clone(),
            rootfs_path.clone(),
            memory_mib,
            vcpus,
        );
        vm_cfg.harness_substrate = manifest.spec.harness_substrate.clone();
        let (vm, port_fds) = VzVm::new(vm_cfg)?;
        if let Err(e) = vm.start().await {
            let _ = tokio::fs::remove_file(&rootfs_path).await;
            return Err(e.into());
        }

        let vsock_uds_path = self.vsock_uds_path_for(new_id);

        // Re-bind the virtio-console UDS bridge against the
        // restored VM. The in-VM `engram-bootstrap` supervisor
        // (kept alive by the bootstrap-as-supervisor pattern this
        // backend matches) is reachable via <vsock_uds>_1025 just
        // like a fresh VM.
        let harness_sink = self.harness_sink.lock().clone();
        let bridge = ConsoleBridge::start(vsock_uds_path.clone(), port_fds, harness_sink)
            .await
            .map_err(SandboxError::from)?;

        self.sandboxes.insert(
            new_id,
            VzSandboxState {
                spec: manifest.spec,
                vm: Arc::new(vm),
                bridge: parking_lot::Mutex::new(Some(bridge)),
                vsock_uds_path,
                rootfs_path,
                guest_ip: Mutex::new(None),
            },
        );
        Ok(new_id)
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
            bridge.stop().await;
        }
        // Best-effort stop. If the VM is already stopped or in a
        // state that can't accept stop (e.g. failed-to-start),
        // VZ surfaces an NSError; we log and continue, since the
        // observable goal of `destroy` is "this sandbox is gone."
        if let Err(e) = state.vm.stop().await {
            tracing::warn!(error = %e, sandbox_id = %id, "vz stop returned an error; releasing handle anyway");
        }
        // Remove the per-sandbox rootfs clone. Best-effort: if
        // the unlink fails (e.g., file already gone), the next
        // sandbox with a fresh UUID still gets its own clone, so
        // we just log and move on. Persistent state lives in
        // snapshot directories, not here.
        if let Err(e) = tokio::fs::remove_file(&state.rootfs_path).await {
            tracing::debug!(
                error = %e,
                path = %state.rootfs_path.display(),
                sandbox_id = %id,
                "vz: failed to remove per-sandbox rootfs (likely already gone)"
            );
        }
        // `state` (and the Arc<VzVm> inside) drops here — releases
        // ObjC retains.
        drop(state);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.sandboxes.iter().map(|kv| *kv.key()).collect())
    }

    /// Discover the guest's primary IPv4 address by asking agentd
    /// over the vsock-bridge. First successful answer is cached on
    /// the per-sandbox state; subsequent calls are O(1) memory reads.
    /// Returns `None` if the agent isn't reachable yet (e.g. shell
    /// requested before bootstrap completes) or reports no
    /// non-loopback address.
    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        if let Some(live) = self.sandboxes.get(&id) {
            if let Some(ip) = live.guest_ip.lock().clone() {
                return Some(ip);
            }
        }
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        // Bound the round-trip — virtio-console doesn't surface clean
        // close semantics back to host UDS reads, so an agent that
        // doesn't understand the GuestIp verb (e.g. an older bake)
        // would otherwise hang the dial here forever. 2s is plenty for
        // a healthy in-process round trip and short enough that a
        // dashboard SHELL-tab click sees a prompt 503.
        let fut = async {
            let conn = UnixStream::connect(&agent_uds).await.ok()?;
            let (mut reader, mut writer) = tokio::io::split(conn);
            write_msg(&mut writer, &WireRequest::GuestIp).await.ok()?;
            let resp: WireResponse = read_msg(&mut reader).await.ok()?;
            match resp {
                WireResponse::GuestIp(ip) => ip,
                _ => None,
            }
        };
        let ip = tokio::time::timeout(Duration::from_secs(2), fut)
            .await
            .ok()
            .flatten();
        if let Some(ref s) = ip {
            if let Some(live) = self.sandboxes.get(&id) {
                *live.guest_ip.lock() = Some(s.clone());
            }
        }
        ip
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
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".vsock"));
    }
}
