use chrono::{DateTime, Utc};
use engram_core::types::ids::TemplateRef;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::template::TemplateRecord;
use engram_core::{HostId, SandboxId, SessionId, SnapshotId};
use serde::{Deserialize, Serialize};

/// Heartbeat message: host -> coordinator, every ~5s. Reports
/// current capacity, the snapshots held locally, and the set of
/// sandbox_ids the host currently has live in its `SandboxBackend`.
///
/// `running_sandboxes` is the load-bearing input to ADR 0009's
/// reconciliation pass: the coord intersects this against
/// `sessions` rows where `host_id = this_host AND status = 'active'`,
/// and any session whose sandbox is missing for N consecutive
/// heartbeats transitions to `Idle` (if its latest snapshot is
/// recoverable) or `Dead`.
///
/// Pre-v5: this carried a `warm_pools: Vec<WarmPoolReport>` field
/// that reported per-image-version warm-slot ready/target counts.
/// Warm pools were deleted as part of the ADR 0008 follow-up — the
/// scheduling preference they enabled (route a session to a host
/// that already had a matching warm slot) gave way to chunked-OCI
/// content-addressable rootfs + canonical-memory restore (FC) and
/// straight cold start (VZ). See the deletion commit for context.
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
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostCapacityReport {
    pub total_mib: u64,
    pub used_mib: u64,
    pub running_sandboxes: u32,
    /// ADR 0014: per-template warm slot inventory. The coord
    /// scheduler uses this as a parallel-ask hint — hosts that
    /// report zero (or no entry) for a template are skipped, no
    /// gRPC round-trip needed. Empty `Vec` (the default) means
    /// "no warm pool here", which is the mode=all / pre-warm-pool
    /// state and matches what the coord scheduler treats as
    /// "everyone is cold".
    #[serde(default)]
    pub warm_slots: Vec<WarmSlotReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WarmSlotReport {
    pub template_ref: TemplateRef,
    /// Sandboxes currently in the free-list (ready to lease).
    pub available: u32,
    /// Autoscaler target N this host is keeping the pool at.
    pub target: u32,
    /// ADR 0014 issue #5: refill failures observed since the last
    /// heartbeat. Drained on read by `WarmPool::list_slots` so
    /// each heartbeat reports the delta, not a cumulative count.
    #[serde(default)]
    pub refill_failures_since_last: u32,
    /// Short classifier of the most recent failure
    /// (`"blob_not_found"`, `"manifest_load"`, `"fc_spawn"`,
    /// `"other"`). Empty when no failures since last heartbeat.
    #[serde(default)]
    pub last_error_class: String,
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
    /// ADR 0014: coord's authoritative active-template set, with
    /// each entry's full SnapshotMetadata inlined so the host's
    /// WarmPool can `restore` without a second round-trip.
    /// Empty `Vec` means the coord has no active templates (or
    /// hasn't enabled warm-pool yet) — the host's WarmPool will
    /// drain its existing slots after the STALE_GRACE window.
    #[serde(default)]
    pub active_templates: Vec<ActiveTemplate>,
}

/// One entry in [`HeartbeatAck::active_templates`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveTemplate {
    pub record: TemplateRecord,
    pub snapshot: SnapshotMetadata,
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
                warm_slots: Vec::new(),
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
            active_templates: Vec::new(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: HeartbeatAck = serde_json::from_str(&json).unwrap();
        assert_eq!(back.revoked_sessions, original.revoked_sessions);
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
}
