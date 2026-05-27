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
    /// The rootfs drive's `path_on_host` as FC actually has it open —
    /// i.e. the absolute path embedded in this VM's `state.bin`. For a
    /// cold-created sandbox this is `rootfs/<own_id>.dev`; for a
    /// restored one it is the ANCESTOR's path (restore never re-points
    /// the root drive — FC inherits the snapshot's embedded path). ADR
    /// 0018 commit 12o: `snapshot()` stamps `source_rootfs_canonical`
    /// from this, not from a path recomputed off the live id, so a
    /// chained snapshot lineage records the path FC really opens. The
    /// per-restore id is just a routing handle; device paths are
    /// anchored to the lineage's original identity. Mirrors
    /// `vsock_uds_path`, which is immutable post-load (PUT /vsock 400)
    /// and so MUST carry the embedded path forward.
    pub rootfs_canonical: PathBuf,
}

/// Reserved vsock port `engram-agentd` listens on inside the guest.
pub const ENGRAM_AGENTD_PORT: u32 = 1024;

/// Filename FC uses for the per-port host-side UDS when the guest
/// dials out via vsock: `<base>_<port>`. Build the path for an
/// arbitrary port — used by the harness-event listener (1026) and
/// the agentd-ready listener (1027). See `firecracker/docs/vsock.md`
/// for the protocol.
fn per_port_uds(base_vsock_uds: &Path, port: u32) -> PathBuf {
    let file_name = base_vsock_uds
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let with_port = format!("{file_name}_{port}");
    match base_vsock_uds.parent() {
        Some(parent) => parent.join(with_port),
        None => PathBuf::from(with_port),
    }
}

fn harness_uds_for(base_vsock_uds: &Path) -> PathBuf {
    per_port_uds(base_vsock_uds, engram_harness_proto::HARNESS_VSOCK_PORT)
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
    /// ADR 0019: optional OTLP collector endpoint reachable *from inside
    /// the guest* (e.g. `http://<tap-gateway-ip>:4317`). When set, cold-boot
    /// `boot_args` carry `engram_otel=<this>` so the in-guest agentd exports
    /// its boot spans to the same trace as the host. `None` = no in-guest
    /// export (the default; restore reuses the snapshot's embedded cmdline).
    pub guest_otel_endpoint: Option<String>,
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
    /// ADR 0014 M1.12 (option D): host-local stub harness ext4 used
    /// as the symlink target for warm-pool restores. State.bin
    /// embeds the bake's harness substrate path, which doesn't exist
    /// on the receiver; `restore_canonical_symlinks` redirects both
    /// host-canonical and source-canonical harness paths at this
    /// local file instead. Required to be present + a real ext4 so
    /// FC `load_snapshot` can open it as a virtio-blk device. Caller
    /// (host-agent on startup, image-builder during bake) is
    /// responsible for materializing the file. The session's real
    /// harness is patched in via `swap_harness_drive` at warm-lease.
    pub stub_harness_path: Option<PathBuf>,
    /// ADR 0014 M1.14: when set, the UFFD handler dumps its recorded
    /// `WorkingSetTrace` to this file on clean shutdown. Used by the
    /// image-builder's synthetic profile pass: after bake's primary
    /// snapshot is taken, a second FC restore runs with this set so
    /// the bake driver can read the trace back from disk + stage it
    /// as an OCI layer. `None` in production (warm-pool refill
    /// doesn't need to dump; the publish-trace-host path is
    /// sufficient for runtime).
    pub working_set_trace_output: Option<PathBuf>,
    /// ADR 0014 M1.14: bake-side override for the UFFD handler's
    /// blob root. The bake's chunk store lives at
    /// `<images_dir>/store/` rather than the runtime convention
    /// `<root>/blobs/`; without this the handler's env-var-driven
    /// resolution can't find the freshly-chunked memory bytes.
    /// `None` in production — runtime resolution is correct there.
    pub uffd_blob_root: Option<PathBuf>,
    /// FC CPU template name passed verbatim to `PUT /machine-config`.
    /// `None` = host passthrough (FC default; guest CPUID reflects
    /// the underlying physical CPU). `Some("T2CL")` masks to a
    /// Cascade Lake baseline so snapshots taken on one host restore
    /// cleanly on a host with a different CPU vendor/family. Prod
    /// host-agent + the bake-time FC VM should pin the SAME value;
    /// resolved at startup via [`cpu_template_from_env`]. `None`
    /// here keeps tests (which boot fresh VMs on whatever CI CPU is
    /// available) opt-out by default — only the prod call sites
    /// in `engram-coordinator` and `engram-image-builder` set it.
    pub cpu_template: Option<String>,
}

