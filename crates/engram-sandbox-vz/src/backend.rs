//! The macOS `SandboxBackend`: one `VZVirtualMachine` per sandbox,
//! driven over a vsock bridge the same way the Firecracker backend
//! drives its guests. The non-macOS shell in `lib.rs` shadows these
//! types so the workspace compiles everywhere.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use dashmap::DashMap;
use engram_agentd::{
    read_msg, write_msg, WireExecEvent, WireExecRequest, WireRequest, WireResponse,
};
use engram_core::traits::sandbox::{
    ForgeSink, HarnessByteStream, HarnessSink, SandboxBackend, UploadSink,
};
use engram_core::types::endpoints::GuestEndpoints;
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::SandboxProbe;
use engram_core::types::sandbox::{
    AgentSpec, AuxRoDrive, ExecEvent, ExecRequest, ExecStream, SandboxSpec, SessionDisks,
    SessionFileMetadata, SessionFileSpec, SessionFileStream,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::vm::{VmConfig, VzVm};
use crate::vsock_bridge::{port_uds_path, VsockBridge, VsockConnector};

/// Vsock port engram-agentd binds inside the rootfs. Same number FC
/// uses; the in-VM binary doesn't know which VMM is hosting it.
const ENGRAM_AGENTD_PORT: u32 = 1024;

#[derive(serde::Serialize)]
struct LegacyWireExecRequest {
    command: Vec<String>,
    stdin: Option<Vec<u8>>,
    env: HashMap<String, String>,
    workdir: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(serde::Serialize)]
enum LegacyWireRequest {
    Exec(LegacyWireExecRequest),
}

/// Static config for the VZ backend — values that are the same for
/// every sandbox the backend creates. Per-sandbox overrides ride on
/// `SandboxSpec` (cpu/memory/disk).
#[derive(Clone, Debug)]
pub struct VzConfig {
    /// The arm64 Linux kernel image VZ boots: the owned kernel that
    /// `just pull-kernel` fetches, the same one Firecracker guests boot.
    pub kernel_path: PathBuf,
    /// Host egress, DNS, and metadata-proxy ports, passed to the guest as
    /// `ENGRAM_EGRESS=<proxy>:<dns>:<metadata>` on the kernel cmdline so
    /// the init shim installs in-guest DNAT redirects. `None` boots with
    /// open egress.
    pub egress_ports: Option<(u16, u16, u16)>,
    /// Default RAM in MiB applied when `SandboxSpec::memory.max_mib`
    /// is zero or unset. VZ minimum is 128 MiB.
    pub default_memory_mib: u32,
    /// Default vCPU count applied when `SandboxSpec::cpu.vcpus` is
    /// zero or unset.
    pub default_vcpus: u32,
    /// Directory of content-addressed bundles (`<sha>.squashfs`) and the
    /// `current.json` stamp. A drive whose `sha256` is `Some` attaches
    /// `bundle_dir/<sha>.squashfs`.
    pub bundle_dir: PathBuf,
}

impl VzConfig {
    pub fn with_kernel(kernel_path: impl Into<PathBuf>) -> Self {
        Self {
            kernel_path: kernel_path.into(),
            egress_ports: None,
            default_memory_mib: 512,
            default_vcpus: 1,
            bundle_dir: PathBuf::from(AuxRoDrive::SHARED_DIR),
        }
    }

    pub fn with_bundle_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.bundle_dir = dir.into();
        self
    }

    pub fn with_egress_ports(mut self, proxy: u16, dns: u16, metadata: u16) -> Self {
        self.egress_ports = Some((proxy, dns, metadata));
        self
    }
}

/// Per-sandbox state owned by `VzBackend`.
struct VzSandboxState {
    /// The spec with symbolic bundle slots; a cold restore re-resolves
    /// them against the host's current stamp.
    spec: SandboxSpec,
    /// The platform machine identifier and virtio-net MAC this VM was
    /// built with. A warm restore must rebuild the identical config, so
    /// the snapshot's warm block carries both.
    machine_identifier: Vec<u8>,
    mac_address: String,
    /// The aux drives attached to the live VM, slots resolved to content
    /// shas. A warm restore re-attaches exactly these.
    resolved_aux_ro_drives: Vec<AuxRoDrive>,
    /// Dropping this releases the ObjC objects once in-flight dispatched
    /// work completes.
    vm: Arc<VzVm>,
    /// `destroy` takes the bridge out to call its async `stop`.
    bridge: parking_lot::Mutex<Option<VsockBridge>>,
    /// Host-to-guest dialer: one fresh vsock stream per `open_guest_stream`.
    connector: VsockConnector,
    /// `<short-socket-dir>/<sandbox_id>.vsock`: the per-port UDS paths
    /// hang off it and must stay within SUN_LEN, so it never sits under
    /// `work_dir`.
    vsock_uds_path: PathBuf,
    /// Guest network identity, cached on the first `guest_endpoints` answer.
    guest_endpoints: Mutex<Option<GuestEndpoints>>,
}

