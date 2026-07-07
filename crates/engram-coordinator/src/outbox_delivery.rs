//! ADR 0073 phase 2: the outbox delivery driver.
//!
//! `SendPrompt`/`AnswerQuestion` durably enqueue (`session_outbox`) and
//! return 202 immediately; THIS loop owns everything that used to make
//! delivery synchronous and lossy: the auto-resume of Idle sessions
//! (now *behind* the enqueue — the caller never waits on it), the
//! forward to the host relay, the in-place harness reattach on the
//! unbound desync, and redelivery until the confirming harness event
//! acks the row (`run_started{prompt_id}` / `prompt_queued` /
//! `question_answered`). Redelivery is safe end-to-end: the harness
//! dedups prompts by `prompt_id` (ADR 0052) and answers are
//! idempotent (ADR 0054).
//!
//! Wake sources: a PG `NOTIFY session_outbox` fired by every enqueue
//! (any replica), relayed here via `pg_listener` — the hot path — plus
//! a poll-interval rescan as the crash-recovery / redelivery fallback.
//!
//! Concurrency model: per-session single-flight *within this process*
//! (a `DashSet` of in-flight sessions); cross-pod duplication is
//! tolerated by design — both drivers deliver, the harness dedups, the
//! first ingested confirming event acks the row. Per-session ORDER is
//! the PG row order: the driver only ever forwards the oldest un-acked
//! row, and a row must be delivered (relay handoff) before the next
//! becomes eligible on the following pass.
//!
//! Epic #543 note: when the per-session op log lands, `deliver`
//! becomes an op verb ordered behind in-flight resume/evict ops and
//! this driver's single-flight moves into the op executor.

use std::sync::Arc;
use std::time::Duration;

use engram_core::types::outbox::{OutboxKind, OutboxRow};
use engram_core::SandboxError;
use tokio::sync::Notify;

use crate::error::ApiError;
use crate::state::SharedState;

/// Fallback rescan cadence. The NOTIFY wake is the hot path; this only
/// bounds redelivery latency after a crash or a missed notification.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// How long a delivered row waits for its confirming event before it
/// re-becomes due. Generous: covers a slow first token from the agent
/// after a cold resume; the cost of redelivering early is nil (dedup).
const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// ADR 0074 rung-2: on a `NotFound` (VM alive, no harness bound), how many
/// plain-retry attempts to wait for the in-guest harness to SELF-reattach
/// before falling back to `start_agent`. A parked-paused un-pause leaves the
/// harness alive in RAM; it re-dials the hub on its own within a beat, and a
/// premature `start_agent` would kill it and pay a full ~40s handshake. Over
/// `failure_backoff`'s 2s/4s/6s schedule this is a ~12s self-reattach window —
/// ample for an in-guest redial, a small delay for the rare genuine desync.
const HARNESS_SELF_REATTACH_ATTEMPTS: i32 = 3;

/// Backoff for rows whose delivery attempt FAILED (resume error, host
/// error). Grows linearly with attempts, capped — an unresumable
/// session shouldn't spin the driver, and there is deliberately no
/// terminal give-up: the row stays until acked or the session dies
/// (FK CASCADE). Un-deliverable ≠ droppable — the user asked.
fn failure_backoff(attempts: i32) -> Duration {
    let secs = ((attempts.max(0) as u64) + 1) * 2;
    Duration::from_secs(secs.min(60))
}

pub fn spawn(state: SharedState, wake: Arc<Notify>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let inflight: Arc<dashmap::DashSet<engram_core::SessionId>> =
            Arc::new(dashmap::DashSet::new());
        loop {
            let due = match state.services.meta.outbox_due_sessions().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "outbox: due-session scan failed");
                    Vec::new()
                }
            };
            for session_id in due {
                if !inflight.insert(session_id) {
                    continue; // already being drained by this process
                }
                let state = state.clone();
                let inflight = inflight.clone();
                tokio::spawn(async move {
                    drain_session(&state, session_id).await;
                    inflight.remove(&session_id);
                });
            }
            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(RESCAN_INTERVAL) => {}
            }
        }
    })
}

/// Deliver the session's due rows oldest-first until none are due or
/// an attempt defers. One resume covers the whole batch.
async fn drain_session(state: &SharedState, session_id: engram_core::SessionId) {
    loop {
        let row = match state.services.meta.outbox_next_due(session_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(session_id = %session_id, error = %e, "outbox: next-due fetch failed");
                return;
            }
        };
        match deliver_one(state, &row).await {
            Ok(()) => {
                if let Err(e) = state
                    .services
                    .meta
                    .outbox_mark_delivered(&row.prompt_id, ACK_TIMEOUT)
                    .await
                {
                    tracing::warn!(prompt_id = %row.prompt_id, error = %e, "outbox: mark_delivered failed");
                    return;
                }
                ::metrics::counter!(crate::metrics::OUTBOX_DELIVERED_TOTAL).increment(1);
            }
            Err(DeliverError::Terminal(reason)) => {
                // The session can never receive this (Dead/Gone). Ack
                // the row as consumed-by-termination so it stops
                // waking the driver; the transcript already shows the
                // user's ask, and the session's terminal state is the
                // visible outcome.
                tracing::info!(
                    session_id = %session_id,
                    prompt_id = %row.prompt_id,
                    %reason,
                    "outbox: dropping row for terminal session",
                );
                let _ = state.services.meta.outbox_ack(&row.prompt_id).await;
                ::metrics::counter!(crate::metrics::OUTBOX_DROPPED_TERMINAL_TOTAL).increment(1);
            }
            Err(DeliverError::Retry(reason)) => {
                tracing::debug!(
                    session_id = %session_id,
                    prompt_id = %row.prompt_id,
                    attempts = row.attempts,
                    %reason,
                    "outbox: delivery deferred",
                );
                let _ = state
                    .services
                    .meta
                    .outbox_defer(&row.prompt_id, failure_backoff(row.attempts))
                    .await;
                ::metrics::counter!(crate::metrics::OUTBOX_DEFERRED_TOTAL).increment(1);
                return; // preserve order: don't skip ahead of a stuck head
            }
        }
    }
}