/// Resolve the FC CPU template from `ENGRAM_FC_CPU_TEMPLATE`.
///
/// - Unset: `None` (passthrough — guest CPUID reflects the
///   underlying physical CPU). This is the safe default because
///   built-in FC templates are vendor-pinned (`T2CL`, `T2`, `T2S`,
///   `C3` are Intel-only; `T2A` is AMD-only). With AMD bake runners
///   (Blacksmith's x64 pool) and Intel prod hosts (GCP n2 Cascade
///   Lake), neither vendor's template loads on both sides — the
///   bake fails with "CPU vendor mismatched between actual CPU and
///   CPU template" if we try T2CL on AMD, or prod restore fails the
///   same way if we baked with T2A. The whole point of the template
///   was cross-vendor portability and the built-in set can't deliver
///   it; revisit once we have a same-vendor bake runner (Intel KVM-
///   capable EC2 via RunsOn, or a self-hosted GCP n2 runner) or a
///   custom JSON template that fakes `GenuineIntel` on both vendors.
/// - Empty string or `"none"` (case-insensitive): `None` — explicit
///   passthrough. Same as unset; here for ops symmetry.
/// - Any other value: `Some(value)` verbatim — opt-in for the
///   day we ship a same-vendor bake.
pub fn cpu_template_from_env() -> Option<String> {
    match std::env::var("ENGRAM_FC_CPU_TEMPLATE") {
        Err(_) => None,
        Ok(s) if s.is_empty() || s.eq_ignore_ascii_case("none") => None,
        Ok(s) => Some(s),
    }
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
            guest_otel_endpoint: None,
            firecracker_bin: PathBuf::from("firecracker"),
            uffd_handler_bin: PathBuf::from("engram-uffd-handler"),
            restore_mode: RestoreMode::File,
            host_id: None,
            uffd_cache_root: None,
            stub_harness_path: None,
            working_set_trace_output: None,
            uffd_blob_root: None,
            // Host-passthrough by default. Prod (`engram-coordinator`)
            // and the bake (`engram-image-builder`) both opt in to a
            // template via `cpu_template_from_env`. Tests stay opted
            // out so `cargo test` on heterogeneous CI runners doesn't
            // wedge if the runner CPU doesn't satisfy the template.
            cpu_template: None,
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
    /// ADR 0014 M1.16: warm-restore-only. When `Some`, the VM
    /// lives inside a per-VM netns; `net` is `None` and the
    /// host-visible IP is `netns.host_reachable_ip()` instead of
    /// `net.vm_cidr.guest()`. `destroy()` calls
    /// `net::teardown_netns` on this. Mutually exclusive with
    /// `net`: at most one is `Some` per `LiveSandbox`.
    netns: Option<net::NetnsSetup>,
    /// Cached IPv4 address discovered by querying agentd on first
    /// `guest_ip` call (mirrors VZ's pattern). Populated lazily
    /// because the agent's eth0 needs IP_PNP DHCP+kernel boot before
    /// it can answer.
    guest_ip: parking_lot::Mutex<Option<String>>,
    /// ADR 0015 M1: receiver for the agentd-readiness signal.
    /// Switches to `true` when the in-VM agentd successfully dials
    /// `<vsock_uds>_<ENGRAM_AGENTD_READY_PORT>` and writes its
    /// `AgentReady` frame. `start_agent` awaits this; restored
    /// sandboxes pre-set to `true` because agentd was already
    /// running when the snapshot was captured. Replaces the
    /// pre-M1 boot-race CONNECT-then-retry on port 1024.
    agent_ready: tokio::sync::watch::Receiver<bool>,
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
    /// The exact `vsock.uds_path` the bake's FC `PUT /vsock`'d, baked
    /// into state.bin. FC `load_snapshot` recreates the vsock UDS at
    /// this path; the host-agent must then dial that same path to
    /// reach the guest. Without this the host-agent dials
    /// `<host_work_dir>/<source_id>.vsock` which doesn't exist on a
    /// cross-host restore (the file FC actually opened lives at the
    /// bake-side path).
    #[serde(default)]
    source_vsock_canonical: Option<PathBuf>,
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
            // Only cold-created sandboxes write an on-disk manifest, so
            // a path-1 reattach only ever sees a cold lineage; the
            // persisted `rootfs_canonical` is `rootfs/<id>.dev`. Carry
            // it forward so a re-snapshot after restart still stamps
            // the embedded path FC has open.
            rootfs_canonical: fc.rootfs_canonical.clone(),
        };
        // Reattach: agentd was already up when the previous host-
        // agent generation tracked it, so the readiness watch starts
        // already-true. No listener spawned — there's nothing left to
        // wait for.
        let (_ready_tx, ready_rx) = tokio::sync::watch::channel(true);
        self.sandboxes.insert(
            id,
            LiveSandbox {
                state,
                child: None,
                fc_pid: Some(fc.process.pid),
                uffd_handler: None,
                net: net_setup,
                // pidfd-reattach is for pre-M1.16 sandboxes that
                // never used a per-VM netns; the field is always
                // None on this path.
                netns: None,
                guest_ip: parking_lot::Mutex::new(None),
                agent_ready: ready_rx,
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
    /// directly connected to the guest's listener on `port`. One-shot:
    /// callers that need to race the boot window are wrong — by ADR
    /// 0015 M1, the boot window is closed before any host-side
    /// dial-out by waiting on the per-sandbox `agent_ready` watch
    /// (filled when agentd dials the host's ready port).
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
    ///
    /// `netns_name`: when `Some`, FC is spawned via
    /// `ip netns exec <name> firecracker …` so the process (and
    /// any TAP it opens via `host_dev_name`) lives inside that
    /// netns. ADR 0014 M1.16 warm-restore path passes the
    /// per-VM netns here; cold path passes `None` and FC stays
    /// in host root, as today.
    async fn spawn_firecracker(
        &self,
        jail_dir: &Path,
        netns_name: Option<&str>,
    ) -> Result<(PathBuf, Child), SandboxError> {
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

        let fc_bin = self.config.firecracker_bin.to_string_lossy().into_owned();
        let socket_arg = socket.to_string_lossy().into_owned();
        let mut cmd = match netns_name {
            Some(ns) => {
                // `ip netns exec` forks + setns(CLONE_NEWNET) + exec
                // the given command — the FC child (and its eventual
                // PUT /network-interfaces host_dev_name lookup) all
                // resolve TAP names inside this netns.
                let mut c = Command::new("ip");
                c.args(["netns", "exec", ns, &fc_bin, "--api-sock", &socket_arg]);
                c
            }
            None => {
                let mut c = Command::new(&self.config.firecracker_bin);
                c.args(["--api-sock", &socket_arg]);
                c
            }
        };
        let mut child = cmd
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_clone))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                vm_err(format!(
                    "spawn {} (netns={:?}): {e}",
                    self.config.firecracker_bin.display(),
                    netns_name,
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
    #[tracing::instrument(name = "fc.spawn_uffd_handler", skip_all)]
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
        // ADR 0014 M1.14: when the caller wires `working_set_trace_output`,
        // it's the bake's profile pass — destroy removes jail_dir
        // immediately after, sweeping the handler's log with it. Park
        // the log next to the trace output (in the bake's scratch
        // tempdir) so post-mortem diagnostics survive.
        let log_path = match self.config.working_set_trace_output.as_ref() {
            Some(p) => p
                .parent()
                .map(|d| d.join("uffd-handler.log"))
                .unwrap_or_else(|| jail_dir.join("uffd-handler.log")),
            None => jail_dir.join("uffd-handler.log"),
        };
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
        // ADR 0014 M1.14: bake's profile pass sets this so the
        // image-builder can read the trace file back without going
        // through BlobStorage. Production warm-restore leaves it
        // None — the publish-trace-host path is the runtime channel.
        if let Some(path) = self.config.working_set_trace_output.as_ref() {
            cmd.arg("--trace-output").arg(path);
        }
        if let Some(path) = self.config.uffd_blob_root.as_ref() {
            cmd.arg("--blob-root").arg(path);
        }
        // ADR 0019: hand the handler our current span's W3C traceparent so
        // its process-root span (and the MAP_POPULATE / fault-serving spans
        // under it) stitch onto this restore's trace. Inert when OTLP is off
        // (`current_traceparent` returns `None`).
        if let Some(tp) = engram_telemetry::current_traceparent() {
            cmd.env("TRACEPARENT", tp);
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
    #[tracing::instrument(name = "fc.create_in_jail", skip_all, fields(sandbox_id = %sandbox_id))]
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
    #[tracing::instrument(name = "fc.create_in_jail_after_net", skip_all, fields(sandbox_id = %sandbox_id))]
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
        // Cold create: FC in the host root netns, TAP lives directly
        // on root (per-VM /30 via `net::provision`). ADR 0014 M1.16
        // only puts warm restores in their own netns; cold path
        // stays simpler.
        let (socket, child) = self.spawn_firecracker(jail_dir, None).await?;

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
            cpu_template: self.config.cpu_template.clone(),
        })
        .await?;
        // Append the per-sandbox `ip=...` to the kernel cmdline so
        // CONFIG_IP_PNP brings up eth0 with the guest's static
        // address before init runs. Skipped when networking is
        // disabled — the guest boots without an eth0.
        let mut boot_args = match net_setup {
            Some(setup) => format!(
                "{} {}",
                self.config.default_boot_args.trim_end(),
                setup.vm_cidr.kernel_ip_arg(),
            ),
            None => self.config.default_boot_args.clone(),
        };
        // ADR 0019: propagate this cold-boot's trace context (+ a
        // guest-reachable OTLP collector) into the guest via the kernel
        // cmdline, so in-guest agentd roots its boot spans on this trace.
        // agentd parses these from /proc/cmdline (same channel as
        // `engram_token`). Only meaningful on cold boot — restore reuses the
        // snapshot's embedded cmdline. Inert when OTLP is off / no active span.
        if let Some(tp) = engram_telemetry::current_traceparent() {
            boot_args.push_str(" engram_traceparent=");
            boot_args.push_str(&tp);
        }
        if let Some(ep) = self.config.guest_otel_endpoint.as_deref() {
            boot_args.push_str(" engram_otel=");
            boot_args.push_str(ep);
        }
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
        // drive (`/dev/vdb`). agentd's harness supervisor mounts it
        // at `/run/engram/harnesses` on SpawnHarness so it can exec
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
        // BEFORE InstanceStart guarantees the guest's first dial-out
        // lands on a live listener:
        //
        //   - port 1026: harness adapter dials with HarnessAttach.
        //   - port 1027 (ADR 0015 M1): agentd dials with AgentReady
        //     once its RPC listener (port 1024) is bound. The accept
        //     fills the `agent_ready` watch that `start_agent` blocks
        //     on — replacing the pre-M1 boot-race poll loop.
        self.spawn_harness_listener(sandbox_id, &vsock_uds_path)
            .await?;
        let agent_ready_rx = self
            .spawn_agent_ready_listener(sandbox_id, &vsock_uds_path)
            .await?;

        api.put_action(ActionType::InstanceStart).await?;

        let state = SandboxState {
            spec,
            firecracker_socket: socket,
            rootfs_path: rootfs,
            vsock_cid,
            vsock_uds_path,
            // Cold create: FC opened the root drive at this sandbox's
            // own id-keyed canonical (the `put_drive` above used it as
            // `path_on_host`), so the embedded path == own id.
            rootfs_canonical,
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
                    rootfs_canonical: state.rootfs_canonical.clone(),
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
                // Cold create path: VM is in host root netns.
                netns: None,
                guest_ip: parking_lot::Mutex::new(None),
                agent_ready: agent_ready_rx,
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

    /// ADR 0015 M1: bind a host-side UDS for the in-VM agentd's
    /// one-shot readiness dial. agentd dials AF_VSOCK CID=2 port=
    /// `ENGRAM_AGENTD_READY_PORT` after binding its RPC listener;
    /// FC bridges that to `<vsock_uds_path>_<port>` here. The first
    /// successful accept reads the `AgentReady` frame and flips the
    /// watch sender to `true`; `start_agent` blocks on the matching
    /// receiver and proceeds straight to SpawnHarness with no poll.
    ///
    /// Returns the receiver half; caller stores it in `LiveSandbox`.
    /// The accept task owns the sender and exits after the first
    /// successful dial (subsequent dials are no-ops — agentd only
    /// signals once per process lifetime).
    async fn spawn_agent_ready_listener(
        &self,
        sandbox_id: SandboxId,
        vsock_uds_path: &Path,
    ) -> Result<tokio::sync::watch::Receiver<bool>, SandboxError> {
        let path = per_port_uds(vsock_uds_path, engram_agentd::ENGRAM_AGENTD_READY_PORT);
        let _ = tokio::fs::remove_file(&path).await;
        let listener = tokio::net::UnixListener::bind(&path).map_err(|e| {
            SandboxError::Vm(
                format!(
                    "bind agent-ready UDS {} for sandbox {sandbox_id}: {e}",
                    path.display()
                )
                .into(),
            )
        })?;
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            match listener.accept().await {
                Ok((mut stream, _peer)) => {
                    match engram_agentd::read_msg::<_, engram_agentd::AgentReady>(&mut stream).await
                    {
                        Ok(ready) => {
                            tracing::info!(
                                %sandbox_id,
                                agent_version = %ready.agent_version,
                                "agentd ready",
                            );
                            // Drop in case the sender side is gone —
                            // sandbox already destroyed before agentd
                            // got to dial. Harmless.
                            let _ = tx.send(true);
                        }
                        Err(e) => {
                            tracing::warn!(
                                %sandbox_id,
                                error = %e,
                                "agent-ready frame read failed; start_agent will block until \
                                 deadline. Sandbox is likely broken (agentd dialed but didn't \
                                 write a parseable AgentReady)."
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, %sandbox_id, "agent-ready UDS accept ended");
                }
            }
        });
        Ok(rx)
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
    #[tracing::instrument(name = "fc.restore_in_jail", skip_all, fields(sandbox_id = %sandbox_id))]
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

        // ADR 0014 M1.16: warm-restore networking provisions a
        // per-VM netns BEFORE spawning FC. The netns has the bake's
        // TAP recreated inside it (collision-free vs other warm VMs
        // on the same host) plus SNAT remapping the VM's bake-time
        // source IP to a unique-per-VM pool slot. FC enters the
        // netns via `ip netns exec` so `load_snapshot`'s TAP open
        // resolves inside it.
        //
        // Legacy/test snapshots with `manifest.net = None` keep the
        // historical host-root flow: TAP lives directly on host
        // root, FC stays in host root, no netns at all.
        let netns_setup = match self
            .reserve_restored_netns(sandbox_id, manifest.net.as_ref())
            .await
        {
            Ok(setup) => setup,
            Err(e) => return Err(e),
        };

        let netns_name = netns_setup.as_ref().map(|s| s.netns_name.clone());
        let (socket, child) = match self
            .spawn_firecracker(jail_dir, netns_name.as_deref())
            .await
        {
            Ok(v) => v,
            Err(e) => {
                if let Some(setup) = netns_setup.as_ref() {
                    net::teardown_netns(setup, &self.net_allocator).await;
                }
                return Err(e);
            }
        };

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
        if let Err(e) = restore_canonical_symlinks(
            &self.work_dir,
            sandbox_id,
            manifest,
            self.config.stub_harness_path.as_deref(),
        )
        .await
        {
            drop(child);
            if let Some(setup) = netns_setup.as_ref() {
                net::teardown_netns(setup, &self.net_allocator).await;
            }
            return Err(e);
        }

        // Legacy/test path: no netns means we still might need the
        // old host-root TAP setup (when `manifest.net.is_some` but
        // the host is configured without a netns-style pool — e.g.
        // a fixture that wants a direct TAP). For now we only take
        // the netns path; legacy snapshots without `manifest.net`
        // get `None` here and keep restoring netless.
        let net_setup: Option<net::NetSetup> = None;

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
                        if let Some(setup) = netns_setup.as_ref() {
                            net::teardown_netns(setup, &self.net_allocator).await;
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
                // ADR 0015 M5: bake-time canonical capture was retired,
                // so canonical_ref == session_ref unconditionally. The
                // UFFD resolver returns `Canonical` for every fault and
                // the local memory.bin mmap serves bytes; no chunk-store
                // I/O on the restore-fault path.
                let canonical_ref = session_ref;
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
                if let Some(setup) = netns_setup.as_ref() {
                    net::teardown_netns(setup, &self.net_allocator).await;
                }
                drop(child);
                return Err(e);
            }
        };

        // ADR 0018 §12p: re-anchor the harness drive onto THIS restored
        // sandbox's id. FC's `load_snapshot` just reopened the harness
        // virtio-blk drive at the path `state.bin` embedded — the
        // snapshot's ANCESTOR id, never this fresh restored id, and
        // nothing re-points it. Left alone, the next `snapshot()`
        // recomputes `source_harness_canonical` from the new live id
        // (see the stamp below in `snapshot`) and diverges from the
        // embedded path, so the FOLLOWING restore can't find the harness
        // backing file ("No such file or directory
        // harness/<ancestor>.ext4") — the harness-drive analog of the
        // 12o rootfs/vsock bug, prod-found via session 3e692ab6 driven
        // through two idle-evict→resume hops. Re-pointing here makes the
        // embedded path track the live id (so the recompute stays
        // correct), and re-attaches the harness fresh on every
        // idle→active / evac resume — what we always want across a
        // relocate or a source-side network partition. Gated on
        // `harness_substrate` exactly like `restore_canonical_symlinks`,
        // which installed the `harness/<id>.ext4` symlink this PATCH
        // re-points the drive at. On failure, mirror the load-failure
        // cleanup so we don't leak a half-wired sandbox.
        if manifest.spec.harness_substrate.is_some() {
            if let Err(e) = self.repoint_harness_drive(&api, sandbox_id).await {
                if let Some(setup) = net_setup.as_ref() {
                    net::teardown(setup, &self.net_allocator).await;
                }
                if let Some(setup) = netns_setup.as_ref() {
                    net::teardown_netns(setup, &self.net_allocator).await;
                }
                drop(child);
                return Err(e);
            }
        }

        // Carry the manifest's spec forward so SandboxState reflects
        // what the snapshot was taken from. rootfs_path mirrors what
        // the original VM had attached — Firecracker reopens that
        // path on load, so it must still be valid on disk.
        let rootfs_path = manifest.spec.rootfs_source.clone().unwrap_or_default();
        // FC `load_snapshot` reads vsock config from state.bin and
        // binds the host-side UDS at the bake-side path. FC's vsock
        // state machine refuses any reconfiguration after load (PUT
        // /vsock returns 400 both pre-load — "configuring boot
        // resources before load" — and post-load — "not supported
        // after starting the microVM"), so we MUST dial the bake's
        // path. `source_vsock_canonical` carries that path. Fall back
        // to the host-derived layout for legacy snapshots / same-host
        // idle resume.
        let vsock_uds_path = manifest
            .source_vsock_canonical
            .clone()
            .unwrap_or_else(|| self.work_dir.join(format!("{}.vsock", manifest.sandbox_id)));
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
            // Restore inherits state.bin's embedded root-drive path —
            // `load_snapshot` reopens the drive at whatever the
            // snapshotting VM had, and we never re-point it. That path
            // is `manifest.source_rootfs_canonical` (which
            // `restore_canonical_symlinks` recreated → live backing).
            // Carry it forward so a re-snapshot of THIS sandbox stamps
            // the path FC actually has open, not one recomputed from
            // the fresh live id. Fallback to the own-id canonical for
            // legacy snapshots that predate the field.
            rootfs_canonical: manifest
                .source_rootfs_canonical
                .clone()
                .unwrap_or_else(|| paths::rootfs_canonical(&self.work_dir, sandbox_id)),
        };
        // ADR 0009 §4: supervisor watches restored FC + (optional)
        // UFFD handler. The UFFD handler is critical — if it dies
        // mid-restore the FC process page-faults forever; we want
        // the entry pruned so reconcile transitions the session.
        let fc_pid = child.id();
        let uffd_pid = uffd_handler.as_ref().and_then(|c| c.id());
        // Restore path: agentd was captured already-running in the
        // snapshot. It won't dial the ready port on resume (no
        // startup happens). Pre-set the watch to true so start_agent
        // proceeds immediately to SpawnHarness.
        let (_ready_tx, ready_rx) = tokio::sync::watch::channel(true);
        self.sandboxes.insert(
            sandbox_id,
            LiveSandbox {
                state,
                child: Some(child),
                fc_pid,
                uffd_handler,
                net: net_setup,
                netns: netns_setup,
                guest_ip: parking_lot::Mutex::new(None),
                agent_ready: ready_rx,
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

    /// ADR 0014 M1.16: per-VM netns provisioning at restore time.
    /// Used by the warm-restore path so every restored slot gets its
    /// own netns where the bake's TAP name is reused collision-free.
    /// Replaces the prior `reserve_restored_net` host-root variant —
    /// the host-root approach can't support N>1 warm slots from one
    /// snapshot because every restore would collide on the bake's
    /// `host_dev_name`, and FC v1.10.1's PATCH /network-interfaces
    /// doesn't permit rebinding it post-load.
    ///
    /// Returns `Ok(None)` when the host has networking disabled
    /// (`config.net_pool = None`, tests) or when the manifest carries
    /// no `net` record (legacy / pre-M1.16 bakes). Returns
    /// `Ok(Some(setup))` on success; the caller threads `setup` into
    /// `LiveSandbox.netns` and `destroy()` reverses the provisioning.
    ///
    /// Unlike `reserve_restored_net`, this path doesn't reserve the
    /// snapshot's exact `/30` (the netns hides it) — it allocates a
    /// fresh slot from the host pool for the SNAT'd source IP, so
    /// multiple warm slots from one template never collide on the
    /// host-visible side.
    #[cfg(target_os = "linux")]
    async fn reserve_restored_netns(
        &self,
        sandbox_id: SandboxId,
        snap: Option<&FcNetSnapshot>,
    ) -> Result<Option<net::NetnsSetup>, SandboxError> {
        let Some(snap) = snap else {
            return Ok(None);
        };
        if self.config.net_pool.is_none() {
            return Ok(None);
        }
        let bake_cidr = net::VmCidr::new(snap.cidr_network);
        match net::provision_netns(sandbox_id, bake_cidr, &snap.tap_name, &self.net_allocator).await
        {
            Ok(setup) => Ok(Some(setup)),
            Err(e) => Err(SandboxError::Vm(
                format!(
                    "provision netns for restored sandbox (tap {}): {e}",
                    snap.tap_name
                )
                .into(),
            )),
        }
    }

    #[cfg(not(target_os = "linux"))]
    async fn reserve_restored_netns(
        &self,
        _sandbox_id: SandboxId,
        _snap: Option<&FcNetSnapshot>,
    ) -> Result<Option<net::NetnsSetup>, SandboxError> {
        Ok(None)
    }

    /// Re-point the running VM's harness virtio-blk drive at `id`'s own
    /// canonical symlink (`harness/<id>.ext4`) via pause → PATCH /drives
    /// → resume (~30ms on FC). Shared by `swap_harness_drive` (warm-lease
    /// swap to a freshly-installed session-harness target) and the
    /// restore path (ADR 0018 §12p — re-anchor the harness onto the live
    /// id so the snapshot lineage stays consistent across chained
    /// restores). The caller MUST have installed the `harness/<id>.ext4`
    /// symlink first: `swap_harness_drive` does so explicitly,
    /// `restore_canonical_symlinks` does so on the restore path.
    async fn repoint_harness_drive(
        &self,
        api: &FirecrackerClient,
        id: SandboxId,
    ) -> Result<(), SandboxError> {
        let harness_canonical = paths::harness_canonical(&self.work_dir, id);
        api.patch_vm_state(VmState::Paused).await?;
        // ADR 0016 §A.1.2: same cancellation-safety story as
        // create_snapshot — if our future is dropped between the
        // pause above and the explicit resume below, async Drop
        // can't run the resume. The guard's sync Drop spawns a
        // detached resume task so the VM doesn't stay paused.
        let mut guard = crate::client::ResumeOnDrop::arm(api.clone());
        let patch_result = api.patch_drive("harnesses", &harness_canonical).await;
        // Always try to resume so a partial failure doesn't leave
        // the VM paused. The patch error (if any) wins.
        let resume_result = api.patch_vm_state(VmState::Resumed).await;
        guard.disarm();
        patch_result?;
        resume_result?;
        Ok(())
    }
}

fn vm_err(msg: impl Into<String>) -> SandboxError {
    SandboxError::Vm(msg.into().into())
}

/// ADR 0014: re-install the canonical rootfs + harness symlinks for
/// a restored sandbox. Two symlinks per drive:
///
/// 1. **The new live sandbox's own canonical path**
///    (`<work_dir>/{rootfs,harness}/<new_sandbox_id>.{dev,ext4}`),
///    keyed by `new_sandbox_id`. This is the one a *future*
///    `snapshot(new_sandbox_id)` asserts via
///    `assert_rootfs_canonical` — without it, re-snapshotting (and
///    therefore operator-evac / drain of) a restored sandbox fails
///    with "rootfs canonical symlink missing". (ADR 0018 commit 12n:
///    this was previously keyed off `manifest.sandbox_id`, the
///    *source* id, so the new id's symlink never got created and
///    re-evac of an already-evac'd session broke — caught in prod.)
/// 2. **The source-id-keyed path** that `state.bin` embedded as
///    `path_on_host` at snapshot time (`manifest.source_rootfs_canonical`
///    / `manifest.source_harness_canonical`). FC's `load_snapshot`
///    opens the drive at this exact absolute path, so it must exist
///    on the receiver too.
///
/// Both point at the same `rootfs_target`. Idempotent.
///
/// `stub_harness_override`, when `Some`, replaces `manifest.spec.
/// harness_substrate` as the symlink target — used by warm-pool
/// restore where the bake's stub.ext4 path doesn't exist on the
/// receiver, but a content-identical host-local stub does. M1.12's
/// `swap_harness_drive` re-points the symlink at the session's real
/// harness ext4 at warm-lease time, so the stub only needs to be
/// openable as a block device by `load_snapshot`.
async fn restore_canonical_symlinks(
    work_dir: &Path,
    new_sandbox_id: SandboxId,
    manifest: &FcSnapshotManifest,
    stub_harness_override: Option<&Path>,
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
        // The host's own canonical-path symlink, keyed by the NEW
        // live sandbox_id. This is what `snapshot(new_sandbox_id)`'s
        // `assert_rootfs_canonical` checks, so a restored sandbox can
        // be re-snapshotted (operator-evac / drain) just like a
        // cold-created one.
        let canonical = paths::rootfs_canonical(work_dir, new_sandbox_id);
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
    // FC `load_snapshot` re-creates the vsock UDS at the path embedded
    // in state.bin (the bake's `<work_dir>/<id>.vsock`). The bake's
    // tempdir is long gone on a cross-host restore, so the bind would
    // fail silently — FC reports load success, then our CONNECT
    // returns "early eof" because there's no listener. Pre-create the
    // parent directory so FC can bind.
    if let Some(source_vsock) = manifest.source_vsock_canonical.as_ref() {
        if let Some(parent) = source_vsock.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                vm_err(format!(
                    "create source vsock canonical parent {}: {e}",
                    parent.display()
                ))
            })?;
        }
        // Stale UDS file from a prior restore on this host blocks the
        // bind. Best-effort remove; missing file is fine.
        let _ = tokio::fs::remove_file(source_vsock).await;
    }
    if manifest.spec.harness_substrate.is_some() {
        // Pick the symlink target: a host-local stub when supplied
        // (the cross-host warm-pool case), otherwise fall back to
        // whatever the manifest names (legacy / same-host resume).
        // If neither is available we'd dangle the symlink, so bail
        // explicitly with a diagnostic.
        let manifest_target = manifest.spec.harness_substrate.as_deref();
        let harness_target: &Path = stub_harness_override
            .or(manifest_target)
            .ok_or_else(|| vm_err("no harness target available for symlink"))?;
        // Keyed by the NEW live sandbox_id (see rootfs note above) so
        // a re-snapshot of the restored sandbox finds the harness
        // canonical path too.
        let canonical = paths::harness_canonical(work_dir, new_sandbox_id);
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
        // ADR 0015 M1: no boot-race retry here. Sessions only reach
        // exec_stream after `start_agent` has returned, and
        // start_agent now waits on the agent_ready watch (filled when
        // agentd dials the host's ready port). If we hit `early eof`
        // here, agentd genuinely went away after readiness signal —
        // surface the error rather than masking with retries.
        Self::exec_stream_via_fc_vsock(id, &vsock_uds_path, ENGRAM_AGENTD_PORT, cmd).await
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        // Read sandbox state under the dashmap guard, drop guard before
        // any await so we don't hold the read lock across an HTTP call.
        let (socket, spec, net_snapshot, live_rootfs_canonical, live_vsock_uds) = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            // Cold-path sandboxes carry `net`; M1.16 warm-restored
            // sandboxes carry `netns` with the same TAP name + CIDR
            // recreated inside the netns. Either way, the manifest's
            // `net` field must echo what `state.bin` references — a
            // future restore (warm or cross-host) re-creates a TAP
            // with this exact name inside its own per-VM netns.
            let net_snapshot = live
                .net
                .as_ref()
                .map(|setup| FcNetSnapshot {
                    tap_name: setup.tap_name.clone(),
                    cidr_network: setup.vm_cidr.network(),
                })
                .or_else(|| {
                    live.netns.as_ref().map(|ns| FcNetSnapshot {
                        tap_name: ns.tap_name.clone(),
                        cidr_network: ns.vm_cidr.network(),
                    })
                });
            (
                live.state.firecracker_socket.clone(),
                live.state.spec.clone(),
                net_snapshot,
                live.state.rootfs_canonical.clone(),
                live.state.vsock_uds_path.clone(),
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
        // ADR 0018 commit 12o: stamp the canonical paths from what the
        // LIVE sandbox actually has open (tracked in `SandboxState`),
        // NOT recomputed from `id`. The two diverge after a restore: FC
        // inherits the snapshot's embedded `path_on_host` and we never
        // re-point the root drive, so a restored sandbox runs with its
        // ANCESTOR's id-keyed path while `id` is a fresh routing handle.
        // Recomputing off `id` stamped a path nobody recreates on the
        // next restore → ENOENT (rootfs) / EADDRINUSE (vsock). Anchoring
        // to the embedded path keeps the whole snapshot lineage
        // consistent across arbitrarily many chained restores. Vsock
        // can't be re-pointed (PUT /vsock 400 post-load), so this
        // carry-forward is the ONLY correct option there — rootfs uses
        // the same mechanism for uniformity.
        let source_rootfs_canonical = if spec.rootfs_source.is_some() {
            Some(live_rootfs_canonical)
        } else {
            None
        };
        // The harness drive IS re-pointed onto the live id's canonical
        // at restore — `repoint_harness_drive` (PATCH /drives) runs on
        // BOTH the warm-lease swap and every idle→active / evac resume
        // (ADR 0018 §12p) — so its embedded `path_on_host` tracks the
        // live id and recomputing here is correct. This was the
        // assumption 12o relied on to exempt the harness drive; §12p
        // made it true on the resume paths (it had only ever held on the
        // warm-lease path), after a prod restore (session 3e692ab6)
        // ENOENT'd on the ancestor harness path. Unlike rootfs/vsock,
        // the harness drive CAN be re-pointed post-load, so Option B
        // (re-anchor on restore) is viable here where it wasn't for the
        // immutable vsock device.
        let source_harness_canonical = if spec.harness_substrate.is_some() {
            Some(paths::harness_canonical(&self.work_dir, id))
        } else {
            None
        };
        let source_vsock_canonical = Some(live_vsock_uds);
        // ADR 0014 sec-hardening: `spec.env` carries session secrets
        // (CLAUDE_CODE_OAUTH_TOKEN, ANTHROPIC_API_KEY, etc.) verbatim
        // — keeping them in the on-disk sidecar would leak them to
        // any operator with read access to /var/lib/engram. Restore
        // doesn't replay env (the running VM's process state already
        // baked it in), so we clear values before serialize. Keys
        // stay for diagnostic value (operators can see "this snapshot
        // had ANTHROPIC_API_KEY set" without the secret itself).
        let mut redacted_spec = spec.clone();
        for (_k, v) in redacted_spec.env.iter_mut() {
            *v = "<redacted>".into();
        }
        let manifest = FcSnapshotManifest {
            sandbox_id: id,
            created_at,
            spec: redacted_spec,
            net: net_snapshot,
            format: MANIFEST_FORMAT_FC.into(),
            // PooledBackend::snapshot patches `memory_manifest`
            // in-place after FC returns (the bare backend can't
            // chunk memory.bin without a chunk-store wiring).
            memory_manifest: None,
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
            source_vsock_canonical,
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

    /// ADR 0018 commit 12m: pause the VM via FC's PATCH /vm
    /// {state: Paused}. Idempotent on FC's side — re-pausing a
    /// paused VM is a no-op success. Callers (PooledBackend) use
    /// this to quiesce the guest before flushing the NBD-backed
    /// disk, so memory + disk capture see the same point-in-time.
    async fn pause(&self, id: SandboxId) -> Result<(), SandboxError> {
        let socket = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .state
            .firecracker_socket
            .clone();
        FirecrackerClient::new(&socket).pause().await
    }

    /// ADR 0018 commit 12m: symmetric companion to `pause`. Resume
    /// the VM via PATCH /vm {state: Resumed}. Idempotent on FC's
    /// side. Not currently invoked by PooledBackend (the
    /// `snapshot` flow's internal resume returns the VM to running)
    /// but exposed for orchestration paths that pause without
    /// snapshotting.
    async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
        let socket = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .state
            .firecracker_socket
            .clone();
        FirecrackerClient::new(&socket).resume().await
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

        // UFFD handler (only set on Uffd-mode restore). When FC dies
        // the kernel reaps FC's mm but the userfaultfd this handler
        // holds via SCM_RIGHTS doesn't auto-close — `read_event()`
        // blocks indefinitely. Send SIGTERM so the kernel-default
        // handler kills the process; if that doesn't take effect
        // within a short window, escalate to SIGKILL. ADR 0014 M1.14
        // relies on the handler having already flushed `--trace-output`
        // by the time we get here, so the write must happen earlier
        // (the runtime dumps after the recorder window closes).
        if let Some(mut handler) = live.uffd_handler {
            if let Some(pid) = handler.id() {
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
            }
            match tokio::time::timeout(Duration::from_secs(2), handler.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    let _ = handler.kill().await;
                    let _ = handler.wait().await;
                }
            }
        }

        // Tear down per-VM networking: yank iptables rules tagged
        // with this sandbox's chain comment, delete the TAP, return
        // the /30 to the allocator. Best-effort — each step logs
        // but doesn't stop the rest.
        if let Some(net_setup) = live.net.as_ref() {
            net::teardown(net_setup, &self.net_allocator).await;
        }
        // ADR 0014 M1.16: warm-restored VMs ran in their own netns
        // — delete it (and the TAP + veth-B inside, the SNAT iptables
        // rule, etc.), unlink the host-side veth-A, free the SNAT
        // pool slot. Best-effort like cold teardown.
        if let Some(netns_setup) = live.netns.as_ref() {
            net::teardown_netns(netns_setup, &self.net_allocator).await;
        }

        // ADR 0018 cross-host evac safety: do NOT unlink the base
        // vsock UDS file at `live.state.vsock_uds_path` on destroy.
        // FC's snapshot embeds the source sandbox's UDS path
        // verbatim (`source_vsock_canonical` in the manifest), and
        // `load_snapshot` re-binds that exact path on the receiving
        // host. On shared-filesystem deployments (dev-vm running
        // multiple host-agents, future co-located scheduler
        // experiments) the source's destroy and the target's
        // load-snapshot race on the same path; if destroy wins it
        // unlinks the dentry while the target's FC is still bound,
        // and host-side `connect(path)` then fails ENOENT even
        // though the kernel binding survives via the FD. Leaving
        // the file behind is safe: the per-port UDS files
        // (`<base>_<port>`, host-agent's accept listeners) already
        // orphan on destroy by the same logic, and `create_in_jail`
        // does `remove_file` of any stale UDS before binding, so a
        // future sandbox at the same UUID path picks up clean. In
        // production deployments with separate filesystems per
        // host the orphan is a 0-byte file on the source host
        // only — bounded by sandbox creation rate.
        //
        // The per-sandbox jail dir + canonical symlinks below are
        // still removed; those are deterministic per local sandbox
        // and don't have the cross-host-path-coupling issue.

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
            // ADR 0014 M1.16: warm-restored VMs all share the
            // bake-time eth0 IP `10.200.0.2` inside the guest, but
            // each netns SNATs to a unique-per-VM pool slot on the
            // host-visible side. Egress proxy registry indexes
            // against that SNAT'd IP and the dashboard SHELL tab
            // dials it for ttyd — so this is the value
            // host-side callers want.
            if let Some(ns) = live.netns.as_ref() {
                let ip = ns.host_reachable_ip().to_string();
                *live.guest_ip.lock() = Some(ip.clone());
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

    /// ADR 0014 issue #6: the per-VM netns this sandbox runs inside,
    /// when warm-restored under the M1.16 netns model. Cold sandboxes
    /// run with TAPs on host root (`Some(net)`, `None`-netns); warm
    /// sandboxes run inside `engr-vm-<id>` (`None`-net, `Some(netns)`).
    /// Returned for ProxyShell so the host-agent can dial ttyd from
    /// inside the right namespace.
    async fn netns_name_for(&self, id: SandboxId) -> Option<String> {
        let live = self.sandboxes.get(&id)?;
        live.netns.as_ref().map(|ns| ns.netns_name.clone())
    }

    /// In-VM dial target for the SHELL tab. Returns the IP ttyd is
    /// bound to inside the guest — distinct from `guest_ip` which
    /// returns the SNAT slot for warm sandboxes (used by the egress
    /// proxy registry, NOT for direct ttyd dials).
    ///
    /// - Warm-restored sandboxes: every VM inherits the bake's
    ///   eth0 IP `bake_cidr.guest()` (10.200.0.2 by default). The
    ///   host's proxy_shell flow enters the per-VM netns before
    ///   dialing, so 10.200.0.2 resolves through the TAP to the
    ///   VM.
    /// - Cold-created sandboxes: no netns indirection; the VM's
    ///   eth0 is at `vm_cidr.guest()` and reachable from host root
    ///   via the TAP.
    async fn vm_internal_ip(&self, id: SandboxId) -> Option<String> {
        let live = self.sandboxes.get(&id)?;
        // Warm: TAP is inside the netns, VM eth0 is at the bake
        // CIDR's guest octet. Every warm VM gets the same value
        // because they live in separate netnses.
        if let Some(ns) = live.netns.as_ref() {
            return Some(ns.vm_cidr.guest().to_string());
        }
        // Cold: per-sandbox unique IP from the same pool, in root
        // netns.
        if let Some(net) = live.net.as_ref() {
            return Some(net.vm_cidr.guest().to_string());
        }
        None
    }

    /// Ask agentd to ensure `ttyd` is running and accepting on its
    /// port. Returns the bound port. ADR 0014 follow-up: replaces
    /// the prior assumption that the snapshot's in-VM init script
    /// would have ttyd bound by the time the host dialed — that
    /// raced under warm restore (prod session 73fe33a3 saw
    /// "Connection refused" 48s post-lease even though the VM had
    /// been warm-running for 14 minutes).
    async fn start_shell(&self, id: SandboxId) -> Result<u16, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or_else(|| {
                SandboxError::Vm(format!("start_shell: no live sandbox {id}").into())
            })?;
            live.state.vsock_uds_path.clone()
        };

        // ADR 0015 M1: no boot-race retry. `/shell` only fires after
        // the session is Active, which now requires start_agent's
        // ready-watch handshake. A one-shot CONNECT against agentd-
        // 1024 is sufficient. The outer 15s timeout covers the
        // ttyd spawn (~150ms typical), not the boot race.
        let fut = async {
            let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                .await
                .map_err(|e| SandboxError::Vm(format!("start_shell: vsock connect: {e}").into()))?;
            engram_agentd::write_msg(&mut conn, &WireRequest::StartShell { port: None })
                .await
                .map_err(|e| SandboxError::Vm(format!("start_shell: send: {e}").into()))?;
            let resp: engram_agentd::WireResponse = engram_agentd::read_msg(&mut conn)
                .await
                .map_err(|e| SandboxError::Vm(format!("start_shell: recv: {e}").into()))?;
            match resp {
                engram_agentd::WireResponse::ShellReady { port, spawned } => {
                    tracing::info!(
                        sandbox_id = %id,
                        port,
                        spawned,
                        "agentd reports ttyd ready",
                    );
                    Ok(port)
                }
                engram_agentd::WireResponse::Error { kind, message } => Err(SandboxError::Vm(
                    format!("start_shell: agentd error ({kind}): {message}").into(),
                )),
                other => Err(SandboxError::Vm(
                    format!("start_shell: unexpected response: {other:?}").into(),
                )),
            }
        };
        tokio::time::timeout(Duration::from_secs(15), fut)
            .await
            .map_err(|_| SandboxError::Vm("start_shell: timed out waiting for agentd".into()))?
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
        // the canonical path at it. Canonicalize first — `new_path`
        // is constructed from work_dir which is typically relative
        // (`./var/...`), and the symlink itself lives elsewhere, so
        // a relative target would dangle when FC follows it.
        let abs_new_path = tokio::fs::canonicalize(&new_path).await.map_err(|e| {
            SandboxError::Vm(
                format!(
                    "canonicalize session harness {} for swap: {e}",
                    new_path.display()
                )
                .into(),
            )
        })?;
        let harness_canonical = paths::harness_canonical(&self.work_dir, id);
        if let Err(e) = paths::install_symlink(&harness_canonical, &abs_new_path).await {
            return Err(SandboxError::Vm(
                format!("install harness symlink for swap: {e}").into(),
            ));
        }
        let api = FirecrackerClient::new(&api_sock);
        self.repoint_harness_drive(&api, id).await
    }

    /// ADR 0020 P1: the host-local stub harness ext4 the base-snapshot
    /// capture attaches as `/dev/vdb` so the snapshot carries a harness
    /// drive slot to re-point per session at restore. From
    /// `FirecrackerConfig.stub_harness_path` (`ENGRAM_STUB_HARNESS_PATH`).
    fn stub_harness_path(&self) -> Option<std::path::PathBuf> {
        self.config.stub_harness_path.clone()
    }

    /// ADR 0020 P1: block until agentd dials its ready port. Extracted
    /// from `start_agent`'s step 1 so the base-snapshot capture can
    /// reach a quiescent guest without spawning a session harness.
    #[tracing::instrument(name = "fc.wait_agent_ready", skip_all, fields(sandbox_id = %id))]
    async fn wait_agent_ready(&self, id: SandboxId) -> Result<(), SandboxError> {
        let mut agent_ready = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.agent_ready.clone()
        };
        // Generous deadline (180 s): the dev-vm's fake-gcs path can
        // stretch chunked-NBD page-ins to ~2-3 min on a cold cache;
        // prod is ~15-20 s. Failure means agentd never came up.
        let wait_deadline = Duration::from_secs(180);
        if !*agent_ready.borrow() {
            tracing::Instrument::instrument(
                tokio::time::timeout(wait_deadline, agent_ready.wait_for(|v| *v)),
                tracing::info_span!("fc.await_agent_ready", phase = "agent_handshake"),
            )
            .await
            .map_err(|_| {
                SandboxError::Vm(
                    format!(
                        "agentd did not dial ready port within {}s — guest never bound \
                         ENGRAM_AGENTD_PORT (kernel panic? engram-init hang?)",
                        wait_deadline.as_secs()
                    )
                    .into(),
                )
            })?
            .map_err(|e| {
                SandboxError::Vm(format!("agent_ready watch closed unexpectedly: {e}").into())
            })?;
        }
        Ok(())
    }

    #[tracing::instrument(name = "fc.start_agent", skip_all, fields(sandbox_id = %id))]
    async fn start_agent(
        &self,
        id: SandboxId,
        agent: engram_core::types::sandbox::AgentSpec,
    ) -> Result<(), SandboxError> {
        // ADR 0015 M1: one in-VM service, one wire surface, one
        // event-driven readiness signal. The flow is:
        //
        //   1. Block on the per-sandbox `agent_ready` watch — set to
        //      `true` by the accept task we spawned in create() when
        //      agentd dials in. No poll, no per-call retry budget;
        //      kernel does the blocking, the deadline only catches
        //      truly-dead VMs.
        //   2. Plain one-shot CONNECT to agentd-1024. By contract
        //      agentd's RPC listener is bound *before* it dials the
        //      ready port, so if step 1 returned Ok the CONNECT
        //      cannot race.
        //   3. SpawnHarness.
        //
        // Restored sandboxes (warm-pool refill, host-agent reattach)
        // pre-set the watch to `true` because agentd was already
        // running when the snapshot was captured. Steps 2-3 run
        // immediately on those paths.
        let phase_start = std::time::Instant::now();
        // Step 1: wait for agentd's startup dial (ADR 0019: the dominant
        // cold-boot phase — guest kernel boot + ext4 mount + chunked-NBD
        // page-in + ready-port dial). Shared with the base-snapshot
        // capture via `wait_agent_ready`.
        self.wait_agent_ready(id).await?;
        let (vsock_uds_path, has_harness) = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            (
                live.state.vsock_uds_path.clone(),
                live.state.spec.harness_substrate.is_some(),
            )
        };

        let (harness_dev, harness_mount) = if has_harness {
            (
                Some("/dev/vdb".to_string()),
                Some("/run/engram/harnesses".to_string()),
            )
        } else {
            (None, None)
        };
        let req = engram_agentd::WireRequest::SpawnHarness(engram_agentd::SpawnHarnessRequest {
            argv: agent.argv,
            env: agent.env.into_iter().collect(),
            harness_dev,
            harness_mount,
        });

        // Harness spawn: connect to agentd-1024 and round-trip SpawnHarness.
        // Separate span so the (usually fast) spawn is distinct from the wait.
        let resp: engram_agentd::WireResponse = tracing::Instrument::instrument(
            async {
                let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT).await?;
                engram_agentd::write_msg(&mut conn, &req)
                    .await
                    .map_err(|e| SandboxError::Vm(format!("write SpawnHarness: {e}").into()))?;
                let resp = engram_agentd::read_msg(&mut conn).await.map_err(|e| {
                    SandboxError::Vm(format!("read SpawnHarness response: {e}").into())
                })?;
                // Pin the block's error type to SandboxError. Without this the
                // inference is ambiguous (connect/write yield SandboxError, the
                // raw read_msg yields io::Error) and resolves differently under
                // `-p coord -p host-agent` unification vs `--workspace` — which
                // is why single-crate check + workspace clippy passed but the
                // integration `-p` build failed.
                Ok::<_, SandboxError>(resp)
            },
            tracing::info_span!("fc.spawn_harness"),
        )
        .await?;
        match resp {
            engram_agentd::WireResponse::HarnessSpawned { pid } => {
                let elapsed = phase_start.elapsed().as_secs_f64();
                tracing::info!(
                    sandbox_id = %id,
                    elapsed_ms = (elapsed * 1000.0) as u64,
                    pid = ?pid,
                    "fc agent handshake complete",
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
            guest_otel_endpoint: None,
            firecracker_bin: PathBuf::from("/nonexistent/firecracker"),
            uffd_handler_bin: PathBuf::from("/nonexistent/engram-uffd-handler"),
            restore_mode: RestoreMode::File,
            net_pool: None,
            egress_proxy_port: None,
            egress_dns_port: None,
            host_id: None,
            uffd_cache_root: None,
            stub_harness_path: None,
            working_set_trace_output: None,
            uffd_blob_root: None,
            cpu_template: None,
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
            trace_host_hint: None,
            source_rootfs_canonical: None,
            source_harness_canonical: None,
            source_vsock_canonical: None,
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

    // ---- cpu_template_from_env ------------------------------------------
    //
    // The env var is process-global so these tests have to serialize.
    // A coarse Mutex behind a `OnceLock` is enough — these are unit
    // tests, not perf-critical.

    fn cpu_template_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// Save+restore the env var so tests don't leak into each other.
    /// (cargo test --jobs N runs tests in threads inside a single
    /// process — without restoration, an unset in test A would
    /// confuse test B running concurrently in the same process.)
    struct CpuTemplateEnvGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl CpuTemplateEnvGuard {
        fn new() -> Self {
            let lock = cpu_template_env_lock()
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let prev = std::env::var("ENGRAM_FC_CPU_TEMPLATE").ok();
            // SAFETY: synchronized via `lock`; no other thread in this
            // process should touch the env var.
            unsafe { std::env::remove_var("ENGRAM_FC_CPU_TEMPLATE") };
            Self { prev, _lock: lock }
        }
        fn set(&self, value: &str) {
            unsafe { std::env::set_var("ENGRAM_FC_CPU_TEMPLATE", value) };
        }
    }
    impl Drop for CpuTemplateEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.as_deref() {
                    Some(v) => std::env::set_var("ENGRAM_FC_CPU_TEMPLATE", v),
                    None => std::env::remove_var("ENGRAM_FC_CPU_TEMPLATE"),
                }
            }
        }
    }

    #[test]
    fn cpu_template_from_env_defaults_to_none_when_unset() {
        // Built-in FC templates are vendor-pinned; passthrough is the
        // safe default until a same-vendor bake runner lands. Setting
        // ENGRAM_FC_CPU_TEMPLATE=T2CL on an Intel-only fleet remains
        // an opt-in we'd take once warm-pool is re-enabled.
        let _g = CpuTemplateEnvGuard::new();
        assert_eq!(cpu_template_from_env(), None);
    }

    #[test]
    fn cpu_template_from_env_returns_none_for_empty_string() {
        let g = CpuTemplateEnvGuard::new();
        g.set("");
        // Empty string is the "explicitly opt out" knob — useful in
        // dev-vm shell snippets where you want `unset NAME` semantics
        // without actually unsetting (which would re-enable the
        // T2CL default).
        assert_eq!(cpu_template_from_env(), None);
    }

    #[test]
    fn cpu_template_from_env_returns_none_for_none_case_insensitive() {
        let g = CpuTemplateEnvGuard::new();
        for v in &["none", "None", "NONE", "nOnE"] {
            g.set(v);
            assert_eq!(cpu_template_from_env(), None, "input={v}");
        }
    }

    #[test]
    fn cpu_template_from_env_passes_through_custom_values() {
        let g = CpuTemplateEnvGuard::new();
        for v in &["T2", "T2S", "T2A", "C3", "T2CL"] {
            g.set(v);
            assert_eq!(cpu_template_from_env(), Some((*v).into()), "input={v}");
        }
    }
}
