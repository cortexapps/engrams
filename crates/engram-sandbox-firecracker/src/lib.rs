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
//! a static-musl `engram-agentd` exec'd out of its bundle slot (ADR 0080),
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
use engram_agentd::{
    read_msg, write_msg, WireExecEvent, WireExecRequest, WireRequest, WireResponse,
};
use engram_core::traits::sandbox::{AgentRefresh, HarnessByteStream, SandboxBackend};
use engram_core::types::endpoints::GuestEndpoints;
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::{
    AuxBundleRef, AuxRoDrive, ExecEvent, ExecRequest, ExecStream, SandboxProbe, SandboxSpec,
    WriteFileResult, WriteFileSpec,
};
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

/// Host-side UDS the harness adapter's port-1026 vsock dial lands on.
/// `pub` so the reattach integration test can assert it gets re-bound.
pub fn harness_uds_for(base_vsock_uds: &Path) -> PathBuf {
    per_port_uds(base_vsock_uds, engram_harness_proto::HARNESS_VSOCK_PORT)
}

/// ADR 0023: host-side UDS Firecracker proxies the in-guest forge
/// helper's `FORGE_VSOCK_PORT` dials through.
fn forge_uds_for(base_vsock_uds: &Path) -> PathBuf {
    per_port_uds(base_vsock_uds, engram_harness_proto::FORGE_VSOCK_PORT)
}

/// ADR 0026: host-side UDS Firecracker proxies the in-guest
/// `engram-share` helper's `UPLOAD_VSOCK_PORT` dials through.
fn upload_uds_for(base_vsock_uds: &Path) -> PathBuf {
    per_port_uds(base_vsock_uds, engram_harness_proto::UPLOAD_VSOCK_PORT)
}

/// Lowest CID we'll hand out to a guest. CIDs 0/1/2 are reserved
/// (hypervisor / loopback / host); user-allocatable starts at 3.
const FIRST_GUEST_CID: u32 = 3;

/// vsock CIDs reserved per test slot (see [`test_resource_slot`]). A
/// single test boots a small handful of VMs, so 4096 is generous
/// headroom and keeps each slot's range far from its neighbours'.
const CID_SLOT_STRIDE: u32 = 4096;

/// Per-process isolation slot for HOST-GLOBAL sandbox resources — the
/// vsock CID space and (when networking is on) the /30 IP pool. Both
/// live outside any per-VM netns, so two `FirecrackerBackend`s in
/// different processes that start their allocators at the same base
/// collide on the host.
///
/// Production runs exactly one backend per host and never sets this, so
/// the slot is 0 and every allocation is byte-for-byte unchanged. Under
/// `cargo nextest`, each concurrently-running test gets its own process
/// with a distinct `NEXTEST_TEST_GLOBAL_SLOT` in `[0, test-threads)`;
/// reading it here lets the FC integration tests run in parallel (their
/// jail/work dirs are already per-process temp dirs and their TAP/netns
/// names are UUID-derived, so the CID — and the pool, for any future
/// networked parallel test — are the only shared namespaces left).
fn test_resource_slot() -> u32 {
    std::env::var("NEXTEST_TEST_GLOBAL_SLOT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Shift the /30 pool into a per-slot /16 (`10.200.x` → `10.(200+slot).x`)
/// so parallel test processes hand out disjoint guest IPs. Slot 0
/// (production, and every serial test) is unchanged; the disabled-
/// networking sentinel (`0.0.0.0`) is left alone. Only the second octet
/// moves, and the test runner's thread count bounds `slot` well under
/// the 55 slots available before the octet saturates.
fn pool_for_slot(pool: std::net::Ipv4Addr, slot: u32) -> std::net::Ipv4Addr {
    if slot == 0 || pool.is_unspecified() {
        return pool;
    }
    let o = pool.octets();
    let second = u32::from(o[1]).saturating_add(slot).min(255) as u8;
    std::net::Ipv4Addr::new(o[0], second, 0, 0)
}

/// How long `destroy` waits for the guest to honour SendCtrlAltDel
/// before escalating to SIGKILL. A healthy debian-slim/ubuntu rootfs
/// halts within ~1s; 3s leaves room for the page-cache drain at
/// snapshot time without making a single destroy feel slow.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// ADR 0045 C2 (E2B fold): per-jail working-set trace dump filename —
/// the uffd-handler's fault-order hot set, read best-effort by the
/// migration capture for the `hot_chunks` rider.
pub const WORKING_SET_TRACE_FILE: &str = "working-set-trace.json";

/// ADR 0019 / telemetry restoration (#526): per-jail prefault-
/// effectiveness snapshot filename, written by the uffd-handler as a
/// sibling of [`WORKING_SET_TRACE_FILE`] in the same jail dir. Must
/// stay in sync with `engram_uffd_handler::runtime::PREFAULT_STATS_FILE`
/// (duplicated, not shared, by design — the two binaries don't depend
/// on each other; the filename is the contract).
pub const PREFAULT_STATS_FILE: &str = "prefault-stats.json";

/// ADR 0045 C2: the peer-mode handler's one-way control socket
/// (Sealed/DrainProgress/DrainDone/PeerLost), bound in the jail dir.
pub const UFFD_CONTROL_SOCK_FILE: &str = "uffd-control.sock";

/// ADR 0045 C2: the marker the destination's prestage writes into the
/// snapshot dir to arm a POST-COPY restore (peer-mode handler spawn,
/// state.bin appears late via the fetch poller).
pub const MIGRATION_PEER_FILE: &str = "migration-peer.json";

/// ADR 0045 C2 (E2B fold): the staged source-hot-set trace the spawn
/// writes into the jail for the handler's drain ordering + prefault.
pub const MIGRATION_HOT_TRACE_FILE: &str = "migration-hot-trace.json";

/// ADR 0045 C2: `migration-peer.json` content.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MigrationPeerSpec {
    /// `host:port` of the source's page server (9102).
    pub peer_addr: String,
    pub export_id: String,
    /// Delivered to the handler via ENGRAM_PEER_TOKEN env (argv leaks
    /// on /proc/*/cmdline). The file lives root-owned in the snapshot
    /// staging dir for the restore's lifetime.
    pub peer_token: String,
    /// E2B fold: the SOURCE's resume-time working set (the capture
    /// rider's `hot_chunks`) — the prediction of exactly the pages
    /// the dest guest will fault first. The spawn stages it as a
    /// local trace file so the handler drains hot-first AND the
    /// post-drain prefault walks it. Serde-default: an old spec
    /// simply yields no hot ordering.
    #[serde(default)]
    pub hot_chunks: Vec<[u8; 32]>,
}

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
    /// ADR 0045 unified memory substrate (v2b). When set, Uffd-mode
    /// restores back guest memory `MAP_PRIVATE` on a per-template base
    /// shm file under this directory (`<dir>/<manifest>-v<n>.base`,
    /// created + lazily populated by the handler), registered
    /// `MISSING|MINOR` by the forked FC — base-identical pages are
    /// then shared page-cache across same-template VMs via
    /// `UFFDIO_CONTINUE`. MUST point at a tmpfs/shmem mount (MINOR
    /// faults are shmem-only). `None` ⇒ stock anonymous Uffd restore.
    pub uffd_base_dir: Option<PathBuf>,
    /// How to wire memory on snapshot restore. File mode synchronously
    /// reads memory.bin (slow, simple, no extra processes). Uffd mode
    /// spawns engram-uffd-handler and serves pages on demand (fast,
    /// requires Linux + the handler binary on the host).
    pub restore_mode: RestoreMode,
    /// ADR 0092: fresh-create memory backend override. `None` = derived
    /// (`uffd_base_dir` set ⇒ Uffd, else File). `Some(File)` restores
    /// fresh creates from the per-image memfile even on a substrate host
    /// (reclaimable page-cache residency); resumes are unaffected.
    pub fresh_restore_override: Option<RestoreMode>,
    /// ADR 0028: arm KVM dirty-page tracking on every VM — cold
    /// creates via `MachineConfig.track_dirty_pages`, restores via
    /// `enable_diff_snapshots` at `snapshot/load` — so periodic
    /// checkpoints can use `SnapshotType::Diff` (capture cost
    /// O(dirty set), not O(guest RAM)). Default `false` until the
    /// checkpoint pipeline lands; tracking has a steady-state KVM
    /// write-protect cost that's only worth paying when diffs are
    /// actually taken.
    pub track_dirty_pages: bool,
    /// ADR 0088 addendum: attach a (deflated, `deflate_on_oom`)
    /// virtio-balloon device to every fresh-created VM, pre-boot. The
    /// capture-time seed shrink inflates it before the cold-base dump
    /// so untouched pages elide from the memory manifest; restored VMs
    /// inherit whatever device set rides their snapshot's `state.bin`
    /// (this flag only affects `create`). Kill switch:
    /// `ENGRAM_FC_BALLOON=0`.
    pub balloon: bool,
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
    /// path. In production this is ALWAYS `Some` — the egress proxy
    /// is mandatory (issue #240). `None` is test/dev only and still
    /// gets a FORWARD default-deny (no public-resolver hatch).
    pub egress_proxy_port: Option<u16>,
    /// UDP+TCP port the filtering DNS proxy listens on. Iptables
    /// REDIRECTs guest `{udp,tcp}/53` to this port so the proxy can
    /// enforce `manifest.network.allow_hosts` on resolution. Default
    /// 5353 (avoids systemd-resolved's 127.0.0.53:53 bind on hosts
    /// that run it). Ignored when `egress_proxy_port` is `None`
    /// (test/dev only; that lane now applies a plain FORWARD
    /// default-deny with no public-resolver ACCEPT — issue #240).
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
    /// (`<work_dir>/chunk-cache/`, shared with the host-agent's chunk
    /// cache so residency prefetch warms what the handler reads —
    /// ADR 0021 P2) when the backend is built via `FirecrackerBackend::new`.
    pub uffd_cache_root: Option<PathBuf>,
    /// ADR 0075: the substrate populate socket handed to the handler
    /// as `--substrate-sock` — the single-writer host-agent's UDS.
    /// `None` (tests, the bake/materialize profile pass) leaves the
    /// handler on its direct-blob fallback. Constructors derive
    /// `<work_dir>/substrate.sock` alongside `uffd_cache_root`.
    pub uffd_substrate_sock: Option<PathBuf>,
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
    /// in `engram-coordinator` and `engram-rootfs-materializer` set it.
    pub cpu_template: Option<String>,
    /// ADR 0035: directory where content-addressed bundle generations
    /// (`<drive_id>-<sha256>.squashfs`) and the bake-time
    /// `current.json` stamp live. Production hosts use the fleet
    /// canonical [`AuxRoDrive::SHARED_DIR`]; tests inject a tempdir.
    pub bundle_dir: PathBuf,
    /// ADR 0044 K2: parent cgroup-v2 dir under which to place each VM's
    /// FC + uffd-handler processes (a leaf `<parent>/<sandbox_id>/`),
    /// moving them OUT of the host-agent's own (pod) cgroup. On K8s the
    /// host-agent runs in the pod's container cgroup; tearing the pod
    /// down `cgroup.kill`s that whole cgroup, which would SIGKILL the
    /// microVMs even under `hostPID`. Escaping to a node-level cgroup
    /// lets them survive a pod restart so the successor can reattach.
    /// `None` (dev / tests) keeps FC in the host-agent's cgroup
    /// — correct where there's no pod scope to escape. The chart sets it
    /// via `ENGRAM_FC_VM_CGROUP_PARENT` (e.g. `/sys/fs/cgroup/engram-vms`).
    pub vm_cgroup_parent: Option<PathBuf>,
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

/// ADR 0020 Route B / ADR 0039: pick the *idle-resume* memory backend
/// from `ENGRAM_FC_RESTORE_MODE` (`uffd` | `file`). **Defaults to `uffd`**
/// (ADR 0039 — UFFD lazy-fault is the one true resume path; cross-host
/// idle-resume can't rely on a resident image, so File there would be a
/// synchronous multi-GB reconstruct). `file` is the explicit opt-out
/// (no chunk store / no `/dev/userfaultfd`). Because this is now the
/// default, the Helm chart no longer needs to set the env. `uffd`
/// requires `engram-uffd-handler` on PATH (or `uffd_handler_bin` set)
/// and `/dev/userfaultfd` accessible to the host-agent.
pub fn restore_mode_from_env() -> RestoreMode {
    match std::env::var("ENGRAM_FC_RESTORE_MODE") {
        Ok(s) if s.eq_ignore_ascii_case("file") => RestoreMode::File,
        Ok(s) if !s.is_empty() && !s.eq_ignore_ascii_case("uffd") => {
            tracing::warn!(value = %s, "unrecognised ENGRAM_FC_RESTORE_MODE; defaulting to uffd");
            RestoreMode::Uffd
        }
        _ => RestoreMode::Uffd,
    }
}

/// ADR 0092: override the *fresh-create* memory backend independently of
/// the substrate. Unset/empty ⇒ `None` (the derived default:
/// `effective_restore_mode` picks Uffd when `uffd_base_dir` is set, File
/// otherwise). `file` lets a substrate host — which still needs
/// UFFD+base-shm for resumes — restore fresh creates from the per-image
/// memfile instead, whose residency is reclaimable page cache (the
/// density win; pair with `ENGRAM_FC_BASE_MEMFILE_PIN=0`). `uffd` pins
/// the derived substrate behavior explicitly.
pub fn fresh_restore_mode_from_env() -> Option<RestoreMode> {
    match std::env::var("ENGRAM_FC_FRESH_RESTORE_MODE") {
        Ok(s) if s.eq_ignore_ascii_case("file") => Some(RestoreMode::File),
        Ok(s) if s.eq_ignore_ascii_case("uffd") => Some(RestoreMode::Uffd),
        Ok(s) if !s.trim().is_empty() => {
            tracing::warn!(
                value = %s,
                "unrecognised ENGRAM_FC_FRESH_RESTORE_MODE; using the derived default",
            );
            None
        }
        _ => None,
    }
}

/// ADR 0045 substrate (v2b): per-template base-shm directory for
/// Uffd-mode restores from `ENGRAM_FC_UFFD_BASE_DIR`. Unset/empty ⇒
/// `None` (stock anonymous Uffd restore — the D2 rollout gate; the
/// D3/D4 parity flips make this the one path and retire the knob).
/// The directory must live on tmpfs/shmem (e.g. `/dev/shm/engram` on
/// the dev VM, the node-prep tmpfs in prod) — UFFD minor faults are
/// shmem-only.
/// ADR 0045: the per-image base shm file's path under a substrate base
/// dir — the SINGLE naming authority shared by the handler spawn, the
/// FC load, and the host-agent's image-prefetch pre-warm (so they can't
/// diverge).
pub fn uffd_base_path_in(
    dir: &Path,
    canonical_ref: &engram_core::types::manifest::ManifestRef,
) -> PathBuf {
    dir.join(format!(
        "{}-v{}.base",
        canonical_ref.manifest_id, canonical_ref.version
    ))
}

pub fn uffd_base_dir_from_env() -> Option<PathBuf> {
    match std::env::var("ENGRAM_FC_UFFD_BASE_DIR") {
        Ok(s) if !s.trim().is_empty() => Some(PathBuf::from(s)),
        _ => None,
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
            uffd_base_dir: None,
            restore_mode: RestoreMode::File,
            fresh_restore_override: None,
            track_dirty_pages: false,
            balloon: std::env::var("ENGRAM_FC_BALLOON").map_or(true, |v| v != "0"),
            host_id: None,
            uffd_cache_root: None,
            uffd_substrate_sock: None,
            uffd_blob_root: None,
            // Host-passthrough by default. Prod (`engram-coordinator`)
            // and the bake (`engram-rootfs-materializer`) both opt in to a
            // template via `cpu_template_from_env`. Tests stay opted
            // out so `cargo test` on heterogeneous CI runners doesn't
            // wedge if the runner CPU doesn't satisfy the template.
            cpu_template: None,
            // ADR 0035: fleet-canonical staging dir; tests override
            // with a tempdir.
            bundle_dir: PathBuf::from(engram_core::types::sandbox::AuxRoDrive::SHARED_DIR),
            // ADR 0044 K2: off by default — only the K8s host-fleet sets it.
            vm_cgroup_parent: None,
        }
    }
}

/// Live sandbox handle. Keeps the spawned firecracker `Child` so
/// `destroy` can SIGKILL it. ADR 0044 K2 removed `kill_on_drop` from
/// the FC and uffd-handler `Child`s so a live VM is decoupled from the
/// host-agent's process lifecycle (a host-agent restart *detaches* its
/// VMs and the successor reattaches them) — so dropping a `LiveSandbox`
/// no longer kills anything. The kill backstops are explicit instead:
/// [`SpawnKillGuard`] over the create/restore window, and (issue #196)
/// over the `destroy` kill window, where the post-removal teardown also
/// runs in a detached `tokio::spawn` ([`destroy_teardown`]) so a
/// cancelled `destroy` can't orphan the VM.
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
    /// UFFD handler pid — the pid analogue of `fc_pid`. Populated for
    /// Uffd-mode restores AND for pidfd-reattached Uffd sandboxes (where
    /// `uffd_handler` is `None` because a previous host-agent generation
    /// owned the `Child`). `destroy()` falls back to this to SIGKILL the
    /// handler when we don't own its `Child` (ADR 0044 K2).
    uffd_pid: Option<u32>,
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
    /// Cached guest-network identity, discovered from `net`/`netns`
    /// state or (lazily) by querying agentd on first
    /// `guest_endpoints` call (mirrors VZ's pattern). Populated
    /// lazily in the vsock-fallback case because the agent's eth0
    /// needs IP_PNP DHCP+kernel boot before it can answer.
    guest_endpoints: parking_lot::Mutex<Option<GuestEndpoints>>,
    /// ADR 0080: true iff this sandbox's fresh-create restore patched
    /// the agentd slot (`AuxRoDrive::AGENTD_SLOT_INDEX`) to a DIFFERENT
    /// content generation than the snapshot pinned. `refresh_agent`
    /// consults this to skip the in-guest RefreshAgent RPC when nothing
    /// changed — the steady state — so the swap machinery adds zero
    /// latency to the create path except on the first creates after an
    /// actual agentd roll. Always `false` for cold creates (the boot
    /// copies the attached generation by construction), resumes (never
    /// swapped), and reattached survivors (already running).
    agentd_slot_swapped: bool,
    /// ADR 0015 M1: receiver for the agentd-readiness signal.
    /// Switches to `true` when the in-VM agentd successfully dials
    /// `<vsock_uds>_<ENGRAM_AGENTD_READY_PORT>` and writes its
    /// `AgentReady` frame. `start_agent` awaits this; restored
    /// sandboxes pre-set to `true` because agentd was already
    /// running when the snapshot was captured. Replaces the
    /// pre-M1 boot-race CONNECT-then-retry on port 1024.
    agent_ready: tokio::sync::watch::Receiver<bool>,
    /// RAM ledger (issue #540): true iff this sandbox is RAM-resident
    /// but its session no longer holds a coordinator memory reservation
    /// (epic-parking-ladder rungs 2-3). Always `false` today — no
    /// backend transition sets it yet; this field is the seam the
    /// ladder's park/unpark ops will flip. `guest_memory_stats` buckets
    /// the PSS/RSS sum by this flag so a parked sandbox's memory is
    /// never added back into `allocatable_mib`. Linux-only, like the
    /// `smaps_rollup` read that consumes it (`guest_memory_stats` is
    /// `None` on non-Linux, so the flag has no reader there).
    #[cfg(target_os = "linux")]
    parked: bool,
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
    /// load_snapshot. `restore` requires this to be exactly `"fc"` —
    /// the old empty-format backwards-compat acceptance (for snapshots
    /// written before this field landed) was retired; every manifest
    /// on disk now postdates the field.
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
    /// Tier 2 (resume-prefault fix): a **session-stable** id (the session
    /// id) that keys this session's working-set trace across ALL of its
    /// checkpoints. Unlike `memory_manifest` (a fresh `ManifestRef` minted
    /// every snapshot), this is constant for the session's life, so the
    /// handler's prefault-replay on resume finds the trace the prior life
    /// published (`traces/<trace_lineage_id>/canonical.json`) instead of
    /// looking under a per-checkpoint manifest id that never matches.
    /// Stamped by the host-agent at snapshot-finish. `None` on base
    /// snapshots and pre-Tier-2 checkpoints (the handler then falls back to
    /// the per-host manifest key — today's always-miss-on-resume behavior).
    #[serde(default)]
    trace_lineage_id: Option<engram_core::SessionId>,
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
    /// The exact `vsock.uds_path` the source VM had open at capture,
    /// as embedded in state.bin. LINEAGE METADATA ONLY since the
    /// vsock re-key: restores pass the fork's `vsock_override` keyed
    /// to the new live sandbox id, so FC never binds this path.
    /// (Inheriting it was the pre-re-key behavior — and the root of
    /// the 2026-06-11 same-image UDS collision, since every VM
    /// descended from one base capture shared this one absolute
    /// path.) Still stamped at snapshot for debuggability.
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
    /// ADR 0023: sink for inbound in-guest forge connections (vsock
    /// `FORGE_VSOCK_PORT`). Same lifecycle as `harness_sink`; `None`
    /// until the coord calls `set_forge_sink` (no forge configured →
    /// the per-sandbox forge accept loop closes connections).
    forge_sink: Arc<parking_lot::RwLock<Option<engram_core::traits::ForgeSink>>>,
    /// ADR 0026: sink for inbound in-guest artifact-upload connections
    /// (vsock `UPLOAD_VSOCK_PORT`). Same lifecycle as `forge_sink`;
    /// `None` until the coord/host-agent calls `set_upload_sink` (the
    /// per-sandbox upload accept loop then closes connections).
    upload_sink: Arc<parking_lot::RwLock<Option<engram_core::traits::UploadSink>>>,
    /// ADR 0035: the bake-time bundle stamp (`current.json` under
    /// `config.bundle_dir`), read once and cached — hosts are immutable
    /// (a host-agent pod restart replaces them), so the stamp can't
    /// change under a running host-agent. `drive_id` → sha256.
    bundle_stamp: tokio::sync::OnceCell<std::collections::HashMap<String, String>>,
}

impl FirecrackerBackend {
    pub fn new(work_dir: impl Into<PathBuf>, config: FirecrackerConfig) -> Self {
        // Per-process test-isolation slot (0 in production). Partitions
        // the two host-global namespaces — vsock CID space + the /30 IP
        // pool — so the FC integration suite can run in parallel under
        // nextest without collisions. See `test_resource_slot`.
        let slot = test_resource_slot();
        // Allocator over the configured pool; falls back to a
        // throwaway 0.0.0.0 pool when networking is disabled (the
        // allocator is created but never consulted in that mode).
        let pool = pool_for_slot(
            config
                .net_pool
                .unwrap_or_else(|| "0.0.0.0".parse().unwrap()),
            slot,
        );
        let net_allocator = Arc::new(parking_lot::Mutex::new(net::NetworkAllocator::new(pool)));
        let work_dir: PathBuf = work_dir.into();
        // ADR 0007 Phase 5: default the UFFD handler's chunk cache
        // to a work_dir-local directory unless the caller picked
        // one explicitly. This avoids the handler's compiled-in
        // `/var/cache/engram/chunks` default that requires root on
        // CI runners + unprivileged production hosts.
        //
        // ADR 0021 P2: share the host's `chunk-cache` by default
        // rather than a separate `uffd-chunk-cache`. The residency
        // invariant is *one* shared content-addressed NVMe cache: the
        // host-agent's restore-prefetch and host-boot residency warm
        // `<work_dir>/chunk-cache`, and the UFFD handler must read the
        // *same* dir or it re-fetches the same chunks cold from GCS.
        // Prod already sets this explicitly (host-agent + coord); making
        // it the default closes the footgun for any caller that forgets
        // (tests, standalone, future deploy paths). A caller that truly
        // wants an isolated cache still opts in via `uffd_cache_root`.
        let mut config = config;
        if config.uffd_cache_root.is_none() {
            config.uffd_cache_root = Some(work_dir.join("chunk-cache"));
            // ADR 0075: same-work_dir default as the host-agent's
            // substrate.sock bind.
            config.uffd_substrate_sock = Some(work_dir.join("substrate.sock"));
        }
        Self {
            work_dir,
            config,
            sandboxes: Arc::new(DashMap::new()),
            next_cid: AtomicU32::new(FIRST_GUEST_CID + slot * CID_SLOT_STRIDE),
            net_allocator,
            harness_sink: Arc::new(parking_lot::RwLock::new(None)),
            forge_sink: Arc::new(parking_lot::RwLock::new(None)),
            upload_sink: Arc::new(parking_lot::RwLock::new(None)),
            bundle_stamp: tokio::sync::OnceCell::new(),
        }
    }

    /// ADR 0035: the host's bake-time bundle stamp (`drive_id` → sha256),
    /// read from `<bundle_dir>/current.json` on first use and cached for
    /// the host-agent's lifetime (hosts are immutable; only a host-agent
    /// pod restart changes the stamp, and that replaces the host). Errors if the stamp
    /// is missing or malformed — callers only reach here when a spec
    /// actually requests aux drives, and a bundle-less host can't satisfy
    /// that correctly, so loud is right.
    async fn read_bundle_stamp(
        &self,
    ) -> Result<&std::collections::HashMap<String, String>, SandboxError> {
        let stamp_path = self.config.bundle_dir.join(AuxRoDrive::CURRENT_STAMP);
        self.bundle_stamp
            .get_or_try_init(|| async {
                let bytes = tokio::fs::read(&stamp_path).await.map_err(|e| {
                    SandboxError::InvalidSpec(format!(
                        "read bundle stamp {}: {e} — this host stages no \
                         bundles; it can't attach aux RO drives",
                        stamp_path.display()
                    ))
                })?;
                serde_json::from_slice(&bytes).map_err(|e| {
                    SandboxError::InvalidSpec(format!(
                        "parse bundle stamp {}: {e}",
                        stamp_path.display()
                    ))
                })
            })
            .await
    }

    /// ADR 0035: host path of a *resolved* aux drive's staged generation.
    /// Errors on a symbolic drive — reaching attach/restore with
    /// `sha256 = None` means the resolve step was skipped (or the manifest
    /// predates ADR 0035, which the incident remediation re-captures away).
    fn staged_bundle_path(&self, aux: &AuxRoDrive) -> Result<PathBuf, SandboxError> {
        let sha = aux.sha256.as_deref().ok_or_else(|| {
            SandboxError::InvalidSpec(format!(
                "aux RO drive `{}` is unresolved (no sha256) — pre-ADR-0035 \
                 snapshot or a skipped resolve step; re-capture the image's \
                 base snapshot",
                aux.drive_id
            ))
        })?;
        Ok(self
            .config
            .bundle_dir
            .join(AuxRoDrive::staged_file_name(sha)))
    }

