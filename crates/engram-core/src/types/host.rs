use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::HostId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostStatus {
    Ready,
    Draining,
    Dead,
    Retired,
}

impl HostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Dead => "dead",
            Self::Retired => "retired",
        }
    }

    /// The legal host-lifecycle edge table — the host-state analogue of
    /// [`crate::types::SessionState::can_transition_to`]. Host status is
    /// reported two ways (the agent's self-reported `draining` shutdown
    /// flag, and the coordinator's dead-host sweep), so unlike session
    /// state it isn't gated by a single CAS writer; this predicate is the
    /// guard that keeps the writers honest where it matters.
    ///
    /// ```text
    /// Ready    -> Draining (agent preStop / coordinator drive)
    ///           | Dead     (dead-host sweep)
    ///           | Retired  (retirement grant)
    /// Draining -> Ready    (host came back from a self-reported drain)
    ///           | Dead     (dead-host sweep)
    ///           | Retired  (retirement grant)
    /// Dead     -> (terminal via heartbeat) — a `dead` row returns ONLY
    ///             via an explicit `POST /api/hosts/register`
    ///             (`upsert_host`, which hardcodes `Ready`), never on the
    ///             host's next heartbeat. A merely-partitioned host that
    ///             the sweep marked `dead` (and whose sessions it
    ///             orphaned) must NOT silently flip back to `ready` and
    ///             resume taking placements with its sessions unbound.
    /// Retired  -> terminal; only DeleteHost removes the row.
    /// ```
    ///
    /// Self-transitions return `false`: a no-op status write carries no
    /// new information and is almost certainly a racing writer.
    pub const fn can_transition_to(&self, target: Self) -> bool {
        use HostStatus::*;
        match self {
            Ready => matches!(target, Draining | Dead | Retired),
            Draining => matches!(target, Ready | Dead | Retired),
            Dead | Retired => false,
        }
    }
}

/// Static identification info reported by a host's CloudBackend.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostMetadata {
    pub instance_id: String,
    pub zone: String,
    pub machine_type: String,
    /// Free-form per-cloud blob; persisted to `hosts.cloud_metadata`.
    pub extra: serde_json::Value,
}

/// Capacity and freshness reported via heartbeat.
///
/// Two precisions live here side by side. The `*_mib` + `running_sandboxes`
/// trio are the source of truth — what the host-agent's heartbeat
/// actually reports and what the API surfaces. The `*_gb` fields are
/// legacy holdovers from the original row schema and are set to zero
/// by current code paths; they'll be dropped in a follow-up once no
/// downstream consumer reads them.
///
/// MiB precision is load-bearing for the SPA's "X.X / Y.Y GiB"
/// display — at GB granularity the readout would visibly round (a
/// 31.4 GiB host shows up as "31 GiB"). Persisting MiB on every
/// heartbeat is also what makes `/api/hosts` consistent across coord
/// replicas: the in-memory `host_registry` only knows about hosts
/// whose WS connected to *this* pod, so a pod fielding the API
/// request for a host owned by a sibling pod falls back to the row
/// from Postgres. Without persisted MiB fields, that fallback gave
/// zero capacity and the UI flashed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostCapacity {
    #[serde(default)]
    pub total_gb: u32,
    #[serde(default)]
    pub used_gb: u32,
    #[serde(default)]
    pub total_mib: u64,
    #[serde(default)]
    pub used_mib: u64,
    #[serde(default)]
    pub running_sandboxes: u32,
}

/// The host chunk-cache disk floor (bytes): free work_dir space below
/// which the host is treated as disk-pressured (issue #528). 20 GiB is the
/// margin under which the host-agent's idle-evict stops pushing
/// candidates. The SINGLE owner of this value: the host-agent's
/// `DEFAULT_DISK_FLOOR_BYTES` aliases it, and ADR 0078's placement
/// affinity vetoes a snapshot-host whose free disk is below it (placing
/// a resume on a host about to disk-evict its cache is pointless). Do not
/// mint a second threshold.
pub const HOST_DISK_CACHE_FLOOR_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// [`HOST_DISK_CACHE_FLOOR_BYTES`] in MiB, for comparison against
/// [`HostUtilization`]'s MiB disk fields on the coordinator placement
/// path.
pub const HOST_DISK_CACHE_FLOOR_MIB: u64 = HOST_DISK_CACHE_FLOOR_BYTES / (1024 * 1024);

/// Placement stops choosing a host whose session-disk pool has less data
/// space free than this. A full data pool queues guest I/O instead of
/// failing it, so the floor keeps the resident sessions running.
pub const HOST_POOL_DATA_FLOOR_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const HOST_POOL_DATA_FLOOR_MIB: u64 = HOST_POOL_DATA_FLOOR_BYTES / (1024 * 1024);

/// The pool data floor in effect: `ENGRAM_PLACEMENT_POOL_FLOOR_BYTES`,
/// else [`HOST_POOL_DATA_FLOOR_BYTES`]. The coordinator places against
/// it and a host reaps kept checkpoint volumes to stay above twice it.
pub fn pool_data_floor_bytes() -> u64 {
    static FLOOR: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FLOOR.get_or_init(|| {
        floor_bytes_from_env(
            "ENGRAM_PLACEMENT_POOL_FLOOR_BYTES",
            HOST_POOL_DATA_FLOOR_BYTES,
        )
    })
}

