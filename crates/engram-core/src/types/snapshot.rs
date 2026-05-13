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
    /// ADR 0007: content-addressed manifest pointing at the
    /// snapshot's memory chunks (512 KiB) in `BlobStorage`. The
    /// UFFD handler reads this at restore time to resolve per-page
    /// faults against the canonical-base mmap or the session's
    /// divergent chunks. `None` outside FC: VZ's memory snapshot is
    /// broken upstream for arm64 (ADR 0003), so memory chunking
    /// stays FC-only.
    #[serde(default)]
    pub memory_manifest: Option<super::manifest::ManifestRef>,
}

/// Persisted row in the `snapshots` table.
///
/// ADR 0007 single-tier durability: every live snapshot references
/// chunked manifests in `BlobStorage` (the chunk store) — the
/// previous "hot tier" (`local_path`) + "cold tier" (envelope-
/// encrypted blob ref) split is gone. `host_id` is the host that
/// produced the snapshot; restore from a different host is fine
/// as long as the chunked manifests are reachable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub id: SnapshotId,
    pub session_id: SessionId,
    pub host_id: Option<HostId>,
    pub image_version: String,
    pub size_bytes: u64,
    pub created_at: DateTime<Utc>,
    pub last_accessed_at: DateTime<Utc>,
    /// ADR 0007: content-addressed manifest ref pointing at the
    /// disk's chunks in `BlobStorage`. Required for any restore
    /// after Phase 7 deletion landed.
    #[serde(default)]
    pub disk_manifest: Option<super::manifest::ManifestRef>,
    /// ADR 0007 / Phase 5: chunked memory manifest. Set on FC
    /// snapshots whose host wraps the backend in a `PooledBackend`
    /// with a `ChunkStore` attached; `None` for backends that
    /// don't capture memory (VZ + Process) or FC snapshots taken
    /// before chunk-store wiring.
    #[serde(default)]
    pub memory_manifest: Option<super::manifest::ManifestRef>,
    /// ADR 0009: TRUE iff the canonical chunked manifests are
    /// HEAD-verified durable in `BlobStorage` at snapshot-creation
    /// time. The coord's reconcile pass reads this column to decide
    /// whether a session whose sandbox has disappeared transitions
    /// to `Idle` (resumable via cold-tier) or `Dead` (terminal).
    ///
    /// Cleared back to FALSE by the chunk-store GC when it reaps a
    /// referenced manifest. Persistence semantics: column reflects
    /// "as of the last GC sweep, the snapshot was recoverable" —
    /// transient blob backend outages between GC sweeps don't flap
    /// session state.
    ///
    /// Defaults to `false` so pre-migration rows and explicit
    /// failures both surface a session as Dead-on-loss rather than
    /// promising an Idle/resume path that can't be delivered.
    #[serde(default)]
    pub recoverable: bool,
}
