use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};

use super::ids::SandboxId;
use super::image::NetworkPolicy;

/// ADR 0068 probe-before-host_lost: the answer to "is this specific
/// sandbox actually there", from GROUND TRUTH — not the in-memory
/// sandbox map `SandboxBackend::list()` reads (that map, or its
/// heartbeat-carried mirror `running_sandboxes`, being wrong is exactly
/// the desync `reconcile::flip_missing` uses this to rescue sessions
/// from). On FC: `process_alive` comes from the persisted per-sandbox
/// manifest (the same three-axis pid/start-time/comm identity the
/// survivor-reattach pass already trusts), read independently of the
/// in-memory map. On VZ/Process (no orphan-VM mode exists there): the
/// live child-process handle IS the ground truth, so both fields
/// mirror it — see `SandboxBackend::probe_sandbox`'s default impl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SandboxProbe {
    /// Present in the backend's in-memory sandbox map (`list()`
    /// membership).
    pub known_to_backend: bool,
    /// The VMM process for this sandbox is alive on this host, checked
    /// independently of `known_to_backend`.
    pub process_alive: bool,
    /// ADR 0091: is the guest's CONTROL PLANE answering — for FC, a
    /// connect() against the VMM API socket. `None` = not probed /
    /// backend has no such concept. A process can be alive with a dead
    /// control socket (the zombie-session class): `process_alive` says
    /// "don't reap me", `control_alive == Some(false)` says "don't
    /// route work into me".
    pub control_alive: Option<bool>,
}

/// Spec for creating a sandbox via `SandboxBackend::create`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Human-readable image identifier (e.g. `warm-2026-04-27`).
    /// Carried on snapshots for record-keeping; not used by the
    /// backend to locate the rootfs (that's `rootfs_source`).
    pub image: String,
    /// Resolved on-disk path to the image's rootfs. ProcessBackend
    /// materializes this directory into the sandbox cwd; future
    /// FirecrackerBackend points the root drive at it (or its
    /// `rootfs.ext4` sibling). `None` = empty workdir / fresh boot
    /// from base kernel; useful for bootstrap and tests.
    #[serde(default)]
    pub rootfs_source: Option<PathBuf>,
    /// Phase 5+: OCI registry URI for the bake image (e.g.
    /// `gcr.io/cortex/api:warm-X`). When set, the host-agent pulls
    /// it into its content-addressable cache and uses the cached
    /// rootfs.ext4 as `rootfs_source`. When `None`, the legacy
    /// `rootfs_source` path applies as-is — preserves single-host
    /// dev workflows that pre-bake images into `<local_path>/images/`.
    #[serde(default)]
    pub image_uri: Option<String>,
    /// ADR 0028 Fix B: chunked-manifest override for the root disk.
    /// When set, the host serves the rootfs from THIS manifest's
    /// chunks (NBD-attached, continuous-flush continues the same
    /// lineage) instead of resolving it from `image_uri` — the
    /// disk-only cold-boot recovery shape: a fresh kernel boot
    /// mounting a session's evolved `live_disk_manifest`. Wire note:
    /// `SandboxSpec` is bincode-framed coord→host, so a mixed-version
    /// fleet mid-deploy can't decode creates — acceptable per the
    /// clean-break norm (coord + host MIG roll together on push).
    #[serde(default)]
    pub rootfs_manifest: Option<super::manifest::ManifestRef>,
    // ADR 0021 P1.5b retired `harness_pack_uri` + `harness_substrate`.
    // The harness now lives in the image rootfs at the manifest-
    // declared `[harness] exec` path — there's no separate registry
    // URI to resolve and no per-session ext4 substrate to attach.
    pub cpu: CpuLimit,
    pub memory: MemoryLimit,
    pub disk: DiskLimit,
    /// Wall-clock TTL after which the host agent will force-stop the VM.
    pub ttl: Option<Duration>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    /// Per-sandbox egress policy derived from the image manifest's
    /// `[network]` block plus any session-time augmentation (e.g. a
    /// `WorkspaceSpec::Git` URL host gets auto-allowed so clone
    /// works). Backends with hard-isolation networking (FC) translate
    /// this into iptables rules; backends without (VZ's Apple NAT)
    /// log a warn-once when `default = Deny` and `allow_hosts` is
    /// non-empty.
    #[serde(default)]
    pub network: NetworkPolicy,
    /// ADR 0027/0055: read-only host-mounted bundles attached as virtio-blk
    /// drives — the RO-mount skills engine. ADR 0055 makes these the fixed
    /// pool of reserved dynamic slots (`dyn_0..dyn_{RESERVED_SLOTS-1}`): each
    /// carries the sentinel at capture and is `patch_drive`-swapped to a
    /// per-session selected skill in the paused restore window.
    ///
    /// ADR 0035: entries from the coord are *symbolic* (`sha256 = None`,
    /// "attach whatever generation this host currently stages"); the FC
    /// backend resolves them against the host's staged-bundle stamp at
    /// capture, and the sandbox manifest records the resolved form so a
    /// restore re-anchors against the exact immutable generation.
    ///
    /// `#[serde(default)]` covers the legacy-snapshot JSON path (older
    /// sidecars predate this field); the coord ↔ host-agent bincode
    /// wire rolls coord+host together, so the positional encode/decode
    /// stay in lockstep (an empty `Vec` still encodes as length 0).
    #[serde(default)]
    pub aux_ro_drives: Vec<AuxRoDrive>,
    /// ADR 0112: size (MiB) of the guest's ephemeral swap device.
    /// `None`/`Some(0)` ⇒ no swap drive is attached. The FC backend
    /// attaches a host-file-backed RW drive of exactly this size at
    /// base-snapshot capture and `patch_drive`s a fresh sparse file
    /// over it in every restore's paused window — contents are
    /// discarded at capture, never persisted, never in a manifest.
    ///
    /// Restore reads this from the snapshot sidecar's recorded spec
    /// (the device geometry is frozen in `state.bin`), so capture and
    /// restore cannot skew. `#[serde(default)]` covers pre-0112
    /// sidecar JSON; the coord ↔ host bincode wire is a lockstep roll
    /// per the clean-break norm above.
    #[serde(default)]
    pub swap_mib: Option<u32>,
}

