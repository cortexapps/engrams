//! ADR 0108 A8: the heartbeat harness-attach disagreement REPAIR.
//!
//! A host heartbeat carries two sandbox sets: `running` (the backend's
//! list) and `attached` (the hub's registered harness connections). An
//! Active session whose sandbox is running but not attached is a hole
//! in the delivery path: `send_prompt` forwards into a socket with no
//! reader, returns Ok, and the outbox row waits out the full
//! ACK_TIMEOUT before the retry finds `NotFound` and re-establishes
//! the harness (a parked session that un-parks hits this shape). The
//! repair recalls the
//! session's waiting outbox rows (`outbox_make_due`) and enqueues a
//! Deliver op directly, so recovery is bounded by the heartbeat
//! cadence, not by the ack timeout.
//!
//! With no waiting rows the repair re-attaches the harness at once, so
//! the next prompt does not pay for it. Every memory capture resets the
//! vsock device: the hub drops the harness link, while the idle harness,
//! which writes nothing, keeps its end half open and does not re-dial
//! until agentd signals it.
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
/// Pure over its inputs plus the metadata store and the injected clock:
/// no entropy, no wire types.
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
                if reattach_idle_harness(state, session_id, sandbox_id).await {
                    redriven += 1;
                }
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

/// Start a re-attach of the harness of an Active session with nothing to
/// deliver, detached from the heartbeat: `start_agent` waits for the
/// guest. Skipped while an attach is in flight (the attach grace) and for
/// a session with no harness. Returns whether a re-attach started.
async fn reattach_idle_harness(
    state: &SharedState,
    session_id: engram_core::SessionId,
    sandbox_id: SandboxId,
) -> bool {
    let now = state.services.clock.now_utc();
    if let Some(stamp) = state.attach_grace.get(&session_id).map(|e| *e.value()) {
        let grace = chrono::Duration::from_std(crate::session_verbs::ATTACH_GRACE)
            .expect("ATTACH_GRACE fits chrono::Duration");
        if now.signed_duration_since(stamp) < grace {
            return false;
        }
    }
    match state.services.meta.get_session(session_id).await {
        Ok(session)
            if !session.mode.is_dev_vm()
                && session.status == engram_core::types::SessionState::Active
                && session.sandbox_id == Some(sandbox_id) => {}
        _ => return false,
    }
    let state = state.clone();
    tokio::spawn(async move { reattach_under_claim(&state, session_id, sandbox_id, now).await });
    true
}

/// The re-attach under an inline op claim: the session's op lane orders
/// it against a delivery, an eviction or a move. A busy lane skips it.
async fn reattach_under_claim(
    state: &SharedState,
    session_id: engram_core::SessionId,
    sandbox_id: SandboxId,
    now: chrono::DateTime<chrono::Utc>,
) {
    let claim = match crate::session_ops::OpClaim::try_acquire(
        state,
        session_id,
        engram_core::types::session_op::OpKind::Deliver,
        serde_json::json!({ "reason": "harness_reattach" }),
    )
    .await
    {
        Ok(Some(claim)) => claim,
        Ok(None) => return,
        Err(e) => {
            tracing::debug!(%session_id, error = %e, "harness desync: op claim failed; next heartbeat retries");
            return;
        }
    };
    let result = crate::api::snapshot::reattach_harness_in_place(
        state,
        session_id,
        sandbox_id,
        claim.fence(),
    )
    .await;
    if matches!(result, Ok(true)) {
        // The attach is in flight: a prompt in the next moments defers
        // to it instead of signalling the harness again.
        state.attach_grace.insert(session_id, now);
        ::metrics::counter!(crate::metrics::HARNESS_DESYNC_REDRIVEN_TOTAL).increment(1);
        tracing::info!(%session_id, %sandbox_id,
            "harness desync repair: re-attached an idle harness");
    }
    match result {
        Ok(_) => {
            claim
                .finish(engram_core::types::session_op::OpState::Done, None)
                .await
        }
        Err(e) => {
            let error = e.to_string();
            tracing::debug!(%session_id, %sandbox_id, %error,
                "harness desync: idle re-attach failed; next heartbeat retries");
            claim
                .finish(
                    engram_core::types::session_op::OpState::Failed,
                    Some(&error),
                )
                .await
        }
    }
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
            last_event_at: None,
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

    /// With nothing waiting, the repair re-attaches the idle harness under
    /// an op claim, so the next prompt finds it attached.
    #[tokio::test]
    async fn an_idle_unattached_harness_is_reattached_under_an_op_claim() {
        let host_id = HostId::new();
        let sandbox_id = SandboxId::new();
        let (state, mini, _local) =
            build_state_for_session(session(SessionState::Active, host_id, sandbox_id));
        let session_id = mini.session.lock().id;

        let running: BTreeSet<_> = [sandbox_id].into();
        let attached = BTreeSet::new();
        assert_eq!(run_once(&state, host_id, &running, &attached).await, 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let claimed = mini.ops.all().iter().any(|o| {
                o.session_id == session_id
                    && o.kind == OpKind::Deliver
                    && o.state != engram_core::types::session_op::OpState::Running
            });
            if claimed {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the re-attach never ran under a finished op claim"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// An attach in flight (a boot, a resume, an earlier repair) and a
    /// session with no harness are left alone.
    #[tokio::test]
    async fn an_attach_in_flight_or_a_dev_vm_is_not_reattached() {
        let host_id = HostId::new();
        let sandbox_id = SandboxId::new();
        let (state, mini, _local) =
            build_state_for_session(session(SessionState::Active, host_id, sandbox_id));
        let session_id = mini.session.lock().id;
        let running: BTreeSet<_> = [sandbox_id].into();
        let attached = BTreeSet::new();

        state.attach_grace.insert(session_id, chrono::Utc::now());
        assert_eq!(run_once(&state, host_id, &running, &attached).await, 0);

        state.attach_grace.remove(&session_id);
        mini.session.lock().mode = engram_core::types::session::SessionMode::DevVm;
        assert_eq!(run_once(&state, host_id, &running, &attached).await, 0);
        assert!(mini.ops.all().is_empty(), "no op claimed");
    }
}
