//! Production [`SandboxBackend`] driving [Firecracker][firecracker]
//! microVMs over its HTTP-over-Unix-socket API.
//!
//! [firecracker]: https://github.com/firecracker-microvm/firecracker
//!
//! # Status
//!
//! All `SandboxBackend` methods are wired:
//!
//! - `create`/`destroy`/`list`/`snapshot`/`restore` go through real
//!   Firecracker microVMs (verified by `tests/lifecycle.rs`,
//!   `tests/snapshot.rs`).
//! - `exec_stream` connects to the in-guest `engram-agentd` via
//!   Firecracker's vsock proxy (the host UDS at
//!   `<work_dir>/<sandbox_id>.vsock`, with a `CONNECT <port>\n`
//!   handshake), sends a `WireExecRequest`, and translates the
//!   streamed `WireExecEvent`s back into `engram_core::ExecEvent`s.
//!
//! `restore` supports both `RestoreMode::File` (synchronous read of
//! memory.bin, simple, slow, no extra processes) and `RestoreMode::Uffd`
//! (lazy paging via the `engram-uffd-handler` companion process — fast,
//! Linux-only). `FirecrackerConfig::restore_mode` defaults to `File`;
//! set it to `Uffd` for production-grade eviction/resume latency. Both
//! are verified by `tests/snapshot.rs` and `tests/snapshot_uffd.rs`.
//!
//! End-to-end `tests/exec_real_vm.rs` bakes a debian-slim rootfs with
//! a static-musl `engram-agentd` injected at `/sbin/engram-agentd`,
//! boots it under FC with `init=/sbin/engram-init`, and round-trips
//! an exec through the agent over vsock — the whole `SandboxBackend`
//! contract works against a real microVM.
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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use engram_agentd::{read_msg, write_msg, WireExecEvent, WireExecRequest, WireRequest};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::{ExecEvent, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub mod client;
pub mod net;

pub use client::{
    ActionType, BootSource, DriveConfig, FirecrackerClient, MachineConfig, NetworkInterface,
    SnapshotPaths, VmState, VsockConfig,
};

/// Per-sandbox state owned by the host agent: the spec it was launched
/// with, the path to its Firecracker control socket, and the path to
/// the rootfs.ext4 image that was attached. `vsock_cid` is the context
/// ID assigned to the guest's virtio-vsock device; `vsock_uds_path` is
/// the host-side Unix socket Firecracker proxies vsock traffic through.
/// To reach the guest's listener on `port`, the host opens
/// `vsock_uds_path` and writes `CONNECT <port>\n`; FC replies
/// `OK <peer_port>\n` and the bytes after that are the guest stream.
#[derive(Clone, Debug)]
pub struct SandboxState {
    pub spec: SandboxSpec,
    pub firecracker_socket: PathBuf,
    pub rootfs_path: PathBuf,
    pub vsock_cid: u32,
    pub vsock_uds_path: PathBuf,
}

/// Reserved vsock port `engram-agentd` listens on inside the guest.
pub const ENGRAM_AGENTD_PORT: u32 = 1024;

/// Filename FC uses for the per-port host-side UDS when the guest
/// dials out via vsock: `<base>_<port>`. Extracted so callers
/// (here for harness 1026) and FC's own filename convention stay
/// in sync. See firecracker/docs/vsock.md for the protocol.
fn harness_uds_for(base_vsock_uds: &Path) -> PathBuf {
    let port = engram_harness_proto::HARNESS_VSOCK_PORT;
    let file_name = base_vsock_uds
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = base_vsock_uds.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{file_name}_{port}"))
}

/// Lowest CID we'll hand out to a guest. CIDs 0/1/2 are reserved
/// (hypervisor / loopback / host); user-allocatable starts at 3.
const FIRST_GUEST_CID: u32 = 3;

/// How long `destroy` waits for the guest to honour SendCtrlAltDel
/// before escalating to SIGKILL. A healthy debian-slim/ubuntu rootfs
/// halts within ~1s; 3s leaves room for the page-cache drain at
/// snapshot time without making a single destroy feel slow.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

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
    /// Path to the UFFD handler binary (engram-uffd-handler). Used
    /// when `restore_mode == Uffd`. Defaults to `engram-uffd-handler`
    /// (resolved via PATH).
    pub uffd_handler_bin: PathBuf,
    /// How to wire memory on snapshot restore. File mode synchronously
    /// reads memory.bin (slow, simple, no extra processes). Uffd mode
    /// spawns engram-uffd-handler and serves pages on demand (fast,
    /// requires Linux + the handler binary on the host).
    pub restore_mode: RestoreMode,
    /// Engram CIDR pool — every sandbox gets a unique /30 carved out
    /// of this. `Some(10.200.0.0)` (the default) provisions per-VM
    /// TAPs + iptables rules; production deployments always want
    /// this. `None` skips all networking provisioning, leaving the
    /// guest with no `eth0`. Used by the unprivileged integration
    /// tests (`tests/lifecycle.rs` etc.) which can't `ip tuntap add`
    /// without `CAP_NET_ADMIN`. Override via `--fc-net-cidr`.
    pub net_pool: Option<std::net::Ipv4Addr>,
    /// TCP port the host-side egress proxy listens on. When `Some`,
    /// `host_startup` REDIRECTs VM→tcp/443 to this port and applies
    /// a default-deny on FORWARD so the proxy is the only egress
    /// path. When `None` (test/dev), VMs get open egress with the
    /// standard hard-isolation drops.
    pub egress_proxy_port: Option<u16>,
}