/// A byte count from `var`, or `default` when unset. A value that does
/// not parse is a configuration error, not a default.
pub fn floor_bytes_from_env(var: &str, default: u64) -> u64 {
    match std::env::var(var) {
        Ok(s) => s
            .parse()
            .unwrap_or_else(|e| panic!("{var}={s:?} is not a byte count: {e}")),
        Err(_) => default,
    }
}

/// Placement stops choosing a host whose pool metadata has less space
/// free than this. Metadata is sized so this never trips; the heartbeat
/// reports it so an alert fires first.
pub const HOST_POOL_METADATA_FLOOR_BYTES: u64 = 64 * 1024 * 1024;
pub const HOST_POOL_METADATA_FLOOR_MIB: u64 = HOST_POOL_METADATA_FLOOR_BYTES / (1024 * 1024);

/// Observed host resource utilization, sampled fresh on every
/// heartbeat. Distinct from [`HostCapacity`], which is the
/// *reservation* model the scheduler reasons about (committed guest
/// RAM); this is what the host is *actually* using right now — the
/// signal the operator-facing fleet view renders. Disk is the one
/// that bites in practice (the chunk cache + memory dumps fill the
/// work_dir mount), so it leads.
///
/// All fields default to 0, so a heartbeat from a host running an
/// older build (no probe) deserializes cleanly to "unknown" and the
/// UI renders an empty bar rather than failing.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostUtilization {
    /// Free / total MiB of the session-disk pool's data device. A zero
    /// total means the host has no pool (sparse-file volumes) or has not
    /// reported one; placement treats it as unmeasured.
    #[serde(default)]
    pub pool_data_free_mib: u64,
    #[serde(default)]
    pub pool_data_total_mib: u64,
    /// Free / total MiB of the pool's metadata device.
    #[serde(default)]
    pub pool_metadata_free_mib: u64,
    #[serde(default)]
    pub pool_metadata_total_mib: u64,
    /// Total / used bytes (MiB) of the host's work_dir filesystem —
    /// the mount that holds the chunk cache, jails, and FC memory
    /// dumps. `statvfs(2)`; available on Linux and macOS.
    #[serde(default)]
    pub disk_total_mib: u64,
    #[serde(default)]
    pub disk_used_mib: u64,
    /// Physical RAM: MemTotal and (MemTotal − MemAvailable) from
    /// `/proc/meminfo`. Zero on non-Linux (no `/proc`).
    #[serde(default)]
    pub mem_total_mib: u64,
    #[serde(default)]
    pub mem_used_mib: u64,
    /// ADR 0046, amended by issue #540 (the host RAM ledger): memory (MiB)
    /// actually available to place NEW sessions on this host —
    /// `MemAvailable + Σ PSS of reservation-backed (non-parked) VMs −
    /// pending base-shm charges`. `MemAvailable` nets out the host daemon,
    /// OS, kube-system pods, the chunk cache, and populated base-shm tmpfs
    /// (shmem isn't kernel-reclaimable) automatically; adding back only
    /// non-parked VM PSS — never parked-resident PSS — is what keeps a
    /// parked-but-RAM-resident sandbox (epic-parking-ladder rungs 2-3) from
    /// being double-counted as both occupied and free. `0` on non-Linux /
    /// pre-0058 hosts, where placement falls back to the raw
    /// `mem_total_mib`. Derived from `RamLedgerSnapshot::allocatable_mib`
    /// in `engram-host-agent::ram_ledger` (issue #540) — see that module
    /// for the full ledger.
    #[serde(default)]
    pub allocatable_mib: u64,
    /// Whole-host CPU utilization in percent (0–100), computed from
    /// the `/proc/stat` aggregate-cpu delta across the heartbeat
    /// interval. Zero on non-Linux or on the first tick (no prior
    /// sample to diff against).
    #[serde(default)]
    pub cpu_pct: f32,
    /// Issue #540: measured (`st_blocks`) bytes (MiB) resident on the
    /// per-image base-shm tmpfs — attribution the pre-ledger formula had
    /// none of (it netted these bytes out of `MemAvailable` silently).
    /// `0` on non-Linux or when no image has ever prewarmed.
    #[serde(default)]
    pub base_shm_mib: u64,
    /// Issue #540: registered-but-not-yet-materialized base-shm prewarm
    /// charges — bytes `image_prefetch` has promised to write but hasn't
    /// finished writing (or the tmpfs scan hasn't caught up to) yet.
    /// Already subtracted out of `allocatable_mib`; broken out here so an
    /// operator can see WHY allocatable dipped during an enable.
    #[serde(default)]
    pub base_shm_pending_mib: u64,
    /// Issue #540: Σ PSS of sandboxes flagged `parked` — RAM-resident but
    /// reservation-free (epic-parking-ladder rungs 2-3; `0` until the
    /// ladder lands). Never folded into `allocatable_mib`; this is the
    /// seam a later reclaim-under-pressure feature reads.
    #[serde(default)]
    pub parked_pss_mib: u64,
    /// Issue #540: Σ PSS of sandboxes NOT flagged `parked` — the same
    /// figure already added back into `allocatable_mib`, broken out for
    /// attribution/dashboards.
    #[serde(default)]
    pub running_pss_mib: u64,
}

