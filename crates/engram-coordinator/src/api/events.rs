//! A session's event stream. Multi-client by design: any number of
//! subscribers (browser tabs, Slack bots, CLI watchers) can attach
//! simultaneously and receive the same events.
//!
//! The transport is the app-gRPC `StreamEvents` RPC. The orchestrator
//! fronts it as `GET /api/v1/sessions/:id/events` (SSE), mapping each
//! frame's `idx` onto the SSE `id:` line — so an EventSource that
//! reconnects sends `Last-Event-ID: <idx>`, which arrives back here as
//! `since`. There is no axum SSE handler in this crate.
//!
//! # ADR 0105: the log is the truth, the bus is an accelerator
//!
//! The replay WALKS the log to its tail in pages — it is not one capped
//! read. The stream keeps one piece of state, the `cursor`: the highest
//! idx it has DELIVERED. It re-reads the log from that cursor whenever it
//! cannot prove it holds every event. That happens twice: at the start
//! (the initial catch-up) and again whenever the bus reports it dropped
//! events (`Lagged`). One catch-up implementation serves both.
//!
//! Sequencing rule: subscribe to the bus *before* reading the log, so an
//! event published mid-walk queues in the broadcast buffer instead of
//! landing on the floor. The cursor then de-dupes the seam: the walk
//! reads strictly above it, the tail drops at-or-below it. That drop is
//! sound because log idx allocation leaves no gaps, so anything at or
//! below the cursor was already delivered by a page.

use std::sync::Arc;

use engram_core::traits::MetadataStore;
use engram_core::SessionId;
use futures::stream::Stream;

use crate::error::ApiError;
use crate::state::{IndexedEvent, SharedState};

/// Page size for the log walk — a PAGE SIZE, **not** a ceiling. The
/// replay keeps reading pages until it reaches the tail, so this bounds
/// per-subscriber memory (one page in flight) and nothing else.
///
/// This was `REPLAY_LIMIT = 1000` behind a SINGLE read, which silently
/// dropped every event past the first page: prod session
/// `eafd98b5-f748-484e-a27b-ed6de8343204` rendered 1000 of its 1291
/// events, hiding the run that opened its PR (ADR 0105). Raising a
/// ceiling only moves the cliff; walking to the tail removes it.
const REPLAY_PAGE: i64 = 500;

/// ADR 0051: the sequencing-sensitive prelude for the app-gRPC
/// `StreamEvents` RPC (`grpc_app/session.rs`, the only caller).
///
/// Two jobs, in this order:
/// 1. Existence check. Eager ON PURPOSE — an unknown session must fail the
///    RPC at call time, not mid-stream.
/// 2. Subscribe to the live bus. This happens BEFORE any log read (the
///    sequencing rule above), so events published while
///    [`merged_event_stream`] walks the log queue up in the broadcast
///    buffer instead of landing on the floor.
///
/// The log reads themselves live in [`merged_event_stream`], which owns the
/// cursor. Handing the receiver over is what forces the ordering: the
/// stream cannot be built without having subscribed first.
pub(crate) async fn events_core(
    state: &SharedState,
    id: SessionId,
) -> Result<tokio::sync::broadcast::Receiver<IndexedEvent>, ApiError> {
    state.services.meta.get_session(id).await?;
    Ok(state.events.subscribe(id))
}

/// ADR 0060: default page size when `limit` is unset / non-positive.
const LIST_DEFAULT_LIMIT: i64 = 500;
/// ADR 0060: hard cap so one unary read stays bounded regardless of `limit`.
const LIST_MAX_LIMIT: i64 = 1000;

/// ADR 0060: unary, bounded, UNFILTERED read of the persistent log — the
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
/// `StreamEvents` RPC maps over [`merged_event_stream`]; all cursor / dedupe
/// / lag semantics live there.
pub(crate) enum MergedEvent {
    /// An event read from the persistent log — by the initial walk OR by a
    /// post-lag backfill. Carries the row's true `recovery_epoch` /
    /// `rewound_at`, unlike [`Self::Live`].
    Replay(engram_core::types::PersistedEvent),
    /// A live event from the broadcast bus (deduplicated against the cursor).
    Live(IndexedEvent),
    /// Broadcast receiver lagged; `n` = number of missed events. No `idx`
    /// — never disturbs the caller's cursor / high-water state.
    Lagged(u64),
}

