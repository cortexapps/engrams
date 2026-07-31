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
//! landing on the floor. The cursor de-dupes the seam: the walk reads
//! strictly above it, the tail drops at-or-below it.
//!
//! **The bus is not ordered by idx; the log is.** `AppState::emit`
//! publishes a local commit at once, while `pg_listener` publishes a peer
//! replica's commit later over LISTEN/NOTIFY — and prod runs two
//! replicas, so a replica can see its own idx 12 before the notification
//! for a peer's idx 11. The log cannot do that: `append_session_event`
//! allocates in ONE autocommit statement holding a row lock on
//! `sessions`, so same-session appends serialize and commit order equals
//! idx order. A visible idx 12 therefore proves idx 11 is visible.
//!
//! That asymmetry is why the tail forwards only `cursor + 1` and sends
//! any jump back to the log. It also means every local event hits the bus
//! TWICE (emit plus the LISTEN echo, which fires on the producer too);
//! the at-or-below-cursor drop is what removes the echo.

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
/// - **Tailing**: forward a bus event only when its idx is exactly
///   `cursor + 1`; a jump goes back to catching up (the bus is not ordered
///   — see the module docs). Phase 1c EPHEMERAL chunks (ADR 0052) were
///   never in the log and carry no real idx, so they bypass the gate
///   entirely.
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
///
/// `durable_only` (ADR 0108 B): suppress ephemeral frames. A cursor-based
/// consumer (the orchestrator SessionListener) has no use for idx-less
/// token chunks, and every idx-less frame it receives must be the `lagged`
/// sentinel — so `Lagged` still passes with the flag set. Unset keeps
/// today's behavior: chunks flow to the SSE passthrough and the browser.
pub(crate) fn merged_event_stream(
    meta: Arc<dyn MetadataStore>,
    id: SessionId,
    mut live: tokio::sync::broadcast::Receiver<IndexedEvent>,
    since: Option<i64>,
    durable_only: bool,
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
                // ADR 0108 B: a durable-only subscriber drops them here
                // instead; the `Lagged` arm below is the one idx-less frame
                // such a subscriber must still receive.
                Ok(indexed) if indexed.ephemeral => {
                    if !durable_only {
                        yield Ok(MergedEvent::Live(indexed));
                    }
                }
                // EXACTLY the next event: contiguity holds, so forwarding it
                // keeps the cursor's meaning ("everything up to here was
                // delivered") true. This is the common case.
                Ok(indexed) if indexed.idx == cursor + 1 => {
                    cursor = indexed.idx;
                    yield Ok(MergedEvent::Live(indexed));
                }
                // A JUMP — the bus is NOT ordered by idx. Two publish paths
                // feed it: `AppState::emit` publishes a local commit
                // immediately, while a peer replica's commit arrives later
                // over LISTEN/NOTIFY (`pg_listener`). So a replica can see
                // its own idx 12 before the notification for a peer's idx
                // 11.
                //
                // Forwarding 12 and advancing the cursor past 11 would
                // strand 11 forever: the late bus copy then looks
                // already-delivered, and a client reconnecting at the
                // advanced Last-Event-ID skips it too. Read the log instead
                // — it IS ordered (idx allocation is one autocommit
                // statement holding a row lock on `sessions`, so
                // same-session commit order equals idx order), so the walk
                // delivers the whole range in order.
                Ok(indexed) if indexed.idx > cursor => {
                    let _ = indexed;
                    catching_up = true;
                }
                // At or below the cursor: already delivered. This is also
                // what drops the LISTEN echo of our own `emit` on the
                // producing replica — that echo fires on every replica
                // INCLUDING the producer, so every local event reaches this
                // bus twice.
                Ok(_) => {}
                // A lag means the bus dropped events this stream never
                // delivered; the log still has them, so report the lag
                // honestly and re-walk from the cursor.
                //
                // NOTE (measured, not assumed): the jump arm above ALREADY
                // subsumes this. A `Lagged` always leaves the ring's newest
                // entries readable, and the next one is by definition above
                // the cursor — so it would trip the jump arm and re-walk one
                // step later. Deleting `catching_up = true` here passes every
                // test in this module. It stays because relying on that tokio
                // ring property is a worse contract than saying what we mean,
                // and because "a lag re-walks" should not be an emergent
                // property of a different arm.
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

    /// Pull one frame and return its idx. Reject a lag notification.
    ///
    /// This helper does NOT check which arm produced the frame. A client
    /// cannot see that difference: it receives frames that carry an idx. A
    /// test that checked the arm would pin HOW the code works instead of
    /// WHAT the client gets. It would also fail a correct redesign — for
    /// example a tail that reorders bus events in place instead of reading
    /// the log again.
    async fn next_idx(s: &mut (impl Stream<Item = Result<MergedEvent, ApiError>> + Unpin)) -> i64 {
        next_frame_idx(s)
            .await
            .expect("expected an event frame, got a lag notification")
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
            false,
        ));

        let mut seen = Vec::with_capacity(TOTAL);
        for _ in 0..TOTAL {
            seen.push(next_idx(&mut s).await);
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
            false,
        ));

        // Drain page one, leaving the walk suspended mid-log.
        let mut seen = Vec::new();
        for _ in 0..REPLAY_PAGE {
            seen.push(next_idx(&mut s).await);
        }

        // A new event arrives NOW — after the walk started, before it
        // finished. Production order: row first, then publish.
        let fresh = append(&meta, id, 600).await;
        bus.publish(id, live_at(fresh));

        // Finish the walk: rows 500..=600, the late arrival included.
        for _ in 0..=(600 - REPLAY_PAGE) {
            seen.push(next_idx(&mut s).await);
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
            false,
        ));

        for _ in 0..REPLAY_PAGE {
            next_idx(&mut s).await;
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
            false,
        ));

        // Walk the short log, then sit in the tail.
        let mut seen = vec![
            next_idx(&mut s).await,
            next_idx(&mut s).await,
            next_idx(&mut s).await,
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
            seen.push(next_idx(&mut s).await);
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

    /// The bus is NOT ordered by idx. `AppState::emit` publishes a local
    /// commit at once; a peer replica's commit arrives later over
    /// LISTEN/NOTIFY. So a replica can see its own idx 4 before the
    /// notification for a peer's idx 3.
    ///
    /// The stream must NOT forward the jump and advance past the hole. That
    /// strands idx 3: its late bus copy then looks already-delivered, and a
    /// client reconnecting at the advanced Last-Event-ID skips it too. Found
    /// by review; prod runs two coordinator replicas, so this is reachable.
    #[tokio::test]
    async fn out_of_order_bus_event_walks_instead_of_skipping_the_hole() {
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
            false,
        ));
        for _ in 0..3 {
            next_idx(&mut s).await;
        }
        // Cursor is now 2.

        // Both rows commit — idx allocation serializes on a row lock, so a
        // visible idx 4 implies idx 3 is visible too.
        let three = append(&meta, id, 3).await;
        let four = append(&meta, id, 4).await;
        assert_eq!((three, four), (3, 4));

        // Only idx 4 reaches the bus. The notification for idx 3 is still
        // in flight on this replica.
        bus.publish(id, live_at(four));

        // The stream must close the hole from the log, IN ORDER — not
        // forward 4 and abandon 3.
        assert_eq!(next_idx(&mut s).await, 3, "the hole is filled first");
        assert_eq!(next_idx(&mut s).await, 4, "then the jumped event");

        // The late notification for idx 3 must not duplicate it.
        bus.publish(id, live_at(three));
        let dup = tokio::time::timeout(std::time::Duration::from_millis(50), s.next()).await;
        assert!(
            dup.is_err(),
            "the late notification is dropped, not re-sent"
        );
    }

    /// `pg_listener` re-broadcasts on EVERY replica including the producer,
    /// so each local event reaches the bus twice: once from `emit`, once as
    /// the LISTEN echo. The cursor must drop the echo.
    ///
    /// The pre-ADR-0105 boundary was a CONSTANT high-water fixed at stream
    /// start, so both copies passed it and the client got the event twice.
    #[tokio::test]
    async fn listen_echo_of_a_local_emit_is_dropped() {
        let meta = sim_meta();
        let id = new_session(&meta).await;
        append(&meta, id, 0).await;

        let bus = SessionEventBus::default();
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
            false,
        ));
        assert_eq!(next_idx(&mut s).await, 0);

        // `emit` publishes idx 1, then the LISTEN echo publishes it again.
        let one = append(&meta, id, 1).await;
        bus.publish(id, live_at(one));
        bus.publish(id, live_at(one));

        match s.next().await {
            Some(Ok(MergedEvent::Live(ev))) => assert_eq!(ev.idx, 1),
            _ => panic!("expected the live event once"),
        }
        let dup = tokio::time::timeout(std::time::Duration::from_millis(50), s.next()).await;
        assert!(dup.is_err(), "the echo must not reach the client");
    }

    /// Pull one frame. Return its idx, or `None` for a lag notification.
    ///
    /// A lag notification carries no idx and does not move the cursor. So a
    /// caller that counts events must skip it. [`next_idx`] rejects it
    /// instead, for tests where no lag can occur.
    async fn next_frame_idx(
        s: &mut (impl Stream<Item = Result<MergedEvent, ApiError>> + Unpin),
    ) -> Option<i64> {
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), s.next())
            .await
            .expect("stream stalled — an event was neither delivered nor recoverable");
        match item {
            Some(Ok(MergedEvent::Replay(ev))) => Some(ev.idx),
            Some(Ok(MergedEvent::Live(ev))) => Some(ev.idx),
            Some(Ok(MergedEvent::Lagged(_))) => None,
            Some(Err(e)) => panic!("unexpected stream error: {e:?}"),
            None => panic!("stream ended before delivering the log tail"),
        }
    }

    /// Shared config for the publish-order properties.
    ///
    /// ADR 0099 H3 pins a counterexample to a file. Each property gets its OWN
    /// file: a shared one would replay one property's seed through the other's
    /// strategy, where that seed no longer reproduces the case it was pinned
    /// for.
    fn order_prop_cfg(cases: u32, regressions: &'static str) -> proptest::test_runner::Config {
        proptest::test_runner::Config {
            cases,
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::Direct(regressions),
            )),
            ..proptest::test_runner::Config::default()
        }
    }

    fn order_prop_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
    }

    /// One case of the publish-order property, shared by both properties
    /// below — they differ only in where the walk ends.
    ///
    /// Seed `seeded` events and walk them. Append one more event per entry of
    /// `order_keys`, then publish those to the bus in the order the keys sort
    /// into; `echo` publishes each one twice.
    ///
    /// Asserts only what a client can observe: every event exactly once, in
    /// ascending idx order, and nothing after the last one. Which arm
    /// delivered a frame, and how many log reads it took, are not asserted.
    async fn publish_order_case(seeded: usize, order_keys: &[u32], echo: bool) {
        let extra = order_keys.len();
        let meta = sim_meta();
        let id = new_session(&meta).await;
        for n in 0..seeded {
            append(&meta, id, n).await;
        }

        let bus = SessionEventBus::default();
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
            false,
        ));
        for _ in 0..seeded {
            next_idx(&mut s).await;
        }
        // The cursor now sits at `seeded - 1`.

        // The log is always contiguous and complete: idx allocation is one
        // autocommit statement under a row lock on `sessions`, so same-session
        // commit order equals idx order.
        let mut appended = Vec::with_capacity(extra);
        for n in 0..extra {
            appended.push(append(&meta, id, seeded + n).await);
        }

        // Publish in an ARBITRARY order — the part production does not
        // control. `echo` replays each publication, modelling the LISTEN echo
        // of a local emit.
        let mut order: Vec<usize> = (0..extra).collect();
        order.sort_by_key(|&i| order_keys[i]);
        for &i in &order {
            bus.publish(id, live_at(appended[i]));
            if echo {
                bus.publish(id, live_at(appended[i]));
            }
        }

        let mut got = Vec::with_capacity(extra);
        while got.len() < extra {
            if let Some(idx) = next_frame_idx(&mut s).await {
                got.push(idx);
            }
        }

        let expected: Vec<i64> = ((seeded as i64)..(seeded as i64 + extra as i64)).collect();
        assert_eq!(
            got, expected,
            "every event exactly once, ascending, for publish order {order:?} \
             (seeded {seeded}, echo {echo})"
        );

        // Nothing further: duplicates and echoes must not leak out.
        let leaked = tokio::time::timeout(std::time::Duration::from_millis(50), s.next()).await;
        assert!(
            leaked.is_err(),
            "extra frame after the full sequence (seeded {seeded}, publish order {order:?})"
        );
    }

    proptest::proptest! {
        #![proptest_config(order_prop_cfg(64, "proptest-regressions/event_stream_order.txt"))]

        /// THE property the example tests failed to state: whatever order the
        /// bus publishes in, the client receives every event exactly once, in
        /// ascending idx order.
        ///
        /// This is the test that would have caught the out-of-order defect.
        /// Every example test above publishes bus events in ascending,
        /// contiguous order — so they encode the author's model of the
        /// producer rather than challenging it, and the difference between
        /// `idx > cursor` and `idx == cursor + 1` is invisible to all of them.
        ///
        /// Production has TWO publishers whose relative order no one
        /// controls: `AppState::emit` (local, immediate) and `pg_listener`
        /// (peer replica, after LISTEN/NOTIFY, and it echoes local events
        /// too). So arbitrary order — including duplicates — is the real
        /// contract. The log stays contiguous, which is what makes recovery
        /// possible.
        #[test]
        fn any_bus_publish_order_delivers_every_event_once_in_order(
            seeded in 1usize..5,
            order_keys in proptest::collection::vec(0u32..48, 1..10),
            echo in proptest::bool::ANY,
        ) {
            order_prop_rt().block_on(publish_order_case(seeded, &order_keys, echo));
        }
    }

    proptest::proptest! {
        #![proptest_config(order_prop_cfg(
            16,
            "proptest-regressions/event_stream_order_page_boundary.txt",
        ))]

        /// The same property, with the walk ending ON a page boundary — the
        /// one place the two states meet.
        ///
        /// The property above seeds a handful of events, so every case ends
        /// the walk on a short first page. It never sees the seam. Here the
        /// walk ends one event short of a full page, exactly on one, and one
        /// past one. An exactly-full page keeps the walk going, so it costs an
        /// extra empty read before the tail starts; a jump arriving then sends
        /// it back for a THIRD read. That interleaving has no other coverage.
        ///
        /// 16 cases, not 64: each one seeds ~500 events. The distribution is
        /// what matters here, not the count — the arbitrary-order property
        /// above carries the volume.
        #[test]
        fn disorder_at_a_page_boundary_delivers_every_event_once_in_order(
            seeded in proptest::prop_oneof![
                proptest::strategy::Just(REPLAY_PAGE as usize - 1),
                proptest::strategy::Just(REPLAY_PAGE as usize),
                proptest::strategy::Just(REPLAY_PAGE as usize + 1),
            ],
            order_keys in proptest::collection::vec(0u32..48, 1..10),
            echo in proptest::bool::ANY,
        ) {
            order_prop_rt().block_on(publish_order_case(seeded, &order_keys, echo));
        }
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
            false,
        ));
        for _ in 0..3 {
            next_idx(&mut s).await;
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

    /// ADR 0108 B: a `durable_only` subscriber must never receive an
    /// ephemeral chunk — but durable events and the `lagged` sentinel (the
    /// one idx-less frame a cursor-based consumer needs) must still flow.
    /// The unset-flag behavior is guarded by
    /// [`ephemeral_chunk_bypasses_the_cursor_gate`] above.
    #[tokio::test]
    async fn durable_only_suppresses_chunks_but_delivers_events_and_lag() {
        const CAP: usize = 8;
        const BURST: i64 = 20;

        let meta = sim_meta();
        let id = new_session(&meta).await;
        for n in 0..3 {
            append(&meta, id, n).await;
        }

        // A small bus so the lag leg below provably overflows it.
        let bus = SessionEventBus::new(CAP);
        let mut s = Box::pin(merged_event_stream(
            meta.clone(),
            id,
            bus.subscribe(id),
            None,
            true,
        ));
        for _ in 0..3 {
            next_idx(&mut s).await;
        }

        // An ephemeral chunk must be dropped server-side: the next poll
        // must not resolve.
        bus.publish(
            id,
            IndexedEvent {
                idx: 0,
                event: chunk_event(),
                ephemeral: true,
            },
        );
        let leaked = tokio::time::timeout(std::time::Duration::from_millis(50), s.next()).await;
        assert!(leaked.is_err(), "durable_only leaked an ephemeral chunk");

        // A durable event still flows, and the dropped chunk did not
        // disturb the cursor.
        let fresh = append(&meta, id, 3).await;
        bus.publish(id, live_at(fresh));
        match s.next().await {
            Some(Ok(MergedEvent::Live(ev))) => assert_eq!(ev.idx, 3),
            _ => panic!("durable event did not reach the durable_only subscriber"),
        }

        // The lagged sentinel still passes: overflow the bus without
        // polling, then expect Lagged before the backfill.
        for n in 4..(4 + BURST) {
            let idx = append(&meta, id, n as usize).await;
            bus.publish(id, live_at(idx));
        }
        assert!(
            BURST > CAP as i64,
            "burst must exceed bus depth or no lag occurs"
        );
        match s.next().await {
            Some(Ok(MergedEvent::Lagged(n))) => assert!(n > 0, "lag reports a positive count"),
            other => panic!(
                "durable_only must still deliver the lagged sentinel, got {:?}",
                other.map(|r| r.map(|ev| merged_to_parts(ev).1))
            ),
        }

        // ...and the hole refills from the log as usual.
        let last = 3 + BURST;
        let mut seen = Vec::new();
        for _ in 4..=last {
            seen.push(next_idx(&mut s).await);
        }
        let expected: Vec<i64> = (4..=last).collect();
        assert_eq!(seen, expected, "the lag backfill is unaffected by the flag");
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