/// A read-only mount the host attaches to the guest as an additional
/// virtio-blk drive (ADR 0027/0055). The guest's init shim RO-mounts it at
/// [`Self::guest_mount`] and reads the bundle's `mount.json` to learn which
/// skills/tools to wire; agentd wires them at `SpawnHarness` time.
///
/// ADR 0055 (uniform dynamic mounts): a base snapshot reserves a fixed pool
/// of slots ([`Self::RESERVED_SLOTS`], `dyn_0..dyn_{N-1}`), each carrying the
/// sentinel until a per-session create swaps the selected skill in via
/// `patch_drive` in the paused restore window. There is no longer a special
/// "skills" / "browser" drive — every mount is one content-addressed skill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuxRoDrive {
    /// Firecracker `drive_id`. For ADR 0055 dynamic mounts this is the
    /// reserved slot id (`"dyn-<i>"`, see [`Self::slot_drive_id`]). Stable
    /// across snapshot/restore — embedded in `state.bin`. The slot is the
    /// *device* identity; the *content* is carried by [`Self::sha256`].
    pub drive_id: String,
    /// Guest mount point the init shim mounts the drive at. For dynamic slots
    /// this is the slot-assigned [`Self::slot_guest_mount`]
    /// (`/opt/engram/dyn/<i>`); the mounted bundle's `mount.json` declares
    /// what to wire, so the path itself is generic (ADR 0055 §1/§7).
    pub guest_mount: PathBuf,
    /// Filesystem type for the guest mount (`"squashfs"` on both backends).
    pub fs_type: String,
    /// ADR 0035: content identity of the attached generation. `None` on the
    /// symbolic coord→host request ("attach whatever this host currently
    /// stages" — sentinel at capture, the selected skill at per-session
    /// swap); `Some` once resolved against the host's staged-bundle stamp.
    /// The host path is *derived* from `sha256` alone via [`Self::staged_path`]
    /// (ADR 0055: content-keyed, no `drive_id` prefix) — path and content can
    /// never disagree, which is the whole point: the 2026-06-03 incident was a
    /// snapshot re-anchoring a fixed path whose bytes a host roll had swapped.
    #[serde(default)]
    pub sha256: Option<String>,
}

