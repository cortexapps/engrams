use chrono::{DateTime, Utc};
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use serde::{Deserialize, Serialize};

/// Serde default for `running_sandboxes_known` (issue #215): a
/// heartbeat that omits the field (pre-fix host-agent mid-roll) is
/// assumed to carry a valid `backend.list()` result.
fn default_true() -> bool {
    true
}

/// Heartbeat message: host -> coordinator, every ~5s. Reports
/// current capacity, the snapshots held locally, the set of
/// sandbox_ids the host currently has live in its `SandboxBackend`,
/// and (ADR 0015 M5) which images this host has fully prefetched to
/// local NVMe — the scheduler uses the readiness set to gate session
/// creates onto hosts that can serve them quickly.
///
/// `running_sandboxes` is the load-bearing input to ADR 0009's
/// reconciliation pass: the coord intersects this against
/// `sessions` rows where `host_id = this_host AND status = 'active'`,
/// and any session whose sandbox is missing for N consecutive
/// heartbeats transitions to `Idle` (if its latest snapshot is
/// recoverable) or `Dead`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Heartbeat {
    pub host_id: HostId,
    pub sent_at: DateTime<Utc>,
    pub capacity: HostCapacityReport,
    pub local_snapshots: Vec<LocalSnapshotReport>,
    /// Sandbox IDs currently live on this host (per `backend.list()`).
    /// Empty on hosts that haven't enabled reconciliation yet (Phase 1
    /// observation window). ~12 B per id × ~50 sandboxes ≈ 600 B per
    /// heartbeat — trivial. Ordered for deterministic test fixtures;
    /// the coord doesn't care.
    ///
    /// Issue #215: when `running_sandboxes_known` is `false` this field
    /// is meaningless (`backend.list()` errored this tick) and the
    /// coord MUST NOT run the ADR 0009 reconcile against it — an empty
    /// list there is "no information", not "no sandboxes", and feeding
    /// it through would strike every active session on the host.
    pub running_sandboxes: Vec<SandboxId>,
    /// Issue #215: `false` iff `backend.list()` failed this tick, so
    /// `running_sandboxes` carries no usable signal. `#[serde(default
    /// = ...)]` to `true` for mixed-version interop — a pre-fix
    /// host-agent that omits the field is assumed to have a valid list
    /// (its old behaviour of reporting empty-on-error is the bug being
    /// fixed, but it's no worse than before for the rollout window).
    #[serde(default = "default_true")]
    pub running_sandboxes_known: bool,
    pub draining: bool,
    /// ADR 0015 M5: manifest digests of every image this host has
    /// fully prefetched to local NVMe. The scheduler's
    /// `pick_for_session` filter requires the requested image's
    /// digest to be present in some host's set; if zero hosts are
    /// ready, the session create returns
    /// `ApiError::ImageNotReady`. Drained on each heartbeat from
    /// the host-agent's `image_prefetch` supervisor.
    #[serde(default)]
    pub ready_images: Vec<ManifestDigest>,
    /// ADR 0028 Fix A: durable checkpoint records this host holds
    /// that no coord has acked into PG yet. Re-advertised every
    /// heartbeat until acked — what makes a checkpoint that reached
    /// GCS become a PG row regardless of which coord (if any)
    /// survived the original capture pipeline, and what subsumes the
    /// coord-died-mid-eviction reconciliation window.
    /// `#[serde(default)]` for mixed-version interop during the roll.
    #[serde(default)]
    pub checkpoints: Vec<CheckpointAdvert>,
    /// Observed disk/mem/cpu utilization sampled this tick. Drives the
    /// operator fleet view; persisted to the `hosts` row so it stays
    /// consistent across coord replicas. `#[serde(default)]` so a
    /// pre-utilization host-agent interops cleanly against this coord.
    #[serde(default)]
    pub utilization: engram_core::types::host::HostUtilization,
}

