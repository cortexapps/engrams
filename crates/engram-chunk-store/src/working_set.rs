//! Working-set traces.
//!
//! The UFFD page-fault handler on Linux+FC records which chunks
//! were faulted in during the first N seconds after a restore.
//! That ordered list is the **working-set trace**. On subsequent
//! restores of the same manifest, the handler prefaults the same
//! chunks before letting vCPUs run — REAP/FaaSnap-style — which
//! collapses fault-storm serialization and lets cold-resume hit
//! sub-100ms after the first warm-up.
//!
//! Traces are host-local: a host that's never run this manifest
//! before doesn't have a trace for it. The first restore on a host
//! pays full UFFD-fault cost, records a trace, and persists it.
//! Subsequent restores on the same host (or other hosts that
//! download the trace) get the prefault benefit.
//!
//! Stored under `traces/<manifest_id>/<host_id>.json` so concurrent
//! hosts don't race over a single trace file. The chunk-store API
//! can also publish a "canonical" trace under
//! `traces/<manifest_id>/canonical.json` for base images (captured
//! at bake time by `engram-rootfs-materializer`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manifest::ChunkHash;

/// Pointer to a stored trace. Like `ManifestRef` but for traces.
/// `host_id` is `None` for the canonical (image-bake-time) trace.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct TraceRef {
    pub manifest_id: Uuid,
    /// `None` ⇒ the canonical trace captured at image-bake time.
    /// `Some(host_id)` ⇒ a per-host recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<Uuid>,
}

impl TraceRef {
    /// Canonical trace produced by `engram-rootfs-materializer` for a
    /// base image. Loaded by every host on first restore of that
    /// image, before vCPUs run.
    pub fn canonical(manifest_id: Uuid) -> Self {
        Self {
            manifest_id,
            host_id: None,
        }
    }

    /// Per-host trace recorded during runtime.
    pub fn host(manifest_id: Uuid, host_id: Uuid) -> Self {
        Self {
            manifest_id,
            host_id: Some(host_id),
        }
    }

    /// Object-storage key.
    pub fn storage_key(&self) -> String {
        match self.host_id {
            None => format!("traces/{}/canonical.json", self.manifest_id),
            Some(host) => format!("traces/{}/{host}.json", self.manifest_id),
        }
    }
}

/// The recorded ordered list of chunks accessed during the first N
/// seconds after a restore. Stored verbatim in object storage; the
/// UFFD handler reads + replays it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkingSetTrace {
    /// Schema version for forward-compat. v1 today.
    pub schema_version: u32,

    /// Capture timestamp. Stale traces (older than some threshold)
    /// might still be useful but indicate a stale image.
    pub captured_at: DateTime<Utc>,

    /// vCPU count at capture. Replay should only use traces
    /// captured against the same vCPU count — different vCPU
    /// counts produce different working sets.
    pub vcpu_count: u32,

    /// Duration of the capture window, in milliseconds. Typical
    /// 5000 ms. Replay reads this for telemetry; doesn't affect
    /// what gets prefaulted.
    pub capture_window_ms: u32,

    /// Ordered list of chunks the handler observed faults for.
    /// First entry = first fault. Replay prefaults in this order.
    pub chunks: Vec<ChunkHash>,
}

impl WorkingSetTrace {
    pub fn new(
        vcpu_count: u32,
        capture_window_ms: u32,
        captured_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            schema_version: 1,
            captured_at,
            vcpu_count,
            capture_window_ms,
            chunks: Vec::new(),
        }
    }

    /// Approximate working-set size in bytes, assuming the
    /// caller's chunk_size. Diagnostic helper for telemetry.
    pub fn approx_bytes(&self, chunk_size: u64) -> u64 {
        self.chunks.len() as u64 * chunk_size
    }
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a
    // decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]

    use super::*;

    #[test]
    fn canonical_trace_storage_key() {
        let r = TraceRef::canonical(Uuid::nil());
        assert_eq!(
            r.storage_key(),
            "traces/00000000-0000-0000-0000-000000000000/canonical.json"
        );
    }

    #[test]
    fn host_trace_storage_key_includes_host() {
        let host = Uuid::new_v4();
        let r = TraceRef::host(Uuid::nil(), host);
        let key = r.storage_key();
        assert!(key.starts_with("traces/00000000-0000-0000-0000-000000000000/"));
        assert!(key.contains(&host.to_string()));
        assert!(key.ends_with(".json"));
    }

    #[test]
    fn trace_round_trips_through_json() {
        let mut t = WorkingSetTrace::new(2, 5000, DateTime::<Utc>::UNIX_EPOCH);
        t.chunks.push(ChunkHash::of(b"a"));
        t.chunks.push(ChunkHash::of(b"b"));
        let json = serde_json::to_string(&t).unwrap();
        let back: WorkingSetTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn approx_bytes_multiplies_chunks_by_size() {
        let mut t = WorkingSetTrace::new(1, 5000, DateTime::<Utc>::UNIX_EPOCH);
        t.chunks.push(ChunkHash::of(b"a"));
        t.chunks.push(ChunkHash::of(b"b"));
        t.chunks.push(ChunkHash::of(b"c"));
        assert_eq!(t.approx_bytes(512 * 1024), 3 * 512 * 1024);
    }
}
