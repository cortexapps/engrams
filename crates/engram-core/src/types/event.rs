use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One row from the `session_events` table — the persistent
/// counterpart to the in-memory `SessionEvent` broadcast in
/// `engram-coordinator`. Stored events are opaque JSON at this layer
/// (the coordinator owns the typed enum); engram-core just carries
/// the wire shape so anyone implementing `MetadataStore` can read /
/// write the log without depending on the coordinator's types.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedEvent {
    /// Monotonic per-session sequence number. Allocated atomically by
    /// `MetadataStore::append_session_event`. Subscribers reconnecting
    /// via `?since=N` send the highest idx they've already seen.
    pub idx: i64,
    /// Event discriminant — matches the SSE `event:` field on the
    /// wire. Values are documented in `0002_session_events.sql`.
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// ADR 0026: one row from the `artifacts` table — a file shared into a
/// session that surfaces in its conversation history (and persists in
/// object storage forever, outside the chunk-GC sweep). The serve
/// endpoint looks this up (scoped to the session) to find the blob key
/// + the coord-detected media type.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactRow {
    pub id: uuid::Uuid,
    /// Object-storage key under the GC-safe `artifacts/<session>/` prefix.
    pub blob_key: String,
    /// Coord-detected media type (never the guest-supplied one).
    pub media_type: String,
    pub size_bytes: i64,
    pub caption: Option<String>,
    pub created_at: DateTime<Utc>,
}
