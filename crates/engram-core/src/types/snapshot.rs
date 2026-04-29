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

/// Persisted row in the `snapshots` table. Snapshots are local-NVMe-only
/// — they support fast same-host hot-resume for hot-suspended sessions.
/// Cross-host durability is git, not snapshots; see `DESIGN.md` Phase 4.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub id: SnapshotId,
    pub session_id: SessionId,
    pub host_id: Option<HostId>,
    pub local_path: Option<PathBuf>,
    pub image_version: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
}