/// ADR 0028 Fix A: one un-acked durable checkpoint. Carries
/// everything `record_snapshot` needs — the advertising host may be
/// the only survivor of the original capture pipeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointAdvert {
    pub snapshot_id: SnapshotId,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub image_version: String,
    pub size_bytes: u64,
    pub disk_manifest: Option<engram_core::types::manifest::ManifestRef>,
    pub memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
    /// ADR 0035 pins for this checkpoint's device model.
    #[serde(default)]
    pub aux_bundles: Vec<engram_core::types::sandbox::AuxBundleRef>,
    /// The pause instant — the coord resolves the A.log
    /// `session_events` cursor as "last event at or before this"
    /// when it records the row.
    pub paused_at: DateTime<Utc>,
    pub captured_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostCapacityReport {
    pub total_mib: u64,
    pub used_mib: u64,
    pub running_sandboxes: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalSnapshotReport {
    pub snapshot_id: SnapshotId,
    pub session_id: SessionId,
    pub size_bytes: u64,
    pub replicated: bool,
    pub last_accessed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeartbeatAck {
    pub server_time: DateTime<Utc>,
    /// Sessions the coordinator has reassigned away from this host.
    /// The host should drop them from local state on next reconciliation.
    pub revoked_sessions: Vec<SessionId>,
    /// ADR 0015 M5: coord's authoritative `enabled_images` set. Hosts
    /// diff this against their local NVMe cache and drive prefetch
    /// for any image whose chunks aren't all present. Empty means
    /// "no images enabled" — host clears its ready set on next
    /// supervisor tick.
    #[serde(default)]
    pub enabled_images: Vec<EnabledImageRef>,
    /// ADR 0028 Fix A: checkpoint adverts from this heartbeat that
    /// the coord successfully recorded into PG (idempotently, on
    /// snapshot_id). The host deletes the matching durable record
    /// files — the PG rows own the references now.
    #[serde(default)]
    pub acked_checkpoints: Vec<SnapshotId>,
}

/// Identity of one enabled image. Manifest digest is the sha256 of
/// the OCI manifest (existing column on `enabled_images`); it's the
/// content-addressed key the scheduler matches `ready_images`
/// against.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnabledImageRef {
    pub image_uri: String,
    pub manifest_digest: ManifestDigest,
    /// ADR 0022 Option A: the base snapshot's id. The host materializes the
    /// per-template contiguous `memory.bin` at `snapshot_path_for(this)/
    /// memory.bin` during residency prefetch — the exact path a base
    /// `session.create` restore reads — so same-template siblings
    /// `MAP_PRIVATE` one resident inode (density + faster boot). Always
    /// present (`enabled_images.base_snapshot_id` is `NOT NULL`, migration
    /// 0038); no `serde(default)` — clean break, coord + hosts deploy together.
    pub base_snapshot_id: SnapshotId,
    /// ADR 0021 P2: the base snapshot's disk manifest, advertised so the host
    /// warms the rootfs working set on NVMe (residency) before sessions
    /// restore. The on-demand serial-from-GCS page-in of these chunks during
    /// `resume` is the measured substrate cost. Always present — the
    /// `enabled_images` column is `NOT NULL` (migration 0042); no
    /// `serde(default)`: this is a clean break, coord + hosts deploy together.
    pub base_snapshot_disk_manifest: engram_core::types::manifest::ManifestRef,
    /// ADR 0021 P2 (memory residency): the base snapshot's memory manifest,
    /// advertised so the host warms the chunked memory image on NVMe at
    /// host-boot — symmetric with the disk manifest above. When present, the
    /// first session on a freshly rolled host restores warm instead of paying a
    /// cold per-restore memory prefetch from GCS (~2.84 s, measured).
    ///
    /// `None` for cold-boot backends (VZ): Apple's arm64 save/restore is broken
    /// (ADR 0003), so VZ clone-snapshots the disk and cold-boots — there is no
    /// memory image to warm. Nullable since migration 0049. No `serde(default)`
    /// — clean break, coord + hosts deploy together.
    pub base_snapshot_memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
}

/// Newtype over the OCI manifest digest string (`sha256:<hex>`).
/// Wraps the raw string so it's distinct from arbitrary `String`s
/// in scheduler / readiness signatures.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ManifestDigest(pub String);

impl ManifestDigest {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ManifestDigest {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for ManifestDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Heartbeat {
        Heartbeat {
            host_id: HostId::new(),
            sent_at: Utc::now(),
            capacity: HostCapacityReport {
                total_mib: 256_000,
                used_mib: 64_000,
                running_sandboxes: 7,
            },
            local_snapshots: vec![LocalSnapshotReport {
                snapshot_id: SnapshotId::new(),
                session_id: SessionId::new(),
                size_bytes: 12_345_678,
                replicated: true,
                last_accessed_at: Utc::now(),
            }],
            running_sandboxes: vec![SandboxId::new(), SandboxId::new()],
            running_sandboxes_known: true,
            draining: false,
            ready_images: Vec::new(),
            checkpoints: Vec::new(),
            utilization: Default::default(),
        }
    }

    #[test]
    fn heartbeat_round_trips_through_json() {
        let original = sample();
        let json = serde_json::to_string(&original).unwrap();
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.host_id, original.host_id);
        assert_eq!(back.capacity.total_mib, original.capacity.total_mib);
        assert_eq!(back.capacity.used_mib, original.capacity.used_mib);
        assert_eq!(
            back.capacity.running_sandboxes,
            original.capacity.running_sandboxes
        );
        assert_eq!(back.local_snapshots.len(), 1);
        assert_eq!(
            back.local_snapshots[0].snapshot_id,
            original.local_snapshots[0].snapshot_id
        );
        assert!(back.local_snapshots[0].replicated);
        assert_eq!(back.running_sandboxes, original.running_sandboxes);
        assert_eq!(back.draining, original.draining);
    }

    #[test]
    fn utilization_round_trips_through_json() {
        let mut original = sample();
        original.utilization = engram_core::types::host::HostUtilization {
            disk_total_mib: 102_400,
            disk_used_mib: 81_920,
            mem_total_mib: 32_768,
            mem_used_mib: 9_001,
            allocatable_mib: 23_767,
            cpu_pct: 42.5,
            // Issue #540: the RAM ledger's attribution fields.
            base_shm_mib: 19_500,
            base_shm_pending_mib: 512,
            parked_pss_mib: 4_096,
            running_pss_mib: 6_000,
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: Heartbeat = serde_json::from_str(&json).unwrap();
        assert_eq!(back.utilization.disk_total_mib, 102_400);
        assert_eq!(back.utilization.disk_used_mib, 81_920);
        assert_eq!(back.utilization.mem_total_mib, 32_768);
        assert_eq!(back.utilization.mem_used_mib, 9_001);
        assert_eq!(back.utilization.allocatable_mib, 23_767);
        assert_eq!(back.utilization.cpu_pct, 42.5);
        assert_eq!(back.utilization.base_shm_mib, 19_500);
        assert_eq!(back.utilization.base_shm_pending_mib, 512);
        assert_eq!(back.utilization.parked_pss_mib, 4_096);
        assert_eq!(back.utilization.running_pss_mib, 6_000);
    }

    #[test]
    fn utilization_ram_ledger_fields_default_to_zero_for_old_hosts() {
        // Rollout interop (same posture as the pre-existing utilization
        // fields): a host-agent on an older build sends no
        // base_shm_mib/base_shm_pending_mib/parked_pss_mib/running_pss_mib
        // keys at all. `#[serde(default)]` must decode that to zeros, not
        // fail the heartbeat.
        let mut v: serde_json::Value = serde_json::to_value(sample()).unwrap();
        v["utilization"] = serde_json::json!({
            "disk_total_mib": 1,
            "disk_used_mib": 1,
            "mem_total_mib": 1,
            "mem_used_mib": 1,
            "allocatable_mib": 1,
            "cpu_pct": 1.0,
        });
        let back: Heartbeat = serde_json::from_value(v).unwrap();
        assert_eq!(back.utilization.base_shm_mib, 0);
        assert_eq!(back.utilization.base_shm_pending_mib, 0);
        assert_eq!(back.utilization.parked_pss_mib, 0);
        assert_eq!(back.utilization.running_pss_mib, 0);
    }

    #[test]
    fn heartbeat_without_utilization_field_defaults_to_zero() {
        // Rollout interop: coord deploys before the host MIG, so a
        // pre-utilization host-agent sends heartbeats with no
        // `utilization` key. `#[serde(default)]` must make that decode
        // to zeros rather than failing the whole heartbeat.
        let json = r#"{
            "host_id": "00000000-0000-0000-0000-000000000000",
            "sent_at": "2026-06-05T00:00:00Z",
            "capacity": {"total_mib": 1024, "used_mib": 0, "running_sandboxes": 0},
            "local_snapshots": [],
            "running_sandboxes": [],
            "draining": false
        }"#;
        let hb: Heartbeat = serde_json::from_str(json).expect("decode without utilization");
        assert_eq!(hb.utilization.disk_total_mib, 0);
        assert_eq!(hb.utilization.cpu_pct, 0.0);
    }

    #[test]
    fn empty_running_sandboxes_serializes_as_array() {
        // ADR 0009: the coord's reconcile pass intersects the inbound
        // `running_sandboxes` against expected-active sessions. An
        // accidental `Option<Vec<_>>` or `skip_serializing_if` would
        // make "host has no sandboxes" indistinguishable from "host
        // didn't send the field," which would mis-flip every session
        // on the host. Pin the wire shape.
        let mut h = sample();
        h.running_sandboxes.clear();
        let v: serde_json::Value = serde_json::to_value(&h).unwrap();
        assert_eq!(v["running_sandboxes"], serde_json::json!([]));
    }

    #[test]
    fn heartbeat_ack_round_trips_through_json() {
        let original = HeartbeatAck {
            server_time: Utc::now(),
            revoked_sessions: vec![SessionId::new(), SessionId::new()],
            acked_checkpoints: vec![],
            enabled_images: vec![EnabledImageRef {
                image_uri: "localhost:5001/test/demo:warm-1".into(),
                manifest_digest: ManifestDigest::new("sha256:abc123"),
                base_snapshot_id: SnapshotId::new(),
                base_snapshot_disk_manifest: engram_core::types::manifest::ManifestRef {
                    manifest_id: uuid::Uuid::nil(),
                    version: 1,
                },
                base_snapshot_memory_manifest: Some(engram_core::types::manifest::ManifestRef {
                    manifest_id: uuid::Uuid::nil(),
                    version: 1,
                }),
            }],
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: HeartbeatAck = serde_json::from_str(&json).unwrap();
        assert_eq!(back.revoked_sessions, original.revoked_sessions);
        assert_eq!(back.enabled_images, original.enabled_images);
    }

    #[test]
    fn empty_local_snapshots_serializes_as_array() {
        // Defending against an accidental switch to Option<Vec<_>> or
        // skip_serializing_if which would change the wire shape and
        // break the host-side parser.
        let mut h = sample();
        h.local_snapshots.clear();
        let v: serde_json::Value = serde_json::to_value(&h).unwrap();
        assert_eq!(v["local_snapshots"], serde_json::json!([]));
    }

    #[test]
    fn ready_images_serializes_as_array() {
        let mut h = sample();
        h.ready_images = vec![ManifestDigest::new("sha256:deadbeef")];
        let v: serde_json::Value = serde_json::to_value(&h).unwrap();
        assert_eq!(v["ready_images"], serde_json::json!(["sha256:deadbeef"]));
    }
}
