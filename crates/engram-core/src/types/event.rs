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
    /// ADR 0028 A.log: the session recovery epoch this event was
    /// written in. 0 = the original timeline; bumped on each rung-1
    /// rewind. Lets consumers segment the transcript across recoveries.
    /// `#[serde(default)]` keeps pre-0054 rows (and non-PG mocks)
    /// decoding as epoch 0.
    #[serde(default)]
    pub recovery_epoch: i64,
    /// ADR 0028 A.log: set when this event was tombstoned by a rung-1
    /// rewind (its idx was past the checkpoint's `events_cursor`). The
    /// row stays for audit + a collapsed/greyed render, but it's NOT
    /// part of the live transcript head. `None` = live.
    #[serde(default)]
    pub rewound_at: Option<DateTime<Utc>>,
}

/// ADR 0028 A.log: the outcome of a rung-1 recovery rewind —
/// returned by `MetadataStore::rewind_session_to_cursor` so the
/// caller can emit an honest, legible boundary event.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RewindSummary {
    /// How many live events were tombstoned (the rolled-back span).
    /// Zero means the checkpoint was already the head — no rewind
    /// happened and the caller should NOT emit a boundary.
    pub rolled_back: u64,
    /// The new session recovery epoch after the bump (events appended
    /// from here carry this).
    pub recovery_epoch: i64,
    /// The `idx` the live head reset to (the checkpoint's
    /// `events_cursor`); the boundary event renders just after it.
    pub through_idx: i64,
    /// Human-readable side-effects detected in the rolled-back span
    /// that touched the outside world and therefore SURVIVE the
    /// rewind (`git push` / opened PR / shared file). The platform
    /// can't undo them — it surfaces them. One line each.
    pub surviving_side_effects: Vec<String>,
    /// Workspace paths whose `file_changed` events fell in the
    /// rolled-back span (2026-07-13 dfa0face incident): those edits are
    /// GONE from the restored guest — the agent won't remember them and
    /// the disk doesn't have them — but the exact hunks remain in the
    /// tombstoned (greyed) transcript. Naming the files on the recovery
    /// boundary is what lets a user tell "cosmetic rollback" from "my
    /// change needs re-applying" without archaeology.
    #[serde(default)]
    pub rolled_back_files: Vec<String>,
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
