//! Live-Postgres tests for the ADR 0079 `session_ops` store layer: the
//! one-round-trip enqueue-and-claim happy path, strict queue ordering
//! behind a running op, idempotency-key dedup, the fencing epoch
//! (fenced writes go 0-row after a reclaim re-claims the op), retry
//! requeue with `not_before` backoff, and both cancellation arms
//! (queued → cancelled; running → cooperative `_cancel` flag). This
//! pins the SQL the op executor stands on — the executor's control
//! flow is thin over these operations.
//!
//! Modeled on `outbox_live_pg.rs`; `connect()` returns the concrete
//! `PostgresStore` (not `Arc<dyn MetadataStore>`) because the reclaim
//! test must age `heartbeat_at` with raw SQL — there is deliberately no
//! trait surface for rewinding a heartbeat.
//!
//! `#[ignore]`'d by default; requires Postgres at
//! `ENGRAM_TEST_DATABASE_URL`. Wired into ci.yml's Postgres-gated list.

// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::MetadataStore;
use engram_core::types::session::{SessionMode, SessionSpec};
use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState, SessionOp};
use engram_core::types::SessionState;
use engram_core::SessionId;

async fn connect() -> Option<Arc<engram_postgres::PostgresStore>> {
    let db = engram_testkit::pg::fresh_db().await?;
    Some(Arc::new(db.store))
}

async fn seed_session(meta: &Arc<engram_postgres::PostgresStore>) -> SessionId {
    meta.create_session(SessionSpec {
        image: "localhost:5001/demo:session-ops-test".into(),
        mode: SessionMode::Agent,
    })
    .await
    .expect("create session")
}

fn claimed(outcome: EnqueueOutcome) -> SessionOp {
    match outcome {
        EnqueueOutcome::Claimed(op) => op,
        other => panic!("expected Claimed, got {other:?}"),
    }
}