impl AuxRoDrive {
    /// Fleet-canonical directory where mount generations are staged
    /// (`<sha256>.squashfs`, baked by the FC-host image or materialized from
    /// BlobStorage on demand). Identical on every host so a snapshot-embedded
    /// path re-anchors on restore.
    pub const SHARED_DIR: &'static str = "/var/lib/engram/shared";

    /// The bake-time stamp file mapping logical mount name → sha256 of the
    /// generation this host image carries. Written by the Packer provisioner;
    /// read by the FC backend to resolve symbolic mounts at capture and by the
    /// host-agent to report `current_bundles`.
    pub const CURRENT_STAMP: &'static str = "current.json";

    /// ADR 0055: stamp key (in `current.json`) for the sentinel squashfs — the
    /// tiny placeholder every reserved slot carries at capture. The guest reads
    /// its `mount.json` (`"kind":"sentinel"`) and skips it. All reserved slots
    /// resolve to this generation at base-snapshot capture; per-session creates
    /// `patch_drive` real skills over it in the paused restore window.
    pub const SENTINEL_STAMP_KEY: &'static str = "sentinel";

    /// ADR 0055: number of reserved dynamic-mount slots captured into every
    /// base snapshot (`dyn_0..dyn_{RESERVED_SLOTS-1}`), one skill per slot.
    /// Bounded by Firecracker's x86 **virtio-mmio** GSI pool — engrams boots
    /// `pci=off`, and the legacy interrupt range is `GSI_LEGACY_START=5 ..
    /// GSI_LEGACY_END=23` (19 lines), minus the baseline virtio devices
    /// (rootfs, net, vsock). That caps total aux drives around ~13, NOT the
    /// hundreds a `pci=on` MSI pool would allow — so this is a small, honest
    /// value, not a stress test. The exact ceiling is pinned by a dev-vm probe
    /// (attach N drives until boot/capture/restore breaks) before the base
    /// re-bake; 12 leaves headroom. A profile needing more skills than fit is
    /// the documented fallback to size-aware packing (ADR 0055 Alternatives).
    /// Each unused slot carries the sentinel; a per-session create
    /// `patch_drive`s the selected skill into a slot in the paused restore
    /// window.
    pub const RESERVED_SLOTS: usize = 12;

    /// ADR 0062: reserved slot index for the harness catalog. Slot 0 (`dyn_0`)
    /// carries the content-addressed catalog squashfs (every registered harness
    /// under `<name>/`); the session `exec`s `/opt/engram/dyn/0/<name>/<exec>`.
    /// Skills assign to `dyn_3..` (slot 1 is agentd, slot 2 is guest-tools —
    /// ADR 0080). The harness is `exec`'d, so unlike a skill its slot must be
    /// coordinator-known up front to build `argv[0]`.
    pub const HARNESS_SLOT_INDEX: usize = 0;

    /// ADR 0080: reserved slot index for the agentd bundle. Slot 1 (`dyn_1`)
    /// carries `engram-agentd` + its `agentd.sha256` content stamp. Unlike
    /// every other slot it must be **resolved at capture** (stamp key
    /// [`Self::AGENTD_STAMP_KEY`], not the sentinel): the capture VM's stage-1
    /// init copies agentd out of this mount to tmpfs and execs it — no agentd
    /// is baked into the rootfs. On a fresh-create restore the coordinator
    /// pins the fleet's current generation here (like the harness slot) and
    /// the host's post-resume `RefreshAgent` lets the captured agentd re-exec
    /// the swapped-in binary — that's how an agentd roll reaches new sessions
    /// with zero image recapture.
    pub const AGENTD_SLOT_INDEX: usize = 1;

    /// ADR 0080: stamp key (in `current.json`) for the agentd bundle
    /// generation this host stages.
    pub const AGENTD_STAMP_KEY: &'static str = "agentd";

