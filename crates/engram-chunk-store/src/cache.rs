//! Local NVMe cache for chunks.
//!
//! Fronts the chunk store with an LRU-bounded local-disk cache so
//! hot reads don't go to GCS. Working-set chunks (those listed in
//! a `WorkingSetTrace`) are pinned — they're not evicted between
//! restores, which is what makes the prefault-on-restore replay
//! actually fast.
//!
//! Fleshed out in the next commit. The skeleton lives here so
//! consumers can import the type names; the LRU + pinning logic
//! lands with the disk adapter (which is the first real caller).

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;

use crate::error::Result;
use crate::manifest::ChunkHash;
use crate::store::ChunkStore;

/// Configuration for the on-disk cache.
#[derive(Clone, Debug)]
pub struct ChunkCacheConfig {
    /// Where cached chunks live. Typically a subdirectory of the
    /// host's NVMe-backed work_dir (e.g.
    /// `/var/lib/engram/chunk-cache/`).
    pub root: PathBuf,
    /// Eviction budget in bytes. The cache enforces this lazily —
    /// each `put` over the budget evicts oldest entries.
    pub budget_bytes: u64,
}

impl ChunkCacheConfig {
    /// Sensible default: cap at 200 GiB.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            budget_bytes: 200 * 1024 * 1024 * 1024,
        }
    }
}

/// Local-disk cache fronting a `ChunkStore`. Cheap to clone.
///
/// **Skeleton**: the full LRU + pinning implementation lands with
/// the disk adapter so I can iterate against a real consumer.
#[derive(Clone)]
pub struct ChunkCache {
    #[allow(dead_code)]
    config: Arc<ChunkCacheConfig>,
    #[allow(dead_code)]
    store: ChunkStore,
}

impl ChunkCache {
    pub fn new(config: ChunkCacheConfig, store: ChunkStore) -> Self {
        Self {
            config: Arc::new(config),
            store,
        }
    }

    /// Get a chunk's bytes, hitting local NVMe first and falling
    /// back to the underlying store on miss.
    ///
    /// **Stub**: today this delegates straight to the store.
    /// Disk-adapter phase wires the local cache.
    pub async fn get(&self, hash: ChunkHash) -> Result<Bytes> {
        self.store.get_chunk(hash).await
    }
}