/// Backing-memory strategy for `restore`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestoreMode {
    /// `mem_backend = { backend_type: "File", ... }`. Synchronous —
    /// Firecracker reads the entire memory file before InstanceStart.
    File,
    /// `mem_backend = { backend_type: "Uffd", ... }`. Lazy — pages
    /// stream in on demand via a `engram-uffd-handler` process the
    /// backend spawns alongside firecracker.
    Uffd,
}

impl FirecrackerConfig {
    /// Convenience constructor for production wiring: just the kernel
    /// path; everything else uses defaults.
    ///
    /// The default boot args include `init=/sbin/engram-init` because
    /// every Engram-baked image ships the init shim that exec's
    /// `engram-agentd` on vsock — without it the host can't reach
    /// the guest. Override `default_boot_args` after construction if
    /// you're booting an image that handles agent launch differently.
    pub fn with_kernel(kernel_image_path: impl Into<PathBuf>) -> Self {
        Self {
            net_pool: Some("10.200.0.0".parse().unwrap()),
            egress_proxy_port: None,
            kernel_image_path: kernel_image_path.into(),
            default_boot_args: "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init"
                .into(),
            firecracker_bin: PathBuf::from("firecracker"),
            uffd_handler_bin: PathBuf::from("engram-uffd-handler"),
            restore_mode: RestoreMode::File,
        }
    }
}

/// Live sandbox handle. Keeps the spawned firecracker `Child` so
/// `destroy` can SIGKILL it; `kill_on_drop` is a backstop in case a
/// `LiveSandbox` is dropped without going through `destroy` (panic).
/// `uffd_handler` is `Some` only for sandboxes restored via
/// `RestoreMode::Uffd`; it lives as long as the VM does and serves
/// page faults from memory.bin.
struct LiveSandbox {
    state: SandboxState,
    child: Child,
    uffd_handler: Option<Child>,
    /// Per-VM /30 + iptables chain + TAP. Stashed so `destroy` can
    /// release the slot back to the allocator and yank exactly its
    /// own iptables rules. `None` if networking failed to provision
    /// at create time — those sandboxes are destroyed before
    /// `LiveSandbox` is constructed, but the field is `Option` for
    /// the symmetry with the `state` rebuild path.
    net: Option<net::NetSetup>,
    /// Cached IPv4 address discovered by querying agentd on first
    /// `guest_ip` call (mirrors VZ's pattern). Populated lazily
    /// because the agent's eth0 needs IP_PNP DHCP+kernel boot before
    /// it can answer.
    guest_ip: parking_lot::Mutex<Option<String>>,
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
    /// Monotonic CID allocator. Each `create` bumps this. We don't
    /// reuse CIDs of destroyed VMs — a u32 gives us 4 billion before
    /// wrap, which is fine for any single host's lifetime.
    next_cid: AtomicU32,
    /// Per-host /30 allocator. Wrapped in a Mutex so concurrent
    /// `create` calls don't hand out the same slot. Recycle on
    /// `destroy` keeps the address space dense.
    net_allocator: Arc<parking_lot::Mutex<net::NetworkAllocator>>,
    /// Sink for inbound harness connections. Set by the coord at
    /// startup via `set_harness_sink`. `None` until then; if a
    /// guest dials before set, the connection is closed (the sink
    /// is what feeds the HarnessHub, so without it we can't route).
    /// Wrapped in an `Arc` so per-sandbox accept loops clone the
    /// pointer and read the current sink on each accept — a
    /// `set_harness_sink` call updates them all atomically.
    harness_sink: Arc<parking_lot::RwLock<Option<engram_core::traits::HarnessSink>>>,
}

