use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{HostId, SessionId, SnapshotId};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotMetadata {
    pub id: SnapshotId,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    /// Image version the source sandbox was launched from.
    pub image_version: String,
}

/// Persisted row in the `snapshots` table. A snapshot may live on local
/// disk, in BlobStorage, or both; the absence of either field is meaningful.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub id: SnapshotId,
    pub session_id: SessionId,
    pub host_id: Option<HostId>,
    pub local_path: Option<PathBuf>,
    pub blob_url: Option<String>,
    pub image_version: String,
    pub size_bytes: u64,
    pub replicated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
}

impl SnapshotRecord {
    /// Whether this snapshot is safe to evict from local disk.
    pub fn evictable(&self) -> bool {
        self.replicated_at.is_some() && self.local_path.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn record(local: Option<PathBuf>, replicated: Option<DateTime<Utc>>) -> SnapshotRecord {
        SnapshotRecord {
            id: SnapshotId::new(),
            session_id: SessionId::new(),
            host_id: None,
            local_path: local,
            blob_url: None,
            image_version: "warm-test".into(),
            size_bytes: 0,
            replicated_at: replicated,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
        }
    }

    #[test]
    fn evictable_requires_both_local_and_replicated() {
        let now = Some(Utc::now());
        let path = Some(PathBuf::from("/tmp/x"));
        assert!(record(path.clone(), now).evictable());
        assert!(
            !record(None, now).evictable(),
            "no local copy: nothing to evict"
        );
        assert!(
            !record(path, None).evictable(),
            "not yet replicated: eviction would lose data"
        );
        assert!(!record(None, None).evictable());
    }
}
