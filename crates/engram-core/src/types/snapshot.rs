use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{HostId, SandboxId, SessionId, SnapshotId};

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
    /// ADR 0045 D4: the IMAGE's base-snapshot memory manifest — the
    /// CANONICAL ref for substrate restores. When set, the UFFD handler
    /// resolves pages still identical to the image base via
    /// `UFFDIO_CONTINUE` against the SHARED per-image base shm file
    /// (one page-cache copy per host across fresh + resumed sessions),
    /// and only session-divergent pages install privately. `None` ⇒
    /// canonical == `memory_manifest` (the pre-D4 behavior; also the
    /// mixed-version fallback — old coordinators simply don't send it).
    #[serde(default)]
    pub base_memory_manifest: Option<super::manifest::ManifestRef>,
    /// ADR 0045 C1: present when this restore is the DESTINATION leg of
    /// a live teleport — everything the dest needs to pull the frozen
    /// source's export and restore from not-yet-durable manifests.
    /// serde-default ⇒ mixed-roll-safe; old hosts ignore it and the
    /// coordinator falls back to snapshot-rehome on `InvalidSpec`.
    #[serde(default)]
    pub migration_source: Option<MigrationSourceInfo>,
    /// ADR 0014: source sandbox_id at snapshot time. Required for
    /// receivers to re-create canonical rootfs/harness symlinks at
    /// `<work_dir>/{rootfs,harness}/<source_sandbox_id>.{dev,ext4}`
    /// before `load_snapshot` opens them. `None` for pre-ADR-0014
    /// snapshots; restore falls back to whatever the FC backend's
    /// `manifest.spec.rootfs_source` says.
    #[serde(default)]
    pub source_sandbox_id: Option<SandboxId>,
    /// ADR 0014: BlobStorage key for the FC `state.bin` artifact
    /// (`snapshots/<snapshot_id>/state.bin`). Receivers download
    /// to `<snapshot_staging>/state.bin` before `load_snapshot`.
    /// `None` when the snapshot stayed local-only (mode=all,
    /// in-process tests).
    #[serde(default)]
    pub state_blob_key: Option<String>,
    /// ADR 0014: BlobStorage key for the FC snapshot sidecar JSON
    /// (`snapshots/<snapshot_id>/sidecar.json`). Sidecar carries
    /// the spec, network config, memory_manifest ref, and source
    /// sandbox_id. Receivers download to
    /// `<snapshot_staging>/manifest.json`.
    #[serde(default)]
    pub sidecar_blob_key: Option<String>,
    /// ADR 0014 M2 interim: BlobStorage key for the tar+zstd-
    /// compressed writable rootfs blob
    /// (`snapshots/<snapshot_id>/rootfs.tar.zst`). Receivers
    /// download + unpack to `manifest.spec.rootfs_source` before
    /// `load_snapshot`. `None` for snapshots whose rootfs is
    /// represented via `disk_manifest` (chunked path) instead.
    #[serde(default)]
    pub rootfs_blob_key: Option<String>,
    /// ADR 0014 M1.14: BlobStorage key for the snapshot's
    /// working-set trace (`traces/<memory_manifest_id>/canonical.json`).
    /// Captured at bake time via a synthetic profiling pass that
    /// restores the just-taken snapshot, exercises mount(2) +
    /// execve(2) on a stub harness, lets the UFFD handler's
    /// recorder accumulate fault hashes for ~5s, then publishes.
    /// Warm-pool refill consumes it to narrow M1.13's parallel
    /// prefetch from "all chunks" to "just the working set,"
    /// shrinking cold-cache refill from ~2s to ~500ms.
    ///
    /// `None` when bake didn't produce a trace. Restore falls back
    /// to M1.13's full-manifest prefetch. (The existing per-host
    /// trace mechanism at `traces/<manifest>/<host_id>.json`
    /// continues to operate independently — the UFFD handler at
    /// runtime accumulates per-host traces from real session
    /// activity. M1.14's canonical trace is the bake-time
    /// pre-seed.)
    #[serde(default)]
    pub working_set_blob_key: Option<String>,
    /// ADR 0035: bundle generations this snapshot's device model
    /// references (resolved `aux_ro_drives`), filled by the host at
    /// snapshot time — base captures AND eviction snapshots, since a
    /// chained restore reopens the same generation. The coord persists
    /// this in `snapshots.aux_bundles`; the union across all rows is the
    /// bundle-GC pin set. Empty for snapshots without aux drives.
    #[serde(default)]
    pub aux_bundles: Vec<super::sandbox::AuxBundleRef>,
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
    /// `None` for template snapshots produced by the image-builder
    /// at bake time (ADR 0014 M1.11): the snapshot is a template
    /// artifact, not a session capture, so the FK to `sessions` is
    /// not meaningful. Session-bound snapshots (idle-eviction,
    /// graceful-drain, M2 background uploader) still set it. The
    /// underlying column was made nullable in migration 0028.
    #[serde(default)]
    pub session_id: Option<SessionId>,
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
    /// ADR 0035: bundle generations this snapshot's device model
    /// references (from `SnapshotMetadata::aux_bundles`). Persisted as
    /// jsonb; the union across all rows is the bundle-GC pin set.
    #[serde(default)]
    pub aux_bundles: Vec<super::sandbox::AuxBundleRef>,
    /// ADR 0028 A.log: the `session_events.idx` high-water-mark at
    /// the checkpoint's pause instant — the third leg of the
    /// (memory, disk, event-log) coherence triple. A rung-1 rewind
    /// tombstones events past this cursor. `None` on pre-0053 rows,
    /// template snapshots, and eviction captures recorded by a coord
    /// that died before resolving it; rung-1 treats `None` as "no
    /// rewind information — surface the boundary without
    /// tombstoning".
    #[serde(default)]
    pub events_cursor: Option<i64>,
}