impl FirecrackerBackend {
    pub fn new(work_dir: impl Into<PathBuf>, config: FirecrackerConfig) -> Self {
        // Allocator over the configured pool; falls back to a
        // throwaway 0.0.0.0 pool when networking is disabled (the
        // allocator is created but never consulted in that mode).
        let pool = config
            .net_pool
            .unwrap_or_else(|| "0.0.0.0".parse().unwrap());
        let net_allocator = Arc::new(parking_lot::Mutex::new(net::NetworkAllocator::new(pool)));
        Self {
            work_dir: work_dir.into(),
            config,
            sandboxes: DashMap::new(),
            next_cid: AtomicU32::new(FIRST_GUEST_CID),
            net_allocator,
            harness_sink: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Apply once-per-host networking setup: enable IP forwarding,
    /// install the inter-VM block rule. Idempotent — safe to call
    /// from a coordinator restart. No-op when `config.net_pool` is
    /// None (tests / disabled-networking deployments).
    pub async fn host_startup(&self) -> Result<(), SandboxError> {
        if self.config.net_pool.is_none() {
            return Ok(());
        }
        net::host_startup(self.config.egress_proxy_port)
            .await
            .map_err(SandboxError::from)
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

    /// Connect to a running `engram-agentd` at `agent_socket`, send a
    /// `WireExecRequest`, and turn the resulting stream of
    /// `WireExecEvent`s into an `ExecStream` of the trait's
    /// `engram_core::ExecEvent`s.
    ///
    /// Production callers go through `<Self as SandboxBackend>::exec_stream`,
    /// which derives `agent_socket` from the sandbox's vsock UDS. This
    /// associated function is `pub` only so the `tests/exec.rs`
    /// integration test (and any future test that wants to exercise
    /// the wire protocol against an agent without booting a microVM)
    /// can drive it directly. Don't call it from the host-agent.
    pub async fn exec_stream_via_agent_socket(
        sandbox_id: SandboxId,
        agent_socket: &Path,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let conn = UnixStream::connect(agent_socket).await.map_err(|e| {
            vm_err(format!(
                "connect to agent at {}: {e}",
                agent_socket.display()
            ))
        })?;
        let (reader, writer) = tokio::io::split(conn);
        drive_exec_protocol(sandbox_id, reader, writer, cmd).await
    }

    /// Connect to the in-guest agent over Firecracker's vsock proxy.
    /// The host UDS at `vsock_uds_path` is multiplexed: every host→
    /// guest connection sends `CONNECT <port>\n` first and reads back
    /// `OK <peer_port>\n`. Only AFTER the handshake is the byte stream
    /// connected to the guest's listener on `port`. Documented at
    /// `firecracker/docs/vsock.md`.
    async fn exec_stream_via_fc_vsock(
        sandbox_id: SandboxId,
        vsock_uds_path: &Path,
        port: u32,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let conn = Self::connect_fc_vsock(vsock_uds_path, port).await?;
        let (reader, writer) = tokio::io::split(conn);
        drive_exec_protocol(sandbox_id, reader, writer, cmd).await
    }

    /// Open the host UDS at `vsock_uds_path`, write `CONNECT <port>\n`,
    /// read back `OK <peer>\n`, and return the resulting stream now
    /// directly connected to the guest's listener on `port`. Shared
    /// between exec (port 1024) and start_agent (port 1025 for
    /// bootstrap).
    async fn connect_fc_vsock(
        vsock_uds_path: &Path,
        port: u32,
    ) -> Result<UnixStream, SandboxError> {
        let mut conn = UnixStream::connect(vsock_uds_path).await.map_err(|e| {
            vm_err(format!(
                "connect to FC vsock UDS {}: {e}",
                vsock_uds_path.display()
            ))
        })?;

        conn.write_all(format!("CONNECT {port}\n").as_bytes())
            .await
            .map_err(|e| vm_err(format!("send CONNECT to FC vsock: {e}")))?;

        // Read exactly one line, byte-by-byte, so we don't over-read
        // and lose bytes the guest has already sent on the now-
        // connected stream.
        let mut line = Vec::with_capacity(32);
        let mut byte = [0u8; 1];
        loop {
            conn.read_exact(&mut byte)
                .await
                .map_err(|e| vm_err(format!("read FC vsock CONNECT response: {e}")))?;
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
            if line.len() > 64 {
                return Err(vm_err(
                    "FC vsock CONNECT response exceeded 64 bytes; protocol mismatch",
                ));
            }
        }
        let line_str = std::str::from_utf8(&line)
            .map_err(|e| vm_err(format!("non-UTF8 FC vsock response: {e}")))?;
        if !line_str.starts_with("OK ") {
            return Err(vm_err(format!(
                "FC vsock refused CONNECT (expected `OK <port>\\n`, got {line_str:?})"
            )));
        }
        Ok(conn)
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

    /// Spawn `engram-uffd-handler --listen <uffd_uds> --memory-bin <mem>`
    /// and wait until it's listening. Stdout/stderr go into the jail
    /// dir's `uffd-handler.log` so a snapshot-restore failure has a
    /// recoverable diagnostic. Returns the live `Child` so the caller
    /// can hold it for the VM's lifetime.
    async fn spawn_uffd_handler(
        &self,
        uffd_uds: &Path,
        memory_bin: &Path,
        jail_dir: &Path,
    ) -> Result<Child, SandboxError> {
        let log_path = jail_dir.join("uffd-handler.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|e| vm_err(format!("open uffd handler log {}: {e}", log_path.display())))?;
        let log_clone = log
            .try_clone()
            .map_err(|e| vm_err(format!("dup uffd-handler log fd: {e}")))?;

        let mut child = Command::new(&self.config.uffd_handler_bin)
            .args([
                "--listen",
                uffd_uds.to_string_lossy().as_ref(),
                "--memory-bin",
                memory_bin.to_string_lossy().as_ref(),
            ])
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_clone))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                vm_err(format!(
                    "spawn {}: {e}",
                    self.config.uffd_handler_bin.display()
                ))
            })?;

        if let Err(e) = wait_for_socket(uffd_uds, Duration::from_secs(5), &mut child).await {
            let _ = child.kill().await;
            let log_tail = read_tail(&log_path, 4096).await.unwrap_or_default();
            return Err(vm_err(format!(
                "uffd handler did not open UDS: {e}\n--- handler log ---\n{log_tail}"
            )));
        }
        Ok(child)
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

        // Provision per-VM networking BEFORE spawning firecracker:
        // the TAP needs to exist when FC opens it via
        // `put_network_interface`. The remainder of create runs
        // inside a closure so any failure between here and the
        // `LiveSandbox` insert tears down the TAP + iptables rules
        // and frees the /30 — otherwise a failed create would leak
        // host-side state. Skipped entirely when networking is
        // disabled (tests).
        let net_setup = if self.config.net_pool.is_some() {
            Some(net::provision(sandbox_id, &self.net_allocator).await?)
        } else {
            None
        };

        let result = self
            .create_in_jail_after_net(sandbox_id, jail_dir, spec, net_setup.as_ref())
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                if let Some(setup) = net_setup.as_ref() {
                    net::teardown(setup, &self.net_allocator).await;
                }
                Err(e)
            }
        }
    }

    /// Inner half of `create_in_jail` — runs after net is
    /// provisioned. Split out so the parent can wrap it in
    /// teardown-on-error without the early-return ergonomics
    /// getting tangled.
    #[allow(clippy::too_many_lines)]
    async fn create_in_jail_after_net(
        &self,
        sandbox_id: SandboxId,
        jail_dir: &Path,
        spec: SandboxSpec,
        net_setup: Option<&net::NetSetup>,
    ) -> Result<(), SandboxError> {
        let rootfs = spec
            .rootfs_source
            .clone()
            .ok_or_else(|| SandboxError::InvalidSpec("rootfs_source missing".into()))?;
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
        // Append the per-sandbox `ip=...` to the kernel cmdline so
        // CONFIG_IP_PNP brings up eth0 with the guest's static
        // address before init runs. Skipped when networking is
        // disabled — the guest boots without an eth0.
        let boot_args = match net_setup {
            Some(setup) => format!(
                "{} {}",
                self.config.default_boot_args.trim_end(),
                setup.vm_cidr.kernel_ip_arg(),
            ),
            None => self.config.default_boot_args.clone(),
        };
        api.put_boot_source(&BootSource {
            kernel_image_path: self.config.kernel_image_path.to_string_lossy().into_owned(),
            boot_args,
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

        // Harness substrate: read-only ext4 image of the host's
        // `cfg.harnesses_dir`, attached as the second virtio-blk
        // drive (`/dev/vdb`). The init shim mounts it at
        // `/run/engram/harnesses` so `engram-bootstrap` can exec
        // `/run/engram/harnesses/<name>/harness`. None when the
        // host's harness registry is empty.
        if let Some(substrate_path) = spec.harness_substrate.as_ref() {
            api.put_drive(&DriveConfig {
                drive_id: "harnesses".into(),
                path_on_host: substrate_path.to_string_lossy().into_owned(),
                is_root_device: false,
                is_read_only: true,
            })
            .await?;
        }

        // virtio-net: bind FC to the TAP we provisioned above. The
        // TAP already has the host-side gateway IP and is admin-up,
        // so FC just opens it and bridges the virtio-net frontend
        // onto it. Skipped when networking is disabled.
        if let Some(setup) = net_setup {
            api.put_network_interface(&NetworkInterface {
                iface_id: "eth0".into(),
                host_dev_name: setup.tap_name.clone(),
                guest_mac: None,
            })
            .await?;
        }

        // Vsock — must be configured BEFORE InstanceStart. Firecracker
        // creates the host-side UDS at vsock_uds_path; host→guest
        // connections go through that base UDS with a `CONNECT <port>\n`
        // handshake (see exec_stream_via_fc_vsock).
        //
        // The path lives at work_dir root (NOT inside jail_dir) on
        // purpose: a snapshot bakes this path into state.bin, and FC
        // reopens it on load. If we put it inside jail_dir, destroy()
        // would remove the parent and break restore. work_dir survives.
        let vsock_cid = self.next_cid.fetch_add(1, Ordering::Relaxed);
        let vsock_uds_path = self.work_dir.join(format!("{sandbox_id}.vsock"));
        let _ = tokio::fs::remove_file(&vsock_uds_path).await;
        api.put_vsock(&VsockConfig {
            guest_cid: vsock_cid,
            uds_path: vsock_uds_path.to_string_lossy().into_owned(),
        })
        .await?;

        // Pre-bind the host-side UDS that FC routes guest-to-host
        // vsock connections through. When the guest dials AF_VSOCK
        // CID=2 port=N, FC connects to `<vsock_uds>_<N>`. Binding
        // BEFORE InstanceStart guarantees the harness's first
        // dial-out lands on a live listener (the engram-init
        // shim spawns engram-bootstrap which dials shortly after
        // boot — we'd race otherwise).
        self.spawn_harness_listener(sandbox_id, &vsock_uds_path)
            .await?;

        api.put_action(ActionType::InstanceStart).await?;

        let state = SandboxState {
            spec,
            firecracker_socket: socket,
            rootfs_path: rootfs,
            vsock_cid,
            vsock_uds_path,
        };
        self.sandboxes.insert(
            sandbox_id,
            LiveSandbox {
                state,
                child,
                uffd_handler: None,
                net: net_setup.cloned(),
                guest_ip: parking_lot::Mutex::new(None),
            },
        );
        tracing::info!(%sandbox_id, jail = %jail_dir.display(), "firecracker microVM started");
        Ok(())
    }

    /// Bind a host-side UDS for inbound harness connections from
    /// this sandbox's guest. The accept loop forwards each accepted
    /// stream into whatever [`HarnessSink`] is currently registered
    /// on `self.harness_sink`. Bound at
    /// `<vsock_uds_path>_<HARNESS_VSOCK_PORT>` so guest dials of
    /// AF_VSOCK CID=2 port=1026 land here (FC's vsock UDS contract).
    async fn spawn_harness_listener(
        &self,
        sandbox_id: SandboxId,
        vsock_uds_path: &Path,
    ) -> Result<(), SandboxError> {
        let path = harness_uds_for(vsock_uds_path);
        let _ = tokio::fs::remove_file(&path).await;
        let listener = tokio::net::UnixListener::bind(&path).map_err(|e| {
            SandboxError::Vm(
                format!(
                    "bind harness UDS {} for sandbox {sandbox_id}: {e}",
                    path.display()
                )
                .into(),
            )
        })?;
        let sink_slot = self.harness_sink.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => match sink_slot.read().clone() {
                        Some(sink) => {
                            sink(Box::pin(stream));
                        }
                        None => {
                            tracing::warn!(
                                %sandbox_id,
                                "harness connection arrived but no sink registered; dropping",
                            );
                        }
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, %sandbox_id, "harness UDS accept ended");
                        return;
                    }
                }
            }
        });
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

        // For UFFD restore, spawn the handler BEFORE PUT /snapshot/load
        // so it's listening when Firecracker connects. The handler
        // takes ownership of the kernel UFFD via SCM_RIGHTS, mmaps
        // memory.bin, and pages it in lazily. Either way the VM is
        // running by the time `load_snapshot*` returns (resume_vm: true).
        let uffd_handler = match self.config.restore_mode {
            RestoreMode::File => {
                api.load_snapshot(&SnapshotPaths {
                    state_path: state_path.clone(),
                    mem_path: mem_path.clone(),
                })
                .await?;
                None
            }
            RestoreMode::Uffd => {
                let uffd_uds = jail_dir.join("uffd.sock");
                let _ = tokio::fs::remove_file(&uffd_uds).await;
                let handler = self
                    .spawn_uffd_handler(&uffd_uds, &mem_path, jail_dir)
                    .await?;
                api.load_snapshot_uffd(&state_path, &uffd_uds).await?;
                Some(handler)
            }
        };

        // Carry the manifest's spec forward so SandboxState reflects
        // what the snapshot was taken from. rootfs_path mirrors what
        // the original VM had attached — Firecracker reopens that
        // path on load, so it must still be valid on disk.
        let rootfs_path = manifest.spec.rootfs_source.clone().unwrap_or_default();
        // FC restored the vsock device at the same UDS path stored in
        // state.bin (under work_dir, by design). We don't have the
        // pre-snapshot CID in the manifest — that was a Firecracker
        // internal — so we surface the original sandbox_id-derived
        // path and a freshly-allocated CID for our SandboxState. The
        // CID we record is informational on the restored side.
        let vsock_uds_path = self.work_dir.join(format!("{}.vsock", manifest.sandbox_id));
        let vsock_cid = self.next_cid.fetch_add(1, Ordering::Relaxed);
        // Re-spawn the harness accept loop for the restored VM. FC
        // restored its vsock device pointing at the snapshot-time UDS
        // (manifest.sandbox_id-derived path), but the host-side accept
        // loop that originally bound `<uds>_1026` died with the
        // pre-snapshot sandbox. Without this, the in-VM adapter's
        // post-resume reconnect dial finds no listener.
        self.spawn_harness_listener(sandbox_id, &vsock_uds_path)
            .await?;
        let state = SandboxState {
            spec: manifest.spec.clone(),
            firecracker_socket: socket,
            rootfs_path,
            vsock_cid,
            vsock_uds_path,
        };
        self.sandboxes.insert(
            sandbox_id,
            LiveSandbox {
                state,
                child,
                uffd_handler,
                // Restore-time net wiring is not implemented yet —
                // the manifest doesn't carry the original /30 (we'd
                // want a different one anyway, the original may be
                // freed already), and FC's snapshot bakes the TAP
                // name into state.bin which the host won't have. A
                // future fix re-provisions a fresh /30 + TAP and
                // hot-plugs it via vmm.update; out of scope for
                // this PR. Restored sandboxes therefore have no
                // egress until destroyed and re-created.
                net: None,
                guest_ip: parking_lot::Mutex::new(None),
            },
        );
        tracing::info!(
            %sandbox_id,
            jail = %jail_dir.display(),
            from = %snapshot_dir.display(),
            mode = ?self.config.restore_mode,
            "firecracker microVM restored from snapshot",
        );
        Ok(())
    }
}

