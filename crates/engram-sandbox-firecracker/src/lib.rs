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
pub mod paths;
pub mod pidfd;
pub mod sandbox_manifest;

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
    /// UDP+TCP port the filtering DNS proxy listens on. Iptables
    /// REDIRECTs guest `{udp,tcp}/53` to this port so the proxy can
    /// enforce `manifest.network.allow_hosts` on resolution. Default
    /// 5353 (avoids systemd-resolved's 127.0.0.53:53 bind on hosts
    /// that run it). Ignored when `egress_proxy_port` is `None` —
    /// no-proxy mode keeps the legacy unconditional ACCEPT to
    /// 1.1.1.1:53.
    pub egress_dns_port: Option<u16>,
    /// ADR 0007 Phase 5: this host's stable `HostId`. Stamped on
    /// the FC sidecar JSON at snapshot time (so cross-host restore
    /// knows which host's working-set trace to prefault) AND
    /// passed to the UFFD handler at restore time as
    /// `--publish-trace-host` (so the recorder publishes this
    /// host's trace under the right key). `None` keeps the
    /// pre-Phase-5 behaviour: traces aren't recorded or replayed,
    /// every restore pays full first-fault cost.
    pub host_id: Option<engram_core::HostId>,
    /// ADR 0007 Phase 5: NVMe-backed chunk cache dir handed to
    /// `engram-uffd-handler` as `--cache-root`. `None` falls back
    /// to the handler's compiled-in default
    /// (`/var/cache/engram/chunks`) which production root-running
    /// hosts can use as-is; unprivileged CI runners + the FC
    /// backend's own work_dir convention should set this
    /// explicitly. Constructors derive a sensible default
    /// (`<work_dir>/uffd-chunk-cache/`) when the backend is built
    /// via `FirecrackerBackend::new`.
    pub uffd_cache_root: Option<PathBuf>,
}

/// ADR 0009 §6 errors from `FirecrackerBackend::reattach_sandbox`.
/// Distinct from `SandboxError` so the live-attach driver can
/// distinguish "FC truly gone, fall through to path 2" from "operator
/// configuration problem, log + skip."
#[derive(Debug)]
pub enum ReattachError {
    /// `kill(pid, 0)` returned ESRCH or /proc lookup failed —
    /// process is gone. Path 1 fails; path 2 (NVMe restore) may
    /// succeed; otherwise orphan-reap.
    PidGone(u32),
    /// `/proc/<pid>/stat` starttime field doesn't match the manifest.
    /// The kernel reused the pid for an unrelated process; we
    /// definitively can't reattach to the original FC.
    StartTimeMismatch { pid: u32, recorded: u64, live: u64 },
    /// `/proc/<pid>/comm` doesn't match. Cheap sanity check —
    /// extremely unlikely after start_time matched but defends
    /// against the corner case.
    CommMismatch {
        pid: u32,
        recorded: String,
        live: String,
    },
    /// FC API socket exists but doesn't accept connections.
    /// Underlying FC may be wedged or its socket file was overwritten
    /// by something else. Path 1 fails; path 2 may succeed.
    ApiUnresponsive(String),
    /// Network state recovery failed — the recorded /30 slot is
    /// in use by something else (cross-host migration of an old
    /// manifest, or operator error). Path 1 fails; path 2 may
    /// succeed.
    NetReserveFailed(String),
    /// `pidfd_open(pid)` failed for a reason other than "process
    /// gone." Likely a kernel version issue (< 5.3). The pidfd
    /// itself is best-effort here; without it the poll supervisor
    /// still works, but the error indicates something more
    /// systemic — treat as a hard failure for now.
    PidFdOpenFailed(String),
}

impl std::fmt::Display for ReattachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PidGone(pid) => write!(f, "reattach: pid {pid} no longer exists"),
            Self::StartTimeMismatch {
                pid,
                recorded,
                live,
            } => write!(
                f,
                "reattach: pid {pid} starttime mismatch (recorded {recorded}, live {live}); \
                 the pid was recycled"
            ),
            Self::CommMismatch {
                pid,
                recorded,
                live,
            } => write!(
                f,
                "reattach: pid {pid} comm mismatch (recorded {recorded}, live {live})"
            ),
            Self::ApiUnresponsive(e) => write!(f, "reattach: FC API unresponsive: {e}"),
            Self::NetReserveFailed(e) => write!(f, "reattach: net slot reserve failed: {e}"),
            Self::PidFdOpenFailed(e) => write!(f, "reattach: pidfd_open failed: {e}"),
        }
    }
}

impl std::error::Error for ReattachError {}

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
            egress_dns_port: None,
            kernel_image_path: kernel_image_path.into(),
            default_boot_args: "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init"
                .into(),
            firecracker_bin: PathBuf::from("firecracker"),
            uffd_handler_bin: PathBuf::from("engram-uffd-handler"),
            restore_mode: RestoreMode::File,
            host_id: None,
            uffd_cache_root: None,
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
    /// `Some` for sandboxes created or restored within this
    /// host-agent process generation; `None` for sandboxes
    /// pidfd-reattached on host-agent startup (ADR 0009 §5/§6) —
    /// we never had a `Child` handle for them because they were
    /// spawned by a previous generation. The destroy path branches
    /// on this: `Some` uses `tokio::process::Child` kill/wait,
    /// `None` falls back to libc kill + poll for exit.
    child: Option<Child>,
    /// FC process pid, captured at create/restore/reattach time.
    /// Always populated (even when `child` is `None`) so destroy
    /// and the supervisor have a stable handle.
    fc_pid: Option<u32>,
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
/// `SandboxSpec`.
///
/// `net` carries the original /30 + TAP name so restore can recreate
/// the host-side networking the snapshot's `state.bin` expects.
/// Without it, FC's snapshot load fails when the virtio-net frontend
/// tries to bind a TAP that doesn't exist on the receiving host.
///
/// Not consumed by Firecracker; entirely ours. We don't try to make
/// this format stable across major versions — snapshots have an
/// implicit shelf life tied to a release.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FcSnapshotManifest {
    sandbox_id: SandboxId,
    created_at: DateTime<Utc>,
    spec: SandboxSpec,
    /// `None` when the source VM was created with networking disabled
    /// (test mode) or when restoring from a pre-net manifest.
    #[serde(default)]
    net: Option<FcNetSnapshot>,
    /// Tag mirroring `VzSnapshotManifest::format` so a cross-VMM
    /// restore (FC pulling a VZ blob, or vice versa) fails fast with
    /// a clear message instead of a confusing parse error inside
    /// load_snapshot. Defaults to empty for snapshots written before
    /// this field landed; `restore` accepts both `"fc"` and `""` for
    /// backwards compat.
    #[serde(default)]
    format: String,
    /// ADR 0007 / Phase 5: chunked memory manifest ref. Populated by
    /// `PooledBackend::snapshot` after FC writes memory.bin; the
    /// chunked UFFD restore path reads it to wire the handler. When
    /// `None` (FC backend ran without `PooledBackend` chunking), UFFD
    /// restore mode refuses to start — the operator must either wrap
    /// with PooledBackend+chunk_store or switch to RestoreMode::File.
    #[serde(default)]
    memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
    /// ADR 0007 / Phase 5: canonical-base memory manifest. Identifies
    /// the per-image bake-time canonical snapshot the UFFD handler
    /// mmaps for cross-VM page-cache sharing. When equal to
    /// `memory_manifest`, the resolver returns `Canonical` for every
    /// fault (no chunk fetches; the local memory.bin mmap serves
    /// everything). When different, divergent chunks fetch from the
    /// chunk store. `None` is treated identically to "equal to
    /// memory_manifest" — convenient default until the image-builder
    /// bake-time canonical capture slice lands.
    #[serde(default)]
    canonical_memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
    /// ADR 0007 / Phase 5: hint about whose working-set trace to
    /// prefault on the restoring host. Set to the snapshotting
    /// host's `HostId` so cross-host restore can ask
    /// `traces/<manifest_id>/<this_hint>.json` for the chunks
    /// the original host knew were hot. `None` skips trace replay
    /// (pre-Phase-5 snapshots or hosts that didn't have a
    /// `HostId` configured on FC). Note: this is a HINT — the
    /// recorder on the restoring host still publishes its own
    /// trace under its own host id, so subsequent restores on
    /// this host use the local recording.
    #[serde(default)]
    trace_host_hint: Option<engram_core::HostId>,
    /// ADR 0014 M1.11: the exact `path_on_host` the bake's FC
    /// instance PUT /drives'd with as the rootfs. FC's `state.bin`
    /// embeds this path; on cross-host restore the receiver must
    /// recreate a file (or symlink) at that exact path before
    /// `load_snapshot`, otherwise FC errors out with "Block: Virtio
    /// backend error: No such file or directory". The receiver's
    /// own work_dir is generally `/var/lib/engram/sandboxes`, but
    /// the bake ran in a `tempfile::tempdir()` (`/tmp/.tmpXXX/`)
    /// so the bake-time and receiver-time work_dirs are different
    /// — without this field, the receiver has no way to know what
    /// state.bin actually embeds.
    ///
    /// `None` for snapshots written before this field landed. The
    /// receiver falls back to its own work_dir, which only works
    /// for same-host (idle resume) restores.
    #[serde(default)]
    source_rootfs_canonical: Option<PathBuf>,
    /// Same shape, for the harness substrate drive. `None` if the
    /// source sandbox booted without a harness substrate.
    #[serde(default)]
    source_harness_canonical: Option<PathBuf>,
}