    /// ADR 0080 §D: reserved slot index for the `guest-tools` bundle. Slot 2
    /// (`dyn_2`) carries engrams-owned in-guest tooling that no longer bakes
    /// into session images — today the static `ttyd` binary agentd's
    /// `shell.rs` lazily spawns for the SHELL tab. Unlike agentd it is NOT
    /// required to boot: capture resolves this slot to the sentinel like any
    /// skill slot, the coordinator pins the fleet's current generation on
    /// fresh creates (soft — a bundle-less fleet warns and falls back to an
    /// image-baked ttyd), and agentd probes the dyn mounts for the tool at
    /// StartShell time.
    pub const GUEST_TOOLS_SLOT_INDEX: usize = 2;

    /// ADR 0080 §D: stamp key (in `current.json`) for the guest-tools bundle
    /// generation this host stages.
    pub const GUEST_TOOLS_STAMP_KEY: &'static str = "guest-tools";

    /// ADR 0062/0080: skill slots start after the harness + agentd +
    /// guest-tools slots, so a session may carry at most this many skills.
    pub const MAX_SKILL_SLOTS: usize = Self::RESERVED_SLOTS - 3;

    /// ADR 0080: first reserved slot index skills may occupy (`dyn_3`).
    pub const FIRST_SKILL_SLOT_INDEX: usize = Self::GUEST_TOOLS_SLOT_INDEX + 1;

    /// Firecracker `drive_id` for reserved dynamic slot `i` (`"dyn_<i>"`).
    /// Underscore, NOT hyphen: FC rejects a `PUT /drives/<id>` whose id isn't
    /// alphanumeric-or-underscore with a 400 (the device-ceiling probe caught
    /// the original `dyn-0`).
    pub fn slot_drive_id(i: usize) -> String {
        format!("dyn_{i}")
    }

    /// Generic guest mount point for reserved dynamic slot `i`
    /// (`/opt/engram/dyn/<i>`). Slot-assigned rather than skill-declared so
    /// mounting stays uniform; agentd reads each mount's `mount.json` to learn
    /// what to wire (ADR 0055 §1/§7).
    pub fn slot_guest_mount(i: usize) -> PathBuf {
        PathBuf::from(format!("/opt/engram/dyn/{i}"))
    }

    /// Inverse of [`Self::slot_drive_id`]: the reserved slot index this drive
    /// targets, parsed from its `dyn_<i>` [`Self::drive_id`]. `None` if the id
    /// isn't a reserved-slot id.
    ///
    /// A backend whose *attach order* fixes the guest `dyn/<i>` index must sort
    /// by this. FC keeps `dyn/<i> == slot i` structurally (all `RESERVED_SLOTS`
    /// attached as sentinels at capture, in slot order, then `patch_drive`d by
    /// `drive_id`), so its `selected_mounts` order is irrelevant. VZ instead
    /// attaches only the *resolved* drives, compacting them onto `/dev/vdb..` in
    /// slice order — so it must attach slot-ascending, or the harness (slot 0,
    /// `exec`'d at the FIXED `/opt/engram/dyn/0/harness`) lands at the wrong
    /// `dyn/<i>` whenever skills (slots 1..) are also selected (ADR 0062).
    pub fn slot_index(&self) -> Option<usize> {
        self.drive_id
            .strip_prefix("dyn_")
            .and_then(|i| i.parse().ok())
    }

    /// A symbolic reserved slot for capture: slot `i` at its generic guest
    /// path, content unresolved (`sha256 = None`) until the FC backend stamps
    /// the sentinel (capture) or the selected skill (per-session swap).
    pub fn reserved_slot(i: usize) -> Self {
        Self {
            drive_id: Self::slot_drive_id(i),
            guest_mount: Self::slot_guest_mount(i),
            fs_type: "squashfs".into(),
            sha256: None,
        }
    }

    /// Staged filename for one mount generation. ADR 0055: keyed by **content**
    /// (`<sha256>.squashfs`), NOT the slot/`drive_id` — so a skill staged once
    /// is reused whatever slot a session swaps it into (per-skill dedup at the
    /// host staging layer; the same blob can't be duplicated per slot).
    pub fn staged_file_name(sha256: &str) -> String {
        format!("{sha256}.squashfs")
    }