/// ADR 0068: the outcome of a single self-verified host capability probe.
/// `Unknown` is the wire default so an old host-agent's register/heartbeat
/// body (missing this field entirely) deserializes to the same soft
/// posture a `schema == 0` [`HostCapabilities`] gets — see
/// `crate::placement`-side gating (host_meets_capabilities), which lives in
/// `engram-coordinator` since this crate has no scheduler.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
pub enum CapStatus {
    /// Absent / pre-roll host-agent — never probed. Soft-tolerated, the
    /// same posture as `wire_version == 0`.
    #[default]
    Unknown,
    /// The probe ran and passed. `detail` carries free-form context
    /// (e.g. `session volumes`) for operator display; never load-bearing.
    Ok(Option<String>),
    /// The probe ran and failed. `detail` is the errno/message.
    Failed(String),
    /// This backend never has this capability (e.g. VZ has no
    /// uffd/base_shm substrate) — distinct from `Failed` so the fleet
    /// view doesn't render a VZ host's substrate row as broken.
    NotApplicable,
}

impl CapStatus {
    /// A capability the placement gate can rely on for a *required*
    /// dimension. `Failed`, `Unknown`, and `NotApplicable` all fail a
    /// required capability — only a probe that actually ran and passed
    /// clears the gate.
    pub fn is_ok(&self) -> bool {
        matches!(self, CapStatus::Ok(_))
    }
}

/// ADR 0068: typed, self-verified host readiness. Probed by the
/// host-agent at startup (before the first register) and re-asserted on
/// every heartbeat; persisted as JSONB on the `hosts` row (migration
/// 0080) and carried as JSON in the register/heartbeat bodies. The
/// host<->coord control plane is HTTP/JSON, so every field is
/// `#[serde(default)]` — an old host-agent's payload (missing this
/// struct, or missing individual fields within it) decodes to
/// `schema == 0` / `CapStatus::Unknown`, the same soft posture
/// `wire_version == 0` gets today. That's what gives a rolling deploy
/// (coord first, then the host MIG) mixed-fleet interop for free.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HostCapabilities {
    /// `0` = never reported (pre-roll host-agent, or a payload with no
    /// `capabilities` field at all); `>= 1` = a probed vector. Gate
    /// semantics live in `engram-coordinator::placement`.
    #[serde(default)]
    pub schema: u32,
    /// `"firecracker" | "vz" | "process"` — selected from the same arm
    /// `main.rs` picks the `SandboxBackend` in.
    #[serde(default)]
    pub backend: String,
    /// A single TCP self-connect to the host-agent's own gRPC listen
    /// port proved it's bound + backlogging.
    #[serde(default)]
    pub grpc_self_connect: CapStatus,
    /// `statfs(TMPFS_MAGIC)` on the UFFD base-shm dir (ADR 0045
    /// substrate). `NotApplicable` off the FC backend.
    #[serde(default)]
    pub base_shm_tmpfs: CapStatus,
    /// A userfaultfd + `UFFDIO_REGISTER` MINOR self-test (memfd-backed,
    /// no guest involved). `NotApplicable` off the FC backend.
    #[serde(default)]
    pub uffd_minor_shmem: CapStatus,
    /// The host owns a session-disk pool. `NotApplicable` off the FC
    /// backend.
    #[serde(default)]
    pub session_disk: CapStatus,
    /// Idle-page bitmap access. Informational; never a placement requirement.
    #[serde(default)]
    pub page_idle: CapStatus,
    /// `bundles::read_stamp(bundle_dir)` returned a non-empty stamp —
    /// the host has *some* current RO-bundle generation staged.
    #[serde(default)]
    pub bundle_stamp: CapStatus,
    /// `firecracker --snapshot-version` output (e.g. `"v10.0.0"`),
    /// probed once at startup and cached. `None` off the FC backend.
    #[serde(default)]
    pub fc_snapshot_version: Option<String>,
    /// Mirror of `engram_protocol::WIRE_VERSION`, carried inside the
    /// vector too so a one-glance fleet-view render doesn't need a
    /// second lookup. The authoritative skew gate stays
    /// `host_wire_version_ok` against `HostRecord::wire_version`.
    #[serde(default)]
    pub wire_version: u32,
    /// Mirror of `engram_protocol::REATTACH_GENERATION`: which builds can
    /// adopt this host's running VMs. `0` for a build that predates it.
    #[serde(default)]
    pub reattach_generation: u32,
}