fn queued(outcome: EnqueueOutcome) -> SessionOp {
    match outcome {
        EnqueueOutcome::Queued(op) => op,
        other => panic!("expected Queued, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn enqueue_and_claim_is_one_round_trip_when_idle() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let op = claimed(
        meta.op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({ "reason": "idle" }),
            None,
            "pod-1",
        )
        .await
        .expect("enqueue+claim"),
    );
    assert_eq!(op.session_id, sid);
    assert_eq!(op.kind, OpKind::Evict);
    assert_eq!(op.state, OpState::Running);
    assert_eq!(
        op.epoch,
        Some(1),
        "fresh session: 0 CAS-bumps to 1 at claim"
    );
    assert_eq!(op.attempts, 1, "the claim counts the attempt");
    assert_eq!(op.claimed_by.as_deref(), Some("pod-1"));
    assert_eq!(op.payload["reason"], "idle");

    // `sessions.current_epoch` really moved: a fenced write with the
    // claimed epoch lands, one with any other epoch is 0-row.
    assert!(
        !meta
            .fenced_assign_sandbox(sid, 99, None, None)
            .await
            .expect("fenced write, wrong epoch"),
        "stale epoch must fence",
    );
    assert!(
        meta.fenced_assign_sandbox(sid, 1, None, None)
            .await
            .expect("fenced write, claimed epoch"),
        "current_epoch must be 1 after the claim",
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn second_enqueue_queues_behind_running() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let evict = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Evict, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue evict"),
    );
    let resume = queued(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue resume"),
    );
    assert_eq!(resume.state, OpState::Queued);
    assert!(resume.epoch.is_none(), "queued rows carry no epoch");

    // Nothing claimable while the evict runs (the one_running invariant).
    assert!(
        meta.op_claim_head(sid, "pod-2")
            .await
            .expect("claim_head")
            .is_none(),
        "head is not claimable behind a running op",
    );
    let running = meta
        .op_running_for(sid)
        .await
        .expect("running_for")
        .expect("evict is running");
    assert_eq!(running.id, evict.id);

    // Finish the evict; the resume becomes the claimable head at the
    // NEXT epoch.
    assert!(meta
        .op_finish(evict.id, 1, OpState::Done, None)
        .await
        .expect("finish evict"));
    // A stale finish (already-finished row) is Ok(false), not an error.
    assert!(!meta
        .op_finish(evict.id, 1, OpState::Done, None)
        .await
        .expect("re-finish evict"));

    let next = meta
        .op_claim_head(sid, "pod-2")
        .await
        .expect("claim_head after finish")
        .expect("resume claimable");
    assert_eq!(next.id, resume.id);
    assert_eq!(next.kind, OpKind::Resume);
    assert_eq!(next.epoch, Some(2), "each claim CAS-bumps the epoch");
    assert_eq!(next.claimed_by.as_deref(), Some("pod-2"));
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn idempotency_key_dedups() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    claimed(
        meta.op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k-1"),
            "pod-1",
        )
        .await
        .expect("first enqueue"),
    );
    // Same (session, kind, key): a no-op, not a second row.
    let dup = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k-1"),
            "pod-1",
        )
        .await
        .expect("duplicate enqueue");
    assert!(
        matches!(dup, EnqueueOutcome::Duplicate),
        "expected Duplicate, got {dup:?}",
    );
    // A different key inserts (and queues behind the running claim).
    queued(
        meta.op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k-2"),
            "pod-1",
        )
        .await
        .expect("distinct-key enqueue"),
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn fenced_write_zero_rows_after_reclaim() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let op = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Evict, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue+claim"),
    );
    assert_eq!(op.epoch, Some(1));

    // Age the heartbeat with raw SQL (no trait surface rewinds one),
    // then reclaim with an hour's staleness so concurrently-running
    // tests' fresh rows never qualify.
    sqlx::query("UPDATE session_ops SET heartbeat_at = now() - interval '2 hours' WHERE id = $1")
        .bind(op.id)
        .execute(meta.pool())
        .await
        .expect("age heartbeat");
    let reclaimed = meta
        .op_reclaim_stale(Duration::from_secs(3600), "pod-2")
        .await
        .expect("reclaim");
    let ours = reclaimed
        .iter()
        .find(|r| r.id == op.id)
        .expect("our op re-claimed");
    assert_eq!(ours.epoch, Some(2), "reclaim CAS-bumps the epoch");
    assert_eq!(ours.claimed_by.as_deref(), Some("pod-2"));
    assert_eq!(ours.state, OpState::Running);
    assert_eq!(ours.attempts, 2);

    // The fenced-out writer's stamps are all 0-row now.
    assert!(
        !meta
            .op_record_step(op.id, 1, "pick_snapshot")
            .await
            .expect("stale record_step"),
        "epoch-1 step write must fence",
    );
    assert!(
        meta.op_record_step(op.id, 2, "pick_snapshot")
            .await
            .expect("fresh record_step"),
        "epoch-2 step write lands",
    );
    // Fenced session-row transition: legal edge, stale epoch → Ok(None);
    // the successor's epoch → applied, returning the previous state.
    // (`Created`, not `Queued`: a fenced flip to `queued` would leave a
    // queue-scanner-visible row with no session_queue entry — a NULL
    // `queue_origin` that poisons the queue_scanner_live_pg suite
    // sharing this database.)
    assert!(
        meta.fenced_transition_session(sid, 1, SessionState::Created)
            .await
            .expect("stale fenced transition")
            .is_none(),
        "epoch-1 transition must fence, not error",
    );
    let prev = meta
        .fenced_transition_session(sid, 2, SessionState::Created)
        .await
        .expect("fresh fenced transition")
        .expect("applied");
    assert_eq!(prev, SessionState::Pending);
}

