//! Coord-side COW diagnostic cache + projection helpers (ADR 0016
//! Phase A).
//!
//! The host-agent reports per-sandbox COW state via the
//! `CowState` / `CowStateAll` RPCs added in commit 3. Two coord
//! endpoints surface that data:
//!
//! - `GET /api/hosts/:id/cow-state` — fan out `cow_state_all()` to
//!   the one host, map each `sandbox_id` back to its session, return
//!   per-session rows.
//! - `GET /api/sessions/:id/cow-state` — look up the session, find
//!   its current host, call `cow_state(sandbox_id)`, project the PG
//!   `snapshots` row into the memory-tier fields.
//!
//! The cache here exists because the web app polls these endpoints
//! every ~1–2 s. A naive fan-out per request would hit every host
//! N×heartbeat_cadence times faster than its actual diagnostic
//! rate. Cache TTL is 1 second — well under any reasonable polling
//! interval, well over the rate at which dirty-byte counts move on a
//! human timescale.
//!
//! Wire shape returned to the API layer is
//! [`engram_core::types::cow_state::CowState`] / `CowStateRecord` —
//! the same bincode-able structs the host returns. Coord-only
//! enrichment (joining `snapshots.memory_manifest`, joining
//! `session_id`) lives on a thin wrapper that serializes as JSON.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use engram_core::traits::HostClient;
use engram_core::types::cow_state::{CowState, CowStateRecord};
use engram_core::HostId;
use engram_core::SandboxError;
use parking_lot::Mutex;

/// Per-host COW cache. Owns a `(captured_at, records)` snapshot per
/// host; reads return the cached value while it's within
/// `Self::ttl`, otherwise the caller must repopulate via the
/// supplied refresh function.
///
/// Lock granularity: one `Mutex` per host slot, not a global lock.
/// Concurrent reads against different hosts proceed in parallel; a
/// concurrent read against the same host serializes briefly during
/// the refresh window (and only one of them ends up calling the
/// host).
pub struct CowStateCache {
    /// Per-host slot: an `Arc<Mutex<Option<(captured_at, records)>>>`.
    /// `Arc` is so callers can hold a slot reference across the
    /// host RPC await without keeping the outer DashMap entry
    /// locked.
    slots: dashmap::DashMap<HostId, Arc<Mutex<Slot>>>,
    ttl: Duration,
}

#[derive(Default)]
struct Slot {
    captured_at: Option<Instant>,
    records: Vec<CowStateRecord>,
}

impl CowStateCache {
    /// Default TTL (1 second) matches the web app's polling shape.
    /// Production may tune this if the diagnostic load shows up in
    /// host-RPC histograms; for now the dial is hard-coded.
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(1))
    }

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            slots: dashmap::DashMap::new(),
            ttl,
        }
    }

    /// Fetch the cached records for `host_id`, refreshing via
    /// `refresh` if the slot is empty or stale. `refresh` is only
    /// invoked when we hold the slot's lock, so concurrent callers
    /// for the same host coalesce naturally (the second waiter sees
    /// the just-refreshed value and skips its own RPC).
    pub async fn get_or_refresh<F, Fut>(
        &self,
        host_id: HostId,
        refresh: F,
    ) -> Result<Vec<CowStateRecord>, SandboxError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Vec<CowStateRecord>, SandboxError>>,
    {
        let slot = self
            .slots
            .entry(host_id)
            .or_insert_with(|| Arc::new(Mutex::new(Slot::default())))
            .clone();

        // Fast path: check freshness under the lock, return a clone
        // if still fresh. Drop the lock BEFORE the await so the
        // miss path doesn't hold a sync lock across the RPC.
        {
            let guard = slot.lock();
            if let Some(captured) = guard.captured_at {
                if captured.elapsed() < self.ttl {
                    return Ok(guard.records.clone());
                }
            }
        }

        let fresh = refresh().await?;
        let mut guard = slot.lock();
        // Double-check: another caller may have refreshed while we
        // were awaiting our own RPC. If so, prefer the newer cached
        // value (theirs may include a sandbox we sampled before it
        // existed). Tie-break: if both have the same captured_at,
        // ours wins by virtue of being the one currently holding
        // the lock.
        let now = Instant::now();
        let theirs_newer = guard
            .captured_at
            .map(|t| t > now - self.ttl)
            .unwrap_or(false);
        if !theirs_newer {
            guard.captured_at = Some(now);
            guard.records = fresh.clone();
        }
        Ok(if theirs_newer {
            guard.records.clone()
        } else {
            fresh
        })
    }

    /// Drop the slot for `host_id`. Called when a host is
    /// unregistered (so a re-registration starts with a clean
    /// cache). Idempotent.
    pub fn forget_host(&self, host_id: HostId) {
        self.slots.remove(&host_id);
    }
}

