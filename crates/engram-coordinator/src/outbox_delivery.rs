//! ADR 0073 phase 2 → ADR 0079 pass 2: the outbox delivery SHIM.
//!
//! `SendPrompt`/`AnswerQuestion` durably enqueue (`session_outbox`) and
//! return 202 immediately. The delivery body — auto-resume ordering, the
//! forward to the host relay, the in-place harness reattach on the
//! unbound desync, redelivery until the confirming harness event acks
//! the row — lives in the **deliver verb** (`session_verbs::deliver`),
//! ordered behind any in-flight resume/evict op on the same session by
//! the op log. This loop only converts "a session has a due outbox row"
//! into "a Deliver op is enqueued for it".
//!
//! Wake sources: a PG `NOTIFY session_outbox` fired by every enqueue
//! (any replica), relayed here via `pg_listener` — plus a poll-interval
//! rescan that owns REDELIVERY: a delivered-but-unacked row re-becomes
//! due on its own `not_before` (`ACK_TIMEOUT`), and it's this rescan
//! that re-enqueues the Deliver op for it. The prompt path also enqueues
//! a Deliver op directly (`enqueue_deliver_op`) so first delivery never
//! waits for a wake hop.
//!
//! Duplicate suppression: `op_pending_exists(session, Deliver)` — a
//! queued-or-running Deliver op means the executor already owns the
//! drain; enqueueing another would only append dead rows. The guard is
//! advisory (a racing wake can still slip one through) — a duplicate
//! Deliver op finds no due rows and completes as a no-op.

use std::sync::Arc;
use std::time::Duration;

use engram_core::types::session_op::OpKind;
use tokio::sync::Notify;

use crate::state::SharedState;

/// Fallback rescan cadence. The NOTIFY wake + the prompt path's direct
/// enqueue are the hot path; this bounds redelivery latency (an unacked
/// row re-becoming due) and crash recovery.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Enqueue a Deliver op for `session_id` unless one is already pending.
/// Best-effort: an error is logged, not surfaced — the shim's next wake
/// (or any replica's) retries, and the outbox row is the durable state.
pub(crate) async fn enqueue_deliver_op(state: &SharedState, session_id: engram_core::SessionId) {
    match state
        .services
        .meta
        .op_pending_exists(session_id, OpKind::Deliver)
        .await
    {
        Ok(true) => return, // the executor already owns this session's drain
        Ok(false) => {}
        Err(e) => {
            tracing::debug!(session_id = %session_id, error = %e,
                "outbox shim: op_pending_exists probe failed; enqueueing anyway");
        }
    }
    if let Err(e) = crate::session_ops::enqueue(
        state,
        session_id,
        OpKind::Deliver,
        serde_json::json!({}),
        None,
    )
    .await
    {
        tracing::warn!(session_id = %session_id, error = %e,
            "outbox shim: deliver op enqueue failed (next wake retries)");
    }
}

pub fn spawn(state: SharedState, wake: Arc<Notify>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let due = match state.services.meta.outbox_due_sessions().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "outbox: due-session scan failed");
                    Vec::new()
                }
            };
            for session_id in due {
                enqueue_deliver_op(&state, session_id).await;
            }
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(RESCAN_INTERVAL) => {}
            }
        }
    })
}