/// Re-review findings #3/#4: `fenced_record_snapshot` writes the row ONLY
/// while `sessions.current_epoch` still equals the op's claimed epoch. A
/// reclaimed-out predecessor (epoch behind the successor's) writes NOTHING
/// and gets `Ok(false)` — so it can never land a phantom `recoverable` row
/// a resume would pick (the 89f7984d durability-lie class), and it never
/// reaches the `commit_snapshot` that follows in the eviction pipeline.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn fenced_record_snapshot_writes_only_under_current_epoch() {
    use engram_core::types::snapshot::SnapshotRecord;
    use engram_core::types::SnapshotId;

    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let mk = |recoverable: bool| SnapshotRecord {
        id: SnapshotId::new(),
        session_id: Some(sid),
        host_id: None,
        image_version: "fenced-snap-test".into(),
        size_bytes: 2048,
        created_at: chrono::Utc::now(),
        last_accessed_at: chrono::Utc::now(),
        disk_manifest: None,
        memory_manifest: None,
        recoverable,
        aux_bundles: vec![],
        events_cursor: None,
        fc_snapshot_version: None,
    };

    // Claim an op → current_epoch = 1.
    let op = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Evict, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue+claim"),
    );
    assert_eq!(op.epoch, Some(1));

    // Under the current fence (epoch 1): the row lands.
    let under = mk(true);
    let under_id = under.id;
    assert!(
        meta.fenced_record_snapshot(under, 1)
            .await
            .expect("fenced record under current epoch"),
        "epoch-1 write lands while current_epoch == 1",
    );

    // Reclaim → current_epoch = 2 (the successor fences the predecessor).
    sqlx::query("UPDATE session_ops SET heartbeat_at = now() - interval '2 hours' WHERE id = $1")
        .bind(op.id)
        .execute(meta.pool())
        .await
        .expect("age heartbeat");
    let reclaimed = meta
        .op_reclaim_stale(Duration::from_secs(3600), "pod-2")
        .await
        .expect("reclaim");
    assert_eq!(
        reclaimed.iter().find(|r| r.id == op.id).unwrap().epoch,
        Some(2),
        "reclaim CAS-bumps the epoch",
    );

    // The fenced-out predecessor (still epoch 1) writes NOTHING.
    let stale = mk(true);
    let stale_id = stale.id;
    assert!(
        !meta
            .fenced_record_snapshot(stale, 1)
            .await
            .expect("stale fenced record"),
        "epoch-1 write must fence (0 rows) after the reclaim bumped current_epoch to 2",
    );

    // The successor (epoch 2) writes normally.
    let fresh = mk(true);
    let fresh_id = fresh.id;
    assert!(
        meta.fenced_record_snapshot(fresh, 2)
            .await
            .expect("fresh fenced record"),
        "epoch-2 write lands under the current fence",
    );

    let rows = meta
        .list_snapshots_for_session(sid)
        .await
        .expect("list snapshots");
    let ids: Vec<_> = rows.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&under_id),
        "the epoch-1-under-fence row persisted"
    );
    assert!(
        !ids.contains(&stale_id),
        "the fenced-out predecessor's phantom row must NOT exist",
    );
    assert!(ids.contains(&fresh_id), "the successor's row persisted");
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn requeue_with_backoff_leaves_not_before() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let op = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue+claim"),
    );
    assert!(meta
        .op_requeue_with_backoff(op.id, 1, Duration::from_secs(3600), "host restore failed")
        .await
        .expect("requeue"));

    // Back to queued but not due: nothing running, nothing claimable,
    // and the session is absent from the due scan.
    assert!(meta
        .op_running_for(sid)
        .await
        .expect("running_for")
        .is_none());
    assert!(
        meta.op_claim_head(sid, "pod-1")
            .await
            .expect("claim_head")
            .is_none(),
        "not_before in the future must block the claim",
    );
    let due = meta.op_due_sessions().await.expect("due sessions");
    assert!(
        !due.contains(&sid),
        "backed-off session must not be in the due scan",
    );
}

#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn cancel_queued_and_cancel_running_flag() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let evict = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Evict, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue evict"),
    );
    queued(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue resume"),
    );

    // Queued op cancels outright; a second cancel finds nothing.
    assert!(meta
        .op_cancel_queued(sid, OpKind::Resume)
        .await
        .expect("cancel queued resume"));
    assert!(!meta
        .op_cancel_queued(sid, OpKind::Resume)
        .await
        .expect("re-cancel queued resume"));

    // Running op cancels cooperatively: the flag lands in the payload
    // and reads back between steps.
    assert!(!meta
        .op_cancel_requested(evict.id)
        .await
        .expect("flag before request"));
    assert!(meta
        .op_request_cancel_running(sid, OpKind::Evict)
        .await
        .expect("request cancel"));
    assert!(meta
        .op_cancel_requested(evict.id)
        .await
        .expect("flag after request"));
    // Unknown op id reads false, never errors.
    assert!(!meta
        .op_cancel_requested(i64::MAX)
        .await
        .expect("flag for unknown op"));

    // The cancelled resume never becomes claimable after the evict ends.
    assert!(meta
        .op_finish(evict.id, 1, OpState::Done, None)
        .await
        .expect("finish evict"));
    assert!(
        meta.op_claim_head(sid, "pod-1")
            .await
            .expect("claim_head")
            .is_none(),
        "cancelled op must not be claimed",
    );
}