const MANIFEST_FORMAT_FC: &str = "fc";

/// What we need from the source VM's `NetSetup` to recreate networking
/// on a restored VM. The `cidr_network` is the /30's network address
/// (`.0`); the receiving host's `NetworkAllocator::reserve` will
/// re-claim that slot if free, or fail-soft (no egress) if taken.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FcNetSnapshot {
    /// `tap-engr-<6 hex>` — see `net::tap_name_for`. Recreated under
    /// this exact name on restore because FC bakes the TAP name into
    /// `state.bin`.
    tap_name: String,
    /// /30 network address (lowest octet's two LSBs are zero).
    cidr_network: std::net::Ipv4Addr,
}

pub struct FirecrackerBackend {
    work_dir: PathBuf,
    config: FirecrackerConfig,
    /// `Arc<DashMap>` (rather than a bare `DashMap`) so the per-VM
    /// supervisor task (ADR 0009 §4, `spawn_process_supervisor`) can
    /// hold an independent reference and prune the entry on
    /// unexpected exit. The Arc-overhead is one pointer per access
    /// — negligible against the cost of any sandbox operation.
    sandboxes: Arc<DashMap<SandboxId, LiveSandbox>>,
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
        let work_dir: PathBuf = work_dir.into();
        // ADR 0007 Phase 5: default the UFFD handler's chunk cache
        // to a work_dir-local directory unless the caller picked
        // one explicitly. This avoids the handler's compiled-in
        // `/var/cache/engram/chunks` default that requires root on
        // CI runners + unprivileged production hosts.
        let mut config = config;
        if config.uffd_cache_root.is_none() {
            config.uffd_cache_root = Some(work_dir.join("uffd-chunk-cache"));
        }
        Self {
            work_dir,
            config,
            sandboxes: Arc::new(DashMap::new()),
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
        net::host_startup(self.config.egress_proxy_port, self.config.egress_dns_port)
            .await
            .map_err(SandboxError::from)
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// ADR 0007 Phase 6: per-snapshot staging dir, owned by the
    /// backend. Snapshots live under `<work_dir>/snapshots/<id>/`
    /// rather than the per-sandbox jail dir, so a snapshot
    /// outlives `destroy(sandbox_id)` cleaning the jail.
    fn snapshot_dir_for(&self, snapshot_id: engram_core::types::SnapshotId) -> PathBuf {
        self.work_dir
            .join("snapshots")
            .join(snapshot_id.to_string())
    }

    pub fn config(&self) -> &FirecrackerConfig {
        &self.config
    }

    /// Used by the host agent for inspection / heartbeat reporting.
    pub fn snapshot_state(&self, id: SandboxId) -> Option<SandboxState> {
        self.sandboxes.get(&id).map(|r| r.state.clone())
    }

    /// ADR 0009 §6 path 2: restore from a local NVMe checkpoint
    /// while preserving the original `sandbox_id`. Unlike the
    /// standard `SandboxBackend::restore` (which always allocates a
    /// fresh sandbox_id — appropriate for cross-host migration),
    /// this variant keeps the caller-supplied id so the coord's
    /// session_id → sandbox_id routing survives a graceful host
    /// reboot. The on-disk artifacts at
    /// `<work_dir>/snapshots/<snapshot_id>/` (written by the
    /// SIGTERM checkpoint pipeline in Phase 7) are read directly.
    pub async fn restore_as_sandbox_id(
        &self,
        sandbox_id: SandboxId,
        snapshot_id: SnapshotId,
    ) -> Result<(), SandboxError> {
        let src = self.snapshot_dir_for(snapshot_id);
        let manifest_bytes = tokio::fs::read(src.join("manifest.json"))
            .await
            .map_err(|e| SandboxError::Snapshot(format!("read manifest: {e}")))?;
        let manifest: FcSnapshotManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| SandboxError::Snapshot(format!("manifest parse: {e}")))?;
        if !matches!(manifest.format.as_str(), MANIFEST_FORMAT_FC | "") {
            return Err(SandboxError::Snapshot(format!(
                "manifest format {:?} is not 'fc' — cross-VMM restore not supported",
                manifest.format,
            )));
        }
        let jail_dir = self.work_dir.join(sandbox_id.to_string());
        self.restore_in_jail(sandbox_id, &jail_dir, &src, &manifest)
            .await
    }

