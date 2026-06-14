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

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use engram_core::SessionId;
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::{IndexedEvent, SharedState};

const REPLAY_LIMIT: i64 = 1000;

#[derive(Deserialize)]
pub struct EventsQuery {
    /// Replay events with `idx > since`. Combine with `Last-Event-ID`
    /// header (EventSource auto-reconnect) — the larger of the two
    /// wins so an explicit query never goes backward across reconnect.
    pub since: Option<i64>,
}

pub async fn events(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    state.services.meta.get_session(id).await?;

    // Pick the higher of explicit query and Last-Event-ID — explicit
    // query wins ties. Default of -1 means "from the start of the log."
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let since = match (query.since, last_event_id) {
        (Some(q), Some(h)) => q.max(h),
        (Some(q), None) => q,
        (None, Some(h)) => h,
        (None, None) => -1,
    };

    // 1) Subscribe FIRST so any event published while we're querying
    //    the log lands in the broadcast queue rather than getting
    //    dropped on the floor.
    let live_rx = state.events.subscribe(id);

    // 2) Replay from the persistent log.
    let replayed = state
        .services
        .meta
        .list_session_events_since(id, since, REPLAY_LIMIT)
        .await?;
    let replay_high_water = replayed.last().map(|e| e.idx).unwrap_or(since);

    // ADR 0050 B: end the stream when the coordinator begins graceful
    // shutdown so this long-lived SSE connection doesn't block hyper's
    // drain (tokio-rs/axum#2673). The browser EventSource auto-reconnects
    // to a healthy replica and resumes from the PG log via Last-Event-ID,
    // so the close is lossless. Fires immediately if shutdown is already
    // underway (don't open a new stream on a draining pod).
    let mut shutdown_rx = state.subscribe_shutdown();
    let shutdown = async move {
        // Resolve ONLY on a genuine shutdown trigger (value → true). On
        // sender-drop `wait_for` returns Err — that's the owning AppState
        // being torn down (process teardown in prod, a fixture drop in
        // tests), NOT a shutdown; park so we never force-end a live
        // stream on it (the stream ends naturally when its source closes).
        if shutdown_rx
            .wait_for(|shutting_down| *shutting_down)
            .await
            .is_err()
        {
            std::future::pending::<()>().await;
        }
    };
    let stream = build_event_stream(replayed, live_rx, replay_high_water).take_until(shutdown);
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

fn build_event_stream(
    replayed: Vec<engram_core::types::PersistedEvent>,
    live_rx: tokio::sync::broadcast::Receiver<IndexedEvent>,
    replay_high_water: i64,
) -> impl Stream<Item = Result<Event, Infallible>> {
    use tokio_stream::wrappers::BroadcastStream;

    // Replay segment: each persisted event becomes an SSE message with
    // the persisted `kind` and `payload` carried verbatim.
    let replay = futures::stream::iter(replayed.into_iter().map(persisted_to_sse));

    // Live segment: drop events we already replayed (idx <= high water)
    // so the seam between the two is gap-free and dup-free.
    let live = BroadcastStream::new(live_rx).filter_map(move |recv| async move {
        match recv {
            Ok(indexed) if indexed.idx > replay_high_water => Some(indexed_to_sse(indexed)),
            Ok(_) => None,
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => Some(
                Event::default()
                    .event("lagged")
                    .data(format!(r#"{{"missed":{n}}}"#)),
            ),
        }
    });

    replay.chain(live).map(Ok::<Event, Infallible>)
}

fn persisted_to_sse(ev: engram_core::types::PersistedEvent) -> Event {
    // ADR 0028 A.log: fold the rewind metadata into the data object so
    // the transcript can grey/collapse tombstoned events and segment by
    // recovery epoch. Underscore-prefixed to never collide with a
    // SessionEvent field. `_rewound` true → this event was rolled back
    // by a rung-1 recovery (kept for audit, not the live head).
    let rewound = ev.rewound_at.is_some();
    let data = with_rewind_meta(ev.payload, ev.recovery_epoch, rewound);
    Event::default()
        .id(ev.idx.to_string())
        .event(ev.kind)
        .data(data)
}

fn indexed_to_sse(indexed: IndexedEvent) -> Event {
    // Live events are, by construction, never tombstoned (they were
    // just emitted). They carry the session's current epoch — but the
    // boundary `recovered_from_checkpoint` event itself carries the
    // new epoch in its payload, so the web tracks "current epoch" from
    // that and live events ride along as not-rewound.
    let payload = serde_json::to_value(&indexed.event).unwrap_or(serde_json::Value::Null);
    let data = with_rewind_meta(payload, 0, false);
    Event::default()
        .id(indexed.idx.to_string())
        .event(indexed.event.kind())
        .data(data)
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