    /// BlobStorage key for one mount generation (content-addressed; publish at
    /// registration/capture, materialize at restore, GC by pin set).
    pub fn blob_key(sha256: &str) -> String {
        format!("bundles/sha256/{sha256}")
    }
    // NOTE: there is deliberately no `staged_path()` here. A staged generation's
    // host path is `<bundle_dir>/<staged_file_name>`, and `bundle_dir` is per-host
    // (ENGRAM_BUNDLE_DIR / the SHARED_DIR default) — it lives on the backend, the
    // single source of truth (`SandboxBackend::bundle_dir`). Hardcoding SHARED_DIR
    // here is exactly the ADR 0062 trap; build the path from the backend's dir.
}

/// ADR 0035: identity of one bundle generation a snapshot references —
/// the GC pin unit. Stamped onto [`SnapshotMetadata`](super::snapshot::SnapshotMetadata)
/// by the host (base capture *and* eviction snapshots: an evicted VM's
/// device model still points at the same generation) and persisted by the
/// coord in `snapshots.aux_bundles`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuxBundleRef {
    pub drive_id: String,
    pub sha256: String,
}

/// Argv + env for the long-running "agent" process (Claude Code,
/// the dev noop harness, future adapters). Passed to
/// `SandboxBackend::start_agent` at session-bind time — *not*
/// stored on `SandboxSpec`, because the agent's argv is per-session
/// (`session_id`, attach token, etc.) while `SandboxSpec` is a
/// per-image template.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Argv. `argv[0]` must be reachable by the backend — for
    /// ProcessBackend this means an absolute host path; for
    /// Firecracker it's a path inside the rootfs.
    pub argv: Vec<String>,
    /// Harness-*only* extras, layered on top of [`Self::session_env`]
    /// for the harness child: the initial prompt, the harness dial
    /// address, the working-directory key, and the per-request forge
    /// broker token. These are deliberately NOT in `session_env` —
    /// they're either harness-specific or short-lived credentials, so
    /// they don't belong in the env every session process inherits.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// The durable session environment: the image manifest `[env]` +
    /// resolved secrets + `ENGRAM_SESSION_ID`. agentd holds this at
    /// bind (it rides the `SpawnHarness` frame) and applies it as the
    /// base env for *every* process it spawns — the harness, `/exec`
    /// commands, and the interactive shell — so "every way you run a
    /// command in a session sees the same environment" holds by
    /// construction. Populated for all modes, including the dev_vm
    /// readiness probe (empty argv) where no harness ever spawns but
    /// exec/shell still need it.
    #[serde(default)]
    pub session_env: HashMap<String, String>,
    /// ADR 0073: binding generation for the harness attach token,
    /// minted by the coordinator (one PG counter per session) at the
    /// moment it commits to this (re)bind — BEFORE the sandbox exists,
    /// which is why it rides the spec instead of the sandbox row. The
    /// backend stamps it (with the sandbox id it minted) into the
    /// harness child env; the hub's durable binding record is monotonic
    /// in it. Zero is never minted: a 0 here means a caller skipped the
    /// mint, and the attach will be rejected `UnknownBinding` — loud,
    /// per the no-silent-Ok rule.
    #[serde(default)]
    pub binding_epoch: u64,
    /// Per-host egress-proxy CA cert in PEM form (ADR 0021 P1).
    /// Populated by the host-agent *after* receiving the spec from
    /// coord, immediately before handing it to the sandbox backend —
    /// only the host knows its own CA. `None` skips the install — used
    /// by tests, dev backends, and any deploy without egress proxying.
    ///
    /// 2026-07 core-ops fold: this rides the guest-bound `SpawnHarness`
    /// wire frame (`engram_agentd::proto::SpawnHarnessRequest`) as a
    /// single first-contact RPC that both installs the CA and spawns
    /// the harness — there is no separate CA-install round trip
    /// anymore. Firecracker and VZ both wire it through (VZ used to
    /// silently drop this field); the Process backend is N/A — no
    /// agentd wire, no guest boundary, so it always sends `None`.
    ///
    /// **Wire format note**: this field intentionally does NOT carry
    /// `#[serde(skip_serializing_if = "Option::is_none")]`. AgentSpec
    /// crosses the coord ↔ host-agent gRPC boundary as bincode (see
    /// `engram-protocol/src/grpc_client.rs::start_agent`), and bincode
    /// is positional — skipping a field on encode breaks the decoder
    /// with "unexpected end of file" because it has no field names to
    /// look up. The `#[serde(default)]` covers the legacy-snapshot
    /// JSON path (manifest read of older sidecars that pre-date this
    /// field).
    #[serde(default)]
    pub host_ca_pem: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CpuLimit {
    pub vcpus: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MemoryLimit {
    pub max_mib: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DiskLimit {
    pub max_gib: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: HashMap<String, String>,
    pub workdir: Option<String>,
    pub timeout: Option<Duration>,
    /// Durable caller ticket. When present, retries attach to the existing
    /// guest journal instead of spawning a second command.
    #[serde(default)]
    pub exec_id: Option<String>,
    /// Per-stream replay cursors used when attaching to a durable exec.
    #[serde(default)]
    pub stdout_offset: Option<u64>,
    #[serde(default)]
    pub stderr_offset: Option<u64>,
    /// Ask the coordinator to wake a parked session before attaching.
    /// Backends receive the normalized request after that wake has happened.
    #[serde(default)]
    pub wake: Option<bool>,
}

/// One file to write into a running sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileSpec {
    pub path: String,
    pub content: Vec<u8>,
    /// Unix permission bits applied after writing. `None` leaves the
    /// platform-created permissions unchanged.
    pub mode: Option<u32>,
}

/// Per-file outcome from a batched [`SandboxBackend::write_files`](
/// crate::traits::SandboxBackend::write_files) operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteFileResult {
    pub path: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Output event from a streaming `exec`. A terminal backend result is marked
/// by exactly one `Exit` or `Refused`. The event stream may instead end
/// without either terminal to report retryable transport loss while a durable
/// result remains attachable.
///
/// The variants are intentionally `Bytes` rather than `String` so a
/// process emitting non-UTF-8 output (binary tools, raw pipe content)
/// flows through unmolested. Callers stringify lossily at the API edge.
#[derive(Clone, Debug)]
pub enum ExecEvent {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(Option<i32>),
    Refused(String),
}

impl ExecEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Exit(_) | Self::Refused(_))
    }
}