    /// ADR 0009 §6: live-VM reattach (path 1). Called from the
    /// host-agent's startup pass once per `sandbox.json` manifest
    /// found under `work_dir/*/`. Verifies the FC process is still
    /// the original one (three-axis pid identity + FC API ping +
    /// TAP exists), then rebuilds the in-memory `LiveSandbox`
    /// without a `Child` handle. The supervisor (§4) is re-spawned
    /// against the reattached pid.
    ///
    /// On verification failure (FC died, pid recycled, TAP gone)
    /// returns `Err(ReattachError::Verify*)` so the caller can fall
    /// through to path 2 (NVMe restore, Phase 8) or path 3
    /// (orphan-reap).
    pub async fn reattach_sandbox(
        &self,
        manifest: &sandbox_manifest::SandboxManifest,
    ) -> Result<(), ReattachError> {
        let id = manifest.sandbox_id;
        let fc = &manifest.firecracker;

        // Three-axis identity check. The kernel never reuses a
        // (pid, starttime) pair within a boot, so a starttime
        // mismatch is a definitive "different process now."
        let live_start = sandbox_manifest::read_proc_start_time_jiffies(fc.process.pid)
            .ok_or(ReattachError::PidGone(fc.process.pid))?;
        if live_start != fc.process.start_time_jiffies {
            return Err(ReattachError::StartTimeMismatch {
                pid: fc.process.pid,
                recorded: fc.process.start_time_jiffies,
                live: live_start,
            });
        }
        let live_comm = sandbox_manifest::read_proc_comm(fc.process.pid)
            .ok_or(ReattachError::PidGone(fc.process.pid))?;
        if live_comm != fc.process.comm {
            return Err(ReattachError::CommMismatch {
                pid: fc.process.pid,
                recorded: fc.process.comm.clone(),
                live: live_comm,
            });
        }

        // FC API liveness probe. The client crate exposes only
        // mutating endpoints — we just need a "is the socket alive?"
        // check, so go straight to `UnixStream::connect`. FC binds
        // the socket at startup and unlinks at shutdown; a
        // successful connect means FC is running and accepting on
        // its API.
        match tokio::net::UnixStream::connect(&fc.api_socket).await {
            Ok(_) => {}
            Err(e) => {
                return Err(ReattachError::ApiUnresponsive(format!(
                    "connect {}: {e}",
                    fc.api_socket.display()
                )));
            }
        }

        // Network rehydration: mark the slot in-use so future
        // `create()` calls don't double-allocate, and verify the
        // TAP still exists. If TAP is gone the kernel state was
        // wiped externally — reattach won't be usable.
        let net_setup = if let Some(net_rec) = manifest.network.as_ref() {
            let vm_cidr = net::VmCidr::new(net_rec.vm_cidr_network);
            self.net_allocator
                .lock()
                .reserve(vm_cidr)
                .map_err(|e| ReattachError::NetReserveFailed(format!("{e:?}")))?;
            Some(net::NetSetup {
                vm_cidr,
                tap_name: net_rec.tap_name.clone(),
            })
        } else {
            None
        };

        // pidfd_open for the future supervisor + the eventual
        // Phase 8 SIGTERM-checkpoint reattach. On non-Linux this
        // returns Unsupported; we treat it as "no pidfd but the
        // poll-based supervisor still works."
        match pidfd::open_pidfd(fc.process.pid) {
            Ok(_fd) => {
                // We could keep the fd in LiveSandbox for a
                // future fd-based exit notification. The polling
                // supervisor is enough for now; drop the fd here.
                // Phase 7+ can store it if it wants AsyncFd-based
                // exit notify (lower latency than 1s polling).
                tracing::debug!(%id, pid = fc.process.pid, "pidfd opened during reattach");
            }
            Err(pidfd::PidFdError::Unsupported) => {
                tracing::warn!(
                    %id,
                    "pidfd not supported on this platform; reattach proceeds without it \
                     (poll-based supervisor still functional)"
                );
            }
            Err(e) => {
                return Err(ReattachError::PidFdOpenFailed(format!("{e}")));
            }
        }

        let state = SandboxState {
            spec: manifest.spec.clone(),
            firecracker_socket: fc.api_socket.clone(),
            rootfs_path: PathBuf::new(), // not used after create; the
            // manifest's `spec.rootfs_source` is authoritative if a
            // resume needs to find the on-disk rootfs. Keep this
            // field for future symmetry with create().
            vsock_cid: fc.vsock_cid,
            vsock_uds_path: fc.vsock_uds_base.clone(),
        };
        self.sandboxes.insert(
            id,
            LiveSandbox {
                state,
                child: None,
                fc_pid: Some(fc.process.pid),
                uffd_handler: None,
                net: net_setup,
                guest_ip: parking_lot::Mutex::new(None),
            },
        );

        // §4 supervisor on the reattached pid.
        spawn_process_supervisor(self.sandboxes.clone(), id, fc.process.pid, "firecracker");
        if let Some(uffd) = manifest.uffd_handler.as_ref() {
            // Verify UFFD handler too. Same three-axis check; if
            // it's gone the FC is mid-restore-with-no-pager —
            // unusable. Phase 8 may add fallback paths; for now
            // we treat this as reattach failure that should clean
            // up FC too.
            let uffd_start = sandbox_manifest::read_proc_start_time_jiffies(uffd.pid);
            let uffd_comm = sandbox_manifest::read_proc_comm(uffd.pid);
            if uffd_start == Some(uffd.start_time_jiffies)
                && uffd_comm.as_deref() == Some(uffd.comm.as_str())
            {
                spawn_process_supervisor(self.sandboxes.clone(), id, uffd.pid, "uffd-handler");
            } else {
                tracing::warn!(
                    %id,
                    uffd_pid = uffd.pid,
                    "UFFD handler gone or recycled during reattach; FC may page-fault forever"
                );
            }
        }

        tracing::info!(
            %id,
            pid = fc.process.pid,
            "FC sandbox pidfd-reattached (ADR 0009 §6 path 1)"
        );
        Ok(())
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

    /// Spawn `engram-uffd-handler` in ADR 0007 chunked mode and
    /// wait until it's listening on `uffd_uds`. `canonical_memory`
    /// is the local file the handler mmaps for canonical-resolved
    /// pages; `canonical_ref` + `session_ref` identify the
    /// manifests it reads from the chunk store. `prefault_trace_host`
    /// optionally points at a host's prior working-set recording for
    /// REAP-style replay; `publish_trace_host` names the host the
    /// recorder publishes the new trace under on clean shutdown.
    ///
    /// Stdout/stderr go into the jail dir's `uffd-handler.log` so a
    /// snapshot-restore failure has a recoverable diagnostic.
    /// Returns the live `Child` so the caller can hold it for the
    /// VM's lifetime.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_uffd_handler(
        &self,
        uffd_uds: &Path,
        canonical_memory: &Path,
        canonical_ref: engram_core::types::manifest::ManifestRef,
        session_ref: engram_core::types::manifest::ManifestRef,
        prefault_trace_host: Option<uuid::Uuid>,
        publish_trace_host: Option<uuid::Uuid>,
        jail_dir: &Path,
    ) -> Result<Child, SandboxError> {
        let log_path = jail_dir.join("uffd-handler.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|e| vm_err(format!("open uffd handler log {}: {e}", log_path.display())))?;
        let log_clone = log
            .try_clone()
            .map_err(|e| vm_err(format!("dup uffd-handler log fd: {e}")))?;

        let mut cmd = Command::new(&self.config.uffd_handler_bin);
        cmd.arg("--listen")
            .arg(uffd_uds)
            .arg("--canonical-memory")
            .arg(canonical_memory)
            .arg("--canonical-manifest")
            .arg(canonical_ref.to_string())
            .arg("--session-manifest")
            .arg(session_ref.to_string());
        // ADR 0007 Phase 5: hand the handler a work_dir-local
        // chunk cache root. `FirecrackerBackend::new` populates a
        // default; callers using `FirecrackerConfig` directly can
        // override or leave `None` (the handler's compiled-in
        // default is `/var/cache/engram/chunks`, root-only).
        if let Some(cache_root) = self.config.uffd_cache_root.as_ref() {
            cmd.arg("--cache-root").arg(cache_root);
        }
        if let Some(host) = prefault_trace_host {
            cmd.arg("--prefault-trace").arg(host.to_string());
        }
        if let Some(host) = publish_trace_host {
            cmd.arg("--publish-trace-host").arg(host.to_string());
        }

        let mut child = cmd
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
        // Validate the spec carries a usable rootfs. We only accept an
        // ext4 image; a directory rootfs would need to be packed into
        // ext4 first by the image-builder.
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
        // ADR 0014: install canonical symlinks for the rootfs (and
        // harness, when present) at sandbox-id-keyed paths under
        // `<work_dir>/{rootfs,harness}/`. FC's `state.bin` embeds
        // `path_on_host` literally; the canonical layer makes that
        // embedded path live OUTSIDE the jail so it survives
        // `destroy()`'s `remove_dir_all(jail_dir)`. Any host with
        // the same `<work_dir>` convention can re-materialize the
        // target under the same canonical path on restore.
        for parent in paths::canonical_parent_dirs(&self.work_dir) {
            tokio::fs::create_dir_all(&parent).await.map_err(|e| {
                SandboxError::Vm(
                    format!("create canonical parent dir {}: {e}", parent.display()).into(),
                )
            })?;
        }
        let rootfs_canonical = paths::rootfs_canonical(&self.work_dir, sandbox_id);
        paths::install_symlink(&rootfs_canonical, &rootfs)
            .await
            .map_err(|e| {
                SandboxError::Vm(
                    format!(
                        "install rootfs canonical symlink {} -> {}: {e}",
                        rootfs_canonical.display(),
                        rootfs.display()
                    )
                    .into(),
                )
            })?;
        api.put_drive(&DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: rootfs_canonical.to_string_lossy().into_owned(),
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
            let harness_canonical = paths::harness_canonical(&self.work_dir, sandbox_id);
            paths::install_symlink(&harness_canonical, substrate_path)
                .await
                .map_err(|e| {
                    SandboxError::Vm(
                        format!(
                            "install harness canonical symlink {} -> {}: {e}",
                            harness_canonical.display(),
                            substrate_path.display()
                        )
                        .into(),
                    )
                })?;
            api.put_drive(&DriveConfig {
                drive_id: "harnesses".into(),
                path_on_host: harness_canonical.to_string_lossy().into_owned(),
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
        // ADR 0009 §4: spawn a supervisor that watches for unexpected
        // FC process exit (kernel OOM, segfault, manual kill) and
        // prunes the entry from `sandboxes`. Without this,
        // `backend.list()` would keep reporting a phantom sandbox
        // whose underlying VM is dead, defeating reconcile.
        let fc_pid = child.id();

        // ADR 0009 §5: write the per-sandbox on-disk manifest before
        // we hand control back to the caller. The Phase 6 reattach
        // pass reads this on host-agent startup to decide whether to
        // pidfd-attach (path 1) the still-live FC process. Manifest
        // write failure is degrading-but-not-fatal: the supervisor
        // (§4) still works, and a host-agent restart loses the
        // ability to reattach this specific sandbox (treated as a
        // missing-sandbox by reconcile, flips per the §3 policy).
        if let Some(pid) = fc_pid {
            let m = sandbox_manifest::SandboxManifest {
                schema_version: sandbox_manifest::SCHEMA_VERSION,
                sandbox_id,
                backend: sandbox_manifest::BACKEND_FIRECRACKER.to_string(),
                spec: state.spec.clone(),
                firecracker: sandbox_manifest::FirecrackerProcessRecord {
                    process: sandbox_manifest::ProcessRecord {
                        pid,
                        start_time_jiffies: sandbox_manifest::read_proc_start_time_jiffies(pid)
                            .unwrap_or(0),
                        comm: sandbox_manifest::read_proc_comm(pid).unwrap_or_default(),
                    },
                    api_socket: state.firecracker_socket.clone(),
                    vsock_uds_base: state.vsock_uds_path.clone(),
                    vsock_cid,
                },
                network: net_setup.map(|ns| sandbox_manifest::NetworkRecord {
                    tap_name: ns.tap_name.clone(),
                    vm_cidr_network: ns.vm_cidr.network(),
                    host_ip: ns.vm_cidr.host(),
                    guest_ip: ns.vm_cidr.guest(),
                }),
                uffd_handler: None,
                last_local_snapshot: None,
            };
            let manifest_path = sandbox_manifest::manifest_path(&self.work_dir, sandbox_id);
            if let Err(e) = sandbox_manifest::write_manifest(&manifest_path, &m) {
                tracing::warn!(
                    %sandbox_id,
                    error = %e,
                    "sandbox manifest write failed; reattach across host-agent restart \
                     will be impossible for this sandbox (it'll flip per reconcile §3 \
                     instead). Sandbox itself is fine."
                );
            }
        }

        self.sandboxes.insert(
            sandbox_id,
            LiveSandbox {
                state,
                child: Some(child),
                fc_pid,
                uffd_handler: None,
                net: net_setup.cloned(),
                guest_ip: parking_lot::Mutex::new(None),
            },
        );
        if let Some(pid) = fc_pid {
            spawn_process_supervisor(self.sandboxes.clone(), sandbox_id, pid, "firecracker");
        } else {
            tracing::warn!(
                %sandbox_id,
                "firecracker Child has no pid (already exited?); supervisor not started"
            );
        }
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

        // ADR 0014: FC's `state.bin` embeds the canonical rootfs
        // path keyed by the **source** sandbox_id, not this new
        // restored sandbox_id. The receiver must materialize the
        // rootfs at that exact host-visible path before
        // `load_snapshot` opens it. Same `<work_dir>` contract +
        // re-create the source-id-keyed symlink pointing at the
        // host-local source (same OCI cache file for same-host
        // restore; cross-host restore expects the caller to have
        // materialized into `manifest.spec.rootfs_source` already).
        // Errors here drop `child` explicitly so the spawned FC
        // process gets SIGKILLed before we propagate.
        if let Err(e) = restore_canonical_symlinks(&self.work_dir, manifest).await {
            drop(child);
            return Err(e);
        }

        // Re-provision the host-side networking BEFORE load_snapshot.
        // FC's `state.bin` references the original TAP by name on the
        // virtio-net frontend, and the snapshot load will fail when
        // FC tries to open a TAP that doesn't exist on the receiving
        // host. Same-host hot-resume: the original TAP/CIDR were freed
        // on `destroy()`, so the slot is back in the allocator.
        // Cross-host cold-resume: the slot may already be taken on
        // this host, in which case we fail-soft (no egress) rather
        // than rejecting the restore — the snapshot is still valid,
        // the VM still boots, the user can re-create to recover egress.
        let net_setup = match self.reserve_restored_net(manifest.net.as_ref()).await {
            Ok(setup) => setup,
            Err(e) => {
                // We've spawned FC; clean up before bailing.
                drop(child);
                return Err(e);
            }
        };

        let api = FirecrackerClient::new(&socket);

        // For UFFD restore, spawn the handler BEFORE PUT /snapshot/load
        // so it's listening when Firecracker connects. The handler
        // takes ownership of the kernel UFFD via SCM_RIGHTS, mmaps
        // memory.bin, and pages it in lazily. Either way the VM is
        // running by the time `load_snapshot*` returns (resume_vm: true).
        let load_result: Result<Option<Child>, SandboxError> = match self.config.restore_mode {
            RestoreMode::File => api
                .load_snapshot(&SnapshotPaths {
                    state_path: state_path.clone(),
                    mem_path: mem_path.clone(),
                })
                .await
                .map(|_| None),
            RestoreMode::Uffd => {
                // ADR 0007: the handler reads its memory manifests
                // from the chunk store. Without `memory_manifest` on
                // the snapshot we have nothing to hand it — refuse
                // loud rather than fall back to a degenerate path
                // that would silently lose the chunked benefits.
                let session_ref = match manifest.memory_manifest {
                    Some(r) => r,
                    None => {
                        drop(child);
                        if let Some(setup) = net_setup.as_ref() {
                            net::teardown(setup, &self.net_allocator).await;
                        }
                        return Err(SandboxError::Snapshot(
                            "RestoreMode::Uffd requires manifest.memory_manifest \
                             (snapshot wasn't taken via PooledBackend with a \
                             chunk_store attached; either wrap the FC backend \
                             with PooledBackend.with_chunk_store(...) before \
                             snapshotting, or switch to RestoreMode::File)"
                                .into(),
                        ));
                    }
                };
                // No bake-time canonical yet → reuse the session ref
                // as canonical. Resolver returns `Canonical` for every
                // fault (canonical == session at every chunk hash),
                // local memory.bin mmap serves the bytes, no chunk
                // store I/O at runtime. The bake-time canonical-base
                // slice will diverge these.
                let canonical_ref = manifest.canonical_memory_manifest.unwrap_or(session_ref);
                let uffd_uds = jail_dir.join("uffd.sock");
                let _ = tokio::fs::remove_file(&uffd_uds).await;
                // ADR 0007 Phase 5: replay the snapshotting
                // host's recorded trace if both ends opted in.
                // Pre-fault host comes from the sidecar JSON (set
                // at snapshot time by the host that captured
                // memory.bin); publish-trace host is the current
                // host's id from FC config (so the recorder
                // republishes under THIS host's key — subsequent
                // restores on this host use the local trace).
                let prefault_host = manifest.trace_host_hint.map(|hid| hid.as_uuid());
                let publish_host = self.config.host_id.map(|hid| hid.as_uuid());
                match self
                    .spawn_uffd_handler(
                        &uffd_uds,
                        &mem_path,
                        canonical_ref,
                        session_ref,
                        prefault_host,
                        publish_host,
                        jail_dir,
                    )
                    .await
                {
                    Ok(handler) => api
                        .load_snapshot_uffd(&state_path, &uffd_uds)
                        .await
                        .map(|_| Some(handler)),
                    Err(e) => Err(e),
                }
            }
        };

        let uffd_handler = match load_result {
            Ok(h) => h,
            Err(e) => {
                // Snapshot load failed: tear down the TAP we provisioned
                // and free the /30 slot before propagating. Otherwise a
                // failed restore leaks host-side network state.
                if let Some(setup) = net_setup.as_ref() {
                    net::teardown(setup, &self.net_allocator).await;
                }
                drop(child);
                return Err(e);
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
        // ADR 0009 §4: supervisor watches restored FC + (optional)
        // UFFD handler. The UFFD handler is critical — if it dies
        // mid-restore the FC process page-faults forever; we want
        // the entry pruned so reconcile transitions the session.
        let fc_pid = child.id();
        let uffd_pid = uffd_handler.as_ref().and_then(|c| c.id());
        self.sandboxes.insert(
            sandbox_id,
            LiveSandbox {
                state,
                child: Some(child),
                fc_pid,
                uffd_handler,
                net: net_setup,
                guest_ip: parking_lot::Mutex::new(None),
            },
        );
        if let Some(pid) = fc_pid {
            spawn_process_supervisor(self.sandboxes.clone(), sandbox_id, pid, "firecracker");
        }
        if let Some(pid) = uffd_pid {
            spawn_process_supervisor(self.sandboxes.clone(), sandbox_id, pid, "uffd-handler");
        }
        tracing::info!(
            %sandbox_id,
            jail = %jail_dir.display(),
            from = %snapshot_dir.display(),
            mode = ?self.config.restore_mode,
            "firecracker microVM restored from snapshot",
        );
        Ok(())
    }

    /// Re-allocate the manifest's /30 slot and recreate the TAP under
    /// the original name. Falls back to `Ok(None)` (no egress) when
    /// the slot is already in use on this host — that's the cross-host
    /// cold-resume case where the receiving host's allocator may have
    /// handed the slot to another VM. `Ok(None)` is also returned when
    /// the manifest predates the net field (older snapshots) or when
    /// the source VM was created with networking disabled.
    async fn reserve_restored_net(
        &self,
        snap: Option<&FcNetSnapshot>,
    ) -> Result<Option<net::NetSetup>, SandboxError> {
        let Some(snap) = snap else {
            return Ok(None);
        };
        if self.config.net_pool.is_none() {
            // Receiving host has networking disabled (test mode).
            // Nothing to reserve, nothing to provision.
            return Ok(None);
        }
        let cidr = net::VmCidr::new(snap.cidr_network);
        match self.net_allocator.lock().reserve(cidr) {
            Ok(_) => {}
            Err(net::AllocError::SlotTaken) => {
                tracing::warn!(
                    cidr = %cidr.cidr_str(),
                    tap = %snap.tap_name,
                    "restore: original /30 already in use on this host; \
                     restored sandbox will have no egress until re-created",
                );
                return Ok(None);
            }
            Err(e) => {
                return Err(SandboxError::Vm(
                    format!("reserve restored /30 {}: {e}", cidr.cidr_str()).into(),
                ));
            }
        }
        // Slot reserved. If TAP creation fails, return the slot to the
        // allocator so the next restore attempt can try again rather
        // than perpetually failing with SlotTaken.
        match net::provision_with_named_tap(cidr, &snap.tap_name).await {
            Ok(setup) => Ok(Some(setup)),
            Err(e) => {
                self.net_allocator.lock().free(cidr);
                Err(SandboxError::Vm(
                    format!("recreate TAP {}: {e}", snap.tap_name).into(),
                ))
            }
        }
    }
}

fn vm_err(msg: impl Into<String>) -> SandboxError {
    SandboxError::Vm(msg.into().into())
}

/// ADR 0014: re-install the canonical rootfs + harness symlinks for
/// a restored sandbox. The symlinks live under
/// `<work_dir>/{rootfs,harness}/<source_sandbox_id>.{dev,ext4}` —
/// keyed by the **source** sandbox_id because that's what
/// `state.bin` embedded as `path_on_host`, not the new restored
/// sandbox_id. Idempotent.
async fn restore_canonical_symlinks(
    work_dir: &Path,
    manifest: &FcSnapshotManifest,
) -> Result<(), SandboxError> {
    for parent in paths::canonical_parent_dirs(work_dir) {
        tokio::fs::create_dir_all(&parent).await.map_err(|e| {
            vm_err(format!(
                "create canonical parent dir {}: {e}",
                parent.display()
            ))
        })?;
    }
    if let Some(rootfs_target) = manifest.spec.rootfs_source.as_ref() {
        // Always create the host's own canonical-path symlink — it
        // keeps the same-host idle-resume path identical to a
        // freshly-snapshotted sandbox, and stays correct under our
        // own naming convention.
        let canonical = paths::rootfs_canonical(work_dir, manifest.sandbox_id);
        paths::install_symlink(&canonical, rootfs_target)
            .await
            .map_err(|e| {
                vm_err(format!(
                    "restore rootfs canonical symlink {} -> {}: {e}",
                    canonical.display(),
                    rootfs_target.display()
                ))
            })?;
        // ADR 0014 M1.11: cross-host restore. FC's state.bin embeds
        // whatever path the bake's PUT /drives used as `path_on_host`
        // — that's the bake's `tempfile::tempdir()/rootfs/<src>.dev`,
        // which doesn't exist on the receiver. If the bake recorded
        // its canonical path on the manifest, recreate the file there
        // too (mkdir parent + symlink) so FC `load_snapshot` finds
        // the drive at the absolute path it expects.
        if let Some(source_canonical) = manifest.source_rootfs_canonical.as_ref() {
            if source_canonical != &canonical {
                if let Some(parent) = source_canonical.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        vm_err(format!(
                            "create source rootfs canonical parent {}: {e}",
                            parent.display()
                        ))
                    })?;
                }
                paths::install_symlink(source_canonical, rootfs_target)
                    .await
                    .map_err(|e| {
                        vm_err(format!(
                            "restore source rootfs canonical symlink {} -> {}: {e}",
                            source_canonical.display(),
                            rootfs_target.display()
                        ))
                    })?;
            }
        }
    }
    if let Some(harness_target) = manifest.spec.harness_substrate.as_ref() {
        let canonical = paths::harness_canonical(work_dir, manifest.sandbox_id);
        paths::install_symlink(&canonical, harness_target)
            .await
            .map_err(|e| {
                vm_err(format!(
                    "restore harness canonical symlink {} -> {}: {e}",
                    canonical.display(),
                    harness_target.display()
                ))
            })?;
        if let Some(source_canonical) = manifest.source_harness_canonical.as_ref() {
            if source_canonical != &canonical {
                if let Some(parent) = source_canonical.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        vm_err(format!(
                            "create source harness canonical parent {}: {e}",
                            parent.display()
                        ))
                    })?;
                }
                paths::install_symlink(source_canonical, harness_target)
                    .await
                    .map_err(|e| {
                        vm_err(format!(
                            "restore source harness canonical symlink {} -> {}: {e}",
                            source_canonical.display(),
                            harness_target.display()
                        ))
                    })?;
            }
        }
    }
    Ok(())
}

