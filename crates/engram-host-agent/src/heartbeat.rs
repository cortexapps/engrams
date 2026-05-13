//! Heartbeat loop. Phase 1 keeps this in-process (the coordinator and
//! host run in the same process when started with --mode=all). Phase 3
//! switches the transport to tonic+gRPC; the wire format is in
//! `engram-protocol`.

use std::time::Duration;

use chrono::Utc;
use engram_protocol::{Heartbeat, HeartbeatAck, HostCapacityReport};

use crate::resource::CapacitySnapshot;

/// Trait the coordinator implements. Phase 1 a single-process binary
/// implements this directly; Phase 3 a tonic client wraps it.
#[async_trait::async_trait]
pub trait HeartbeatSink: Send + Sync {
    async fn send(&self, hb: Heartbeat) -> HeartbeatAck;
}

pub fn build_heartbeat(
    host_id: engram_core::HostId,
    cap: &CapacitySnapshot,
    draining: bool,
) -> Heartbeat {
    Heartbeat {
        host_id,
        sent_at: Utc::now(),
        capacity: HostCapacityReport {
            total_mib: cap.total_mib,
            used_mib: cap.used_mib,
            running_sandboxes: cap.running_sandboxes,
        },
        local_snapshots: Vec::new(),
        draining,
    }
}

pub fn default_interval() -> Duration {
    Duration::from_secs(5)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::HostId;

    #[test]
    fn build_heartbeat_copies_capacity_fields() {
        let host = HostId::new();
        let cap = CapacitySnapshot {
            total_mib: 65_536,
            used_mib: 12_288,
            running_sandboxes: 3,
        };
        let hb = build_heartbeat(host, &cap, false);

        assert_eq!(hb.host_id, host);
        assert_eq!(hb.capacity.total_mib, 65_536);
        assert_eq!(hb.capacity.used_mib, 12_288);
        assert_eq!(hb.capacity.running_sandboxes, 3);
        assert!(!hb.draining);
        // Phase 1: the local snapshot field isn't populated by the
        // helper. It gets filled by the host agent before send.
        assert!(hb.local_snapshots.is_empty());
    }

    #[test]
    fn build_heartbeat_propagates_drain_flag() {
        let cap = CapacitySnapshot::default();
        let hb = build_heartbeat(HostId::new(), &cap, true);
        assert!(hb.draining);
    }

    #[test]
    fn sent_at_is_recent() {
        // We don't pin sent_at, but it must be roughly "now" — verifying
        // catches a bug where someone replaces Utc::now() with epoch 0.
        let before = chrono::Utc::now();
        let hb = build_heartbeat(HostId::new(), &CapacitySnapshot::default(), false);
        let after = chrono::Utc::now();
        assert!(hb.sent_at >= before);
        assert!(hb.sent_at <= after);
    }
}