/// Boxed event stream. Owned (`'static`) so it can be moved into a
/// background task or returned across an `axum` handler boundary.
pub type ExecEventStream = Pin<Box<dyn Stream<Item = ExecEvent> + Send + 'static>>;

/// Streaming counterpart to [`ExecHandle`]. The backend returns immediately
/// with metadata + a stream; the stream yields output as the underlying
/// process produces it and ends with a single `Exit` or `Refused` only when
/// it has a terminal result. End-without-terminal means transport loss, not
/// completion.
pub struct ExecStream {
    pub sandbox_id: SandboxId,
    pub exec_id: String,
    pub events: ExecEventStream,
}

impl std::fmt::Debug for ExecStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecStream")
            .field("sandbox_id", &self.sandbox_id)
            .field("exec_id", &self.exec_id)
            .field("events", &"<Stream<Item = ExecEvent>>")
            .finish()
    }
}

/// Buffered counterpart to [`ExecStream`]. Returned by the default
/// `SandboxBackend::exec` which drains the stream into in-memory
/// buffers — fine for short commands, never use it for long-running
/// agent processes (use `exec_stream` instead).
#[derive(Debug)]
pub struct ExecHandle {
    pub sandbox_id: SandboxId,
    pub exec_id: String,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: Option<i32>,
}

/// Resource accounting for a single exec, surfaced on `ExecCompleted`
/// events and the sync `/exec` response.
///
/// `wall_ms` is the time the exec was actually running, measured by
/// the coordinator. Cheap to capture, available on every platform.
///
/// `peak_rss_kb`, `user_cpu_ms`, `sys_cpu_ms` are reserved for
/// backend-supplied data (Firecracker's `GET /metrics`, Linux cgroups,
/// libvirt rusage, etc.). The dev `ProcessBackend` on macOS doesn't
/// have a clean way to capture them without unsafe FFI, so they're
/// `None` there. They land for real with the Phase 2 Firecracker
/// integration, which exposes per-VM CPU + memory metrics natively.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct ExecRusage {
    pub wall_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_rss_kb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_cpu_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sys_cpu_ms: Option<u64>,
}