/// A VZ VM has no host pid. `vm::VmStopDelegate` flips a per-VM dead
/// flag from the framework's stop callbacks; `list()` and `probe_sandbox`
/// read it, and cleanup stays coordinator-driven through `destroy`.
pub struct VzBackend {
    work_dir: PathBuf,
    cfg: VzConfig,
    sandboxes: DashMap<SandboxId, VzSandboxState>,
    teardowns: engram_core::teardown::TeardownRegistry<SandboxId>,
    /// The sinks the vsock bridge hands guest-initiated connections to:
    /// harness (port 1026), forge credentials (1028), uploads (1029).
    harness_sink: Mutex<Option<HarnessSink>>,
    forge_sink: Mutex<Option<ForgeSink>>,
    upload_sink: Mutex<Option<UploadSink>>,
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
                 `just pull-kernel`)",
                cfg.kernel_path.display()
            )));
        }
        Ok(Self {
            work_dir,
            cfg,
            sandboxes: DashMap::new(),
            teardowns: Default::default(),
            harness_sink: Mutex::new(None),
            forge_sink: Mutex::new(None),
            upload_sink: Mutex::new(None),
        })
    }

    fn vsock_uds_path_for(&self, id: SandboxId) -> PathBuf {
        let dir = engram_core::socket::short_socket_dir();
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{id}.vsock"))
    }

    /// Resolve the symbolic stamped slots against this host's
    /// `current.json`. The agentd slot is required: the guest's init
    /// execs agentd out of that mount. The guest-tools slot is optional:
    /// it carries the SHELL tab's ttyd, and an unresolved slot is skipped
    /// at attach.
    fn resolve_agentd_slot(&self, drives: &mut [AuxRoDrive]) -> Result<(), SandboxError> {
        fn unresolved(drives: &[AuxRoDrive], slot: usize) -> bool {
            drives
                .iter()
                .any(|d| d.sha256.is_none() && d.slot_index() == Some(slot))
        }
        fn resolve(drives: &mut [AuxRoDrive], slot: usize, sha: &str) {
            for d in drives.iter_mut() {
                if d.sha256.is_none() && d.slot_index() == Some(slot) {
                    d.sha256 = Some(sha.to_string());
                }
            }
        }
        let needs_agentd = unresolved(drives, AuxRoDrive::AGENTD_SLOT_INDEX);
        let needs_guest_tools = unresolved(drives, AuxRoDrive::GUEST_TOOLS_SLOT_INDEX);
        if !needs_agentd && !needs_guest_tools {
            return Ok(());
        }
        let stamp_path = self.cfg.bundle_dir.join(AuxRoDrive::CURRENT_STAMP);
        let bytes = std::fs::read(&stamp_path).map_err(|e| {
            SandboxError::InvalidSpec(format!(
                "read bundle stamp {}: {e} — this host stages no bundles; it \
                 can't boot the agentd slot",
                stamp_path.display()
            ))
        })?;
        let stamp: std::collections::HashMap<String, String> = serde_json::from_slice(&bytes)
            .map_err(|e| {
                SandboxError::InvalidSpec(format!(
                    "parse bundle stamp {}: {e}",
                    stamp_path.display()
                ))
            })?;
        if needs_agentd {
            let sha = stamp.get(AuxRoDrive::AGENTD_STAMP_KEY).ok_or_else(|| {
                SandboxError::InvalidSpec(format!(
                    "bundle stamp {} carries no `{}` entry — restage bundles \
                     (`just bundles-squashfs`)",
                    stamp_path.display(),
                    AuxRoDrive::AGENTD_STAMP_KEY,
                ))
            })?;
            resolve(drives, AuxRoDrive::AGENTD_SLOT_INDEX, sha);
        }
        if needs_guest_tools {
            match stamp.get(AuxRoDrive::GUEST_TOOLS_STAMP_KEY) {
                Some(sha) => resolve(drives, AuxRoDrive::GUEST_TOOLS_SLOT_INDEX, sha),
                None => tracing::warn!(
                    stamp = %stamp_path.display(),
                    "bundle stamp carries no `{}` entry — the SHELL tab only \
                     works if the image bakes ttyd; restage bundles \
                     (`just bundles-squashfs`)",
                    AuxRoDrive::GUEST_TOOLS_STAMP_KEY,
                ),
            }
        }
        Ok(())
    }

    /// Resolved sizing: the spec's values, or the backend defaults where
    /// the spec says zero. The warm block records the result so a restore
    /// never recomputes it against changed defaults.
    fn sizing(&self, spec: &SandboxSpec) -> (u32, u32) {
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
        (memory_mib, vcpus)
    }

    /// The swap disk to attach: the caller's, when the VM's shape has a
    /// swap drive. The device size IS the per-sandbox cap, so the caller
    /// sizes the disk.
    fn swap_disk(
        swap_mib: Option<u32>,
        swap: Option<PathBuf>,
    ) -> Result<Option<PathBuf>, SandboxError> {
        if swap_mib.is_none_or(|m| m == 0) {
            return Ok(None);
        }
        swap.map(Some).ok_or_else(|| {
            SandboxError::InvalidSpec("the spec has swap but no swap disk was provided".into())
        })
    }

    /// Snapshots outlive their sandbox, so they live under
    /// `<work_dir>/snapshots/<id>/`, never under a per-sandbox dir.
    fn snapshot_dir_for(&self, snapshot_id: SnapshotId) -> PathBuf {
        self.work_dir
            .join("snapshots")
            .join(snapshot_id.to_string())
    }

    /// The kernel cmdline a VM built today boots with. The warm block
    /// records it, and a restore whose value differs goes cold: the
    /// resumed guest's in-memory state (its egress DNAT rules) reflects
    /// the saved cmdline, and only the init shim applies today's.
    fn current_kernel_cmdline(&self) -> String {
        VmConfig::new(
            self.cfg.kernel_path.clone(),
            std::path::PathBuf::new(),
            0,
            0,
        )
        .with_egress_ports(self.cfg.egress_ports)
        .kernel_cmdline
    }

    /// Ask the in-guest agentd to `sync(2)`. Best effort, bounded to 5 s
    /// so a wedged guest cannot stall the snapshot; on failure the disk
    /// holds the last ext4 commit.
    async fn flush_guest_fs(&self, id: SandboxId, vsock_uds_path: &Path) {
        let agent_uds = port_uds_path(vsock_uds_path, ENGRAM_AGENTD_PORT);
        let fut = async {
            let conn = UnixStream::connect(&agent_uds).await.ok()?;
            let (mut reader, mut writer) = tokio::io::split(conn);
            write_msg(&mut writer, &WireRequest::Sync).await.ok()?;
            let resp: WireResponse = read_msg(&mut reader).await.ok()?;
            Some(resp)
        };
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(Some(WireResponse::Synced)) => {
                tracing::debug!(sandbox_id = %id, "vz: guest fs flushed before snapshot");
            }
            Ok(Some(other)) => {
                tracing::warn!(sandbox_id = %id, ?other, "vz: unexpected reply to Sync; snapshot will use last ext4 commit");
            }
            Ok(None) => {
                tracing::warn!(sandbox_id = %id, "vz: guest fs flush dial failed; snapshot will use last ext4 commit");
            }
            Err(_) => {
                tracing::warn!(sandbox_id = %id, "vz: guest fs flush timed out after 5s; snapshot will use last ext4 commit");
            }
        }
    }

    /// Push the host's wall clock into a warm-restored guest. VZ exposes
    /// no PTP device, so the guest wakes with `CLOCK_REALTIME` frozen at
    /// save time. Best effort, bounded like `flush_guest_fs`.
    async fn step_guest_clock(&self, id: SandboxId, vsock_uds_path: &Path) {
        let agent_uds = port_uds_path(vsock_uds_path, ENGRAM_AGENTD_PORT);
        let unix_nanos = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(_) => return,
        };
        let fut = async {
            let conn = UnixStream::connect(&agent_uds).await.ok()?;
            let (mut reader, mut writer) = tokio::io::split(conn);
            write_msg(&mut writer, &WireRequest::StepClock { unix_nanos })
                .await
                .ok()?;
            let resp: WireResponse = read_msg(&mut reader).await.ok()?;
            Some(resp)
        };
        match tokio::time::timeout(Duration::from_secs(5), fut).await {
            Ok(Some(WireResponse::ClockStepped {
                applied_offset_nanos,
            })) => {
                tracing::info!(
                    sandbox_id = %id,
                    applied_offset_secs = applied_offset_nanos.map(|n| n / 1_000_000_000),
                    "vz: warm-restored guest clock stepped (None = under threshold)"
                );
            }
            Ok(Some(other)) => {
                tracing::warn!(sandbox_id = %id, ?other, "vz: unexpected reply to StepClock");
            }
            Ok(None) => {
                tracing::warn!(
                    sandbox_id = %id,
                    "vz: StepClock dial failed (old agentd generation?) — guest clock stays stale"
                );
            }
            Err(_) => {
                tracing::warn!(sandbox_id = %id, "vz: StepClock timed out after 5s");
            }
        }
    }

    /// Start the vsock bridge for `vm` with the registered sinks. The
    /// bridge binds the host-side agentd UDS before it returns, so a
    /// later dial never races an unbound listener.
    async fn attach_bridge(
        &self,
        vm: &Arc<VzVm>,
        vsock_uds_path: &Path,
    ) -> Result<(VsockBridge, VsockConnector), SandboxError> {
        let harness_sink = self.harness_sink.lock().clone();
        let forge_sink = self.forge_sink.lock().clone();
        let upload_sink = self.upload_sink.lock().clone();
        VsockBridge::start(
            vm.raw_clone(),
            vm.queue_clone(),
            vsock_uds_path.to_path_buf(),
            harness_sink,
            forge_sink,
            upload_sink,
        )
        .await
        .map_err(SandboxError::from)
    }

    /// The warm-restore gate: pure checks, so `Err(reason)` costs
    /// nothing and the restore cold-boots.
    fn warm_gate(
        &self,
        manifest: &crate::snapshot::VzSnapshotManifest,
        src: &Path,
    ) -> Result<crate::snapshot::WarmMachineState, String> {
        let Some(w) = manifest.warm.clone() else {
            return Err("manifest has no warm block (the save failed)".into());
        };
        if !src.join(crate::snapshot::MACHINE_STATE_FILENAME).exists() {
            return Err("machine.vzs missing from the snapshot dir".into());
        }
        let current = self.current_kernel_cmdline();
        if w.kernel_cmdline != current {
            return Err(format!(
                "kernel cmdline drift since save (saved {:?}, current {:?}) — cold reboot \
                 re-runs the init shim",
                w.kernel_cmdline, current
            ));
        }
        for d in &w.aux_ro_drives {
            if let Some(sha) = d.sha256.as_deref() {
                let p = crate::vm::staged_bundle_path(&self.cfg.bundle_dir, sha);
                if !p.exists() {
                    return Err(format!(
                        "staged bundle {} ({}) is gone — the saved device config can't be \
                         rebuilt",
                        p.display(),
                        d.drive_id
                    ));
                }
            }
        }
        // Two live VMs on the shared NAT must never carry the same MAC.
        if self
            .sandboxes
            .iter()
            .any(|kv| kv.value().mac_address == w.mac_address)
        {
            return Err(
                "saved MAC is already live on this host (double restore / source still \
                 running)"
                    .into(),
            );
        }
        Ok(w)
    }

    /// Boot the saved machine state: rebuild the byte-equivalent config
    /// from the warm block, restore, attach the bridge while still
    /// paused, resume, step the guest clock.
    async fn try_warm_restore(
        &self,
        new_id: SandboxId,
        src: &Path,
        w: &crate::snapshot::WarmMachineState,
        disks: &SessionDisks,
        spec: &SandboxSpec,
    ) -> Result<(), SandboxError> {
        // The config must match the saved device shape, so the swap drive
        // follows the warm block's size, not today's spec.
        let swap_path = Self::swap_disk(w.swap_mib, disks.swap.clone())?;
        let vm_cfg = VmConfig::new(
            self.cfg.kernel_path.clone(),
            disks.root.clone(),
            w.memory_mib,
            w.vcpus,
        )
        .with_swap_path(swap_path)
        .with_aux_ro_drives(w.aux_ro_drives.clone(), self.cfg.bundle_dir.clone())
        .with_egress_ports(self.cfg.egress_ports)
        .with_machine_identifier(w.machine_identifier.clone())
        .with_mac_address(w.mac_address.clone());
        let vm = Arc::new(VzVm::new(vm_cfg)?);
        vm.restore(&src.join(crate::snapshot::MACHINE_STATE_FILENAME))
            .await?;
        let vsock_uds_path = self.vsock_uds_path_for(new_id);
        let (bridge, connector) = self.attach_bridge(&vm, &vsock_uds_path).await?;
        vm.resume().await?;
        self.step_guest_clock(new_id, &vsock_uds_path).await;
        self.sandboxes.insert(
            new_id,
            VzSandboxState {
                spec: spec.clone(),
                machine_identifier: w.machine_identifier.clone(),
                mac_address: w.mac_address.clone(),
                resolved_aux_ro_drives: w.aux_ro_drives.clone(),
                vm,
                bridge: parking_lot::Mutex::new(Some(bridge)),
                connector,
                vsock_uds_path,
                guest_endpoints: Mutex::new(None),
            },
        );
        tracing::info!(sandbox_id = %new_id, "vz: warm restore, memory intact");
        Ok(())
    }

    /// Warm when the gate passes, cold otherwise. `mounts_override =
    /// Some` (a fresh create) replaces the snapshot's slots with this
    /// session's resolved skills and is always cold: a different device
    /// config than the saved VM. `None` (resume) keeps the pinned drives.
    async fn restore_impl(
        &self,
        new_id: SandboxId,
        metadata: SnapshotMetadata,
        disks: Option<SessionDisks>,
        mounts_override: Option<Vec<AuxRoDrive>>,
    ) -> Result<(), SandboxError> {
        let disks = disks.ok_or_else(|| {
            SandboxError::Snapshot("restore requires the session's disk paths".into())
        })?;
        if !disks.root.exists() {
            return Err(SandboxError::Snapshot(format!(
                "restore: root disk {} does not exist",
                disks.root.display()
            )));
        }
        let src = self.snapshot_dir_for(metadata.id);
        let manifest = crate::snapshot::read_manifest(&src).await?;
        let warm_requested = mounts_override.is_none();
        let mut spec = manifest.spec.clone();
        spec.rootfs_source = Some(disks.root.clone());
        spec.swap_source = disks.swap.clone();
        if let Some(mounts) = mounts_override {
            spec.aux_ro_drives = mounts;
        }
        tokio::fs::create_dir_all(&self.work_dir).await?;

        // WARM restore (memory intact) when the gate passes. A failure
        // past the gate is an error: the briefly-resumed guest may have
        // written the caller's disk, so a cold boot of it is not a
        // pristine fallback.
        if warm_requested {
            match self.warm_gate(&manifest, &src) {
                Err(reason) => {
                    tracing::info!(
                        snapshot_id = %metadata.id,
                        reason,
                        "vz: warm restore unavailable — cold-booting"
                    );
                }
                Ok(w) => {
                    return self.try_warm_restore(new_id, &src, &w, &disks, &spec).await;
                }
            }
        }

        // Cold boot: a new machine with a fresh identity, and the symbolic
        // slots resolved against this host's current stamp (the warm path
        // keeps the captured agentd running instead).
        self.resolve_agentd_slot(&mut spec.aux_ro_drives)?;
        let (memory_mib, vcpus) = self.sizing(&spec);
        let (machine_identifier, mac_address) = mint_vm_identity();
        let swap_path = Self::swap_disk(spec.swap_mib, disks.swap)?;
        let vm_cfg = VmConfig::new(self.cfg.kernel_path.clone(), disks.root, memory_mib, vcpus)
            .with_swap_path(swap_path)
            .with_aux_ro_drives(spec.aux_ro_drives.clone(), self.cfg.bundle_dir.clone())
            .with_egress_ports(self.cfg.egress_ports)
            .with_machine_identifier(machine_identifier.clone())
            .with_mac_address(mac_address.clone());
        let vm = Arc::new(VzVm::new(vm_cfg)?);
        vm.start().await?;
        let vsock_uds_path = self.vsock_uds_path_for(new_id);
        let (bridge, connector) = self.attach_bridge(&vm, &vsock_uds_path).await?;

        self.sandboxes.insert(
            new_id,
            VzSandboxState {
                resolved_aux_ro_drives: spec.aux_ro_drives.clone(),
                spec,
                machine_identifier,
                mac_address,
                vm,
                bridge: parking_lot::Mutex::new(Some(bridge)),
                connector,
                vsock_uds_path,
                guest_endpoints: Mutex::new(None),
            },
        );
        Ok(())
    }
}