impl Default for CowStateCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Fetch a single host's COW snapshot through the cache, calling
/// `host_client.cow_state_all()` on miss. Returns the raw
/// `CowStateRecord`s; the API layer does the session-id join.
pub async fn fetch_for_host(
    cache: &CowStateCache,
    host_id: HostId,
    host_client: Arc<dyn HostClient>,
) -> Result<Vec<CowStateRecord>, SandboxError> {
    cache
        .get_or_refresh(
            host_id,
            move || async move { host_client.cow_state_all().await },
        )
        .await
}

/// Project a host-side [`CowState`] + the latest snapshot row's
/// memory-tier columns into the JSON shape the web app consumes.
/// `memory_manifest` and `last_snapshot_unix_ms` are coord-side
/// enrichment — the host doesn't know the session's snapshot
/// history.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CowStateView {
    pub sandbox_id: engram_core::SandboxId,
    pub session_id: Option<engram_core::SessionId>,
    pub disk_manifest_id: String,
    pub disk_manifest_version: u64,
    pub dirty_chunks: u32,
    pub dirty_bytes: u64,
    pub last_flush_at: Option<DateTime<Utc>>,
    pub base_chunks: u32,
    pub base_chunks_local: u32,
    pub memory_manifest_id: Option<String>,
    pub memory_manifest_version: Option<u64>,
    pub last_snapshot_at: Option<DateTime<Utc>>,
}

impl CowStateView {
    pub fn from_record(
        record: &CowStateRecord,
        session_id: Option<engram_core::SessionId>,
        memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
        last_snapshot_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self::from_parts(
            record.sandbox_id,
            session_id,
            &record.state,
            memory_manifest,
            last_snapshot_at,
        )
    }

