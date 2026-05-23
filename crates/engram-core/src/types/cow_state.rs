//! COW-tier diagnostic state for a single sandbox.
//!
//! ADR 0016 Phase A. The shape the `CowState` / `CowStateAll` RPCs
//! return to coord, and that coord projects to the
//! `GET /api/{hosts,sessions}/:id/cow-state` endpoints.
//!
//! Three things to know reading this file:
//!
//! 1. **All fields are observational.** Nothing here drives behaviour
//!    on host or coord. The diagnostic endpoint reads, the web app
//!    renders, the operator reasons. The Phase B continuous-flush
//!    scheduler will derive its own threshold trigger from
//!    `ChunkedDiskBackend::dirty_bytes` directly, not from this
//!    serialised struct.
//!
//! 2. **Live vs. durable.** The disk-tier fields describe the LIVE
//!    state on the host (dirty buffer + current manifest version);
//!    the memory-tier fields describe the most recent DURABLE state
//!    (last snapshot's memory manifest, last snapshot timestamp).
//!    There is no "live memory" tier — see ADR 0016 §Decision for
//!    why continuous memory sync is out of scope.
//!
//! 3. **Wire shape.** Serialised over gRPC as a bincode blob today
//!    (same pattern as `SnapshotMetadata`), matching ADR 0013's
//!    "structurally-complex payloads stay bincode-encoded inside a
//!    `bytes` field" guidance. The proto `CowStateResponse` message
//!    is the transport envelope; this struct is the payload.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::SandboxId;
use super::manifest::ManifestRef;

/// COW state for a single sandbox. Returned by
/// `HostClient::cow_state(sandbox_id)`. Roll-up over multiple
/// sandboxes uses [`CowStateRecord`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CowState {
    // -- disk tier (live) -----------------------------------------
    /// Current disk manifest version. Ticks on every successful
    /// `flush()` (snapshot-driven or scheduler-driven, Phase B).
    /// Always present — a sandbox without a disk-manifest backend
    /// (Process / VZ-without-NBD) doesn't appear in `CowStateAll`
    /// at all.
    pub disk_manifest: ManifestRef,
    /// Number of dirty chunks resident in the host's RAM buffer.
    /// `0` for steady-state read-only sessions or right after a
    /// flush. See `ChunkedDiskBackend::dirty_chunks_count`.
    pub dirty_chunks: u32,
    /// Total bytes in the dirty buffer. 16 MiB per chunk in the
    /// typical FC config. Counts the whole chunk buffer, not just
    /// the patched ranges within it (a 1-byte write materialises
    /// the full 16 MiB chunk). See
    /// `ChunkedDiskBackend::dirty_bytes`.
    pub dirty_bytes: u64,
    /// Unix-millis timestamp of the last successful `flush()`. `0`
    /// = never flushed since this backend was constructed (the
    /// `ChunkedDiskBackend::last_flush_unix_ms` sentinel). The
    /// renderer interprets `0` as "never" rather than 1970-01-01.
    pub last_flush_unix_ms: i64,
    /// Total chunks the disk manifest references (base chunks
    /// only — dirty chunks aren't yet in the manifest until
    /// `flush()` rolls them in). `0` for a manifest with only
    /// sparse / zero-filled holes.
    pub base_chunks: u32,
    /// Of `base_chunks`, how many are currently resident on the
    /// host's local NVMe `ChunkCache`. Read-only stat over the
    /// cache (see `ChunkCache::contains`). Reads on chunks NOT in
    /// the cache fall through to BlobStorage; the operator-visible
    /// signal is "warm vs. cold per session".
    pub base_chunks_local: u32,

    // -- memory tier (durable; snapshot-bounded) ------------------
    /// Most recent memory-manifest ref captured in a snapshot for
    /// the session bound to this sandbox. `None` if the sandbox
    /// has never been snapshotted (Active-never-idle); see
    /// `last_snapshot_unix_ms` for the same signal at backend
    /// granularity. This field is wire-side enrichment by coord
    /// (it joins the latest `snapshots` row), not host-side data —
    /// the host doesn't know the session's snapshot history.
    #[serde(default)]
    pub memory_manifest: Option<ManifestRef>,
    /// Unix-millis timestamp of the last successful `snapshot()`
    /// for this sandbox's underlying VM. `0` = never snapshotted.
    /// Coord projects this from `snapshots.created_at` of the
    /// latest row; host-side reading via
    /// `SandboxBackend::last_snapshot_unix_ms` is the per-host
    /// fallback when the session isn't joined yet.
    pub last_snapshot_unix_ms: i64,
}

