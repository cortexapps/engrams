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
