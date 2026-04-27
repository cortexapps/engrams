//! Snapshot manager. Two-tier storage: hot local NVMe, cold BlobStorage.
//! Phase 2 lights this up; the surface here is the eviction LRU and
//! upload-trigger machinery.
//!
//! See DESIGN.md "Snapshot manager (lives inside host agent)".

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use engram_core::traits::BlobStorage;
use engram_core::types::ids::{SessionId, SnapshotId};
use parking_lot::RwLock;

#[derive(Clone, Debug)]
pub struct LocalSnapshot {
    pub id: SnapshotId,
    pub session_id: SessionId,
    pub local_path: PathBuf,
    pub size_bytes: u64,
    pub replicated_at: Option<DateTime<Utc>>,
    pub last_accessed_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct SnapshotManager {
    blob: Arc<dyn BlobStorage>,
    inner: Arc<RwLock<SnapshotInner>>,
    cap_bytes: u64,
}

#[derive(Default)]
struct SnapshotInner {
    by_id: BTreeMap<SnapshotId, LocalSnapshot>,
    used_bytes: u64,
}

impl SnapshotManager {
    pub fn new(blob: Arc<dyn BlobStorage>, cap_bytes: u64) -> Self {
        Self {
            blob,
            inner: Arc::new(RwLock::new(SnapshotInner::default())),
            cap_bytes,
        }
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    pub fn used_bytes(&self) -> u64 {
        self.inner.read().used_bytes
    }

    pub fn record_local(&self, snap: LocalSnapshot) {
        let mut g = self.inner.write();
        g.used_bytes = g.used_bytes.saturating_add(snap.size_bytes);
        g.by_id.insert(snap.id, snap);
    }

    pub fn touch(&self, id: SnapshotId) {
        if let Some(s) = self.inner.write().by_id.get_mut(&id) {
            s.last_accessed_at = Utc::now();
        }
    }

    /// Return ids whose replicated copy makes them safe to evict, in
    /// LRU order.
    pub fn lru_evictable(&self) -> Vec<SnapshotId> {
        let g = self.inner.read();
        let mut v: Vec<_> = g
            .by_id
            .values()
            .filter(|s| s.replicated_at.is_some())
            .collect();
        v.sort_by_key(|s| s.last_accessed_at);
        v.into_iter().map(|s| s.id).collect()
    }

    /// Phase 2: stream `local_path` -> `blob.put(key, ...)` with zstd-3
    /// compression in flight, then mark `replicated_at`.
    pub async fn replicate(&self, _id: SnapshotId) {
        let _ = &self.blob;
        // TODO(phase-2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use engram_core::SessionId;
    use engram_storage_local::LocalStorage;

    fn snap(id: SnapshotId, replicated: bool, accessed_secs_ago: i64, size: u64) -> LocalSnapshot {
        let now = Utc::now();
        LocalSnapshot {
            id,
            session_id: SessionId::new(),
            local_path: PathBuf::from(format!("/tmp/{id}")),
            size_bytes: size,
            replicated_at: replicated.then(|| now - ChronoDuration::seconds(accessed_secs_ago)),
            last_accessed_at: now - ChronoDuration::seconds(accessed_secs_ago),
        }
    }

    fn manager() -> SnapshotManager {
        // Cap is intentionally small so eviction tests are easy to reason about.
        let blob = Arc::new(LocalStorage::new("/tmp/engram-snapshot-test"));
        SnapshotManager::new(blob, 1_024 * 1_024)
    }

    #[test]
    fn used_bytes_accumulates_across_records() {
        let m = manager();
        let a = snap(SnapshotId::new(), true, 0, 1_000);
        let b = snap(SnapshotId::new(), false, 0, 2_500);
        m.record_local(a);
        m.record_local(b);
        assert_eq!(m.used_bytes(), 3_500);
    }

    #[test]
    fn lru_evictable_excludes_unreplicated() {
        let m = manager();
        let id_replicated = SnapshotId::new();
        let id_pending = SnapshotId::new();
        m.record_local(snap(id_replicated, true, 100, 0));
        m.record_local(snap(id_pending, false, 1_000, 0));

        let evictable = m.lru_evictable();
        assert_eq!(
            evictable,
            vec![id_replicated],
            "unreplicated snapshots must not be eligible for eviction"
        );
    }

    #[test]
    fn lru_evictable_orders_oldest_first() {
        let m = manager();
        let newest = SnapshotId::new();
        let middle = SnapshotId::new();
        let oldest = SnapshotId::new();
        m.record_local(snap(newest, true, 1, 0));
        m.record_local(snap(middle, true, 60, 0));
        m.record_local(snap(oldest, true, 600, 0));
        // Order in by_id is irrelevant; LRU returns coldest first.
        assert_eq!(m.lru_evictable(), vec![oldest, middle, newest]);
    }

    #[test]
    fn touch_promotes_snapshot_to_most_recently_used() {
        let m = manager();
        let id_old = SnapshotId::new();
        let id_new = SnapshotId::new();
        m.record_local(snap(id_old, true, 600, 0));
        m.record_local(snap(id_new, true, 1, 0));
        // Before touch: id_old is the oldest.
        assert_eq!(m.lru_evictable().first().copied(), Some(id_old));

        // After touch on id_old, it should no longer be the LRU candidate.
        m.touch(id_old);
        let after = m.lru_evictable();
        assert_eq!(after.first().copied(), Some(id_new));
    }

    #[test]
    fn touch_on_unknown_id_is_a_noop() {
        let m = manager();
        let known = SnapshotId::new();
        m.record_local(snap(known, true, 60, 0));
        // touching an id we don't have shouldn't panic or perturb state
        m.touch(SnapshotId::new());
        assert_eq!(m.lru_evictable(), vec![known]);
    }
}