/// ADR 0009 §6: branch destroy's wait-for-exit on whether we own a
/// `tokio::process::Child` (newly-created sandboxes) or only the
/// recorded pid (pidfd-reattached sandboxes). For `Some(child)` use
/// the proper `Child::wait` to reap. For `None` poll
/// `kill(pid, 0)` until ESRCH.
async fn wait_for_fc_exit(child: &mut Option<Child>, pid: Option<u32>) -> std::io::Result<()> {
    if let Some(c) = child.as_mut() {
        c.wait().await.map(|_| ())
    } else if let Some(pid) = pid {
        wait_for_pid_death(pid, Duration::from_secs(30)).await
    } else {
        Ok(())
    }
}

/// SIGKILL the FC process. For owned children, drive via tokio's
/// `Child::kill`. For reattached (no Child), shoot the pid directly.
async fn kill_fc(child: &mut Option<Child>, pid: Option<u32>, id: SandboxId) {
    if let Some(c) = child.as_mut() {
        if let Err(e) = c.kill().await {
            tracing::warn!(sandbox_id = %id, error = %e, "firecracker Child::kill failed");
        }
        return;
    }
    if let Some(pid) = pid {
        // SAFETY: `libc::kill` with SIGKILL on a pid we recorded is
        // a basic process-control syscall. We aren't relying on its
        // result to maintain any internal invariant — failure is
        // logged and we continue.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            // ESRCH is fine — process already gone.
            if errno != libc::ESRCH {
                tracing::warn!(
                    sandbox_id = %id,
                    %pid,
                    errno,
                    "libc::kill(SIGKILL) failed; FC may linger"
                );
            }
        }
    }
}

