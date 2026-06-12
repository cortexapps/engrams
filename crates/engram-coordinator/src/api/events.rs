//! `GET /sessions/:id/events` — SSE subscription to a session's event
//! stream. Multi-client by design: any number of subscribers (browser
//! tabs, Slack bots, CLI watchers) can attach simultaneously and
//! receive the same events.
//!
//! # Late-join / reconnect
//!
//! Each SSE message carries `id: <idx>` (the monotonic per-session
//! index allocated by the persistent log). EventSource clients that
//! disconnect and reconnect automatically send `Last-Event-ID: <idx>`,
//! and we use that — or the explicit `?since=N` query param — to
//! replay missed events from the persistent log before tailing the
//! live bus.
//!
//! Sequencing rule: subscribe to the bus *before* querying the log.
//! Any event published while the replay query was in flight is
//! captured by the bus subscription; we de-dupe by tracking the
//! highest replayed idx and skipping live events at or below it.

use engram_core::SessionId;
use futures::stream::{Stream, StreamExt};
use tokio::sync::broadcast;

use crate::error::ApiError;
use crate::state::{IndexedEvent, SharedState};

pub(crate) const REPLAY_LIMIT: i64 = 1000;

/// A single item from the unified replay→live event stream.
///
/// Both transports (SSE handler, gRPC `StreamEvents`) map over
/// [`merged_event_stream`]; all dedupe/lag/high-water semantics live
/// there and here, not scattered across call sites.
pub(crate) enum MergedEvent {
    /// An event replayed from the persistent log.
    Replay(engram_core::types::PersistedEvent),
    /// A live event from the broadcast bus (deduplicated against replay).
    Live(IndexedEvent),
    /// Broadcast receiver lagged; `n` = number of missed events.
    /// No `idx` — never disturbs cursor/high-water state on the caller.
    Lagged(u64),
}

/// THE replay→live merge. Both transports (SSE handler, gRPC
/// `StreamEvents`) map over this; dedupe/lag/high-water semantics live
/// here ONLY.
///
/// Internals:
/// - `replay_high_water` is computed as
///   `replayed.last().map(|e| e.idx).unwrap_or(since.unwrap_or(-1))`.
///   The `-1` sentinel means "from the start"; this function resolves
///   it internally so callers never need to touch it.
/// - Replay items stream first; the live [`BroadcastStream`] follows,
///   filtering out any event at or below `replay_high_water` so the
///   seam is gap-free and dup-free.
/// - Broadcast lag maps to [`MergedEvent::Lagged`] rather than an error.
pub(crate) fn merged_event_stream(
    replayed: Vec<engram_core::types::PersistedEvent>,
    live: broadcast::Receiver<IndexedEvent>,
    since: Option<i64>,
) -> impl Stream<Item = MergedEvent> + Send {
    use tokio_stream::wrappers::BroadcastStream;

    // Compute the high-water mark here (the single authoritative
    // location for this expression). `since.unwrap_or(-1)` is the
    // "all" sentinel; if replay is empty we use it so the live arm
    // does not accidentally drop events that arrived before the first
    // replay item would have.
    let replay_high_water = replayed
        .last()
        .map(|e| e.idx)
        .unwrap_or(since.unwrap_or(-1));

    let replay = futures::stream::iter(replayed.into_iter().map(MergedEvent::Replay));

    let live_stream = BroadcastStream::new(live).filter_map(move |recv| async move {
        match recv {
            Ok(indexed) if indexed.idx > replay_high_water => Some(MergedEvent::Live(indexed)),
            // Already replayed — drop silently.
            Ok(_) => None,
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                Some(MergedEvent::Lagged(n))
            }
        }
    });

    replay.chain(live_stream)
}

/// Decode a [`MergedEvent`] into its wire-ready parts: `(idx, kind,
/// payload_json)`.
///
/// `payload_json` already has the ADR 0028 A.log rewind metadata
/// folded in (`_recovery_epoch`, `_rewound`) — computed ONCE here so
/// both transports (SSE, gRPC) produce byte-identical payloads.
///
/// Live events always carry `recovery_epoch = 0` and `rewound = false`
/// (they were just emitted and are never tombstoned). The boundary
/// `recovered_from_checkpoint` event carries the new epoch in its own
/// payload; the web tracks "current epoch" from that event and live
/// events ride along as not-rewound.
///
/// `idx` is `None` for [`MergedEvent::Lagged`] — the lagged sentinel
/// must never disturb client cursors or the SSE `id:` line.
pub(crate) fn merged_to_parts(ev: MergedEvent) -> (Option<i64>, String, String) {
    match ev {
        MergedEvent::Replay(ev) => {
            let rewound = ev.rewound_at.is_some();
            let payload_json = with_rewind_meta(ev.payload, ev.recovery_epoch, rewound);
            (Some(ev.idx), ev.kind, payload_json)
        }
        MergedEvent::Live(indexed) => {
            let payload = serde_json::to_value(&indexed.event).unwrap_or(serde_json::Value::Null);
            let payload_json = with_rewind_meta(payload, 0, false);
            (
                Some(indexed.idx),
                indexed.event.kind().to_string(),
                payload_json,
            )
        }
        MergedEvent::Lagged(n) => (None, "lagged".to_string(), format!(r#"{{"missed":{n}}}"#)),
    }
}

/// Sequencing-sensitive core: existence check, subscribe-before-query,
/// replay up to `REPLAY_LIMIT`. Returns the replayed events and a live
/// broadcast receiver already subscribed before the query was issued —
/// so no event emitted during the query is lost.
///
/// Callers:
/// - the axum SSE handler (below), which wraps the result in
///   `merged_event_stream` + `merged_to_parts` → SSE framing.
/// - the tonic `StreamEvents` RPC (`grpc_app/session.rs`), which maps
///   the same items to `app::SessionEvent` proto messages.
///
/// `since`: replay events with `idx > since`. Pass `None` (or `-1`)
/// for "from the start of the log". The HTTP -1 sentinel is resolved
/// internally by this function and by `merged_event_stream`; callers
/// pass `Option<i64>` where `None` ≡ `-1`.
pub(crate) async fn events_core(
    state: &SharedState,
    id: SessionId,
    since: Option<i64>,
) -> Result<
    (
        Vec<engram_core::types::PersistedEvent>,
        broadcast::Receiver<IndexedEvent>,
    ),
    ApiError,
> {
    state.services.meta.get_session(id).await?;

    let since = since.unwrap_or(-1);

    // 1) Subscribe FIRST so any event published while we're querying
    //    the log lands in the broadcast queue rather than getting
    //    dropped on the floor. This is the sequencing rule documented
    //    at the top of this file.
    let live_rx = state.events.subscribe(id);

    // 2) Replay from the persistent log.
    let replayed = state
        .services
        .meta
        .list_session_events_since(id, since, REPLAY_LIMIT)
        .await?;

    Ok((replayed, live_rx))
}

// ADR 0039 Task 32: `events` axum shim removed. See `events_core` for the gRPC entry point.
// ADR 0039 final cleanup: `build_event_stream` removed (SSE shim; no references after axum routes dropped).

/// Merge ADR 0028 A.log rewind metadata into an event's data object.
/// Non-object payloads (shouldn't happen for our typed events) pass
/// through unchanged.
pub(crate) fn with_rewind_meta(
    mut payload: serde_json::Value,
    recovery_epoch: i64,
    rewound: bool,
) -> String {
    if let serde_json::Value::Object(map) = &mut payload {
        map.insert("_recovery_epoch".into(), recovery_epoch.into());
        map.insert("_rewound".into(), rewound.into());
    }
    payload.to_string()
}