enum DeliverError {
    /// Try again after backoff (resume failed transiently, host error,
    /// harness not yet re-bound).
    Retry(String),
    /// The session is terminally gone — drop the row.
    Terminal(String),
}

async fn deliver_one(state: &SharedState, row: &OutboxRow) -> Result<(), DeliverError> {
    // Resume happens BEHIND the enqueue — this is the line that used to
    // hold the user's SendPrompt hostage for 12-89s.
    match crate::api::snapshot::ensure_active(state, row.session_id).await {
        Ok(()) => {}
        Err(ApiError::Gone(msg)) => return Err(DeliverError::Terminal(msg)),
        Err(e) => return Err(DeliverError::Retry(format!("ensure_active: {e}"))),
    }
    let Some(sandbox_id) = state.resolve_sandbox(row.session_id).await else {
        return Err(DeliverError::Retry(
            "no live sandbox after ensure_active".into(),
        ));
    };

    let forward = || async {
        match row.kind {
            OutboxKind::Prompt => {
                let text = row
                    .payload
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                state
                    .services
                    .host
                    .send_prompt(sandbox_id, row.prompt_id.clone(), text)
                    .await
            }
            OutboxKind::Answer => {
                let tool_call_id = row
                    .payload
                    .get("tool_call_id")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default()
                    .to_string();
                let answers: engram_harness_proto::Answers = row
                    .payload
                    .get("answers")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| SandboxError::InvalidSpec(format!("outbox answers: {e}")))?
                    .unwrap_or_default();
                state
                    .services
                    .host
                    .answer_question(sandbox_id, tool_call_id, answers)
                    .await
            }
        }
    };

    match forward().await {
        Ok(()) => Ok(()),
        Err(SandboxError::NotFound) => {
            // The VM is alive but no harness is attached for delivery. There are
            // TWO causes, and they want OPPOSITE responses:
            //
            //  1. ADR 0074 rung-2 parked-paused un-pause: the VM was PAUSED with
            //     the harness alive in RAM, then un-paused by the ascent. The
            //     harness's ADR 0073 self-auth loop re-dials the hub on its own
            //     (its vsock dropped across the pause) and re-binds with its
            //     still-valid epoch within a beat. `start_agent` here is not just
            //     wasteful — it KILLS that self-reattaching harness and pays a
            //     full ~40s agent_handshake (respawn + resume prefault), turning
            //     a returning-user un-pause (should be sub-second) into WORSE
            //     than a plain evict+resume. This was the rung-2 regression.
            //
            //  2. Genuine harness-unbound desync (harness process gone, VM alive):
            //     the harness will NOT come back on its own — `start_agent` (the
            //     e35ed1fa self-heal) is required.
            //
            // We can't synchronously distinguish them (no per-sandbox harness-
            // attach RPC), so give the self-reattach a bounded window of plain
            // retries FIRST; only fall back to `start_agent` once the harness
            // clearly isn't self-reattaching. The window (`failure_backoff`
            // cumulative over `HARNESS_SELF_REATTACH_ATTEMPTS`) is a few seconds
            // — ample for an in-guest redial, a small delay for the rare desync.
            if row.attempts < HARNESS_SELF_REATTACH_ATTEMPTS {
                return Err(DeliverError::Retry(
                    "harness not attached — awaiting in-guest self-reattach".into(),
                ));
            }
            match crate::api::snapshot::reattach_harness_in_place(state, row.session_id, sandbox_id)
                .await
            {
                Ok(true) => Err(DeliverError::Retry(
                    "harness reattach issued (start_agent fallback)".into(),
                )),
                Ok(false) => Err(DeliverError::Retry(
                    "session moved off the sandbox mid-delivery".into(),
                )),
                Err(e) => Err(DeliverError::Retry(format!("reattach failed: {e}"))),
            }
        }
        Err(e) => Err(DeliverError::Retry(format!("forward: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_backoff_grows_and_caps() {
        assert_eq!(failure_backoff(0), Duration::from_secs(2));
        assert_eq!(failure_backoff(4), Duration::from_secs(10));
        assert_eq!(failure_backoff(1000), Duration::from_secs(60));
    }
}