impl HostCapabilities {
    /// ADR 0068 fleet-view surface: names of the capabilities that are
    /// NOT `Ok` — `Failed` always counts; `Unknown` counts only once
    /// the host has reported a real vector (`schema >= 1`), since an
    /// `Unknown` at `schema == 0` just means "hasn't reported yet,"
    /// not "probed and something's wrong." `NotApplicable` never
    /// counts — it's the correct steady state for e.g. `session_disk` on a VZ
    /// host, not a failure. Fixed field order (not alphabetical) for
    /// deterministic rendering.
    pub fn failing_capabilities(&self) -> Vec<&'static str> {
        let is_failing = |c: &CapStatus| -> bool {
            match c {
                CapStatus::Failed(_) => true,
                CapStatus::Unknown => self.schema >= 1,
                CapStatus::Ok(_) | CapStatus::NotApplicable => false,
            }
        };
        let mut out = Vec::new();
        if is_failing(&self.grpc_self_connect) {
            out.push("grpc_self_connect");
        }
        if is_failing(&self.base_shm_tmpfs) {
            out.push("base_shm_tmpfs");
        }
        if is_failing(&self.uffd_minor_shmem) {
            out.push("uffd_minor_shmem");
        }
        if is_failing(&self.session_disk) {
            out.push("session_disk");
        }
        if matches!(self.page_idle, CapStatus::Failed(_)) {
            out.push("page_idle");
        }
        if is_failing(&self.bundle_stamp) {
            out.push("bundle_stamp");
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostRecord {
    pub id: HostId,
    pub hostname: String,
    pub cloud_metadata: HostMetadata,
    pub capacity: HostCapacity,
    /// Observed disk/mem/cpu utilization from the latest heartbeat.
    /// Persisted to the `hosts` row (migration 0056) so `/api/hosts`
    /// reads stay consistent across coord replicas, same as
    /// [`HostCapacity`]. `#[serde(default)]` for pre-0056 rows.
    #[serde(default)]
    pub utilization: HostUtilization,
    pub status: HostStatus,
    pub last_heartbeat_at: DateTime<Utc>,
    /// ADR 0013: gRPC dial address (e.g. `http://10.10.0.42:9101`)
    /// reported via `POST /api/hosts/register`. `None` for pre-0013
    /// rows or hosts that haven't re-registered since the migration —
    /// the coord's `GrpcHostPool` treats those as unreachable.
    #[serde(default)]
    pub host_addr: Option<String>,
    /// ADR 0047: manifest digests of images this host has fully
    /// prefetched (heartbeat-persisted; migration 0060). The placement
    /// readiness gate reads these — from PG, so every coordinator
    /// replica schedules from the same authority.
    #[serde(default)]
    pub ready_images: Vec<String>,
    /// ADR 0035/0047: the host's current bundle bake stamp
    /// (heartbeat-persisted). Operator visibility into fleet skew.
    #[serde(default)]
    pub current_bundles: Vec<super::sandbox::AuxBundleRef>,
    /// ADR 0035 amendment D2: per-running-sandbox aux bundle attachments
    /// (heartbeat-persisted; migration 0113). `bundle_pin_set` unions
    /// these so a live-but-unsnapshotted sandbox pins its generations
    /// against the host sweep and the bundle GC. `#[serde(default)]`
    /// for pre-0113 rows and mid-roll hosts (the GC grace period
    /// covers the unreported window).
    #[serde(default)]
    pub sandbox_bundles: Vec<super::sandbox::SandboxAuxBundles>,
    /// ADR 0047: coordinator-owned cordon bit. Written only by the
    /// admin cordon/uncordon endpoints (and the ADR 0048 wave driver);
    /// heartbeats never touch it, so it can't be clobbered back to
    /// schedulable mid-drain. Effective schedulability =
    /// `status == Ready && !cordoned && fresh`.
    #[serde(default)]
    pub cordoned: bool,
    pub cordon_owner: Option<CordonOwner>,
    pub cordon_reason: Option<String>,
    pub retire_requested_at: Option<DateTime<Utc>>,
    pub retired_at: Option<DateTime<Utc>>,
    /// ADR 0048: host core count from the heartbeat. The CPU packing
    /// budget is `total_vcpus × overcommit`. 0 = not yet reported.
    #[serde(default)]
    pub total_vcpus: u32,
    /// Issue #229: the host-agent's bincode `engram_protocol::WIRE_VERSION`,
    /// reported on every heartbeat (migration 0066). The placement filter
    /// excludes a host whose version is both NONZERO and != the
    /// coordinator's, turning a non-atomic rolling deploy into a graceful
    /// drain instead of a stream of 400 decode errors. `0` = not yet
    /// reported (a just-registered host before its first heartbeat, or a
    /// pre-0066 row) and is tolerated — soft, like an unmeasured
    /// allocatable.
    #[serde(default)]
    pub wire_version: u32,
    /// ADR 0036 amendment (issue #538): true iff this host-agent runs the
    /// image-prefetch supervisor (`chunk_store` + `chunk_cache` configured —
    /// production/VZ hosts; Process-backend dev hosts lack both and never
    /// spawn it). The enable scanner's prestage stage waits only on hosts
    /// with this bit; a fleet with zero eligible staging hosts passes the
    /// stage vacuously. `#[serde(default)]` → `false` for pre-migration rows
    /// (the exempt, safe posture). Migration 0081.
    #[serde(default)]
    pub stages_images: bool,
    /// ADR 0068: the host's self-verified capability vector, from the
    /// most recent register/heartbeat (migration 0080). `schema == 0`
    /// for pre-0068 rows / hosts mid-roll — soft-tolerated by the
    /// placement gate, same posture as `wire_version == 0`.
    #[serde(default)]
    pub capabilities: HostCapabilities,
    /// ADR 0116 A-D1: the binding-lease deadline (migration 0115).
    /// Write side (`upsert_host`): the register-time renewal target
    /// (`now + LEASE_TTL`), computed by the handler from the injected
    /// clock — the store REPLACES the stored deadline with it
    /// (successor presence ends a handoff early). Read side: the
    /// current deadline. `None` = no renewal observed since the
    /// migration; enforcement falls back to
    /// `last_heartbeat_at + LEASE_TTL`.
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// ADR 0116 A-D1: lease lifecycle state. Read-only on this struct —
    /// `upsert_host` forces `Active`, `begin_host_handoff` sets
    /// `Handoff`, heartbeats never demote a handoff.
    #[serde(default)]
    pub lease_state: HostLeaseState,
    /// ADR 0116 A-D1: host-agent generation, bumped by every register.
    /// Read-only on this struct (the store owns the bump).
    #[serde(default)]
    pub lease_epoch: u64,
}

/// Whether a register lifts the host's cordon (ADR 0126). Only an upgrade
/// across a reattach generation raises the generation a host reports, and
/// that upgrade's drain placed the cordon.
pub fn register_lifts_cordon(stored: &HostRecord, reported: &HostCapabilities) -> bool {
    stored.cordoned && reported.reattach_generation > stored.capabilities.reattach_generation
}

/// ADR 0116 A-D2: hard ceiling on a declared handoff TTL, shared by the
/// coordinator's clamp and the operator's own request sizing so the two
/// can never disagree (#1218 review: a 1 h coordinator clamp silently
/// truncated the operator's default ~102 min roll budget — the deadline
/// could not cover exactly the long enable-work rolls it exists for).
///
/// The asymmetry that sizes it: a too-LONG shield on a genuinely dead
/// host delays orphaning by at most this ceiling (bounded, recoverable —
/// expiry still fires); a too-SHORT shield lets the dead-host path
/// strike a mid-roll host and destroy healthy VMs (the 2026-08-12
/// incident class). So the ceiling errs long: 24 h dominates any sane
/// roll budget while still bounding how long a buggy declaration can
/// wedge detection.
pub const MAX_HANDOFF_TTL_SECS: u64 = 86_400;

/// ADR 0116 A-D1: the host binding-lease lifecycle (migration 0115).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostLeaseState {
    /// Pre-migration row / no renewal observed yet.
    #[default]
    None,
    /// Heartbeat-sustained.
    Active,
    /// A planned operation declared a successor deadline; replaced by
    /// `Active` on the successor's register, or consumed by expiry.
    Handoff,
}

impl HostLeaseState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Active => "active",
            Self::Handoff => "handoff",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "active" => Some(Self::Active),
            "handoff" => Some(Self::Handoff),
            _ => None,
        }
    }
}

