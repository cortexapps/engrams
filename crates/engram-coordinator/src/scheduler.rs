//! Scheduling logic. Phase 1: trivial — there's only one host (the
//! local one) and one in-process SandboxBackend, so every session
//! lands here. Phase 3 grows this into a real router that picks
//! "host with snapshot local → host with capacity". (Pre-v5 also had
//! a warm-pool tier; deleted with ADR 0008.)

use engram_core::types::HostRecord;

pub fn pick_host_for_session<'a>(
    hosts: &'a [HostRecord],
    _session_repo: &str,
) -> Option<&'a HostRecord> {
    hosts.iter().find(|h| {
        matches!(
            h.status,
            engram_core::types::HostStatus::Ready | engram_core::types::HostStatus::Draining
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use engram_core::types::{HostCapacity, HostMetadata, HostStatus};
    use engram_core::HostId;

    fn host(name: &str, status: HostStatus) -> HostRecord {
        HostRecord {
            id: HostId::new(),
            hostname: name.into(),
            cloud_metadata: HostMetadata::default(),
            capacity: HostCapacity {
                total_gb: 100,
                used_gb: 10,
                total_mib: 0,
                used_mib: 0,
                running_sandboxes: 0,
            },
            utilization: Default::default(),
            status,
            last_heartbeat_at: Utc::now(),
            host_addr: None,
            ready_images: Vec::new(),
            local_snapshots: Vec::new(),
            current_bundles: Vec::new(),
            cordoned: false,
            total_vcpus: 0,
            wire_version: 0,
            stages_images: false,
            capabilities: engram_core::types::host::HostCapabilities::default(),
        }
    }

    #[test]
    fn returns_none_when_no_hosts() {
        assert!(pick_host_for_session(&[], "repo").is_none());
    }

    #[test]
    fn picks_first_ready_host() {
        let hosts = vec![host("a", HostStatus::Ready), host("b", HostStatus::Ready)];
        let picked = pick_host_for_session(&hosts, "repo").unwrap();
        assert_eq!(picked.hostname, "a");
    }

    #[test]
    fn skips_dead_hosts_in_favor_of_ready() {
        let hosts = vec![
            host("dead-1", HostStatus::Dead),
            host("ready-1", HostStatus::Ready),
        ];
        let picked = pick_host_for_session(&hosts, "repo").unwrap();
        assert_eq!(picked.hostname, "ready-1");
    }

    #[test]
    fn draining_host_is_acceptable_when_no_ready_host_exists() {
        // Draining hosts can take work; they just shouldn't be preferred
        // over Ready ones. With no Ready host available, scheduler falls
        // through to a Draining one rather than returning None.
        let hosts = vec![
            host("dead-1", HostStatus::Dead),
            host("draining-1", HostStatus::Draining),
        ];
        let picked = pick_host_for_session(&hosts, "repo").unwrap();
        assert_eq!(picked.hostname, "draining-1");
    }

    #[test]
    fn all_dead_returns_none() {
        let hosts = vec![host("d1", HostStatus::Dead), host("d2", HostStatus::Dead)];
        assert!(pick_host_for_session(&hosts, "repo").is_none());
    }
}
