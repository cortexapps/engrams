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
    /// ADR 0007: content-addressed manifest pointing at the disk's
    /// chunks in `BlobStorage`, captured at snapshot time. `None`
    /// for backends that haven't wired chunk-store snapshot yet
    /// (FC; lights up with Phase 4's NBD work). `Some` for VZ on
    /// macOS once a `ChunkStore` is attached to its config.
    #[serde(default)]
    pub disk_manifest: Option<super::manifest::ManifestRef>,
}

/// Where a snapshot's bytes currently live. Computed from
/// (`local_path`, `blob_present`) on read; the DB columns are flat,
/// the enum is the convenience.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotResidency {
    /// Bytes only on a host's local NVMe — host-pinned, hot resume.
    Hot,
    /// Bytes on local NVMe AND in cold-tier blob storage. Disk-pressure
    /// detector's "cheap drop" target — clearing the local copy is
    /// free since the cold tier already has it.
    HotAndCold,
    /// Bytes only in cold-tier blob storage. Cross-host resume; coord
    /// drives a download into a target host's local cache before VM
    /// restore.
    Cold,
    /// Both tiers gone. Session is `Dead` — no continuation.
    Gone,
}

/// Persisted row in the `snapshots` table.
///
/// Two-tier residency (ADR 0005):
/// - Hot tier: `local_path` is `Some` — bytes on a host's local NVMe.
///   FC `memory.bin` + `state.json` for sub-second UFFD-backed resume,
///   or APFS-clone of rootfs.ext4 on VZ.
/// - Cold tier: `blob_present` is true — sealed blob ref stored in the
///   accompanying envelope-encryption columns (set by the same
///   `engram-crypto::CredCipher` pattern used for registry creds and
///   session secrets). Restorable on any host with the matching OCI
///   manifest digest cached.
///
/// The two are not exclusive. A session that's been flushed to cold
/// can still have its local copy on disk if the disk-pressure detector
/// hasn't reaped it yet — that intermediate "hot AND cold" state is
/// the disk-pressure detector's cheap-drop target (free disk by
/// removing local files; bytes are safely in blob).
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
    /// Cold-tier presence. `true` ↔ the envelope-encrypted columns
    /// (`wrapped_dek`, `nonce`, `ciphertext`, `key_id`) carry the
    /// sealed blob ref. Indexed for the LRU/disk-pressure scan.
    ///
    /// **ADR 0007 deprecation**: the cold-tier columns retire with
    /// Phase 7 of the chunked-storage rollout. `disk_manifest` is
    /// the replacement durability primitive; this field stays
    /// alongside until the legacy code path is deleted.
    #[serde(default)]
    pub blob_present: bool,
    /// When the cold-tier upload completed. `None` until first flush.
    #[serde(default)]
    pub replicated_at: Option<DateTime<Utc>>,
    /// ADR 0007: content-addressed manifest ref pointing at the
    /// disk's chunks in `BlobStorage`. `Some` for rows produced by
    /// the chunked-snapshot write path (VZ today; FC once Phase 4's
    /// NBD work lands); `None` for legacy rows + backends that
    /// haven't wired chunked snapshot yet. Persisted to the
    /// `disk_manifest_id` + `disk_manifest_version` columns added
    /// by migration 0018.
    #[serde(default)]
    pub disk_manifest: Option<super::manifest::ManifestRef>,
}

impl SnapshotRecord {
    /// Derive the residency state from the flat columns. Cheap; no
    /// allocation. See `SnapshotResidency` for semantics.
    pub fn residency(&self) -> SnapshotResidency {
        match (self.local_path.is_some(), self.blob_present) {
            (true, true) => SnapshotResidency::HotAndCold,
            (true, false) => SnapshotResidency::Hot,
            (false, true) => SnapshotResidency::Cold,
            (false, false) => SnapshotResidency::Gone,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(local: Option<PathBuf>, blob: bool) -> SnapshotRecord {
        SnapshotRecord {
            id: SnapshotId::new(),
            session_id: SessionId::new(),
            host_id: None,
            local_path: local,
            image_version: "test".into(),
            size_bytes: 0,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
            blob_present: blob,
            replicated_at: None,
            disk_manifest: None,
        }
    }

    #[test]
    fn residency_hot_only_when_local_path_set_and_no_blob() {
        let r = record(Some(PathBuf::from("/tmp/snap")), false);
        assert_eq!(r.residency(), SnapshotResidency::Hot);
    }

    #[test]
    fn residency_hot_and_cold_when_both_present() {
        let r = record(Some(PathBuf::from("/tmp/snap")), true);
        assert_eq!(r.residency(), SnapshotResidency::HotAndCold);
    }

    #[test]
    fn residency_cold_only_when_local_cleared_after_flush() {
        let r = record(None, true);
        assert_eq!(r.residency(), SnapshotResidency::Cold);
    }

    #[test]
    fn residency_gone_when_neither_tier_holds_bytes() {
        let r = record(None, false);
        assert_eq!(r.residency(), SnapshotResidency::Gone);
    }
}
