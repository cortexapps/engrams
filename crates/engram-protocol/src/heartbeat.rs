use chrono::{DateTime, Utc};
use engram_core::{HostId, SessionId, SnapshotId};
use serde::{Deserialize, Serialize};

/// Heartbeat message: host -> coordinator, every ~5s. Reports current
/// capacity, warm-pool state, and the snapshots held locally.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Heartbeat {
    pub host_id: HostId,
    pub sent_at: DateTime<Utc>,
    pub capacity: HostCapacityReport,
    pub warm_pools: Vec<WarmPoolReport>,
    pub local_snapshots: Vec<LocalSnapshotReport>,
    pub draining: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HostCapacityReport {
    pub total_mib: u64,
    pub used_mib: u64,
    pub running_sandboxes: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WarmPoolReport {
    pub repo: String,
    pub image_version: String,
    pub ready: u32,
    pub target: u32,
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
            warm_pools: vec![WarmPoolReport {
                repo: "cortex/api".into(),
                image_version: "warm-2026-04".into(),
                ready: 2,
                target: 4,
            }],
            local_snapshots: vec![LocalSnapshotReport {
                snapshot_id: SnapshotId::new(),
                session_id: SessionId::new(),
                size_bytes: 12_345_678,
                replicated: true,
                last_accessed_at: Utc::now(),
            }],
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
        assert_eq!(back.warm_pools.len(), 1);
        assert_eq!(back.warm_pools[0].repo, "cortex/api");
        assert_eq!(back.warm_pools[0].ready, 2);
        assert_eq!(back.warm_pools[0].target, 4);
        assert_eq!(back.local_snapshots.len(), 1);
        assert_eq!(
            back.local_snapshots[0].snapshot_id,
            original.local_snapshots[0].snapshot_id
        );
        assert!(back.local_snapshots[0].replicated);
        assert_eq!(back.draining, original.draining);
    }

    #[test]
    fn heartbeat_ack_round_trips_through_json() {
        let original = HeartbeatAck {
            server_time: Utc::now(),
            revoked_sessions: vec![SessionId::new(), SessionId::new()],
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: HeartbeatAck = serde_json::from_str(&json).unwrap();
        assert_eq!(back.revoked_sessions, original.revoked_sessions);
    }

    #[test]
    fn empty_warm_pools_and_snapshots_serialize_as_arrays() {
        // Defending against an accidental switch to Option<Vec<_>> or
        // skip_serializing_if which would change the wire shape and
        // break the host-side parser.
        let mut h = sample();
        h.warm_pools.clear();
        h.local_snapshots.clear();
        let v: serde_json::Value = serde_json::to_value(&h).unwrap();
        assert_eq!(v["warm_pools"], serde_json::json!([]));
        assert_eq!(v["local_snapshots"], serde_json::json!([]));
    }
}