    /// Apply once-per-host networking setup: enable IP forwarding,
    /// install the inter-VM block rule. Idempotent — safe to call
    /// from a coordinator restart. No-op when `config.net_pool` is
    /// None (tests / disabled-networking deployments).
    pub async fn host_startup(&self) -> Result<(), SandboxError> {
        if self.config.net_pool.is_none() {
            return Ok(());
        }
        // ADR 0019 / #526 phase 2: when in-guest OTLP export is
        // configured, pinhole the collector port so the guest's dial
        // to its gateway survives the host-INPUT DROP. A malformed
        // endpoint yields no pinhole (fail closed) — the guest export
        // just stays dark, same as unconfigured.
        let guest_otel_port = self
            .config
            .guest_otel_endpoint
            .as_deref()
            .and_then(net::otel_endpoint_port);
        if self.config.guest_otel_endpoint.is_some() && guest_otel_port.is_none() {
            tracing::warn!(
                endpoint = self.config.guest_otel_endpoint.as_deref(),
                "ENGRAM_GUEST_OTEL_ENDPOINT has an unparseable port; \
                 installing no guest->collector pinhole (guest OTLP export \
                 will fail closed at the host INPUT chain)"
            );
        }
        net::host_startup(
            self.config.egress_proxy_port,
            self.config.egress_dns_port,
            guest_otel_port,
        )
        .await
        .map_err(SandboxError::from)
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Issue #198: the teardown callback handed to every
    /// `spawn_process_supervisor`. When a supervisor wins the prune race
    /// (its watched FC or uffd pid died and it removed the map entry), it
    /// invokes this with the removed `LiveSandbox` so the SAME
    /// kill/free/cleanup sequence `destroy()` runs executes against the
    /// surviving sibling + host state — instead of dropping `LiveSandbox`
    /// silently (which, post-ADR-0044-K2 `kill_on_drop` removal, leaks a
    /// wedged VM, its allocator /30, TAP/netns, and `sandbox.json`).
    ///
    /// Captures owned clones of the allocator + config so the returned
    /// closure is `'static` and can move into the detached supervisor
    /// task. `destroy_teardown` is the shared, double-teardown-tolerant
    /// helper also used by `destroy()`.
    fn supervisor_teardown_fn(
        &self,
    ) -> impl FnOnce(SandboxId, LiveSandbox) -> futures_util::future::BoxFuture<'static, ()>
           + Send
           + 'static {
        let net_allocator = self.net_allocator.clone();
        let vm_cgroup_parent = self.config.vm_cgroup_parent.clone();
        let work_dir = self.work_dir.clone();
        move |id, live| {
            Box::pin(destroy_teardown(
                id,
                live,
                net_allocator,
                vm_cgroup_parent,
                work_dir,
            ))
        }
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
        if manifest.format.as_str() != MANIFEST_FORMAT_FC {
            return Err(SandboxError::Snapshot(format!(
                "manifest format {:?} is not 'fc' — cross-VMM restore not supported",
                manifest.format,
            )));
        }
        let jail_dir = self.work_dir.join(sandbox_id.to_string());
        // Same-id reattach after a graceful host reboot: resume
        // semantics — the session keeps its pinned bundle generations,
        // and the memory backend follows `restore_mode` (resume).
        self.restore_in_jail(
            sandbox_id,
            &jail_dir,
            &src,
            &manifest,
            /*swap_aux_to_current=*/ false,
            /*selected_mounts=*/ Vec::new(),
            self.effective_restore_mode(/*fresh=*/ false),
            // Reattach has no coordinator metadata; canonical falls back
            // to the session ref (unshared but correct — rare path).
            None,
        )
        .await
    }

    /// The effective memory backend for one restore (ADR 0045 D3).
    /// `fresh` (the `swap_aux_to_current` flavor) is a base
    /// `session.create`: with the substrate enabled (`uffd_base_dir`
    /// set) it restores Uffd against the shared base shm — parity-gated
    /// against File mode (density 35% == 35%, median 61 ms vs 56 ms,
    /// dev VM 2026-06-10) — and without it keeps ADR 0022's File-mode
    /// density path. Idle-resume (`fresh == false`) always follows
    /// `restore_mode`. The per-host env is the one switch, so a fleet
    /// roll flips fresh-create and resume together host-by-host with no
    /// coordination (the retired `ENGRAM_FC_BASE_RESTORE_MODE` knob's
    /// job is now derived, not configured).
    fn effective_restore_mode(&self, fresh: bool) -> RestoreMode {
        if fresh {
            // ADR 0092: explicit override first — `file` on a substrate
            // host restores fresh creates from the per-image memfile
            // (reclaimable page-cache residency) while resumes keep Uffd.
            if let Some(m) = self.config.fresh_restore_override {
                return m;
            }
            if self.config.uffd_base_dir.is_some() {
                RestoreMode::Uffd
            } else {
                RestoreMode::File
            }
        } else {
            self.config.restore_mode
        }
    }

    /// Shared body of [`SandboxBackend::restore`] (resume flavor,
    /// `swap_aux_to_current = false`) and
    /// [`SandboxBackend::restore_fresh`] (fresh-create flavor, `true`).
    /// See ADR 0035 §3 for why the flavors attach aux bundles
    /// differently.
    async fn restore_with(
        &self,
        metadata: SnapshotMetadata,
        swap_aux_to_current: bool,
        selected_mounts: Vec<AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        // ADR 0007 Phase 6: backend looks up its own staging dir.
        let src = self.snapshot_dir_for(metadata.id);
        let manifest_bytes = tokio::fs::read(src.join("manifest.json"))
            .await
            .map_err(|e| SandboxError::Snapshot(format!("read manifest: {e}")))?;
        let manifest: FcSnapshotManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| SandboxError::Snapshot(format!("manifest parse: {e}")))?;

        // Reject cross-VMM restores fast: a VZ blob (`format == "vz"`)
        // would otherwise reach load_snapshot and fail with a
        // confusing FC parse error on state.bin.
        if manifest.format.as_str() != MANIFEST_FORMAT_FC {
            return Err(SandboxError::Snapshot(format!(
                "manifest format {:?} is not 'fc' — cross-VMM restore not supported",
                manifest.format,
            )));
        }

        // Always allocate a *fresh* sandbox id — same on-disk state,
        // different lifecycle handle.
        let sandbox_id = SandboxId::new();
        let jail_dir = self.work_dir.join(sandbox_id.to_string());
        // ADR 0022: base-create (swap_aux_to_current) may use File; resume
        // follows restore_mode.
        //
        // ADR 0045 C1: a migration restore is UFFD-shaped by
        // construction — its memory is the staged local session
        // manifest + cache-resident chunks, and there is deliberately
        // NO memory.bin to File-load. Override whatever the host's
        // configured mode says.
        let restore_mode = if src.join("migration-session-manifest.json").exists() {
            RestoreMode::Uffd
        } else {
            self.effective_restore_mode(swap_aux_to_current)
        };

        match self
            .restore_in_jail(
                sandbox_id,
                &jail_dir,
                &src,
                &manifest,
                swap_aux_to_current,
                selected_mounts,
                restore_mode,
                metadata.base_memory_manifest,
            )
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
        //
        // Host-root path (cold-created VMs): re-reserve the per-VM /30.
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

        // ADR 0044 K2 — per-VM netns path (warm-restored VMs): the netns,
        // veth pair, TAP, and SNAT rule all survive in the kernel across a
        // host-agent restart. Verify the netns survived, re-reserve the
        // SNAT `/30` from the host pool (existence-check FIRST so a missing
        // netns doesn't leak a reservation), and rebuild the in-memory
        // `NetnsSetup` so `destroy()` can later tear it down + `guest_endpoints()`
        // resolves `egress_identity` to the SNAT IP. Mutually exclusive with `net_setup`.
        let netns_setup = if let Some(ns_rec) = manifest.netns.as_ref() {
            let netns_path =
                std::path::PathBuf::from(format!("/var/run/netns/{}", ns_rec.netns_name));
            if !netns_path.exists() {
                // ADR 0044 K2: the named bind-mount lived in the predecessor
                // pod's mount namespace and is gone — but the netns *kernel
                // object* survives because the FC process still holds it open
                // (it's that process's /proc/<pid>/ns/net). Re-bind it under
                // the name so reattach can resolve it again. Under hostNetwork
                // the veth host-side + SNAT iptables are in the node root netns
                // and already survived the pod delete, so re-naming the netns
                // is the only missing piece. The network analog of the cgroup
                // escape (the FC process itself survived the same way).
                net::reattach_netns_name(&ns_rec.netns_name, fc.process.pid)
                    .await
                    .map_err(|e| {
                        ReattachError::NetReserveFailed(format!(
                            "netns {} name gone and re-bind from fc pid {} failed: {e}",
                            ns_rec.netns_name, fc.process.pid
                        ))
                    })?;
                tracing::info!(
                    netns = %ns_rec.netns_name,
                    fc_pid = fc.process.pid,
                    "ADR 0044 K2: re-bound surviving VM netns name after pod restart",
                );
            }
            let snat_cidr = net::VmCidr::new(ns_rec.snat_cidr_network);
            self.net_allocator
                .lock()
                .reserve(snat_cidr)
                .map_err(|e| ReattachError::NetReserveFailed(format!("{e:?}")))?;
            Some(net::NetnsSetup {
                netns_name: ns_rec.netns_name.clone(),
                veth_host: ns_rec.veth_host.clone(),
                veth_ns: ns_rec.veth_ns.clone(),
                tap_name: ns_rec.tap_name.clone(),
                vm_cidr: net::VmCidr::new(ns_rec.vm_cidr_network),
                snat_cidr,
            })
        } else {
            None
        };

        // Re-verify the uffd handler (Uffd-mode restores) BEFORE adopting
        // the sandbox, so we only record a still-live handler. A
        // stale/recycled pid is dropped to `None` — otherwise `destroy()`
        // could later SIGTERM an unrelated process that recycled the pid.
        let uffd_pid = manifest.uffd_handler.as_ref().and_then(|uffd| {
            let live_start = sandbox_manifest::read_proc_start_time_jiffies(uffd.pid);
            let live_comm = sandbox_manifest::read_proc_comm(uffd.pid);
            if live_start == Some(uffd.start_time_jiffies)
                && live_comm.as_deref() == Some(uffd.comm.as_str())
            {
                Some(uffd.pid)
            } else {
                tracing::warn!(
                    %id,
                    uffd_pid = uffd.pid,
                    "UFFD handler gone or recycled during reattach; FC may page-fault forever"
                );
                None
            }
        });

