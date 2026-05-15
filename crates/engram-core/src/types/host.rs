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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostRecord {
    pub id: HostId,
    pub hostname: String,
    pub cloud_metadata: HostMetadata,
    pub capacity: HostCapacity,
    pub status: HostStatus,
    pub last_heartbeat_at: DateTime<Utc>,
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