/// One entry in [`HostClient::cow_state_all`]. The host returns one
/// of these per sandbox it currently hosts; coord joins to
/// `MetadataStore::host_for_sandbox` (M3) to map each
/// `sandbox_id` back to its session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CowStateRecord {
    pub sandbox_id: SandboxId,
    pub state: CowState,
}

/// Helper for the API layer: convert a unix-ms field to an
/// `Option<DateTime<Utc>>`, mapping the `0` sentinel to `None`.
/// Centralises the "never" semantic so renderers don't reinvent
/// the check.
pub fn unix_ms_to_dt(ms: i64) -> Option<DateTime<Utc>> {
    if ms <= 0 {
        return None;
    }
    let secs = ms / 1000;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    DateTime::from_timestamp(secs, nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ids::SandboxId;
    use crate::types::manifest::ManifestRef;

    #[test]
    fn unix_ms_to_dt_zero_is_none() {
        assert!(unix_ms_to_dt(0).is_none());
        assert!(unix_ms_to_dt(-1).is_none());
    }

    #[test]
    fn unix_ms_to_dt_round_trips_a_known_instant() {
        // 2026-05-23T13:42:00Z = 1779565320 unix-secs.
        let ms = 1_779_565_320_000_i64 + 250;
        let dt = unix_ms_to_dt(ms).unwrap();
        assert_eq!(dt.timestamp_millis(), ms);
    }

    fn synth_cow_state() -> CowState {
        CowState {
            disk_manifest: ManifestRef::new(),
            dirty_chunks: 3,
            dirty_bytes: 48 * 1024 * 1024,
            last_flush_unix_ms: 1_779_565_320_000,
            base_chunks: 256,
            base_chunks_local: 240,
            memory_manifest: Some(ManifestRef::new()),
            last_snapshot_unix_ms: 1_779_565_000_000,
        }
    }

    #[test]
    fn cow_state_bincode_round_trips() {
        // Wire shape used by `CowStateResponse.state_bincode`. The
        // proto carries the encoded bytes; decode must reproduce
        // every field including the `memory_manifest` Some/None
        // distinction.
        let s = synth_cow_state();
        let enc = bincode::serialize(&s).unwrap();
        let back: CowState = bincode::deserialize(&enc).unwrap();
        assert_eq!(back.disk_manifest, s.disk_manifest);
        assert_eq!(back.dirty_chunks, s.dirty_chunks);
        assert_eq!(back.dirty_bytes, s.dirty_bytes);
        assert_eq!(back.last_flush_unix_ms, s.last_flush_unix_ms);
        assert_eq!(back.base_chunks, s.base_chunks);
        assert_eq!(back.base_chunks_local, s.base_chunks_local);
        assert_eq!(back.memory_manifest, s.memory_manifest);
        assert_eq!(back.last_snapshot_unix_ms, s.last_snapshot_unix_ms);
    }

    #[test]
    fn cow_state_record_vec_bincode_round_trips() {
        // Wire shape used by `CowStateAllResponse.records_bincode`.
        // Vec round-trip + per-record identity. The host-agent's
        // `cow_state_all` returns one entry per chunk-tracked
        // sandbox; this test pins the structural assumption that an
        // empty list and a populated list both decode cleanly.
        let records: Vec<CowStateRecord> = vec![CowStateRecord {
            sandbox_id: SandboxId::new(),
            state: synth_cow_state(),
        }];
        let enc = bincode::serialize(&records).unwrap();
        let back: Vec<CowStateRecord> = bincode::deserialize(&enc).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].sandbox_id, records[0].sandbox_id);
        assert_eq!(back[0].state.dirty_chunks, records[0].state.dirty_chunks);

        let empty: Vec<CowStateRecord> = Vec::new();
        let enc = bincode::serialize(&empty).unwrap();
        // Non-empty bytes — the length prefix is 0 but bincode emits
        // 8 bytes for the length field by default.
        assert!(!enc.is_empty());
        let back: Vec<CowStateRecord> = bincode::deserialize(&enc).unwrap();
        assert!(back.is_empty());
    }
}
