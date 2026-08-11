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

impl PersistedEvent {
    /// The tool name this event refers to, if its kind carries one.
    /// `None` for every non-tool kind AND for a tool kind whose payload
    /// does not hold the field (a malformed row) — see
    /// [`tool_name_field`] for why that distinction matters to the
    /// filter.
    pub fn tool_name(&self) -> Option<&str> {
        self.payload
            .get(tool_name_field(&self.kind)?)
            .and_then(|v| v.as_str())
    }
}

/// Which JSONB field holds the tool name for a given event kind, or
/// `None` when the kind carries no tool name.
///
/// This is the ONE place the kind→field map lives. `PostgresStore`
/// expands it into a SQL `CASE` so the filter runs in the database, and
/// `SimMetadataStore` reads it in memory — one table, so the two stores
/// cannot drift (ADR 0098 D4).
///
/// `tool_result_submitted` is absent on purpose: it holds a
/// `tool_call_id`, not a name, and the resolution to a name needs a
/// second lookup. A kind with no tool-name field is therefore NEVER
/// removed by a tool-name filter — see
/// `MetadataStore::list_session_events_window`.
pub fn tool_name_field(kind: &str) -> Option<&'static str> {
    TOOL_NAME_FIELDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, field)| *field)
}

/// Every `(kind, jsonb field)` pair, the backing table for
/// [`tool_name_field`]. A caller that must build the map into a query
/// (PostgresStore's `CASE`) enumerates this instead of repeating it.
pub const TOOL_NAME_FIELDS: &[(&str, &str)] = &[
    ("tool_call_requested", "name"),
    ("tool_call_started", "tool_name"),
    ("tool_call_completed", "tool_name"),
];

/// Which end of the log a windowed event read starts from, and which way
/// it walks. Both variants return the page in ASCENDING idx order — the
/// direction picks the ANCHOR, not the output order, so a consumer
/// renders every page the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventCursor {
    /// Events with `idx > n`, oldest first. `-1` starts at the head of
    /// the log. The forward walk every catch-up reader uses.
    After(i64),
    /// Events with `idx < n`, read newest-first and then re-ascended, so
    /// the page is the LAST `limit` events below the anchor. The
    /// transcript opens on the tail and backfills upward with this.
    /// `i64::MAX` therefore means "the newest page".
    Before(i64),
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
    /// Sanitized guest-declared basename; `None` for artifacts shared
    /// before file names rode the wire.
    pub file_name: Option<String>,
    pub created_at: DateTime<Utc>,
}