/// ADR 0079 acceptance: enqueue an EVICT then a RESUME for one session —
/// the claim order is strict (evict first; the resume is queued behind
/// and not claimable until the evict finishes), with zero sleeps. Stub
/// work only: this drives REAL claim ordering via the store; the
/// pipeline-level e2e is the e2e stack's job.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn evict_then_resume_claims_in_strict_order() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    // The nomination enqueues (and, on an idle lane, claims) the evict.
    let evict = claimed(
        meta.op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({ "target": "idle", "allow_park": true, "nominated": true }),
            Some("evict:ordering-test"),
            "pod-1",
        )
        .await
        .expect("enqueue evict"),
    );
    // The returning user's resume queues BEHIND the running evict.
    let resume = queued(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue resume"),
    );
    assert!(resume.id > evict.id, "log order is id order");

    // Strictly ordered: nothing claimable while the evict runs — on this
    // pod or any other.
    assert!(meta
        .op_claim_head(sid, "pod-2")
        .await
        .expect("claim_head")
        .is_none());
    // `op_get` (the bounded-observe read) sees both rows honestly.
    let seen_evict = meta
        .op_get(evict.id)
        .await
        .expect("op_get evict")
        .expect("evict row");
    assert_eq!(seen_evict.state, OpState::Running);
    let seen_resume = meta
        .op_get(resume.id)
        .await
        .expect("op_get resume")
        .expect("resume row");
    assert_eq!(seen_resume.state, OpState::Queued);

    // Evict finishes (stub work) → the resume is the claimable head, at
    // the next epoch. No sleeps anywhere: ordering is by log, not poll.
    assert!(meta
        .op_finish(evict.id, evict.epoch.expect("epoch"), OpState::Done, None)
        .await
        .expect("finish evict"));
    let next = meta
        .op_claim_head(sid, "pod-2")
        .await
        .expect("claim_head after evict")
        .expect("resume claimable");
    assert_eq!(
        next.id, resume.id,
        "the resume runs strictly after the evict"
    );
    assert_eq!(next.kind, OpKind::Resume);
    assert!(next.epoch.expect("epoch") > evict.epoch.expect("epoch"));
}

/// ADR 0079 pass 2 (the deliver-behind-resume ordering's store half):
/// a requeued-with-backoff op keeps its LOW row id, but `op_claim_head`'s
/// due-gating (`not_before <= now()` before the `ORDER BY id` head pick)
/// makes a LATER-enqueued, due op the claimable head — this is exactly
/// what lets the deliver verb requeue itself and have the resume it just
/// enqueued run first. Zero sleeps.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn backoff_gated_low_id_yields_the_head_to_a_due_later_op() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    // The deliver claims first (lowest id)...
    let deliver = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Deliver, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue deliver"),
    );
    // ...the resume it enqueues mid-run queues behind it (higher id)...
    let resume = queued(
        meta.op_enqueue_and_claim(
            sid,
            OpKind::Resume,
            serde_json::json!({ "flavor": "for_delivery" }),
            None,
            "pod-1",
        )
        .await
        .expect("enqueue resume"),
    );
    assert!(resume.id > deliver.id);
    // ...and the deliver requeues with a backoff well in the future.
    assert!(meta
        .op_requeue_with_backoff(
            deliver.id,
            deliver.epoch.expect("epoch"),
            Duration::from_secs(3600),
            "resume enqueued; delivery after it",
        )
        .await
        .expect("requeue deliver"));

    // Head claim: the RESUME (higher id, due) — the backing-off deliver
    // is due-gated out despite its lower id.
    let head = meta
        .op_claim_head(sid, "pod-1")
        .await
        .expect("claim_head")
        .expect("resume must be claimable");
    assert_eq!(
        head.id, resume.id,
        "due-gating must yield the head to the resume"
    );
    assert_eq!(head.kind, OpKind::Resume);

    // After the resume finishes, the deliver is STILL gated (its retry
    // waits out the backoff) — nothing claimable, nothing due.
    assert!(meta
        .op_finish(head.id, head.epoch.expect("epoch"), OpState::Done, None)
        .await
        .expect("finish resume"));
    assert!(
        meta.op_claim_head(sid, "pod-1")
            .await
            .expect("claim_head after resume")
            .is_none(),
        "the deliver stays due-gated until its backoff elapses",
    );
    let due = meta.op_due_sessions().await.expect("due sessions");
    assert!(!due.contains(&sid));

    // ADR 0079 pass 2: `op_pending_exists` (the outbox shim's duplicate
    // guard) sees the backing-off deliver as pending.
    assert!(meta
        .op_pending_exists(sid, OpKind::Deliver)
        .await
        .expect("pending_exists"));
    assert!(!meta
        .op_pending_exists(sid, OpKind::Destroy)
        .await
        .expect("pending_exists destroy"));
}

