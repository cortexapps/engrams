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
use engram_core::traits::sandbox::{HarnessByteStream, HarnessSink, SandboxBackend, UploadSink};
use engram_core::types::endpoints::GuestEndpoints;
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::{
    AgentSpec, AuxRoDrive, ExecEvent, ExecRequest, ExecStream, SandboxSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::disk::{clone_or_copy, per_sandbox_rootfs_path, SNAPSHOT_ROOTFS_FILENAME};
use crate::vm::{VmConfig, VzVm};
use crate::vsock_bridge::{port_uds_path, VsockBridge, VsockConnector};

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
    /// canonical source is `just pull-kernel`, which fetches
    /// the Kata Containers static kernel.
    pub kernel_path: PathBuf,
    /// Default RAM in MiB applied when `SandboxSpec::memory.max_mib`
    /// is zero or unset. VZ minimum is 128 MiB.
    pub default_memory_mib: u32,
    /// Default vCPU count applied when `SandboxSpec::cpu.vcpus` is
    /// zero or unset.
    pub default_vcpus: u32,
    /// ADR 0061: directory holding content-addressed skill bundles
    /// (`<sha>.erofs`) + the `current.json` stamp — the VZ mirror of the
    /// FC host's `/var/lib/engram/shared`. Set from
    /// `bundles::bundle_dir_from_env()` by the host-agent. Drives whose
    /// `sha256` is `Some` attach `bundle_dir/<sha>.erofs`.
    pub bundle_dir: PathBuf,
}

impl VzConfig {
    pub fn with_kernel(kernel_path: impl Into<PathBuf>) -> Self {
        Self {
            kernel_path: kernel_path.into(),
            default_memory_mib: 512,
            default_vcpus: 1,
            bundle_dir: PathBuf::from(AuxRoDrive::SHARED_DIR),
        }
    }

    /// ADR 0061: point the backend at the host's staged-bundle dir
    /// (`ENGRAM_BUNDLE_DIR` in dev, `/var/lib/engram/shared` in prod).
    pub fn with_bundle_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.bundle_dir = dir.into();
        self
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
    /// vsock bridge tasks (agentd UDS pump + guest-initiated listeners).
    /// Held in a Mutex so `destroy` can take it out and call its async
    /// `stop`. None after stop.
    bridge: parking_lot::Mutex<Option<VsockBridge>>,
    /// Host→guest dialer shared with the bridge's agentd pump. Used by
    /// `open_guest_stream` (ADR 0066 port relay, guest vsock 1030) — one
    /// fresh vsock stream per forwarded browser connection.
    connector: VsockConnector,
    /// `<short-socket-dir>/<sandbox_id>.vsock` — base path (see
    /// `engram_core::socket`; kept short, not under `work_dir`, so the
    /// per-port `_<port>` UDS bind paths stay within SUN_LEN even when
    /// `work_dir` is deep). The vsock bridge binds the `_1024` agentd UDS
    /// listener next to it. Stored on the state so future `start_agent` /
    /// `exec_stream` calls can look up the per-port path without recomputing it.
    vsock_uds_path: PathBuf,
    /// Per-sandbox APFS clone of the bake (or snapshot) rootfs.
    /// Created at `create()` / `restore()` time and removed at
    /// `destroy()`. The clone is what VZ actually attaches; the
    /// originating bake / snapshot file stays intact.
    rootfs_path: PathBuf,
    /// Cached guest-network identity, discovered by querying agentd
    /// over the existing vsock-bridge transport. Populated on first
    /// `guest_endpoints` call (the agent's eth0 takes a moment to come
    /// up after IP_PNP DHCP, so we don't try at create time). Used by
    /// the coordinator's `GET /sessions/:id/shell` proxy to dial
    /// `ttyd` running inside the guest.
    guest_endpoints: Mutex<Option<GuestEndpoints>>,
}