/// ADR 0047: everything a heartbeat persists, in one struct — the
/// argument to `MetadataStore::touch_host_heartbeat`, which is the
/// single per-heartbeat `hosts` UPDATE. `status` is the HOST-reported
/// side (`draining` = the agent's own shutdown/preStop flag);
/// `cordoned` deliberately has no field here.
#[derive(Clone, Debug)]
pub struct HostHeartbeat {
    pub status: HostStatus,
    pub capacity: HostCapacity,
    pub utilization: HostUtilization,
    pub ready_images: Vec<String>,
    pub current_bundles: Vec<super::sandbox::AuxBundleRef>,
    /// ADR 0035 amendment D2: aux bundle refs attached to each running sandbox
    /// this tick — see [`HostRecord::sandbox_bundles`].
    pub sandbox_bundles: Vec<super::sandbox::SandboxAuxBundles>,
    pub total_vcpus: u32,
    /// Issue #229: the host-agent's bincode `WIRE_VERSION` this tick.
    pub wire_version: u32,
    /// ADR 0036 amendment (issue #538): whether this host's image-prefetch
    /// supervisor is spawned — see [`HostRecord::stages_images`].
    pub stages_images: bool,
    /// ADR 0068: this tick's re-probed capability vector.
    pub capabilities: HostCapabilities,
    /// ADR 0116 A-D3: the lease renewal target for this tick
    /// (`now + LEASE_TTL`, computed by the handler from the injected
    /// clock — same single-clock posture as `HostRecord::
    /// last_heartbeat_at`). The store applies it with GREATEST
    /// semantics: a renewal never shrinks a longer (handoff) deadline,
    /// and a heartbeat never demotes `Handoff` to `Active`. `None`
    /// skips renewal entirely (legacy/mock callers); the production
    /// handler always passes `Some` — the coordinator renews on every
    /// heartbeat regardless of host-agent version.
    pub lease_renew_until: Option<DateTime<Utc>>,
}

/// Per-host reserved memory and CPU: Σ of `mem_budget_mib` and
/// `cpu_budget_vcpus` over the sessions in a memory-reserving state (a
/// parked VM included), the capturing VMs and the reserving teleport legs
/// on the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReservedBudget {
    pub mem_mib: i64,
    pub vcpus: i64,
}