/// ADR 0079: `op_cancel_by_id` cancels exactly ONE queued row (the
/// inline claim's give-up path) — a sibling queued op of the same kind
/// is untouched, and a running row is never cancellable by id.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn cancel_by_id_is_row_scoped() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let running = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Evict, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue evict"),
    );
    let q1 = queued(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue resume 1"),
    );
    let q2 = queued(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue resume 2"),
    );

    // Row-scoped: cancels q1 only.
    assert!(meta.op_cancel_by_id(q1.id).await.expect("cancel q1"));
    assert_eq!(
        meta.op_get(q1.id).await.unwrap().unwrap().state,
        OpState::Cancelled
    );
    assert_eq!(
        meta.op_get(q2.id).await.unwrap().unwrap().state,
        OpState::Queued,
        "the sibling queued row must be untouched",
    );
    // Running rows are not id-cancellable (cooperative cancel only).
    assert!(!meta
        .op_cancel_by_id(running.id)
        .await
        .expect("cancel running"));
    assert_eq!(
        meta.op_get(running.id).await.unwrap().unwrap().state,
        OpState::Running
    );
}

// ---------------------------------------------------------------------
// ADR 0079 pre-merge review regression tests.
// ---------------------------------------------------------------------

/// Review finding #3: `fenced_transition_session` must check the FENCE
/// before the legality gate. A fenced-out predecessor whose successor
/// already transitioned into a state from which the predecessor's target
/// is ILLEGAL must get `Ok(None)` (silent fenced stop), NOT a bare
/// `Conflict` — the latter would slip past callers matching
/// `starts_with("fenced:")` and fire the compensation a fenced executor
/// must never run (the 89f7984d brick class).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn fenced_transition_checks_fence_before_legality() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    // Claim (epoch 1), then reclaim (epoch 2) — epoch 1 is now fenced.
    let op = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue+claim"),
    );
    assert_eq!(op.epoch, Some(1));
    sqlx::query("UPDATE session_ops SET heartbeat_at = now() - interval '2 hours' WHERE id = $1")
        .bind(op.id)
        .execute(meta.pool())
        .await
        .expect("age heartbeat");
    meta.op_reclaim_stale(Duration::from_secs(3600), "pod-2")
        .await
        .expect("reclaim");

    // Successor (epoch 2) transitions Pending -> Created.
    let prev = meta
        .fenced_transition_session(sid, 2, SessionState::Created)
        .await
        .expect("successor transition")
        .expect("applied");
    assert_eq!(prev, SessionState::Pending);

    // The fenced-out epoch-1 executor attempts Created -> Idle, which is
    // ILLEGAL. Pre-fix: legality ran first -> bare Conflict (Err). Post-
    // fix: the epoch mismatch is seen first -> Ok(None).
    let result = meta
        .fenced_transition_session(sid, 1, SessionState::Idle)
        .await;
    assert!(
        matches!(result, Ok(None)),
        "epoch-mismatch must be the silent-stop path even when the \
         successor's state makes the target illegal; got {result:?}",
    );
}