/// Poll `kill(pid, 0)` until ESRCH or timeout. Returns Ok(()) on
/// exit, error on timeout. 50 ms poll interval — fast enough that
/// destroy doesn't perceive lag, slow enough that we don't burn
/// CPU in a tight loop.
async fn wait_for_pid_death(pid: u32, timeout: Duration) -> std::io::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // SAFETY: kill(pid, 0) inspects-but-doesn't-mutate; see
        // the SAFETY comment in `spawn_process_supervisor`.
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno == libc::ESRCH {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("pid {pid} did not exit within {timeout:?}"),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// ADR 0009 §4: per-VM process supervisor. Spawned at create/restore
/// time for each FC process (and the UFFD handler when present). Polls
/// `kill(pid, 0)` every 1s; when the pid disappears (`ESRCH`), prunes
/// the sandbox from the host's in-memory map so `backend.list()` no
/// longer reports a phantom. The reconcile pass (ADR 0009 §1-§3)
/// then catches the absence on the next heartbeat tick and flips the
/// owning session per the §3 policy.
///
/// Polling vs `child.wait()`: `wait()` requires `&mut Child`, which
/// can't be shared with the existing destroy path. Polling is simpler
/// and has acceptable latency (1s detection + 15s reconcile grace =
/// ~16s session-loss visibility, vs minutes-to-forever today).
///
/// Polling vs pidfd_open: pidfd is more accurate (immune to PID
/// recycling) but requires Linux 5.3+. We use pidfd for Phase 6's
/// reattach across host-agent restart where correctness depends on
/// it; this supervisor is best-effort and tolerates the edge case
/// where the OS recycles a PID before we notice (reconcile still
/// catches eventually).
///
/// The watcher pings every 1s instead of every 100ms to keep host
/// load negligible: with N sandboxes the per-second syscall rate is
/// N × 1. For a fully-loaded 50-sandbox host that's 50 syscalls/sec
/// — invisible against the FC API + vsock I/O.
fn spawn_process_supervisor<V: Send + Sync + 'static>(
    sandboxes: Arc<DashMap<SandboxId, V>>,
    sandbox_id: SandboxId,
    pid: u32,
    role: &'static str,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // `kill(pid, 0)` is a no-op signal that returns 0 if the
            // caller has permission to signal a process with that
            // pid, -1 with ESRCH if the process doesn't exist. Same
            // uid (we spawned the process) so permission is granted
            // as long as the pid is live.
            //
            // SAFETY: `libc::kill` with `sig=0` is provably safe.
            // The signature is `unsafe extern "C"` because libc can't
            // express "this signal value never mutates kernel state,"
            // but `0` is documented (man 2 kill) as the no-op /
            // existence-check signal: returns 0 if a process with the
            // given pid exists AND the caller could signal it,
            // errno=ESRCH if not. No side effects, no UB.
            let rc = unsafe { libc::kill(pid as i32, 0) };
            if rc == 0 {
                continue;
            }
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::ESRCH {
                // EPERM or some other unexpected error — log and
                // bail. EPERM here would mean the pid was recycled
                // to a process we can no longer signal; treat that
                // as effectively gone.
                tracing::warn!(
                    %sandbox_id,
                    %pid,
                    role,
                    errno,
                    "process supervisor: kill(0) returned non-ESRCH error; treating as gone"
                );
            }
            // Process exited unexpectedly (kernel OOM, segfault,
            // manual kill, etc.) — destroy() would have removed
            // the entry already, so this is the "supervisor wins
            // the race" path.
            if sandboxes.remove(&sandbox_id).is_some() {
                tracing::warn!(
                    %sandbox_id,
                    %pid,
                    role,
                    "host-side VM supervisor (ADR 0009 §4) pruned unexpectedly-dead sandbox"
                );
            } else {
                tracing::debug!(
                    %sandbox_id,
                    %pid,
                    role,
                    "process supervisor: entry already removed by destroy; no-op"
                );
            }
            return;
        }
    });
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
        let sandbox_id = SandboxId::new();
        let jail_dir = self.work_dir.join(sandbox_id.to_string());

        // Two-pipe observability: span for forensics (one
        // log line per attempt, with sandbox_id), histogram for
        // aggregates (p50/p99 boot latency across many attempts,
        // partitioned by outcome). Phase-level breakdown (image
        // pull / materialize / fc_boot / agent_handshake) is the
        // next iteration — for now this gives us total-cycle
        // numbers we don't have today.
        let start = std::time::Instant::now();
        let span = tracing::info_span!("fc.create", %sandbox_id);
        let _guard = span.enter();

        let result = self.create_in_jail(sandbox_id, &jail_dir, spec).await;
        let elapsed = start.elapsed().as_secs_f64();

        let outcome = match &result {
            Ok(()) => "success",
            Err(SandboxError::InvalidSpec(_)) => "invalid_spec",
            Err(_) => "fc_error",
        };
        metrics::histogram!(
            "engram_sandbox_boot_seconds",
            "phase" => "fc_boot",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(elapsed);
        metrics::counter!(
            "engram_sandbox_create_total",
            "outcome" => outcome,
        )
        .increment(1);
        tracing::info!(
            elapsed_ms = (elapsed * 1000.0) as u64,
            outcome,
            "fc create complete",
        );

        match result {
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

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        // Read sandbox state under the dashmap guard, drop guard before
        // any await so we don't hold the read lock across an HTTP call.
        let (socket, spec, net_snapshot) = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            let net_snapshot = live.net.as_ref().map(|setup| FcNetSnapshot {
                tap_name: setup.tap_name.clone(),
                cidr_network: setup.vm_cidr.network(),
            });
            (
                live.state.firecracker_socket.clone(),
                live.state.spec.clone(),
                net_snapshot,
            )
        };

        // ADR 0014: refuse to snapshot a sandbox whose canonical
        // rootfs symlink is missing or dangling. FC's `state.bin`
        // embeds `<work_dir>/rootfs/<sandbox_id>.dev` as
        // `path_on_host`; capture-time validation prevents shipping
        // a blob that fails opaquely at restore on a sibling host.
        paths::assert_rootfs_canonical(&self.work_dir, id)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("non-canonical jail layout: {e}")))?;

        // ADR 0007 Phase 6: allocate the snapshot id first, derive
        // the staging dir from it. Coord no longer dictates layout.
        let snapshot_id = SnapshotId::new();
        let dest = self.snapshot_dir_for(snapshot_id);
        tokio::fs::create_dir_all(&dest).await.map_err(|e| {
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
        let paths = api.create_snapshot(&dest).await?;

        let created_at = Utc::now();
        let source_rootfs_canonical = if spec.rootfs_source.is_some() {
            Some(paths::rootfs_canonical(&self.work_dir, id))
        } else {
            None
        };
        let source_harness_canonical = if spec.harness_substrate.is_some() {
            Some(paths::harness_canonical(&self.work_dir, id))
        } else {
            None
        };
        let manifest = FcSnapshotManifest {
            sandbox_id: id,
            created_at,
            spec: spec.clone(),
            net: net_snapshot,
            format: MANIFEST_FORMAT_FC.into(),
            // PooledBackend::snapshot patches `memory_manifest`
            // in-place after FC returns (the bare backend can't
            // chunk memory.bin without a chunk-store wiring).
            memory_manifest: None,
            // ADR 0007 Phase 5: canonical-base memory manifest
            // lifted from the image bundle. Set on session create
            // by PooledBackend; carried on the spec through every
            // snapshot. UFFD handler `mmap`s the canonical file
            // and serves shared-canonical reads from page cache.
            canonical_memory_manifest: spec.canonical_memory_manifest,
            // ADR 0007 Phase 5: snapshotting host's id, so cross-
            // host restore can request this host's recorded
            // trace via `--prefault-trace <hint>`. Set from FC
            // config; falls back to None when not wired.
            trace_host_hint: self.config.host_id,
            // ADR 0014 M1.11: canonical paths embedded in FC's
            // state.bin. Cross-host restore reads these to recreate
            // the EXACT path FC tries to open at load_snapshot
            // time (state.bin has the bake's absolute path baked
            // in; the receiver's own work_dir is a different
            // location and wouldn't satisfy FC).
            source_rootfs_canonical,
            source_harness_canonical,
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
            id: snapshot_id,
            size_bytes,
            created_at,
            image_version: spec.image,
            // FC snapshots capture VM state + memory only; disk state
            // lives on the per-sandbox rootfs file. Phase 4's NBD work
            // produces a disk_manifest here when it lands.
            disk_manifest: None,
            // FC backend's bare snapshot writes memory.bin to disk
            // and stops there. `PooledBackend::snapshot` is the
            // integration point that chunks memory.bin into the
            // chunk store and patches this field afterward — that
            // way the FC backend stays chunk-store-agnostic and
            // dev/test paths don't need a chunk-store wiring.
            memory_manifest: None,
            // ADR 0014: portable-snapshot fields are populated by
            // `PooledBackend::snapshot` after the inner backend
            // returns. Bare FC stays BlobStorage-agnostic.
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
        })
    }

    fn snapshot_path_for(&self, snapshot_id: SnapshotId) -> PathBuf {
        self.snapshot_dir_for(snapshot_id)
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        // ADR 0007 Phase 6: backend looks up its own staging dir.
        let src = self.snapshot_dir_for(metadata.id);
        let manifest_bytes = tokio::fs::read(src.join("manifest.json"))
            .await
            .map_err(|e| SandboxError::Snapshot(format!("read manifest: {e}")))?;
        let manifest: FcSnapshotManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| SandboxError::Snapshot(format!("manifest parse: {e}")))?;

        // Reject cross-VMM restores fast: a VZ blob (`format == "vz"`)
        // would otherwise reach load_snapshot and fail with a
        // confusing FC parse error on state.bin. Empty string accepted
        // for snapshots written before the format field landed; once
        // those have rotated out a future cleanup can drop the empty
        // case.
        if !matches!(manifest.format.as_str(), MANIFEST_FORMAT_FC | "") {
            return Err(SandboxError::Snapshot(format!(
                "manifest format {:?} is not 'fc' — cross-VMM restore not supported",
                manifest.format,
            )));
        }

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
        // ADR 0009 §6: `child` is `None` for sandboxes pidfd-reattached
        // on host-agent startup (we don't own a `Child` for those —
        // the original process was spawned by a previous host-agent
        // generation). For those, we fall back to libc::kill +
        // poll-for-exit since there's no `Child::wait` to drive.
        let graceful = async {
            api.put_action(ActionType::SendCtrlAltDel).await?;
            wait_for_fc_exit(&mut live.child, live.fc_pid)
                .await
                .map_err(SandboxError::from)
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
                kill_fc(&mut live.child, live.fc_pid, id).await;
                let _ = wait_for_fc_exit(&mut live.child, live.fc_pid).await;
            }
            Err(_) => {
                // Timeout: guest didn't honour CAD within the grace
                // window. Force-kill.
                tracing::debug!(
                    sandbox_id = %id,
                    timeout = ?GRACEFUL_SHUTDOWN_TIMEOUT,
                    "firecracker didn't exit in time; SIGKILLing",
                );
                kill_fc(&mut live.child, live.fc_pid, id).await;
                let _ = wait_for_fc_exit(&mut live.child, live.fc_pid).await;
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

        // ADR 0014: canonical rootfs / harness symlinks at
        // `<work_dir>/{rootfs,harness}/<sandbox_id>.{dev,ext4}`
        // live outside the jail by design. Remove them explicitly;
        // any restore that wants this sandbox_id back will re-create
        // them pointing at whatever it has materialized locally.
        for entry in paths::canonical_entries_for(&self.work_dir, id) {
            // NotFound is benign — sandbox may have been created
            // without a harness substrate, or pre-ADR-0014 (no entry
            // at all). `remove_file` on a symlink unlinks the entry,
            // not the target.
            let _ = tokio::fs::remove_file(&entry).await;
        }

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
            // Fast path: the /30 allocator assigned the guest a
            // deterministic .2 from the network address. We don't
            // need to dial agentd to learn what we already know.
            // Skips a ~2s vsock RTT on every fresh-session
            // `notify_session_policy` call — critical because the
            // coord-side policy registration races the agent's boot.
            if let Some(net) = live.net.as_ref() {
                let ip = net.vm_cidr.guest().to_string();
                *live.guest_ip.lock() = Some(ip.clone());
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

    /// ADR 0014 M1.12: brief pause → `PATCH /drives` → resume on
    /// the harness virtio-blk drive. Warm-pool lease path uses
    /// this to swap the bake-time stub harness for the session's
    /// chosen harness ext4 just before `start_agent` dials
    /// bootstrap. ~30ms wall-clock on FC.
    async fn swap_harness_drive(
        &self,
        id: SandboxId,
        new_path: std::path::PathBuf,
    ) -> Result<(), SandboxError> {
        let api_sock = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.firecracker_socket.clone()
        };
        // Re-install the canonical symlink so state.bin's embedded
        // path resolves on the next guest read. The session's
        // harness ext4 lives in the host's image_cache; we point
        // the canonical path at it.
        let harness_canonical = paths::harness_canonical(&self.work_dir, id);
        if let Err(e) = paths::install_symlink(&harness_canonical, &new_path).await {
            return Err(SandboxError::Vm(
                format!("install harness symlink for swap: {e}").into(),
            ));
        }
        let api = FirecrackerClient::new(&api_sock);
        api.patch_vm_state(VmState::Paused).await?;
        let patch_result = api.patch_drive("harnesses", &harness_canonical).await;
        // Always try to resume so a partial failure doesn't leave
        // the VM paused. The patch error (if any) wins.
        let resume_result = api.patch_vm_state(VmState::Resumed).await;
        patch_result?;
        resume_result?;
        Ok(())
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        agent: engram_core::types::sandbox::AgentSpec,
    ) -> Result<(), SandboxError> {
        // `agent_handshake` wall-clock is recorded by the caller
        // (`grpc_server::start_agent` for cold-create, `WarmPool::
        // launch` for warm-lease) so the histogram can carry the
        // `kind` label without this backend method knowing which
        // path it's serving. The tracing log here is still useful
        // for per-sandbox forensics.
        let phase_start = std::time::Instant::now();
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
        //
        // Deadline is generous because cold chunked-NBD rootfs/
        // workspace mounts can take 15-25s to page in their first
        // blocks from GCS; engram-init blocks on those mounts
        // before exec'ing the bootstrap binary, so the vsock
        // listener doesn't appear until after the mounts complete.
        // After the chunks land in the host's local cache,
        // subsequent boots finish in well under a second; this
        // ceiling only matters on the cold path.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
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
        match tokio::time::timeout(Duration::from_secs(60), conn.read_exact(&mut marker)).await {
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
                    "timed out waiting for bootstrap ready marker (60s)".into(),
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
        // ADR 0014 M1.12 (option D): bootstrap mounts the harness
        // device, not engram-init. Only sandboxes with an attached
        // harness substrate get the mount instruction — sandboxes
        // booted without a harness (rare, but supported for
        // `kind = none` sessions) leave both fields None so bootstrap
        // skips the mount step.
        let has_harness = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.spec.harness_substrate.is_some()
        };
        let (harness_dev, harness_mount) = if has_harness {
            (
                Some("/dev/vdb".to_string()),
                Some("/run/engram/harnesses".to_string()),
            )
        } else {
            (None, None)
        };
        let launch = engram_harness_proto::BootstrapLaunch {
            argv: agent.argv,
            env: agent.env.into_iter().collect(),
            harness_dev,
            harness_mount,
        };
        engram_harness_proto::write_msg(&mut conn, &launch)
            .await
            .map_err(|e| SandboxError::Vm(format!("write BootstrapLaunch: {e}").into()))?;
        // Best-effort flush; bootstrap closes its end on exec.
        let _ = conn.shutdown().await;
        let elapsed = phase_start.elapsed().as_secs_f64();
        tracing::info!(
            sandbox_id = %id,
            elapsed_ms = (elapsed * 1000.0) as u64,
            "fc agent handshake complete",
        );
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
            egress_dns_port: None,
            host_id: None,
            uffd_cache_root: None,
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
            canonical_memory_manifest: None,
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
        match b.snapshot(SandboxId::new()).await {
            Err(SandboxError::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn restore_with_missing_manifest_errors_cleanly() {
        // Restoring from a fresh metadata whose snapshot id was
        // never persisted shouldn't try to spawn firecracker —
        // manifest read is the first step and it should fail loudly
        // with a Snapshot error.
        let (b, _d) = backend();
        let metadata = SnapshotMetadata {
            id: SnapshotId::new(),
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
        };
        match b.restore(metadata).await {
            Err(SandboxError::Snapshot(msg)) => {
                assert!(
                    msg.contains("manifest"),
                    "error should reference the missing manifest: {msg}",
                );
            }
            other => panic!("expected Snapshot error, got {other:?}"),
        }
    }

    #[test]
    fn manifest_without_net_field_still_deserializes() {
        // Old snapshots written before the `net` field landed must
        // still parse — `serde(default)` lets `net` default to None.
        let json = r#"{
            "sandbox_id": "00000000-0000-0000-0000-000000000000",
            "created_at": "2026-01-01T00:00:00Z",
            "spec": {
                "image": "warm-test",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 256},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            }
        }"#;
        let manifest: FcSnapshotManifest = serde_json::from_str(json).unwrap();
        assert!(manifest.net.is_none());
    }

    #[test]
    fn manifest_with_net_field_round_trips() {
        let manifest = FcSnapshotManifest {
            sandbox_id: SandboxId::new(),
            created_at: Utc::now(),
            spec: spec(),
            net: Some(FcNetSnapshot {
                tap_name: "tap-engr-abcdef".into(),
                cidr_network: std::net::Ipv4Addr::new(10, 200, 0, 4),
            }),
            format: MANIFEST_FORMAT_FC.into(),
            memory_manifest: None,
            canonical_memory_manifest: None,
            trace_host_hint: None,
            source_rootfs_canonical: None,
            source_harness_canonical: None,
        };
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let parsed: FcSnapshotManifest = serde_json::from_slice(&bytes).unwrap();
        let net = parsed.net.expect("net field should round-trip");
        assert_eq!(net.tap_name, "tap-engr-abcdef");
        assert_eq!(net.cidr_network, std::net::Ipv4Addr::new(10, 200, 0, 4));
        assert_eq!(parsed.format, MANIFEST_FORMAT_FC);
    }

    #[tokio::test]
    async fn restore_rejects_vz_format_manifest() {
        // A blob accidentally tagged with VZ's format must surface
        // a clear cross-VMM error rather than reaching FC's
        // load_snapshot with garbage state.bin contents.
        let (b, _d) = backend();
        let manifest = serde_json::json!({
            "sandbox_id": "00000000-0000-0000-0000-000000000000",
            "created_at": "2026-01-01T00:00:00Z",
            "spec": {
                "image": "warm-test",
                "rootfs_source": null,
                "image_uri": null,
                "harness_pack_uri": null,
                "cpu": {"vcpus": 1},
                "memory": {"max_mib": 256},
                "disk": {"max_gib": 1},
                "ttl": null,
                "env": {},
                "workdir": null,
                "harness_substrate": null,
                "network": {}
            },
            "format": "vz"
        });
        // Phase 6: backend owns its staging dir. Plant the VZ-tagged
        // manifest where the backend will look for snapshot_id.
        let snapshot_id = SnapshotId::new();
        let snap_path = b.snapshot_path_for(snapshot_id);
        tokio::fs::create_dir_all(&snap_path).await.unwrap();
        tokio::fs::write(
            snap_path.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .await
        .unwrap();
        let metadata = SnapshotMetadata {
            id: snapshot_id,
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
        };
        match b.restore(metadata).await {
            Err(SandboxError::Snapshot(msg)) => {
                assert!(
                    msg.contains("not 'fc'") && msg.contains("cross-VMM"),
                    "expected cross-VMM error, got: {msg}",
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

    /// ADR 0009 §4: the supervisor must prune the entry from the
    /// map when the watched pid disappears. Uses a real subprocess
    /// (`sleep 600`) so the kill-detection is exercised end-to-end:
    /// we kill the process, then assert the entry leaves the map
    /// within the polling latency.
    #[tokio::test]
    async fn supervisor_prunes_entry_when_pid_disappears() {
        let sandboxes: Arc<DashMap<SandboxId, ()>> = Arc::new(DashMap::new());
        let id = SandboxId::new();
        sandboxes.insert(id, ());

        // Spawn a real child we can kill. `sleep 600` blocks the
        // child for 10 minutes; we kill it explicitly below.
        let mut child = tokio::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child has pid");

        spawn_process_supervisor(sandboxes.clone(), id, pid, "test-sleep");

        // Initial: supervisor sees the pid alive, doesn't prune.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            sandboxes.contains_key(&id),
            "supervisor must not prune while pid is alive"
        );

        // Kill the child. After ~1s the supervisor's next poll
        // sees ESRCH and prunes.
        child.kill().await.expect("kill child");
        // Reap so we don't leak a zombie.
        let _ = child.wait().await;

        // Wait up to 3s for the supervisor to notice. The poll
        // interval is 1s + reap latency.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !sandboxes.contains_key(&id) {
                return; // success
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("supervisor did not prune within 3s of pid death");
    }

    /// If the entry is already removed (e.g. destroy() got there
    /// first) the supervisor's later poll-and-prune is a no-op.
    /// Belt-and-suspenders against double-remove logic errors.
    #[tokio::test]
    async fn supervisor_removal_after_destroy_is_a_noop() {
        let sandboxes: Arc<DashMap<SandboxId, ()>> = Arc::new(DashMap::new());
        let id = SandboxId::new();
        // Don't insert — simulates the entry already being gone.

        let mut child = tokio::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child has pid");
        spawn_process_supervisor(sandboxes.clone(), id, pid, "test-sleep");

        child.kill().await.expect("kill child");
        let _ = child.wait().await;

        // Wait long enough for the supervisor to have polled at
        // least twice; the map should remain empty (no panic, no
        // weird state).
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!sandboxes.contains_key(&id));
    }
}