/// One host's resource accounting: what the host can hold, what the
/// coordinator has reserved on it, and what is left. Every decision that
/// asks "is there room on this host" reads it: placement (create, resume,
/// queue, capture, teleport, drain guard), parking, and the fleet's
/// autoscaling signal.
///
/// The host measures `allocatable_mib`: the RAM a reservation may use,
/// with every resident VM's memory added back, so a VM's RAM counts once,
/// in `reserved`. The coordinator owns `reserved`. Free is the
/// difference: a parked VM keeps its reservation until it descends, so
/// it is not free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostHeadroom {
    /// Host-measured `allocatable_mib`; `<= 0` means unmeasured (a host
    /// that has not sampled its RAM yet).
    pub allocatable_mib: i64,
    /// `total_vcpus × overcommit` ([`host_cpu_budget`]); `0` means the
    /// host has not reported its cores.
    pub cpu_budget: i64,
    pub reserved: ReservedBudget,
}

impl HostHeadroom {
    pub fn of(host: &HostRecord, reserved: ReservedBudget) -> Self {
        Self {
            allocatable_mib: host.utilization.allocatable_mib as i64,
            cpu_budget: host_cpu_budget(host.total_vcpus),
            reserved,
        }
    }

    /// Whether the host has measured its RAM. Unmeasured RAM does not
    /// gate placement; a reported CPU budget still gates. A host with
    /// unmeasured RAM never parks a session.
    pub fn measured(&self) -> bool {
        self.allocatable_mib > 0
    }

    /// Unreserved RAM. Only meaningful when [`Self::measured`].
    pub fn free_mib(&self) -> i64 {
        self.allocatable_mib - self.reserved.mem_mib
    }

    /// Unreserved vCPU budget; `None` when the host reports no cores.
    pub fn free_vcpus(&self) -> Option<i64> {
        (self.cpu_budget > 0).then(|| self.cpu_budget - self.reserved.vcpus)
    }

    /// Whether a reservation of `mem_mib` and `vcpus` fits now. An
    /// unmeasured RAM figure or an unreported core count does not gate.
    pub fn fits(&self, mem_mib: i64, vcpus: i64) -> bool {
        (!self.measured() || self.free_mib() >= mem_mib)
            && self.free_vcpus().is_none_or(|free| free >= vcpus)
    }
}

/// ADR 0048: the CPU overcommit factor. The host CPU budget is
/// `total_vcpus × factor` — FC guests idle heavily and RAM is the hard
/// constraint, so CPU is deliberately oversubscribed to keep packing
/// from being CPU-bound far below the memory ceiling.
/// `ENGRAM_CPU_OVERCOMMIT`, default 4.0; a non-positive / unparseable
/// value falls back to the default.
pub fn cpu_overcommit_factor() -> f64 {
    std::env::var("ENGRAM_CPU_OVERCOMMIT")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| *f > 0.0)
        .unwrap_or(4.0)
}

/// ADR 0048: a host's schedulable vCPU budget — `total_vcpus × overcommit`.
/// `0` when the host hasn't reported its core count yet (pre-0048
/// host-agent), which the picker treats as "no CPU constraint" (the same
/// soft posture as an unmeasured RAM allocatable).
pub fn host_cpu_budget(total_vcpus: u32) -> i64 {
    (total_vcpus as f64 * cpu_overcommit_factor()).floor() as i64
}

