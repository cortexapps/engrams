//! Garbage collection.
//!
//! A chunk is **reachable** if at least one live manifest references
//! it. A live manifest is one whose `manifest_id` appears in the
//! coordinator's `snapshots` table (for sandbox state) or
//! `enabled_images` (for base images). Unreachable chunks are safe
//! to delete after a retention window — long enough for in-flight
//! writers (a session that's about to commit a new manifest version
//! referencing a "currently unreachable" chunk) to settle.
//!
//! Two flavors:
//!
//! 1. **Manifest GC** — sweep `manifests/<id>/v*.json` for ids that
//!    no live row references. Drops old versions of a still-live
//!    manifest is a separate concern (caller-driven retention).
//! 2. **Chunk GC** — sweep `chunks/sha256/**` for hashes that no
//!    surviving manifest references.
//!
//! Both require a "list under a prefix" capability on `BlobStorage`
//! which isn't on the trait today. The GC implementation lands once
//! the trait is extended (probably alongside Phase 4 or earlier if
//! disk cache LRU needs it).
//!
//! For now this module exists so the API surface is stable; the
//! actual sweep is a follow-up.

use std::time::Duration;

use crate::error::Result;
use crate::store::ChunkStore;

/// Stats returned from a GC pass. Useful for telemetry + sanity
/// checks (e.g. "we GC'd zero chunks last night, are manifests not
/// being deleted?").
#[derive(Clone, Debug, Default)]
pub struct GcStats {
    pub chunks_deleted: u64,
    pub bytes_freed: u64,
    pub manifest_versions_deleted: u64,
    pub elapsed: Duration,
}

/// Run a GC pass. **Stub** — wired in a follow-up once
/// `BlobStorage` exposes prefix listing.
///
/// `retain_for` is the minimum age an unreachable chunk must reach
/// before it's eligible for deletion. Set to >> the longest live
/// write transaction (e.g. 24h) so a session committing a new
/// manifest version isn't racing the sweep.
pub async fn run(
    _store: &ChunkStore,
    _retain_for: Duration,
    _live_manifest_ids: impl IntoIterator<Item = uuid::Uuid>,
) -> Result<GcStats> {
    // TODO: implement once BlobStorage::list_prefix exists.
    Ok(GcStats::default())
}