fn vm_err(msg: impl Into<String>) -> SandboxError {
    SandboxError::Vm(msg.into().into())
}

/// Send the WireExecRequest, spawn the reader task that translates
/// agent events into the trait's `ExecEvent`, return an `ExecStream`.
/// Generic over the reader/writer halves so both the direct-UDS and
/// the FC-vsock-CONNECT paths can share it.
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
        .map_err(|e| vm_err(format!("send WireRequest::Exec: {e}")))?;

    let exec_id = format!("fc-{}", uuid::Uuid::new_v4().simple());
    // 64 events of buffer is enough that a slow consumer doesn't
    // immediately backpressure the agent; the agent has its own
    // 64-event channel so total in-flight bound is bounded.
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
                    // Connection died before Exit: surface as a
                    // synthetic Exit(None) so the consumer's
                    // ".next() until Exit" loop terminates.
                    tracing::warn!(
                        error = %e,
                        "agent connection ended without explicit Exit",
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
        // Phase 4 Track A wired the harness protocol. The
        // Firecracker path for spawning the in-VM harness adapter
        // (`engram-bootstrap` reading argv from a file dropped into
        // the rootfs at create-time) lands with Phase 5's image-baker
        // changes. Until then, the trait-default `start_agent`
        // errors with `InvalidSpec` — production sessions don't yet
        // declare an agent and dev sessions use ProcessBackend.
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
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.vsock_uds_path.clone()
        };
        // The agent inside the guest takes a couple of seconds to come up
        // after `create()` returns: kernel boot → init → engram-agentd
        // bind on vsock 1024. Connecting before that returns "early eof"
        // on the CONNECT response (FC closes the UDS when there's no
        // listener on the requested guest port). Retry with exponential
        // backoff for ~10s; bail fast on any non-handshake error so
        // genuine breakage doesn't get hidden.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut sleep = Duration::from_millis(50);
        loop {
            match Self::exec_stream_via_fc_vsock(
                id,
                &vsock_uds_path,
                ENGRAM_AGENTD_PORT,
                cmd.clone(),
            )
            .await
            {
                Ok(s) => return Ok(s),
                Err(e) => {
                    let msg = format!("{e}");
                    let is_boot_race = msg.contains("read FC vsock CONNECT response")
                        || msg.contains("connect to FC vsock UDS");
                    if !is_boot_race || std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(sleep).await;
                    sleep = (sleep * 2).min(Duration::from_secs(1));
                }
            }
        }
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
        // Snapshot duration scales with guest memory (every dirty page
        // is flushed to memory.bin synchronously). The default 10s
        // client timeout fits a 64 MiB VM but trips on larger ones —
        // give the snapshot path 60s explicitly. Tune up for huge VMs.
        let api = FirecrackerClient::new(&socket).with_timeout(Duration::from_secs(60));
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
                // Set ENGRAM_FC_KEEP_JAIL_ON_FAILURE=1 to keep the
                // jail dir for post-mortem of firecracker.log /
                // uffd-handler.log. Default is to clean up.
                if std::env::var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE").is_err() {
                    let _ = tokio::fs::remove_dir_all(&jail_dir).await;
                } else {
                    tracing::warn!(
                        jail = %jail_dir.display(),
                        "preserving jail dir for diagnostics (ENGRAM_FC_KEEP_JAIL_ON_FAILURE)",
                    );
                }
                Err(e)
            }
        }
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // Idempotent: removing an unknown id is a no-op, matching the
        // contract ProcessBackend follows.
        let Some((_, mut live)) = self.sandboxes.remove(&id) else {
            return Ok(());
        };

        // Try a graceful shutdown first: PUT /actions { SendCtrlAltDel }
        // tells the guest kernel to halt cleanly via the keyboard
        // controller's CAD signal, draining the page cache before we
        // pull the plug. If the guest doesn't respond within
        // `GRACEFUL_SHUTDOWN_TIMEOUT`, we fall through to SIGKILL.
        let api = FirecrackerClient::new(&live.state.firecracker_socket);
        let graceful = async {
            api.put_action(ActionType::SendCtrlAltDel).await?;
            // Wait for the firecracker process to exit on its own.
            // `Child::wait` returns once the process is reaped.
            live.child.wait().await.map_err(SandboxError::from)
        };
        match tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, graceful).await {
            Ok(Ok(_)) => {
                tracing::debug!(sandbox_id = %id, "firecracker exited cleanly via SendCtrlAltDel");
            }
            Ok(Err(e)) => {
                // SendCtrlAltDel rejected (FC API rejected the action,
                // socket closed, etc.) — escalate to SIGKILL. This
                // also covers the case where the API socket is gone
                // but the child somehow survives.
                tracing::debug!(
                    sandbox_id = %id,
                    error = %e,
                    "graceful shutdown failed; escalating to SIGKILL",
                );
                if let Err(e) = live.child.kill().await {
                    tracing::warn!(sandbox_id = %id, error = %e, "firecracker kill failed");
                }
                let _ = live.child.wait().await;
            }
            Err(_) => {
                // Timeout: guest didn't honour CAD within the grace
                // window. Force-kill.
                tracing::debug!(
                    sandbox_id = %id,
                    timeout = ?GRACEFUL_SHUTDOWN_TIMEOUT,
                    "firecracker didn't exit in time; SIGKILLing",
                );
                if let Err(e) = live.child.kill().await {
                    tracing::warn!(sandbox_id = %id, error = %e, "firecracker kill failed");
                }
                let _ = live.child.wait().await;
            }
        }

        // UFFD handler (only set on Uffd-mode restore) follows
        // firecracker into oblivion. Its UDS + log live inside
        // jail_dir, which the remove_dir_all below sweeps.
        if let Some(mut handler) = live.uffd_handler {
            let _ = handler.kill().await;
            let _ = handler.wait().await;
        }

        // Tear down per-VM networking: yank iptables rules tagged
        // with this sandbox's chain comment, delete the TAP, return
        // the /30 to the allocator. Best-effort — each step logs
        // but doesn't stop the rest.
        if let Some(net_setup) = live.net.as_ref() {
            net::teardown(net_setup, &self.net_allocator).await;
        }

        // Vsock UDS lives at work_dir root; remove explicitly since
        // it isn't inside the jail dir we wipe below.
        let _ = tokio::fs::remove_file(&live.state.vsock_uds_path).await;

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

    /// IPv4 the host can use to reach a TCP service inside this
    /// guest (used by the dashboard SHELL tab to dial `ttyd` on
    /// :7681). Mirrors the VZ pattern: dial agentd via vsock, send
    /// `WireRequest::GuestIp`, cache the answer. None until the
    /// guest's eth0 has an address — kernel `ip=` cmdline brings it
    /// up before init, but the agent has to start before answering.
    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        if let Some(live) = self.sandboxes.get(&id) {
            if let Some(ip) = live.guest_ip.lock().clone() {
                return Some(ip);
            }
        }
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id)?;
            live.state.vsock_uds_path.clone()
        };
        let fut = async {
            let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                .await
                .ok()?;
            engram_agentd::write_msg(&mut conn, &WireRequest::GuestIp)
                .await
                .ok()?;
            let resp: engram_agentd::WireResponse =
                engram_agentd::read_msg(&mut conn).await.ok()?;
            match resp {
                engram_agentd::WireResponse::GuestIp(ip) => ip,
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

    async fn start_agent(
        &self,
        id: SandboxId,
        agent: engram_core::types::sandbox::AgentSpec,
    ) -> Result<(), SandboxError> {
        // Read sandbox state under the dashmap guard, drop it
        // before any await — the path/cid we need is `Clone`.
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.vsock_uds_path.clone()
        };

        // Connect to the in-guest engram-bootstrap. The bootstrap
        // listener takes seconds to come up after VM boot — same
        // race as `exec_stream`'s vsock CONNECT. Retry the
        // handshake-only error patterns with backoff.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut backoff = Duration::from_millis(100);
        let mut conn = loop {
            match Self::connect_fc_vsock(
                &vsock_uds_path,
                engram_harness_proto::BOOTSTRAP_VSOCK_PORT,
            )
            .await
            {
                Ok(c) => break c,
                Err(e) => {
                    let msg = format!("{e}");
                    let is_boot_race = msg.contains("read FC vsock CONNECT response")
                        || msg.contains("connect to FC vsock UDS");
                    if !is_boot_race || std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                }
            }
        };

        // Wait for bootstrap's readiness marker before writing the
        // launch. See vz/backend.rs::start_agent for why — same race
        // shape, slightly different surface (vsock here, virtio-
        // console there). The marker is a one-byte write bootstrap
        // does immediately after `listener.accept()` returns.
        use tokio::io::AsyncReadExt;
        let mut marker = [0u8; 1];
        match tokio::time::timeout(Duration::from_secs(15), conn.read_exact(&mut marker)).await {
            Ok(Ok(_)) => {
                if marker[0] != engram_harness_proto::BOOTSTRAP_READY_BYTE {
                    tracing::warn!(
                        sandbox_id = %id,
                        got = marker[0],
                        "fc start_agent: unexpected bootstrap marker; proceeding anyway"
                    );
                }
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

        // Push the BootstrapLaunch frame. The in-guest bootstrap
        // is a long-running supervisor that loops on accept, so a
        // call here also covers post-resume re-launch (the
        // supervisor kill+respawns its child harness on every fresh
        // launch frame). The connection drops on our end after the
        // write — bootstrap reads the frame and goes back to
        // accept().
        let launch = engram_harness_proto::BootstrapLaunch {
            argv: agent.argv,
            env: agent.env.into_iter().collect(),
        };
        engram_harness_proto::write_msg(&mut conn, &launch)
            .await
            .map_err(|e| SandboxError::Vm(format!("write BootstrapLaunch: {e}").into()))?;
        // Best-effort flush; bootstrap closes its end on exec.
        let _ = conn.shutdown().await;
        Ok(())
    }

    fn set_harness_sink(&self, sink: engram_core::traits::HarnessSink) {
        *self.harness_sink.write() = Some(sink);
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
            uffd_handler_bin: PathBuf::from("/nonexistent/engram-uffd-handler"),
            restore_mode: RestoreMode::File,
            net_pool: None,
            egress_proxy_port: None,
        };
        (FirecrackerBackend::new(dir.path(), cfg), dir)
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            image_uri: None,
            harness_pack_uri: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 256 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            harness_substrate: None,
            network: Default::default(),
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

    #[tokio::test]
    async fn exec_stream_unknown_id_is_not_found() {
        let (b, _d) = backend();
        let req = ExecRequest {
            command: vec!["true".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
        };
        match b.exec_stream(SandboxId::new(), req).await {
            Err(SandboxError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
