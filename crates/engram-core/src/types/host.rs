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
    /// Physical RAM: MemTotal and (MemTotal − MemAvailable) from
    /// `/proc/meminfo`. Zero on non-Linux (no `/proc`).
    #[serde(default)]
    pub mem_total_mib: u64,
    #[serde(default)]
    pub mem_used_mib: u64,
    /// ADR 0046: memory (MiB) actually available to place NEW sessions on this
    /// host — `MemAvailable + Σ guest-resident (PSS)`. It nets out the host
    /// daemon, OS, kube-system pods, the chunk cache, and the mlock'd
    /// base-memfile residency (ADR 0022) automatically — everything in
    /// `MemUsed` that isn't a running VM — so placement subtracts only session
    /// budgets from it. `0` on non-Linux / pre-0058 hosts, where placement
    /// falls back to the raw `mem_total_mib`.
    #[serde(default)]
    pub allocatable_mib: u64,
    /// Whole-host CPU utilization in percent (0–100), computed from
    /// the `/proc/stat` aggregate-cpu delta across the heartbeat
    /// interval. Zero on non-Linux or on the first tick (no prior
    /// sample to diff against).
    #[serde(default)]
    pub cpu_pct: f32,
}

/// ADR 0047: a snapshot the host holds locally, as persisted in the
/// `hosts.local_snapshots` JSONB column. Field-compatible with the
/// heartbeat wire type (`engram_protocol::heartbeat::LocalSnapshotReport`)
/// so the handler serializes the wire payload straight into the row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostLocalSnapshot {
    pub snapshot_id: super::ids::SnapshotId,
    pub session_id: super::ids::SessionId,
    pub size_bytes: u64,
    pub replicated: bool,
    pub last_accessed_at: DateTime<Utc>,
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
    /// ADR 0047: snapshots the host holds locally (heartbeat-persisted).
    /// Snapshot-affinity ranking reads the ids; the fleet view renders
    /// the count.
    #[serde(default)]
    pub local_snapshots: Vec<HostLocalSnapshot>,
    /// ADR 0035/0047: the host's current bundle bake stamp
    /// (heartbeat-persisted). Operator visibility into fleet skew.
    #[serde(default)]
    pub current_bundles: Vec<super::sandbox::AuxBundleRef>,
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
    pub local_snapshots: Vec<HostLocalSnapshot>,
    pub current_bundles: Vec<super::sandbox::AuxBundleRef>,
    pub total_vcpus: u32,
    /// Issue #229: the host-agent's bincode `WIRE_VERSION` this tick.
    pub wire_version: u32,
    /// ADR 0036 amendment (issue #538): whether this host's image-prefetch
    /// supervisor is spawned — see [`HostRecord::stages_images`].
    pub stages_images: bool,
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
    pub preemptible: bool,
    pub disk_gb: u32,
    pub labels: Vec<(String, String)>,
}

/// Notice that the host running this process will be reclaimed soon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreemptionNotice {
    pub reason: String,
    /// Approximate seconds until the instance is forcibly terminated.
    /// `None` if the cloud doesn't surface a deadline.
    pub deadline_secs: Option<u32>,
    pub received_at: DateTime<Utc>,
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
}