/// THE replay→live merge behind the app-gRPC `StreamEvents` transport.
/// Cursor / dedupe / lag semantics live here ONLY.
///
/// ADR 0105. A two-state loop over one piece of state — `cursor`, the
/// highest idx this stream has DELIVERED:
///
/// - **Catching up**: read `idx > cursor` from the log in [`REPLAY_PAGE`]
///   pages, yielding each event and advancing the cursor. A short page
///   means the tail is reached → start tailing. This is a WALK, not a
///   capped read; every event reaches the client.
/// - **Tailing**: forward bus events above the cursor. Phase 1c EPHEMERAL
///   chunks (ADR 0052) were never in the log and carry no real idx, so
///   they bypass the cursor gate entirely.
///
/// A `Lagged(n)` flips the loop back to catching up: the bus dropped events
/// this stream never delivered, but the log still holds them, so we report
/// the lag honestly and then re-walk from the cursor. Before ADR 0105 a lag
/// was reported and never refilled, leaving the client a permanent hole
/// until it reconnected.
///
/// Backfilled events are `Replay`, not `Live` — so they carry their true
/// `recovery_epoch` / `rewound_at` from the log rather than the `Live`
/// arm's hardcoded `(0, false)`.
///
/// `Err` ends the stream and means "the log read failed" — NEVER a clean
/// end. A truncated stream that looks complete is the whole defect class
/// this function exists to prevent; the client must reconnect from its own
/// cursor instead.
pub(crate) fn merged_event_stream(
    meta: Arc<dyn MetadataStore>,
    id: SessionId,
    mut live: tokio::sync::broadcast::Receiver<IndexedEvent>,
    since: Option<i64>,
) -> impl Stream<Item = Result<MergedEvent, ApiError>> + Send {
    use tokio::sync::broadcast::error::RecvError;

    async_stream::stream! {
        // The ONLY dedupe/resume state: the log walk reads strictly above
        // it, the live tail drops at-or-below it, a lag re-walks from it.
        let mut cursor = since.unwrap_or(-1);
        let mut catching_up = true;

        loop {
            if catching_up {
                let page = match meta
                    .list_session_events_since(id, cursor, REPLAY_PAGE)
                    .await
                {
                    Ok(page) => page,
                    Err(e) => {
                        // Honest terminal error, never a silent truncation.
                        yield Err(ApiError::from(e));
                        return;
                    }
                };
                // A full page means there may be more behind it; a short
                // (or empty) one means we reached the tail. An exactly-full
                // final page costs one extra empty read, which then ends
                // the walk.
                let full = page.len() as i64 == REPLAY_PAGE;
                for ev in page {
                    cursor = ev.idx;
                    yield Ok(MergedEvent::Replay(ev));
                }
                catching_up = full;
                continue;
            }

            match live.recv().await {
                // Phase 1c: ephemeral chunks were never persisted and hold
                // no meaningful idx — pass through without touching the
                // cursor, so they can't advance a client's Last-Event-ID.
                Ok(indexed) if indexed.ephemeral => yield Ok(MergedEvent::Live(indexed)),
                Ok(indexed) if indexed.idx > cursor => {
                    cursor = indexed.idx;
                    yield Ok(MergedEvent::Live(indexed));
                }
                // Already delivered by a walk — drop it, don't duplicate.
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => {
                    yield Ok(MergedEvent::Lagged(n));
                    catching_up = true;
                }
                Err(RecvError::Closed) => break,
            }
        }
    }
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
    use crate::state::{IndexedEvent, SessionEvent, SessionEventBus};
    use chrono::DateTime;
    use engram_core::types::session::{SessionMode, SessionSpec};
    use futures::StreamExt;

    fn chunk_event() -> SessionEvent {
        SessionEvent::HarnessAgentMessageChunk {
            run_id: "r1".into(),
            message_id: "m1".into(),
            chunk: "hi".into(),
            // Fixed timestamp — these tests only exercise idx/kind
            // plumbing, never the event's time (ADR 0098 D1).
            at: DateTime::UNIX_EPOCH,
        }
    }

    // -----------------------------------------------------------------
    // ADR 0105: the replay walks the log to its tail.
    //
    // These drive `merged_event_stream` by hand, so every interleaving
    // (a page boundary, an event arriving mid-walk, a read failure, a bus
    // overflow) is exact rather than timing-dependent.
    // -----------------------------------------------------------------

    fn sim_meta() -> Arc<engram_sim::SimMetadataStore> {
        engram_sim::SimMetadataStore::new(
            Arc::new(engram_core::traits::SystemClock::new()),
            Arc::new(engram_sim::SimEntropy::seeded(0x0105)),
        )
    }

    /// A durable log event. `idx` is allocated by the store, so the
    /// payload only has to be distinguishable.
    async fn append(meta: &Arc<engram_sim::SimMetadataStore>, id: SessionId, n: usize) -> i64 {
        meta.append_session_event(id, "agent_message", serde_json::json!({ "text": n }))
            .await
            .expect("append")
    }

    /// A live bus event carrying `idx` — the shape `AppState::emit`
    /// publishes after the row lands.
    fn live_at(idx: i64) -> IndexedEvent {
        IndexedEvent {
            idx,
            event: SessionEvent::HarnessIdle {
                at: DateTime::UNIX_EPOCH,
            },
            ephemeral: false,
        }
    }

    async fn new_session(meta: &Arc<engram_sim::SimMetadataStore>) -> SessionId {
        meta.create_session(SessionSpec {
            image: "test.invalid/replay:latest".into(),
            mode: SessionMode::Agent,
        })
        .await
        .expect("create session")
    }

    /// Pull one item and require it to be a replayed (log-sourced) event,
    /// returning its idx.
    ///
    /// Bounded on purpose: a walk that stops short leaves the stream parked
    /// on the live tail forever, so an unbounded `next()` would HANG rather
    /// than fail. That is exactly the regression shape here (the pre-ADR
    /// 0105 single read stalls after one page), and a hanging test reports
    /// a timeout instead of naming the bug.
    async fn next_replay_idx(
        s: &mut (impl Stream<Item = Result<MergedEvent, ApiError>> + Unpin),
    ) -> i64 {
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), s.next())
            .await
            .expect("stream stalled — the walk stopped before reaching the log tail");
        match item {
            Some(Ok(MergedEvent::Replay(ev))) => ev.idx,
            Some(Ok(MergedEvent::Live(ev))) => panic!("expected Replay, got Live idx={}", ev.idx),
            Some(Ok(MergedEvent::Lagged(n))) => panic!("expected Replay, got Lagged({n})"),
            Some(Err(e)) => panic!("expected Replay, got Err({e:?})"),
            None => panic!("expected Replay, stream ended"),
        }
    }

    /// THE regression test for the prod incident. A session with more
    /// events than one page must stream ALL of them. Before ADR 0105 this
    /// stopped dead at the page size (1000 of 1291 events in prod, hiding
    /// the run that opened the PR).
    #[tokio::test]
    async fn replay_walks_past_one_page() {
        // Spans three pages at REPLAY_PAGE=500: 500 + 500 + 200.
        const TOTAL: usize = 1200;
        assert!(
            TOTAL as i64 > REPLAY_PAGE,
            "fixture must exceed one page or the test is vacuous"
        );

        let meta = sim_meta();
        let id = new_session(&meta).await;
        for n in 0..TOTAL {
            append(&meta, id, n).await;
        }

        let bus = SessionEventBus::default();
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
        ));

        let mut seen = Vec::with_capacity(TOTAL);
        for _ in 0..TOTAL {
            seen.push(next_replay_idx(&mut s).await);
        }

        assert_eq!(seen.len(), TOTAL, "every event reaches the client");
        let expected: Vec<i64> = (0..TOTAL as i64).collect();
        assert_eq!(seen, expected, "contiguous idx order, no gap and no dup");
    }

    /// The handoff from walking the log to tailing the bus must neither
    /// drop nor duplicate an event that lands mid-walk. Subscribing before
    /// the first read is what makes this hold.
    #[tokio::test]
    async fn replay_to_live_seam_is_gap_free_and_dup_free() {
        let meta = sim_meta();
        let id = new_session(&meta).await;
        // 600 rows = one full page plus a short one.
        for n in 0..600 {
            append(&meta, id, n).await;
        }

        let bus = SessionEventBus::default();
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
        ));

        // Drain page one, leaving the walk suspended mid-log.
        let mut seen = Vec::new();
        for _ in 0..REPLAY_PAGE {
            seen.push(next_replay_idx(&mut s).await);
        }

        // A new event arrives NOW — after the walk started, before it
        // finished. Production order: row first, then publish.
        let fresh = append(&meta, id, 600).await;
        bus.publish(id, live_at(fresh));

        // Finish the walk: rows 500..=600, the late arrival included.
        for _ in 0..=(600 - REPLAY_PAGE) {
            seen.push(next_replay_idx(&mut s).await);
        }

        let expected: Vec<i64> = (0..=600).collect();
        assert_eq!(
            seen, expected,
            "the late event appears exactly once, in idx order"
        );

        // The bus copy of `fresh` sits at or below the cursor, so the
        // tail must drop it rather than emit a duplicate. Nothing else is
        // pending, so the next poll must not resolve.
        let dup = tokio::time::timeout(std::time::Duration::from_millis(50), s.next()).await;
        assert!(dup.is_err(), "no duplicate for the already-walked event");
    }

    /// A log read that fails mid-walk must end the stream with an ERROR.
    /// Ending cleanly would be indistinguishable from a complete
    /// transcript — the exact defect ADR 0105 removes.
    #[tokio::test]
    async fn failed_page_read_is_terminal_not_clean_eof() {
        let meta = sim_meta();
        let id = new_session(&meta).await;
        for n in 0..600 {
            append(&meta, id, n).await;
        }

        let bus = SessionEventBus::default();
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
        ));

        for _ in 0..REPLAY_PAGE {
            next_replay_idx(&mut s).await;
        }

        // The store breaks before the walk asks for page two.
        meta.set_outage(true);

        match s.next().await {
            Some(Err(_)) => {}
            Some(Ok(MergedEvent::Replay(ev))) => {
                panic!("expected Err, got Replay idx={}", ev.idx)
            }
            Some(Ok(_)) => panic!("expected Err, got a live/lagged item"),
            None => panic!("clean EOF hid a failed read — a truncated stream looked complete"),
        }
    }

    /// The bus holds a bounded window. If it drops events this stream
    /// never delivered, the log still has them: report the lag, then
    /// re-walk from the cursor. Before ADR 0105 the lag was reported and
    /// never refilled, leaving a permanent hole.
    #[tokio::test]
    async fn bus_lag_backfills_from_the_log() {
        const CAP: usize = 8;
        const BURST: i64 = 20;

        let meta = sim_meta();
        let id = new_session(&meta).await;
        for n in 0..3 {
            append(&meta, id, n).await;
        }

        // A deliberately tiny bus so the burst provably overflows it.
        let bus = SessionEventBus::new(CAP);
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
        ));

        // Walk the short log, then sit in the tail.
        let mut seen = vec![
            next_replay_idx(&mut s).await,
            next_replay_idx(&mut s).await,
            next_replay_idx(&mut s).await,
        ];
        assert_eq!(seen, vec![0, 1, 2]);

        // Burst past the bus depth WITHOUT polling — rows land, and the
        // receiver falls behind and loses the oldest.
        for n in 3..(3 + BURST) {
            let idx = append(&meta, id, n as usize).await;
            bus.publish(id, live_at(idx));
        }
        assert!(
            BURST > CAP as i64,
            "burst must exceed bus depth or no lag occurs"
        );

        // The lag is surfaced honestly first...
        match s.next().await {
            Some(Ok(MergedEvent::Lagged(n))) => assert!(n > 0, "lag reports a positive count"),
            Some(Ok(MergedEvent::Replay(ev))) => {
                panic!("expected Lagged first, got Replay idx={}", ev.idx)
            }
            Some(Ok(MergedEvent::Live(ev))) => {
                panic!("expected Lagged first, got Live idx={}", ev.idx)
            }
            Some(Err(e)) => panic!("expected Lagged, got Err({e:?})"),
            None => panic!("stream ended instead of reporting lag"),
        }

        // ...then the hole is refilled from the log, in full.
        let last = 2 + BURST;
        for _ in 3..=last {
            seen.push(next_replay_idx(&mut s).await);
        }
        let expected: Vec<i64> = (0..=last).collect();
        assert_eq!(
            seen, expected,
            "every dropped event came back from the log, no gap and no dup"
        );

        // Bus entries at or below the cursor must not re-emit.
        let dup = tokio::time::timeout(std::time::Duration::from_millis(50), s.next()).await;
        assert!(dup.is_err(), "backfilled events are not duplicated");
    }

    /// Phase 1c (ADR 0052): ephemeral token chunks were never persisted
    /// and hold no meaningful idx. They must pass the cursor gate and must
    /// not move it, or a client's Last-Event-ID would skip real events.
    #[tokio::test]
    async fn ephemeral_chunk_bypasses_the_cursor_gate() {
        let meta = sim_meta();
        let id = new_session(&meta).await;
        for n in 0..3 {
            append(&meta, id, n).await;
        }

        let bus = SessionEventBus::default();
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
        ));
        for _ in 0..3 {
            next_replay_idx(&mut s).await;
        }

        // idx 0 is far below the cursor (2) — an ephemeral chunk must
        // still pass, because the gate does not apply to it.
        bus.publish(
            id,
            IndexedEvent {
                idx: 0,
                event: chunk_event(),
                ephemeral: true,
            },
        );
        match s.next().await {
            Some(Ok(MergedEvent::Live(ev))) => assert!(ev.ephemeral, "the chunk passed through"),
            _ => panic!("ephemeral chunk was dropped by the cursor gate"),
        }

        // It must NOT have advanced the cursor: a real event at idx 3
        // still arrives.
        let fresh = append(&meta, id, 3).await;
        bus.publish(id, live_at(fresh));
        match s.next().await {
            Some(Ok(MergedEvent::Live(ev))) => assert_eq!(ev.idx, 3),
            _ => panic!("cursor was disturbed by an ephemeral chunk"),
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
            event: SessionEvent::HarnessIdle {
                at: DateTime::UNIX_EPOCH,
            },
            ephemeral: false,
        }));
        assert_eq!(idx, Some(7));
    }
}
