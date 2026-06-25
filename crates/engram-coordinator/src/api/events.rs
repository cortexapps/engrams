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

use crate::error::ApiError;
use crate::state::{IndexedEvent, SharedState};

const REPLAY_LIMIT: i64 = 1000;

/// ADR 0051: the sequencing-sensitive core (existence check → subscribe
/// FIRST → replay) shared by the axum SSE handler above and the app-gRPC
/// `StreamEvents` RPC (`grpc_app/session.rs`). Returns the replayed events
/// plus a live receiver already subscribed *before* the query, so nothing
/// emitted during the query is lost. `since`: replay `idx > since`; `None`
/// ≡ `-1` (from the start of the log). The gRPC handler maps the items to
/// proto; the axum handler frames them as SSE + ADR 0050 B shutdown.
pub(crate) async fn events_core(
    state: &SharedState,
    id: SessionId,
    since: Option<i64>,
) -> Result<
    (
        Vec<engram_core::types::PersistedEvent>,
        tokio::sync::broadcast::Receiver<IndexedEvent>,
    ),
    ApiError,
> {
    state.services.meta.get_session(id).await?;
    let since = since.unwrap_or(-1);
    // 1) Subscribe FIRST (the sequencing rule at the top of this file) so
    //    any event published while we're querying the log lands in the
    //    broadcast queue rather than getting dropped on the floor.
    let live_rx = state.events.subscribe(id);
    // 2) Replay from the persistent log.
    let replayed = state
        .services
        .meta
        .list_session_events_since(id, since, REPLAY_LIMIT)
        .await?;
    Ok((replayed, live_rx))
}

/// ADR 0059: default page size when `limit` is unset / non-positive.
const LIST_DEFAULT_LIMIT: i64 = 500;
/// ADR 0059: hard cap so one unary read stays bounded regardless of `limit`.
const LIST_MAX_LIMIT: i64 = 1000;

/// ADR 0059: unary, bounded, UNFILTERED read of the persistent log — the
/// catch-up read the reverse-channel pump (`SessionIngestWorkflow`) walks
/// forward. Returns events with `idx > after_idx` (`None` ≡ from the start of
/// the log), capped, plus the cursor to pass as `after_idx` next time: the
/// last returned idx, or the request's `after_idx` echoed back when the page
/// is empty so a caller at the tail never rewinds. Curation is the consumer's
/// concern (unlike the SSE/stream path, which is also unfiltered). The gRPC
/// handler maps each [`PersistedEvent`](engram_core::types::PersistedEvent) to
/// a proto `SessionEvent` via [`merged_to_parts`] so the unary read is
/// byte-identical to the stream's replay arm.
pub(crate) async fn list_session_events_core(
    state: &SharedState,
    id: SessionId,
    after_idx: Option<i64>,
    limit: Option<i64>,
) -> Result<(Vec<engram_core::types::PersistedEvent>, i64), ApiError> {
    state.services.meta.get_session(id).await?;
    let after = after_idx.unwrap_or(-1);
    let events = state
        .services
        .meta
        .list_session_events_since(id, after, clamp_list_limit(limit))
        .await?;
    let next_after_idx = events.last().map(|e| e.idx).unwrap_or(after);
    Ok((events, next_after_idx))
}

/// Clamp a caller-supplied `limit` to `(0, LIST_MAX_LIMIT]`, defaulting an
/// unset / non-positive value to `LIST_DEFAULT_LIMIT`.
fn clamp_list_limit(limit: Option<i64>) -> i64 {
    match limit {
        Some(n) if n > 0 => n.min(LIST_MAX_LIMIT),
        _ => LIST_DEFAULT_LIMIT,
    }
}

/// A single item from the unified replay→live event stream. The app-gRPC
/// `StreamEvents` RPC maps over [`merged_event_stream`]; all dedupe / lag /
/// high-water semantics live there, and the axum SSE handler frames the
/// same merge as SSE messages.
pub(crate) enum MergedEvent {
    /// An event replayed from the persistent log.
    Replay(engram_core::types::PersistedEvent),
    /// A live event from the broadcast bus (deduplicated against replay).
    Live(IndexedEvent),
    /// Broadcast receiver lagged; `n` = number of missed events. No `idx`
    /// — never disturbs the caller's cursor / high-water state.
    Lagged(u64),
}

/// THE replay→live merge shared by the axum SSE handler and the app-gRPC
/// `StreamEvents` transport. Dedupe / lag / high-water semantics live here
/// ONLY.
///
/// `replay_high_water` is `replayed.last().idx`, or `since.unwrap_or(-1)`
/// (the "from the start" sentinel) when replay is empty. Replay items
/// stream first; the live [`BroadcastStream`] follows, dropping any event
/// at or below the high-water so the seam is gap-free and dup-free. Lag
/// maps to [`MergedEvent::Lagged`] rather than an error.
pub(crate) fn merged_event_stream(
    replayed: Vec<engram_core::types::PersistedEvent>,
    live: tokio::sync::broadcast::Receiver<IndexedEvent>,
    since: Option<i64>,
) -> impl Stream<Item = MergedEvent> + Send {
    use tokio_stream::wrappers::BroadcastStream;

    // De-dupe boundary: highest replayed idx (or the "from start" sentinel
    // when replay is empty) — live events at or below it were already
    // replayed.
    let replay_high_water = replayed
        .last()
        .map(|e| e.idx)
        .unwrap_or(since.unwrap_or(-1));

    // Replay segment: each persisted event becomes a Replay item.
    let replay = futures::stream::iter(replayed.into_iter().map(MergedEvent::Replay));

    // Live segment: drop events we already replayed (idx <= high water)
    // so the seam between the two is gap-free and dup-free. Phase 1c:
    // EPHEMERAL events (live token chunks) were never in the replay log
    // and carry no real `idx`, so they bypass the high-water gate and
    // always pass through.
    let live_stream = BroadcastStream::new(live).filter_map(move |recv| async move {
        match recv {
            Ok(indexed) if indexed.ephemeral || indexed.idx > replay_high_water => {
                Some(MergedEvent::Live(indexed))
            }
            Ok(_) => None,
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                Some(MergedEvent::Lagged(n))
            }
        }
    });

    replay.chain(live_stream)
}