// ADR 0009 §4 (host-side VM supervision) is FC-only by design. The
// rationale for skipping VZ here:
//
//   - VZ VMs run **in-process** as `VZVirtualMachine` ObjC objects
//     hosted by Apple's `Virtualization.framework`. There is no
//     separate VM process whose pid we could poll the way the FC
//     backend does. The VM's lifecycle is the `VzVm` Rust struct's
//     Drop lifecycle.
//   - The way a VZ VM dies "out from under" the host-agent is via
//     internal state transitions surfaced through
//     `VZVirtualMachineDelegate` callbacks
//     (`virtualMachine:didStopWithError:` etc.). Wiring those
//     properly requires creating an ObjC class that conforms to the
//     delegate protocol and threading it through
//     `objc2-virtualization` — significantly more work than the
//     FC poll-based supervisor, and VZ is dev-only.
//   - The bug case §4 is meant to catch (host-agent alive, VM dies
//     unexpectedly) is significantly rarer for VZ. In dev the user
//     restarting `just dev` kills the host-agent and the VM
//     together; in that scenario reconcile's clean-slate startup
//     path handles things correctly via the empty `running_sandboxes`
//     heartbeat.
//
// If/when VZ VM crash detection becomes important (i.e. a sandbox
// goes wedged-but-not-killed and we want eager pruning), the right
// path is a `VZVirtualMachineDelegate` shim. Tracked in
// `docs/state-reconciliation-rollout.md` as a future enhancement.
pub struct VzBackend {
    work_dir: PathBuf,
    cfg: VzConfig,
    sandboxes: DashMap<SandboxId, VzSandboxState>,
    /// Latest harness sink (set by `set_harness_sink`). The vsock
    /// bridge passes guest-initiated 1026 connections to this sink
    /// in the same way `engram-sandbox-firecracker` does.
    harness_sink: Mutex<Option<HarnessSink>>,
    /// ADR 0026: latest artifact-upload sink (set by `set_upload_sink`).
    /// The vsock bridge hands each guest-initiated port-1029 connection to
    /// it — one vsock stream per upload, exactly as
    /// `engram-sandbox-firecracker` serves over vsock.
    upload_sink: Mutex<Option<UploadSink>>,
    /// ADR 0007: when set, `snapshot()` chunks the snapshot rootfs
    /// into this store and populates `SnapshotMetadata.disk_manifest`.
    /// `None` keeps the legacy "rootfs-only-on-host" behavior — the
    /// snapshot remains restorable on this host but can't migrate.
    chunk_store: Option<engram_chunk_store::ChunkStore>,
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
            harness_sink: Mutex::new(None),
            upload_sink: Mutex::new(None),
            chunk_store: None,
        })
    }

    /// Attach a chunk store. Once set, every `snapshot()` chunks
    /// the rootfs clone into the store and the returned metadata
    /// carries the `disk_manifest` ref. Required for cross-host
    /// resume / spot-preemption migration.
    pub fn with_chunk_store(mut self, chunk_store: engram_chunk_store::ChunkStore) -> Self {
        self.chunk_store = Some(chunk_store);
        self
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn config(&self) -> &VzConfig {
        &self.cfg
    }

    fn vsock_uds_path_for(&self, id: SandboxId) -> PathBuf {
        // vsock UDS bind paths are capped at SUN_LEN (~104B on macOS). `work_dir`
        // can be deep (a git-worktree checkout, a long $HOME), and the per-port
        // `<id>.vsock_<port>` filename adds ~47B, so root the socket in a short
        // /tmp dir rather than under `work_dir`. See `engram_core::socket`.
        let dir = engram_core::socket::short_socket_dir();
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{id}.vsock"))
    }

    /// ADR 0080: resolve the symbolic stamped slots against this host's
    /// staged stamp (`<bundle_dir>/current.json`) — the VZ mirror of the FC
    /// cold-boot resolution. The agentd slot (`AGENTD_SLOT_INDEX`, key
    /// `agentd`) is HARD: the guest's stage-1 init execs agentd out of that
    /// mount, so a cold boot without it can't come up. The guest-tools slot
    /// (`GUEST_TOOLS_SLOT_INDEX`, key `guest-tools`, ADR 0080 §D) is SOFT:
    /// a missing key warns and the slot stays symbolic (skipped at attach;
    /// the SHELL tab then needs an image-baked ttyd). A no-op when no
    /// symbolic stamped slot is present (resolved specs, bundle-less test
    /// specs).
    fn resolve_agentd_slot(&self, drives: &mut [AuxRoDrive]) -> Result<(), SandboxError> {
        let needs_agentd = drives
            .iter()
            .any(|d| d.sha256.is_none() && d.slot_index() == Some(AuxRoDrive::AGENTD_SLOT_INDEX));
        let needs_guest_tools = drives.iter().any(|d| {
            d.sha256.is_none() && d.slot_index() == Some(AuxRoDrive::GUEST_TOOLS_SLOT_INDEX)
        });
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
            // Hard: the guest's stage-1 init execs agentd out of this mount,
            // so a cold boot without it can't come up.
            let sha = stamp.get(AuxRoDrive::AGENTD_STAMP_KEY).ok_or_else(|| {
                SandboxError::InvalidSpec(format!(
                    "bundle stamp {} carries no `{}` entry — restage bundles \
                     (`just bundles-vz`)",
                    stamp_path.display(),
                    AuxRoDrive::AGENTD_STAMP_KEY,
                ))
            })?;
            for d in drives.iter_mut() {
                if d.sha256.is_none() && d.slot_index() == Some(AuxRoDrive::AGENTD_SLOT_INDEX) {
                    d.sha256 = Some(sha.clone());
                }
            }
        }
        if needs_guest_tools {
            // Soft (ADR 0080 §D): guest-tools carries the SHELL-tab ttyd, not
            // anything boot-critical. A stamp without it warns loudly and
            // leaves the slot symbolic — VZ's attach skips unresolved slots,
            // and agentd falls back to an image-baked ttyd at StartShell.
            match stamp.get(AuxRoDrive::GUEST_TOOLS_STAMP_KEY) {
                Some(sha) => {
                    for d in drives.iter_mut() {
                        if d.sha256.is_none()
                            && d.slot_index() == Some(AuxRoDrive::GUEST_TOOLS_SLOT_INDEX)
                        {
                            d.sha256 = Some(sha.clone());
                        }
                    }
                }
                None => tracing::warn!(
                    stamp = %stamp_path.display(),
                    "bundle stamp carries no `{}` entry — the SHELL tab only \
                     works if the image bakes ttyd; restage bundles \
                     (`just bundles-vz`)",
                    AuxRoDrive::GUEST_TOOLS_STAMP_KEY,
                ),
            }
        }
        Ok(())
    }

    /// ADR 0007 Phase 6: per-snapshot staging dir, owned by the
    /// backend. Lives under `<work_dir>/snapshots/<id>/` rather
    /// than the per-sandbox dir, so destroy(sandbox) doesn't
    /// take its snapshots with it.
    fn snapshot_dir_for(&self, snapshot_id: SnapshotId) -> PathBuf {
        self.work_dir
            .join("snapshots")
            .join(snapshot_id.to_string())
    }

    /// Ask the in-guest agentd to `sync(2)` so dirty page-cache writes
    /// land in the virtio-blk-backed rootfs file before `snapshot()`
    /// clones it. Best-effort + bounded: a 5 s cap keeps a wedged guest
    /// from stalling the snapshot indefinitely (the dial gets its own
    /// vsock stream to agentd's port 1024, so it doesn't queue behind an
    /// in-flight exec). On any failure we log and proceed — the clone then
    /// reflects the last ext4 commit, the pre-existing behaviour.
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

    /// ADR 0061: shared cold-resume path. `mounts_override = Some` (fresh
    /// create) replaces the snapshot's reserved/sentinel slots with this
    /// session's resolved skills; `None` (resume) keeps the snapshot's
    /// pinned `spec.aux_ro_drives`.
    async fn restore_impl(
        &self,
        metadata: SnapshotMetadata,
        mounts_override: Option<Vec<AuxRoDrive>>,
    ) -> Result<SandboxId, SandboxError> {
        let src = self.snapshot_dir_for(metadata.id);
        let manifest = crate::snapshot::read_manifest(&src).await?;
        let mut spec = manifest.spec;
        if let Some(mounts) = mounts_override {
            spec.aux_ro_drives = mounts;
        }
        // ADR 0080/0096: re-resolve symbolic stamped slots against this
        // host's CURRENT stamp, exactly like create() — the manifest may
        // carry `sha256 = None` slots (a backend-level spec resolves at
        // attach, and create() never writes the resolution back), and an
        // unresolved slot is skipped at attach, which cold-boots a guest
        // with no agentd bundle → the init shim panics the kernel. A
        // no-op for coordinator-resolved specs (sha already pinned); for
        // symbolic ones this is also the honest resume semantic — VZ's
        // cold boot picks up the host's current agentd generation, the
        // cold-boot analogue of FC's post-resume RefreshAgent.
        self.resolve_agentd_slot(&mut spec.aux_ro_drives)?;
        let snapshot_rootfs = spec.rootfs_source.clone().ok_or_else(|| {
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

        let new_id = SandboxId::new();
        tokio::fs::create_dir_all(&self.work_dir).await?;
        let rootfs_path = per_sandbox_rootfs_path(&self.work_dir, new_id);

        // Clone the snapshot's rootfs into a fresh per-sandbox file. The
        // snapshot's clone stays intact (so a forked session or a re-resume
        // after this one can clone it again); the new sandbox writes only to
        // its own clone.
        clone_or_copy(&snapshot_rootfs, &rootfs_path).await?;
        tracing::debug!(
            sandbox_id = %new_id,
            src = %snapshot_rootfs.display(),
            dst = %rootfs_path.display(),
            "vz: cloned snapshot rootfs to fresh per-sandbox path for cold-resume"
        );

        let vm_cfg = VmConfig::new(
            self.cfg.kernel_path.clone(),
            rootfs_path.clone(),
            memory_mib,
            vcpus,
        )
        .with_aux_ro_drives(spec.aux_ro_drives.clone(), self.cfg.bundle_dir.clone());
        let vm = VzVm::new(vm_cfg)?;
        if let Err(e) = vm.start().await {
            let _ = tokio::fs::remove_file(&rootfs_path).await;
            return Err(e.into());
        }
        let vm = Arc::new(vm);

        let vsock_uds_path = self.vsock_uds_path_for(new_id);

        let harness_sink = self.harness_sink.lock().clone();
        let upload_sink = self.upload_sink.lock().clone();
        let (bridge, connector) = VsockBridge::start(
            vm.raw_clone(),
            vm.queue_clone(),
            vsock_uds_path.clone(),
            harness_sink,
            upload_sink,
        )
        .await
        .map_err(SandboxError::from)?;

        self.sandboxes.insert(
            new_id,
            VzSandboxState {
                spec,
                vm,
                bridge: parking_lot::Mutex::new(Some(bridge)),
                connector,
                vsock_uds_path,
                rootfs_path,
                guest_endpoints: Mutex::new(None),
            },
        );
        Ok(new_id)
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
    fn bundle_dir(&self) -> &std::path::Path {
        // Same dir VZ stages + attaches `<sha>.erofs` from, so the heartbeat
        // reports exactly what restore will attach (ADR 0062).
        &self.cfg.bundle_dir
    }

    fn bundle_file_ext(&self) -> &'static str {
        // VZ stages + attaches erofs (see `staged_erofs_path`); the Kata guest
        // kernel mounts erofs, not squashfs. The BundleStore must materialize/
        // sweep `<sha>.erofs`, not the FC-default `<sha>.squashfs`.
        "erofs"
    }

    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        warn_vz_ignores_allow_hosts_once(&spec.network);
        let bake_rootfs = spec.rootfs_source.clone().ok_or_else(|| {
            SandboxError::InvalidSpec(
                "VzBackend requires SandboxSpec.rootfs_source — point it at the ext4 \
                 rootfs produced by `just bake-demo`"
                    .into(),
            )
        })?;
        // Validate up front rather than letting VZ surface a less
        // specific NSError later.
        if !bake_rootfs.exists() {
            return Err(SandboxError::InvalidSpec(format!(
                "vz rootfs not found at {} — bake an image with `just bake-demo` and \
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

        // ADR 0080: resolve the (symbolic) agentd slot against this host's
        // staged stamp BEFORE building the VM config. The cold-booting
        // guest's stage-1 init copies agentd out of this mount and execs
        // it — no agentd is baked into the rootfs — so unlike the sentinel
        // slots (which VZ deliberately skips) this one must attach.
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

        let vm_cfg = VmConfig::new(
            self.cfg.kernel_path.clone(),
            rootfs_path.clone(),
            memory_mib,
            vcpus,
        )
        // ADR 0061: attach this spec's skill bundles. During base-snapshot
        // capture these are sentinel placeholders (sha = None) and attach
        // nothing (except the agentd slot, resolved above); a plain
        // cold-create with resolved drives attaches them.
        .with_aux_ro_drives(aux_ro_drives, self.cfg.bundle_dir.clone());
        let vm = VzVm::new(vm_cfg)?;

        // Start the VM; if start fails, drop the VM via the early
        // return (no half-registered state in the sandboxes map).
        // Also clean up the per-sandbox rootfs we just cloned.
        if let Err(e) = vm.start().await {
            let _ = tokio::fs::remove_file(&rootfs_path).await;
            return Err(e.into());
        }
        let vm = Arc::new(vm);

        let vsock_uds_path = self.vsock_uds_path_for(id);

        // Wire up the real-vsock bridge (ADR 0066 Phase 2). This binds the
        // <vsock_uds>_1024 agentd UDS immediately so a subsequent
        // start_agent / exec_stream dial can't race a not-yet-bound
        // window, and registers the guest-initiated listeners (harness
        // 1026, upload 1029, ready 1027). The returned connector serves
        // the port relay (guest vsock 1030) via `open_guest_stream`.
        let harness_sink = self.harness_sink.lock().clone();
        let upload_sink = self.upload_sink.lock().clone();
        let (bridge, connector) = VsockBridge::start(
            vm.raw_clone(),
            vm.queue_clone(),
            vsock_uds_path.clone(),
            harness_sink,
            upload_sink,
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
                vm,
                bridge: parking_lot::Mutex::new(Some(bridge)),
                connector,
                vsock_uds_path,
                rootfs_path,
                guest_endpoints: Mutex::new(None),
            },
        );
        Ok(id)
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        // ADR 0015 M1: one in-VM service, plain connect. The vsock
        // bridge binds the host-side agentd UDS in `VsockBridge::start`
        // (before `create`/`restore` returns), so `UnixStream::connect`
        // returns a live stream immediately regardless of whether agentd
        // in the guest has bound its vsock port yet — the wait happens
        // naturally at the connectToPort dial-with-retry + the first read,
        // which blocks until agentd writes the SpawnHarness response. No
        // host-side retry loop, no deadline knob; the FC path's boot-race
        // problem doesn't exist here.
        //
        // VZ also doesn't use the option-D harness-late-bind path —
        // its in-VM mount story is handled by engram-init via the
        // VZ disk-attach config. Leave harness_{dev,mount} unset.
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

        // 2026-07 core-ops fold: this frame also carries the per-host
        // egress-proxy CA (ADR 0021 P1) — agentd installs it before
        // spawning. Previously VZ built this request from
        // argv/env/session_env only and never referenced
        // `agent.host_ca_pem`, so a VZ host running an egress proxy
        // silently delivered no CA to the guest; passing it through
        // here fixes that for free (VZ exercises the identical
        // agentd code path as FC).
        // ADR 0067: stamp the attach token into the harness child env —
        // the backend is the only party that knows the sandbox id
        // pre-boot; the epoch was minted coordinator-side into the spec.
        let token_env = agent.attach_token_env(id);
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
            engram_agentd::WireResponse::Error { kind, message } => Err(SandboxError::Vm(
                format!("SpawnHarness rejected ({kind}): {message}").into(),
            )),
            other => Err(SandboxError::Vm(
                format!("SpawnHarness: unexpected response: {other:?}").into(),
            )),
        }
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
        // ADR 0015 M2: `Active` now actually means "agentd is bound
        // and responsive to RPC" (the coord layer doesn't transition
        // a row to Active until `start_agent` returns OK). exec_stream
        // is only reachable from a handler that gated on Active, so
        // agentd is provably listening on vsock 1024 by the time we
        // get here — no boot-race window to retry through. A failed
        // connect now reflects a real fault (process crashed, vsock
        // tunnel torn down) that retrying wouldn't recover.
        let conn = UnixStream::connect(&agent_uds).await.map_err(|e| {
            SandboxError::Vm(
                format!("connect engram-agentd UDS {}: {e}", agent_uds.display()).into(),
            )
        })?;
        let (reader, writer) = tokio::io::split(conn);
        drive_exec_protocol(id, reader, writer, cmd).await
    }

    /// ADR 0066 Phase 2: dial the in-guest agentd relay on `port` (the
    /// host-agent calls this with `PROXY_PORT_VSOCK_PORT` = 1030 per
    /// forwarded browser connection) and hand back the connected vsock
    /// stream. The host-agent then writes the `RelayConnect` header, reads
    /// the `RelayAck`, and splices bytes — reaching a dev server bound to
    /// the guest's `127.0.0.1` (Vite, the Tilt UI, `next dev`) that the old
    /// `dial_ip`/eth0 dial can't. Each call is its own `connectToPort`
    /// stream, and `VZVirtioSocketDevice` muxes them freely, so a
    /// persistent forwarded connection can't head-of-line block others —
    /// the parity fix over the retired single-stream-per-port console
    /// bridge. Returning `Some` here flips VZ off the trait-default
    /// `None`/`guest_endpoints` fallback and onto the relay path.
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

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let (vm, spec, rootfs_path, vsock_uds_path) = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            (
                live.vm.clone(),
                live.spec.clone(),
                live.rootfs_path.clone(),
                live.vsock_uds_path.clone(),
            )
        };

        // Flush the guest filesystem BEFORE pausing + cloning. The clone
        // captures only on-disk state (cold-boot restore, no memory image),
        // so any write still in the guest's page cache would be silently
        // lost from the snapshot — unlike FC, whose memory snapshot carries
        // the dirty pages. Ask agentd to `sync(2)` while the VM is still
        // running so the bytes land in the virtio-blk-backed rootfs file the
        // clone is about to copy. Best-effort: a flush failure (agent not
        // up, slow boot) shouldn't abort the snapshot — we fall back to the
        // last ext4 commit, same as before this call existed.
        self.flush_guest_fs(id, &vsock_uds_path).await;

        // ADR 0007 Phase 6: allocate snapshot id + derive staging
        // dir from it. Coord no longer dictates layout.
        let snapshot_id = SnapshotId::new();
        let dest = self.snapshot_dir_for(snapshot_id);
        tokio::fs::create_dir_all(&dest).await.map_err(|e| {
            SandboxError::Snapshot(format!("create snapshot dir {}: {e}", dest.display()))
        })?;
        let dest = dest.as_path();

        // Clone-based snapshot semantics. VZ's
        // `restoreMachineStateFromURL` is broken upstream for
        // arm64 Linux guests on macOS (see UTM #6654, Apple
        // Developer Forum thread 745168, and Apple's own
        // `containerization` framework which avoids it entirely
        // — sub-second cold-boot is the canonical path). With the
        // guest FS already flushed above, we pause the VM (freeze
        // vCPUs so no new writes race the clone), APFS-clone the
        // per-sandbox rootfs into the snapshot dir, and resume.
        // The clone IS the snapshot — restore re-clones it back
        // to a fresh per-sandbox file and cold-boots a new VM.
        // agentd's harness supervisor pattern + Claude's
        // `--resume <session-id>` (persisted on the rootfs at
        // /workspace/.engrams/claude-session-id) recover
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

        // ADR 0007: when a chunk store is wired, chunk the cloned
        // rootfs into the store so the snapshot is restorable on
        // any host (the manifest_ref + the bytes in BlobStorage are
        // sufficient). The host-local clone stays alongside as the
        // hot-path restore source — Phase 6 retires the clone once
        // restore takes a ManifestRef natively.
        let disk_manifest = if let Some(cs) = self.chunk_store.as_ref() {
            tracing::debug!(
                sandbox_id = %id,
                rootfs = %snapshot_rootfs.display(),
                "chunking snapshot rootfs into store",
            );
            let m = cs
                .chunk_file(
                    &snapshot_rootfs,
                    engram_chunk_store::ManifestKind::Disk,
                    None,
                )
                .await
                .map_err(|e| SandboxError::Snapshot(format!("chunk snapshot rootfs: {e}")))?;
            let mref = engram_core::types::manifest::ManifestRef::new();
            cs.put_manifest(mref, &m)
                .await
                .map_err(|e| SandboxError::Snapshot(format!("put snapshot manifest: {e}")))?;
            tracing::info!(
                sandbox_id = %id,
                manifest = %mref,
                chunks = m.chunks.len(),
                total_bytes = m.total_bytes,
                "snapshot rootfs chunked",
            );
            Some(mref)
        } else {
            None
        };

        crate::snapshot::build_metadata(dest, &spec.image, disk_manifest, snapshot_id).await
    }

    fn snapshot_path_for(&self, snapshot_id: SnapshotId) -> PathBuf {
        self.snapshot_dir_for(snapshot_id)
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        // Resume: keep the snapshot's pinned aux drives (manifest.spec).
        self.restore_impl(metadata, None).await
    }

    async fn restore_fresh(
        &self,
        metadata: SnapshotMetadata,
        selected_mounts: Vec<AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        // ADR 0061: fresh session create — the base snapshot is skill-
        // agnostic; bind this session's resolved skills into the VM's aux
        // drives. The PooledBackend calls this for `fresh == true`
        // (pooled_backend.rs restore_with); `selected_mounts` carry
        // `sha256 = Some` resolved against the host's current generation.
        self.restore_impl(metadata, Some(selected_mounts)).await
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

    /// Ensure the in-guest `ttyd` is running, returning the port it
    /// accepted on. Forwards to agentd's `StartShell` RPC (which lazily
    /// spawns ttyd and only replies once a loopback probe succeeds) —
    /// the same contract the FC backend implements. Without this
    /// override VZ would inherit the trait default `Ok(7681)`, which
    /// promises a listener that nothing started: the coordinator's
    /// `proxy_shell` then dials the guest IP and hits `connection
    /// refused` (the SHELL tab never opens). agentd carries the ttyd
    /// binary + the StartShell handler in every bake, so this is a
    /// host-side-only change.
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
        // `port: None` → agentd's default (7681). Bound the round-trip:
        // ttyd spawn + the in-guest readiness probe are normally
        // sub-second, so 30 s is generous headroom without hanging a
        // wedged guest forever.
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

    /// ADR 0065: ask agentd to ensure the in-guest browser stack (Xvfb +
    /// openbox + chromium + x11vnc) is running and x11vnc is accepting on its
    /// port. Returns the bound port plus agentd's optional chromium-CDP
    /// liveness warning (issue #569). Mirrors [`Self::start_shell`]: the
    /// host's `proxy_vnc` (P1.4) calls this just before dialing the guest's
    /// raw-TCP VNC port, so the connect finds a listener.
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
        // `port: None` → agentd's default (5900). Worst-case serial path
        // inside agentd's start_browser post issue #569's mutex-hold/
        // CDP-probe fixes: up to a 2s RFB-banner-read timeout (issue #567's
        // wedge detection) + ~0.3s force-stop grace + up to 20s
        // (READY_DEADLINE) for the launcher's `--ensure` to bring x11vnc up
        // + up to a 1s fast CDP probe — call it ~24s worst case (the
        // fresh-spawn CDP watch itself runs off-path in a detached
        // background task and never blocks this reply). 45s here still
        // comfortably covers it.
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

    /// ADR 0065: tear down the in-guest browser stack. Idempotent — a no-op
    /// when the sandbox is gone or nothing is running.
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

    /// ADR 0085: ask agentd to ensure the in-guest IDE (code-server) is
    /// running and answering `/healthz` on its loopback HTTP port. Mirrors
    /// [`Self::start_browser`]: the coordinator's `ensure_ide` calls this
    /// just before the orchestrator relays to the guest's port, so the
    /// dial finds a server.
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
        // `port: None` → agentd's default (13337). Worst-case serial path
        // inside agentd's start_ide: up to a 2s /healthz probe timeout
        // (issue #567's wedge detection) + ~0.3s force-stop grace + up to
        // 20s (READY_DEADLINE) for the launcher's `--ensure` to bring
        // code-server up — call it ~23s worst case. 45s here still
        // comfortably covers it (StartBrowser parity).
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

    /// ADR 0085: tear down the in-guest IDE. Idempotent — a no-op when the
    /// sandbox is gone or nothing is running.
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

    /// Discover the guest's network identity by asking agentd over
    /// the vsock-bridge. First successful answer is cached on the
    /// per-sandbox state; subsequent calls are O(1) memory reads.
    /// Returns `None` if the agent isn't reachable yet (e.g. shell
    /// requested before bootstrap completes), reports no non-loopback
    /// address, or reports an address that doesn't parse as IPv4.
    /// VZ has no netns indirection, so `egress_identity` and
    /// `dial_ip` are always the same value.
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
        // Bound the round-trip — an agent that doesn't understand the
        // GuestIp verb (e.g. an older bake) would otherwise leave the read
        // hanging. 2s is plenty for a healthy in-guest round trip and short
        // enough that a dashboard SHELL-tab click sees a prompt 503.
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
        let ip_str: Option<String> = tokio::time::timeout(Duration::from_secs(2), fut)
            .await
            .ok()
            .flatten();
        // Not cached: today agentd only ever answers via
        // `read_primary_ipv4()`, which can't produce a non-IPv4
        // string, so this is unreachable in practice. If that ever
        // changes, a parse failure re-pays the full 2s vsock
        // round-trip on every subsequent call instead of failing
        // fast from a cached negative.
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