/// Run the agentd exec protocol over a connected byte stream. The
/// Firecracker backend carries the same function; only the connection
/// setup differs.
async fn drive_exec_protocol<R, W>(
    sandbox_id: SandboxId,
    mut reader: R,
    mut writer: W,
    cmd: ExecRequest,
    durable_capable: bool,
) -> Result<ExecStream, SandboxError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let exec_id = cmd
        .exec_id
        .clone()
        .unwrap_or_else(|| format!("vz-{}", uuid::Uuid::new_v4().simple()));
    if durable_capable {
        let req = WireRequest::Exec(WireExecRequest {
            command: cmd.command.clone(),
            stdin: cmd.stdin.clone(),
            env: cmd.env.clone(),
            workdir: cmd.workdir.clone(),
            timeout_ms: cmd.timeout.map(|d| d.as_millis() as u64),
            exec_id: Some(exec_id.clone()),
            stdout_offset: cmd.stdout_offset,
            stderr_offset: cmd.stderr_offset,
            wake: cmd.wake,
            attach_only: false,
        });
        write_msg(&mut writer, &req)
            .await
            .map_err(|e| SandboxError::Vm(format!("send durable WireRequest::Exec: {e}").into()))?;
    } else {
        write_msg(
            &mut writer,
            &LegacyWireRequest::Exec(LegacyWireExecRequest {
                command: cmd.command.clone(),
                stdin: cmd.stdin.clone(),
                env: cmd.env.clone(),
                workdir: cmd.workdir.clone(),
                timeout_ms: cmd.timeout.map(|duration| duration.as_millis() as u64),
            }),
        )
        .await
        .map_err(|e| SandboxError::Vm(format!("send stage-1 WireRequest::Exec: {e}").into()))?;
    }

    let (tx, rx) = mpsc::channel::<ExecEvent>(64);
    let reader_exec_id = exec_id.clone();
    tokio::spawn(async move {
        let exec_id = reader_exec_id;
        loop {
            match read_msg::<_, WireExecEvent>(&mut reader).await {
                Ok(WireExecEvent::Started(started_id)) => {
                    if started_id != exec_id {
                        let _ = tx
                            .send(ExecEvent::Stderr(Bytes::from(format!(
                                "durable exec protocol violation: expected Started({exec_id}), got Started({started_id})\n"
                            ))))
                            .await;
                        let _ = tx.send(ExecEvent::Exit(None)).await;
                        return;
                    }
                }
                Ok(WireExecEvent::Degraded(reason)) => {
                    tracing::warn!(%reason, "guest durable exec journal degraded");
                }
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
                Ok(WireExecEvent::Refused { reason }) => {
                    let _ = tx.send(ExecEvent::Refused(reason)).await;
                    return;
                }
                Err(e) => {
                    if durable_capable {
                        // EOF/read failure is transport state, not a guest
                        // journal verdict. End without Exit so the coordinator
                        // exposes a retryable Unavailable and the caller can
                        // attach again with its delivered offsets.
                        tracing::warn!(
                            %sandbox_id,
                            %exec_id,
                            error = %e,
                            "durable vz exec connection ended without explicit Exit; ending event stream",
                        );
                        return;
                    }
                    // An old agent has no spawn-dedupe journal. Retrying may
                    // double-run the command, so keep the stage-1 terminal
                    // floor for the legacy path.
                    tracing::warn!(
                        %sandbox_id,
                        %exec_id,
                        error = %e,
                        "legacy vz exec connection ended without explicit Exit; emitting Exit(None)",
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

async fn probe_durable_exec(mut stream: UnixStream) -> Result<bool, SandboxError> {
    write_msg(
        &mut stream,
        &WireRequest::CancelExec {
            exec_id: engram_agentd::handler::DURABLE_EXEC_CAPABILITY_PROBE.into(),
        },
    )
    .await
    .map_err(|error| SandboxError::Vm(format!("durable exec capability send: {error}").into()))?;
    match read_msg::<_, WireResponse>(&mut stream).await {
        Ok(WireResponse::ExecCancelled) => Ok(true),
        Ok(WireResponse::Error { kind, message }) => {
            tracing::warn!(%kind, %message, "guest agentd has no durable exec; using stage-1 exec");
            Ok(false)
        }
        Ok(other) => Err(SandboxError::Vm(
            format!("durable exec capability returned {other:?}").into(),
        )),
        Err(error) => Err(SandboxError::Vm(
            format!("durable exec capability failed before submission: {error}").into(),
        )),
    }
}

/// A fresh per-VM identity: a platform machine identifier plus a random
/// locally-administered unicast MAC. A VM built without an explicit
/// identity gets one VZ never exposes, so its machine state could never
/// be restored.
fn mint_vm_identity() -> (Vec<u8>, String) {
    let mid = crate::vm::fresh_machine_identifier();
    let b = uuid::Uuid::new_v4().into_bytes();
    let mac = format!(
        "0a:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4]
    );
    (mid, mac)
}

/// `VZNATNetworkDeviceAttachment` takes no host-side filter, so VZ
/// enforces `allow_hosts` only through the init shim's in-guest DNAT,
/// which a root guest can flush. Warn once so nobody mistakes dev
/// steering for isolation.
fn warn_vz_soft_egress_once(network: &engram_core::types::NetworkPolicy) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    let restrictive = matches!(network.default, engram_core::types::NetworkDefault::Deny)
        && !network.allow_hosts.is_empty();
    if restrictive && !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "VZ enforces manifest.network.allow_hosts only via in-guest \
             steering: a root guest can bypass it, and images without iptables \
             get open egress. Use the Firecracker backend on Linux for \
             production isolation."
        );
    }
}

#[async_trait]
impl SandboxBackend for VzBackend {
    fn bundle_dir(&self) -> &std::path::Path {
        &self.cfg.bundle_dir
    }

    async fn create(&self, id: SandboxId, spec: SandboxSpec) -> Result<(), SandboxError> {
        warn_vz_soft_egress_once(&spec.network);
        let rootfs_path = spec.rootfs_source.clone().ok_or_else(|| {
            SandboxError::InvalidSpec(
                "VzBackend requires SandboxSpec.rootfs_source: this sandbox's own \
                 writable ext4 with the init shim"
                    .into(),
            )
        })?;
        // Validate up front rather than letting VZ surface a less
        // specific NSError later.
        if !rootfs_path.exists() {
            return Err(SandboxError::InvalidSpec(format!(
                "vz rootfs not found at {}",
                rootfs_path.display()
            )));
        }

        let (memory_mib, vcpus) = self.sizing(&spec);
        tokio::fs::create_dir_all(&self.work_dir).await?;

        let mut aux_ro_drives = spec.aux_ro_drives.clone();
        self.resolve_agentd_slot(&mut aux_ro_drives)?;
        tracing::info!(
            sandbox_id = %id,
            bundle_dir = %self.cfg.bundle_dir.display(),
            resolved = ?aux_ro_drives
                .iter()
                .filter(|d| d.sha256.is_some())
                .map(|d| format!("{}={}", d.drive_id, d.sha256.as_deref().unwrap_or("")))
                .collect::<Vec<_>>(),
            "vz: aux drives after agentd-slot resolution"
        );

        let (machine_identifier, mac_address) = mint_vm_identity();
        let resolved_aux_ro_drives = aux_ro_drives.clone();
        let swap_path = Self::swap_disk(spec.swap_mib, spec.swap_source.clone())?;
        let vm_cfg = VmConfig::new(self.cfg.kernel_path.clone(), rootfs_path, memory_mib, vcpus)
            .with_swap_path(swap_path)
            .with_aux_ro_drives(aux_ro_drives, self.cfg.bundle_dir.clone())
            .with_egress_ports(self.cfg.egress_ports)
            .with_machine_identifier(machine_identifier.clone())
            .with_mac_address(mac_address.clone());
        // An error before the insert below drops the VM: no half-registered
        // sandbox.
        let vm = Arc::new(VzVm::new(vm_cfg)?);
        vm.start().await?;
        let vsock_uds_path = self.vsock_uds_path_for(id);
        let (bridge, connector) = self.attach_bridge(&vm, &vsock_uds_path).await?;

        self.sandboxes.insert(
            id,
            VzSandboxState {
                spec,
                machine_identifier,
                mac_address,
                resolved_aux_ro_drives,
                vm,
                bridge: parking_lot::Mutex::new(Some(bridge)),
                connector,
                vsock_uds_path,
                guest_endpoints: Mutex::new(None),
            },
        );
        Ok(())
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        // The host-side UDS is bound before create/restore returns, so the
        // connect succeeds at once and the first read waits for agentd.
        tracing::debug!(
            sandbox_id = %id,
            argv0 = %agent.argv.first().map(|s| s.as_str()).unwrap_or("<empty>"),
            "vz start_agent: dialing agentd UDS",
        );
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);

        let mut conn = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(format!("connect agentd UDS {}: {e}", agent_uds.display()).into())
        })?;

        // The backend is the only party that knows the sandbox id before
        // boot, so it stamps the attach token into the harness env.
        let token_env = engram_harness_proto::attach_token::AttachToken {
            sandbox_id: id,
            binding_epoch: agent.binding_epoch,
        }
        .env();
        let mut agent = agent;
        agent.env.extend(token_env);

        let req = engram_agentd::WireRequest::SpawnHarness(engram_agentd::SpawnHarnessRequest {
            argv: agent.argv,
            env: agent.env.into_iter().collect(),
            session_env: agent.session_env.into_iter().collect(),
            host_ca_pem: agent.host_ca_pem,
        });
        engram_agentd::write_msg(&mut conn, &req)
            .await
            .map_err(|e| SandboxError::Vm(format!("write SpawnHarness: {e}").into()))?;
        let resp: engram_agentd::WireResponse = engram_agentd::read_msg(&mut conn)
            .await
            .map_err(|e| SandboxError::Vm(format!("read SpawnHarness response: {e}").into()))?;
        match resp {
            engram_agentd::WireResponse::HarnessSpawned { pid, ca_changed } => {
                tracing::debug!(
                    sandbox_id = %id,
                    pid = ?pid,
                    ca_changed = ?ca_changed,
                    "vz start_agent: harness spawned",
                );
                Ok(())
            }
            engram_agentd::WireResponse::Error { kind, message } => {
                Err(SandboxError::HarnessSpawn { kind, message })
            }
            other => Err(SandboxError::Vm(
                format!("SpawnHarness: unexpected response: {other:?}").into(),
            )),
        }
    }

    fn set_forge_sink(&self, sink: ForgeSink) {
        *self.forge_sink.lock() = Some(sink);
    }

    fn set_harness_sink(&self, sink: HarnessSink) {
        *self.harness_sink.lock() = Some(sink);
    }

    fn set_upload_sink(&self, sink: UploadSink) {
        *self.upload_sink.lock() = Some(sink);
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
        let probe = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(
                format!("connect engram-agentd UDS {}: {e}", agent_uds.display()).into(),
            )
        })?;
        let durable_capable = probe_durable_exec(probe).await?;
        let conn = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(
                format!(
                    "connect engram-agentd UDS {} after durable probe: {e}",
                    agent_uds.display()
                )
                .into(),
            )
        })?;
        let (reader, writer) = tokio::io::split(conn);
        drive_exec_protocol(id, reader, writer, cmd, durable_capable).await
    }

    async fn cancel_exec(&self, id: SandboxId, exec_id: String) -> Result<(), SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        let mut stream = UnixStream::connect(&agent_uds).await.map_err(|error| {
            SandboxError::Vm(
                format!("connect engram-agentd UDS {}: {error}", agent_uds.display()).into(),
            )
        })?;
        write_msg(
            &mut stream,
            &WireRequest::CancelExec {
                exec_id: exec_id.clone(),
            },
        )
        .await
        .map_err(|error| SandboxError::Vm(format!("send CancelExec: {error}").into()))?;
        match read_msg::<_, WireResponse>(&mut stream).await {
            Ok(WireResponse::ExecCancelled) => Ok(()),
            Ok(WireResponse::Error { kind, message }) => Err(SandboxError::Vm(
                format!("CancelExec({exec_id}) rejected ({kind}): {message}").into(),
            )),
            Ok(other) => Err(SandboxError::Vm(
                format!("CancelExec({exec_id}) returned {other:?}").into(),
            )),
            Err(error) => Err(SandboxError::Vm(
                format!("CancelExec({exec_id}) failed (old agentd protocol skew): {error}").into(),
            )),
        }
    }

    async fn write_file(
        &self,
        id: SandboxId,
        spec: SessionFileSpec,
        bytes: SessionFileStream,
    ) -> Result<SessionFileMetadata, SandboxError> {
        let agent_uds = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            port_uds_path(&live.vsock_uds_path, ENGRAM_AGENTD_PORT)
        };
        let connection = UnixStream::connect(&agent_uds).await.map_err(|error| {
            SandboxError::Vm(
                format!("connect engram-agentd UDS {}: {error}", agent_uds.display()).into(),
            )
        })?;
        engram_agentd::file_transfer::write_file(connection, spec, bytes).await
    }

    async fn read_file(
        &self,
        id: SandboxId,
        path: String,
    ) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError> {
        let agent_uds = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            port_uds_path(&live.vsock_uds_path, ENGRAM_AGENTD_PORT)
        };
        let connection = UnixStream::connect(&agent_uds).await.map_err(|error| {
            SandboxError::Vm(
                format!("connect engram-agentd UDS {}: {error}", agent_uds.display()).into(),
            )
        })?;
        engram_agentd::file_transfer::read_file(connection, path).await
    }

    /// Dial the in-guest relay on `port` and hand back the stream. Each
    /// call is its own vsock stream, so one forwarded connection cannot
    /// head-of-line block another.
    async fn open_guest_stream(
        &self,
        id: SandboxId,
        port: u32,
    ) -> Result<Option<HarnessByteStream>, SandboxError> {
        let connector = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.connector.clone()
        };
        let stream = connector.connect_stream(port).await.map_err(|e| {
            SandboxError::Vm(format!("vz open_guest_stream port {port}: {e}").into())
        })?;
        Ok(Some(stream))
    }

    /// Idempotent on a paused VM.
    async fn pause(&self, id: SandboxId) -> Result<(), SandboxError> {
        let vm = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vm.clone()
        };
        vm.pause().await.map_err(SandboxError::from)
    }

    /// Idempotent on a running VM.
    async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
        let vm = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vm.clone()
        };
        vm.resume().await.map_err(SandboxError::from)
    }

    async fn snapshot_hold(
        &self,
        id: SandboxId,
        _diff: bool,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.pause(id).await?;
        self.snapshot(id).await
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let (vm, spec, vsock_uds_path, machine_identifier, mac_address, resolved_aux) = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            (
                live.vm.clone(),
                live.spec.clone(),
                live.vsock_uds_path.clone(),
                live.machine_identifier.clone(),
                live.mac_address.clone(),
                live.resolved_aux_ro_drives.clone(),
            )
        };
        let (memory_mib, vcpus) = self.sizing(&spec);

        // The disk owner takes the disk under its own pause after a guest
        // fs flush. Called on a running VM, flush here too so a cold
        // restore of a save that failed still sees the page cache's
        // writes. A paused guest cannot answer the Sync RPC.
        let already_paused =
            vm.state().await == objc2_virtualization::VZVirtualMachineState::Paused;
        if already_paused {
            tracing::debug!(sandbox_id = %id, "vz: snapshot of a parked VM — skipping guest fs flush");
        } else {
            self.flush_guest_fs(id, &vsock_uds_path).await;
        }

        let snapshot_id = SnapshotId::new();
        let dest = self.snapshot_dir_for(snapshot_id);
        tokio::fs::create_dir_all(&dest).await.map_err(|e| {
            SandboxError::Snapshot(format!("create snapshot dir {}: {e}", dest.display()))
        })?;
        let dest = dest.as_path();

        vm.pause().await?;
        // Save the machine state (memory + device state) while paused.
        // BEST-EFFORT: a failure (locked keychain on a headless host, an
        // unsupported config) logs and omits the warm block, and the
        // snapshot restores cold. VZ writes the touched working set, not
        // the full RAM size.
        let warm = {
            let state_path = dest.join(crate::snapshot::MACHINE_STATE_FILENAME);
            match vm.save(&state_path).await {
                Ok(()) => Some(crate::snapshot::WarmMachineState {
                    machine_identifier: machine_identifier.clone(),
                    mac_address: mac_address.clone(),
                    kernel_cmdline: self.current_kernel_cmdline(),
                    aux_ro_drives: resolved_aux.clone(),
                    memory_mib,
                    vcpus,
                    swap_mib: spec.swap_mib.filter(|m| *m > 0),
                }),
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %id,
                        error = %e,
                        "vz: machine-state save failed — snapshot will be cold-boot only \
                         (locked keychain on a headless host?)"
                    );
                    let _ = tokio::fs::remove_file(&state_path).await;
                    None
                }
            }
        };
        // An externally paused VM stays paused: the caller owns that
        // pause.
        if !already_paused {
            vm.resume().await?;
        }

        let manifest = crate::snapshot::VzSnapshotManifest::new(id, spec.clone(), warm);
        let manifest_path = dest.join(crate::snapshot::MANIFEST_FILENAME);
        let bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| SandboxError::Snapshot(format!("manifest serialize: {e}")))?;
        tokio::fs::write(&manifest_path, bytes)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("write manifest: {e}")))?;

        crate::snapshot::build_metadata(dest, &spec.image, snapshot_id).await
    }

    fn snapshot_path_for(&self, snapshot_id: SnapshotId) -> PathBuf {
        self.snapshot_dir_for(snapshot_id)
    }

    async fn restore(
        &self,
        id: SandboxId,
        metadata: SnapshotMetadata,
        disks: Option<SessionDisks>,
    ) -> Result<(), SandboxError> {
        self.restore_impl(id, metadata, disks, None).await
    }

    async fn restore_fresh(
        &self,
        id: SandboxId,
        metadata: SnapshotMetadata,
        disks: Option<SessionDisks>,
        selected_mounts: Vec<AuxRoDrive>,
    ) -> Result<(), SandboxError> {
        self.restore_impl(id, metadata, disks, Some(selected_mounts))
            .await
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        if !self.sandboxes.contains_key(&id) {
            return match self.teardowns.join(&id) {
                Some(task) => task
                    .await
                    .map_err(|error| SandboxError::Vm(Box::new(error))),
                None => Ok(()),
            };
        }
        self.teardowns.run_or_join(id, || {
            let state = self.sandboxes.remove(&id).map(|(_, state)| state);
            Box::pin(async move {
                let Some(state) = state else { return Ok(()); };
                // The bridge stops before the VM so its pumps never reach
                // into a dying VM. The guard is non-Send: take, then await.
                let bridge = state.bridge.lock().take();
                if let Some(mut bridge) = bridge {
                    bridge.stop().await;
                }
                // A VM that is already stopped refuses stop; the sandbox is
                // gone either way.
                if let Err(e) = state.vm.stop().await {
                    tracing::warn!(error = %e, sandbox_id = %id, "vz stop returned an error; releasing handle anyway");
                }
                // The disks belong to the caller.
                drop(state);
                Ok(())
            })
        }).await.map_err(|error| SandboxError::Vm(Box::new(error)))
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        // A dead VM leaves the list so the heartbeat reports ground truth;
        // its map entry stays until the coordinator destroys it.
        let mut ids: std::collections::HashSet<_> = self
            .sandboxes
            .iter()
            .filter(|kv| !kv.value().vm.is_dead())
            .map(|kv| *kv.key())
            .collect();
        ids.extend(self.teardowns.keys());
        Ok(ids.into_iter().collect())
    }

    /// Each live sandbox's attached bundle generations.
    async fn aux_bundles_all(&self) -> Vec<engram_core::types::sandbox::SandboxAuxBundles> {
        self.sandboxes
            .iter()
            .filter(|kv| !kv.value().vm.is_dead())
            .filter_map(|kv| {
                let bundles: Vec<engram_core::types::sandbox::AuxBundleRef> = kv
                    .value()
                    .resolved_aux_ro_drives
                    .iter()
                    .filter_map(|d| {
                        d.sha256
                            .as_ref()
                            .map(|sha| engram_core::types::sandbox::AuxBundleRef {
                                drive_id: d.drive_id.clone(),
                                sha256: sha.clone(),
                            })
                    })
                    .collect();
                (!bundles.is_empty()).then(|| engram_core::types::sandbox::SandboxAuxBundles {
                    sandbox_id: *kv.key(),
                    bundles,
                })
            })
            .collect()
    }

    async fn probe_sandbox(&self, id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        let vm = match self.sandboxes.get(&id) {
            Some(live) => live.vm.clone(),
            None => {
                return Ok(SandboxProbe {
                    known_to_backend: false,
                    process_alive: false,
                    control_alive: None,
                })
            }
        };
        use objc2_virtualization::VZVirtualMachineState as S;
        let state = vm.state().await;
        let alive = !vm.is_dead()
            && matches!(
                state,
                S::Running | S::Paused | S::Starting | S::Pausing | S::Resuming
            );
        Ok(SandboxProbe {
            known_to_backend: true,
            process_alive: alive,
            control_alive: None,
        })
    }

    /// Ensure the in-guest `ttyd` is running and return its port. agentd
    /// replies once a loopback probe succeeds.
    async fn start_shell(&self, id: SandboxId) -> Result<u16, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        let conn = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(
                format!(
                    "connect agentd UDS for StartShell {}: {e}",
                    agent_uds.display()
                )
                .into(),
            )
        })?;
        let (mut reader, mut writer) = tokio::io::split(conn);
        // `port: None` is agentd's default. The bound keeps a wedged guest
        // from hanging the caller.
        write_msg(&mut writer, &WireRequest::StartShell { port: None })
            .await
            .map_err(|e| SandboxError::Vm(format!("write StartShell: {e}").into()))?;
        let resp: WireResponse =
            tokio::time::timeout(Duration::from_secs(30), read_msg(&mut reader))
                .await
                .map_err(|_| SandboxError::Vm("StartShell timed out after 30s".into()))?
                .map_err(|e| SandboxError::Vm(format!("read StartShell response: {e}").into()))?;
        match resp {
            WireResponse::ShellReady { port, .. } => Ok(port),
            WireResponse::Error { kind, message } => Err(SandboxError::Vm(
                format!("StartShell rejected ({kind}): {message}").into(),
            )),
            other => Err(SandboxError::Vm(
                format!("StartShell: unexpected response: {other:?}").into(),
            )),
        }
    }

    /// Ensure the in-guest browser stack is running and x11vnc accepts on
    /// its port. Returns the port plus agentd's optional CDP warning.
    async fn start_browser(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        let conn = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(
                format!(
                    "connect agentd UDS for StartBrowser {}: {e}",
                    agent_uds.display()
                )
                .into(),
            )
        })?;
        let (mut reader, mut writer) = tokio::io::split(conn);
        // agentd's own worst case (wedge probe + force stop + the launcher's
        // 20 s ready deadline) is about 24 s.
        write_msg(&mut writer, &WireRequest::StartBrowser { port: None })
            .await
            .map_err(|e| SandboxError::Vm(format!("write StartBrowser: {e}").into()))?;
        let resp: WireResponse =
            tokio::time::timeout(Duration::from_secs(45), read_msg(&mut reader))
                .await
                .map_err(|_| SandboxError::Vm("StartBrowser timed out after 45s".into()))?
                .map_err(|e| SandboxError::Vm(format!("read StartBrowser response: {e}").into()))?;
        match resp {
            WireResponse::BrowserReady {
                port, cdp_warning, ..
            } => Ok(engram_core::traits::sandbox::BrowserStart {
                port,
                warning: cdp_warning,
            }),
            WireResponse::Error { kind, message } => Err(SandboxError::Vm(
                format!("StartBrowser rejected ({kind}): {message}").into(),
            )),
            other => Err(SandboxError::Vm(
                format!("StartBrowser: unexpected response: {other:?}").into(),
            )),
        }
    }

    /// Idempotent: a no-op when the sandbox is gone or nothing runs.
    async fn stop_browser(&self, id: SandboxId) -> Result<(), SandboxError> {
        let vsock_uds_path = {
            let Some(live) = self.sandboxes.get(&id) else {
                return Ok(());
            };
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        let conn = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(format!("connect agentd UDS for StopBrowser: {e}").into())
        })?;
        let (mut reader, mut writer) = tokio::io::split(conn);
        write_msg(&mut writer, &WireRequest::StopBrowser)
            .await
            .map_err(|e| SandboxError::Vm(format!("write StopBrowser: {e}").into()))?;
        let _: WireResponse = tokio::time::timeout(Duration::from_secs(15), read_msg(&mut reader))
            .await
            .map_err(|_| SandboxError::Vm("StopBrowser timed out".into()))?
            .map_err(|e| SandboxError::Vm(format!("read StopBrowser response: {e}").into()))?;
        Ok(())
    }

    /// Ensure the in-guest IDE (code-server) is running and answering
    /// `/healthz`. Returns its port.
    async fn start_ide(&self, id: SandboxId) -> Result<u16, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        let conn = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(
                format!(
                    "connect agentd UDS for StartIde {}: {e}",
                    agent_uds.display()
                )
                .into(),
            )
        })?;
        let (mut reader, mut writer) = tokio::io::split(conn);
        // agentd's own worst case is about 23 s, as for StartBrowser.
        write_msg(&mut writer, &WireRequest::StartIde { port: None })
            .await
            .map_err(|e| SandboxError::Vm(format!("write StartIde: {e}").into()))?;
        let resp: WireResponse =
            tokio::time::timeout(Duration::from_secs(45), read_msg(&mut reader))
                .await
                .map_err(|_| SandboxError::Vm("StartIde timed out after 45s".into()))?
                .map_err(|e| SandboxError::Vm(format!("read StartIde response: {e}").into()))?;
        match resp {
            WireResponse::IdeReady { port, .. } => Ok(port),
            WireResponse::Error { kind, message } => Err(SandboxError::Vm(
                format!("StartIde rejected ({kind}): {message}").into(),
            )),
            other => Err(SandboxError::Vm(
                format!("StartIde: unexpected response: {other:?}").into(),
            )),
        }
    }

    /// Idempotent: a no-op when the sandbox is gone or nothing runs.
    async fn stop_ide(&self, id: SandboxId) -> Result<(), SandboxError> {
        let vsock_uds_path = {
            let Some(live) = self.sandboxes.get(&id) else {
                return Ok(());
            };
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        let conn = UnixStream::connect(&agent_uds)
            .await
            .map_err(|e| SandboxError::Vm(format!("connect agentd UDS for StopIde: {e}").into()))?;
        let (mut reader, mut writer) = tokio::io::split(conn);
        write_msg(&mut writer, &WireRequest::StopIde)
            .await
            .map_err(|e| SandboxError::Vm(format!("write StopIde: {e}").into()))?;
        let _: WireResponse = tokio::time::timeout(Duration::from_secs(15), read_msg(&mut reader))
            .await
            .map_err(|_| SandboxError::Vm("StopIde timed out".into()))?
            .map_err(|e| SandboxError::Vm(format!("read StopIde response: {e}").into()))?;
        Ok(())
    }

    /// The guest's network identity, asked of agentd and cached. Waits for
    /// the guest: callers are on the boot path and need the real address.
    /// VZ has no netns, so `egress_identity` and `dial_ip` are equal.
    async fn guest_endpoints(&self, id: SandboxId) -> Option<GuestEndpoints> {
        if let Some(live) = self.sandboxes.get(&id) {
            if let Some(ep) = live.guest_endpoints.lock().clone() {
                return Some(ep);
            }
        }
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id)?;
            live.vsock_uds_path.clone()
        };
        let agent_uds = port_uds_path(&vsock_uds_path, ENGRAM_AGENTD_PORT);
        // A hang guard, not a latency bound: the read waits for the guest's
        // agentd, which a cold boot reaches in a few seconds. A bound that
        // races the boot leaves the coordinator with no guest address and
        // the egress proxy with no policy for this VM.
        const GUEST_IP_HANG_GUARD: Duration = Duration::from_secs(30);
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
        let ip_str: Option<String> = tokio::time::timeout(GUEST_IP_HANG_GUARD, fut)
            .await
            .ok()
            .flatten();
        let ip: std::net::Ipv4Addr = ip_str?.parse().ok()?;
        let ep = GuestEndpoints {
            egress_identity: ip,
            dial_ip: ip,
            netns: None,
            vsock_uds: Some(vsock_uds_path),
        };
        if let Some(live) = self.sandboxes.get(&id) {
            *live.guest_endpoints.lock() = Some(ep.clone());
        }
        Some(ep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn protocol_exec(exec_id: &str) -> ExecRequest {
        ExecRequest {
            command: vec!["printf".into(), "partial".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
            exec_id: Some(exec_id.into()),
            stdout_offset: Some(0),
            stderr_offset: Some(0),
            wake: None,
        }
    }

    // Pure protocol tests live beside the existing backend unit tests. They
    // instantiate no VZ objects and require neither a VM nor the
    // virtualization entitlement, so the normal macOS nextest lane runs them.
    #[tokio::test]
    async fn durable_exec_eof_after_partial_output_ends_without_exit() {
        let sandbox_id = SandboxId::new();
        let exec_id = "durable-eof";
        let (host_reader, mut agent_writer) = tokio::io::duplex(4 * 1024);
        let (host_writer, _agent_reader) = tokio::io::duplex(4 * 1024);
        let agent = tokio::spawn(async move {
            write_msg(&mut agent_writer, &WireExecEvent::Started(exec_id.into()))
                .await
                .unwrap();
            write_msg(
                &mut agent_writer,
                &WireExecEvent::Stdout(b"partial-output".to_vec()),
            )
            .await
            .unwrap();
            // Dropping the writer is the transport EOF under test.
        });

        let mut stream = drive_exec_protocol(
            sandbox_id,
            host_reader,
            host_writer,
            protocol_exec(exec_id),
            true,
        )
        .await
        .unwrap()
        .events;
        agent.await.unwrap();

        match stream.next().await {
            Some(ExecEvent::Stdout(bytes)) => assert_eq!(bytes, b"partial-output"[..]),
            other => panic!("expected partial stdout before EOF, got {other:?}"),
        }
        assert!(
            stream.next().await.is_none(),
            "durable transport EOF must not fabricate Exit(None)"
        );
    }

    #[tokio::test]
    async fn durable_exec_refusal_is_forwarded_as_terminal_refusal() {
        let sandbox_id = SandboxId::new();
        let exec_id = "durable-refused";
        let (host_reader, mut agent_writer) = tokio::io::duplex(4 * 1024);
        let (host_writer, _agent_reader) = tokio::io::duplex(4 * 1024);
        let agent = tokio::spawn(async move {
            write_msg(&mut agent_writer, &WireExecEvent::Started(exec_id.into()))
                .await
                .unwrap();
            write_msg(
                &mut agent_writer,
                &WireExecEvent::Refused {
                    reason: "first writer wins".into(),
                },
            )
            .await
            .unwrap();
        });

        let mut stream = drive_exec_protocol(
            sandbox_id,
            host_reader,
            host_writer,
            protocol_exec(exec_id),
            true,
        )
        .await
        .unwrap()
        .events;
        agent.await.unwrap();

        assert!(matches!(
            stream.next().await,
            Some(ExecEvent::Refused(reason)) if reason == "first writer wins"
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn legacy_exec_eof_preserves_exit_none_floor() {
        let sandbox_id = SandboxId::new();
        let exec_id = "legacy-eof";
        let (host_reader, agent_writer) = tokio::io::duplex(4 * 1024);
        let (host_writer, _agent_reader) = tokio::io::duplex(4 * 1024);
        drop(agent_writer);

        let mut stream = drive_exec_protocol(
            sandbox_id,
            host_reader,
            host_writer,
            protocol_exec(exec_id),
            false,
        )
        .await
        .unwrap()
        .events;

        assert!(matches!(stream.next().await, Some(ExecEvent::Exit(None))));
        assert!(stream.next().await.is_none());
    }

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
    fn vsock_uds_path_roots_socket_in_short_sun_len_safe_dir() {
        // Path construction doesn't need a real kernel; we only need
        // the existence check in `new` to pass, so a tempfile is fine.
        let kernel = tempfile::NamedTempFile::new().unwrap();
        let cfg = VzConfig::with_kernel(kernel.path());
        // A deliberately deep work_dir: the whole point is that the socket path
        // does NOT track it (that would overflow SUN_LEN).
        let work = std::env::temp_dir().join("engram-vz-test-paths/a/very/deep/nested/work/dir");
        std::fs::create_dir_all(&work).unwrap();
        let backend = match VzBackend::new(&work, cfg) {
            Ok(b) => b,
            Err(e) => panic!("backend construction failed: {e}"),
        };
        let sid = SandboxId::new();
        let path = backend.vsock_uds_path_for(sid);
        // Rooted in the short SUN_LEN-safe dir (engram_core::socket), NOT under
        // the deep work_dir.
        assert_eq!(
            path.parent().unwrap(),
            engram_core::socket::short_socket_dir()
        );
        assert_ne!(path.parent().unwrap(), work);
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("{sid}.vsock")
        );
        // The property this exists for: the longest per-port bind path we
        // derive stays within SUN_LEN even though work_dir is deep.
        let per_port = port_uds_path(&path, 1030);
        assert!(
            per_port.as_os_str().len() <= engram_core::socket::SUN_PATH_MAX,
            "per-port UDS {} is {}B, over the SUN_LEN cap",
            per_port.display(),
            per_port.as_os_str().len(),
        );
    }
}