    pub fn from_parts(
        sandbox_id: engram_core::SandboxId,
        session_id: Option<engram_core::SessionId>,
        state: &CowState,
        memory_manifest: Option<engram_core::types::manifest::ManifestRef>,
        last_snapshot_at: Option<DateTime<Utc>>,
    ) -> Self {
        // Memory-tier last-snapshot timestamp: prefer the PG-joined
        // `snapshots.created_at` (durable, cross-replica) over the
        // host's in-memory `last_snapshot_unix_ms` (lost on
        // host-agent restart). Fall back to the host's stamp if PG
        // has nothing yet (e.g., the snapshot is in flight,
        // host-side stamped, coord-side not committed). The `0`
        // sentinel on the host stamp encodes "never" and is
        // converted by `unix_ms_to_dt`.
        let snapshot_at = last_snapshot_at
            .or_else(|| engram_core::types::cow_state::unix_ms_to_dt(state.last_snapshot_unix_ms));
        Self {
            sandbox_id,
            session_id,
            disk_manifest_id: state.disk_manifest.manifest_id.to_string(),
            disk_manifest_version: state.disk_manifest.version,
            dirty_chunks: state.dirty_chunks,
            dirty_bytes: state.dirty_bytes,
            last_flush_at: engram_core::types::cow_state::unix_ms_to_dt(state.last_flush_unix_ms),
            base_chunks: state.base_chunks,
            base_chunks_local: state.base_chunks_local,
            memory_manifest_id: memory_manifest.map(|m| m.manifest_id.to_string()),
            memory_manifest_version: memory_manifest.map(|m| m.version),
            last_snapshot_at: snapshot_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::manifest::ManifestRef;
    use engram_core::SandboxId;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn synth_record() -> CowStateRecord {
        CowStateRecord {
            sandbox_id: SandboxId::new(),
            state: CowState {
                disk_manifest: ManifestRef::new(),
                dirty_chunks: 2,
                dirty_bytes: 32 * 1024 * 1024,
                last_flush_unix_ms: 0,
                base_chunks: 16,
                base_chunks_local: 12,
                memory_manifest: None,
                last_snapshot_unix_ms: 0,
            },
        }
    }

    #[tokio::test]
    async fn cache_hit_skips_refresh() {
        let cache = CowStateCache::with_ttl(Duration::from_secs(60));
        let host = HostId::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let calls_clone = calls.clone();
        let first = cache
            .get_or_refresh(host, || async move {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(vec![synth_record()])
            })
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call within TTL → no second RPC.
        let calls_clone = calls.clone();
        let second = cache
            .get_or_refresh(host, || async move {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            })
            .await
            .unwrap();
        assert_eq!(second.len(), 1, "must return the cached value");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "refresh closure must not have been invoked",
        );
    }

    #[tokio::test]
    async fn cache_miss_after_ttl_refreshes() {
        let cache = CowStateCache::with_ttl(Duration::from_millis(5));
        let host = HostId::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let calls_clone = calls.clone();
        let _ = cache
            .get_or_refresh(host, || async move {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(vec![synth_record()])
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let calls_clone = calls.clone();
        let _ = cache
            .get_or_refresh(host, || async move {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(vec![synth_record()])
            })
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "stale entry must trigger a fresh RPC",
        );
    }

    #[tokio::test]
    async fn forget_host_drops_slot() {
        let cache = CowStateCache::with_ttl(Duration::from_secs(60));
        let host = HostId::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let calls_clone = calls.clone();
        let _ = cache
            .get_or_refresh(host, || async move {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(vec![synth_record()])
            })
            .await
            .unwrap();
        cache.forget_host(host);

        let calls_clone = calls.clone();
        let _ = cache
            .get_or_refresh(host, || async move {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(vec![synth_record()])
            })
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "forget_host must force a fresh RPC",
        );
    }

    #[test]
    fn cow_state_view_uses_pg_snapshot_when_provided() {
        // PG-joined `last_snapshot_at` wins over the host's
        // in-memory `last_snapshot_unix_ms` — the host stamp is
        // host-local and resets on host-agent restart; PG is the
        // durable cross-replica answer. The host stamp is only
        // consulted as a fallback (e.g., snapshot in flight,
        // host-side stamped but coord-side not committed).
        let pg_at = DateTime::from_timestamp(1_779_565_000, 0).unwrap();
        let host_stamp = 1_779_500_000_000_i64;
        let mut s = synth_record().state;
        s.last_snapshot_unix_ms = host_stamp;
        let view = CowStateView::from_parts(SandboxId::new(), None, &s, None, Some(pg_at));
        assert_eq!(view.last_snapshot_at, Some(pg_at));
    }

    #[test]
    fn cow_state_view_falls_back_to_host_stamp_when_pg_silent() {
        let host_stamp = 1_779_565_100_000_i64;
        let mut s = synth_record().state;
        s.last_snapshot_unix_ms = host_stamp;
        let view = CowStateView::from_parts(SandboxId::new(), None, &s, None, None);
        assert_eq!(
            view.last_snapshot_at.unwrap().timestamp_millis(),
            host_stamp,
        );
    }
}
