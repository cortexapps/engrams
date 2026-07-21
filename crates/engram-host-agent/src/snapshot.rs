//! Snapshot manager: in-memory LRU bookkeeping for local-NVMe snapshots.
//!
//! After Phase 4's blob removal, snapshots are local-NVMe-only — they
//! support fast same-host hot-resume for hot-suspended sessions. Cross-
//! host durability is git, not snapshots; see `DESIGN.md` Phase 4. This
//! manager just tracks (id, path, size, last-access) for LRU eviction
//! when the local snapshot directory fills past `cap_bytes`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use engram_core::traits::{Clock, SystemClock};
use engram_core::types::ids::{SessionId, SnapshotId};
use parking_lot::RwLock;

#[derive(Clone, Debug)]
pub struct LocalSnapshot {
    pub id: SnapshotId,
    pub session_id: SessionId,
    pub local_path: PathBuf,
    pub size_bytes: u64,
    pub last_accessed_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct SnapshotManager {
    inner: Arc<RwLock<SnapshotInner>>,
    cap_bytes: u64,
    /// ADR 0098 D1: wall clock is an injected world input (the LRU
    /// `last_accessed_at` mark feeds eviction). P1 wires the production
    /// clock; sim injection rides the flow-extraction PRs.
    clock: Arc<dyn Clock>,
}

#[derive(Default)]
struct SnapshotInner {
    by_id: BTreeMap<SnapshotId, LocalSnapshot>,
    used_bytes: u64,
}

impl SnapshotManager {
    pub fn new(cap_bytes: u64) -> Self {
        Self {
            inner: Arc::new(RwLock::new(SnapshotInner::default())),
            cap_bytes,
            clock: Arc::new(SystemClock::new()),
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
            s.last_accessed_at = self.clock.now_utc();
        }
    }

    /// Return ids in LRU order (oldest-accessed first), so callers can
    /// evict the coldest snapshots first when the local cap is exceeded.
    pub fn lru(&self) -> Vec<SnapshotId> {
        let g = self.inner.read();
        let mut v: Vec<_> = g.by_id.values().collect();
        v.sort_by_key(|s| s.last_accessed_at);
        v.into_iter().map(|s| s.id).collect()
    }
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use chrono::Duration as ChronoDuration;
    use engram_core::SessionId;

    fn snap(id: SnapshotId, accessed_secs_ago: i64, size: u64) -> LocalSnapshot {
        let now = Utc::now();
        LocalSnapshot {
            id,
            session_id: SessionId::new(),
            local_path: PathBuf::from(format!("/tmp/{id}")),
            size_bytes: size,
            last_accessed_at: now - ChronoDuration::seconds(accessed_secs_ago),
        }
    }

    fn manager() -> SnapshotManager {
        SnapshotManager::new(1_024 * 1_024)
    }

    #[test]
    fn used_bytes_accumulates_across_records() {
        let m = manager();
        m.record_local(snap(SnapshotId::new(), 0, 1_000));
        m.record_local(snap(SnapshotId::new(), 0, 2_500));
        assert_eq!(m.used_bytes(), 3_500);
    }

    #[test]
    fn lru_orders_oldest_first() {
        let m = manager();
        let newest = SnapshotId::new();
        let middle = SnapshotId::new();
        let oldest = SnapshotId::new();
        m.record_local(snap(newest, 1, 0));
        m.record_local(snap(middle, 60, 0));
        m.record_local(snap(oldest, 600, 0));
        assert_eq!(m.lru(), vec![oldest, middle, newest]);
    }

    #[test]
    fn touch_promotes_snapshot_to_most_recently_used() {
        let m = manager();
        let id_old = SnapshotId::new();
        let id_new = SnapshotId::new();
        m.record_local(snap(id_old, 600, 0));
        m.record_local(snap(id_new, 1, 0));
        assert_eq!(m.lru().first().copied(), Some(id_old));

        m.touch(id_old);
        assert_eq!(m.lru().first().copied(), Some(id_new));
    }

    #[test]
    fn touch_on_unknown_id_is_a_noop() {
        let m = manager();
        let known = SnapshotId::new();
        m.record_local(snap(known, 60, 0));
        m.touch(SnapshotId::new());
        assert_eq!(m.lru(), vec![known]);
    }
}