/// Review finding #4: the idempotency index is scoped to ACTIVE states,
/// so a TERMINAL keyed op does not burn the key forever — a fresh enqueue
/// after a terminal failure lands a new row (the scanner/reaper can
/// re-drive instead of getting `Duplicate` every tick on a wedged
/// session).
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn terminal_keyed_op_does_not_burn_the_key() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let op = claimed(
        meta.op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k"),
            "pod-1",
        )
        .await
        .expect("first enqueue"),
    );
    // While it's active, the key IS held (Duplicate).
    assert!(
        matches!(
            meta.op_enqueue_and_claim(
                sid,
                OpKind::Evict,
                serde_json::json!({}),
                Some("k"),
                "pod-1"
            )
            .await
            .expect("dup while active"),
            EnqueueOutcome::Duplicate
        ),
        "an active keyed op holds the key",
    );
    // Finish it terminally (Failed).
    assert!(meta
        .op_finish(op.id, op.epoch.unwrap(), OpState::Failed, Some("boom"))
        .await
        .expect("finish failed"));
    // Re-enqueue the SAME key — must NOT be Duplicate now (a fresh row).
    let after = meta
        .op_enqueue_and_claim(
            sid,
            OpKind::Evict,
            serde_json::json!({}),
            Some("k"),
            "pod-1",
        )
        .await
        .expect("re-enqueue after terminal");
    assert!(
        !matches!(after, EnqueueOutcome::Duplicate),
        "a terminal keyed row must not burn the key; got {after:?}",
    );
    let fresh = claimed(after);
    assert_ne!(fresh.id, op.id, "a new row, not the terminal one");
}

/// Review finding #1: the within-step heartbeat (`op_heartbeat`) proves
/// liveness WITHOUT clobbering the step marker, so a long-running step is
/// not reclaimed. An op whose heartbeat was just bumped is excluded from
/// `op_reclaim_stale`, and its recorded step survives the beat.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn within_step_heartbeat_keeps_op_alive_without_clobbering_step() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let op = claimed(
        meta.op_enqueue_and_claim(sid, OpKind::Resume, serde_json::json!({}), None, "pod-1")
            .await
            .expect("enqueue+claim"),
    );
    let epoch = op.epoch.unwrap();
    assert!(meta.op_record_step(op.id, epoch, "restore").await.unwrap());

    // Simulate a long-running step: age the heartbeat well past the stale
    // bound, then the within-step beat bumps it back to now.
    sqlx::query("UPDATE session_ops SET heartbeat_at = now() - interval '2 hours' WHERE id = $1")
        .bind(op.id)
        .execute(meta.pool())
        .await
        .expect("age heartbeat");
    assert!(
        meta.op_heartbeat(op.id, epoch).await.expect("heartbeat"),
        "the heartbeat lands while the op is still ours",
    );

    // Not reclaimed (heartbeat fresh)...
    let reclaimed = meta
        .op_reclaim_stale(Duration::from_secs(3600), "pod-2")
        .await
        .expect("reclaim");
    assert!(
        !reclaimed.iter().any(|r| r.id == op.id),
        "a freshly-heartbeated op must NOT be reclaimed",
    );
    // ...and the step marker is intact (the beat bumped only heartbeat_at).
    assert_eq!(
        meta.op_get(op.id).await.unwrap().unwrap().step.as_deref(),
        Some("restore"),
        "op_heartbeat must not clobber the crash-resume step",
    );
}