/// Decode a [`MergedEvent`] into its wire-ready parts: `(idx, kind,
/// payload_json)`. `payload_json` already has the ADR 0028 A.log rewind
/// metadata folded in (via [`with_rewind_meta`]) so the gRPC framing is
/// byte-identical to the SSE one. `idx` is `None` for the lagged sentinel
/// so it never disturbs client cursors.
pub(crate) fn merged_to_parts(ev: MergedEvent) -> (Option<i64>, String, String) {
    match ev {
        MergedEvent::Replay(ev) => {
            // ADR 0028 A.log: fold the rewind metadata into the data object
            // so the transcript can grey/collapse tombstoned events and
            // segment by recovery epoch. `_rewound` true → this event was
            // rolled back by a rung-1 recovery (kept for audit, not the
            // live head).
            let rewound = ev.rewound_at.is_some();
            let payload_json = with_rewind_meta(ev.payload, ev.recovery_epoch, rewound);
            (Some(ev.idx), ev.kind, payload_json)
        }
        MergedEvent::Live(indexed) => {
            // Live events are, by construction, never tombstoned (they were
            // just emitted). They carry the session's current epoch — but
            // the boundary `recovered_from_checkpoint` event itself carries
            // the new epoch in its payload, so the web tracks "current
            // epoch" from that and live events ride along as not-rewound.
            let payload = serde_json::to_value(&indexed.event).unwrap_or(serde_json::Value::Null);
            let payload_json = with_rewind_meta(payload, 0, false);
            // Phase 1c: ephemeral chunks carry NO `idx` on the wire, so a
            // client never advances its `Last-Event-ID` cursor past them
            // (they were never persisted and won't be replayed on reconnect).
            let idx = if indexed.ephemeral {
                None
            } else {
                Some(indexed.idx)
            };
            (idx, indexed.event.kind().to_string(), payload_json)
        }
        MergedEvent::Lagged(n) => (None, "lagged".to_string(), format!(r#"{{"missed":{n}}}"#)),
    }
}

/// Merge ADR 0028 A.log rewind metadata into an event's data object.
/// Non-object payloads (shouldn't happen for our typed events) pass
/// through unchanged.
fn with_rewind_meta(mut payload: serde_json::Value, recovery_epoch: i64, rewound: bool) -> String {
    if let serde_json::Value::Object(map) = &mut payload {
        map.insert("_recovery_epoch".into(), recovery_epoch.into());
        map.insert("_rewound".into(), rewound.into());
    }
    payload.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{IndexedEvent, SessionEvent};
    use chrono::Utc;

    fn chunk_event() -> SessionEvent {
        SessionEvent::HarnessAgentMessageChunk {
            run_id: "r1".into(),
            message_id: "m1".into(),
            chunk: "hi".into(),
            at: Utc::now(),
        }
    }

    #[test]
    fn clamp_list_limit_defaults_and_caps() {
        // Unset / non-positive → the default page size.
        assert_eq!(clamp_list_limit(None), LIST_DEFAULT_LIMIT);
        assert_eq!(clamp_list_limit(Some(0)), LIST_DEFAULT_LIMIT);
        assert_eq!(clamp_list_limit(Some(-5)), LIST_DEFAULT_LIMIT);
        // In-range → passed through verbatim.
        assert_eq!(clamp_list_limit(Some(10)), 10);
        // Above the hard cap → clamped to it.
        assert_eq!(clamp_list_limit(Some(LIST_MAX_LIMIT + 1)), LIST_MAX_LIMIT);
        assert_eq!(clamp_list_limit(Some(1_000_000)), LIST_MAX_LIMIT);
    }

    #[test]
    fn ephemeral_live_event_frames_with_no_idx() {
        // Phase 1c: an ephemeral chunk MUST surface with idx=None so it never
        // advances a client's Last-Event-ID cursor (it was never persisted and
        // won't be replayed on reconnect).
        let (idx, kind, _payload) = merged_to_parts(MergedEvent::Live(IndexedEvent {
            idx: 0,
            event: chunk_event(),
            ephemeral: true,
        }));
        assert_eq!(idx, None);
        assert_eq!(kind, "agent_message_chunk");
    }

    #[test]
    fn durable_live_event_keeps_its_idx() {
        // A normal persisted event still carries its real idx on the wire.
        let (idx, _kind, _payload) = merged_to_parts(MergedEvent::Live(IndexedEvent {
            idx: 7,
            event: SessionEvent::HarnessIdle { at: Utc::now() },
            ephemeral: false,
        }));
        assert_eq!(idx, Some(7));
    }
}