/// Specification for provisioning a new host (autoscaling).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostSpec {
    pub machine_type: String,
    pub zone: String,
    pub disk_gb: u32,
    pub labels: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CordonOwner {
    Operator,
    Admin,
}
impl CordonOwner {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::Admin => "admin",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "operator" => Some(Self::Operator),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "count")]
pub enum RetirementBlocker {
    BoundSessions(u64),
    CaptureJobs(u64),
    OpenTeleportsAsSource(u64),
    OpenTeleportsAsDest(u64),
    PendingTombstones(u64),
    ResidentSandboxes(u64),
    EnableWork(u64),
    NoHeartbeatSinceRequest,
    NotCordoned,
}
impl RetirementBlocker {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::BoundSessions(..) => "bound_sessions",
            Self::CaptureJobs(..) => "capture_jobs",
            Self::OpenTeleportsAsSource(..) => "open_teleports_as_source",
            Self::OpenTeleportsAsDest(..) => "open_teleports_as_dest",
            Self::PendingTombstones(..) => "pending_tombstones",
            Self::ResidentSandboxes(..) => "resident_sandboxes",
            Self::EnableWork(..) => "enable_work",
            Self::NoHeartbeatSinceRequest => "no_heartbeat_since_request",
            Self::NotCordoned => "not_cordoned",
        }
    }
    pub const fn count(&self) -> u64 {
        match self {
            Self::BoundSessions(n)
            | Self::CaptureJobs(n)
            | Self::OpenTeleportsAsSource(n)
            | Self::OpenTeleportsAsDest(n)
            | Self::PendingTombstones(n)
            | Self::ResidentSandboxes(n)
            | Self::EnableWork(n) => *n,
            Self::NoHeartbeatSinceRequest | Self::NotCordoned => 0,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RetirementStatus {
    pub requested_at: Option<DateTime<Utc>>,
    pub retired_at: Option<DateTime<Utc>>,
    pub blockers: Vec<RetirementBlocker>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetirementGrant {
    Granted,
    Blocked(Vec<RetirementBlocker>),
    NotRequested,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeartbeatAck {
    Accepted,
    Refused(HostStatus),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteHostOutcome {
    Deleted,
    NotRetired(HostStatus),
}

/// Lease used by enable-work counts and retirement checks.
pub const ENABLE_MATERIALIZE_LEASE_SECS: u32 = 300;

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [HostStatus; 4] = [
        HostStatus::Ready,
        HostStatus::Draining,
        HostStatus::Dead,
        HostStatus::Retired,
    ];

    /// Exhaustive `{from} × {to}` table — the host-state mirror of the
    /// session-state transition test. Every pair is asserted explicitly
    /// so adding/removing an edge forces this table to be updated, and a
    /// silent widening of the legal set can't slip through.
    #[test]
    fn host_status_transition_table_is_exhaustive() {
        use HostStatus::*;
        let legal = |from: HostStatus, to: HostStatus| -> bool {
            matches!(
                (from, to),
                (Ready, Draining)
                    | (Ready, Dead)
                    | (Draining, Ready)
                    | (Draining, Dead)
                    | (Ready, Retired)
                    | (Draining, Retired)
            )
        };
        for &from in &ALL {
            for &to in &ALL {
                assert_eq!(
                    from.can_transition_to(to),
                    legal(from, to),
                    "edge {from:?} -> {to:?} disagrees with the expected table",
                );
            }
        }
    }

    /// The load-bearing invariant for issue #230: a `dead` host can never
    /// transition anywhere via the predicate (self-transitions included),
    /// so the only way back to `ready` is an explicit re-register.
    #[test]
    fn dead_is_terminal() {
        for &to in &ALL {
            assert!(
                !HostStatus::Dead.can_transition_to(to),
                "dead -> {to:?} must be illegal; a partitioned host marked \
                 dead must not resurrect on its next heartbeat",
            );
        }
    }

    #[test]
    fn self_transitions_are_illegal() {
        for &s in &ALL {
            assert!(
                !s.can_transition_to(s),
                "self-transition {s:?} -> {s:?} carries no new information",
            );
        }
    }

    #[test]
    fn host_status_serializes_lowercase() {
        for (variant, wire) in [
            (HostStatus::Ready, "ready"),
            (HostStatus::Draining, "draining"),
            (HostStatus::Dead, "dead"),
            (HostStatus::Retired, "retired"),
        ] {
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                serde_json::json!(wire)
            );
            let back: HostStatus = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
            assert_eq!(back, variant);
            assert_eq!(variant.as_str(), wire);
        }
    }

    /// ADR 0068: an old host-agent's register/heartbeat body — no
    /// `capabilities` field at all — must decode to `schema == 0`, the
    /// same soft posture `wire_version == 0` gets. This is what keeps a
    /// mixed-fleet rolling deploy placeable.
    #[test]
    fn host_capabilities_defaults_to_schema_zero_when_absent() {
        let caps: HostCapabilities = serde_json::from_str("{}").unwrap();
        assert_eq!(caps.schema, 0);
        assert_eq!(caps.grpc_self_connect, CapStatus::Unknown);
        assert_eq!(caps.fc_snapshot_version, None);
    }

    #[test]
    fn cap_status_round_trips_every_variant() {
        for cap in [
            CapStatus::Unknown,
            CapStatus::Ok(None),
            CapStatus::Ok(Some("session volumes".to_string())),
            CapStatus::Failed("ENOENT".to_string()),
            CapStatus::NotApplicable,
        ] {
            let wire = serde_json::to_value(&cap).unwrap();
            let back: CapStatus = serde_json::from_value(wire).unwrap();
            assert_eq!(back, cap);
        }
    }

    #[test]
    fn cap_status_is_ok_only_for_the_ok_variant() {
        assert!(CapStatus::Ok(None).is_ok());
        assert!(CapStatus::Ok(Some("x".into())).is_ok());
        assert!(!CapStatus::Unknown.is_ok());
        assert!(!CapStatus::Failed("x".into()).is_ok());
        assert!(!CapStatus::NotApplicable.is_ok());
    }

    #[test]
    fn failing_capabilities_ignores_not_applicable_and_schema_zero_unknown() {
        // schema 0 (never reported): Unknown everywhere, but nothing
        // "failing" — this is the mid-roll soft-pass posture, not a
        // probed failure.
        let never_reported = HostCapabilities::default();
        assert!(never_reported.failing_capabilities().is_empty());

        // schema 1, a real vector: NotApplicable (a VZ host's substrate
        // fields) never counts; Failed always does; Unknown at schema
        // 1 (a field the host-agent build didn't populate) counts too.
        let vz_like = HostCapabilities {
            schema: 1,
            backend: "vz".to_string(),
            grpc_self_connect: CapStatus::Ok(None),
            base_shm_tmpfs: CapStatus::NotApplicable,
            uffd_minor_shmem: CapStatus::NotApplicable,
            session_disk: CapStatus::NotApplicable,
            page_idle: CapStatus::NotApplicable,
            bundle_stamp: CapStatus::Failed("no stamp".to_string()),
            fc_snapshot_version: None,
            wire_version: 7,
            reattach_generation: 0,
        };
        assert_eq!(vz_like.failing_capabilities(), vec!["bundle_stamp"]);
    }

    #[test]
    fn page_idle_failure_is_visible_but_old_payloads_still_decode() {
        let mut caps: HostCapabilities = serde_json::from_str("{}").unwrap();
        assert_eq!(caps.page_idle, CapStatus::Unknown);
        caps.page_idle = CapStatus::Failed("not configured".into());
        assert_eq!(caps.failing_capabilities(), vec!["page_idle"]);
    }

    #[test]
    fn failing_capabilities_sorted_deterministic_order() {
        let broken = HostCapabilities {
            schema: 1,
            backend: "firecracker".to_string(),
            grpc_self_connect: CapStatus::Failed("x".into()),
            base_shm_tmpfs: CapStatus::Failed("x".into()),
            uffd_minor_shmem: CapStatus::Ok(None),
            session_disk: CapStatus::Unknown,
            page_idle: CapStatus::Unknown,
            bundle_stamp: CapStatus::Ok(None),
            fc_snapshot_version: None,
            wire_version: 7,
            reattach_generation: 0,
        };
        assert_eq!(
            broken.failing_capabilities(),
            vec!["grpc_self_connect", "base_shm_tmpfs", "session_disk"]
        );
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;

    #[test]
    fn only_a_higher_reattach_generation_lifts_a_cordon() {
        let host = |cordoned, generation| HostRecord {
            id: HostId::new(),
            hostname: "h".into(),
            cloud_metadata: Default::default(),
            capacity: HostCapacity {
                total_gb: 0,
                used_gb: 0,
                total_mib: 0,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: Default::default(),
            status: HostStatus::Ready,
            last_heartbeat_at: chrono::DateTime::UNIX_EPOCH,
            host_addr: None,
            ready_images: Vec::new(),
            current_bundles: Vec::new(),
            sandbox_bundles: Vec::new(),
            cordoned,
            cordon_owner: None,
            cordon_reason: None,
            retire_requested_at: None,
            retired_at: None,
            total_vcpus: 0,
            wire_version: 0,
            stages_images: false,
            capabilities: HostCapabilities {
                reattach_generation: generation,
                ..Default::default()
            },
            lease_expires_at: None,
            lease_state: Default::default(),
            lease_epoch: 0,
        };
        let reports = |generation| HostCapabilities {
            reattach_generation: generation,
            ..Default::default()
        };
        assert!(register_lifts_cordon(&host(true, 0), &reports(1)));
        assert!(!register_lifts_cordon(&host(true, 1), &reports(1)));
        assert!(!register_lifts_cordon(&host(true, 2), &reports(1)));
        assert!(!register_lifts_cordon(&host(false, 0), &reports(1)));
    }
    #[test]
    fn blocker_kind_strings() {
        let cases = [
            (RetirementBlocker::BoundSessions(1), "bound_sessions", 1),
            (RetirementBlocker::CaptureJobs(2), "capture_jobs", 2),
            (
                RetirementBlocker::OpenTeleportsAsSource(3),
                "open_teleports_as_source",
                3,
            ),
            (
                RetirementBlocker::OpenTeleportsAsDest(4),
                "open_teleports_as_dest",
                4,
            ),
            (
                RetirementBlocker::PendingTombstones(5),
                "pending_tombstones",
                5,
            ),
            (
                RetirementBlocker::ResidentSandboxes(6),
                "resident_sandboxes",
                6,
            ),
            (RetirementBlocker::EnableWork(7), "enable_work", 7),
            (
                RetirementBlocker::NoHeartbeatSinceRequest,
                "no_heartbeat_since_request",
                0,
            ),
            (RetirementBlocker::NotCordoned, "not_cordoned", 0),
        ];
        for (b, kind, count) in cases {
            assert_eq!(b.kind(), kind);
            assert_eq!(b.count(), count);
        }
    }
}

#[cfg(test)]
mod headroom_tests {
    use super::{HostHeadroom, ReservedBudget};

    fn room(allocatable_mib: i64, cpu_budget: i64, mem_mib: i64, vcpus: i64) -> HostHeadroom {
        HostHeadroom {
            allocatable_mib,
            cpu_budget,
            reserved: ReservedBudget { mem_mib, vcpus },
        }
    }

    #[test]
    fn free_is_allocatable_less_reserved_in_both_dimensions() {
        let r = room(32_768, 32, 28_000, 30);
        assert_eq!(r.free_mib(), 4_768);
        assert_eq!(r.free_vcpus(), Some(2));
        assert!(r.fits(4_096, 2));
        assert!(!r.fits(8_192, 2), "RAM binds");
        assert!(!r.fits(4_096, 4), "CPU binds");
    }

    #[test]
    fn an_unmeasured_host_does_not_gate_placement() {
        let r = room(0, 0, 0, 0);
        assert!(!r.measured());
        assert!(
            r.fits(65_536, 64),
            "unmeasured RAM and unreported cores do not gate"
        );
        assert_eq!(room(32_768, 0, 0, 0).free_vcpus(), None);
    }

    #[test]
    fn an_over_reserved_host_has_negative_free_ram() {
        assert_eq!(room(60_000, 0, 70_000, 0).free_mib(), -10_000);
    }
}