/// Review finding #8: an exclusive inline claim is atomic — when the lane
/// is busy it returns `None` and leaves NO grabbable queued row (the old
/// enqueue-then-cancel shape left a row the executor could claim and run
/// the full verb after the caller was told "busy").
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn exclusive_claim_leaves_no_row_when_busy() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    let first = meta
        .op_enqueue_and_claim_exclusive(sid, OpKind::Teleport, serde_json::json!({}), "pod-1")
        .await
        .expect("first exclusive")
        .expect("lane free -> claimed");
    assert_eq!(first.state, OpState::Running);

    // A second exclusive claim finds the lane busy -> None, and rolls back
    // its own insert (no orphan queued row).
    let second = meta
        .op_enqueue_and_claim_exclusive(sid, OpKind::Teleport, serde_json::json!({}), "pod-2")
        .await
        .expect("second exclusive");
    assert!(second.is_none(), "busy lane must return None");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM session_ops WHERE session_id = $1")
        .bind(sid.as_uuid())
        .fetch_one(meta.pool())
        .await
        .expect("count");
    assert_eq!(
        count, 1,
        "the busy exclusive claim must leave no orphan row"
    );
    // And there is nothing an executor could claim.
    assert!(
        meta.op_claim_head(sid, "pod-3")
            .await
            .expect("claim head")
            .is_none(),
        "no grabbable queued row exists",
    );
}

/// Review finding #5: a Pending session that lost its create_boot op
/// (placed but no active create_boot) is surfaced by
/// `orphaned_pending_sessions` so the reclaim sweep can re-enqueue; a
/// Pending session WITH an active create_boot op is not.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn orphaned_pending_detected_only_without_active_create_boot() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;
    // Fresh session is Pending; age last_active_at past the grace window.
    sqlx::query("UPDATE sessions SET last_active_at = now() - interval '1 hour' WHERE id = $1")
        .bind(sid.as_uuid())
        .execute(meta.pool())
        .await
        .expect("age last_active_at");

    let orphans = meta
        .orphaned_pending_sessions(Duration::from_secs(60))
        .await
        .expect("scan");
    assert!(
        orphans.contains(&sid),
        "a Pending session with no create_boot op must be surfaced",
    );

    // Give it an active create_boot op -> no longer orphaned.
    claimed(
        meta.op_enqueue_and_claim(
            sid,
            OpKind::CreateBoot,
            serde_json::json!({}),
            None,
            "pod-1",
        )
        .await
        .expect("enqueue create_boot"),
    );
    let orphans = meta
        .orphaned_pending_sessions(Duration::from_secs(60))
        .await
        .expect("scan 2");
    assert!(
        !orphans.contains(&sid),
        "a Pending session with an active create_boot op is not orphaned",
    );
}

/// ADR 0079 latency fix: a backed-off queued op is woken (not_before → now)
/// by op_wake_queued_kind, so the completion re-drive claims it immediately
/// instead of waiting out the 5s fallback poll. Models the deliver-behind-
/// for_delivery-resume path: the deliver requeues with a failure backoff,
/// the resume completes and wakes it, and it becomes due at once.
#[tokio::test]
#[ignore = "requires live Postgres at ENGRAM_TEST_DATABASE_URL"]
async fn wake_queued_kind_pulls_not_before_to_now() {
    let Some(meta) = connect().await else { return };
    let sid = seed_session(&meta).await;

    // A deliver op, claimed then requeued with a long backoff (the
    // ordering-wait shape) — not due for 60s.
    let d = match meta
        .op_enqueue_and_claim(sid, OpKind::Deliver, serde_json::json!({}), None, "pod-a")
        .await
        .expect("enqueue deliver")
    {
        EnqueueOutcome::Claimed(op) => op,
        other => panic!("expected Claimed, got {other:?}"),
    };
    meta.op_requeue_with_backoff(
        d.id,
        d.epoch.unwrap(),
        std::time::Duration::from_secs(60),
        "wait",
    )
    .await
    .expect("requeue");
    // Backed off → not claimable.
    assert!(
        meta.op_claim_head(sid, "pod-a").await.unwrap().is_none(),
        "backed-off deliver must not be due",
    );

    // Wake it → due now → claimable immediately.
    let woken = meta
        .op_wake_queued_kind(sid, OpKind::Deliver)
        .await
        .expect("wake");
    assert_eq!(woken, 1, "one queued deliver woken");
    let claimed = meta.op_claim_head(sid, "pod-a").await.unwrap();
    assert!(
        claimed.is_some(),
        "woken deliver must be immediately claimable (no poll wait)",
    );
    // A second wake with nothing backed-off is a no-op (idempotent).
    assert_eq!(
        meta.op_wake_queued_kind(sid, OpKind::Deliver)
            .await
            .unwrap(),
        0,
        "no queued deliver left to wake",
    );
}
