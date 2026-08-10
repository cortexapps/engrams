use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::HostId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostStatus {
    Ready,
    Draining,
    Dead,
}

impl HostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Dead => "dead",
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
    /// Draining -> Ready    (host came back from a self-reported drain)
    ///           | Dead     (dead-host sweep)
    /// Dead     -> (terminal via heartbeat) — a `dead` row returns ONLY
    ///             via an explicit `POST /api/hosts/register`
    ///             (`upsert_host`, which hardcodes `Ready`), never on the
    ///             host's next heartbeat. A merely-partitioned host that
    ///             the sweep marked `dead` (and whose sessions it
    ///             orphaned) must NOT silently flip back to `ready` and
    ///             resume taking placements with its sessions unbound.
    /// ```
    ///
    /// Self-transitions return `false`: a no-op status write carries no
    /// new information and is almost certainly a racing writer.
    pub const fn can_transition_to(&self, target: Self) -> bool {
        use HostStatus::*;
        match self {
            Ready => matches!(target, Draining | Dead),
            Draining => matches!(target, Ready | Dead),
            Dead => false,
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
/// which the host is treated as disk-pressured. Issue #528 / the
/// `engrams-fc-xngk` post-mortem: 20 GiB is the margin under which the
/// host-agent's idle-evict stops pushing candidates. The SINGLE owner of
/// this value — the host-agent's `DEFAULT_DISK_FLOOR_BYTES` aliases it,
/// and ADR 0078's placement affinity vetoes a snapshot-host whose free
/// disk is below it (placing a resume on a host about to disk-evict its
/// cache is pointless). Do not mint a second threshold.
pub const HOST_DISK_CACHE_FLOOR_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// [`HOST_DISK_CACHE_FLOOR_BYTES`] in MiB, for comparison against
/// [`HostUtilization`]'s MiB disk fields on the coordinator placement
/// path.
pub const HOST_DISK_CACHE_FLOOR_MIB: u64 = HOST_DISK_CACHE_FLOOR_BYTES / (1024 * 1024);

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
    /// Total / used bytes (MiB) of the host's work_dir filesystem —
    /// the mount that holds the chunk cache, jails, and FC memory
    /// dumps. `statvfs(2)`; available on Linux and macOS.
    #[serde(default)]
    pub disk_total_mib: u64,
    #[serde(default)]
    pub disk_used_mib: u64,
    /// ADR 0112: Σ `swap_mib` over this host's live sandboxes — the
    /// worst case the ephemeral swap files can allocate on the work_dir
    /// mount. COMMITTED, not walked: after unlink-after-attach the
    /// backing inodes are anonymous (visible to statvfs in aggregate,
    /// invisible to any path walk). Placement subtracts it from free
    /// disk before the floor test so admission is reservation-safe.
    #[serde(default)]
    pub committed_swap_mib: u64,
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
    /// (e.g. `nbds_max=128`) for operator display; never load-bearing.
    Ok(Option<String>),
    /// The probe ran and failed. `detail` is the errno/message.
    Failed(String),
    /// This backend never has this capability (e.g. VZ has no
    /// uffd/nbd/base_shm substrate) — distinct from `Failed` so the fleet
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
    /// The `nbd` kernel module is loaded (`/sys/module/nbd/parameters/nbds_max`
    /// readable). `detail` carries `nbds_max=<n>`. `NotApplicable` off the
    /// FC backend. Does NOT change `build_from_kernel`'s materialize-to-file
    /// dev fallback — this only makes a prod misconfiguration visible.
    #[serde(default)]
    pub nbd: CapStatus,
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
}

impl HostCapabilities {
    /// ADR 0068 fleet-view surface: names of the capabilities that are
    /// NOT `Ok` — `Failed` always counts; `Unknown` counts only once
    /// the host has reported a real vector (`schema >= 1`), since an
    /// `Unknown` at `schema == 0` just means "hasn't reported yet,"
    /// not "probed and something's wrong." `NotApplicable` never
    /// counts — it's the correct steady state for e.g. `nbd` on a VZ
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
        if is_failing(&self.nbd) {
            out.push("nbd");
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
    /// ADR 0115 D2: per-running-sandbox aux bundle attachments
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
    /// ADR 0115 D2: aux bundle refs attached to each running sandbox
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
}

/// ADR 0048: per-host reserved budget across BOTH placement dimensions —
/// Σ over the memory-reserving session states of `mem_budget_mib` and
/// `cpu_budget_vcpus`. The read-side twin of `reserve_placement`'s
/// in-transaction aggregate, for the resume/evac picker and the fleet view.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReservedBudget {
    pub mem_mib: i64,
    pub vcpus: i64,
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

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [HostStatus; 3] = [HostStatus::Ready, HostStatus::Draining, HostStatus::Dead];

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
                (Ready, Draining) | (Ready, Dead) | (Draining, Ready) | (Draining, Dead)
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
            CapStatus::Ok(Some("nbds_max=128".to_string())),
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
            nbd: CapStatus::NotApplicable,
            bundle_stamp: CapStatus::Failed("no stamp".to_string()),
            fc_snapshot_version: None,
            wire_version: 7,
        };
        assert_eq!(vz_like.failing_capabilities(), vec!["bundle_stamp"]);
    }

    #[test]
    fn failing_capabilities_sorted_deterministic_order() {
        let broken = HostCapabilities {
            schema: 1,
            backend: "firecracker".to_string(),
            grpc_self_connect: CapStatus::Failed("x".into()),
            base_shm_tmpfs: CapStatus::Failed("x".into()),
            uffd_minor_shmem: CapStatus::Ok(None),
            nbd: CapStatus::Unknown,
            bundle_stamp: CapStatus::Ok(None),
            fc_snapshot_version: None,
            wire_version: 7,
        };
        assert_eq!(
            broken.failing_capabilities(),
            vec!["grpc_self_connect", "base_shm_tmpfs", "nbd"]
        );
    }
}