/// ADR 0045 C1: the destination-side rider on a migration restore's
/// metadata. The coordinator assembles it from `MigrationCaptureOut` +
/// the source's `host_addr`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MigrationSourceInfo {
    pub export_id: String,
    /// h2c URL of the source host-agent, e.g. `http://10.10.0.42:9101`.
    pub source_addr: String,
    pub memory_manifest_json: Vec<u8>,
    pub disk_manifest_json: Vec<u8>,
    pub memory_manifest_ref: super::manifest::ManifestRef,
    pub disk_manifest_ref: super::manifest::ManifestRef,
    pub new_memory_chunk_hashes: Vec<[u8; 32]>,
    pub new_disk_chunk_hashes: Vec<[u8; 32]>,
    /// ADR 0045 C2 (E2B fold): the source guest's hot set in fault
    /// order — the destination pulls these FIRST. Best-effort rider
    /// (empty when the source had no trace); serde-default keeps
    /// mixed rolls safe.
    #[serde(default)]
    pub hot_chunks: Vec<[u8; 32]>,
}

/// ADR 0045 C1: what `migration_capture` hands the coordinator — the
/// frozen sandbox's not-yet-durable next manifests (inline JSON; durable
/// only after the destination's catch-up) plus the transfer set the
/// destination must pull from the source's export.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MigrationCaptureOut {
    pub export_id: String,
    pub memory_manifest_json: Vec<u8>,
    pub disk_manifest_json: Vec<u8>,
    pub memory_manifest_ref: super::manifest::ManifestRef,
    /// PROVISIONAL — shared-template disk lineages version-race; the
    /// destination's catch-up publish does the conflict-retry dance.
    pub disk_manifest_ref: super::manifest::ManifestRef,
    pub new_memory_chunk_hashes: Vec<[u8; 32]>,
    pub new_disk_chunk_hashes: Vec<[u8; 32]>,
    /// ADR 0045 C2 (E2B fold): see `MigrationSourceInfo::hot_chunks`.
    #[serde(default)]
    pub hot_chunks: Vec<[u8; 32]>,
    pub snapshot_id: super::ids::SnapshotId,
    pub paused_at_unix_ms: i64,
}

/// One artifact the destination pulls from a migration export.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MigrationItem {
    StateBin,
    Sidecar,
    Chunk([u8; 32]),
}

/// A frame of `migration_fetch`'s stream.
#[derive(Debug, Clone)]
pub struct MigrationFrame {
    pub item_idx: u32,
    pub offset: u64,
    pub data: bytes::Bytes,
    pub last: bool,
}
