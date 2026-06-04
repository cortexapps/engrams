use chrono::{DateTime, Utc};
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use serde::{Deserialize, Serialize};

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
    /// observation window) or where the backend returned an error.
    /// ~12 B per id × ~50 sandboxes ≈ 600 B per heartbeat — trivial.
    /// Ordered for deterministic test fixtures; the coord doesn't care.
    pub running_sandboxes: Vec<SandboxId>,
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
    /// ADR 0018 Phase B: sandbox IDs whose backing `/dev/nbdN` has
    /// failed health probes for N consecutive checks (default 3 × 5s
    /// = 15s of degraded I/O). Coord's per-heartbeat consumer fires
    /// the evacuation primitive against each entry, relocating the
    /// affected session to a peer host with `EvacLoss::Memory`
    /// (the source disk is unreachable for a fresh memory snapshot).
    ///
    /// Empty in the common case. The probe lives in
    /// `engram-host-agent::disk_daemon`; `#[serde(default)]` so
    /// hosts running older builds (pre-Phase-B) interop cleanly
    /// against this coord.
    #[serde(default)]
    pub nbd_unhealthy: Vec<SandboxId>,
    /// ADR 0028 Fix A: durable checkpoint records this host holds
    /// that no coord has acked into PG yet. Re-advertised every
    /// heartbeat until acked — what makes a checkpoint that reached
    /// GCS become a PG row regardless of which coord (if any)
    /// survived the original capture pipeline, and what subsumes the
    /// coord-died-mid-eviction reconciliation window.
    /// `#[serde(default)]` for mixed-version interop during the roll.
    #[serde(default)]
    pub checkpoints: Vec<CheckpointAdvert>,
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
            draining: false,
            ready_images: Vec::new(),
            nbd_unhealthy: Vec::new(),
            checkpoints: Vec::new(),
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