/// ADR 0073: env key carrying the sandbox identity half of the harness
/// attach token. Stamped by the BACKEND at spawn (the only party that
/// knows the sandbox id pre-boot); read by the harness; presented in
/// `HarnessAttach`.
pub const SANDBOX_ID_ENV: &str = "ENGRAM_SANDBOX_ID";

/// ADR 0073: env key carrying the binding-generation half of the attach
/// token. Minted coordinator-side into [`AgentSpec::binding_epoch`];
/// stamped into the harness child env by the backend at spawn.
pub const BINDING_EPOCH_ENV: &str = "ENGRAM_BINDING_EPOCH";

impl AgentSpec {
    /// The two attach-token env entries a backend must add to the
    /// harness child env at spawn (ADR 0073). Kept as a helper so the
    /// three backends cannot drift on key names or formatting.
    pub fn attach_token_env(&self, sandbox_id: crate::SandboxId) -> [(String, String); 2] {
        [
            (SANDBOX_ID_ENV.to_string(), sandbox_id.to_string()),
            (
                BINDING_EPOCH_ENV.to_string(),
                self.binding_epoch.to_string(),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the bincode/serde footgun PR #40 ran into:
    /// `AgentSpec` crosses the coord ↔ host-agent gRPC boundary as
    /// bincode, and bincode is positional — any field marked
    /// `#[serde(skip_serializing_if = "Option::is_none")]` makes the
    /// encoder emit a shorter buffer when that field is `None`, and
    /// the decoder hits "unexpected end of file" when it tries to
    /// read the missing tag. The test pins the round-trip for both
    /// `None` and `Some` host_ca_pem so a future "let's clean up the
    /// JSON shape" change can't silently break the wire.
    #[test]
    fn agent_spec_bincode_roundtrips_with_none_and_some_host_ca_pem() {
        let none = AgentSpec {
            binding_epoch: 0,
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env: HashMap::from_iter([("FOO".into(), "bar".into())]),
            session_env: HashMap::from_iter([("RUSTC_WRAPPER".into(), "sccache".into())]),
            host_ca_pem: None,
        };
        let bytes = bincode::serialize(&none).expect("bincode encode None");
        let back: AgentSpec = bincode::deserialize(&bytes).expect("bincode decode None");
        assert_eq!(back.argv, none.argv);
        assert_eq!(back.env, none.env);
        assert_eq!(back.session_env, none.session_env);
        assert!(back.host_ca_pem.is_none());

        let some = AgentSpec {
            binding_epoch: 0,
            argv: vec!["/opt/engram/harness/harness".into()],
            env: HashMap::new(),
            session_env: HashMap::new(),
            host_ca_pem: Some(
                "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".into(),
            ),
        };
        let bytes = bincode::serialize(&some).expect("bincode encode Some");
        let back: AgentSpec = bincode::deserialize(&bytes).expect("bincode decode Some");
        assert_eq!(back.host_ca_pem, some.host_ca_pem);
    }

    /// `slot_index` is the inverse of `slot_drive_id` for every reserved slot,
    /// and rejects non-slot ids. VZ's slot-ordered attach relies on this to put
    /// the harness (slot 0) at `dyn/0` (ADR 0062).
    #[test]
    fn slot_index_round_trips_slot_drive_id() {
        for i in 0..AuxRoDrive::RESERVED_SLOTS {
            let drive = AuxRoDrive::reserved_slot(i);
            assert_eq!(drive.slot_index(), Some(i), "slot {i} round-trip");
        }
        // A non-slot drive_id (e.g. the ext4 CA drive is never an AuxRoDrive,
        // but be defensive) yields None rather than a bogus slot.
        let bogus = AuxRoDrive {
            drive_id: "rootfs".into(),
            guest_mount: PathBuf::from("/"),
            fs_type: "ext4".into(),
            sha256: None,
        };
        assert_eq!(bogus.slot_index(), None);
    }

    fn spec_with_aux_drives(drives: Vec<AuxRoDrive>) -> SandboxSpec {
        SandboxSpec {
            image: "warm-1".into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 2 },
            memory: MemoryLimit { max_mib: 4096 },
            disk: DiskLimit { max_gib: 8 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: NetworkPolicy::default(),
            aux_ro_drives: drives,
            swap_mib: None,
        }
    }

    /// `SandboxSpec` crosses the coord ↔ host-agent boundary as bincode
    /// (positional) and is persisted as JSON in snapshot sidecars. Pin
    /// both round-trips for the ADR-0027 `aux_ro_drives` field so a
    /// future wire change can't silently break either path.
    #[test]
    fn sandbox_spec_aux_ro_drives_round_trip_bincode_and_json() {
        let spec = spec_with_aux_drives(vec![
            // Symbolic (sentinel reserved slot, coord request) and resolved
            // (a per-session swapped-in skill) forms both cross the wire — pin both.
            AuxRoDrive::reserved_slot(0),
            AuxRoDrive {
                sha256: Some("a".repeat(64)),
                ..AuxRoDrive::reserved_slot(1)
            },
        ]);

        let bytes = bincode::serialize(&spec).expect("bincode encode");
        let back: SandboxSpec = bincode::deserialize(&bytes).expect("bincode decode");
        assert_eq!(back.aux_ro_drives, spec.aux_ro_drives);

        let json = serde_json::to_string(&spec).expect("json encode");
        let back: SandboxSpec = serde_json::from_str(&json).expect("json decode");
        assert_eq!(back.aux_ro_drives, spec.aux_ro_drives);

        // ADR 0112: `swap_mib` rides both encodings (wire v25). Pin the
        // `Some` form — `None` is covered by every other literal here.
        let swap_spec = SandboxSpec {
            swap_mib: Some(6144),
            ..spec_with_aux_drives(vec![])
        };
        let bytes = bincode::serialize(&swap_spec).expect("bincode encode swap");
        let back: SandboxSpec = bincode::deserialize(&bytes).expect("bincode decode swap");
        assert_eq!(back.swap_mib, Some(6144));
        let json = serde_json::to_string(&swap_spec).expect("json encode swap");
        let back: SandboxSpec = serde_json::from_str(&json).expect("json decode swap");
        assert_eq!(back.swap_mib, Some(6144));

        // Empty is the common case (no bundles attached) — must encode as
        // a zero-length vec and decode cleanly.
        let bytes =
            bincode::serialize(&spec_with_aux_drives(vec![])).expect("bincode encode empty");
        let back: SandboxSpec = bincode::deserialize(&bytes).expect("bincode decode empty");
        assert!(back.aux_ro_drives.is_empty());
    }

    /// `#[serde(default)]` lets a legacy snapshot sidecar that predates
    /// `aux_ro_drives` decode (JSON path) — the field comes back empty.
    #[test]
    fn sandbox_spec_legacy_json_without_aux_ro_drives_defaults_empty() {
        let legacy = r#"{
            "image": "warm-1",
            "rootfs_source": null,
            "image_uri": null,
            "cpu": { "vcpus": 2 },
            "memory": { "max_mib": 4096 },
            "disk": { "max_gib": 8 },
            "ttl": null,
            "env": {},
            "workdir": null
        }"#;
        let spec: SandboxSpec = serde_json::from_str(legacy).expect("legacy json decode");
        assert!(spec.aux_ro_drives.is_empty());
        // ADR 0112: pre-swap sidecars decode with no swap device.
        assert_eq!(spec.swap_mib, None);
    }

    #[test]
    fn exec_event_terminal_only_on_exit_or_refusal() {
        assert!(!ExecEvent::Stdout(Bytes::from_static(b"x")).is_terminal());
        assert!(!ExecEvent::Stderr(Bytes::from_static(b"x")).is_terminal());
        assert!(ExecEvent::Exit(Some(0)).is_terminal());
        assert!(ExecEvent::Exit(None).is_terminal());
        assert!(ExecEvent::Refused("first writer wins".into()).is_terminal());
    }
}
