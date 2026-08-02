//! ADR 0108 A8: the heartbeat harness-attach disagreement REPAIR.
//!
//! A host heartbeat carries two sandbox sets: `running` (the backend's
//! list) and `attached` (the hub's registered harness connections). An
//! Active session whose sandbox is running but not attached is a hole
//! in the delivery path: `send_prompt` forwards into a socket with no
//! reader, returns Ok, and the outbox row waits out the full
//! ACK_TIMEOUT before the retry finds `NotFound` and re-establishes
//! the harness (prod 7eddce62: a parked session un-parked in 120 ms,
//! then stalled 35.5 s on exactly this shape). The repair recalls the
//! session's waiting outbox rows (`outbox_make_due`) and enqueues a
//! Deliver op directly, so recovery is bounded by the heartbeat
//! cadence, not by the ack timeout.
//!
//! The decision logic lives here — NOT in the HTTP handler — so the
//! DST swarm can drive `run_once` from the sim world without the wire
//! layer (ADR 0098: keep the `spawn()`/`run_once()` split).

use std::collections::BTreeSet;

use engram_core::SandboxId;

use crate::state::SharedState;

/// One repair pass over one host's heartbeat view. Returns the number
/// of sessions re-driven (rows recalled + Deliver op enqueued).
///
/// Pure over its inputs plus the metadata store: no clock, no entropy,
/// no wire types.
pub async fn run_once(
    state: &SharedState,
    host_id: engram_core::HostId,
    running: &BTreeSet<SandboxId>,
    attached: &BTreeSet<SandboxId>,
) -> usize {
    let resident = match state
        .services
        .meta
        .list_resident_sandbox_assignments_on_host(host_id)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!(host_id = %host_id, error = %e,
                "harness desync: resident-assignment read failed; skipping this tick");
            return 0;
        }
    };
    let mut redriven = 0usize;
    // Active-only ON PURPOSE: a parked VM is paused, so "running but
    // no attached harness" is its normal, healthy shape — it must not
    // tick the disagreement alarm or the repair.
    for (session_id, sandbox_id, _) in resident
        .into_iter()
        .filter(|(_, _, st)| *st == engram_core::types::SessionState::Active)
    {
        if !running.contains(&sandbox_id) || attached.contains(&sandbox_id) {
            continue;
        }
        ::metrics::counter!(crate::metrics::HARNESS_ATTACH_DISAGREEMENT_TOTAL).increment(1);
        // Recall the waiting rows; a row moved means a delivery is
        // provably parked behind a dead link, so drive it now rather
        // than wait for the shim's rescan. `outbox_make_due` never
        // bumps `attempts` and is idempotent across heartbeats, so a
        // repeated disagreement never inflates the retry backoff.
        match state.services.meta.outbox_make_due(session_id).await {
            Ok(0) => {
                tracing::debug!(host_id = %host_id, %session_id, %sandbox_id,
                    "harness desync: running sandbox with no attached harness; no waiting outbox rows");
            }
            Ok(moved) => {
                ::metrics::counter!(crate::metrics::HARNESS_DESYNC_REDRIVEN_TOTAL).increment(1);
                tracing::info!(host_id = %host_id, %session_id, %sandbox_id, moved,
                    "harness desync repair: recalled waiting outbox rows; enqueueing deliver op");
                crate::outbox_delivery::enqueue_deliver_op(state, session_id).await;
                redriven += 1;
            }
            Err(e) => {
                tracing::debug!(host_id = %host_id, %session_id, error = %e,
                    "harness desync: outbox make-due failed (ack timeout backstops)");
            }
        }
    }
    redriven
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::state::tests::build_state_for_session;
    use engram_core::types::session_op::OpKind;
    use engram_core::types::{Session, SessionState};
    use engram_core::{HostId, SessionId};

    fn session(status: SessionState, host_id: HostId, sandbox_id: SandboxId) -> Session {
        Session {
            id: SessionId::new(),
            status,
            host_id: Some(host_id),
            sandbox_id: Some(sandbox_id),
            image: "test/repo:desync".into(),
            mode: engram_core::types::session::SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    /// An unacked row parked on a future `not_before` — the shape a
    /// forward into a dead link leaves behind (delivered, waiting out
    /// ACK_TIMEOUT).
    fn waiting_row(session_id: SessionId) -> engram_core::types::outbox::OutboxRow {
        let now = chrono::Utc::now();
        engram_core::types::outbox::OutboxRow {
            prompt_id: "p-desync".into(),
            session_id,
            kind: engram_core::types::outbox::OutboxKind::Prompt,
            payload: serde_json::json!({"text": "hi"}),
            created_at: now,
            attempts: 1,
            not_before: now + chrono::Duration::seconds(30),
            delivered_at: Some(now),
            acked_at: None,
        }
    }

    #[tokio::test]
    async fn disagreeing_active_session_is_redriven() {
        let host_id = HostId::new();
        let sandbox_id = SandboxId::new();
        let (state, mini, _local) =
            build_state_for_session(session(SessionState::Active, host_id, sandbox_id));
        let session_id = mini.session.lock().id;
        mini.outbox.lock().push(waiting_row(session_id));

        let running: BTreeSet<_> = [sandbox_id].into();
        let attached = BTreeSet::new();
        assert_eq!(run_once(&state, host_id, &running, &attached).await, 1);

        // The waiting row was recalled to due, with no attempts bump.
        let row = mini.outbox.lock()[0].clone();
        assert!(row.not_before <= chrono::Utc::now(), "row pulled to due");
        assert_eq!(row.attempts, 1, "make_due must not bump attempts");
        // A Deliver op was enqueued directly (no shim-rescan wait).
        assert!(
            mini.ops
                .all()
                .iter()
                .any(|o| o.session_id == session_id && o.kind == OpKind::Deliver),
            "deliver op enqueued",
        );
    }

    #[tokio::test]
    async fn attached_session_is_untouched() {
        let host_id = HostId::new();
        let sandbox_id = SandboxId::new();
        let (state, mini, _local) =
            build_state_for_session(session(SessionState::Active, host_id, sandbox_id));
        let session_id = mini.session.lock().id;
        mini.outbox.lock().push(waiting_row(session_id));

        let both: BTreeSet<_> = [sandbox_id].into();
        assert_eq!(run_once(&state, host_id, &both, &both).await, 0);
        assert!(
            mini.outbox.lock()[0].not_before > chrono::Utc::now(),
            "row still waiting",
        );
        assert!(mini.ops.all().is_empty(), "no deliver op enqueued");
    }

    #[tokio::test]
    async fn non_active_resident_session_is_untouched() {
        // Parked is resident (reserves host memory) and its "running,
        // no attached harness" shape is healthy — the repair must skip
        // it exactly like the alarm does.
        let host_id = HostId::new();
        let sandbox_id = SandboxId::new();
        let (state, mini, _local) =
            build_state_for_session(session(SessionState::Parked, host_id, sandbox_id));
        let session_id = mini.session.lock().id;
        mini.outbox.lock().push(waiting_row(session_id));

        let running: BTreeSet<_> = [sandbox_id].into();
        let attached = BTreeSet::new();
        assert_eq!(run_once(&state, host_id, &running, &attached).await, 0);
        assert!(
            mini.outbox.lock()[0].not_before > chrono::Utc::now(),
            "row still waiting",
        );
        assert!(mini.ops.all().is_empty(), "no deliver op enqueued");
    }
}