        // pidfd_open confirms the kernel still has this exact process
        // (race-free vs the pid-recycling the three-axis check guards
        // against). On non-Linux this returns Unsupported; we treat it as
        // "no pidfd but the poll-based supervisor still works."
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
            // Both cold-created and warm-restored sandboxes persist a
            // manifest (ADR 0044 K2), so the recorded `rootfs_canonical`
            // is whatever FC has open — `rootfs/<id>.dev` for cold, the
            // ancestor's path for a restore. Carry it forward so a
            // re-snapshot after restart still stamps the embedded path.
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
                // No owned `Child` for a reattached handler (a previous
                // generation spawned it); destroy() kills it via `uffd_pid`.
                uffd_handler: None,
                uffd_pid,
                net: net_setup,
                // ADR 0044 K2: warm-restored VMs reattach with their per-VM
                // netns rehydrated above; cold/host-root VMs leave this None.
                netns: netns_setup,
                guest_endpoints: parking_lot::Mutex::new(None),
                #[cfg(target_os = "linux")]
                parked: false,
                agentd_slot_swapped: false,
                agent_ready: ready_rx,
            },
        );

        // §4 supervisor on the reattached FC pid, and the uffd handler pid
        // when one was verified still-live above.
        spawn_process_supervisor(
            self.sandboxes.clone(),
            id,
            fc.process.pid,
            "firecracker",
            self.supervisor_teardown_fn(),
        );
        if let Some(pid) = uffd_pid {
            spawn_process_supervisor(
                self.sandboxes.clone(),
                id,
                pid,
                "uffd-handler",
                self.supervisor_teardown_fn(),
            );
        }

        // ADR 0044 K2: re-bind the host-side vsock listeners. The in-guest
        // harness/forge/upload adapters re-dial when their connection to the
        // dead host-agent drops; the run is decoupled from the host link and
        // resumes losslessly on reconnect (engram-harness-claude backpressures
        // rather than dropping events) — but ONLY if a listener is here to
        // reconnect to. Without this the reattached session wedges: the run
        // pauses forever and the transcript stops flowing. create() + restore()
        // bind these too. Best-effort + loud: a bind failure leaves the FC
        // tracked (destroy can still reap it) rather than orphan-leaked.
        for (label, res) in [
            (
                "harness",
                self.spawn_harness_listener(id, &fc.vsock_uds_base).await,
            ),
            (
                "forge",
                self.spawn_forge_listener(id, &fc.vsock_uds_base).await,
            ),
            (
                "upload",
                self.spawn_upload_listener(id, &fc.vsock_uds_base).await,
            ),
        ] {
            if let Err(e) = res {
                tracing::warn!(
                    %id,
                    listener = label,
                    error = %e,
                    "reattach: failed to re-bind vsock listener; the guest adapter cannot \
                     reconnect and the session will wedge until destroyed"
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

    /// Drive agentd's existing `Upload` verb over a direct UDS. Public for
    /// the protocol integration test; production uses the FC-vsock variant.
    pub async fn write_files_via_agent_socket(
        sandbox_id: SandboxId,
        agent_socket: &Path,
        files: Vec<WriteFileSpec>,
    ) -> Result<Vec<WriteFileResult>, SandboxError> {
        let mut results = Vec::with_capacity(files.len());
        for file in files {
            let result = match UnixStream::connect(agent_socket).await {
                Ok(conn) => upload_file_over_stream(conn, file).await,
                Err(error) => write_file_failure(
                    file.path,
                    format!(
                        "sandbox {sandbox_id}: connect to agent at {}: {error}",
                        agent_socket.display()
                    ),
                ),
            };
            results.push(result);
        }
        Ok(results)
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

    async fn write_files_via_fc_vsock(
        sandbox_id: SandboxId,
        vsock_uds_path: &Path,
        port: u32,
        files: Vec<WriteFileSpec>,
    ) -> Result<Vec<WriteFileResult>, SandboxError> {
        let mut results = Vec::with_capacity(files.len());
        for file in files {
            let result = match Self::connect_fc_vsock(vsock_uds_path, port).await {
                Ok(conn) => upload_file_over_stream(conn, file).await,
                Err(error) => write_file_failure(
                    file.path,
                    format!("sandbox {sandbox_id}: connect to agentd vsock: {error}"),
                ),
            };
            results.push(result);
        }
        Ok(results)
    }

    /// Open the host UDS at `vsock_uds_path`, write `CONNECT <port>\n`,
    /// read back `OK <peer>\n`, and return the resulting stream now
    /// directly connected to the guest's listener on `port`. One-shot:
    /// callers that need to race the boot window are wrong — by ADR
    /// 0015 M1, the boot window is closed before any host-side
    /// dial-out by waiting on the per-sandbox `agent_ready` watch
    /// (filled when agentd dials the host's ready port). The warm-
    /// restore path's invariant is symmetric: the snapshot must have
    /// captured agentd already-bound on vsock 1024 — the upstream
    /// callers (e.g. coord's `start_agent` after `finish_resume`,
    /// `bake-then-snapshot` flows) settle agentd before snapshotting
    /// so the resumed VM is dial-ready by the time `restore` returns.
    /// A pre-bind snapshot resumes into a half-initialised kernel
    /// vsock driver and FC re-exits ~1 s later via `panic=1`-driven
    /// `KVM_EXIT_SHUTDOWN`; no host-side timeout fixes that.
    async fn connect_fc_vsock(
        vsock_uds_path: &Path,
        port: u32,
    ) -> Result<UnixStream, SandboxError> {
        // Post-`load_snapshot`, FC's vsock muxer has a brief window where it
        // accepts a host CONNECT and returns OK, then closes the connection
        // before the guest's accept-loop wakes — surfacing as `early eof` on
        // the CONNECT-response read 3-5 ms in (the same window `start_agent`'s
        // `SpawnHarness` first-contact retry documents, one layer up). It
        // widens with the device count a restore has to kick (ADR 0027 added
        // the RO bundle drive), which tipped the previously-lucky post-resume
        // `/exec` dial into it. The CONNECT handshake is PRE-APPLICATION — no
        // bytes have reached the guest service yet — so re-dialing is safe
        // for every caller (exec, forge, upload, shell tunnel, harness spawn
        // + CA). Retry EOF/RST-shaped handshake failures with a short
        // backoff; a genuinely-dead agentd EOFs every attempt and the final
        // error propagates.
        // The muxer-settle window is usually a few ms, but on a cold boot,
        // under CI load, or with more restore-time devices (ADR 0027 added a
        // RO bundle drive) it can stretch well past the old flat 5×50 ms
        // (~250 ms) budget — observed as a CI `early eof` flake when
        // `start_shell` dials agentd on a freshly cold-booted VM. Use more
        // attempts with an escalating-but-capped backoff so the total budget
        // (~1.75 s) covers the long tail, while staying cheap on the common
        // (few-ms) case and well under every caller's outer timeout (e.g.
        // start_shell's 15 s). Re-dialing is always safe here — the CONNECT
        // handshake is pre-application, no bytes have reached the guest.
        const MAX_ATTEMPTS: u32 = 10;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            match Self::connect_fc_vsock_once(vsock_uds_path, port).await {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    let msg = format!("{e}");
                    let retryable = msg.contains("early eof")
                        || msg.contains("unexpected end of file")
                        || msg.contains("connection reset")
                        || msg.contains("broken pipe");
                    if !retryable || attempt >= MAX_ATTEMPTS {
                        return Err(e);
                    }
                    let backoff_ms = (attempt as u64 * 50).min(250);
                    tracing::debug!(
                        attempt,
                        port,
                        backoff_ms,
                        error = %e,
                        "FC vsock CONNECT transient (muxer settle); retrying",
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }

    /// One CONNECT-handshake attempt. See [`Self::connect_fc_vsock`] for the
    /// retry wrapper and why a fresh dial is always safe.
    async fn connect_fc_vsock_once(
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
            conn.read_exact(&mut byte).await.map_err(|e| {
                vm_err(format!(
                    "read FC vsock CONNECT response: {}",
                    describe_relay_connect_failure(port, &e)
                ))
            })?;
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
    /// `netns_name`: when `Some`, FC is exec'd directly and `setns`'d
    /// into that netns from a `pre_exec` closure (no `ip netns exec`
    /// wrapper fork) so the process (and any TAP it opens via
    /// `host_dev_name`) lives inside it. ADR 0014 M1.16 warm-restore
    /// path passes the per-VM netns here; cold path passes `None`
    /// and FC stays in host root, as today.
    async fn spawn_firecracker(
        &self,
        jail_dir: &Path,
        netns_name: Option<&str>,
    ) -> Result<(PathBuf, Child, SpawnKillGuard), SandboxError> {
        // jail_dir is created by the caller (create_in_jail / restore_in_jail)
        // before we're invoked — single owner for dir creation (ADR 0020 P3),
        // so the UFFD-handler leg can't race a create_dir_all buried here.
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

        let socket_arg = socket.to_string_lossy().into_owned();
        let mut cmd = Command::new(&self.config.firecracker_bin);
        cmd.args(["--api-sock", &socket_arg]);

        // `_ns_file` must stay alive (open, in this parent process)
        // across the `cmd.spawn()` call below — `spawn()` forks
        // synchronously, and the child's duplicated fd table is only
        // valid for fds still open in the parent at that instant. It
        // can be (and is, implicitly) dropped once `spawn()` returns.
        #[cfg(target_os = "linux")]
        let _ns_file = match netns_name {
            Some(ns) => {
                let ns_path = format!("/var/run/netns/{ns}");
                let ns_file = std::fs::File::open(&ns_path)
                    .map_err(|e| vm_err(format!("open netns {ns} ({ns_path}): {e}")))?;
                use std::os::unix::io::AsRawFd;
                let ns_fd = ns_file.as_raw_fd();
                // SAFETY: this closure runs in the forked child between
                // `fork()` and `exec()`, when the child is guaranteed
                // single-threaded — the constraint `pre_exec` documents.
                // `setns(2)` only touches this process's own namespace
                // membership and, like `close(2)`, is async-signal-safe,
                // so calling it here (instead of e.g. allocating) is
                // sound. `ns_fd` is valid for the duration of the fork
                // because `ns_file` is held alive in the caller's scope
                // (`_ns_file`, bound below) past the `spawn()` call.
                unsafe {
                    cmd.pre_exec(move || {
                        if libc::setns(ns_fd, libc::CLONE_NEWNET) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
                Some(ns_file)
            }
            None => None,
        };
        #[cfg(not(target_os = "linux"))]
        if netns_name.is_some() {
            return Err(vm_err(
                "netns-scoped Firecracker spawn requires Linux".to_string(),
            ));
        }

        // ADR 0044 K2: NO `kill_on_drop` — a live FC must survive the
        // host-agent process exiting (detach + reattach). The
        // create-window backstop is the explicit `SpawnKillGuard` armed
        // BELOW the instant `.spawn()` succeeds; steady-state teardown is
        // `destroy()` via pid.
        let mut child = cmd
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_clone))
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| {
                vm_err(format!(
                    "spawn {} (netns={:?}): {e}",
                    self.config.firecracker_bin.display(),
                    netns_name,
                ))
            })?;

        // Issue #197: arm the SIGKILL backstop HERE, before `wait_for_socket`,
        // not in the caller after a multi-second await. Without kill_on_drop a
        // restore future cancelled inside that wait (e.g. a post-copy
        // `restore_task.abort()` while leg 2 parks on the peer sock) would
        // otherwise drop this live Child unguarded and orphan FC. The caller
        // holds the returned guard through the rest of setup and `disarm()`s it
        // at the commit point.
        let guard = child
            .id()
            .map(SpawnKillGuard::new)
            .ok_or_else(|| vm_err("firecracker child has no pid right after spawn"))?;

        // Race the socket appearing against the process exiting. If
        // firecracker dies during startup, surface that with the log.
        if let Err(e) = wait_for_socket(&socket, Duration::from_secs(5), &mut child).await {
            let _ = child.kill().await;
            let log_tail = read_tail(&log_path, 4096).await.unwrap_or_default();
            return Err(vm_err(format!(
                "firecracker did not open API socket: {e}\n--- firecracker log ---\n{log_tail}"
            )));
        }

        Ok((socket, child, guard))
    }

    /// Spawn `engram-uffd-handler` in ADR 0020 chunk-native mode and
    /// wait until it's listening on `uffd_uds`. `canonical_ref` +
    /// `session_ref` identify the manifests it reads from the chunk
    /// store; it serves every fault from chunks (no `memory.bin`).
    /// `prefault_trace_host` optionally points at a host's prior
    /// working-set recording for REAP-style replay; `publish_trace_host`
    /// names the host the recorder publishes the new trace under on
    /// clean shutdown.
    ///
    /// Stdout/stderr go into the jail dir's `uffd-handler.log` so a
    /// snapshot-restore failure has a recoverable diagnostic.
    /// Returns the live `Child` so the caller can hold it for the
    /// VM's lifetime.
    #[allow(clippy::too_many_arguments)]
    /// ADR 0045 substrate (v2b): the per-template base shm path for a
    /// canonical manifest, or `None` when the substrate is off. Both the
    /// handler spawn (creates + populates it) and the FC load (maps it
    /// `MAP_PRIVATE`) derive the path through here so they can't diverge.
    fn uffd_base_path(
        &self,
        canonical_ref: &engram_core::types::manifest::ManifestRef,
    ) -> Option<PathBuf> {
        self.config
            .uffd_base_dir
            .as_ref()
            .map(|dir| uffd_base_path_in(dir, canonical_ref))
    }

    #[tracing::instrument(name = "fc.spawn_uffd_handler", skip_all)]
    // 8 args: the migration manifest file is a restore-time input
    // orthogonal to the trace/publish hosts; a params struct is the
    // cleanup when the next arg arrives.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_uffd_handler(
        &self,
        uffd_uds: &Path,
        canonical_ref: engram_core::types::manifest::ManifestRef,
        session_ref: engram_core::types::manifest::ManifestRef,
        // ADR 0045 C1: when a migration pre-staged the (not-yet-durable)
        // session manifest as a local JSON file, the handler reads it
        // from disk instead of the blob store.
        session_manifest_json: Option<&Path>,
        prefault_trace_host: Option<uuid::Uuid>,
        publish_trace_host: Option<uuid::Uuid>,
        // Tier 2 (resume-prefault fix): the session-stable trace key (the
        // session id). When set, the handler keys prefault-replay + publish
        // by this id under the canonical variant, so a resumed session finds
        // the trace its own prior life recorded.
        trace_key: Option<uuid::Uuid>,
        jail_dir: &Path,
        // ADR 0045 C2: post-copy peer mode. The handler dials the
        // source's page server and PARKS until the capture's SEAL; it
        // binds the control sock immediately but the FC-facing UDS only
        // post-seal — so this spawn waits on the CONTROL sock, and the
        // load gate (not this fn) waits on the UDS.
        peer: Option<&MigrationPeerSpec>,
    ) -> Result<(Child, SpawnKillGuard), SandboxError> {
        let log_path = jail_dir.join("uffd-handler.log");
        let log = std::fs::File::create(&log_path)
            .map_err(|e| vm_err(format!("open uffd handler log {}: {e}", log_path.display())))?;
        let log_clone = log
            .try_clone()
            .map_err(|e| vm_err(format!("dup uffd-handler log fd: {e}")))?;

        let mut cmd = Command::new(&self.config.uffd_handler_bin);
        cmd.arg("--listen")
            .arg(uffd_uds)
            .arg("--canonical-manifest")
            .arg(canonical_ref.to_string())
            .arg("--session-manifest")
            .arg(session_ref.to_string());
        if let Some(path) = session_manifest_json {
            cmd.arg("--session-manifest-json").arg(path);
        }
        // ADR 0007 Phase 5: hand the handler a work_dir-local
        // chunk cache root. `FirecrackerBackend::new` populates a
        // default; callers using `FirecrackerConfig` directly can
        // override or leave `None` (the handler's compiled-in
        // default is `/var/cache/engram/chunks`, root-only).
        if let Some(cache_root) = self.config.uffd_cache_root.as_ref() {
            cmd.arg("--cache-root").arg(cache_root);
        }
        // ADR 0075: point the handler at the single writer's populate
        // socket. Misses populate through the host-agent's cache (one
        // singleflight / pin set / budget per host); the handler keeps
        // a direct-blob fallback for writer-unreachable windows.
        if let Some(sock) = self.config.uffd_substrate_sock.as_ref() {
            cmd.arg("--substrate-sock").arg(sock);
        }
        // ADR 0045 substrate (v2b): the handler creates + sizes the base
        // shm file (it knows total_bytes from the canonical manifest)
        // BEFORE binding the UDS, and `wait_for_socket` below orders the
        // FC load after that — so FC's O_RDONLY open of the same path
        // always sees a fully-sized file.
        if let Some(base) = self.uffd_base_path(&canonical_ref) {
            if let Some(dir) = base.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .map_err(|e| vm_err(format!("create uffd base dir {}: {e}", dir.display())))?;
            }
            cmd.arg("--base-shm").arg(&base);
        }
        // Post-copy: the source's hot set beats any host-local trace —
        // it is the working set of THIS guest measured minutes ago,
        // not a same-template cousin's. Staged as a local file; the
        // handler both orders its drain by it and prefaults from it.
        let migration_hot_trace = match peer {
            Some(p) if !p.hot_chunks.is_empty() => {
                let trace = engram_chunk_store::working_set::WorkingSetTrace {
                    schema_version: 1,
                    captured_at: chrono::Utc::now(),
                    vcpu_count: 0, // unknown here; replay ignores it
                    capture_window_ms: 0,
                    chunks: p
                        .hot_chunks
                        .iter()
                        .map(|h| engram_chunk_store::manifest::ChunkHash::from_bytes(*h))
                        .collect(),
                };
                let path = jail_dir.join(MIGRATION_HOT_TRACE_FILE);
                let json = serde_json::to_vec(&trace)
                    .map_err(|e| vm_err(format!("serialize migration hot trace: {e}")))?;
                tokio::fs::write(&path, json)
                    .await
                    .map_err(|e| vm_err(format!("write migration hot trace: {e}")))?;
                Some(path)
            }
            _ => None,
        };
        if let Some(path) = migration_hot_trace.as_ref() {
            cmd.arg("--prefault-trace")
                .arg(format!("file:{}", path.display()));
        } else if let Some(host) = prefault_trace_host {
            cmd.arg("--prefault-trace").arg(host.to_string());
        }
        if let Some(host) = publish_trace_host {
            cmd.arg("--publish-trace-host").arg(host.to_string());
        }
        // Tier 2 (resume-prefault fix): the per-session stable trace key.
        // Keyed under the canonical variant (`traces/<key>/canonical.json`)
        // for both replay and publish, so life N's recorded working-set
        // trace lands exactly where life N+1's resume looks — unlike the
        // per-checkpoint `session_manifest.manifest_id`, which is minted
        // fresh each snapshot and so always missed on resume.
        if let Some(key) = trace_key {
            cmd.arg("--trace-key").arg(key.to_string());
        }
        // ADR 0045 C2 (E2B fold): production spawns default to a
        // PER-JAIL trace file — the migration capture reads it to ship
        // the `hot_chunks` rider (the publish-trace-host channel only
        // lands at handler EXIT, which is too late for a live move).
        cmd.arg("--trace-output")
            .arg(jail_dir.join(WORKING_SET_TRACE_FILE));
        if let Some(path) = self.config.uffd_blob_root.as_ref() {
            cmd.arg("--blob-root").arg(path);
        }
        if let Some(peer) = peer {
            cmd.arg("--peer-addr").arg(&peer.peer_addr);
            cmd.arg("--peer-export-id").arg(&peer.export_id);
            cmd.arg("--control-sock")
                .arg(jail_dir.join(UFFD_CONTROL_SOCK_FILE));
            // Token via env: argv leaks on /proc/*/cmdline.
            cmd.env("ENGRAM_PEER_TOKEN", &peer.peer_token);
        }
        // ADR 0019: hand the handler our current span's W3C traceparent so
        // its process-root span (and the fault-serving spans
        // under it) stitch onto this restore's trace. Inert when OTLP is off
        // (`current_traceparent` returns `None`).
        if let Some(tp) = engram_telemetry::current_traceparent() {
            cmd.env("TRACEPARENT", tp);
        }

        // ADR 0044 K2: no `kill_on_drop` (see spawn_firecracker) — the
        // uffd handler must also survive a host-agent restart so the
        // successor can reattach it. Create-window cleanup is the
        // `SpawnKillGuard` armed BELOW; teardown is `destroy()` via pid.
        let mut child = cmd
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_clone))
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| {
                vm_err(format!(
                    "spawn {}: {e}",
                    self.config.uffd_handler_bin.display()
                ))
            })?;

        // Issue #197: arm the SIGKILL backstop HERE, right after spawn —
        // a peer-mode handler parks in the `wait_for_socket` below until the
        // SOURCE seals, which is exactly when a post-copy abort drops this
        // future. Caller holds the guard through setup and `disarm()`s at commit.
        let guard = child
            .id()
            .map(SpawnKillGuard::new)
            .ok_or_else(|| vm_err("uffd handler child has no pid right after spawn"))?;

        // Peer mode binds the FC-facing UDS only after the source's
        // SEAL arrives (which is what makes \"FC can load\" imply
        // \"seal held\"); its immediately-bound control sock is the
        // spawn-liveness signal instead. The load gate waits on the
        // UDS with the long budget.
        let spawn_gate = match peer {
            Some(_) => jail_dir.join(UFFD_CONTROL_SOCK_FILE),
            None => uffd_uds.to_path_buf(),
        };
        if let Err(e) = wait_for_socket(&spawn_gate, Duration::from_secs(5), &mut child).await {
            let _ = child.kill().await;
            let log_tail = read_tail(&log_path, 4096).await.unwrap_or_default();
            return Err(vm_err(format!(
                "uffd handler did not open {}: {e}\n--- handler log ---\n{log_tail}",
                spawn_gate.display()
            )));
        }
        Ok((child, guard))
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
        // ext4 first by the enable-time materializer
        // (engram-rootfs-materializer).
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

        // jail_dir (FC's per-sandbox chroot + sockets) is the caller's to
        // create — symmetric with restore_in_jail — so spawn_firecracker can
        // assume it exists (ADR 0020 P3: single owner for dir creation).
        tokio::fs::create_dir_all(jail_dir)
            .await
            .map_err(|e| vm_err(format!("create jail dir {}: {e}", jail_dir.display())))?;

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
        mut spec: SandboxSpec,
        net_setup: Option<&net::NetSetup>,
    ) -> Result<(), SandboxError> {
        // ADR 0035: resolve symbolic aux drives ("attach whatever generation
        // this host currently stages") against the bake stamp BEFORE anything
        // embeds the spec — the manifest written below and FC's state.bin
        // must both carry the resolved, content-addressed form.
        // ADR 0055: at capture every aux drive is a reserved slot carrying the
        // sentinel, so resolve all symbolic drives to the host's staged sentinel
        // generation (`current.json`'s `sentinel` key). Per-session skill
        // selection happens later, at restore, by `patch_drive`-ing real skill
        // shas into slots — never here.
        // ADR 0080: the ONE exception is the agentd slot, which resolves to
        // the host's staged `agentd` bundle — the cold-booting guest's
        // stage-1 init copies agentd out of that mount and execs it (no
        // agentd is baked into the rootfs), so a cold boot without it can't
        // come up. Loud when unstaged.
        if spec.aux_ro_drives.iter().any(|d| d.sha256.is_none()) {
            let stamp = self.read_bundle_stamp().await?;
            let stamp_for = |key: &str| {
                stamp.get(key).cloned().ok_or_else(|| {
                    SandboxError::InvalidSpec(format!(
                        "reserved aux slots requested but this host's bundle stamp \
                         ({}/{}) carries no `{key}` entry — re-bake or roll the host image",
                        self.config.bundle_dir.display(),
                        AuxRoDrive::CURRENT_STAMP,
                    ))
                })
            };
            for drive in &mut spec.aux_ro_drives {
                if drive.sha256.is_none() {
                    // Lazy per-slot: only demand the stamp keys the spec's
                    // symbolic slots actually reference (a sentinel-only
                    // spec must not require an agentd entry, and vice
                    // versa).
                    let key = if drive.slot_index() == Some(AuxRoDrive::AGENTD_SLOT_INDEX) {
                        AuxRoDrive::AGENTD_STAMP_KEY
                    } else {
                        AuxRoDrive::SENTINEL_STAMP_KEY
                    };
                    drive.sha256 = Some(stamp_for(key)?);
                }
            }
        }

        let rootfs = spec
            .rootfs_source
            .clone()
            .ok_or_else(|| SandboxError::InvalidSpec("rootfs_source missing".into()))?;
        // Cold create: FC in the host root netns, TAP lives directly
        // on root (per-VM /30 via `net::provision`). ADR 0014 M1.16
        // only puts warm restores in their own netns; cold path
        // stays simpler.
        // ADR 0044 K2 / issue #197: `spawn_firecracker` arms the create-window
        // SIGKILL backstop internally (FC has no kill_on_drop), so it's live
        // across its own `wait_for_socket` and across everything below. Every
        // `?` from here to the `sandboxes.insert` reaps the half-spawned FC; we
        // `disarm()` it immediately before committing the live sandbox.
        let (socket, child, mut fc_guard) = self.spawn_firecracker(jail_dir, None).await?;

        // Configure + start. Any failure here drops the Child (no kill —
        // kill_on_drop is gone) and fires `fc_guard`, which SIGKILLs FC;
        // the Child drop then reaps the zombie via tokio's orphan queue.
        let api = FirecrackerClient::new(&socket);
        api.put_machine_config(&MachineConfig {
            vcpu_count: spec.cpu.vcpus.try_into().map_err(|_| {
                SandboxError::InvalidSpec(format!("cpu.vcpus {} doesn't fit in u8", spec.cpu.vcpus))
            })?,
            mem_size_mib: spec.memory.max_mib,
            smt: false,
            track_dirty_pages: self.config.track_dirty_pages,
            cpu_template: self.config.cpu_template.clone(),
        })
        .await?;
        // ADR 0088 addendum: the balloon must be attached BEFORE
        // InstanceStart (FC rejects post-boot device adds). Attached
        // deflated; only the capture-time seed shrink ever inflates it.
        if self.config.balloon {
            api.put_balloon(&client::BalloonConfig {
                amount_mib: 0,
                deflate_on_oom: true,
                stats_polling_interval_s: 1,
            })
            .await?;
        }
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
        // ADR 0021 P1.5: no second virtio-blk for the harness — the
        // harness binary travels in the rootfs at the manifest-
        // declared `[harness] exec` path.

        // ADR 0027 + 0035: extra read-only host-mounted bundles (the skills /
        // playwright squashfs), attached at their content-addressed staged
        // paths (`<drive_id>-<sha256>.squashfs`). The drives arrived symbolic
        // from the coord and were resolved against this host's bake stamp at
        // the top of create — `state.bin` therefore embeds an immutable
        // generation, never a path whose bytes a host roll can swap.
        //
        // HARD-FAIL on a missing staged file (no skip-if-absent hedge). This
        // cold-create path is base-snapshot capture (ADR 0020), an operator-
        // controlled step (POST /api/enabled-images). The stamp said this
        // generation is staged; if the file is absent the host image is
        // corrupt — 500 the enable loudly rather than silently capture a
        // skills-less snapshot every session would then inherit. A snapshot
        // therefore only ever exists with ALL its declared bundles resolved
        // and present, which is what the restore-side materialize relies on.
        for aux in &spec.aux_ro_drives {
            let staged = self.staged_bundle_path(aux)?;
            if !tokio::fs::try_exists(&staged).await.unwrap_or(false) {
                return Err(SandboxError::Vm(
                    format!(
                        "aux RO bundle {:?} ({}) is in this host's bundle stamp \
                         but not staged — the FC-host image is corrupt; re-bake \
                         or roll the host image before enabling this image",
                        staged.display(),
                        aux.drive_id
                    )
                    .into(),
                ));
            }
            api.put_drive(&DriveConfig {
                drive_id: aux.drive_id.clone(),
                path_on_host: staged.to_string_lossy().into_owned(),
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
        // ADR 0023: forge bridge accept loop, alongside the harness one.
        self.spawn_forge_listener(sandbox_id, &vsock_uds_path)
            .await?;
        // ADR 0026: artifact-upload bridge accept loop.
        self.spawn_upload_listener(sandbox_id, &vsock_uds_path)
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

        // ADR 0009 §5 / ADR 0044 K2: write the per-sandbox on-disk
        // manifest before handing control back, so the startup reattach
        // pass can re-adopt this still-live FC after a host-agent
        // restart. Cold create = host-root `network`, no netns, no uffd.
        if let Some(pid) = fc_pid {
            persist_sandbox_manifest(
                &self.work_dir,
                sandbox_id,
                &state,
                pid,
                net_setup.map(|ns| sandbox_manifest::NetworkRecord {
                    tap_name: ns.tap_name.clone(),
                    vm_cidr_network: ns.vm_cidr.network(),
                    host_ip: ns.vm_cidr.host(),
                    guest_ip: ns.vm_cidr.guest(),
                }),
                None,
                None,
            );
            // ADR 0044 K2: move FC into the node cgroup so a host-agent pod
            // restart doesn't `cgroup.kill` it. Cold create = FC only.
            place_vm_in_node_cgroup(self.config.vm_cgroup_parent.as_deref(), sandbox_id, &[pid]);
        }

        // FC is configured, started, and recorded in its manifest: the
        // sandbox is now committed. Defuse the create-window backstop so
        // the VM is fully decoupled from this host-agent's lifecycle.
        fc_guard.disarm();
        self.sandboxes.insert(
            sandbox_id,
            LiveSandbox {
                state,
                child: Some(child),
                fc_pid,
                uffd_handler: None,
                uffd_pid: None,
                net: net_setup.cloned(),
                // Cold create path: VM is in host root netns.
                netns: None,
                guest_endpoints: parking_lot::Mutex::new(None),
                #[cfg(target_os = "linux")]
                parked: false,
                agentd_slot_swapped: false,
                agent_ready: agent_ready_rx,
            },
        );
        if let Some(pid) = fc_pid {
            spawn_process_supervisor(
                self.sandboxes.clone(),
                sandbox_id,
                pid,
                "firecracker",
                self.supervisor_teardown_fn(),
            );
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
            // ADR 0020: keep accepting until we read a valid AgentReady, rather
            // than giving up after the first connection. On a slow cold boot the
            // guest's vsock connect can time out (FC's single device thread
            // starved by the rootfs read storm) and close before writing the
            // frame; agentd then re-dials, so we must re-accept. Looping also
            // keeps the watch sender alive across failed reads — otherwise the
            // first failure drops `tx`, the channel closes, and `wait_agent_ready`
            // returns "channel closed" instead of waiting its full deadline.
            // Bounded just past `wait_agent_ready`'s 180s so the task + UDS
            // listener can't outlive a destroyed sandbox.
            let listen = async {
                loop {
                    match listener.accept().await {
                        Ok((mut stream, _peer)) => {
                            match engram_agentd::read_msg::<_, engram_agentd::AgentReady>(
                                &mut stream,
                            )
                            .await
                            {
                                Ok(ready) => {
                                    tracing::info!(
                                        %sandbox_id,
                                        agent_version = %ready.agent_version,
                                        "agentd ready",
                                    );
                                    let _ = tx.send(true);
                                    return;
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        %sandbox_id,
                                        error = %e,
                                        "agent-ready read failed; re-accepting (guest likely \
                                         re-dialing under slow-boot I/O)",
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, %sandbox_id, "agent-ready accept failed; re-accepting");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                    // Sandbox destroyed (all receivers dropped) → stop.
                    if tx.is_closed() {
                        return;
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(190), listen).await;
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

    /// ADR 0023: per-sandbox accept loop for the in-guest forge helper
    /// (`FORGE_VSOCK_PORT`). Mirrors `spawn_harness_listener` — binds
    /// `<vsock_uds>_<FORGE_VSOCK_PORT>` and fires `forge_sink` for each
    /// guest dial. Re-stood-up on resume alongside the harness listener.
    async fn spawn_forge_listener(
        &self,
        sandbox_id: SandboxId,
        vsock_uds_path: &Path,
    ) -> Result<(), SandboxError> {
        let path = forge_uds_for(vsock_uds_path);
        let _ = tokio::fs::remove_file(&path).await;
        let listener = tokio::net::UnixListener::bind(&path).map_err(|e| {
            SandboxError::Vm(
                format!(
                    "bind forge UDS {} for sandbox {sandbox_id}: {e}",
                    path.display()
                )
                .into(),
            )
        })?;
        let sink_slot = self.forge_sink.clone();
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
                                "forge connection arrived but no sink registered; dropping",
                            );
                        }
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, %sandbox_id, "forge UDS accept ended");
                        return;
                    }
                }
            }
        });
        Ok(())
    }

    /// ADR 0026: per-sandbox accept loop for the in-guest `engram-share`
    /// helper (`UPLOAD_VSOCK_PORT`). Mirrors `spawn_forge_listener` —
    /// binds `<vsock_uds>_<UPLOAD_VSOCK_PORT>` and fires `upload_sink`
    /// for each guest dial. Re-stood-up on resume alongside the harness
    /// and forge listeners.
    async fn spawn_upload_listener(
        &self,
        sandbox_id: SandboxId,
        vsock_uds_path: &Path,
    ) -> Result<(), SandboxError> {
        let path = upload_uds_for(vsock_uds_path);
        let _ = tokio::fs::remove_file(&path).await;
        let listener = tokio::net::UnixListener::bind(&path).map_err(|e| {
            SandboxError::Vm(
                format!(
                    "bind upload UDS {} for sandbox {sandbox_id}: {e}",
                    path.display()
                )
                .into(),
            )
        })?;
        let sink_slot = self.upload_sink.clone();
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
                                "upload connection arrived but no sink registered; dropping",
                            );
                        }
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, %sandbox_id, "upload UDS accept ended");
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
    // 8 args: the D4 canonical ref is a restore-time input that can't ride
    // the capture-time sidecar; bundling into a params struct is the cleanup
    // when the next one arrives.
    #[allow(clippy::too_many_arguments)]
    async fn restore_in_jail(
        &self,
        sandbox_id: SandboxId,
        jail_dir: &Path,
        snapshot_dir: &Path,
        manifest: &FcSnapshotManifest,
        swap_aux_to_current: bool,
        // ADR 0055: per-session skills the coordinator assigned to reserved
        // slots (dyn_i + content sha), patch_drived in load-paused. Empty on
        // resume / reattach.
        selected_mounts: Vec<AuxRoDrive>,
        // ADR 0022: the effective memory backend for THIS restore
        // (base-create may be File while resume is UFFD). Computed by the
        // caller via `effective_restore_mode` rather than read from
        // `self.config.restore_mode`, which is now resume-only.
        restore_mode: RestoreMode,
        // ADR 0045 D4: the IMAGE's base manifest from the coordinator's
        // restore metadata (a restore-time input — the capture-time
        // sidecar can't know it). `Some` ⇒ substrate restores CONTINUE
        // base-identical pages against the shared per-image base shm;
        // `None` ⇒ canonical == session (old coordinators, reattach).
        base_memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
    ) -> Result<(), SandboxError> {
        let state_path = snapshot_dir.join("state.bin");
        let mem_path = snapshot_dir.join("memory.bin");

        // ADR 0048 (surfaced by the fleet load test): same-base concurrent
        // restores share the FC-embedded `source_rootfs_canonical` rootfs path
        // and collide on it. The fix — install that symlink + hold a
        // per-source-path lock ONLY across `load_snapshot` (when FC opens the
        // rootfs fd) — lives right before the load below, NOT here: holding it
        // across all of `restore_in_jail` (netns + spawn + the cold chunk
        // fetch) serialized a burst so hard the coordinator's restore RPC timed
        // out. The load itself is ~5 ms in UFFD mode, so the tight window is
        // nearly free.

        // ADR 0035 §3/§4: aux RO bundles.
        //
        // The PINNED generation must be present regardless of flavor —
        // `load_snapshot` opens the `state.bin`-embedded path before any
        // `patch_drive` is possible. `PooledBackend::restore` materializes
        // missing generations from BlobStorage before calling here; this
        // assert is the backstop that turns a miss into a clear error
        // instead of an opaque FC virtio "No such file".
        //
        // On the fresh-create flavor (`swap_aux_to_current`) we also plan a
        // post-load, pre-resume `patch_drive` to this host's CURRENT
        // generation for any drive whose pin is stale — that's how a skill
        // edit reaches new sessions without re-enabling images (Invariant 2).
        // Resumes never swap: live guest processes may hold fds into the
        // pinned bundle, and an in-flight session keeps the world it was
        // working in. Missing stamp / missing current file degrade to the
        // pinned generation with a warning — staler skills beat a failed
        // create.
        let mut live_spec = manifest.spec.clone();
        let mut aux_swap_plan: Vec<(String, PathBuf)> = Vec::new();
        if !live_spec.aux_ro_drives.is_empty() {
            for aux in &live_spec.aux_ro_drives {
                let pinned = self.staged_bundle_path(aux)?;
                if !tokio::fs::try_exists(&pinned).await.unwrap_or(false) {
                    return Err(SandboxError::Snapshot(format!(
                        "aux RO bundle {} ({}) pinned by the snapshot is not \
                         staged on this host and wasn't materialized from \
                         BlobStorage — restore can't proceed",
                        pinned.display(),
                        aux.drive_id
                    )));
                }
            }
            if swap_aux_to_current {
                match self.read_bundle_stamp().await {
                    Ok(stamp) => {
                        for aux in &mut live_spec.aux_ro_drives {
                            let Some(current_sha) = stamp.get(&aux.drive_id) else {
                                tracing::warn!(
                                    drive_id = %aux.drive_id,
                                    "bundle stamp carries no entry for pinned aux \
                                     drive; keeping the pinned generation",
                                );
                                continue;
                            };
                            if aux.sha256.as_deref() == Some(current_sha.as_str()) {
                                continue; // pin is already current
                            }
                            let current_path = self
                                .config
                                .bundle_dir
                                .join(AuxRoDrive::staged_file_name(current_sha));
                            if !tokio::fs::try_exists(&current_path).await.unwrap_or(false) {
                                tracing::warn!(
                                    drive_id = %aux.drive_id,
                                    current = %current_path.display(),
                                    "stamp's current bundle generation is not \
                                     staged (corrupt host image?); keeping the \
                                     pinned generation",
                                );
                                continue;
                            }
                            aux_swap_plan.push((aux.drive_id.clone(), current_path));
                            // The live VM's device now points at the current
                            // generation — record that on the live spec so a
                            // later `snapshot()` pins what's actually attached.
                            aux.sha256 = Some(current_sha.clone());
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "no bundle stamp on this host; fresh create keeps \
                             the snapshot's pinned bundle generations",
                        );
                    }
                }
            }
        }
        // ADR 0080: does this restore actually change the agentd slot's
        // content? Computed against the snapshot's pinned generation
        // BEFORE the loop below overwrites it on the live spec.
        // `refresh_agent` skips the in-guest RPC when this stays false
        // (the steady state), keeping the create path's added latency at
        // zero except on the first creates after an agentd roll.
        let agentd_drive_id =
            AuxRoDrive::slot_drive_id(engram_core::types::sandbox::AuxRoDrive::AGENTD_SLOT_INDEX);
        let mut agentd_slot_swapped = false;
        // ADR 0055: per-session skill selection. The coordinator assigned each
        // selected skill to a reserved slot (dyn_i) + content sha; plan a
        // `patch_drive` over that slot's sentinel and record it on the live spec
        // so a later eviction snapshot pins the skill (resume re-attaches it).
        // Only the fresh-create flavor carries selections; resume passes none.
        for sel in &selected_mounts {
            // ADR 0062: resolve via the backend's `bundle_dir` (config) — the
            // single source of truth — NOT a hardcoded SHARED_DIR. The sentinel
            // swap just above already uses `config.bundle_dir`; this used to read
            // `AuxRoDrive::staged_path()` (hardcoded SHARED_DIR), which only
            // matched while the FC config silently defaulted there too. On a
            // dev/e2e host (ENGRAM_BUNDLE_DIR) it checked the wrong dir and every
            // selected skill/harness 404'd "not staged".
            let staged = self.staged_bundle_path(sel)?;
            if !tokio::fs::try_exists(&staged).await.unwrap_or(false) {
                return Err(SandboxError::Snapshot(format!(
                    "ADR 0055 selected skill {} for slot {} is not staged on this \
                     host (catalog materialize gap?) — restore can't proceed",
                    sel.sha256.as_deref().unwrap_or("?"),
                    sel.drive_id,
                )));
            }
            // Explicit per-session selection wins over any sentinel→current swap
            // already planned for this slot.
            aux_swap_plan.retain(|(id, _)| id != &sel.drive_id);
            aux_swap_plan.push((sel.drive_id.clone(), staged));
            if let Some(slot) = live_spec
                .aux_ro_drives
                .iter_mut()
                .find(|d| d.drive_id == sel.drive_id)
            {
                // ADR 0080: an agentd-slot selection that differs from the
                // snapshot's pin is the (rare) "agentd rolled since capture"
                // case `refresh_agent` acts on.
                if sel.drive_id == agentd_drive_id && slot.sha256 != sel.sha256 {
                    agentd_slot_swapped = true;
                }
                slot.sha256 = sel.sha256.clone();
            }
        }
        // ADR 0045 C2: a post-copy destination starts restoring BEFORE
        // the source pauses — state.bin appears later via the fetch
        // poller, and the pre-load gate below waits for it.
        let post_copy = snapshot_dir.join(MIGRATION_PEER_FILE).exists();
        if !post_copy && !state_path.exists() {
            return Err(SandboxError::Snapshot(format!(
                "snapshot state.bin missing at {}",
                state_path.display()
            )));
        }
        // ADR 0020 Route B: UFFD mode faults memory pages from chunks
        // on demand via the handler we spawn below; memory.bin is not
        // read by `PUT /snapshot/load` in this mode and won't exist on
        // disk after a cross-host receive (PooledBackend::restore
        // skips `materialize_memory_if_missing` when
        // `restore_memory_is_lazy_for(fresh)` returns true). Requiring it here
        // unconditionally was the prod blocker on 2026-05-29: after a
        // host-agent pod restart the new hosts had every base-snapshot restore fail
        // with "snapshot memory.bin missing" before the mode-specific
        // load branch could run. Same-host UFFD restore passed only
        // because `PooledBackend::snapshot` had already written
        // memory.bin during capture on the same host.
        if matches!(restore_mode, RestoreMode::File) && !mem_path.exists() {
            return Err(SandboxError::Snapshot(format!(
                "snapshot memory.bin missing at {}",
                mem_path.display()
            )));
        }

        // ADR 0020 P3 fix: create jail_dir up front, before the parallel legs.
        // Both spawn_firecracker (its chroot/jail setup) AND spawn_uffd_handler
        // (its log + UDS at jail_dir/{uffd-handler.log,uffd.sock}) write here. In
        // the old serial form FC-spawn's own create_dir_all ran first; once the
        // two legs run concurrently the UFFD leg races it and File::create on the
        // log fails with ENOENT (prod regression 501e30e, reverted a91f83b).
        // Creating it here is idempotent with FC-spawn's create_dir_all and makes
        // the dir present for whichever leg touches it first.
        tokio::fs::create_dir_all(jail_dir).await.map_err(|e| {
            vm_err(format!(
                "create restore jail dir {}: {e}",
                jail_dir.display()
            ))
        })?;

        // ADR 0014 M1.16: warm-restore networking provisions a
        // per-VM netns BEFORE spawning FC. The netns has the bake's
        // TAP recreated inside it (collision-free vs other warm VMs
        // on the same host) plus SNAT remapping the VM's bake-time
        // source IP to a unique-per-VM pool slot. FC enters the netns
        // via `spawn_firecracker`'s direct exec + `pre_exec` `setns(2)`
        // closure (no `ip netns exec` wrapper fork) so `load_snapshot`'s
        // TAP open resolves inside it.
        //
        // Legacy/test snapshots with `manifest.net = None` keep the
        // historical host-root flow: TAP lives directly on host
        // root, FC stays in host root, no netns at all.
        // ADR 0020 P3: the restore-setup legs are independent up to the
        // `load_snapshot` gate. {reserve netns → spawn FC into it} is a real
        // chain, but spawning the UFFD handler (a separate process on its own
        // UDS) need not wait for it. Run the two concurrently. We use `join!`,
        // NOT `try_join!`: try_join cancels the slower leg on the first error,
        // which can strand a half-created netns / FC child / handler; `join!`
        // lets both finish, then we tear down whatever succeeded if either
        // failed. Measured (trace 309633f4): collapses the ~250 ms serial setup
        // to ~max-leg — the ~102 ms UFFD spawn hides under the ~110 ms
        // netns→spawn chain. Each leg self-cleans its own partial state, so its
        // Err carries no live resource; reconciliation below only undoes the
        // OTHER leg.

        // Leg 1 — netns → FC spawn → rootfs symlinks (a chain; the symlinks are
        // ~0.5 ms so they ride here rather than as a third leg). Spans inline.
        let leg_setup = async {
            let netns_setup = tracing::Instrument::instrument(
                self.reserve_restored_netns(sandbox_id, manifest.net.as_ref()),
                tracing::info_span!("fc.reserve_netns"),
            )
            .await?;
            // Issue #197: arm the drop-based netns teardown the instant the
            // netns is provisioned, so ANY subsequent cancel/early-return
            // (FC spawn failing, the symlink step, a cancellation of the
            // whole restore future inside the join below) reclaims the
            // netns + veth + SNAT slot. Replaces the per-`Err`-arm
            // `teardown_netns` calls this leg used to carry.
            let netns_name = netns_setup.as_ref().map(|s| s.netns_name.clone());
            let netns_guard = NetnsGuard::new(netns_setup, Arc::clone(&self.net_allocator));
            let (socket, child, fc_guard) = tracing::Instrument::instrument(
                self.spawn_firecracker(jail_dir, netns_name.as_deref()),
                tracing::info_span!("fc.spawn_process"),
            )
            .await?;
            // ADR 0014: `state.bin` embeds the canonical rootfs path keyed by
            // the SOURCE sandbox_id; recreate the source-id-keyed symlink
            // pointing at the host-local backing before `load_snapshot` opens
            // it. On failure `fc_guard` (SIGKILL FC) and `netns_guard` (tear
            // the netns down) fire on drop — no manual cleanup needed.
            tracing::Instrument::instrument(
                restore_canonical_symlinks(&self.work_dir, sandbox_id, manifest),
                tracing::info_span!("fc.restore_symlinks"),
            )
            .await?;
            Ok::<_, SandboxError>((netns_guard, socket, child, fc_guard))
        };

        // Leg 2 — spawn the UFFD page-fault handler (Uffd mode) so it's
        // listening before `load_snapshot` connects; File mode has none.
        // Issue #197: `spawn_uffd_handler` arms a `SpawnKillGuard` internally
        // (no kill_on_drop — ADR 0044 K2), so a cancellation while it parks
        // on the peer control sock SIGKILLs the handler on drop. Returns
        // (handler, the UDS the gate dials, the guard).
        let leg_uffd = async {
            match restore_mode {
                RestoreMode::File => Ok::<_, SandboxError>(None),
                RestoreMode::Uffd => {
                    // ADR 0007: the handler reads its memory manifest from the
                    // chunk store; without one, refuse loud rather than
                    // silently lose chunked restore.
                    let session_ref = manifest.memory_manifest.ok_or_else(|| {
                        SandboxError::Snapshot(
                            "RestoreMode::Uffd requires manifest.memory_manifest \
                             (snapshot wasn't taken via PooledBackend with a \
                             chunk_store attached; either wrap the FC backend \
                             with PooledBackend.with_chunk_store(...) before \
                             snapshotting, or switch to RestoreMode::File)"
                                .into(),
                        )
                    })?;
                    // ADR 0045 D4: the canonical ref is the IMAGE's base
                    // manifest when the coordinator supplies it — resumed
                    // sessions then CONTINUE base-identical pages against
                    // the SHARED per-image base shm (density across fresh +
                    // resumed sessions, and resumes stop minting
                    // session-keyed base files). Fallback (old coordinator
                    // or fresh create, where the snapshot IS the base):
                    // canonical == session, the ADR 0015 M5 behavior.
                    let canonical_ref = base_memory_manifest.unwrap_or(session_ref);
                    let uffd_uds = jail_dir.join("uffd.sock");
                    let _ = tokio::fs::remove_file(&uffd_uds).await;
                    // ADR 0045 C1: a migration destination pre-stages the
                    // not-yet-durable session manifest as a local file in
                    // the snapshot dir (the catch-up upload publishes it
                    // later); when present, the handler resolves the
                    // session manifest from disk instead of the store.
                    let migration_manifest = snapshot_dir.join("migration-session-manifest.json");
                    let migration_manifest =
                        migration_manifest.exists().then_some(migration_manifest);
                    // ADR 0045 C2: a post-copy destination additionally
                    // stages the peer spec — the handler dials the
                    // source's page server and parks for the SEAL.
                    let peer_spec: Option<MigrationPeerSpec> = {
                        let p = snapshot_dir.join(MIGRATION_PEER_FILE);
                        match tokio::fs::read(&p).await {
                            Ok(bytes) => Some(serde_json::from_slice(&bytes).map_err(|e| {
                                SandboxError::Snapshot(format!("parse {MIGRATION_PEER_FILE}: {e}"))
                            })?),
                            Err(_) => None,
                        }
                    };
                    // ADR 0007 Phase 5: prefault host from the sidecar (capture
                    // host); publish under THIS host so later restores here use
                    // the local trace.
                    let prefault_host = manifest.trace_host_hint.map(|hid| hid.as_uuid());
                    let publish_host = self.config.host_id.map(|hid| hid.as_uuid());
                    // Tier 2 (resume-prefault fix): the session-stable trace
                    // key from the sidecar. Present on resume/evac restores
                    // (stamped by the host-agent at snapshot-finish); `None`
                    // on base snapshots keeps the per-host manifest keying.
                    let trace_key = manifest.trace_lineage_id.map(|sid| sid.as_uuid());
                    let (handler, uffd_guard) = self
                        .spawn_uffd_handler(
                            &uffd_uds,
                            canonical_ref,
                            session_ref,
                            migration_manifest.as_deref(),
                            prefault_host,
                            publish_host,
                            trace_key,
                            jail_dir,
                            peer_spec.as_ref(),
                        )
                        .await?;
                    Ok(Some((handler, uffd_uds, uffd_guard)))
                }
            }
        };

        let (setup_res, uffd_res) = tokio::join!(leg_setup, leg_uffd);

        // Reconcile. Issue #197: every per-leg resource now rides a Drop
        // guard (FC + uffd on `SpawnKillGuard`, the netns on `NetnsGuard`),
        // so on ANY failure we just `return Err` — dropping the OTHER leg's
        // success tuple fires its guards and self-cleans. No manual
        // kill/teardown reconciliation (and no window where one leg's
        // resources are live but unguarded).
        let (netns_guard, socket, child, mut fc_guard, uffd_leg) = match (setup_res, uffd_res) {
            (Ok((netns_guard, socket, child, fc_guard)), Ok(uffd)) => {
                (netns_guard, socket, child, fc_guard, uffd)
            }
            // On either-or-both failure, the surviving Ok tuple drops here,
            // running its guards. Surface the setup error first when present.
            (Err(e), _) => return Err(e),
            (Ok(_), Err(e)) => return Err(e),
        };

        // The guards armed inside the legs stay live from here through
        // `load_snapshot` and the listener spawns — every `?` below reaps FC,
        // the uffd handler, and the netns. They're disarmed just before the
        // `sandboxes.insert` once the VM is resumed and committed (ADR 0044
        // K2: a committed VM is decoupled from this host-agent's lifecycle).
        let (uffd_leg, mut uffd_guard) = match uffd_leg {
            Some((handler, uds, guard)) => (Some((handler, uds)), Some(guard)),
            None => (None, None),
        };

        // Legacy/test path: no netns means we still might need the
        // old host-root TAP setup (when `manifest.net.is_some` but
        // the host is configured without a netns-style pool — e.g.
        // a fixture that wants a direct TAP). For now we only take
        // the netns path; legacy snapshots without `manifest.net`
        // get `None` here and keep restoring netless.
        let net_setup: Option<net::NetSetup> = None;

        // `PUT /snapshot/load` in File mode reads the entire memory.bin
        // synchronously before resuming — the symmetric cost to
        // `create_snapshot` (which already uses 60s here for the same
        // reason). The default 10s client timeout fits a tiny VM but
        // trips on a multi-GiB one (a 4 GiB base snapshot false-failed
        // with "PUT /snapshot/load timed out after 10s" on a cold
        // chunk-materialized memory.bin). Give the load the same 60s.
        // (UFFD mode returns immediately — pages fault lazily — so this
        // ceiling only bites File mode; tune up for huge VMs.)
        let api = FirecrackerClient::new(&socket).with_timeout(Duration::from_secs(60));

        // Re-key the vsock UDS to THIS sandbox's id via the fork's
        // `vsock_override` load param. Without the override FC binds the
        // ancestor path embedded in state.bin (`source_vsock_canonical`),
        // which is shared by EVERY VM descended from the same base
        // capture — two same-image VMs on one host then fight over one
        // absolute path, and exec/harness traffic silently follows the
        // last binder (the 2026-06-11 cross-session misroute: a teleport
        // dest, an idle-resume, and a warm-pool refill of one image took
        // turns stealing each other's channels). Per-sandbox keying makes
        // the path unique by construction; the unlink below only ever
        // touches OUR OWN id-keyed path, never a live sibling's.
        let vsock_uds_path = paths::vsock_uds_path(&self.work_dir, sandbox_id);
        let _ = tokio::fs::remove_file(&vsock_uds_path).await;

        // The UFFD handler was already spawned concurrently in leg 2
        // (`uffd_leg`); the gate just dials it. File mode loads memory.bin
        // synchronously; Uffd returns immediately and pages fault lazily.
        // Either way the VM is running once `load_snapshot*` returns.
        // ADR 0035 §3: with a pending aux-bundle swap, load PAUSED, patch
        // each stale drive to the host's current generation (FC reopens the
        // backing file on PATCH /drives), then resume — the same load-paused
        // → patch → resume sequence ADR 0014's warm-lease harness swap used
        // (pinned by `tests/patch_drive_swap.rs`). agentd umount/remounts the
        // bundle mounts at session bind so the guest's squashfs superblock
        // re-parses the swapped device. With no swap pending, keep the
        // single-call load-and-resume.
        // ADR 0048 (load-test finding): install the SHARED, FC-embedded
        // `source_rootfs_canonical` rootfs symlink pointing at OUR device, and
        // hold a per-source-path lock ACROSS the load below so a concurrent
        // same-base restore can't repoint it between our install and FC opening
        // the rootfs fd (that repoint → FC opens the WRONG device; the racing
        // install → "/dev/nbdN: File exists" 503). The per-NEW-sandbox
        // `canonical` was already installed in leg 1 (unique, uncontended).
        // Scoped to the load only: UFFD load returns in ~ms, so this serializes
        // just the FD-open instant — the netns/spawn/cold chunk-fetch above all
        // ran concurrently. Distinct bases use distinct paths and never contend.
        let uniq_canonical = paths::rootfs_canonical(&self.work_dir, sandbox_id);
        let _src_canon_guard = match (
            manifest.source_rootfs_canonical.as_ref(),
            manifest.spec.rootfs_source.as_ref(),
        ) {
            (Some(src), Some(target)) if src.as_path() != uniq_canonical.as_path() => {
                let guard = source_canonical_lock(src).lock_owned().await;
                if let Some(parent) = src.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        vm_err(format!(
                            "create source rootfs canonical parent {}: {e}",
                            parent.display()
                        ))
                    })?;
                }
                paths::install_symlink(src, target).await.map_err(|e| {
                    vm_err(format!(
                        "restore source rootfs canonical symlink {} -> {}: {e}",
                        src.display(),
                        target.display()
                    ))
                })?;
                Some(guard)
            }
            _ => None,
        };
        let load_result: Result<(), SandboxError> = async {
            match &uffd_leg {
                None => {
                    let paths = SnapshotPaths {
                        state_path: state_path.clone(),
                        mem_path: mem_path.clone(),
                    };
                    tracing::Instrument::instrument(
                        async {
                            // ADR 0028: restored VMs re-arm dirty
                            // tracking here (no machine-config PUT on
                            // the restore path).
                            api.load_snapshot_opts(
                                &paths,
                                /*resume_vm=*/ aux_swap_plan.is_empty(),
                                self.config.track_dirty_pages,
                                Some(&vsock_uds_path),
                            )
                            .await
                        },
                        tracing::info_span!("fc.load_snapshot", mode = "file"),
                    )
                    .await?;
                }
                Some((_handler, uffd_uds)) => {
                    // ADR 0045 C2 load gates: in post-copy mode the
                    // handler binds its UDS only once SEALED, and
                    // state.bin lands only once the fetch poller pulls
                    // it from the captured source. Wait for both (the
                    // blackout-side budget: capture + fetch; generous —
                    // a timeout tears the restore down via the normal
                    // failure path and the coordinator aborts the move).
                    if post_copy {
                        let budget = Duration::from_secs(240);
                        let started = std::time::Instant::now();
                        while !(uffd_uds.exists() && state_path.exists()) {
                            if started.elapsed() > budget {
                                // The "postcopy-never-loaded" marker is
                                // LOAD-BEARING: the coordinator's abort-
                                // to-source arm keys on it (the timeout
                                // PRECEDES the FC load, so the dest
                                // provably never ran this state and an
                                // un-pause of the source is zero-loss
                                // sound).
                                return Err(SandboxError::Snapshot(format!(
                                    "postcopy-never-loaded: load gate timed out after {budget:?} \
                                     (uds: {}, state.bin: {})",
                                    uffd_uds.exists(),
                                    state_path.exists(),
                                )));
                            }
                            // 2 ms: a file-existence check is ~µs and
                            // this wait sits inside the guest-observed
                            // blackout — the old 25 ms grain was pure
                            // tax.
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                        tracing::info!(
                            sandbox_id = %sandbox_id,
                            waited_ms = started.elapsed().as_millis() as u64,
                            "post-copy load gate open (sealed + state.bin staged)",
                        );
                    }
                    // ADR 0045 substrate: same canonical ref as the handler
                    // spawn (D4: the image base manifest when supplied, else
                    // the session manifest), so the derived base path always
                    // matches the handler's.
                    let base = base_memory_manifest
                        .or(manifest.memory_manifest)
                        .and_then(|r| self.uffd_base_path(&r));
                    let t_load = std::time::Instant::now();
                    tracing::Instrument::instrument(
                        async {
                            api.load_snapshot_uffd_opts(
                                &state_path,
                                uffd_uds,
                                /*resume_vm=*/ aux_swap_plan.is_empty(),
                                self.config.track_dirty_pages,
                                base.as_deref(),
                                Some(&vsock_uds_path),
                            )
                            .await
                        },
                        tracing::info_span!("fc.load_snapshot", mode = "uffd"),
                    )
                    .await?;
                    // Restore-tail attribution (blackout-critical on a
                    // post-copy dest: the load demand-faults early
                    // guest pages through the handler → P2P, and the
                    // resume rides this call when no aux swap runs).
                    tracing::info!(
                        sandbox_id = %sandbox_id,
                        load_ms = t_load.elapsed().as_millis() as u64,
                        resumed_in_load = aux_swap_plan.is_empty(),
                        "fc snapshot load complete (uffd)",
                    );
                }
            }
            if !aux_swap_plan.is_empty() {
                let span = tracing::info_span!("fc.swap_aux_bundles");
                tracing::Instrument::instrument(
                    async {
                        for (drive_id, current_path) in &aux_swap_plan {
                            api.patch_drive(drive_id, current_path).await?;
                            tracing::info!(
                                %drive_id,
                                to = %current_path.display(),
                                "aux bundle swapped to host's current generation",
                            );
                        }
                        api.resume().await
                    },
                    span,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        // FC has opened the rootfs fd (load returned); the shared path is free
        // for the next same-base restore. Release before the post-load work.
        drop(_src_canon_guard);

        let uffd_handler: Option<Child> = match load_result {
            Ok(()) => uffd_leg.map(|(handler, _uds)| handler),
            Err(e) => {
                // Snapshot load failed. Issue #197: the still-armed
                // `fc_guard` / `uffd_guard` SIGKILL FC + the handler, and
                // `netns_guard` tears down the per-VM netns + frees the SNAT
                // slot — all on this early-return drop (ADR 0044 K2). No
                // manual cleanup needed.
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
        // ADR 0021 P1.5: no harness drive to repoint — the harness
        // travels in the rootfs now. The repoint_harness_drive PATCH
        // path can also retire once nothing else gates on it.

        // Carry the manifest's spec forward so SandboxState reflects
        // what the snapshot was taken from. rootfs_path mirrors what
        // the original VM had attached — Firecracker reopens that
        // path on load, so it must still be valid on disk.
        let rootfs_path = manifest.spec.rootfs_source.clone().unwrap_or_default();
        // `vsock_uds_path` was re-keyed to THIS sandbox's id before the
        // load (the fork's `vsock_override` rewrote the device state, so
        // FC bound it — NOT the `source_vsock_canonical` ancestor path
        // that PUT /vsock's post-load 400 used to force us to inherit).
        // The manifest's `source_vsock_canonical` is lineage metadata
        // only from here on.
        let vsock_cid = self.next_cid.fetch_add(1, Ordering::Relaxed);
        // Re-spawn the harness accept loop for the restored VM at the
        // re-keyed base. The host-side accept loop that originally bound
        // `<uds>_1026` died with the pre-snapshot sandbox (or never
        // existed on this host). Without this, the in-VM adapter's
        // post-resume reconnect dial finds no listener.
        self.spawn_harness_listener(sandbox_id, &vsock_uds_path)
            .await?;
        // ADR 0023: forge bridge accept loop, alongside the harness one.
        self.spawn_forge_listener(sandbox_id, &vsock_uds_path)
            .await?;
        // ADR 0026: artifact-upload bridge accept loop.
        self.spawn_upload_listener(sandbox_id, &vsock_uds_path)
            .await?;
        let state = SandboxState {
            // ADR 0035: `live_spec` reflects any aux-bundle swap above, so a
            // later `snapshot()` of this sandbox pins the generation that's
            // actually attached.
            spec: live_spec,
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
        // VM is resumed, listeners are up, and (below) its manifest is
        // written: the restored sandbox is committed. Defuse the
        // create-window backstops so the VM is fully decoupled from this
        // host-agent's lifecycle (ADR 0044 K2 detach). Issue #197: the netns
        // guard hands its `NetnsSetup` over to the live sandbox here — past
        // this point `destroy()` owns netns teardown, not the guard.
        fc_guard.disarm();
        if let Some(g) = uffd_guard.as_mut() {
            g.disarm();
        }
        let netns_setup = netns_guard.into_committed();
        // ADR 0044 K2: persist the manifest so the successor host-agent
        // can reattach this still-live restored VM — recording the per-VM
        // netns (warm restores) and the uffd handler pid (Uffd mode).
        if let Some(fc_pid_u) = fc_pid {
            persist_sandbox_manifest(
                &self.work_dir,
                sandbox_id,
                &state,
                fc_pid_u,
                None, // warm restores run in a per-VM netns, not host-root
                netns_setup
                    .as_ref()
                    .map(|ns| sandbox_manifest::NetnsRecord {
                        netns_name: ns.netns_name.clone(),
                        veth_host: ns.veth_host.clone(),
                        veth_ns: ns.veth_ns.clone(),
                        tap_name: ns.tap_name.clone(),
                        vm_cidr_network: ns.vm_cidr.network(),
                        snat_cidr_network: ns.snat_cidr.network(),
                    }),
                uffd_pid,
            );
            // ADR 0044 K2: move FC AND the uffd handler (Uffd mode) into the
            // node cgroup — both must survive a host-agent pod restart, or a
            // killed handler leaves the restored guest page-faulting forever.
            let mut pids = vec![fc_pid_u];
            pids.extend(uffd_pid);
            place_vm_in_node_cgroup(self.config.vm_cgroup_parent.as_deref(), sandbox_id, &pids);
        }
        self.sandboxes.insert(
            sandbox_id,
            LiveSandbox {
                state,
                child: Some(child),
                fc_pid,
                uffd_handler,
                uffd_pid,
                net: net_setup,
                netns: netns_setup,
                guest_endpoints: parking_lot::Mutex::new(None),
                #[cfg(target_os = "linux")]
                parked: false,
                agentd_slot_swapped,
                agent_ready: ready_rx,
            },
        );
        if let Some(pid) = fc_pid {
            spawn_process_supervisor(
                self.sandboxes.clone(),
                sandbox_id,
                pid,
                "firecracker",
                self.supervisor_teardown_fn(),
            );
        }
        if let Some(pid) = uffd_pid {
            spawn_process_supervisor(
                self.sandboxes.clone(),
                sandbox_id,
                pid,
                "uffd-handler",
                self.supervisor_teardown_fn(),
            );
        }
        tracing::info!(
            %sandbox_id,
            jail = %jail_dir.display(),
            from = %snapshot_dir.display(),
            mode = ?restore_mode,
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

    // ADR 0021 P1.5: `repoint_harness_drive` retired with option-D.
}

fn vm_err(msg: impl Into<String>) -> SandboxError {
    SandboxError::Vm(msg.into().into())
}

/// #567 / prod session 8174b7aa: an agentd RPC recv that hits a bare
/// early EOF (zero bytes back) is indistinguishable, on the wire,
/// from agentd crashing mid-call -- but it's *exactly* what an older
/// guest agentd produces when the host sends a `WireRequest` variant
/// the guest predates: the frame fails to bincode-decode, `serve_
/// connection` returns before writing anything, and the connection
/// just drops (engram-agentd's typed-NAK fix only helps once the
/// guest image is re-baked). Appends both plausible causes to an
/// EOF-shaped recv error so an operator doesn't have to already know
/// this incident's history to triage it; every other error passes
/// through untouched -- this is purely additive context, not a new
/// error path.
fn describe_agentd_rpc_recv_failure(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        format!(
            "{e} -- agentd closed the stream without a response: guest \
             agentd may predate this request (host/guest version skew: \
             re-bake + RefreshImage) or crashed mid-call"
        )
    } else {
        e.to_string()
    }
}

/// #567: the FC vsock CONNECT handshake for the ADR-0066 port relay
/// (guest port [`engram_harness_proto::PROXY_PORT_VSOCK_PORT`])
/// failing with an early EOF means nothing accepted the CONNECT
/// inside the guest -- either this agentd predates the relay
/// listener entirely (host/guest version skew), or a once-live relay
/// died (the dead-relay bug #567's other fixes target). Gated on the
/// relay port specifically: `connect_fc_vsock_once` is shared by
/// every guest port (exec included), and the same read failing on
/// agentd's exec port (1024) means something else entirely.
fn describe_relay_connect_failure(port: u32, e: &std::io::Error) -> String {
    if port == engram_harness_proto::PROXY_PORT_VSOCK_PORT
        && e.kind() == std::io::ErrorKind::UnexpectedEof
    {
        format!(
            "{e} -- no listener on the guest relay port: guest agentd may \
             predate the ADR-0066 relay (version skew: re-bake + \
             RefreshImage), or the relay died (#567)"
        )
    } else {
        e.to_string()
    }
}

/// Timeout for `PUT /snapshot/create`. The call flushes the full guest
/// memory to memory.bin synchronously, so its duration scales with guest
/// RAM. A flat 60s fit small VMs but tripped on large warm images — the
/// dev-brain 32 GiB capture failed with "PUT /snapshot/create timed out
/// after 60s". Budget a 60s base (pause + device serialize; preserves the
/// prior behavior for small VMs) plus a conservative 128 MiB/s flush-
/// throughput floor (the host disk may be cold or shared), so a 32 GiB VM
/// gets ~5min instead of 60s. A too-generous ceiling is harmless — a
/// genuinely hung FC is caught by the process supervisor, not this timeout.
fn snapshot_create_timeout(mem_mib: u32) -> Duration {
    const BASE_SECS: u64 = 60;
    // Per-MiB write budget. FC's `PUT /snapshot/create` writes the full guest
    // `memory.bin` to local disk; this floor is the slowest sustained write we
    // budget for. The 291 GiB pd-ssd host disks nominally do ~140 MiB/s, but
    // during a base-snapshot capture the concurrent chunk-flush (reading the
    // snapshot back to upload it to GCS) roughly halves effective throughput to
    // ~70 MiB/s. A 128 MiB/s floor was too optimistic: the 24 GiB dev-brain
    // guest's 252s budget expired mid-write (prod, 2026-06). 64 MiB/s gives that
    // guest ~444s, comfortably above the observed write time.
    const MIB_PER_SEC_FLOOR: u64 = 64;
    Duration::from_secs(BASE_SECS + mem_mib as u64 / MIB_PER_SEC_FLOOR)
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
/// ADR 0048: per-host lock keyed by a base snapshot's `source_rootfs_canonical`
/// path. Restores that descend from the SAME base share this path (FC opens the
/// rootfs at the state.bin-embedded absolute path, and there's no load-time
/// rootfs override the way there is for vsock), so their
/// `restore_canonical_symlinks` → `load_snapshot` windows must not interleave on
/// one host. Distinct base paths (e.g. per-session resumes) get distinct keys
/// and never contend. The map only ever grows by the number of distinct base
/// paths a host serves (bounded by enabled images), so it isn't reaped.
fn source_canonical_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::LazyLock<DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> =
        std::sync::LazyLock::new(DashMap::new);
    LOCKS.entry(path.to_path_buf()).or_default().clone()
}

async fn restore_canonical_symlinks(
    work_dir: &Path,
    new_sandbox_id: SandboxId,
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
        // NOTE: the SHARED `source_rootfs_canonical` symlink (the state.bin-
        // embedded path, identical across every VM descended from one base) is
        // installed by `restore_in_jail` under a per-source-path lock held only
        // across `load_snapshot` — NOT here. Doing it here, off the lock, let
        // same-base concurrent restores race it (TOCTOU `EEXIST` 503 + cross-VM
        // repoint). This function only installs the per-NEW-sandbox `canonical`
        // above, which is unique and never contended.
    }
    // Vsock: nothing to recreate. The load passes `vsock_override`
    // keyed to the NEW live sandbox id, so FC never binds the
    // state.bin-embedded ancestor path. The old arm here unlinked
    // `source_vsock_canonical` pre-load — on a host where a same-image
    // sibling VM was LIVE on that shared path, that unlink stole its
    // socket (the 2026-06-11 cross-session misroute). Deliberately
    // gone; the per-sandbox path's stale-file unlink happens at the
    // load site in `restore_in_jail`.
    // ADR 0021 P1.5: no harness canonical symlink to restore — the
    // harness binary lives in the rootfs at the manifest-declared
    // `[harness] exec` path, so there's nothing for the host to
    // re-point.

    Ok(())
}

/// ADR 0044 K2: disarm-on-success SIGKILL backstop for the
/// create/restore window. We removed `kill_on_drop(true)` from the FC
/// and uffd-handler spawns so a live VM is decoupled from the
/// host-agent's process lifecycle (a host-agent restart *detaches* its
/// VMs — leaves them running — and the successor reattaches). But that
/// also means a create/restore which errors out *before* the sandbox
/// is fully live would otherwise leak a half-spawned process. This
/// guard restores that cleanup precisely: it SIGKILLs `pid` on drop
/// unless [`disarm`](Self::disarm)ed, so every `?` early-return between
/// spawn and the final `sandboxes.insert` reaps the process, while a
/// successful path disarms it and the VM keeps running. Pairs with the
/// still-held `Child` (now `kill_on_drop=false`) dropping at scope exit,
/// which reaps the just-killed zombie via tokio's orphan queue.
struct SpawnKillGuard {
    pid: u32,
    armed: bool,
}

impl SpawnKillGuard {
    fn new(pid: u32) -> Self {
        Self { pid, armed: true }
    }

    /// Defuse the guard once the spawned process is committed to a
    /// live sandbox. After this, dropping the guard is a no-op.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SpawnKillGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: `kill(pid, SIGKILL)` on a pid we just spawned is a
        // basic process-control syscall with no UB; the result is
        // best-effort cleanup, not relied on for any invariant.
        let rc = unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno != libc::ESRCH {
                tracing::warn!(
                    pid = self.pid,
                    errno,
                    "SpawnKillGuard: SIGKILL of half-spawned process failed; may linger"
                );
            }
        }
    }
}

/// Issue #197: drop-based per-VM netns cleanup, mirroring [`SpawnKillGuard`]
/// for the network half of a restore. ADR 0014 M1.16 provisions a per-VM
/// netns (veth pair, in-netns TAP, SNAT iptables rule, allocator `/30` slot)
/// BEFORE FC spawns, but it was only ever torn down in explicit `Err` arms
/// — never on `Drop`. A restore future cancelled in the spawn/load window
/// (the post-copy `restore_task.abort()` is the deterministic case), or the
/// post-load listener early-returns, would skip every one of those arms and
/// leak the netns + veth + SNAT rule + allocator slot.
///
/// Constructed right after `reserve_restored_netns` succeeds and held
/// through the rest of restore; `disarm()`ed at the commit point where the
/// `NetnsSetup` moves into the live sandbox (whose `destroy()` then owns
/// teardown). If dropped while armed it runs `net::teardown_netns` —
/// `Drop` can't `.await`, so (following the detached-task idiom this crate
/// already uses for async-from-Drop cleanup) it spawns the teardown on a
/// runtime handle captured at construction.
struct NetnsGuard {
    /// `None` when networking is disabled / the snapshot has no `net`
    /// record — then the guard is inert (nothing was provisioned).
    setup: Option<net::NetnsSetup>,
    allocator: Arc<parking_lot::Mutex<net::NetworkAllocator>>,
    handle: tokio::runtime::Handle,
    armed: bool,
}

impl NetnsGuard {
    fn new(
        setup: Option<net::NetnsSetup>,
        allocator: Arc<parking_lot::Mutex<net::NetworkAllocator>>,
    ) -> Self {
        Self {
            setup,
            allocator,
            handle: tokio::runtime::Handle::current(),
            armed: true,
        }
    }

    /// Defuse and yield the owned `NetnsSetup` at the commit point — the
    /// live sandbox (and its `destroy()`) now owns teardown. Consuming
    /// `self` here makes it impossible to keep an armed guard alive past
    /// commit by accident.
    fn into_committed(mut self) -> Option<net::NetnsSetup> {
        self.armed = false;
        self.setup.take()
    }
}

impl Drop for NetnsGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(setup) = self.setup.take() else {
            return; // nothing was provisioned
        };
        let allocator = Arc::clone(&self.allocator);
        // `teardown_netns` is async (shells `ip netns delete` + frees the
        // allocator slot under the same lock other restores contend on), so
        // we can't block here. Detach it on the captured runtime handle —
        // same pattern as the cancel-safe `destroy()` teardown.
        self.handle.spawn(async move {
            net::teardown_netns(&setup, &allocator).await;
            tracing::debug!(
                netns = %setup.netns_name,
                "NetnsGuard: tore down per-VM netns on cancelled/failed restore"
            );
        });
    }
}

/// Build a `ProcessRecord` (pid + `/proc` starttime + comm) for a live
/// pid. starttime/comm fall back to 0/"" if the pid is already gone — the
/// reattach pass's three-axis identity check then fails closed and
/// orphans the sandbox rather than re-adopting a recycled pid.
fn process_record(pid: u32) -> sandbox_manifest::ProcessRecord {
    sandbox_manifest::ProcessRecord {
        pid,
        start_time_jiffies: sandbox_manifest::read_proc_start_time_jiffies(pid).unwrap_or(0),
        comm: sandbox_manifest::read_proc_comm(pid).unwrap_or_default(),
    }
}

/// Write the per-sandbox `sandbox.json` so the startup reattach pass can
/// re-adopt this still-live VM after a host-agent restart (ADR 0044 K2).
/// Called at the end of BOTH `create()` and `restore()` — the only
/// difference is host-root `network` vs per-VM `netns`, and whether a
/// uffd handler is present. Best-effort: a write failure means this one
/// sandbox can't be reattached (it orphan-reaps + the session reconciles
/// instead), but the running VM itself is unaffected.
fn persist_sandbox_manifest(
    work_dir: &std::path::Path,
    sandbox_id: SandboxId,
    state: &SandboxState,
    fc_pid: u32,
    network: Option<sandbox_manifest::NetworkRecord>,
    netns: Option<sandbox_manifest::NetnsRecord>,
    uffd_pid: Option<u32>,
) {
    let m = sandbox_manifest::SandboxManifest {
        schema_version: sandbox_manifest::SCHEMA_VERSION,
        sandbox_id,
        backend: sandbox_manifest::BACKEND_FIRECRACKER.to_string(),
        spec: state.spec.clone(),
        firecracker: sandbox_manifest::FirecrackerProcessRecord {
            process: process_record(fc_pid),
            api_socket: state.firecracker_socket.clone(),
            vsock_uds_base: state.vsock_uds_path.clone(),
            rootfs_canonical: state.rootfs_canonical.clone(),
            vsock_cid: state.vsock_cid,
        },
        network,
        netns,
        uffd_handler: uffd_pid.map(process_record),
        migration_role: None,
    };
    let path = sandbox_manifest::manifest_path(work_dir, sandbox_id);
    if let Err(e) = sandbox_manifest::write_manifest(&path, &m) {
        tracing::warn!(
            %sandbox_id,
            error = %e,
            "sandbox manifest write failed; this sandbox can't be reattached across a \
             host-agent restart (it'll orphan-reap + the session reconciles). VM is fine."
        );
    }
}

/// ADR 0044 K2: move the VM's processes (FC + any uffd-handler) into a
/// node-level cgroup-v2 leaf `<parent>/<sandbox_id>/`, OUT of the
/// host-agent's own (pod) cgroup, so a pod-scope `cgroup.kill` on
/// teardown doesn't reap the microVM. `hostPID` alone is insufficient —
/// it shares the PID namespace, but the FC processes stay in the pod's
/// cgroup and die with it. Best-effort: returns the error so the caller
/// can log loudly, but it never aborts create/restore (the VM still
/// runs; it just won't survive a pod restart on this host).
#[cfg(target_os = "linux")]
fn escape_to_node_cgroup(
    parent: &Path,
    sandbox_id: SandboxId,
    pids: &[u32],
) -> std::io::Result<()> {
    let dir = parent.join(sandbox_id.to_string());
    std::fs::create_dir_all(&dir)?;
    let procs = dir.join("cgroup.procs");
    for &pid in pids {
        // cgroup v2: writing a pid to `cgroup.procs` migrates the whole
        // process (all its threads — FC's vCPU threads included) into
        // this cgroup. One pid per write.
        std::fs::write(&procs, pid.to_string())?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn escape_to_node_cgroup(
    _parent: &Path,
    _sandbox_id: SandboxId,
    _pids: &[u32],
) -> std::io::Result<()> {
    Ok(())
}

/// Place the VM's processes in the node cgroup when one is configured,
/// logging LOUDLY on failure — a configured-but-failing move means
/// detach-survival is silently broken on this host, which we want
/// visible rather than discovered at the next deploy.
fn place_vm_in_node_cgroup(parent: Option<&Path>, sandbox_id: SandboxId, pids: &[u32]) {
    let Some(parent) = parent else {
        return; // dev / tests: no pod scope to escape.
    };
    match escape_to_node_cgroup(parent, sandbox_id, pids) {
        Ok(()) => tracing::debug!(
            %sandbox_id,
            parent = %parent.display(),
            ?pids,
            "ADR 0044 K2: moved VM processes to node cgroup (detached from pod scope)"
        ),
        Err(e) => tracing::warn!(
            %sandbox_id,
            parent = %parent.display(),
            error = %e,
            "ADR 0044 K2: FAILED to move VM to a node cgroup; this VM will NOT survive a \
             host-agent pod restart (detach disabled for it). Requires a privileged \
             host-agent with /sys/fs/cgroup mounted rw."
        ),
    }
}

/// Remove the per-VM node cgroup leaf [`escape_to_node_cgroup`] created.
/// `rmdir` of a cgroup only succeeds once it's empty, so `destroy()`
/// calls this AFTER killing FC + the uffd handler. Best-effort.
fn remove_node_cgroup(parent: Option<&Path>, sandbox_id: SandboxId) {
    let Some(parent) = parent else {
        return;
    };
    let dir = parent.join(sandbox_id.to_string());
    if let Err(e) = std::fs::remove_dir(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::debug!(
                %sandbox_id,
                path = %dir.display(),
                error = %e,
                "ADR 0044 K2: could not remove per-VM node cgroup (non-empty or already gone)"
            );
        }
    }
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

/// ADR 0022 Option A: read `(Pss, Rss)` in bytes from one FC process's
/// `/proc/<pid>/smaps_rollup`. Returns `None` if the file is unreadable
/// (process exited mid-sample) or the fields are missing — the caller
/// skips that pid rather than failing the whole density sample. The
/// kernel pre-aggregates `smaps_rollup`, so this is a single small read,
/// not a walk of every VMA.
#[cfg(target_os = "linux")]
async fn read_smaps_rollup_pss_rss(pid: u32) -> Option<(u64, u64)> {
    let text = tokio::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .await
        .ok()?;
    // Each line is `Field:   <value> kB`. Convert kB → bytes.
    let field_kb = |name: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
    };
    Some((field_kb("Pss:")? * 1024, field_kb("Rss:")? * 1024))
}

/// ADR 0009 §4: per-VM process supervisor. Spawned at create/restore
/// time for each FC process (and the UFFD handler when present). Polls
/// `kill(pid, 0)` every 1s; when the pid disappears (`ESRCH`), prunes
/// the sandbox from the host's in-memory map so `backend.list()` no
/// longer reports a phantom. The reconcile pass (ADR 0009 §1-§3)
/// then catches the absence on the next heartbeat tick and flips the
/// owning session per the §3 policy.
///
/// Issue #198: a bare map-remove is NOT enough. A Uffd-mode sandbox is
/// watched by TWO supervisors over the SAME `sandbox_id` — one on
/// `fc_pid`, one on `uffd_pid`. When only ONE of the pair dies (e.g. the
/// uffd handler is OOM-killed — it's a prime OOM target since it holds
/// page caches), the surviving sibling keeps running: FC then
/// page-faults forever on any non-resident page (a dead handler can't
/// service the userfaultfd), or a surviving handler blocks in
/// `read_event()` after FC's mm is gone. Merely dropping the removed
/// `LiveSandbox` kills nothing (ADR 0044 K2 removed `kill_on_drop`), and
/// because the map entry is now gone, a later `destroy()` hits its
/// idempotent early-return and reports success — leaving a wedged
/// RAM-holding VM, a leaked allocator /30 + SNAT slot (→ `NetReserveFailed`
/// on future restores of the same snapshot), and a `sandbox.json` that
/// reattach re-adopts as live across a host-agent restart.
///
/// So when the prune actually removes the entry we hand the removed
/// value to `on_prune`, which runs the SAME full teardown `destroy()`
/// uses (`destroy_teardown`): SIGKILL the surviving sibling, free
/// net/netns + the allocator slot, drop the cgroup, unlink the canonical
/// symlinks + jail dir (incl. `sandbox.json`). The helper is
/// double-teardown tolerant (ESRCH, already-deleted netns are ignored)
/// because the supervisor races `destroy()` by design — only one of them
/// wins the `remove`, and only the winner runs teardown.
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
fn spawn_process_supervisor<V, F, Fut>(
    sandboxes: Arc<DashMap<SandboxId, V>>,
    sandbox_id: SandboxId,
    pid: u32,
    role: &'static str,
    on_prune: F,
) where
    V: Send + Sync + 'static,
    F: FnOnce(SandboxId, V) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
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
            if let Some((_, live)) = sandboxes.remove(&sandbox_id) {
                tracing::warn!(
                    %sandbox_id,
                    %pid,
                    role,
                    "host-side VM supervisor (ADR 0009 §4) pruned unexpectedly-dead sandbox; \
                     running full teardown to reap the surviving sibling + free host state (issue #198)"
                );
                // Issue #198: this is the ONLY owner of the removed
                // `LiveSandbox` now — run the same kill/free/cleanup
                // `destroy()` runs so the surviving sibling (FC when the
                // handler died; the handler when FC died), its net/netns
                // + allocator slot, cgroup, and jail dir (incl.
                // `sandbox.json`) are all reaped instead of leaked.
                on_prune(sandbox_id, live).await;
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

async fn upload_file_over_stream<S>(mut stream: S, file: WriteFileSpec) -> WriteFileResult
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let path = file.path.clone();
    let request = WireRequest::Upload {
        path: file.path,
        bytes: file.content,
        mode: file.mode,
    };
    if let Err(error) = write_msg(&mut stream, &request).await {
        return write_file_failure(path, format!("send Upload request: {error}"));
    }
    match read_msg::<_, WireResponse>(&mut stream).await {
        Ok(WireResponse::UploadOk) => WriteFileResult {
            path,
            ok: true,
            error: None,
        },
        Ok(WireResponse::Error { kind, message }) => {
            write_file_failure(path, format!("agentd rejected Upload ({kind}): {message}"))
        }
        Ok(other) => write_file_failure(path, format!("unexpected Upload response: {other:?}")),
        Err(error) => write_file_failure(path, format!("read Upload response: {error}")),
    }
}

fn write_file_failure(path: String, error: String) -> WriteFileResult {
    WriteFileResult {
        path,
        ok: false,
        error: Some(error),
    }
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

    async fn write_files(
        &self,
        id: SandboxId,
        files: Vec<WriteFileSpec>,
    ) -> Result<Vec<WriteFileResult>, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.vsock_uds_path.clone()
        };
        Self::write_files_via_fc_vsock(id, &vsock_uds_path, ENGRAM_AGENTD_PORT, files).await
    }

    /// ADR 0066: connect to the in-guest agentd relay listener on `port`
    /// (host→guest via the FC vsock CONNECT handshake, same primitive exec uses
    /// on 1024). Reuses `connect_fc_vsock`'s post-restore muxer-settle retry, so
    /// this is dial-ready cold and warm alike (the vsock UDS is a host-root
    /// path, not netns-scoped). The host-agent writes the `RelayConnect` header
    /// and splices from here.
    async fn open_guest_stream(
        &self,
        id: SandboxId,
        port: u32,
    ) -> Result<Option<HarnessByteStream>, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.vsock_uds_path.clone()
        };
        let conn = Self::connect_fc_vsock(&vsock_uds_path, port).await?;
        Ok(Some(Box::pin(conn)))
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        self.snapshot_with_type(id, client::SnapshotType::Full)
            .await
    }

    /// ADR 0028 Fix A: diff-flavored capture — same snapshot dir +
    /// sidecar + vmstate contract as `snapshot()`, but the memory
    /// artifact is `memory.diff` (sparse, dirty-pages-only; the KVM
    /// dirty bitmap resets on capture so successive calls chain).
    /// Requires `FirecrackerConfig::track_dirty_pages`.
    async fn snapshot_diff(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        self.snapshot_with_type(id, client::SnapshotType::Diff)
            .await
    }

    /// ADR 0028 Fix A: FC supports diff checkpoints exactly when KVM
    /// dirty tracking is armed. Gates the periodic checkpoint driver
    /// so non-dirty-tracking hosts never run it.
    fn supports_diff_checkpoints(&self) -> bool {
        self.config.track_dirty_pages
    }

    /// ADR 0045 C2 (E2B fold): the per-jail trace dump the spawn wires
    /// via `--trace-output`.
    fn working_set_trace_path(&self, id: SandboxId) -> Option<PathBuf> {
        Some(
            self.work_dir
                .join(id.to_string())
                .join(WORKING_SET_TRACE_FILE),
        )
    }

    /// ADR 0019 / telemetry restoration (#526): the per-jail prefault
    /// stats sibling of `working_set_trace_path`, written by the
    /// uffd-handler at the end of `prefault_from_trace` (or, for the
    /// no-trace case, from its own startup path).
    ///
    /// `None` unless a uffd-handler actually exists for this sandbox
    /// (`uffd_pid.is_some()` — populated for `RestoreMode::Uffd`
    /// restores AND pidfd-reattached Uffd sandboxes, ADR 0044 K2).
    /// `RestoreMode::File` restores (the default when there's no
    /// `chunk_store`, and the documented `ENGRAM_FC_RESTORE_MODE=file`
    /// fleet knob) never spawn a handler, so nothing can ever write
    /// this file — returning `Some` there would make every File-mode
    /// resume alarm as `stats_missing`. Trait contract: `None` = "this
    /// backend/sandbox has no per-sandbox prefault detector" (mirrors
    /// the VZ/Process default).
    fn prefault_stats_path(&self, id: SandboxId) -> Option<PathBuf> {
        let live = self.sandboxes.get(&id)?;
        live.uffd_pid?;
        Some(self.work_dir.join(id.to_string()).join(PREFAULT_STATS_FILE))
    }

    /// ADR 0045 C2: trait forwarding to the inherent composition (the
    /// pooled wrapper reaches these through `dyn SandboxBackend`).
    fn compose_live_sidecar(
        &self,
        id: SandboxId,
        memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
    ) -> Result<Vec<u8>, SandboxError> {
        FirecrackerBackend::compose_live_sidecar(self, id, memory_manifest)
    }

    async fn snapshot_vmstate_only_package(
        &self,
        id: SandboxId,
        sidecar_json: &[u8],
    ) -> Result<(SnapshotId, PathBuf), SandboxError> {
        FirecrackerBackend::snapshot_vmstate_only_package(self, id, sidecar_json).await
    }

    /// ADR 0045 C2: what the page server needs to read this paused
    /// guest's memory from outside. `None` unless this is a live FC
    /// sandbox on a substrate host (post-copy needs the base-file
    /// mapping to translate guest offsets to FC virtual addresses).
    fn post_copy_source_view(
        &self,
        id: SandboxId,
    ) -> Option<engram_core::traits::sandbox::PostCopySourceView> {
        let base_dir = self.config.uffd_base_dir.clone()?;
        let live = self.sandboxes.get(&id)?;
        let fc_pid = live.fc_pid?;
        Some(engram_core::traits::sandbox::PostCopySourceView {
            fc_pid,
            uffd_base_dir: base_dir,
        })
    }

    /// ADR 0045 C2 (destination): the peer-mode handler's control
    /// socket. The path is deterministic per jail; existence implies
    /// the handler was spawned in peer mode (it binds the listener
    /// before connecting out).
    fn post_copy_control_sock(&self, id: SandboxId) -> Option<PathBuf> {
        let path = self
            .work_dir
            .join(id.to_string())
            .join(UFFD_CONTROL_SOCK_FILE);
        path.exists().then_some(path)
    }

    /// ADR 0044 K2: the rootfs `path_on_host` this sandbox's FC has
    /// open — `/dev/nbdN` for a chunked rootfs. Survivor rehydrate
    /// uses it to RECONFIGURE the same device instead of attaching a
    /// fresh slot. Filtered to block-device paths so a file-backed
    /// rootfs answers `None`.
    fn rootfs_device(&self, id: SandboxId) -> Option<PathBuf> {
        let live = self.sandboxes.get(&id)?;
        let path = live.state.spec.rootfs_source.clone()?;
        path.starts_with("/dev").then_some(path)
    }

    /// ADR 0045 C2: rewrite the sandbox manifest with the post-copy
    /// role (atomic tmp+rename, same discipline as the original
    /// write). A missing manifest is an error — the role fence must
    /// not silently fail to persist for a reattachable sandbox.
    async fn set_manifest_migration_role(
        &self,
        id: SandboxId,
        role: Option<&str>,
    ) -> Result<(), SandboxError> {
        let path = sandbox_manifest::manifest_path(&self.work_dir, id);
        let mut m = sandbox_manifest::read_manifest(&path)
            .map_err(|e| SandboxError::Vm(format!("read sandbox manifest: {e}").into()))?;
        m.migration_role = role.map(|r| r.to_string());
        sandbox_manifest::write_manifest(&path, &m)
            .map_err(|e| SandboxError::Vm(format!("write sandbox manifest: {e}").into()))
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

    /// ADR 0088 addendum: inflate toward `target_mib`, polling
    /// `GET /balloon/statistics` until the guest's `actual_mib`
    /// stabilizes (two identical consecutive reads at/after a partial
    /// grant) or `deadline` elapses. Whatever the guest granted by
    /// then is accepted — partial inflation still elides partially. A
    /// VM without the device surfaces FC's 400 as `InvalidSpec` via
    /// `patch_balloon`, which callers treat as fail-open.
    async fn balloon_reclaim(
        &self,
        id: SandboxId,
        target_mib: u64,
        deadline: std::time::Duration,
    ) -> Result<u64, SandboxError> {
        let socket = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .state
            .firecracker_socket
            .clone();
        let api = FirecrackerClient::new(&socket);
        // Adversarial-review fix: the initial PATCH is the one place
        // "400 ⇒ no balloon device" is a sound, TYPED inference (the
        // request is fixed-shape) — surface it as InvalidSpec so
        // callers can distinguish "nothing to release" from "balloon
        // state unknown".
        api.patch_balloon(target_mib).await.map_err(|e| {
            if client::is_balloon_device_missing(&e) {
                SandboxError::InvalidSpec(format!("no balloon device in this VM: {e}"))
            } else {
                e
            }
        })?;
        let started = std::time::Instant::now();
        let mut last_actual = 0u64;
        let mut stable_reads = 0u32;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            // Adversarial-review fix: from here the inflate PATCH has
            // LANDED — any polling error must not strand the balloon at
            // the high target. Best-effort rollback before propagating;
            // the caller still owes a release-and-confirm on this path.
            let stats = match api.get_balloon_statistics().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %id,
                        error = %e,
                        "balloon statistics poll failed after inflate PATCH; rolling target back to 0",
                    );
                    if let Err(rollback) = api.patch_balloon(0).await {
                        tracing::warn!(sandbox_id = %id, error = %rollback, "balloon rollback PATCH failed too");
                    }
                    return Err(e);
                }
            };
            if stats.actual_mib >= target_mib {
                last_actual = stats.actual_mib;
                break;
            }
            if stats.actual_mib == last_actual && stats.actual_mib > 0 {
                stable_reads += 1;
                // Two identical sub-target reads = the guest has given
                // what it can; more waiting won't reclaim more.
                if stable_reads >= 2 {
                    break;
                }
            } else {
                stable_reads = 0;
                last_actual = stats.actual_mib;
            }
            if started.elapsed() >= deadline {
                break;
            }
        }
        tracing::info!(
            sandbox_id = %id,
            target_mib,
            actual_mib = last_actual,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "balloon reclaim settled",
        );
        Ok(last_actual)
    }

    /// Adversarial-review fix: a release is only a release once the
    /// GUEST has taken its pages back — the target PATCH is
    /// asynchronous (the reclaim loop above exists precisely because
    /// target ≠ actual). Deflate is fast (the driver just reclaims the
    /// ballooned pages), so the confirm loop is normally one or two
    /// polls; a guest that cannot deflate within the deadline is a
    /// guest we must not run a warm hook in, surfaced as an error.
    async fn balloon_release(&self, id: SandboxId) -> Result<(), SandboxError> {
        let socket = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .state
            .firecracker_socket
            .clone();
        let api = FirecrackerClient::new(&socket);
        api.patch_balloon(0).await.map_err(|e| {
            if client::is_balloon_device_missing(&e) {
                SandboxError::InvalidSpec(format!("no balloon device in this VM: {e}"))
            } else {
                e
            }
        })?;
        const DEFLATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
        let started = std::time::Instant::now();
        loop {
            let stats = api.get_balloon_statistics().await?;
            if stats.actual_mib == 0 {
                tracing::debug!(
                    sandbox_id = %id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "balloon deflate confirmed (actual_mib=0)",
                );
                return Ok(());
            }
            if started.elapsed() >= DEFLATE_DEADLINE {
                return Err(SandboxError::Snapshot(format!(
                    "balloon deflate did not complete within {DEFLATE_DEADLINE:?} \
                     (actual_mib={} still ballooned)",
                    stats.actual_mib,
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
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
        self.restore_with(metadata, /*swap_aux_to_current=*/ false, Vec::new())
            .await
    }

    async fn restore_fresh(
        &self,
        metadata: SnapshotMetadata,
        selected_mounts: Vec<AuxRoDrive>,
    ) -> Result<SandboxId, SandboxError> {
        // ADR 0035 §3 + 0055: fresh creates track the host's current bundle
        // generations AND patch the per-session selected skills into reserved
        // slots; both swaps happen load-paused inside restore_in_jail.
        self.restore_with(
            metadata,
            /*swap_aux_to_current=*/ true,
            selected_mounts,
        )
        .await
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // Idempotent: removing an unknown id is a no-op, matching the
        // contract ProcessBackend follows.
        let Some((_, live)) = self.sandboxes.remove(&id) else {
            return Ok(());
        };

        // ADR 0050 / issue #196: destroy MUST be cancel-safe. The map
        // entry is already gone (above), so once we own `live` the only
        // record of the running FC + uffd handler is on this stack. The
        // teardown that follows parks in cancellable awaits for several
        // seconds (graceful-shutdown timeout, uffd SIGTERM wait), and the
        // host-agent tonic handler runs destroy inline — so a wire-level
        // cancellation (client disconnect / RPC deadline) drops this
        // future. If that happened before the kills ran, FC + uffd kept
        // running (ADR 0044 K2 removed `kill_on_drop`, so dropping `live`
        // kills nothing) AND a retried destroy hit the idempotent
        // early-return above, reporting success while the VM lived on —
        // a leaked microVM, allocator /30 slot, TAP/netns, and a
        // `sandbox.json` that reattach re-adopts after a host-agent
        // restart.
        //
        // The fix: hand the entire teardown to a detached `tokio::spawn`
        // and merely *await* its `JoinHandle`. Wire cancellation now only
        // drops the awaiter; the spawned task runs the full
        // kill/free/cleanup sequence to completion regardless. We still
        // return the task's outcome to the caller so a retry that races a
        // still-running teardown sees a coherent result.
        let net_allocator = self.net_allocator.clone();
        let vm_cgroup_parent = self.config.vm_cgroup_parent.clone();
        let work_dir = self.work_dir.clone();
        let handle = tokio::spawn(destroy_teardown(
            id,
            live,
            net_allocator,
            vm_cgroup_parent,
            work_dir,
        ));
        match handle.await {
            Ok(()) => Ok(()),
            Err(join_err) => {
                // The detached task panicked. The map entry is already
                // gone and the kill/free best-effort steps each swallow
                // their own errors, so a panic here is unexpected — but
                // surface it rather than silently claim success.
                tracing::error!(
                    sandbox_id = %id,
                    error = %join_err,
                    "destroy teardown task panicked",
                );
                Err(SandboxError::Vm(
                    format!("destroy teardown task panicked: {join_err}").into(),
                ))
            }
        }
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.sandboxes.iter().map(|r| *r.key()).collect())
    }

    /// ADR 0068 probe-before-host_lost: overrides the trait default
    /// with an INDEPENDENT ground-truth check — the in-memory
    /// `self.sandboxes` map, or its heartbeat-carried mirror
    /// `running_sandboxes`, being wrong is exactly the desync
    /// `reconcile::flip_missing` uses this probe to catch, so
    /// `process_alive` must not be derived from that same map.
    /// Instead: read the persisted per-sandbox manifest
    /// (`sandbox_manifest::manifest_path`) and verify the same
    /// three-axis pid identity (pid + start-time-jiffies + comm) the
    /// survivor-reattach pass (`reattach_sandbox`) already trusts. A
    /// missing/malformed manifest (never written, or already deleted
    /// at the start of `destroy()`) reads as "unverifiable" — falls
    /// back to the in-memory signal, since there's nothing else to
    /// check against.
    async fn probe_sandbox(&self, id: SandboxId) -> Result<SandboxProbe, SandboxError> {
        let known_to_backend = self.sandboxes.contains_key(&id);
        let manifest_path = sandbox_manifest::manifest_path(&self.work_dir, id);
        let process_alive = match sandbox_manifest::read_manifest(&manifest_path) {
            Ok(manifest) => {
                let rec = &manifest.firecracker.process;
                sandbox_manifest::read_proc_start_time_jiffies(rec.pid)
                    == Some(rec.start_time_jiffies)
                    && sandbox_manifest::read_proc_comm(rec.pid).as_deref()
                        == Some(rec.comm.as_str())
            }
            // No manifest on disk (never written, or already deleted at
            // the start of `destroy()`) — nothing to independently
            // verify against; fall back to the in-memory signal.
            Err(_) => known_to_backend,
        };
        // ADR 0091: the control-plane leg — a connect() against the VMM
        // API socket. Refused/missing with the process alive is the
        // zombie-guest class (2026-07-11 campaign C1: a dead FC read
        // `active` for 16+ min). Only probed when we know the socket
        // path (in-memory entry); a reattach-window miss reads `None`,
        // never `Some(false)`.
        let control_alive = match self.sandboxes.get(&id) {
            Some(entry) => {
                let sock = entry.state.firecracker_socket.clone();
                drop(entry);
                Some(tokio::net::UnixStream::connect(&sock).await.is_ok())
            }
            None => None,
        };
        Ok(SandboxProbe {
            known_to_backend,
            process_alive,
            control_alive,
        })
    }

    /// The sandbox's guest-network identity. Mirrors the VZ pattern:
    /// derive from `net`/`netns` state when we have it (no I/O),
    /// falling back to an agentd vsock query + cache when we don't.
    ///
    /// - Warm-restored (`netns` is `Some`): `egress_identity` is the
    ///   netns SNAT pool slot (`ns.host_reachable_ip()`) — the value
    ///   the egress-proxy registry indexes against. `dial_ip` is the
    ///   bake-time in-VM eth0 address (`ns.vm_cidr.guest()`,
    ///   `10.200.0.2` by default) — every warm VM shares it because
    ///   each lives in its own netns; the SHELL tab (pre-ADR-0066)
    ///   entered the netns before dialing, so it resolved through the
    ///   TAP to the VM, not the netns's own veth IP. Returning the
    ///   SNAT slot as the dial target would miss the VM entirely
    ///   (prod-shape failure mode caught by e2e_shell_warm: `connect
    ///   10.200.0.6:7681: Connection refused`).
    /// - Cold-created (`net` is `Some`, no netns indirection):
    ///   `egress_identity` and `dial_ip` collapse to the same
    ///   per-sandbox `net.vm_cidr.guest()` — the TAP is in host root,
    ///   reachable directly. This is also the deterministic fast path
    ///   (no agentd dial): the /30 allocator already assigned this
    ///   value, so we skip a ~2s vsock RTT on every fresh-session
    ///   `notify_session_policy` call — critical because the
    ///   coord-side policy registration races the agent's boot.
    /// - Neither `net` nor `netns` (host networking disabled in
    ///   tests, or a restored legacy pre-M1.16-bake manifest with no
    ///   net record): fall back to an agentd `WireRequest::GuestIp`
    ///   vsock query, both IP fields set to the answer.
    async fn guest_endpoints(&self, id: SandboxId) -> Option<GuestEndpoints> {
        if let Some(live) = self.sandboxes.get(&id) {
            if let Some(ep) = live.guest_endpoints.lock().clone() {
                return Some(ep);
            }
            let vsock_uds = Some(live.state.vsock_uds_path.clone());
            if let Some(ns) = live.netns.as_ref() {
                let ep = GuestEndpoints {
                    egress_identity: ns.host_reachable_ip(),
                    dial_ip: ns.vm_cidr.guest(),
                    netns: Some(ns.netns_name.clone()),
                    vsock_uds,
                };
                *live.guest_endpoints.lock() = Some(ep.clone());
                return Some(ep);
            }
            if let Some(net) = live.net.as_ref() {
                let ip = net.vm_cidr.guest();
                let ep = GuestEndpoints {
                    egress_identity: ip,
                    dial_ip: ip,
                    netns: None,
                    vsock_uds,
                };
                *live.guest_endpoints.lock() = Some(ep.clone());
                return Some(ep);
            }
        }
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id)?;
            live.state.vsock_uds_path.clone()
        };
        let vsock_uds = Some(vsock_uds_path.clone());
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
        let ip_str: Option<String> = tokio::time::timeout(Duration::from_secs(2), fut)
            .await
            .ok()
            .flatten();
        let ip: std::net::Ipv4Addr = ip_str?.parse().ok()?;
        let ep = GuestEndpoints {
            egress_identity: ip,
            dial_ip: ip,
            netns: None,
            vsock_uds,
        };
        if let Some(live) = self.sandboxes.get(&id) {
            *live.guest_endpoints.lock() = Some(ep.clone());
        }
        Some(ep)
    }

    /// ADR 0080: one `RefreshAgent` round against the restored guest's
    /// agentd, then — when it re-execs — a bounded `Ping` re-poll until
    /// the new agentd answers. Fresh-create restores only (the caller,
    /// `PooledBackend::restore_base_for_session`, treats every error
    /// here as non-fatal: a degraded refresh keeps the captured agentd,
    /// never fails the create).
    ///
    /// Two shapes of "it's restarting" are equivalent: the typed
    /// `AgentRefreshed { restarting: true }` reply, and a connection
    /// error after the request was sent (the reply raced the exec's
    /// connection teardown) — both fall through to the re-poll. A typed
    /// `Error` reply is the old-agentd skew shape (pre-ADR-0080 bake):
    /// surfaced as an error for the caller to log-and-proceed.
    async fn refresh_agent(&self, id: SandboxId) -> Result<AgentRefresh, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or_else(|| {
                SandboxError::Vm(format!("refresh_agent: no live sandbox {id}").into())
            })?;
            // Latency guard: the restore recorded whether the agentd
            // slot's content actually changed vs. the snapshot's pin.
            // Unchanged (the steady state) ⇒ same binary bytes ⇒ nothing
            // to adopt — skip the in-guest round entirely, adding ZERO to
            // the create path. Only the first creates after an agentd
            // roll pay the RPC + re-exec.
            if !live.agentd_slot_swapped {
                return Ok(AgentRefresh::UpToDate);
            }
            live.state.vsock_uds_path.clone()
        };

        let round = async {
            let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                .await
                .map_err(|e| {
                    SandboxError::Vm(format!("refresh_agent: vsock connect: {e}").into())
                })?;
            engram_agentd::write_msg(&mut conn, &WireRequest::RefreshAgent)
                .await
                .map_err(|e| SandboxError::Vm(format!("refresh_agent: send: {e}").into()))?;
            let resp: Result<engram_agentd::WireResponse, _> =
                engram_agentd::read_msg(&mut conn).await;
            match resp {
                Ok(engram_agentd::WireResponse::AgentRefreshed { restarting, sha256 }) => {
                    tracing::debug!(sandbox_id = %id, restarting, ?sha256, "RefreshAgent reply");
                    Ok(restarting)
                }
                Ok(engram_agentd::WireResponse::Error { kind, message }) => Err(SandboxError::Vm(
                    format!("refresh_agent: agentd error ({kind}): {message}").into(),
                )),
                Ok(other) => Err(SandboxError::Vm(
                    format!("refresh_agent: unexpected response: {other:?}").into(),
                )),
                // The reply can race the re-exec's connection teardown:
                // the request landed, the exec closed the CLOEXEC'd
                // socket before (or while) the reply flushed. Treat as
                // restarting and let the re-poll below arbitrate.
                Err(e) => {
                    tracing::debug!(
                        sandbox_id = %id,
                        error = %e,
                        "RefreshAgent reply lost; assuming re-exec and re-polling",
                    );
                    Ok(true)
                }
            }
        };
        let restarting = tokio::time::timeout(Duration::from_secs(10), round)
            .await
            .map_err(|_| SandboxError::Vm("refresh_agent: timed out".into()))??;
        if !restarting {
            return Ok(AgentRefresh::UpToDate);
        }

        // Re-exec in flight: poll until the new agentd's listener
        // answers a Ping. ~10 ms typical (copy+exec of a small static
        // binary), so poll tight — this sits on the session-create
        // path. The 10 s ceiling is pure paranoia.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let probe = async {
                let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                    .await
                    .ok()?;
                engram_agentd::write_msg(&mut conn, &WireRequest::Ping)
                    .await
                    .ok()?;
                match engram_agentd::read_msg(&mut conn).await {
                    Ok(engram_agentd::WireResponse::Pong) => Some(()),
                    _ => None,
                }
            };
            match tokio::time::timeout(Duration::from_millis(1000), probe).await {
                Ok(Some(())) => {
                    tracing::info!(sandbox_id = %id, "agentd re-exec'd onto the staged bundle generation");
                    return Ok(AgentRefresh::Restarted);
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(SandboxError::Vm(
                        "refresh_agent: agentd never answered after re-exec".into(),
                    ));
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
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
            let resp: engram_agentd::WireResponse =
                engram_agentd::read_msg(&mut conn).await.map_err(|e| {
                    SandboxError::Vm(
                        format!(
                            "start_shell: recv: {}",
                            describe_agentd_rpc_recv_failure(&e)
                        )
                        .into(),
                    )
                })?;
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

    /// ADR 0065: ask agentd to ensure the in-guest browser stack (Xvfb +
    /// openbox + chromium + x11vnc) is running and x11vnc is accepting on its
    /// port. Returns the bound port plus agentd's optional chromium-CDP
    /// liveness warning (issue #569). Mirrors [`Self::start_shell`] — the
    /// host's `proxy_vnc` (P1.4) calls this just before dialing the guest's
    /// raw-TCP VNC port, so the connect finds a listener.
    async fn start_browser(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or_else(|| {
                SandboxError::Vm(format!("start_browser: no live sandbox {id}").into())
            })?;
            live.state.vsock_uds_path.clone()
        };
        let fut = async {
            let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                .await
                .map_err(|e| {
                    SandboxError::Vm(format!("start_browser: vsock connect: {e}").into())
                })?;
            engram_agentd::write_msg(&mut conn, &WireRequest::StartBrowser { port: None })
                .await
                .map_err(|e| SandboxError::Vm(format!("start_browser: send: {e}").into()))?;
            let resp: engram_agentd::WireResponse =
                engram_agentd::read_msg(&mut conn).await.map_err(|e| {
                    SandboxError::Vm(
                        format!(
                            "start_browser: recv: {}",
                            describe_agentd_rpc_recv_failure(&e)
                        )
                        .into(),
                    )
                })?;
            match resp {
                engram_agentd::WireResponse::BrowserReady {
                    port,
                    spawned,
                    cdp_warning,
                } => {
                    tracing::info!(
                        sandbox_id = %id,
                        port,
                        spawned,
                        cdp_warning = ?cdp_warning,
                        "agentd reports browser ready",
                    );
                    Ok(engram_core::traits::sandbox::BrowserStart {
                        port,
                        warning: cdp_warning,
                    })
                }
                engram_agentd::WireResponse::Error { kind, message } => Err(SandboxError::Vm(
                    format!("start_browser: agentd error ({kind}): {message}").into(),
                )),
                other => Err(SandboxError::Vm(
                    format!("start_browser: unexpected response: {other:?}").into(),
                )),
            }
        };
        // Worst-case serial path inside agentd's start_browser post issue
        // #569's mutex-hold/CDP-probe fixes: up to a 2s RFB-banner-read
        // timeout (issue #567's wedge detection) + ~0.3s force-stop grace +
        // up to 20s (READY_DEADLINE) for the launcher's `--ensure` to bring
        // x11vnc up + up to a 1s fast CDP probe — call it ~24s worst case.
        // The fresh-spawn CDP watch itself runs off-path in a detached
        // background task and never blocks this reply. 30s of headroom here
        // still comfortably covers it.
        tokio::time::timeout(Duration::from_secs(30), fut)
            .await
            .map_err(|_| SandboxError::Vm("start_browser: timed out waiting for agentd".into()))?
    }

    /// ADR 0065: tear down the in-guest browser stack. Idempotent — a no-op
    /// when the sandbox is gone or nothing is running. Mirrors
    /// [`Self::start_shell`]'s connection pattern.
    async fn stop_browser(&self, id: SandboxId) -> Result<(), SandboxError> {
        let vsock_uds_path = {
            let Some(live) = self.sandboxes.get(&id) else {
                return Ok(());
            };
            live.state.vsock_uds_path.clone()
        };
        let fut = async {
            let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                .await
                .map_err(|e| {
                    SandboxError::Vm(format!("stop_browser: vsock connect: {e}").into())
                })?;
            engram_agentd::write_msg(&mut conn, &WireRequest::StopBrowser)
                .await
                .map_err(|e| SandboxError::Vm(format!("stop_browser: send: {e}").into()))?;
            let _: engram_agentd::WireResponse =
                engram_agentd::read_msg(&mut conn).await.map_err(|e| {
                    SandboxError::Vm(
                        format!(
                            "stop_browser: recv: {}",
                            describe_agentd_rpc_recv_failure(&e)
                        )
                        .into(),
                    )
                })?;
            Ok(())
        };
        // A wedged guest must never hang teardown — bound the round-trip at
        // 15s for parity with VZ's `stop_browser` (start_browser allows 30s
        // because it cold-starts Xvfb+chromium; teardown is far cheaper).
        tokio::time::timeout(Duration::from_secs(15), fut)
            .await
            .map_err(|_| SandboxError::Vm("stop_browser: timed out waiting for agentd".into()))?
    }

    /// ADR 0085: ask agentd to ensure the in-guest IDE (code-server) is
    /// running and answering `/healthz` on its loopback HTTP port. Mirrors
    /// [`Self::start_browser`] — the coordinator's `ensure_ide` calls this
    /// just before the orchestrator relays to the guest's port, so the dial
    /// finds a server.
    async fn start_ide(&self, id: SandboxId) -> Result<u16, SandboxError> {
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or_else(|| {
                SandboxError::Vm(format!("start_ide: no live sandbox {id}").into())
            })?;
            live.state.vsock_uds_path.clone()
        };
        let fut = async {
            let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                .await
                .map_err(|e| SandboxError::Vm(format!("start_ide: vsock connect: {e}").into()))?;
            engram_agentd::write_msg(&mut conn, &WireRequest::StartIde { port: None })
                .await
                .map_err(|e| SandboxError::Vm(format!("start_ide: send: {e}").into()))?;
            let resp: engram_agentd::WireResponse =
                engram_agentd::read_msg(&mut conn).await.map_err(|e| {
                    SandboxError::Vm(
                        format!("start_ide: recv: {}", describe_agentd_rpc_recv_failure(&e)).into(),
                    )
                })?;
            match resp {
                engram_agentd::WireResponse::IdeReady { port, spawned } => {
                    tracing::info!(
                        sandbox_id = %id,
                        port,
                        spawned,
                        "agentd reports ide ready",
                    );
                    Ok(port)
                }
                engram_agentd::WireResponse::Error { kind, message } => Err(SandboxError::Vm(
                    format!("start_ide: agentd error ({kind}): {message}").into(),
                )),
                other => Err(SandboxError::Vm(
                    format!("start_ide: unexpected response: {other:?}").into(),
                )),
            }
        };
        // Worst-case serial path inside agentd's start_ide: up to a 2s
        // /healthz probe timeout (issue #567's wedge detection) + ~0.3s
        // force-stop grace + up to 20s (READY_DEADLINE) for the launcher's
        // `--ensure` to bring code-server up — call it ~23s worst case. 30s
        // of headroom here still comfortably covers it (browser parity).
        tokio::time::timeout(Duration::from_secs(30), fut)
            .await
            .map_err(|_| SandboxError::Vm("start_ide: timed out waiting for agentd".into()))?
    }

    /// ADR 0085: tear down the in-guest IDE. Idempotent — a no-op when the
    /// sandbox is gone or nothing is running. Mirrors
    /// [`Self::stop_browser`]'s connection pattern.
    async fn stop_ide(&self, id: SandboxId) -> Result<(), SandboxError> {
        let vsock_uds_path = {
            let Some(live) = self.sandboxes.get(&id) else {
                return Ok(());
            };
            live.state.vsock_uds_path.clone()
        };
        let fut = async {
            let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT)
                .await
                .map_err(|e| SandboxError::Vm(format!("stop_ide: vsock connect: {e}").into()))?;
            engram_agentd::write_msg(&mut conn, &WireRequest::StopIde)
                .await
                .map_err(|e| SandboxError::Vm(format!("stop_ide: send: {e}").into()))?;
            let _: engram_agentd::WireResponse =
                engram_agentd::read_msg(&mut conn).await.map_err(|e| {
                    SandboxError::Vm(
                        format!("stop_ide: recv: {}", describe_agentd_rpc_recv_failure(&e)).into(),
                    )
                })?;
            Ok(())
        };
        // A wedged guest must never hang teardown — bound the round-trip at
        // 15s (stop_browser parity; teardown is far cheaper than start).
        tokio::time::timeout(Duration::from_secs(15), fut)
            .await
            .map_err(|_| SandboxError::Vm("stop_ide: timed out waiting for agentd".into()))?
    }

    // ADR 0021 P1.5: `swap_harness_drive` retired with the rest of
    // option-D. The harness lives in the rootfs now, so there's no
    // host file backing a virtio-blk drive to swap.

    fn bundle_dir(&self) -> &std::path::Path {
        // The one dir `read_bundle_stamp` + `resolve_aux_drive` read from, so
        // the host-agent heartbeat reports exactly what restore will attach.
        &self.config.bundle_dir
    }

    fn restore_memory_is_lazy_for(&self, fresh: bool) -> bool {
        // ADR 0022 Option A: mirror `effective_restore_mode` exactly so
        // the materialize decision can't drift from the load decision —
        // base-create under File materializes the (shared) memfile;
        // resume under UFFD stays lazy.
        matches!(self.effective_restore_mode(fresh), RestoreMode::Uffd)
    }

    async fn guest_memory_stats(&self) -> Option<engram_core::traits::sandbox::GuestMemoryStats> {
        // ADR 0022 Option A: sum PSS/RSS over every live FC process from
        // `/proc/<pid>/smaps_rollup`. PSS divides shared clean pages by
        // their mapcount, so Σpss/Σrss across same-template File-backend
        // siblings is the density ratio. Error-tolerant: a vanished or
        // unreadable pid is skipped, never fatal.
        //
        // Issue #540: bucket the sum by the per-sandbox `parked` flag so
        // the RAM ledger can add back only reservation-backed (non-parked)
        // residents into `allocatable_mib`. `parked` is always `false`
        // today (no backend transition sets it yet), so this is a
        // behavior-preserving split until epic-parking-ladder lands.
        #[cfg(target_os = "linux")]
        {
            let pids: Vec<(u32, bool)> = self
                .sandboxes
                .iter()
                .filter_map(|e| e.value().fc_pid.map(|pid| (pid, e.value().parked)))
                .collect();
            let mut stats = engram_core::traits::sandbox::GuestMemoryStats::default();
            for (pid, parked) in pids {
                if let Some((pss, rss)) = read_smaps_rollup_pss_rss(pid).await {
                    if parked {
                        stats.parked_pss_bytes += pss;
                        stats.parked_rss_bytes += rss;
                        stats.parked_sampled += 1;
                    } else {
                        stats.pss_bytes += pss;
                        stats.rss_bytes += rss;
                        stats.sampled += 1;
                    }
                }
            }
            Some(stats)
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
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
        let t_ready = phase_start.elapsed().as_millis() as u64;
        let vsock_uds_path = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.vsock_uds_path.clone()
        };

        // ADR 0021 P1.4: no harness drive — argv points at a path
        // inside the rootfs (the image manifest's `[harness] exec`).
        // The `harness_substrate` / `harness_pack_uri` plumbing on
        // SandboxSpec stays (always-None on the new coord path)
        // until P1.5 retires it together with option-D.
        //
        // 2026-07 core-ops fold: this single frame also carries the
        // per-host egress-proxy CA (ADR 0021 P1). agentd installs it
        // (if present) before spawning, so there is exactly ONE
        // host→guest first-contact RPC per `start_agent` instead of
        // two — the CA-specific retry ladder that used to precede
        // this call is gone; its bounded first-contact retry moves
        // to wrap this round trip instead (below). Idempotent —
        // agentd's `last_pem` cache makes a resume-with-same-cert a
        // zero-I/O hot path, and an empty/`None` PEM is a no-op.
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

        // Harness spawn: connect to agentd-1024 and round-trip SpawnHarness.
        //
        // This is now the ONLY host→guest first-contact RPC `start_agent`
        // makes, so it inherits the bounded EOF-retry the CA-install verb
        // used to have — not as a CA-specific bandage, but as a first-
        // contact guard against the FC vsock-muxer settle race. Restored
        // sandboxes pre-set `agent_ready` to `true` (`restore_in_jail`),
        // so `wait_agent_ready` above returns instantly, but FC's vsock
        // muxer has a brief window post-`load_snapshot` where it accepts a
        // host `CONNECT`/returns `OK`, then closes the connection before
        // agentd's accept-loop task wakes and drains the request bytes —
        // surfacing as `read SpawnHarness response: early eof` 3-5 ms
        // after the `CONNECT/OK` round trip. Cold paths don't hit this:
        // the agentd dial of `AgentReady` gates `wait_agent_ready`, and by
        // the time `AgentReady` fires the muxer has settled.
        //
        // SpawnHarness is safe to retry: `HarnessSupervisor::spawn`
        // serialises on a mutex and reattaches a still-live previous
        // child (SIGUSR1 reconnect nudge) rather than killing it, so a
        // lost-but-actually-delivered attempt just gets reattached by the
        // retry, not double-spawned. Each attempt is a fresh connect,
        // fresh write, fresh read, so double-send is structurally
        // impossible to distinguish from — and structurally harmless
        // either way.
        //
        // The round trip is bounded — per-attempt AND in total. The
        // earlier shape carried NO deadline, keyed on a prod 299 s
        // handshake that eventually succeeded under guest CPU/IO
        // starvation during UFFD/NBD page-in. That reasoning let a single
        // wedged exchange pin the caller unbounded: prod 2026-07-17
        // (session 03e6535e) resumed a VM onto a dead rootfs device,
        // agentd never answered the SpawnHarness read, and the
        // coordinator's resume `finish` step hung for 34 minutes —
        // bounded only by a deploy rolling the pod. Because SpawnHarness
        // is reattach-idempotent (above), a timeout is just another
        // retryable shape: the retry reconnects and `HarnessSupervisor::
        // spawn` reattaches whatever the earlier attempt actually
        // started. A genuinely starved guest that needs longer than the
        // per-attempt budget now fails THIS attempt and is retried (fresh
        // connect), and — once the guest is responsive — a later attempt
        // reattaches in milliseconds. Slow success degrades to
        // success-on-retry; a wedge stops being an unbounded hang.
        // Starvation itself is still a separate concern
        // (prefault-admission-control) to fix at the source.
        //
        // The true fix for the underlying muxer race lives in the
        // vendored FC fork (`third_party/firecracker`), not here — this
        // retry only bounds the blast radius of a race we can't close
        // from the host side (see `connect_fc_vsock`'s doc comment and
        // `harness_supervisor.rs`'s SIGUSR1 comment for why agentd can't
        // signal its own resume over an already-held vsock connection).
        //
        // Budgets: per-attempt 60 s bounds the single hung read (a
        // healthy reattach answers in milliseconds; only a starved first
        // spawn approaches it); total 210 s is a HARD cap on the retry
        // ladder, under the coordinator's 240 s `start_agent` `grpc-timeout`
        // (`restore_rpc_timeout`) so the host returns a typed error before
        // the caller cancels the RPC out from under it. Each attempt is
        // wrapped in `min(SPAWN_ATTEMPT_TIMEOUT, remaining_budget)` — a
        // fixed per-attempt cap alone would let a 4th attempt begun near
        // 180 s run to ~240 s and race the gRPC deadline (adversarial-review
        // finding); the `min` keeps the WALL-CLOCK total ≤ 210 s.
        const SPAWN_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
        const SPAWN_TOTAL_BUDGET: Duration = Duration::from_secs(210);
        let t_total = std::time::Instant::now();
        let max_attempts: u32 = 5;
        let mut attempt: u32 = 0;
        let resp: engram_agentd::WireResponse = loop {
            attempt += 1;
            let span = tracing::info_span!("fc.spawn_harness", attempt);
            // Per-attempt, not per-round-trip: reset at the top of each
            // iteration so `connect_ms` below measures THIS attempt's
            // CONNECT leg only. A single Instant hoisted above the loop
            // would accumulate prior attempts' full connect+write+read
            // plus their 50 ms backoff sleeps into later attempts'
            // `connect_ms`, corrupting the sub-leg split the ADR 0045 C1
            // diagnosis below relies on (a slow CONNECT reading as a
            // starved guest that was actually just a late retry).
            let t_attempt = std::time::Instant::now();
            // Cap this attempt at whatever remains of the total budget, so
            // the loop's wall-clock can never exceed SPAWN_TOTAL_BUDGET
            // (and thus stays under the caller's gRPC deadline). A hung
            // attempt begun late no longer overshoots.
            let remaining = SPAWN_TOTAL_BUDGET.saturating_sub(t_total.elapsed());
            if remaining.is_zero() {
                return Err(SandboxError::Vm(
                    format!(
                        "SpawnHarness exhausted its {SPAWN_TOTAL_BUDGET:?} total budget after \
                         {} attempt(s)",
                        attempt - 1,
                    )
                    .into(),
                ));
            }
            let attempt_timeout = remaining.min(SPAWN_ATTEMPT_TIMEOUT);
            let inner = async {
                let mut conn = Self::connect_fc_vsock(&vsock_uds_path, ENGRAM_AGENTD_PORT).await?;
                // ADR 0045 C1 tail diagnosis: split the handshake into
                // host-visible sub-legs — a slow CONNECT means the guest
                // isn't accepting (vCPUs starved / vsock not settled); a
                // slow round-trip after a fast CONNECT means agentd
                // itself is stuck past accept.
                tracing::info!(
                    sandbox_id = %id,
                    attempt,
                    connect_ms = t_attempt.elapsed().as_millis() as u64,
                    "spawn-harness vsock connected",
                );
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
            };
            let outcome = match tokio::time::timeout(
                attempt_timeout,
                tracing::Instrument::instrument(inner, span),
            )
            .await
            {
                Ok(r) => r,
                // A per-attempt deadline expiry reuses the retry ladder
                // below (reattach-idempotent), distinguished by its
                // message so `retryable` can admit it.
                Err(_) => Err(SandboxError::Vm(
                    format!(
                        "SpawnHarness round-trip exceeded the per-attempt deadline \
                         ({attempt_timeout:?})"
                    )
                    .into(),
                )),
            };
            match outcome {
                Ok(r) => break r,
                Err(e) => {
                    // Retry EOF/RST-shaped errors — the muxer-settle
                    // signature — plus a per-attempt deadline expiry (a
                    // wedged/starved read; the retry reconnects and
                    // reattaches). Other failures (write failures,
                    // bincode decode errors, protocol mismatches,
                    // connection refused) are structural — retrying won't
                    // help, and `Connection refused` in particular stays
                    // a non-retryable, terminal class (the FC process
                    // itself isn't accepting; out of scope for this
                    // fold).
                    let msg = format!("{e}");
                    let retryable = msg.contains("early eof")
                        || msg.contains("unexpected end of file")
                        || msg.contains("connection reset")
                        || msg.contains("broken pipe")
                        || msg.contains("per-attempt deadline");
                    // Total-budget exhaustion is enforced at the top of the
                    // loop (`remaining.is_zero()`) plus the per-attempt
                    // `min` cap; here we only gate on retryability + the
                    // attempt count.
                    if !retryable || attempt >= max_attempts {
                        return Err(SandboxError::Vm(
                            format!(
                                "SpawnHarness failed after {attempt} attempt(s) \
                                 ({:?} elapsed): {e}",
                                t_total.elapsed(),
                            )
                            .into(),
                        ));
                    }
                    tracing::debug!(
                        sandbox_id = %id,
                        attempt,
                        error = %e,
                        "SpawnHarness transient failure; retrying after 50 ms (vsock muxer settle)",
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        };
        match resp {
            engram_agentd::WireResponse::HarnessSpawned { pid, ca_changed } => {
                let elapsed = phase_start.elapsed().as_secs_f64();
                // `ca_changed` is only authoritative at `attempt == 1`.
                // agentd's `last_pem` cache is keyed on the PEM content,
                // not on this round trip: if attempt 1's request landed
                // and installed a genuinely new CA (changed=true) but the
                // *response* was lost to a retryable error (connection
                // reset / broken pipe — not just the early-eof muxer-
                // settle race), attempt 2 resends the identical PEM, hits
                // the now-warm cache, and reports changed=false — masking
                // the ADR 0045 C1 cross-host-resume signal on exactly the
                // retried-restore path. Treat `ca_changed=false` with
                // `attempt > 1` as "unknown", not "no rotation happened".
                tracing::info!(
                    sandbox_id = %id,
                    elapsed_ms = (elapsed * 1000.0) as u64,
                    wait_ready_ms = t_ready,
                    pid = ?pid,
                    ca_changed = ?ca_changed,
                    attempt,
                    "fc agent handshake complete",
                );
                // Slow-handshake forensics without a jail shell: surface
                // the uffd handler's own log tail into the host-agent's
                // (pod-visible) logs. The handler runs detached with its
                // stderr in the jail — invisible exactly when we need to
                // see whether the eager sweep / fault loop was the stall.
                if elapsed > 5.0 {
                    let log_path = self.work_dir.join(id.to_string()).join("uffd-handler.log");
                    if let Ok(contents) = std::fs::read_to_string(&log_path) {
                        let tail: Vec<&str> = contents.lines().rev().take(30).collect();
                        for line in tail.iter().rev() {
                            tracing::info!(sandbox_id = %id, "uffd-handler.log| {line}");
                        }
                    }
                }
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

    fn set_forge_sink(&self, sink: engram_core::traits::ForgeSink) {
        *self.forge_sink.write() = Some(sink);
    }

    fn set_upload_sink(&self, sink: engram_core::traits::UploadSink) {
        *self.upload_sink.write() = Some(sink);
    }
}

/// The cancel-safe teardown body for
/// [`FirecrackerBackend::destroy`].
///
/// `destroy` removes the map entry, then hands the owned [`LiveSandbox`]
/// to this function inside a detached `tokio::spawn` and merely *awaits*
/// the `JoinHandle`. That structure is what makes destroy cancel-safe
/// (issue #196): the host-agent tonic handler runs `destroy` inline, so a
/// wire-level cancellation (client disconnect / RPC deadline) drops the
/// `destroy` future — but that only drops the *awaiter*. This spawned
/// task runs to completion regardless, so the FC process, the uffd
/// handler, the per-VM cgroup, the TAP/netns + allocator `/30` slot, and
/// the jail dir are always reaped. Without this, a cancelled destroy left
/// a running microVM AND a retried destroy hit the idempotent
/// early-return and falsely reported success.
///
/// Sequence: graceful shutdown → SIGKILL of FC → SIGTERM/SIGKILL of the
/// uffd handler → cgroup rmdir → TAP/netns teardown + allocator `free()`
/// → canonical-entry + jail-dir removal. Each step is best-effort and
/// logs rather than aborting the rest.
///
/// Belt-and-braces: FC and the uffd pid are armed with [`SpawnKillGuard`]s
/// for the kill window, so even if THIS task is itself aborted mid-kill
/// (e.g. runtime shutdown during a host-agent stop) the guards SIGKILL the
/// processes on drop. The guards are disarmed once the respective kill is
/// confirmed. (The guards alone don't free netns/allocator/jail — that's
/// why the detached-task structure is still required.)
async fn destroy_teardown(
    id: SandboxId,
    mut live: LiveSandbox,
    net_allocator: Arc<parking_lot::Mutex<net::NetworkAllocator>>,
    vm_cgroup_parent: Option<PathBuf>,
    work_dir: PathBuf,
) {
    // Belt-and-braces kill backstop (issue #196): arm a SIGKILL guard for
    // the FC and uffd pids up front. If this task is itself aborted before
    // the kills complete, dropping these guards SIGKILLs the processes —
    // so no path between here and the confirmed kills can leave FC/uffd
    // alive. Disarmed once the respective kill is done.
    let mut fc_guard = live.fc_pid.map(SpawnKillGuard::new);
    let mut uffd_guard = live
        .uffd_handler
        .as_ref()
        .and_then(|h| h.id())
        .or(live.uffd_pid)
        .map(SpawnKillGuard::new);

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
    // FC is confirmed gone — defuse its kill guard.
    if let Some(g) = fc_guard.as_mut() {
        g.disarm();
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
    // We own a `Child` for a handler spawned in THIS host-agent
    // generation; for a pidfd-reattached one we only have the pid
    // (`uffd_pid`, ADR 0044 K2). Either way: SIGTERM, then escalate to
    // SIGKILL if it doesn't exit promptly.
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
    } else if let Some(pid) = live.uffd_pid {
        #[cfg(unix)]
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
        if wait_for_pid_death(pid, Duration::from_secs(2))
            .await
            .is_err()
        {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            let _ = wait_for_pid_death(pid, Duration::from_secs(5)).await;
        }
    }
    // UFFD handler is confirmed gone (or there was none) — defuse its
    // kill guard.
    if let Some(g) = uffd_guard.as_mut() {
        g.disarm();
    }

    // ADR 0044 K2: FC + the uffd handler are gone, so the per-VM node
    // cgroup is now empty — remove it (rmdir of a cgroup only succeeds
    // once empty). No-op when no `vm_cgroup_parent` is configured.
    remove_node_cgroup(vm_cgroup_parent.as_deref(), id);

    // Tear down per-VM networking: yank iptables rules tagged
    // with this sandbox's chain comment, delete the TAP, return
    // the /30 to the allocator. Best-effort — each step logs
    // but doesn't stop the rest.
    if let Some(net_setup) = live.net.as_ref() {
        net::teardown(net_setup, &net_allocator).await;
    }
    // ADR 0014 M1.16: warm-restored VMs ran in their own netns
    // — delete it (and the TAP + veth-B inside, the SNAT iptables
    // rule, etc.), unlink the host-side veth-A, free the SNAT
    // pool slot. Best-effort like cold teardown.
    if let Some(netns_setup) = live.netns.as_ref() {
        net::teardown_netns(netns_setup, &net_allocator).await;
    }

    // Do NOT unlink the base vsock UDS file at
    // `live.state.vsock_uds_path` on destroy. Historically (ADR
    // 0018 cross-host evac safety) this guarded the
    // shared-filesystem race where a receiver's `load_snapshot`
    // re-bound the SAME canonical path the source's destroy was
    // unlinking. Since the vsock re-key the path is per-sandbox
    // (restores pass `vsock_override`), so the cross-host race is
    // gone — but leaving the file behind stays the safe default:
    // create/restore `remove_file` any stale UDS before binding,
    // so a future sandbox at the same UUID path picks up clean,
    // and the orphan is a 0-byte file bounded by sandbox
    // creation rate.
    //
    // The per-sandbox jail dir + canonical symlinks below are
    // still removed; those are deterministic per local sandbox
    // and don't have the cross-host-path-coupling issue.

    // ADR 0014: canonical rootfs / harness symlinks at
    // `<work_dir>/{rootfs,harness}/<sandbox_id>.{dev,ext4}`
    // live outside the jail by design. Remove them explicitly;
    // any restore that wants this sandbox_id back will re-create
    // them pointing at whatever it has materialized locally.
    for entry in paths::canonical_entries_for(&work_dir, id) {
        // NotFound is benign — sandbox may have been created
        // without a harness substrate, or pre-ADR-0014 (no entry
        // at all). `remove_file` on a symlink unlinks the entry,
        // not the target.
        let _ = tokio::fs::remove_file(&entry).await;
    }

    let jail_dir = work_dir.join(id.to_string());
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
}

/// ADR 0045 C2: the post-copy capture surface — a vmstate-only
/// snapshot package (state.bin + sidecar, NO memory artifact: the
/// destination demand-faults memory from this paused VM's address
/// space) and the pre-pause sidecar composition the presetup ships so
/// the destination can BEGIN restoring before the source pauses.
/// (Nothing in the sidecar changes across the freeze, so the two are
/// byte-identical by construction.)
impl FirecrackerBackend {
    /// Issue #540 / epic-parking-ladder seam: flip the RAM-ledger `parked`
    /// bit for `id`. **No `SandboxBackend` trait method wraps this** —
    /// the ladder's park/unpark ops own the transition that calls it;
    /// this issue only lands the flag and the accounting split. Test-
    /// visible (`pub(crate)`) so ledger unit tests can exercise the
    /// parked-exclusion behavior without a live ladder.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn set_parked_for_test(&self, id: SandboxId, parked: bool) {
        if let Some(mut live) = self.sandboxes.get_mut(&id) {
            live.parked = parked;
        }
    }

    /// Compose the restore sidecar (`manifest.json` content) from the
    /// LIVE sandbox state — the same composition `snapshot_with_type`
    /// writes at capture (spec env redacted, net echo, canonical-path
    /// anchors), minus any snapshot artifacts.
    pub fn compose_live_sidecar(
        &self,
        id: SandboxId,
        memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
    ) -> Result<Vec<u8>, SandboxError> {
        let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
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
        let mut redacted_spec = live.state.spec.clone();
        for v in redacted_spec.env.values_mut() {
            *v = "<redacted>".into();
        }
        let source_rootfs_canonical = live
            .state
            .spec
            .rootfs_source
            .is_some()
            .then(|| live.state.rootfs_canonical.clone());
        let manifest = FcSnapshotManifest {
            sandbox_id: id,
            created_at: Utc::now(),
            spec: redacted_spec,
            net: net_snapshot,
            format: MANIFEST_FORMAT_FC.into(),
            memory_manifest,
            trace_host_hint: self.config.host_id,
            // Tier 2 (resume-prefault fix): the FC backend is session-
            // agnostic, so it can't know the session id here; the host-agent
            // patches this to the session id at snapshot-finish.
            trace_lineage_id: None,
            source_rootfs_canonical,
            source_harness_canonical: None,
            source_vsock_canonical: Some(live.state.vsock_uds_path.clone()),
        };
        serde_json::to_vec_pretty(&manifest)
            .map_err(|e| SandboxError::Snapshot(format!("sidecar serialize: {e}")))
    }

    /// ADR 0045 C2: write a vmstate-only snapshot package into a fresh
    /// snapshot dir: the given sidecar bytes as `manifest.json` + a
    /// fork-v3 `state.bin`. The CALLER holds the VM paused and owns
    /// resume — no auto-resume anywhere on this path (split-brain
    /// guard). Requires the fork-v3 binary (stock FC rejects the
    /// create field — the capability gate).
    pub async fn snapshot_vmstate_only_package(
        &self,
        id: SandboxId,
        sidecar_json: &[u8],
    ) -> Result<(SnapshotId, PathBuf), SandboxError> {
        let socket = {
            let live = self.sandboxes.get(&id).ok_or(SandboxError::NotFound)?;
            live.state.firecracker_socket.clone()
        };
        paths::assert_rootfs_canonical(&self.work_dir, id)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("non-canonical jail layout: {e}")))?;
        let snapshot_id = SnapshotId::new();
        let dest = self.snapshot_dir_for(snapshot_id);
        tokio::fs::create_dir_all(&dest).await.map_err(|e| {
            SandboxError::Snapshot(format!("create snapshot dir {}: {e}", dest.display()))
        })?;
        tokio::fs::write(dest.join("manifest.json"), sidecar_json)
            .await
            .map_err(|e| SandboxError::Snapshot(format!("write sidecar: {e}")))?;
        let api = FirecrackerClient::new(&socket).with_timeout(Duration::from_secs(60));
        api.create_snapshot_vmstate_only(&dest.join("state.bin"))
            .await?;
        Ok((snapshot_id, dest))
    }
}

/// ADR 0028 Fix A: shared body of `SandboxBackend::snapshot` (Full →
/// `memory.bin`) and `snapshot_diff` (Diff → sparse `memory.diff`).
/// Identical sidecar/vmstate/dir contract either way; only the memory
/// artifact's name + capture type differ.
impl FirecrackerBackend {
    async fn snapshot_with_type(
        &self,
        id: SandboxId,
        snapshot_type: client::SnapshotType,
    ) -> Result<SnapshotMetadata, SandboxError> {
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
        // Snapshot duration scales with guest memory (every dirty page is
        // flushed to memory.bin synchronously), so the timeout scales with
        // the VM's RAM — a flat 60s tripped on large warm images (dev-brain's
        // 32 GiB capture). See `snapshot_create_timeout`.
        let api = FirecrackerClient::new(&socket)
            .with_timeout(snapshot_create_timeout(spec.memory.max_mib));
        let paths = match snapshot_type {
            client::SnapshotType::Full => api.create_snapshot(&dest).await?,
            client::SnapshotType::Diff => {
                api.create_snapshot_at(
                    dest.join("state.bin"),
                    dest.join("memory.diff"),
                    client::SnapshotType::Diff,
                )
                .await?
            }
        };

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
        // consistent across arbitrarily many chained restores. (Vsock is
        // re-keyed per-sandbox at load via the fork's `vsock_override`
        // these days, so its stamp below is lineage metadata; the live
        // value is still the honest one to record.)
        let source_rootfs_canonical = if spec.rootfs_source.is_some() {
            Some(live_rootfs_canonical)
        } else {
            None
        };
        // The harness drive IS re-pointed onto the live id's canonical
        // at restore — `repoint_harness_drive` (PATCH /drives) runs on
        // BOTH the warm-lease swap and every idle→active / evac resume
        // (ADR 0018 §12p) — so its embedded `path_on_host` tracks the
        // ADR 0021 P1.5: harness drive retired — nothing to anchor.
        let source_harness_canonical: Option<PathBuf> = None;
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
        for v in redacted_spec.env.values_mut() {
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
            // Tier 2 (resume-prefault fix): patched to the session id by the
            // host-agent at snapshot-finish (session-agnostic here).
            trace_lineage_id: None,
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
            base_memory_manifest: None,
            migration_source: None,
            // ADR 0014: portable-snapshot fields are populated by
            // `PooledBackend::snapshot` after the inner backend
            // returns. Bare FC stays BlobStorage-agnostic.
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            // ADR 0035: pin the bundle generations this VM's device
            // model references — the live spec reflects any fresh-create
            // swap, so this is what `load_snapshot` will reopen on the
            // next restore. The coord persists it to
            // `snapshots.aux_bundles` (the GC pin set); PooledBackend
            // publishes the bytes to BlobStorage.
            aux_bundles: spec
                .aux_ro_drives
                .iter()
                .filter_map(|d| {
                    d.sha256.as_ref().map(|sha| AuxBundleRef {
                        drive_id: d.drive_id.clone(),
                        sha256: sha.clone(),
                    })
                })
                .collect(),
            // Issue #529: the bare FC backend doesn't know the eviction/
            // checkpoint pause instant — `PooledBackend::SnapshotFinisher`
            // stamps it from `SnapshotCapture::paused_at` after this
            // returns (same layering as `memory_manifest` above).
            paused_at: None,
            // ADR 0095: capture never stamps peer hints; the resume
            // assembler does, coordinator-side.
            peer_hints: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn snapshot_timeout_scales_with_guest_memory() {
        // Small VMs stay essentially at the 60s floor (no regression).
        assert_eq!(snapshot_create_timeout(64), Duration::from_secs(61));
        assert_eq!(snapshot_create_timeout(4096), Duration::from_secs(60 + 64));
        // The 24 GiB dev-brain capture that timed out at the old 128 MiB/s floor
        // (252s, prod 2026-06) now gets 444s.
        assert_eq!(
            snapshot_create_timeout(24 * 1024),
            Duration::from_secs(60 + 384)
        );
        // Big guests get an ample budget.
        let big = snapshot_create_timeout(32 * 1024);
        assert!(
            big >= Duration::from_secs(300),
            "32 GiB snapshot budget must be ample, got {big:?}"
        );
        // Monotonic in memory.
        assert!(snapshot_create_timeout(16 * 1024) < big);
    }

    // ---- #567 version-skew message enrichment ----------------------

    #[test]
    fn agentd_rpc_recv_failure_names_version_skew_on_eof() {
        let e = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "early eof");
        let msg = describe_agentd_rpc_recv_failure(&e);
        assert!(
            msg.starts_with("early eof"),
            "must preserve the existing leading text so log-grep muscle \
             memory keeps working: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("skew"),
            "must name host/guest version skew: {msg}"
        );
        assert!(msg.contains("RefreshImage"), "must name the remedy: {msg}");
    }

    #[test]
    fn agentd_rpc_recv_failure_passes_through_non_eof_errors() {
        let e = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
        let msg = describe_agentd_rpc_recv_failure(&e);
        assert_eq!(
            msg,
            e.to_string(),
            "non-EOF errors must pass through unchanged"
        );
    }

    #[test]
    fn relay_connect_failure_names_version_skew_for_relay_port_eof() {
        let e = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "early eof");
        let msg = describe_relay_connect_failure(engram_harness_proto::PROXY_PORT_VSOCK_PORT, &e);
        assert!(
            msg.starts_with("early eof"),
            "must preserve the existing leading text: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("skew"),
            "must name host/guest version skew: {msg}"
        );
        assert!(
            msg.contains("#567"),
            "must name the dead-relay alternative: {msg}"
        );
    }

    #[test]
    fn relay_connect_failure_passes_through_for_non_relay_ports() {
        // The same read fails for unrelated reasons on agentd's exec
        // port (1024) -- relay-specific wording there would misname
        // the cause.
        let e = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "early eof");
        let msg = describe_relay_connect_failure(ENGRAM_AGENTD_PORT, &e);
        assert_eq!(
            msg,
            e.to_string(),
            "non-relay ports must not get relay-specific wording"
        );
    }

    #[test]
    fn relay_connect_failure_passes_through_for_non_eof_errors() {
        let e = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset");
        let msg = describe_relay_connect_failure(engram_harness_proto::PROXY_PORT_VSOCK_PORT, &e);
        assert_eq!(msg, e.to_string());
    }

    // ADR 0044 K2: node-cgroup escape helpers. A tempdir stands in for the
    // cgroup parent — this exercises the leaf-path + pid-write + rmdir logic,
    // NOT the kernel process-migration (which needs a real cgroupfs and is
    // dev-vm-validated; on cgroupfs the pid write moves the process).
    #[cfg(target_os = "linux")]
    #[test]
    fn node_cgroup_escape_creates_leaf_and_writes_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        escape_to_node_cgroup(tmp.path(), id, &[4242]).expect("escape");
        let leaf = tmp.path().join(id.to_string());
        assert!(leaf.is_dir(), "per-VM leaf cgroup dir created");
        let procs = std::fs::read_to_string(leaf.join("cgroup.procs")).unwrap();
        assert_eq!(procs.trim(), "4242", "pid written to cgroup.procs");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn node_cgroup_remove_drops_empty_leaf_idempotently() {
        // Models destroy() after FC + the handler are killed (empty cgroup).
        let tmp = tempfile::tempdir().unwrap();
        let id = SandboxId::new();
        let leaf = tmp.path().join(id.to_string());
        std::fs::create_dir_all(&leaf).unwrap();
        remove_node_cgroup(Some(tmp.path()), id);
        assert!(!leaf.exists(), "empty leaf removed");
        remove_node_cgroup(Some(tmp.path()), id); // gone → no-op
        remove_node_cgroup(None, id); // unconfigured → no-op
    }

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
            uffd_base_dir: None,
            restore_mode: RestoreMode::File,
            fresh_restore_override: None,
            track_dirty_pages: false,
            balloon: false,
            net_pool: None,
            egress_proxy_port: None,
            egress_dns_port: None,
            host_id: None,
            uffd_cache_root: None,
            uffd_substrate_sock: None,
            uffd_blob_root: None,
            cpu_template: None,
            bundle_dir: dir.path().join("bundles"),
            vm_cgroup_parent: None,
        };
        (FirecrackerBackend::new(dir.path(), cfg), dir)
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 256 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        }
    }

    /// ADR 0055: a reserved slot requested on a host without a bundle stamp
    /// (no `sentinel` entry) must fail loudly at the resolve step — capturing a
    /// sentinel-less snapshot silently is the ADR 0035 incident's setup.
    #[tokio::test]
    async fn create_with_reserved_slot_requires_sentinel_stamp() {
        let (b, d) = backend();
        // Satisfy the kernel + rootfs existence checks (they precede
        // the resolve step) so create reaches aux-drive resolution.
        std::fs::write(d.path().join("nonexistent-vmlinux"), b"vmlinux").unwrap();
        let rootfs = d.path().join("rootfs.ext4");
        std::fs::write(&rootfs, b"not-really-ext4").unwrap();
        let mut sp = spec();
        sp.rootfs_source = Some(rootfs);
        sp.aux_ro_drives = vec![AuxRoDrive::reserved_slot(0)];
        match b.create(sp).await {
            Err(SandboxError::InvalidSpec(msg)) => {
                assert!(msg.contains("stages no bundles"), "{msg}");
            }
            other => panic!("expected InvalidSpec(no stamp), got {other:?}"),
        }
    }

    /// ADR 0055: a stamp that doesn't carry the `sentinel` entry is equally
    /// loud (the host image staged something else but not the sentinel).
    #[tokio::test]
    async fn create_with_missing_sentinel_stamp_errors() {
        let (b, d) = backend();
        std::fs::write(d.path().join("nonexistent-vmlinux"), b"vmlinux").unwrap();
        let rootfs = d.path().join("rootfs.ext4");
        std::fs::write(&rootfs, b"not-really-ext4").unwrap();
        let bundle_dir = d.path().join("bundles");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        std::fs::write(
            bundle_dir.join(AuxRoDrive::CURRENT_STAMP),
            br#"{"something-else": "aaaa"}"#,
        )
        .unwrap();
        let mut sp = spec();
        sp.rootfs_source = Some(rootfs);
        sp.aux_ro_drives = vec![AuxRoDrive::reserved_slot(0)];
        match b.create(sp).await {
            Err(SandboxError::InvalidSpec(msg)) => {
                assert!(msg.contains("sentinel"), "{msg}");
            }
            other => panic!("expected InvalidSpec(missing sentinel), got {other:?}"),
        }
    }

    /// ADR 0055: with a valid `sentinel` stamp entry + staged file the resolve
    /// step passes — create proceeds past it (and fails much later on the
    /// nonexistent firecracker binary, the negative-path fixture's expected
    /// terminal error). Distinguishing the error kind proves resolution
    /// consumed the stamp.
    #[tokio::test]
    async fn create_with_resolvable_sentinel_passes_resolution() {
        let (b, d) = backend();
        std::fs::write(d.path().join("nonexistent-vmlinux"), b"vmlinux").unwrap();
        let rootfs = d.path().join("rootfs.ext4");
        std::fs::write(&rootfs, b"not-really-ext4").unwrap();
        let bundle_dir = d.path().join("bundles");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let sha = "a".repeat(64);
        std::fs::write(
            bundle_dir.join(AuxRoDrive::CURRENT_STAMP),
            format!("{{\"{}\": \"{sha}\"}}", AuxRoDrive::SENTINEL_STAMP_KEY),
        )
        .unwrap();
        std::fs::write(
            bundle_dir.join(AuxRoDrive::staged_file_name(&sha)),
            b"squashfs-bytes",
        )
        .unwrap();
        let mut sp = spec();
        sp.rootfs_source = Some(rootfs);
        sp.aux_ro_drives = vec![AuxRoDrive::reserved_slot(0)];
        match b.create(sp).await {
            Err(SandboxError::InvalidSpec(msg)) => {
                panic!("resolution should have passed, got InvalidSpec: {msg}")
            }
            Err(_) => {} // FC spawn failure — past the resolve step.
            Ok(_) => panic!("create can't succeed without a real firecracker"),
        }
    }

    /// ADR 0080: the agentd slot resolves against the `agentd` stamp key —
    /// and a stamp without it is loud (a host that can't boot the agentd
    /// slot can't cold-boot at all). The lookup is lazy per-slot: the
    /// sentinel-only specs above never demand an `agentd` entry.
    #[tokio::test]
    async fn create_with_agentd_slot_demands_agentd_stamp_key() {
        let (b, d) = backend();
        std::fs::write(d.path().join("nonexistent-vmlinux"), b"vmlinux").unwrap();
        let rootfs = d.path().join("rootfs.ext4");
        std::fs::write(&rootfs, b"not-really-ext4").unwrap();
        let bundle_dir = d.path().join("bundles");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let sha = "a".repeat(64);
        // Stamp carries only the sentinel — the agentd slot must fail loud.
        std::fs::write(
            bundle_dir.join(AuxRoDrive::CURRENT_STAMP),
            format!("{{\"{}\": \"{sha}\"}}", AuxRoDrive::SENTINEL_STAMP_KEY),
        )
        .unwrap();
        let mut sp = spec();
        sp.rootfs_source = Some(rootfs.clone());
        sp.aux_ro_drives = vec![AuxRoDrive::reserved_slot(AuxRoDrive::AGENTD_SLOT_INDEX)];
        match b.create(sp).await {
            Err(SandboxError::InvalidSpec(msg)) => {
                assert!(msg.contains(AuxRoDrive::AGENTD_STAMP_KEY), "{msg}");
            }
            other => panic!("expected InvalidSpec(missing agentd), got {other:?}"),
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
            migration_source: None,
            id: SnapshotId::new(),
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: None,
            base_memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
            peer_hints: Vec::new(),
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
            trace_lineage_id: None,
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
            migration_source: None,
            id: snapshot_id,
            size_bytes: 0,
            created_at: Utc::now(),
            image_version: "test:1".into(),
            disk_manifest: None,
            memory_manifest: None,
            base_memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
            peer_hints: Vec::new(),
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

        spawn_process_supervisor(sandboxes.clone(), id, pid, "test-sleep", |_id, ()| async {});

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
        spawn_process_supervisor(sandboxes.clone(), id, pid, "test-sleep", |_id, ()| async {});

        child.kill().await.expect("kill child");
        let _ = child.wait().await;

        // Wait long enough for the supervisor to have polled at
        // least twice; the map should remain empty (no panic, no
        // weird state).
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!sandboxes.contains_key(&id));
    }

    /// Issue #198 regression: when a supervisor wins the prune race it
    /// must hand the removed value to its teardown callback so the
    /// surviving sibling process + host state get reaped — NOT drop the
    /// value silently. Before the fix the prune was a bare
    /// `sandboxes.remove(..)` with no callback at all, so the removed
    /// `LiveSandbox` (owning the still-live FC `Child`) was dropped and,
    /// post-ADR-0044-K2, killed nothing → wedged VM + leaked /30 +
    /// re-adopted `sandbox.json`.
    ///
    /// Mirrors the real Uffd-mode topology: TWO supervisors over the
    /// SAME id (one per pid). When one pid dies, exactly ONE of them
    /// wins `remove` and runs teardown; the other observes the entry
    /// already gone and is a no-op. We assert teardown fires exactly
    /// once and receives the value we inserted.
    #[tokio::test]
    async fn supervisor_prune_runs_teardown_on_removed_value() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Value carries a sentinel so we can prove teardown got THE
        // removed value, not a default/empty stand-in.
        let sandboxes: Arc<DashMap<SandboxId, u64>> = Arc::new(DashMap::new());
        let id = SandboxId::new();
        let sentinel: u64 = 0xDEAD_BEEF;
        sandboxes.insert(id, sentinel);

        let teardown_calls = Arc::new(AtomicUsize::new(0));
        let seen_value = Arc::new(parking_lot::Mutex::new(None::<u64>));

        // A "fc-like" and a "uffd-like" supervisor over the same id,
        // each watching a distinct real child pid — matching the two
        // supervisors a Uffd-mode sandbox gets.
        let mut child_a = tokio::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep A");
        let mut child_b = tokio::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep B");
        let pid_a = child_a.id().expect("child A has pid");
        let pid_b = child_b.id().expect("child B has pid");

        let mk_cb = || {
            let calls = teardown_calls.clone();
            let seen = seen_value.clone();
            move |_id: SandboxId, removed: u64| {
                let calls = calls.clone();
                let seen = seen.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    *seen.lock() = Some(removed);
                }
            }
        };

        spawn_process_supervisor(sandboxes.clone(), id, pid_a, "fc-like", mk_cb());
        spawn_process_supervisor(sandboxes.clone(), id, pid_b, "uffd-like", mk_cb());

        // Kill BOTH children so BOTH supervisors observe ESRCH and race
        // to `remove`. The fix must ensure exactly one teardown runs.
        child_a.kill().await.expect("kill A");
        let _ = child_a.wait().await;
        child_b.kill().await.expect("kill B");
        let _ = child_b.wait().await;

        // Wait for the prune + teardown to land (1s poll + reap +
        // callback). Generous deadline to stay non-flaky in CI.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !sandboxes.contains_key(&id) && teardown_calls.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            !sandboxes.contains_key(&id),
            "entry must be pruned from the map"
        );
        // Exactly one teardown: the race winner removed the entry and
        // ran teardown; the loser saw `None` and was a no-op. A second
        // teardown would mean a double-free of host state.
        assert_eq!(
            teardown_calls.load(Ordering::SeqCst),
            1,
            "teardown must run exactly once (race winner only), not zero (old silent-drop bug) \
             and not twice (double-free)"
        );
        assert_eq!(
            *seen_value.lock(),
            Some(sentinel),
            "teardown must receive the exact LiveSandbox value that was removed",
        );
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

    // ---- ADR 0045 D3: effective_restore_mode (substrate-aware) ----

    #[test]
    fn effective_restore_mode_bifurcates_create_vs_resume() {
        let (mut be, _dir) = backend();
        // Default: no substrate dir — base session.create keeps ADR
        // 0022's File-mode density path; idle-resume follows
        // restore_mode.
        be.config.restore_mode = RestoreMode::Uffd;
        be.config.uffd_base_dir = None;
        assert_eq!(
            be.effective_restore_mode(/*fresh=*/ true),
            RestoreMode::File,
            "without the substrate, base session.create stays on File",
        );
        assert_eq!(
            be.effective_restore_mode(/*fresh=*/ false),
            RestoreMode::Uffd,
            "idle-resume always follows restore_mode",
        );

        // Substrate enabled: ONE switch flips fresh-create to Uffd
        // against the shared base shm (ADR 0045 D3 parity-gated).
        be.config.uffd_base_dir = Some(std::path::PathBuf::from("/dev/shm/engram"));
        assert_eq!(be.effective_restore_mode(true), RestoreMode::Uffd);
        assert_eq!(be.effective_restore_mode(false), RestoreMode::Uffd);

        // ADR 0092: the fresh-create override beats the derivation — a
        // substrate host restores fresh creates from the per-image
        // memfile (reclaimable page-cache residency) while resumes keep
        // the substrate.
        be.config.fresh_restore_override = Some(RestoreMode::File);
        assert_eq!(
            be.effective_restore_mode(true),
            RestoreMode::File,
            "explicit file override wins for fresh creates on a substrate host",
        );
        assert_eq!(
            be.effective_restore_mode(false),
            RestoreMode::Uffd,
            "the override never touches resumes",
        );
        be.config.fresh_restore_override = Some(RestoreMode::Uffd);
        be.config.uffd_base_dir = None;
        assert_eq!(
            be.effective_restore_mode(true),
            RestoreMode::Uffd,
            "explicit uffd override wins over the substrate-off derivation",
        );
    }

    #[test]
    fn restore_memory_is_lazy_for_tracks_effective_mode() {
        // The host-agent restore path keys BOTH its prefetch-block gate and its
        // memory.bin-materialize gate on `restore_memory_is_lazy_for(fresh)`, so
        // lock that it mirrors `effective_restore_mode`: lazy iff Uffd. A
        // substrate fresh-create being lazy is exactly what lets a cold
        // base-create background its memory prefetch instead of blocking on it.
        let (mut be, _dir) = backend();
        be.config.restore_mode = RestoreMode::Uffd;

        // Substrate off: File base-create is NOT lazy (needs memory.bin);
        // a UFFD resume IS lazy.
        be.config.uffd_base_dir = None;
        assert!(
            !be.restore_memory_is_lazy_for(/*fresh=*/ true),
            "File base-create must materialize memory.bin (not lazy)",
        );
        assert!(
            be.restore_memory_is_lazy_for(/*fresh=*/ false),
            "UFFD resume serves memory lazily",
        );

        // Substrate on: fresh-create flips to Uffd, so it is lazy too.
        be.config.uffd_base_dir = Some(std::path::PathBuf::from("/dev/shm/engram"));
        assert!(be.restore_memory_is_lazy_for(true));
        assert!(be.restore_memory_is_lazy_for(false));
    }

    /// Review finding 2 regression test: `prefault_stats_path` must be
    /// `None` for a sandbox with no live uffd-handler — `RestoreMode::File`
    /// (the config default, and the documented `ENGRAM_FC_RESTORE_MODE=file`
    /// fleet knob) never spawns one, so nothing could ever write the file;
    /// returning `Some` there made every File-mode resume falsely alarm as
    /// `stats_missing`. Keyed off `uffd_pid` (populated for
    /// `RestoreMode::Uffd` restores AND pidfd-reattached Uffd sandboxes),
    /// not the backend's static config — a fleet can run both modes.
    #[test]
    fn prefault_stats_path_none_without_a_uffd_handler() {
        let (be, _dir) = backend();
        let id = SandboxId::new();

        // No entry in `sandboxes` at all (unknown/not-yet-live sandbox).
        assert_eq!(be.prefault_stats_path(id), None);

        let live_without_uffd = |uffd_pid: Option<u32>| {
            let (_tx, agent_ready) = tokio::sync::watch::channel(true);
            LiveSandbox {
                state: SandboxState {
                    spec: spec(),
                    firecracker_socket: be.work_dir.join(id.to_string()).join("fc.sock"),
                    rootfs_path: be.work_dir.join(id.to_string()).join("rootfs.ext4"),
                    vsock_cid: 3,
                    vsock_uds_path: be.work_dir.join(id.to_string()).join("vsock.sock"),
                    rootfs_canonical: be.work_dir.join(id.to_string()).join("rootfs.ext4"),
                },
                child: None,
                fc_pid: None,
                uffd_handler: None,
                uffd_pid,
                net: None,
                netns: None,
                guest_endpoints: parking_lot::Mutex::new(None),
                #[cfg(target_os = "linux")]
                parked: false,
                agentd_slot_swapped: false,
                agent_ready,
            }
        };

        // Live, but File-mode restore: no uffd_pid → still None.
        be.sandboxes.insert(id, live_without_uffd(None));
        assert_eq!(
            be.prefault_stats_path(id),
            None,
            "File-mode sandboxes never spawn a handler; must not alarm as stats_missing",
        );

        // Live Uffd-mode restore (or a pidfd-reattached one): uffd_pid is
        // Some → the sibling path resolves.
        be.sandboxes.insert(id, live_without_uffd(Some(4242)));
        assert_eq!(
            be.prefault_stats_path(id),
            Some(be.work_dir.join(id.to_string()).join(PREFAULT_STATS_FILE)),
        );
    }

    // ADR 0022: the smaps_rollup parser, exercised against the test
    // process's own /proc entry. Linux-only (no smaps_rollup on macOS);
    // runs on CI's Linux runner in the normal unit-test job.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn read_smaps_rollup_parses_own_process() {
        let pid = std::process::id();
        let (pss, rss) = super::read_smaps_rollup_pss_rss(pid)
            .await
            .expect("own process must have a readable smaps_rollup");
        // A running process has non-zero resident + proportional set.
        assert!(rss > 0, "Rss must be > 0");
        assert!(pss > 0, "Pss must be > 0");
        assert!(pss <= rss, "Pss ({pss}) can never exceed Rss ({rss})");
        // A nonexistent pid returns None, not an error.
        assert!(super::read_smaps_rollup_pss_rss(u32::MAX).await.is_none());
    }

    /// Issue #540: `guest_memory_stats` must bucket PSS/RSS by the
    /// per-sandbox `parked` flag so the RAM ledger never adds a parked
    /// resident's memory back into `allocatable_mib`. Both entries
    /// sample the test process's own pid (no real FC process needed) —
    /// the property under test is the split, not the smaps parse
    /// (covered by `read_smaps_rollup_parses_own_process`).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn guest_memory_stats_buckets_parked_pss_separately() {
        let (be, _dir) = backend();
        let own_pid = std::process::id();
        let (_tx, agent_ready) = tokio::sync::watch::channel(true);
        let dummy_state = || SandboxState {
            spec: spec(),
            firecracker_socket: PathBuf::new(),
            rootfs_path: PathBuf::new(),
            vsock_cid: 3,
            vsock_uds_path: PathBuf::new(),
            rootfs_canonical: PathBuf::new(),
        };

        let running_id = SandboxId::new();
        be.sandboxes.insert(
            running_id,
            LiveSandbox {
                state: dummy_state(),
                child: None,
                fc_pid: Some(own_pid),
                uffd_handler: None,
                uffd_pid: None,
                net: None,
                netns: None,
                guest_endpoints: parking_lot::Mutex::new(None),
                parked: false,
                agentd_slot_swapped: false,
                agent_ready: agent_ready.clone(),
            },
        );
        let parked_id = SandboxId::new();
        be.sandboxes.insert(
            parked_id,
            LiveSandbox {
                state: dummy_state(),
                child: None,
                fc_pid: Some(own_pid),
                uffd_handler: None,
                uffd_pid: None,
                net: None,
                netns: None,
                guest_endpoints: parking_lot::Mutex::new(None),
                parked: false,
                agentd_slot_swapped: false,
                agent_ready: agent_ready.clone(),
            },
        );
        be.set_parked_for_test(parked_id, true);

        let stats = be
            .guest_memory_stats()
            .await
            .expect("linux backend samples memory");
        assert_eq!(stats.sampled, 1, "only the non-parked sandbox counts here");
        assert_eq!(stats.parked_sampled, 1, "the parked sandbox counts here");
        assert!(stats.pss_bytes > 0, "non-parked PSS must be charged");
        assert!(stats.rss_bytes > 0);
        assert!(
            stats.parked_pss_bytes > 0,
            "parked PSS must be measured (never assumed 0)"
        );
        assert!(
            stats.parked_rss_bytes > 0,
            "parked RSS must be measured too, not discarded alongside PSS"
        );
        // The two entries sample the SAME real pid, so the two buckets
        // should be roughly equal — the point is they land in DIFFERENT
        // buckets, not summed into one.
        assert_ne!(
            stats.pss_bytes, 0,
            "parked residency must not silently zero out the running bucket"
        );
    }

    /// True iff `pid` is alive (`kill(pid, 0)` succeeds). A dead/reaped
    /// pid yields `ESRCH`.
    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        // SAFETY: signal 0 performs only the existence/permission check,
        // it sends nothing. Trivially sound.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Issue #196 regression: `destroy()` must be cancel-safe. The map
    /// entry is removed up front, so if the post-removal teardown is
    /// cancelled mid-await (the host-agent tonic handler runs destroy
    /// inline → wire cancellation drops the future), the OLD code left the
    /// FC process running forever AND a retried destroy hit the idempotent
    /// early-return, falsely reporting success.
    ///
    /// We model the FC process with a real long-lived child (`sleep`,
    /// `kill_on_drop(false)` to mirror ADR 0044 K2 — dropping the
    /// `LiveSandbox` must NOT kill it), and force `destroy` to park in the
    /// cancellable graceful-shutdown window with a UDS that accepts the
    /// SendCtrlAltDel connection but never answers (so `put_action` blocks
    /// for the full `GRACEFUL_SHUTDOWN_TIMEOUT`). We then cancel `destroy`
    /// after 100ms — well inside that 3s window — and assert the spawned
    /// teardown task still SIGKILLs the child, removes the jail dir + the
    /// canonical entry, and that a retried destroy stays `Ok` with the
    /// invariants holding (idempotency preserved, but no longer a lie).
    ///
    /// Pre-fix this test fails: the cancelled future drops `live`, the
    /// child survives, the jail dir lingers, and the retry no-ops.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn destroy_is_cancel_safe_kills_fc_and_cleans_up() {
        use std::time::Duration;
        use tokio::io::AsyncReadExt;

        // `_dir` keeps the backend's work_dir tempdir alive for the test.
        let (be, _dir) = backend();
        let id = SandboxId::new();

        // A UDS at the FC API socket path that accepts but never replies,
        // so `FirecrackerClient::put_action(SendCtrlAltDel)` blocks inside
        // `tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, ..)` — i.e. the
        // cancellable window the bug lives in. Rooted in the shared short
        // socket dir because UDS paths are capped at SUN_LEN (~104B) and the
        // per-test tempdir + sandbox-id filename overflow it.
        let sock_dir = engram_core::socket::short_socket_dir();
        std::fs::create_dir_all(&sock_dir).unwrap();
        let socket = sock_dir.join("fc.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind fc api stub socket");
        let _accept_task = tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                // Hold the connection open and never write a response;
                // just drain so the kernel doesn't RST. Dropped when the
                // test ends.
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = conn.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                    }
                });
            }
        });

        // Real, long-lived stand-in for the FC process. `kill_on_drop`
        // OFF mirrors prod (ADR 0044 K2): dropping the `LiveSandbox`'s
        // `Child` must NOT reap it — only an explicit kill does. That's
        // exactly what makes the cancel orphan a VM pre-fix.
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("120").kill_on_drop(false);
        let child = cmd.spawn().expect("spawn sleep stand-in for firecracker");
        let fc_pid = child.id().expect("sleep child has a pid");
        assert!(
            pid_alive(fc_pid),
            "stand-in FC process is alive pre-destroy"
        );

        // Materialize the jail dir + a canonical entry so we can assert
        // they're removed by the (detached) teardown.
        let jail_dir = be.work_dir.join(id.to_string());
        std::fs::create_dir_all(&jail_dir).unwrap();
        let canonical = paths::canonical_entries_for(&be.work_dir, id);
        for entry in &canonical {
            if let Some(parent) = entry.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(entry, b"stub").unwrap();
        }

        let (_tx, agent_ready) = tokio::sync::watch::channel(true);
        be.sandboxes.insert(
            id,
            LiveSandbox {
                state: SandboxState {
                    spec: spec(),
                    firecracker_socket: socket.clone(),
                    rootfs_path: jail_dir.join("rootfs.ext4"),
                    vsock_cid: 3,
                    vsock_uds_path: jail_dir.join("vsock.sock"),
                    rootfs_canonical: jail_dir.join("rootfs.ext4"),
                },
                child: Some(child),
                fc_pid: Some(fc_pid),
                uffd_handler: None,
                uffd_pid: None,
                net: None,
                netns: None,
                guest_endpoints: parking_lot::Mutex::new(None),
                #[cfg(target_os = "linux")]
                parked: false,
                agentd_slot_swapped: false,
                agent_ready,
            },
        );

        // Cancel destroy 100ms in — deep inside the 3s graceful window.
        let cancelled = tokio::time::timeout(Duration::from_millis(100), be.destroy(id)).await;
        assert!(
            cancelled.is_err(),
            "destroy must still be parked in the graceful window when we cancel it"
        );

        // The map entry is gone immediately (removed up front) — pre- and
        // post-fix alike.
        assert!(
            !be.sandboxes.contains_key(&id),
            "map entry removed by destroy"
        );

        // THE invariant: despite the cancellation, the detached teardown
        // task runs to completion. After the graceful window elapses it
        // SIGKILLs the FC stand-in and removes the on-disk artifacts.
        // Poll generously (graceful window is 3s; add slack).
        let mut killed = false;
        for _ in 0..120 {
            if !pid_alive(fc_pid) && !jail_dir.exists() {
                killed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            killed,
            "cancelled destroy must STILL kill the FC process and remove the jail dir \
             (pid_alive={}, jail_exists={})",
            pid_alive(fc_pid),
            jail_dir.exists(),
        );
        for entry in &canonical {
            assert!(
                !entry.exists(),
                "canonical entry {} must be removed by the teardown",
                entry.display()
            );
        }

        // A retried destroy stays Ok AND the invariants still hold — the
        // idempotent early-return is no longer a lie.
        be.destroy(id).await.expect("retried destroy is Ok");
        assert!(!pid_alive(fc_pid), "FC process stays dead after retry");
        assert!(!be.sandboxes.contains_key(&id), "map entry stays absent");

        // The FC stand-in's `Child` was moved into the `LiveSandbox` and
        // reaped inside `kill_fc`'s `Child::kill().await`, so no zombie is
        // left. Tidy up the stub socket dir.
        let _ = std::fs::remove_dir_all(&sock_dir);
    }

    /// Issue #197: the `SpawnKillGuard` is now armed INSIDE the spawn
    /// helpers (right after `.spawn()`), so it covers the helper's own
    /// `wait_for_socket` await AND everything the caller does before the
    /// commit point — closing the window where a restore future cancelled
    /// between `spawn()` and the (previously caller-side, post-`join!`)
    /// guard arming orphaned the FC/uffd process.
    ///
    /// We can't drive a real FC spawn in a unit test, but we CAN assert the
    /// invariant the fix relies on: a guard constructed exactly the way the
    /// helpers now construct it — `child.id().map(SpawnKillGuard::new)` the
    /// instant after spawn — SIGKILLs the (kill_on_drop=false, mirroring ADR
    /// 0044 K2) process when dropped without `disarm()`, and leaves it alone
    /// when disarmed. Pre-fix the guard didn't exist during that window at
    /// all, so a dropped future killed nothing.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_kill_guard_armed_at_spawn_reaps_on_cancel_window_drop() {
        use std::time::Duration;

        // Stand-in for FC/uffd: long-lived, kill_on_drop OFF (prod shape).
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("120").kill_on_drop(false);
        let child = cmd.spawn().expect("spawn sleep stand-in");
        let pid = child.id().expect("child has a pid");
        assert!(pid_alive(pid), "stand-in alive right after spawn");

        // Arm the guard the way the spawn helpers do now.
        let guard = child.id().map(SpawnKillGuard::new).expect("pid present");

        // Model the cancellation window: the enclosing future is dropped
        // (here, scope exit) while the guard is still armed. The Child is
        // also dropped — with kill_on_drop OFF it would NOT reap the process,
        // so the guard is the only thing that can.
        drop(child);
        drop(guard);

        let mut reaped = false;
        for _ in 0..50 {
            if !pid_alive(pid) {
                reaped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            reaped,
            "armed SpawnKillGuard must SIGKILL the half-spawned process on drop"
        );

        // Disarm variant: a committed VM's guard is defused and the process
        // survives the drop (only `destroy()`/an explicit kill reaps it).
        let child2 = cmd.spawn().expect("spawn second sleep stand-in");
        let pid2 = child2.id().expect("child2 has a pid");
        let mut guard2 = child2.id().map(SpawnKillGuard::new).expect("pid present");
        guard2.disarm();
        drop(guard2);
        // Give a (would-be) SIGKILL time to land if disarm were ineffective.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            pid_alive(pid2),
            "disarmed SpawnKillGuard must leave the committed process running"
        );
        // Clean up the survivor.
        unsafe {
            libc::kill(pid2 as i32, libc::SIGKILL);
        }
    }

    /// Issue #197 companion leak: per-VM netns teardown was `Err`-arm-only,
    /// never drop-based, so a cancelled/early-returned restore leaked the
    /// netns + veth + SNAT iptables rule + the allocator's `/30` slot. The
    /// new `NetnsGuard` makes teardown drop-based.
    ///
    /// Linux-only because the allocator-free + `ip netns delete` body of
    /// `net::teardown_netns` is `cfg(target_os = "linux")` — the observable
    /// behavior (the SNAT slot returning to the pool) only exists there.
    /// Runs in CI's `tests (linux)` workspace nextest pass (not `#[ignore]`,
    /// no KVM). We use a bogus netns name so `ip netns delete` best-effort
    /// no-ops; the allocator free runs regardless.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn netns_guard_drop_frees_snat_slot_on_cancel() {
        use std::net::Ipv4Addr;
        use std::time::Duration;

        let allocator = Arc::new(parking_lot::Mutex::new(net::NetworkAllocator::new(
            Ipv4Addr::new(10, 200, 0, 0),
        )));
        // Allocate a real SNAT slot, as `provision_netns` would.
        let snat_cidr = allocator.lock().alloc().expect("alloc snat slot");
        let baseline = allocator.lock().live_count();
        assert!(
            baseline >= 2,
            "slot 0 (reserved) + our snat slot are in use"
        );

        let setup = net::NetnsSetup {
            netns_name: format!("eng197-test-{}", std::process::id()),
            veth_host: "eng197h".into(),
            veth_ns: "eng197n".into(),
            tap_name: "tap0".into(),
            vm_cidr: net::VmCidr::new(Ipv4Addr::new(10, 200, 0, 0)),
            snat_cidr,
        };

        // Armed guard dropped (the cancellation/early-return case) must run
        // teardown on its detached task and free the SNAT slot.
        {
            let _guard = NetnsGuard::new(Some(setup.clone()), Arc::clone(&allocator));
        } // drop here

        let mut freed = false;
        for _ in 0..100 {
            if allocator.lock().live_count() < baseline {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            freed,
            "armed NetnsGuard drop must free the SNAT slot (live_count {} >= baseline {})",
            allocator.lock().live_count(),
            baseline,
        );

        // Committed path: `into_committed()` disarms and the slot stays in
        // use — the live sandbox's `destroy()` now owns teardown.
        let snat2 = allocator.lock().alloc().expect("alloc second snat slot");
        let baseline2 = allocator.lock().live_count();
        let setup2 = net::NetnsSetup {
            snat_cidr: snat2,
            ..setup
        };
        let guard = NetnsGuard::new(Some(setup2), Arc::clone(&allocator));
        let committed = guard.into_committed();
        assert!(committed.is_some(), "into_committed yields the setup");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            allocator.lock().live_count(),
            baseline2,
            "disarmed (committed) NetnsGuard must NOT free the slot"
        );
    }
}
